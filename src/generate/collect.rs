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
    Ok(TickInput {
        events,
        talks,
        mood_label,
        mood_valence: mv,
        mood_arousal: ma,
        mood_intensity: mi,
        self_narrative,
        since_last_seconds: ((now_ms - since_ms).max(0)) / 1000,
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

/// 写一条手帐。
pub fn write_journal(
    db: &Db,
    j: &crate::generate::tick::JournalOut,
    channel: &str,
) -> Result<String> {
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
