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
            // **跳过不记账**：tick 每 60 秒检查一次，每次都写一行的话
            // 一天 1440 行纯噪声，会把真正有价值的记录（尤其失败）淹掉 ——
            // `gen_log_fail` 那个索引就是为「找失败」建的。
            // 跳过原因打 trace 日志；面板上的「为什么没生成」由
            // `tick_state` 的 pending / last_run 回答。
            tracing::debug!("tick 跳过：{}", s.as_str());
            return Ok(TickSummary {
                ran: false,
                why: s.as_str().into(),
                ..Default::default()
            });
        }
        Decision::Run(r) => r,
    };

    // ---- 2. 采输入
    //
    // 窗口起点有两个来源，取**较新的那个**：
    // - 节流器的水位线（本次进程内跑过 tick，就只看它之后的）；
    // - `gen_state` 里存的持久水位线（重启后接着跑，不重吃已处理过的）。
    //
    // 为什么需要持久水位线：`Throttle` 在内存里，重启就归零 ——
    // 没有它的话每次重启都会把过去 6 小时重吃一遍（真实花费）。
    let mem_wm = {
        let t = state.throttle.lock().unwrap_or_else(|e| e.into_inner());
        t.watermark_ms()
    };
    let disk_wm = runner::state_get(db, "tick_watermark").unwrap_or(0);
    let since = match (mem_wm, disk_wm) {
        (0, 0) => now - 6 * 3_600_000, // 从没跑过：6 小时窗口
        (a, b) => a.max(b),
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
            // 持久水位线：重启后不重吃已经处理过的事件。
            //
            // **只在成功时推进** —— 失败时下次要重试同一段，
            // 推进了就等于把它悄悄丢了。
            runner::state_set(db, "tick_watermark", now);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_llm() -> crate::config::Config {
        let mut c = crate::config::Config::default();
        c.llm.base_url = "http://127.0.0.1:1/v1".into();
        c.llm.api_key = "k".into();
        c.llm.model = "m".into();
        c.llm.retries = 0;
        c.llm.timeout_seconds = 5;
        c
    }

    /// LLM 明显连不上时：要记失败、**不推进水位线**、也不 panic。
    #[tokio::test]
    async fn 失败时不推进水位线以便重试() {
        let (db, _g) = crate::generate::runner::tests_support::fresh_db();
        let cfg = cfg_with_llm();
        let state = TickState::default();

        // 造一条够格的事件，否则会因为「没新东西」提前返回
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO event(at_ms, kind, channel, summary, importance, scope)
                 VALUES (?1,'died','mc','被砸死',0.9,'companion')",
                rusqlite::params![crate::db::now_ms()],
            )?;
            Ok(())
        })
        .unwrap();

        let r = run_tick(&db, &cfg, &state, true).await.unwrap();
        assert!(!r.ran, "连不上就该失败：{r:?}");
        assert!(
            crate::generate::runner::state_get(&db, "tick_watermark").is_none(),
            "失败了不能推进水位线，否则这段事件被悄悄丢掉"
        );

        // 但失败要留下记录
        let rows = crate::generate::runner::recent(&db, 5).unwrap();
        assert!(rows.iter().any(|r| !r.ok), "失败必须入库：{rows:?}");
        assert!(
            crate::generate::runner::consecutive_failures(&db, Purpose::Tick) >= 1,
            "连续失败数要能算出来（面板红点用）"
        );
    }

    /// 没有新东西时不该白花一次请求。
    #[tokio::test]
    async fn 没新东西就不调用() {
        let (db, _g) = crate::generate::runner::tests_support::fresh_db();
        let cfg = cfg_with_llm();
        // 把水位线推到「现在之后」→ 没有任何事件在窗口里
        crate::generate::runner::state_set(&db, "tick_watermark", crate::db::now_ms() + 60_000);
        let state = TickState::default();
        let r = run_tick(&db, &cfg, &state, false).await.unwrap();
        assert!(!r.ran);
        assert_eq!(r.why, "nothing_new");
    }

    /// 关掉 tick 就该完全不动。
    #[tokio::test]
    async fn 关掉就不跑() {
        let (db, _g) = crate::generate::runner::tests_support::fresh_db();
        let mut cfg = cfg_with_llm();
        cfg.generate.tick_enabled = false;
        let state = TickState::default();
        let r = run_tick(&db, &cfg, &state, false).await.unwrap();
        assert!(!r.ran);
        assert_eq!(r.why, "disabled");
    }
}
