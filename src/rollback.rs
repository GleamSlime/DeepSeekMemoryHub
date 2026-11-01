//! 逻辑回滚：把记忆「忘记」到某个时间点，但**可撤销**。
//!
//! # 为什么是逻辑回滚而不是真删除
//!
//! 用户的要求是「回滚相当于忘记目标天数之后的所有记忆」。
//! 直译成物理删除很容易，但那意味着**一次误点就永久销毁数据**——
//! 而这个系统里最不可再生的东西恰恰是记忆本身。
//!
//! 所以这里采用**标记删除**：
//!
//! - 每条可遗忘的记录都有 `deleted_by INTEGER`。`NULL` = 仍然有效。
//! - 回滚到时间点 T 时，把 `at_ms > T` 的记录的 `deleted_by` 填成本次回滚的 id。
//! - 所有读路径一律带 `WHERE deleted_by IS NULL`。
//! - 「撤销回滚」= 把 `deleted_by = 那次回滚的 id` 的行清回 `NULL`。
//!
//! 代价是磁盘占用略高，旧数据要等到 [`purge`] 才真正回收。
//! 这个取舍是刻意的：**先保住可撤销，再谈空间**。
//!
//! # 边界
//!
//! 有两类东西**不回滚**：
//!
//! 1. **身份绑定**（`person` / `person_account`）。忘掉记忆不等于忘掉「这个人是谁」——
//!    解绑是另一件事，由 `/v1/person/unbind` 负责。
//! 2. **审计日志**（`snapshot` / `rollback_log` / `recall_log`）。
//!    回滚本身就是被审计的对象，如果连审计都回滚掉，就再也说不清发生过什么。

use crate::db::{now_ms, Db};
use crate::error::{Error, Result};

/// 哪些表参与回滚，以及用哪个列判断「发生在什么时候」。
///
/// 显式列表而不是反射：**回滚漏掉一张表 = 记忆没忘干净**，
/// 这种事不该靠「猜对了表名」来保证。
pub const ROLLBACK_TABLES: &[(&str, &str)] = &[
    // (表名, 时间列)
    ("event", "at_ms"),
    ("journal", "at_ms"),
    ("fact", "updated_ms"),
    ("diary", "updated_ms"),
    ("self_narrative", "created_ms"),
    ("promise", "updated_ms"),
    ("channel_message", "at_ms"),
    ("recall_log", "at_ms"),
    // 语音与记忆画面也是记忆：回滚到某天之前，那之后录的语音、
    // 收的图都该被忘掉。不包含它们的话「回滚 = 忘掉之后的一切」就是假的。
    ("voice", "created_ms"),
    ("image", "created_ms"),
];

/// 一次回滚的结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RollbackOutcome {
    /// `rollback_log.id`，撤销时要用它。
    pub rollback_id: i64,
    /// 回滚到哪一刻。
    pub target_ms: i64,
    /// 各表被标记为「遗忘」的行数。
    pub affected: Vec<TableAffected>,
    pub total: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TableAffected {
    pub table: String,
    pub rows: i64,
}

/// 回滚到 `target_ms`：把**晚于该时刻**的记忆标记为遗忘。
///
/// 注意是**严格大于**。回滚到某个快照时刻 T 时，T 当下发生的事情要保留，
/// 因为那正是快照里有的东西；只有 T 之后新增的才该被忘掉。
pub fn rollback_to(db: &Db, target_ms: i64, note: Option<&str>) -> Result<RollbackOutcome> {
    let now = now_ms();
    if target_ms <= 0 {
        return Err(Error::Rollback("目标时间必须是正数毫秒时间戳".into()));
    }
    if target_ms > now {
        return Err(Error::Rollback(format!(
            "目标时间在未来（target={target_ms}, now={now}）。回滚不能忘掉还没发生的事。"
        )));
    }

    // 先插审计行拿到 id，再按 id 打标记——顺序不能反，
    // 否则中途失败会留下一堆「没有出处」的删除标记。
    let (rollback_id, affected) = db.with_write(|tx| {
        tx.execute(
            "INSERT INTO rollback_log(at_ms, target_ms, mode, affected_json, note)
             VALUES (?1, ?2, 'logical', '', ?3)",
            rusqlite::params![now, target_ms, note],
        )?;
        let rollback_id = tx.last_insert_rowid();

        let mut affected = Vec::new();
        for (table, time_col) in ROLLBACK_TABLES {
            // recall_log 没有 deleted_by 列，单独处理。
            if *table == "recall_log" {
                let rows = tx.execute(
                    "DELETE FROM recall_log WHERE at_ms > ?1",
                    rusqlite::params![target_ms],
                )? as i64;
                if rows > 0 {
                    affected.push(TableAffected {
                        table: table.to_string(),
                        rows,
                    });
                }
                continue;
            }
            let sql = format!(
                "UPDATE {table} SET deleted_by = ?1
                 WHERE {time_col} > ?2 AND deleted_by IS NULL"
            );
            let rows = tx.execute(&sql, rusqlite::params![rollback_id, target_ms])? as i64;
            if rows > 0 {
                affected.push(TableAffected {
                    table: table.to_string(),
                    rows,
                });
            }
        }

        let json = serde_json::to_string(&affected).unwrap_or_else(|_| "[]".into());
        tx.execute(
            "UPDATE rollback_log SET affected_json = ?1 WHERE id = ?2",
            rusqlite::params![json, rollback_id],
        )?;
        Ok((rollback_id, affected))
    })?;

    let total = affected.iter().map(|a| a.rows).sum();
    Ok(RollbackOutcome {
        rollback_id,
        target_ms,
        affected,
        total,
    })
}

/// 撤销一次回滚：把那次标记为遗忘的行全部恢复。
///
/// 只恢复 `deleted_by = rollback_id` 的行，所以**多次回滚可以按相反顺序逐层撤销**。
/// 乱序撤销（先撤早的、再撤晚的）也能工作，因为每行只认自己那一次的 id。
pub fn undo(db: &Db, rollback_id: i64) -> Result<RollbackOutcome> {
    let target_ms: i64 = db.with_read(|c| {
        c.query_row(
            "SELECT target_ms FROM rollback_log WHERE id = ?1",
            rusqlite::params![rollback_id],
            |r| r.get(0),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                Error::NotFound(format!("回滚记录 {rollback_id}"))
            }
            other => other.into(),
        })
    })?;

    let affected = db.with_write(|tx| {
        let mut affected = Vec::new();
        for (table, _) in ROLLBACK_TABLES {
            // recall_log 是物理删除的，撤销时无法恢复——这是有意的取舍：
            // 它只是「谁进过上下文」的统计，不是记忆本身。
            if *table == "recall_log" {
                continue;
            }
            let sql = format!("UPDATE {table} SET deleted_by = NULL WHERE deleted_by = ?1");
            let rows = tx.execute(&sql, rusqlite::params![rollback_id])? as i64;
            if rows > 0 {
                affected.push(TableAffected {
                    table: table.to_string(),
                    rows,
                });
            }
        }
        tx.execute(
            "INSERT INTO rollback_log(at_ms, target_ms, mode, affected_json, undo_of)
             VALUES (?1, ?2, 'undo', ?3, ?4)",
            rusqlite::params![
                now_ms(),
                target_ms,
                serde_json::to_string(&affected).unwrap_or_else(|_| "[]".into()),
                rollback_id
            ],
        )?;
        Ok(affected)
    })?;

    let total = affected.iter().map(|a| a.rows).sum();
    Ok(RollbackOutcome {
        rollback_id,
        target_ms,
        affected,
        total,
    })
}

/// 彻底清理：把已经被遗忘（`deleted_by IS NOT NULL`）的行物理删除。
///
/// 这是不可逆的。默认**不**自动执行——由操作者显式调用，
/// 或者在保留期过后由维护任务执行。
pub fn purge(db: &Db) -> Result<Vec<TableAffected>> {
    db.with_write(|tx| {
        let mut out = Vec::new();
        for (table, _) in ROLLBACK_TABLES {
            if *table == "recall_log" {
                continue;
            }
            let rows = tx.execute(
                &format!("DELETE FROM {table} WHERE deleted_by IS NOT NULL"),
                [],
            )? as i64;
            if rows > 0 {
                out.push(TableAffected {
                    table: table.to_string(),
                    rows,
                });
            }
        }
        Ok(out)
    })
}

/// 当前「被遗忘但尚未清理」的行数。面板上要能看到它，否则用户不知道空间去哪了。
pub fn forgotten_count(db: &Db) -> Result<i64> {
    db.with_read(|c| {
        let mut total = 0i64;
        for (table, _) in ROLLBACK_TABLES {
            if *table == "recall_log" {
                continue;
            }
            let n: i64 = c.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE deleted_by IS NOT NULL"),
                [],
                |r| r.get(0),
            )?;
            total += n;
        }
        Ok(total)
    })
}

/// 最近的回滚历史，供面板展示与撤销操作挑选 id。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RollbackRecord {
    pub id: i64,
    pub at_ms: i64,
    pub target_ms: i64,
    pub mode: String,
    pub undo_of: Option<i64>,
    pub note: Option<String>,
}

pub fn history(db: &Db, limit: usize) -> Result<Vec<RollbackRecord>> {
    db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT id, at_ms, target_ms, mode, undo_of, note
             FROM rollback_log ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![limit as i64], |r| {
                Ok(RollbackRecord {
                    id: r.get(0)?,
                    at_ms: r.get(1)?,
                    target_ms: r.get(2)?,
                    mode: r.get(3)?,
                    undo_of: r.get(4)?,
                    note: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    /// 建一个库，并按「天数」塞入事件。`base` 是「现在」。
    fn seeded() -> (tempfile::TempDir, std::sync::Arc<Db>, i64) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        let now = crate::db::now_ms();
        // 造 10 天，每天一条 event + 一条 journal + 一条 fact
        db.with_write(|tx| {
            for d in 0..10 {
                let at = now - (10 - d) * DAY;
                tx.execute(
                    "INSERT INTO event(at_ms, kind, channel, summary, importance, scope)
                     VALUES (?1, 'test', 'mc', ?2, 0.5, 'companion')",
                    rusqlite::params![at, format!("day{d}")],
                )?;
                tx.execute(
                    "INSERT INTO journal(id, at_ms, type, source, summary, importance, confidence)
                     VALUES (?1, ?2, 'task_done', 'plugin', ?3, 0.5, 0.9)",
                    rusqlite::params![format!("j{d}"), at, format!("day{d}")],
                )?;
                tx.execute(
                    "INSERT INTO fact(id, key, value, scope, status, tier, confidence,
                                      importance, created_ms, updated_ms)
                     VALUES (?1, 'k', 'v', 'companion', 'confirmed', 'extracted', 0.8, 0.5, ?2, ?2)",
                    rusqlite::params![format!("f{d}"), at],
                )?;
            }
            Ok(())
        })
        .unwrap();
        (dir, db, now)
    }

    fn live_events(db: &Db) -> i64 {
        db.with_read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM event WHERE deleted_by IS NULL",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap()
    }

    fn all_events(db: &Db) -> i64 {
        db.with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))?))
            .unwrap()
    }

    #[test]
    fn 回滚忘掉目标时刻之后的记忆() {
        let (_d, db, now) = seeded();
        // 回滚到 3 天前：应该只剩 7 天（day0..day6 中 <= now-3day 的部分）
        let target = now - 3 * DAY;
        let out = rollback_to(&db, target, Some("测试回滚")).unwrap();
        assert!(out.total > 0, "应该有记录被遗忘：{out:?}");

        let live = live_events(&db);
        assert!(live < 10, "应有事件被遗忘，实际剩余 {live}");
        // 物理行还在（逻辑删除）。
        assert_eq!(all_events(&db), 10, "逻辑回滚不该物理删除");
    }

    #[test]
    fn 回滚的边界是严格大于() {
        let (_d, db, now) = seeded();
        let exact = now - 5 * DAY;
        // 先记下恰好等于 target 的行数
        let at_target: i64 = db
            .with_read(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM event WHERE at_ms = ?1",
                    rusqlite::params![exact],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        rollback_to(&db, exact, None).unwrap();
        let still_there: i64 = db
            .with_read(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM event WHERE at_ms = ?1 AND deleted_by IS NULL",
                    rusqlite::params![exact],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(
            at_target, still_there,
            "恰好等于目标时刻的记录必须保留（快照里就有它）"
        );
    }

    #[test]
    fn 回滚可以撤销() {
        let (_d, db, now) = seeded();
        let before = live_events(&db);
        let out = rollback_to(&db, now - 3 * DAY, None).unwrap();
        assert!(live_events(&db) < before);

        let undone = undo(&db, out.rollback_id).unwrap();
        assert_eq!(undone.total, out.total, "撤销应恢复同样多的行");
        assert_eq!(live_events(&db), before, "撤销后应完全恢复");
    }

    #[test]
    fn 多次回滚可逐层撤销() {
        let (_d, db, now) = seeded();
        let r1 = rollback_to(&db, now - 2 * DAY, Some("第一次")).unwrap();
        let after1 = live_events(&db);
        let r2 = rollback_to(&db, now - 6 * DAY, Some("第二次")).unwrap();
        let after2 = live_events(&db);
        assert!(after2 < after1, "第二次回滚更早，应该忘得更多");

        // 撤销第二次 → 回到第一次之后的状态
        undo(&db, r2.rollback_id).unwrap();
        assert_eq!(live_events(&db), after1, "撤销第二次应回到第一次之后");

        // 再撤销第一次 → 回到最初
        undo(&db, r1.rollback_id).unwrap();
        assert_eq!(live_events(&db), 10, "全部撤销应回到最初");
    }

    #[test]
    fn 乱序撤销也能工作() {
        let (_d, db, now) = seeded();
        let r1 = rollback_to(&db, now - 2 * DAY, None).unwrap();
        let r2 = rollback_to(&db, now - 6 * DAY, None).unwrap();
        // 先撤早的（这是「错」的顺序），再撤晚的。
        undo(&db, r1.rollback_id).unwrap();
        undo(&db, r2.rollback_id).unwrap();
        assert_eq!(
            live_events(&db),
            10,
            "每行只认自己那次 id，乱序也应恢复干净"
        );
    }

    #[test]
    fn 回滚到未来会被拒绝() {
        let (_d, db, _now) = seeded();
        let err = rollback_to(&db, crate::db::now_ms() + 10 * DAY, None).unwrap_err();
        assert!(matches!(err.kind(), crate::error::ErrorKind::Rollback));
        assert!(err.to_string().contains("未来"), "错误信息应说清原因");
    }

    #[test]
    fn 回滚到非正数会被拒绝() {
        let (_d, db, _now) = seeded();
        assert!(rollback_to(&db, 0, None).is_err());
        assert!(rollback_to(&db, -1, None).is_err());
    }

    #[test]
    fn 撤销不存在的回滚报找不到() {
        let (_d, db, _now) = seeded();
        let err = undo(&db, 99999).unwrap_err();
        assert!(matches!(err.kind(), crate::error::ErrorKind::NotFound));
    }

    #[test]
    fn 回滚不动身份绑定() {
        let (_d, db, now) = seeded();
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO person(person_id, display_name, status, created_ms)
                 VALUES ('p-1','某人',?1,?2)",
                rusqlite::params!["confirmed", now],
            )?;
            tx.execute(
                "INSERT INTO person_account(channel, account, person_id, first_seen_ms, last_seen_ms)
                 VALUES ('mc','uuid-1','p-1',?1,?2)",
                rusqlite::params![now, now],
            )?;
            Ok(())
        })
        .unwrap();

        // 回滚到很早，连「这个人」的创建时间之后都忘掉。
        rollback_to(&db, now - 30 * DAY, None).unwrap();

        let people: i64 = db
            .with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM person", [], |r| r.get(0))?))
            .unwrap();
        let accounts: i64 = db
            .with_read(
                |c| Ok(c.query_row("SELECT COUNT(*) FROM person_account", [], |r| r.get(0))?),
            )
            .unwrap();
        assert_eq!(people, 1, "身份绑定不该被记忆回滚影响");
        assert_eq!(accounts, 1, "渠道账号不该被记忆回滚影响");
    }

    #[test]
    fn 回滚留审计记录() {
        let (_d, db, now) = seeded();
        let out = rollback_to(&db, now - 3 * DAY, Some("因为测试")).unwrap();
        let hist = history(&db, 10).unwrap();
        assert!(!hist.is_empty());
        let rec = hist.iter().find(|r| r.id == out.rollback_id).unwrap();
        assert_eq!(rec.mode, "logical");
        assert_eq!(rec.note.as_deref(), Some("因为测试"));
        assert_eq!(rec.target_ms, now - 3 * DAY);
    }

    #[test]
    fn 撤销本身也被审计() {
        let (_d, db, now) = seeded();
        let out = rollback_to(&db, now - 3 * DAY, None).unwrap();
        undo(&db, out.rollback_id).unwrap();
        let hist = history(&db, 10).unwrap();
        let undo_rec = hist
            .iter()
            .find(|r| r.mode == "undo")
            .expect("应有撤销记录");
        assert_eq!(undo_rec.undo_of, Some(out.rollback_id));
    }

    #[test]
    fn 彻底清理后不可撤销() {
        let (_d, db, now) = seeded();
        let out = rollback_to(&db, now - 3 * DAY, None).unwrap();
        assert!(forgotten_count(&db).unwrap() > 0);

        let purged = purge(&db).unwrap();
        assert!(!purged.is_empty());
        assert_eq!(forgotten_count(&db).unwrap(), 0, "清理后不该还有遗忘标记");
        assert!(all_events(&db) < 10, "清理应物理删除行");

        // 此时撤销也无行可恢复。
        let undone = undo(&db, out.rollback_id).unwrap();
        assert_eq!(undone.total, 0, "已清理的数据撤不回来");
    }

    #[test]
    fn 遗忘计数反映待清理量() {
        let (_d, db, now) = seeded();
        let out = rollback_to(&db, now - 3 * DAY, None).unwrap();
        let n = forgotten_count(&db).unwrap();
        assert_eq!(n, out.total, "遗忘计数应等于本次标记的行数");
    }

    #[test]
    fn 回滚覆盖所有渠道() {
        // 单主体后没有「多个对象」，但仍有**多个渠道**共用一条时间轴。
        // 回滚必须覆盖全部渠道，不能只忘掉 MC 的。
        let (_d, db, now) = seeded();
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO event(at_ms, kind, channel, summary, importance, scope)
                 VALUES (?1, 'test', 'qq', '群里的消息', 0.5, 'companion')",
                rusqlite::params![now],
            )?;
            Ok(())
        })
        .unwrap();
        rollback_to(&db, now - 3 * DAY, None).unwrap();
        let qq_live: i64 = db
            .with_read(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM event WHERE channel='qq' AND deleted_by IS NULL",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(qq_live, 0, "时间轴是全局的，回滚应覆盖所有渠道");
    }
}
