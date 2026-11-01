//! 真正跑一次 tick / 日记 / 周记，并保证**任何失败都不会拖垮服务**。
//!
//! # 为什么这里到处是 `?` 转日志
//!
//! 生成是**旁路**：记忆的读写是核心功能，不该因为 LLM 挂了就不可用。
//! 所以对外只有「成功/失败」两种结果，失败**只记 generation_log**，
//! 由面板显示红点。绝不让错误冒泡到 HTTP handler 或后台任务外层。

use std::sync::{Arc, Mutex};

use crate::db::Db;
use crate::error::Result;
use crate::generate::runner::{self, Record};
use crate::generate::throttle::{Decision, Throttle};
use crate::generate::tick;
use crate::llm::Purpose;

/// tick 的内存态（节流器）。
pub struct TickState {
    pub throttle: Mutex<Throttle>,
}

impl Default for TickState {
    fn default() -> Self {
        Self {
            throttle: Mutex::new(Throttle::default()),
        }
    }
}

/// 跑一次 tick。返回给面板/日志看的摘要。
pub async fn run_tick(
    db: &Arc<Db>,
    cfg: &crate::config::Config,
    state: &TickState,
    force: bool,
) -> Result<TickSummary> {
    let now = crate::db::now_ms();
    let g = &cfg.generate;

    // ---- 1. 该不该跑
    let decision = {
        let t = state.throttle.lock().unwrap_or_else(|e| e.into_inner());
        if force {
            Decision::Run(crate::generate::throttle::RunReason::Manual)
        } else {
            t.decide(now, g.tick_enabled, g.tick_batch, g.tick_interval_seconds)
        }
    };

    let reason = match decision {
        Decision::Skip(s) => {
            // 跳过也记账：排错时能回答「为什么没生成」
            runner::log(
                db,
                Record {
                    purpose: Purpose::Tick,
                    ok: true,
                    trigger: s.as_str(),
                    model: &cfg.llm.model,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    elapsed_ms: 0,
                    result: Some("跳过".into()),
                    error: None,
                },
            );
            return Ok(TickSummary {
                ran: false,
                why: s.as_str().into(),
                ..Default::default()
            });
        }
        Decision::Run(r) => r,
    };

    // ---- 2. 采输入
    let since = {
        let t = state.throttle.lock().unwrap_or_else(|e| e.into_inner());
        // 从没跑过就用 6 小时窗口：跑太远会把几天的东西一次灌进去
        if t.watermark_ms() > 0 {
            t.watermark_ms()
        } else {
            now - 6 * 3_600_000
        }
    };
    let input = crate::generate::collect::gather(
        db,
        since,
        now,
        g.tick_min_importance,
        40, // 事件上限：再多就该靠重要度筛，不是靠堆
        30, // 对话上限
    )?;
    if input.events.is_empty() && input.talks.is_empty() {
        {
            let mut t = state.throttle.lock().unwrap_or_else(|e| e.into_inner());
            t.mark_run(now);
        }
        return Ok(TickSummary {
            ran: false,
            why: "nothing_new".into(),
            ..Default::default()
        });
    }

    // ---- 3. 调模型
    let client = runner::client(cfg);
    let (res, elapsed) = runner::timed(tick::run(&client, &input)).await;

    // ---- 4. 记账 + 写回
    match res {
        Ok(out) => {
            let mut journals = 0usize;
            let mut self_updated = false;
            if g.write_back {
                if let Err(e) = crate::generate::collect::write_mood(db, &out.mood) {
                    tracing::warn!("写情绪失败：{e}");
                }
                for j in &out.journal {
                    match crate::generate::collect::write_journal(db, j, "system") {
                        Ok(_) => journals += 1,
                        Err(e) => tracing::warn!("写手帐失败：{e}"),
                    }
                }
                if let Some(n) = out.self_narrative.as_deref() {
                    if !n.trim().is_empty() {
                        match crate::generate::collect::write_self(db, n) {
                            Ok(_) => self_updated = true,
                            Err(e) => tracing::warn!("写自我认知失败：{e}"),
                        }
                    }
                }
            }
            {
                let mut t = state.throttle.lock().unwrap_or_else(|e| e.into_inner());
                t.mark_run(now);
            }
            let summary = format!(
                "情绪:{} · 手帐 {} 条{}",
                out.mood.label,
                journals,
                if self_updated {
                    " · 自我认知已更新"
                } else {
                    ""
                }
            );
            runner::log(
                db,
                Record {
                    purpose: Purpose::Tick,
                    ok: true,
                    trigger: reason.as_str(),
                    model: &cfg.llm.model,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    elapsed_ms: elapsed,
                    result: Some(summary.clone()),
                    error: None,
                },
            );
            tracing::info!("tick 完成：{summary}（{elapsed}ms）");
            Ok(TickSummary {
                ran: true,
                why: reason.as_str().into(),
                mood: Some(out.mood.label),
                journals,
                self_updated,
                elapsed_ms: elapsed,
            })
        }
        Err(e) => {
            let msg = e.to_string();
            runner::log(
                db,
                Record {
                    purpose: Purpose::Tick,
                    ok: false,
                    trigger: reason.as_str(),
                    model: &cfg.llm.model,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    elapsed_ms: elapsed,
                    result: None,
                    error: Some(msg.clone()),
                },
            );
            // **不推进水位线**：失败了下次还要重试这一段
            tracing::warn!("tick 失败（下次重试）：{msg}");
            Ok(TickSummary {
                ran: false,
                why: format!("失败：{msg}"),
                ..Default::default()
            })
        }
    }
}

/// tick 结果摘要（面板显示）。
#[derive(Debug, Default, serde::Serialize)]
pub struct TickSummary {
    pub ran: bool,
    pub why: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mood: Option<String>,
    pub journals: usize,
    pub self_updated: bool,
    pub elapsed_ms: i64,
}
