//! 从库里采集 tick 需要的东西：最近事件、最近对话、当前情绪、自我认知。
//!
//! # 为什么要单独一层
//!
//! 这些查询和「面板展示」用的查询**不一样**：
//! - 面板要全量、可翻页；
//! - tick 只要「上次之后 + 值得看的」，且要**过滤噪声**（不然几千条闲聊进上下文，
//!   既贵又让模型得出「今天特别热闹」的错误结论）。

use crate::db::Db;
use crate::error::Result;
use crate::generate::tick::{TickEvent, TickInput, TickTalk};

/// 采一次 tick 的输入。
///
/// `since_ms` 是上次 tick 的水位线；第一次跑时传一个窗口（比如 6 小时前）。
pub fn gather(
    db: &Db,
    since_ms: i64,
    now_ms: i64,
    min_importance: f64,
    max_events: usize,
    max_talks: usize,
) -> Result<TickInput> {
    let events = recent_events(db, since_ms, min_importance, max_events)?;
    let talks = recent_talks(db, since_ms, max_talks)?;
    let (mood_label, mv, ma, mi) = current_mood(db)?;
    let self_narrative = current_self(db)?;
    let relations = gather_relations(db, since_ms)?;
    Ok(TickInput {
        events,
        talks,
        relations,
        mood_label,
        mood_valence: mv,
        mood_arousal: ma,
        mood_intensity: mi,
        self_narrative,
        since_last_seconds: ((now_ms - since_ms).max(0)) / 1000,
    })
}

/// 数一下「自 `since` 以来有多少条够格的新东西」（事件 + 对话）。
///
/// 这是 tick「该不该跑」的判据。用查库而不是内存计数器，理由见
/// `exec::run_tick` 里的注释：计数器要靠写入路径调 `note_event`，
/// 而实际一个调用点都没有。
pub fn count_new(db: &Db, since: i64, to: i64, min_importance: f64) -> Result<usize> {
    db.with_read(|c| {
        let ev: i64 = c.query_row(
            "SELECT COUNT(*) FROM event
             WHERE deleted_by IS NULL AND at_ms > ?1 AND at_ms <= ?2 AND importance >= ?3",
            rusqlite::params![since, to, min_importance],
            |r| r.get(0),
        )?;
        // 对话也计入：一群人聊天本身就是「有事发生」，
        // 哪怕没有一条够格的事件（用户要的「群聊也触发情绪」正是这个）
        let tk: i64 = c.query_row(
            "SELECT COUNT(*) FROM channel_message
             WHERE deleted_by IS NULL AND at_ms > ?1 AND at_ms <= ?2",
            rusqlite::params![since, to],
            |r| r.get(0),
        )?;
        Ok((ev + tk) as usize)
    })
}

/// 按时间范围取事件（日记/周记用）。
pub fn range_events(
    db: &Db,
    from: i64,
    to: i64,
    min_importance: f64,
    limit: usize,
) -> Result<Vec<TickEvent>> {
    db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT at_ms, channel, kind, summary, importance
             FROM event
             WHERE deleted_by IS NULL AND at_ms > ?1 AND at_ms <= ?2
               AND importance >= ?3
             ORDER BY at_ms ASC LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![from, to, min_importance, limit as i64],
                |r| {
                    Ok(TickEvent {
                        at_ms: r.get(0)?,
                        channel: r.get(1)?,
                        kind: r.get(2)?,
                        summary: r.get(3)?,
                        importance: r.get(4)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

/// 按时间范围取对话。
pub fn range_talks(db: &Db, from: i64, to: i64, limit: usize) -> Result<Vec<TickTalk>> {
    db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT at_ms, sender_name, content FROM channel_message
             WHERE deleted_by IS NULL AND at_ms > ?1 AND at_ms <= ?2
             ORDER BY at_ms ASC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![from, to, limit as i64], |r| {
                Ok(TickTalk {
                    at_ms: r.get(0)?,
                    who: r
                        .get::<_, Option<String>>(1)?
                        .unwrap_or_else(|| "某人".into()),
                    content: r.get(2)?,
                    is_self: false,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

/// 按时间范围取日记（周记要看前几天的自己）。
pub fn range_diaries(db: &Db, from: i64, to: i64, limit: usize) -> Result<Vec<(i64, String)>> {
    db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT window_end_ms, body FROM diary
             WHERE deleted_by IS NULL AND kind = 'day'
               AND window_end_ms >= ?1 AND window_end_ms <= ?2
             ORDER BY window_end_ms ASC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![from, to, limit as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

/// 解析 `YYYY-MM-DD` 成**本地**当天 0 点的毫秒时间戳。
pub fn parse_day(s: &str) -> Option<i64> {
    let mut it = s.trim().split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let days = crate::generate::runner::days_from_civil_pub(y, m, d);
    Some(days * 86_400_000)
}

/// 最近 `n` 天里**还没有** day 日记的那些天（本地 0 点毫秒）。
///
/// 回填用：定时任务只处理「今天」，历史缺口得靠它找出来。
pub fn missing_days(db: &Db, n: i64) -> Result<Vec<i64>> {
    let now = crate::db::now_ms();
    let today = now.div_euclid(86_400_000);
    db.with_read(|c| {
        let mut out = Vec::new();
        for i in 1..=n {
            let day = (today - i) * 86_400_000;
            let end = day + 86_400_000;
            let exists: i64 = c.query_row(
                "SELECT COUNT(*) FROM diary
                 WHERE deleted_by IS NULL AND kind = 'day'
                   AND window_end_ms > ?1 AND window_end_ms <= ?2",
                rusqlite::params![day, end],
                |r| r.get(0),
            )?;
            if exists == 0 {
                out.push(day);
            }
        }
        Ok(out)
    })
}

/// 把毫秒时间戳格式化成 `2026-11-01`（本地）。
pub fn fmt_day(ms: i64) -> String {
    // 用 SQLite 的 localtime 省掉时区依赖；拿不到就退回 UTC 日期
    _fmt(ms)
}

fn _fmt(ms: i64) -> String {
    // 纯算术：与 runner 里的 days_from_civil 同一套
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86400);
    let (y, m, d) = crate::generate::runner::civil_from_days_pub(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// 收集「这次在场的人」及其当前关系。
///
/// 「在场」的判据：窗口内有发言，或者有关系记录且最近见过。
/// 没有这层的话模型只能凭摘要猜谁重要，而那正是关系乱涨的来源。
fn gather_relations(db: &Db, since_ms: i64) -> Result<Vec<crate::generate::tick::TickRelation>> {
    use std::collections::HashMap;
    // 窗口里每个人说了几句（参与度的直接证据，比"提到名字"可靠）
    let said: HashMap<String, usize> = db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT account, COUNT(*) FROM channel_message
             WHERE deleted_by IS NULL AND at_ms > ?1
             GROUP BY account",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![since_ms], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    r.get::<_, i64>(1)? as usize,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().collect())
    })?;

    db.with_read(|c| {
        // 取所有已建立关系的人（关系只在首次接触时建，建了就一直在）
        let mut stmt = c.prepare(
            "SELECT r.person_id, p.display_name, r.familiarity, r.trust, r.closeness
             FROM relation r
             JOIN person p ON p.person_id = r.person_id
             ORDER BY r.closeness DESC, r.person_id ASC",
        )?;
        let mut out = Vec::new();
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (pid, name, fam, trust, close) = row?;
            // 这个人的账号（用来对上发言数）
            let n = db
                .with_read(|c2| {
                    let mut st = c2.prepare(
                        "SELECT account FROM person_account
                         WHERE person_id = ?1 AND channel = 'qq'",
                    )?;
                    let accts: Vec<String> = st
                        .query_map(rusqlite::params![&pid], |r| r.get::<_, String>(0))?
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    Ok(accts.iter().filter_map(|a| said.get(a)).sum::<usize>())
                })
                .unwrap_or(0);
            out.push(crate::generate::tick::TickRelation {
                person_id: pid,
                display_name: name,
                familiarity: fam,
                trust,
                closeness: close,
                said: n,
            });
        }
        Ok(out)
    })
}

/// 给「有身份但还没关系」的人补建初始关系。
///
/// # 为什么需要这一步
///
/// 2026-11-01 查出来：`relation` 表**只有 1 条**，而 `person` 有 5 个。
/// 原因在 MC 侧 —— `Affect.fire()` 里是
///
/// ```java
/// var rel = repo.relation(companion, playerUuid);
/// if (rel != null) { ... }        // 关系不存在就直接跳过
/// ```
///
/// 全项目只有那一处调 `upsertRelation`，所以**关系永远建不起来**。
/// 而群聊里的人连 MC 都不经过，更不可能有关系。
///
/// 现在由中枢补：第一次在该人身上发生事件时建一条低值关系。
pub fn ensure_relation(db: &Db, person_id: &str) -> Result<bool> {
    if person_id.trim().is_empty() {
        return Ok(false);
    }
    let now = crate::db::now_ms();
    db.with_write(|tx| {
        // 只在**已有身份**时补：没有 person 行就不该凭空建关系
        let has: i64 = tx.query_row(
            "SELECT COUNT(*) FROM person WHERE person_id = ?1",
            rusqlite::params![person_id],
            |r| r.get(0),
        )?;
        if has == 0 {
            return Ok(false);
        }
        let n = tx.execute(
            "INSERT INTO relation(person_id, familiarity, trust, closeness,
                                  first_met_ms, last_met_ms, note)
             VALUES (?1, 1, 0, 5, ?2, ?2, '自动建立（首次接触）')
             ON CONFLICT(person_id) DO UPDATE SET last_met_ms = ?2",
            rusqlite::params![person_id, now],
        )?;
        Ok(n > 0)
    })
}

/// 应用模型给的关系增量。
///
/// **只加不减到 0 以下**（clamp 0..100），且**不衰减** ——
/// 用户明确要求「不走降低衰减」：关系不该因为「最近没怎么聊」自己掉，
/// 只有具体的事（被冒犯、被欺骗）才会让它降，而那由模型显式给出负增量。
pub fn apply_relation_delta(db: &Db, d: &crate::generate::tick::RelationDelta) -> Result<bool> {
    if d.familiarity == 0 && d.trust == 0 && d.closeness == 0 {
        return Ok(false);
    }
    let now = crate::db::now_ms();
    db.with_write(|tx| {
        let n = tx.execute(
            "UPDATE relation SET
               familiarity = MIN(100, MAX(0, familiarity + ?1)),
               trust       = MIN(100, MAX(0, trust + ?2)),
               closeness   = MIN(100, MAX(0, closeness + ?3)),
               last_met_ms = ?4
             WHERE person_id = ?5",
            rusqlite::params![d.familiarity, d.trust, d.closeness, now, d.person_id],
        )?;
        if n == 0 {
            return Ok(false);
        }
        // 变化要能解释：面板上点开一个人能看到「为什么涨了」
        if !d.reason.trim().is_empty() {
            let _ = tx.execute(
                "INSERT INTO affect_log(at_ms, person_id, kind, field, delta, reason)
                 VALUES (?1, ?2, 'relation', 'closeness', ?3, ?4)",
                rusqlite::params![now, d.person_id, d.closeness, d.reason],
            );
        }
        Ok(true)
    })
}

fn recent_events(
    db: &Db,
    since_ms: i64,
    min_importance: f64,
    limit: usize,
) -> Result<Vec<TickEvent>> {
    db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT at_ms, channel, kind, summary, importance
             FROM event
             WHERE deleted_by IS NULL AND at_ms > ?1 AND importance >= ?2
             ORDER BY at_ms ASC
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![since_ms, min_importance, limit as i64],
                |r| {
                    Ok(TickEvent {
                        at_ms: r.get(0)?,
                        channel: r.get(1)?,
                        kind: r.get(2)?,
                        summary: r.get(3)?,
                        importance: r.get(4)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

/// 最近的对话。
///
/// **自己做的话（`player.talked` 已经在 event 里了）不重复取** ——
/// 那样模型会把同一句看到两遍，判断偏重。
fn recent_talks(db: &Db, since_ms: i64, limit: usize) -> Result<Vec<TickTalk>> {
    db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT at_ms, sender_name, content
             FROM channel_message
             WHERE deleted_by IS NULL AND at_ms > ?1
             ORDER BY at_ms ASC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![since_ms, limit as i64], |r| {
                Ok(TickTalk {
                    at_ms: r.get(0)?,
                    who: r
                        .get::<_, Option<String>>(1)?
                        .unwrap_or_else(|| "某人".into()),
                    content: r.get(2)?,
                    // 群消息里没有「自己发的」（接入方不回流自己的话），
                    // 所以这里一律 false；`is_self` 留给将来的车道
                    is_self: false,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

/// 当前情绪（已按半衰期衰减到「现在」）。
fn current_mood(db: &Db) -> Result<(String, f64, f64, f64)> {
    let now = crate::db::now_ms();
    let m = db.with_read(|c| {
        Ok(c.query_row(
            "SELECT valence, arousal, dominance, label, reason, updated_ms
             FROM mood WHERE id = 1",
            [],
            |r| {
                Ok(crate::affect::Mood {
                    valence: r.get(0)?,
                    arousal: r.get(1)?,
                    dominance: r.get(2)?,
                    label: r.get(3)?,
                    reason: r.get(4)?,
                    updated_ms: r.get(5)?,
                })
            },
        )
        .ok())
    })?;
    let mut m = m.unwrap_or_else(|| crate::affect::Mood::neutral(now));
    m.decay(now - m.updated_ms, crate::affect::MOOD_HALF_LIFE_MS);
    let intensity = crate::affect::intensity(&m);
    Ok((m.label, m.valence, m.arousal, intensity))
}

/// 当前自我认知（最新一条）。
fn current_self(db: &Db) -> Result<Option<String>> {
    db.with_read(|c| {
        Ok(c.query_row(
            "SELECT body FROM self_narrative
             WHERE superseded_ms IS NULL
             ORDER BY created_ms DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok())
    })
}

// ================================================================== 写回

/// 把情绪写库。**保留旧标签**用于面板对比（`reason` 里带上模型给的原因）。
pub fn write_mood(db: &Db, m: &crate::generate::tick::MoodOut) -> Result<()> {
    let now = crate::db::now_ms();
    db.with_write(|tx| {
        tx.execute(
            "UPDATE mood SET valence = ?1, arousal = ?2, dominance = ?3,
                             label = ?4, reason = ?5, updated_ms = ?6
             WHERE id = 1",
            rusqlite::params![m.valence, m.arousal, m.dominance, m.label, m.reason, now],
        )?;
        // 没有行就插一条（首次）
        if tx.changes() == 0 {
            tx.execute(
                "INSERT INTO mood(id, owner_id, valence, arousal, dominance,
                                  label, reason, updated_ms)
                 VALUES (1, 'me', ?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![m.valence, m.arousal, m.dominance, m.label, m.reason, now],
            )?;
        }
        Ok(())
    })
}

/// 最近 `window_ms` 内是不是已经有内容很相似的手帐了？
///
/// # 为什么需要
///
/// 实测（2026-11-01 并发测试）：同一批「20 条并发测试」被两次 tick 处理，
/// 写出了两条手帐 —— 「完成 20 项并发测试（probe.concurrent 1–20）」和
/// 「完成 20 次 probe.concurrent 并发测试」。**同一件事，措辞不同**。
///
/// 所以不能靠摘要哈希，得看**内容相似度**：去掉标点与空白后，
/// 两个字符串的最长公共子序列占比过半就算重。
/// 比编辑距离便宜，而且对这种「换个说法」的场景够用。
pub fn is_duplicate_journal(db: &Db, summary: &str, window_ms: i64) -> Result<bool> {
    let now = crate::db::now_ms();
    let target = normalize_summary(summary);
    if target.chars().count() < 4 {
        return Ok(false);
    }
    db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT summary FROM journal
             WHERE deleted_by IS NULL AND at_ms > ?1
             ORDER BY at_ms DESC LIMIT 40",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![now - window_ms], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for old in rows {
            let o = normalize_summary(&old);
            if o.is_empty() {
                continue;
            }
            let common = lcs_len(&target, &o);
            let shorter = target.chars().count().min(o.chars().count()).max(1);
            if common as f64 / shorter as f64 >= 0.6 {
                return Ok(true);
            }
        }
        Ok(false)
    })
}

/// 去掉标点、空白，只留实词，便于比相似度。
fn normalize_summary(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_ascii_punctuation()
                && !c.is_whitespace()
                && !matches!(
                    c,
                    '，' | '。' | '、' | '：' | '；' | '「' | '」' | '（' | '）' | '…'
                )
        })
        .collect()
}

/// 最长公共子序列长度（按 char）。
fn lcs_len(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            cur[j] = if a[i - 1] == b[j - 1] {
                prev[j - 1] + 1
            } else {
                prev[j].max(cur[j - 1])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// 写一条手帐。
pub fn write_journal(
    db: &Db,
    j: &crate::generate::tick::JournalOut,
    channel: &str,
) -> Result<String> {
    // 同一件事写两遍是最难发现的数据污染：面板上像"记了两次"，
    // 检索时又会把权重抬高。写入前查一次近 2 小时有没有说法差不多的。
    //
    // 实测来源（2026-11-01 并发测试）：同一批事件被两次 tick 处理，
    // 写出「完成 20 项并发测试（probe.concurrent 1–20）」和
    // 「完成 20 次 probe.concurrent 并发测试」—— **同一件事，措辞不同**，
    // 所以不能靠摘要哈希，得比内容相似度。
    if is_duplicate_journal(db, &j.summary, 2 * 3_600_000)? {
        tracing::info!("手帐重复，跳过：{}", j.summary);
        return Ok(String::new());
    }
    let id = format!("j-{}-{}", crate::db::now_ms(), crate::http::rand_suffix());
    let now = crate::db::now_ms();
    db.with_write(|tx| {
        tx.execute(
            // 列名是 `type` 不是 `type_`（`type_` 是 API 层的字段名，
            // 两者不一致是历史遗留，写错会得到 "no column named type_"）
            "INSERT INTO journal(id, at_ms, type, source, summary, details,
                                 importance, confidence, channel, recall_count)
             VALUES (?1, ?2, ?3, 'generated', ?4, NULL, ?5, 0.7, ?6, 0)",
            rusqlite::params![id, now, j.kind, j.summary, j.importance, channel],
        )?;
        Ok(())
    })?;
    Ok(id)
}

/// 写自我认知：把旧的那条标成 `superseded`，插新的。
pub fn write_self(db: &Db, body: &str) -> Result<()> {
    let now = crate::db::now_ms();
    let body = body.trim().to_string();
    if body.is_empty() {
        return Ok(());
    }
    db.with_write(|tx| {
        tx.execute(
            "UPDATE self_narrative SET superseded_ms = ?1 WHERE superseded_ms IS NULL",
            rusqlite::params![now],
        )?;
        tx.execute(
            "INSERT INTO self_narrative(body, model, created_ms) VALUES (?1, 'generated', ?2)",
            rusqlite::params![body, now],
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db() -> (
        std::sync::Arc<Db>,
        crate::generate::runner::tests_support::Guard,
    ) {
        crate::generate::runner::tests_support::fresh_db()
    }

    fn ev(db: &Db, at: i64, imp: f64, summary: &str) {
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO event(at_ms, kind, channel, summary, importance, scope)
                 VALUES (?1,'said','qq',?2,?3,'companion')",
                rusqlite::params![at, summary, imp],
            )?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 低重要度事件不进上下文() {
        // 几千条「哈哈哈」进上下文既贵又误导
        let (db, _g) = tmp_db();
        ev(&db, 1000, 0.1, "哈哈哈");
        ev(&db, 2000, 0.8, "被炸死了");
        let i = gather(&db, 0, 3000, 0.35, 50, 50).unwrap();
        assert_eq!(i.events.len(), 1);
        assert_eq!(i.events[0].summary, "被炸死了");
    }

    #[test]
    fn 只看水位线之后的() {
        let (db, _g) = tmp_db();
        ev(&db, 1000, 0.9, "旧的");
        ev(&db, 5000, 0.9, "新的");
        let i = gather(&db, 3000, 6000, 0.35, 50, 50).unwrap();
        assert_eq!(i.events.len(), 1);
        assert_eq!(i.events[0].summary, "新的");
    }

    #[test]
    fn 事件按时间正序() {
        let (db, _g) = tmp_db();
        ev(&db, 3000, 0.9, "c");
        ev(&db, 1000, 0.9, "a");
        ev(&db, 2000, 0.9, "b");
        let i = gather(&db, 0, 9000, 0.35, 50, 50).unwrap();
        let got: Vec<_> = i.events.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(got, vec!["a", "b", "c"], "模型要知道先后顺序");
    }

    #[test]
    fn 删除的事件不取() {
        let (db, _g) = tmp_db();
        ev(&db, 1000, 0.9, "要删的");
        db.with_write(|tx| {
            tx.execute("UPDATE event SET deleted_by = 1", [])?;
            Ok(())
        })
        .unwrap();
        let i = gather(&db, 0, 9000, 0.35, 50, 50).unwrap();
        assert!(i.events.is_empty());
    }

    #[test]
    fn 对话也采集() {
        let (db, _g) = tmp_db();
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO channel_message(channel, group_id, account, sender_name,
                                             content, at_ms)
                 VALUES ('qq','g1','1','姆姆','今天好累',5000)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let i = gather(&db, 0, 9000, 0.35, 50, 50).unwrap();
        assert_eq!(i.talks.len(), 1);
        assert_eq!(i.talks[0].who, "姆姆");
        assert_eq!(i.talks[0].content, "今天好累");
    }

    #[test]
    fn 解析日期() {
        assert_eq!(parse_day("1970-01-01"), Some(0));
        assert_eq!(parse_day("2026-11-01"), Some(20758 * 86_400_000));
        assert_eq!(parse_day("坏"), None);
        assert_eq!(parse_day("2026-13-01"), None, "月份越界");
        assert_eq!(parse_day("2026-01-32"), None, "日期越界");
    }

    #[test]
    fn 找出缺日记的天() {
        let (db, _g) = tmp_db();
        let now = crate::db::now_ms();
        let today = now.div_euclid(86_400_000);
        // 昨天有日记，前天没有
        db.with_write(|tx| {
            let y = (today - 1) * 86_400_000;
            tx.execute(
                "INSERT INTO diary(id, kind, window_start_ms, window_end_ms,
                                   title, body, model, created_ms, updated_ms)
                 VALUES ('d1','day',?1,?2,'昨天','正文','m',1,1)",
                rusqlite::params![y, y + 86_400_000],
            )?;
            Ok(())
        })
        .unwrap();
        let miss = missing_days(&db, 3).unwrap();
        assert_eq!(miss.len(), 2, "前天和大前天缺：{miss:?}");
        assert!(
            !miss.contains(&((today - 1) * 86_400_000)),
            "昨天有，不该算缺"
        );
    }

    fn add_person(db: &Db, pid: &str, name: &str) {
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO person(person_id, display_name, status, created_ms, ignored)
                 VALUES (?1, ?2, 'confirmed', 1, 0)",
                rusqlite::params![pid, name],
            )?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 首次接触自动建关系_且给低值() {
        // 回归：2026-11-01 实测 person 有 5 个而 relation 只有 1 条 ——
        // MC 侧 `if (rel != null)` 导致关系永远建不起来。
        let (db, _g) = tmp_db();
        add_person(&db, "p1", "姆姆");
        assert!(ensure_relation(&db, "p1").unwrap());
        let (f, t, c): (i64, i64, i64) = db
            .with_read(|c2| {
                Ok(c2.query_row(
                    "SELECT familiarity, trust, closeness FROM relation WHERE person_id='p1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?)
            })
            .unwrap();
        assert_eq!((f, t, c), (1, 0, 5), "初始应该是「陌生」档，不是一上来就熟");
    }

    #[test]
    fn 重复接触不重置关系() {
        let (db, _g) = tmp_db();
        add_person(&db, "p1", "姆姆");
        ensure_relation(&db, "p1").unwrap();
        // 涨到 50
        db.with_write(|tx| {
            tx.execute(
                "UPDATE relation SET closeness = 50 WHERE person_id='p1'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        // 再「首次接触」一次
        ensure_relation(&db, "p1").unwrap();
        let c: i64 = db
            .with_read(|c2| {
                Ok(c2.query_row(
                    "SELECT closeness FROM relation WHERE person_id='p1'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(c, 50, "已存在的关系不该被重置成 5");
    }

    #[test]
    fn 没有身份的人不建关系() {
        let (db, _g) = tmp_db();
        assert!(!ensure_relation(&db, "不存在的人").unwrap());
    }

    #[test]
    fn 关系增量上下有界且不衰减() {
        let (db, _g) = tmp_db();
        add_person(&db, "p1", "姆姆");
        ensure_relation(&db, "p1").unwrap();
        db.with_write(|tx| {
            tx.execute(
                "UPDATE relation SET closeness = 99 WHERE person_id='p1'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

        // 加 3 封顶 100（不越界）
        let d = crate::generate::tick::RelationDelta {
            person_id: "p1".into(),
            familiarity: 3,
            trust: 3,
            closeness: 3,
            reason: "一起盖房子".into(),
        };
        assert!(apply_relation_delta(&db, &d).unwrap());
        let (f, c): (i64, i64) = db
            .with_read(|c2| {
                Ok(c2.query_row(
                    "SELECT familiarity, closeness FROM relation WHERE person_id='p1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(c, 100, "封顶 100");
        assert_eq!(f, 4, "1 + 3");

        // **零增量不写库**：绝大多数 tick 应该什么都不改
        let zero = crate::generate::tick::RelationDelta {
            person_id: "p1".into(),
            familiarity: 0,
            trust: 0,
            closeness: 0,
            reason: String::new(),
        };
        assert!(
            !apply_relation_delta(&db, &zero).unwrap(),
            "零增量不该算变更"
        );

        // **负增量能降，但不会低于 0**
        db.with_write(|tx| {
            tx.execute("UPDATE relation SET closeness = 2 WHERE person_id='p1'", [])?;
            Ok(())
        })
        .unwrap();
        let neg = crate::generate::tick::RelationDelta {
            person_id: "p1".into(),
            familiarity: -5,
            trust: -5,
            closeness: -5,
            reason: "被冒犯".into(),
        };
        apply_relation_delta(&db, &neg).unwrap();
        let (f2, c2v): (i64, i64) = db
            .with_read(|r| {
                Ok(r.query_row(
                    "SELECT familiarity, closeness FROM relation WHERE person_id='p1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(f2, 0, "降到 0 就不动了");
        assert_eq!(c2v, 0);
    }

    #[test]
    fn 关系不会因为时间流逝自己掉() {
        // 用户明确要求「不走降低衰减」：淡了也不扣分。
        // 这条测试钉住：把 last_met 推到很久以前，值不变。
        let (db, _g) = tmp_db();
        add_person(&db, "p1", "姆姆");
        ensure_relation(&db, "p1").unwrap();
        db.with_write(|tx| {
            tx.execute(
                "UPDATE relation SET closeness = 60, familiarity = 40, last_met_ms = 1000
                 WHERE person_id='p1'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        // 采集（会算 stage 等）之后值应该原样
        let rels = gather_relations(&db, 0).unwrap();
        let r = rels.iter().find(|x| x.person_id == "p1").unwrap();
        assert_eq!(r.closeness, 60, "不该因时间久了自己掉");
        assert_eq!(r.familiarity, 40);
    }

    #[test]
    fn 写情绪_首次插入之后更新() {
        let (db, _g) = tmp_db();
        let m = crate::generate::tick::MoodOut {
            label: "雀跃".into(),
            valence: 0.6,
            arousal: 0.7,
            dominance: 0.1,
            reason: "被夸了".into(),
        };
        write_mood(&db, &m).unwrap();
        let (label, v): (String, f64) = db
            .with_read(|c| {
                Ok(
                    c.query_row("SELECT label, valence FROM mood WHERE id=1", [], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })?,
                )
            })
            .unwrap();
        assert_eq!(label, "雀跃");
        assert!((v - 0.6).abs() < 1e-9);

        // 再写一次 → 更新而不是插第二条
        let m2 = crate::generate::tick::MoodOut {
            label: "平静".into(),
            valence: 0.0,
            ..m
        };
        write_mood(&db, &m2).unwrap();
        let n: i64 = db
            .with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM mood", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 1, "mood 表只该有一行");
    }

    #[test]
    fn 写手帐() {
        let (db, _g) = tmp_db();
        let id = write_journal(
            &db,
            &crate::generate::tick::JournalOut {
                kind: "died".into(),
                summary: "被苦力怕炸死".into(),
                importance: 0.8,
            },
            "mc",
        )
        .unwrap();
        let (s, src): (String, String) = db
            .with_read(|c| {
                Ok(c.query_row(
                    "SELECT summary, source FROM journal WHERE id = ?1",
                    rusqlite::params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(s, "被苦力怕炸死");
        assert_eq!(src, "generated", "要能区分是中枢生成的");
    }

    #[test]
    fn 同一件事换个说法不会记两条() {
        // 实测来源：并发测试时同一批事件被处理两次，写出
        // 「完成 20 项并发测试（probe.concurrent 1–20）」和
        // 「完成 20 次 probe.concurrent 并发测试」—— 同一件事。
        let (db, _g) = tmp_db();
        let j1 = crate::generate::tick::JournalOut {
            kind: "task_done".into(),
            summary: "完成 20 项并发测试（probe.concurrent 1–20）".into(),
            importance: 0.5,
        };
        assert!(
            !write_journal(&db, &j1, "mc").unwrap().is_empty(),
            "第一条要写进去"
        );

        let j2 = crate::generate::tick::JournalOut {
            kind: "task_done".into(),
            summary: "完成 20 次 probe.concurrent 并发测试".into(),
            importance: 0.5,
        };
        assert!(
            write_journal(&db, &j2, "mc").unwrap().is_empty(),
            "同一件事换个说法不该再记一条"
        );
        let n: i64 = db
            .with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn 不同的事照样记() {
        let (db, _g) = tmp_db();
        for s in ["被苦力怕炸死了", "把房顶盖完了", "钓到三条鳕鱼"] {
            let j = crate::generate::tick::JournalOut {
                kind: "note".into(),
                summary: s.into(),
                importance: 0.5,
            };
            assert!(
                !write_journal(&db, &j, "mc").unwrap().is_empty(),
                "{s} 该写进去"
            );
        }
        let n: i64 = db
            .with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 3, "三件不同的事都该留");
    }

    #[test]
    fn 相似度算法() {
        assert_eq!(lcs_len("abc", "abc"), 3);
        assert_eq!(lcs_len("abc", "xyz"), 0);
        assert!(lcs_len("完成并发测试", "完成了并发测试") >= 4);
    }

    #[test]
    fn 写自我认知会作废旧的那条() {
        let (db, _g) = tmp_db();
        write_self(&db, "我是一个爱吃米饭的鲸鱼娘").unwrap();
        write_self(&db, "我更在意陪人说话了").unwrap();
        let cur = current_self(&db).unwrap();
        assert_eq!(cur.as_deref(), Some("我更在意陪人说话了"));
        let n: i64 = db
            .with_read(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM self_narrative WHERE superseded_ms IS NOT NULL",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(n, 1, "旧的要被标记，不是删掉（要能回看成长轨迹）");
    }

    #[test]
    fn 空自我认知不写() {
        let (db, _g) = tmp_db();
        write_self(&db, "   ").unwrap();
        assert!(current_self(&db).unwrap().is_none());
    }
}
