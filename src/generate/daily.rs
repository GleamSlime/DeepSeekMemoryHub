//! 定时任务：每日日记、每周周记。
//!
//! # 与 tick 的区别
//!
//! | | tick | 日记/周记 |
//! | --- | --- | --- |
//! | 触发 | 攒够事件 / 到间隔 | 到点（04:00） |
//! | 输入 | 上次之后的新事件 | **一整段时间**（一天/一周） |
//! | 产出 | 情绪 + 手帐 + 自我认知 | 一篇叙述性文字 |
//!
//! # 为什么日记要单独一次请求
//!
//! 它要的是**连贯的叙述**，而 tick 要的是**结构化判断**。
//! 合成一次会让模型既写日记又算情绪，两个都做不好
//! （实测过：它会把日记写成「情绪：平静；手帐：…」的清单）。
//! 而日记一天只有一次，省这一次请求没有意义 —— 省请求要省在高频的地方。

use std::sync::Arc;

use crate::db::Db;
use crate::error::{Error, Result};
use crate::generate::runner::{self, Record};
use crate::llm::{ChatClient, ChatMessage, Purpose};

/// 日记的系统提示词。
const DIARY_SYS: &str = r#"你是一个有自己生活的角色，现在要写今天的日记。

你会拿到：今天发生的事、今天的对话。

要求：
- 用第一人称写，像真的在记日记，不要写成报告或清单；
- 300 到 600 字，可以用 2–4 个自然段；
- 写你在意的事、当时的感觉，不是流水账；
- **不要编造没发生过的事**：只依据给的材料，可以描述感受，不能加事件；
- 不要小标题、不要 markdown、不要「今天」开头的模板句。

直接输出日记正文。"#;

/// 周记的系统提示词。
const WEEKLY_SYS: &str = r#"你是一个有自己生活的角色，现在要写这周的周记。

你会拿到：这一周每天发生的事、这几天的日记。

要求：
- 用第一人称，回看这一周，写出**变化**和**模式**（不是把七天重新说一遍）；
- 300 到 600 字；
- 有具体的回忆，也有对自己的观察；
- **不要编造**：只依据给的材料；
- 不要小标题、不要 markdown。

直接输出周记正文。"#;

/// 到点了就跑（内部判断该不该跑）。
///
/// 幂等：`gen_state` 记着「最后一次成功的时刻」，同一天不会重复生成。
pub async fn maybe_run(db: &Arc<Db>, cfg: &crate::config::Config, purpose: Purpose) -> Result<()> {
    let g = &cfg.generate;
    let now = crate::db::now_ms();

    // ---- 1. 该不该跑
    let (should, key, window_start) = match purpose {
        Purpose::Diary => {
            let last = runner::state_get(db, "diary");
            let due = runner::due_daily(db, now, &g.diary_at, last);
            // 窗口：昨天这个点 → 现在（第一次跑就取 24 小时）
            let start = last.unwrap_or(now - 86_400_000).max(now - 86_400_000);
            (due, "diary", start)
        }
        Purpose::Weekly => {
            let last = runner::state_get(db, "weekly");
            let due = runner::due_weekly(db, now, g.weekly_day, &g.weekly_at, last);
            let start = last
                .unwrap_or(now - 7 * 86_400_000)
                .max(now - 7 * 86_400_000);
            (due, "weekly", start)
        }
        Purpose::Tick => return Ok(()),
    };
    if !should {
        return Ok(());
    }

    // ---- 2. 采材料
    let material = match purpose {
        Purpose::Diary => collect_day(db, window_start, now)?,
        _ => collect_week(db, window_start, now)?,
    };
    if material.trim().is_empty() {
        // 一整天什么都没有：不生成（也**不推进水位线**？—— 要推进，
        // 否则每天都会重试一次空日记）。记一条「素材为空」并推进。
        tracing::info!("{} 没有素材，跳过", purpose.as_str());
        runner::state_set(db, key, now);
        runner::log(
            db,
            Record {
                purpose,
                ok: true,
                trigger: "schedule",
                model: &cfg.llm.model,
                prompt_tokens: 0,
                completion_tokens: 0,
                elapsed_ms: 0,
                result: Some("素材为空，跳过".into()),
                error: None,
            },
        );
        return Ok(());
    }

    // ---- 3. 调模型
    let sys = match purpose {
        Purpose::Weekly => WEEKLY_SYS,
        _ => DIARY_SYS,
    };
    let client: ChatClient = runner::client(cfg);
    let (res, elapsed) = runner::timed(client.chat(&[
        ChatMessage::system(sys),
        ChatMessage::user(material.clone()),
    ]))
    .await;

    // ---- 4. 记账 / 写回
    match res {
        Ok(out) => {
            let body = out.text.trim();
            if body.is_empty() {
                let msg = "模型返回空文本".to_string();
                runner::log(
                    db,
                    Record {
                        purpose,
                        ok: false,
                        trigger: "schedule",
                        model: &cfg.llm.model,
                        prompt_tokens: out.prompt_tokens,
                        completion_tokens: out.completion_tokens,
                        elapsed_ms: elapsed,
                        result: None,
                        error: Some(msg.clone()),
                    },
                );
                return Err(Error::Storage(msg));
            }
            let mut note = String::new();
            if g.write_back {
                write_diary(db, purpose, body, window_start, now)?;
            } else {
                note = "（未写库，仅记录）".into();
            }
            // 水位线只在成功时推进：失败下次重试
            runner::state_set(db, key, now);
            let summary = format!(
                "{} 字{}",
                body.chars().count(),
                if out.truncated { "（被截断）" } else { "" }
            );
            runner::log(
                db,
                Record {
                    purpose,
                    ok: true,
                    trigger: "schedule",
                    model: &cfg.llm.model,
                    prompt_tokens: out.prompt_tokens,
                    completion_tokens: out.completion_tokens,
                    elapsed_ms: elapsed,
                    result: Some(format!("{summary}{note}")),
                    error: None,
                },
            );
            tracing::info!("{} 生成完成：{summary}（{elapsed}ms）", purpose.as_str());
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            runner::log(
                db,
                Record {
                    purpose,
                    ok: false,
                    trigger: "schedule",
                    model: &cfg.llm.model,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    elapsed_ms: elapsed,
                    result: None,
                    error: Some(msg.clone()),
                },
            );
            tracing::warn!("{} 生成失败（下次重试）：{msg}", purpose.as_str());
            Err(e)
        }
    }
}

/// 采一天的材料：事件 + 对话。
fn collect_day(db: &Db, from: i64, to: i64) -> Result<String> {
    let events = crate::generate::collect::range_events(db, from, to, 0.25, 200)?;
    let talks = crate::generate::collect::range_talks(db, from, to, 300)?;
    Ok(render_material(&events, &talks, None))
}

/// 采一周的材料：事件 + 对话 + **这几天已有的日记**。
///
/// 周记要写「变化」，只看事件看不出变化 —— 得看前几天自己怎么记的。
fn collect_week(db: &Db, from: i64, to: i64) -> Result<String> {
    let events = crate::generate::collect::range_events(db, from, to, 0.4, 400)?;
    let talks = crate::generate::collect::range_talks(db, from, to, 300)?;
    let diaries = crate::generate::collect::range_diaries(db, from, to, 7)?;
    Ok(render_material(&events, &talks, Some(&diaries)))
}

fn render_material(
    events: &[crate::generate::tick::TickEvent],
    talks: &[crate::generate::tick::TickTalk],
    diaries: Option<&[(i64, String)]>,
) -> String {
    let mut s = String::new();
    s.push_str("## 发生的事\n");
    if events.is_empty() {
        s.push_str("（没有记录）\n");
    } else {
        for e in events {
            s.push_str(&format!(
                "- [{}] {} · {}\n",
                e.channel,
                e.kind,
                one_line(&e.summary, 120)
            ));
        }
    }
    s.push_str("\n## 对话\n");
    if talks.is_empty() {
        s.push_str("（没有对话）\n");
    } else {
        for t in talks {
            s.push_str(&format!("- {}：{}\n", t.who, one_line(&t.content, 90)));
        }
    }
    if let Some(ds) = diaries {
        s.push_str("\n## 这几天我写的日记\n");
        if ds.is_empty() {
            s.push_str("（没有）\n");
        } else {
            for (ms, body) in ds {
                s.push_str(&format!(
                    "### {}（{} 字）\n{}\n",
                    crate::generate::collect::fmt_day(*ms),
                    body.chars().count(),
                    one_line(body, 400)
                ));
            }
        }
    }
    s
}

fn one_line(s: &str, max: usize) -> String {
    let t: String = s.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    let t = t.trim();
    if t.chars().count() <= max {
        t.to_string()
    } else {
        format!("{}…", t.chars().take(max).collect::<String>())
    }
}

/// 写库。`kind` 是 `day` / `week`。
fn write_diary(db: &Db, purpose: Purpose, body: &str, from: i64, to: i64) -> Result<String> {
    let id = format!("d-{}-{}", crate::db::now_ms(), crate::http::rand_suffix());
    let now = crate::db::now_ms();
    let kind = if purpose == Purpose::Weekly {
        "week"
    } else {
        "day"
    };
    // 标题：让面板上一眼能分清是哪天
    let title = crate::generate::collect::fmt_day(if kind == "week" { to } else { from });
    db.with_write(|tx| {
        tx.execute(
            "INSERT INTO diary(id, kind, window_start_ms, window_end_ms, title, body,
                               model, created_ms, updated_ms)
             VALUES (?1,?2,?3,?4,?5,?6,'generated',?7,?7)",
            rusqlite::params![id, kind, from, to, title, body, now],
        )?;
        Ok(())
    })?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 材料渲染包含三块() {
        let ev = vec![crate::generate::tick::TickEvent {
            at_ms: 1,
            channel: "mc".into(),
            kind: "died".into(),
            summary: "被炸死".into(),
            importance: 0.8,
        }];
        let tk = vec![crate::generate::tick::TickTalk {
            at_ms: 1,
            who: "姆姆".into(),
            content: "在吗".into(),
            is_self: false,
        }];
        let m = render_material(&ev, &tk, None);
        assert!(m.contains("被炸死"));
        assert!(m.contains("姆姆：在吗"));
        assert!(!m.contains("日记"), "没给日记就不该出现那段");
    }

    #[test]
    fn 周记材料带日记() {
        let m = render_material(&[], &[], Some(&[(1_700_000_000_000, "昨天钓了鱼".into())]));
        assert!(m.contains("这几天我写的日记"));
        assert!(m.contains("昨天钓了鱼"));
    }

    #[test]
    fn 长文本会截断() {
        let long = "啊".repeat(500);
        let out = one_line(&long, 100);
        assert_eq!(out.chars().count(), 101, "100 字 + 省略号");
        assert!(out.ends_with('…'));
    }

    #[test]
    fn 空材料要能被识别() {
        let m = render_material(&[], &[], None);
        // 只有两个空段落标题 —— 调用方据此判断「没有素材」
        assert!(!m.contains("被炸死"));
    }
}
