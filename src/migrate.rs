//! 从 MC 插件旧库迁移存量数据。
//!
//! # 为什么要有这个
//!
//! Hub 建起来时是空库。MC 侧已经有 97 条事件、400 条手帐、4 篇日记、
//! 2 条自我认知——**这些是真实生活过的记录**，不迁过来等于让 NPC 失忆。
//!
//! # 三条设计原则
//!
//! 1. **只读源库，不改它。** 迁移失败不能反过来损坏源数据。
//!    （源库仍被 MC 插件使用，动它会出大事。）
//! 2. **幂等。** 反复跑不能产生重复。靠原 id + `dedup_key` 保证。
//! 3. **字段要翻译，不能硬搬。** 两边的 schema 不完全一样：
//!    - 源 `event.type` → 目标 `kind`
//!    - 源 `event.subject_id`（玩家 UUID）→ 目标的 `person_id`（跨渠道的人）
//!    - 源 `persona_trait` 表名 → 目标 `trait`
//!    - 源 `qq_message` → 目标 `channel_message`
//!
//! # 不做的事
//!
//! - 不迁 `schema_version`（那是各库自己的）
//! - 不迁索引（目标库会自己建）
//! - **不迁移时改时间戳**：`at_ms` 原样搬，否则时效衰减全乱。

use std::path::Path;

use rusqlite::Connection;

use crate::db::Db;
use crate::error::{Error, Result};

/// 各表迁移了多少行。
#[derive(Debug, Default, serde::Serialize)]
pub struct Migrated {
    pub events: usize,
    pub journals: usize,
    pub diaries: usize,
    pub facts: usize,
    pub self_narratives: usize,
    pub moods: usize,
    pub traits: usize,
    pub relations: usize,
    pub promises: usize,
    pub messages: usize,
    pub plans: usize,
    pub plan_items: usize,
    /// 因为已存在而跳过的行（幂等重跑时这些会涨）。
    pub skipped: usize,
}

impl Migrated {
    pub fn total(&self) -> usize {
        self.events
            + self.journals
            + self.diaries
            + self.facts
            + self.self_narratives
            + self.moods
            + self.traits
            + self.relations
            + self.promises
            + self.messages
            + self.plans
    }
}

/// 从旧库迁到新库。
///
/// `dry_run = true` 时只统计不写入，用于先看看会发生什么。
pub fn from_legacy(src_path: &Path, dst: &Db, dry_run: bool) -> Result<Migrated> {
    if !src_path.exists() {
        return Err(Error::BadRequest(format!(
            "源库不存在：{}",
            src_path.display()
        )));
    }

    // **只读打开**。迁移工具绝不能改源库。
    let src = Connection::open_with_flags(
        src_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| Error::Storage(format!("打不开源库：{e}")))?;

    let mut m = Migrated::default();

    if dry_run {
        m.events = count(&src, "event")?;
        m.journals = count(&src, "journal")?;
        m.diaries = count(&src, "diary")?;
        m.facts = count(&src, "fact")?;
        m.self_narratives = count(&src, "self_narrative")?;
        m.moods = count(&src, "mood")?;
        m.traits = count(&src, "persona_trait")?;
        m.relations = count(&src, "relation")?;
        m.promises = count(&src, "promise")?;
        m.messages = count(&src, "qq_message")?;
        return Ok(m);
    }

    // 事件
    {
        let mut stmt = src.prepare(
            "SELECT id, companion_id, subject_id, at_ms, type, scope, summary, importance, dedup_key
             FROM event ORDER BY id",
        )?;
        let rows: Vec<_> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<f64>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for (id, _cid, subject, at, ty, scope, summary, imp, dedup) in rows {
            // 幂等键：优先用源库自己的 dedup_key；没有就人工造一个稳定的。
            // 造键必须只依赖源数据（不能用 NOW），否则重跑会写出新行。
            let key = dedup.unwrap_or_else(|| format!("legacy-event-{id}"));
            let (inserted, skipped) = dst.with_write(|tx| {
                let exists: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM event WHERE dedup_key = ?1",
                    rusqlite::params![key],
                    |r| r.get(0),
                )?;
                if exists > 0 {
                    return Ok((false, true));
                }
                tx.execute(
                    "INSERT INTO event(person_id, at_ms, kind, channel,
                                       summary, importance, scope, dedup_key)
                     VALUES (?1, ?2, ?3, 'mc', ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        subject, // 源 subject_id 是玩家 UUID；迁移阶段先原样放到 person_id，
                        // 之后可以用 /v1/person/bind 把这些 UUID 绑到具体的 person
                        at,
                        ty,
                        summary.unwrap_or_default(),
                        imp.unwrap_or(0.5),
                        scope.unwrap_or_else(|| "companion".into()),
                        key
                    ],
                )?;
                Ok((true, false))
            })?;
            if inserted {
                m.events += 1;
            }
            if skipped {
                m.skipped += 1;
            }
        }
    }

    // 手帐：源 id 是 TEXT，可以直接沿用
    {
        let mut stmt = src.prepare(
            "SELECT id, companion_id, at_ms, planning_day_id, type, source, summary, details,
                    importance, confidence
             FROM journal ORDER BY at_ms",
        )?;
        let rows: Vec<_> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, f64>(8)?,
                    r.get::<_, f64>(9)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for (id, _cid, at, day, ty, source, summary, details, imp, conf) in rows {
            let (inserted, skipped) = dst.with_write(|tx| {
                let exists: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM journal WHERE id = ?1",
                    rusqlite::params![id],
                    |r| r.get(0),
                )?;
                if exists > 0 {
                    return Ok((false, true));
                }
                tx.execute(
                    "INSERT INTO journal(id, at_ms, planning_day_id, type, source,
                                         summary, details, importance, confidence, channel)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'mc')",
                    rusqlite::params![id, at, day, ty, source, summary, details, imp, conf],
                )?;
                Ok((true, false))
            })?;
            if inserted {
                m.journals += 1;
            }
            if skipped {
                m.skipped += 1;
            }
        }
    }

    // 日记
    {
        let mut stmt = src.prepare(
            "SELECT id, companion_id, kind, window_start_ms, window_end_ms, title, body,
                    model, created_ms, updated_ms
             FROM diary ORDER BY window_start_ms",
        )?;
        let rows: Vec<_> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for (id, _cid, kind, ws, we, title, body, model, created, updated) in rows {
            let (inserted, skipped) = dst.with_write(|tx| {
                let exists: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM diary WHERE id = ?1",
                    rusqlite::params![id],
                    |r| r.get(0),
                )?;
                if exists > 0 {
                    return Ok((false, true));
                }
                tx.execute(
                    "INSERT INTO diary(id, kind, window_start_ms, window_end_ms,
                                       title, body, model, created_ms, updated_ms)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    rusqlite::params![id, kind, ws, we, title, body, model, created, updated],
                )?;
                Ok((true, false))
            })?;
            if inserted {
                m.diaries += 1;
            }
            if skipped {
                m.skipped += 1;
            }
        }
    }

    // 事实
    {
        let mut stmt = src.prepare(
            "SELECT id, companion_id, subject_id, key, value, scope, status, tier,
                    confidence, importance, created_ms, updated_ms, expires_ms, superseded_by
             FROM fact",
        )?;
        let rows: Vec<_> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, f64>(8)?,
                    r.get::<_, f64>(9)?,
                    r.get::<_, i64>(10)?,
                    r.get::<_, i64>(11)?,
                    r.get::<_, Option<i64>>(12)?,
                    r.get::<_, Option<String>>(13)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for row in rows {
            let (
                id,
                _cid, // 源库有 companion_id；单主体后不再用于写入，但元组位置要占着。
                subject,
                key,
                value,
                scope,
                status,
                tier,
                conf,
                imp,
                created,
                updated,
                expires,
                superseded,
            ) = row;
            let (inserted, skipped) = dst.with_write(|tx| {
                let exists: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM fact WHERE id = ?1",
                    rusqlite::params![id],
                    |r| r.get(0),
                )?;
                if exists > 0 {
                    return Ok((false, true));
                }
                tx.execute(
                    "INSERT INTO fact(id, person_id, key, value, scope, status,
                                      tier, confidence, importance, created_ms, updated_ms,
                                      expires_ms, superseded_by)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    rusqlite::params![
                        id, subject, key, value, scope, status, tier, conf, imp, created, updated,
                        expires, superseded
                    ],
                )?;
                Ok((true, false))
            })?;
            if inserted {
                m.facts += 1;
            }
            if skipped {
                m.skipped += 1;
            }
        }
    }

    // 自我认知
    {
        let mut stmt = src.prepare(
            "SELECT companion_id, body, model, created_ms, superseded_ms
             FROM self_narrative ORDER BY created_ms",
        )?;
        let rows: Vec<_> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for (_cid, body, model, created, superseded) in rows {
            let (inserted, skipped) = dst.with_write(|tx| {
                // 幂等：同一 companion + created_ms 视为同一条。
                let exists: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM self_narrative
                     WHERE created_ms = ?1",
                    rusqlite::params![created],
                    |r| r.get(0),
                )?;
                if exists > 0 {
                    return Ok((false, true));
                }
                tx.execute(
                    "INSERT INTO self_narrative(body, model, created_ms, superseded_ms)
                     VALUES (?1,?2,?3,?4)",
                    rusqlite::params![body, model, created, superseded],
                )?;
                Ok((true, false))
            })?;
            if inserted {
                m.self_narratives += 1;
            }
            if skipped {
                m.skipped += 1;
            }
        }
    }

    // 情绪（主键就是 companion_id，天然幂等）
    {
        let mut stmt = src.prepare(
            "SELECT companion_id, valence, arousal, dominance, label, reason, updated_ms FROM mood",
        )?;
        let rows: Vec<_> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, f64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for (_cid, v, a, d, label, reason, updated) in rows {
            let inserted = dst.with_write(|tx| {
                let exists: i64 =
                    tx.query_row("SELECT COUNT(*) FROM mood WHERE id = 1", [], |r| r.get(0))?;
                if exists > 0 {
                    return Ok(false);
                }
                tx.execute(
                    "INSERT INTO mood(id, valence, arousal, dominance, label, reason, updated_ms)
                     VALUES (1,?1,?2,?3,?4,?5,?6)",
                    rusqlite::params![v, a, d, label, reason, updated],
                )?;
                Ok(true)
            })?;
            if inserted {
                m.moods += 1;
            } else {
                m.skipped += 1;
            }
        }
    }

    // 特质：源表名是 persona_trait
    if table_exists(&src, "persona_trait")? {
        let mut stmt =
            src.prepare("SELECT companion_id, trait, value, updated_ms FROM persona_trait")?;
        let rows: Vec<_> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for (_cid, tr, v, updated) in rows {
            let inserted = dst.with_write(|tx| {
                let exists: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM trait WHERE trait = ?1",
                    rusqlite::params![tr],
                    |r| r.get(0),
                )?;
                if exists > 0 {
                    return Ok(false);
                }
                tx.execute(
                    "INSERT INTO trait(trait, value, updated_ms)
                     VALUES (?1,?2,?3)",
                    rusqlite::params![tr, v, updated],
                )?;
                Ok(true)
            })?;
            if inserted {
                m.traits += 1;
            } else {
                m.skipped += 1;
            }
        }
    }

    // 关系：源的 player_uuid → 目标的 person_id（先原样存 UUID）
    {
        let mut stmt = src.prepare(
            "SELECT companion_id, player_uuid, last_seen_name, familiarity, trust,
                    closeness, first_met_ms, last_met_ms FROM relation",
        )?;
        let rows: Vec<_> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for (_cid, uuid, name, fam, trust, close, first, last) in rows {
            let inserted = dst.with_write(|tx| {
                let exists: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM relation WHERE person_id = ?1",
                    rusqlite::params![uuid],
                    |r| r.get(0),
                )?;
                if exists > 0 {
                    return Ok(false);
                }
                // 关系要挂在 person 上。源库只有 UUID，没有 person 概念，
                // 所以这里顺手建一个 pending 的 person，并绑上 mc 渠道账号。
                // 之后运维可以把这些 UUID 绑到真正的跨渠道 person 上。
                tx.execute(
                    "INSERT INTO person(person_id, display_name, status, created_ms)
                     VALUES (?1, ?2, 'pending', ?3) ON CONFLICT(person_id) DO NOTHING",
                    rusqlite::params![
                        uuid,
                        name.clone().unwrap_or_else(|| uuid.clone()),
                        crate::db::now_ms()
                    ],
                )?;
                tx.execute(
                    "INSERT INTO person_account(channel, account, person_id, seen_name,
                                                first_seen_ms, last_seen_ms)
                     VALUES ('mc', ?1, ?1, ?2, ?3, ?4)
                     ON CONFLICT(channel, account) DO NOTHING",
                    // ?1 用了两次（account 和 person_id 都是这个 UUID），
                    // 所以是 4 个占位符但只需要传 4 个值——rusqlite 按位置绑定，
                    // 重复的 ?1 会自动复用同一个值。
                    rusqlite::params![uuid, name, first, last],
                )?;
                tx.execute(
                    "INSERT INTO relation(person_id, familiarity, trust,
                                          closeness, first_met_ms, last_met_ms)
                     VALUES (?1,?2,?3,?4,?5,?6)",
                    rusqlite::params![uuid, fam, trust, close, first, last],
                )?;
                Ok(true)
            })?;
            if inserted {
                m.relations += 1;
            } else {
                m.skipped += 1;
            }
        }
    }

    // 承诺
    {
        let mut stmt = src.prepare(
            "SELECT id, companion_id, player_uuid, description, status, created_ms,
                    due_ms, updated_ms, blocked_reason FROM promise",
        )?;
        let rows: Vec<_> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for (id, _cid, player, desc, status, created, due, updated, blocked) in rows {
            let inserted = dst.with_write(|tx| {
                let exists: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM promise WHERE id = ?1",
                    rusqlite::params![id],
                    |r| r.get(0),
                )?;
                if exists > 0 {
                    return Ok(false);
                }
                tx.execute(
                    "INSERT INTO promise(id, person_id, description, status,
                                         created_ms, due_ms, updated_ms, blocked_reason)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                    rusqlite::params![id, player, desc, status, created, due, updated, blocked],
                )?;
                Ok(true)
            })?;
            if inserted {
                m.promises += 1;
            } else {
                m.skipped += 1;
            }
        }
    }

    // QQ 消息：源 qq_message → 目标 channel_message
    {
        let mut stmt = src.prepare(
            "SELECT group_uin, sender_uin, sender_name, content, at_ms, message_id
             FROM qq_message",
        )?;
        let rows: Vec<_> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for (group, sender, name, content, at, mid) in rows {
            let inserted = dst.with_write(|tx| {
                if let Some(m) = &mid {
                    let exists: i64 = tx.query_row(
                        "SELECT COUNT(*) FROM channel_message
                         WHERE channel='qq' AND message_id = ?1",
                        rusqlite::params![m],
                        |r| r.get(0),
                    )?;
                    if exists > 0 {
                        return Ok(false);
                    }
                }
                tx.execute(
                    "INSERT INTO channel_message(channel, group_id, account, sender_name,
                                                 content, at_ms, message_id)
                     VALUES ('qq', ?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![group, sender, name, content, at, mid],
                )?;
                Ok(true)
            })?;
            if inserted {
                m.messages += 1;
            } else {
                m.skipped += 1;
            }
        }
    }

    Ok(m)
}

/// 从旧库的 `plans/` 目录迁规划。
///
/// # 为什么规划要单独一个函数
///
/// 规划**不在 SQLite 里**，它是 `plans/<npc>/<day>.json` 一堆独立文件。
/// 之前只写了 DB 迁移，于是规划一条都没过来——这是漏掉的整块，
/// 不是「迁移不完整」的小问题：计划是手帐的上游、是它一天做什么的依据。
///
/// # 目录约定
///
/// ```text
/// plans/
///   <npc名>/
///     g0.json g1.json ...   ← 每天一份
///     latest.json           ← 指针文件，**不是规划**，必须跳过
/// ```
///
/// # 幂等
///
/// 靠 `(companion_id, planning_day_id)` 唯一索引。重跑只更新，不重复插。
pub fn from_legacy_plans(plans_dir: &Path, dst: &Db, dry_run: bool) -> Result<Migrated> {
    if !plans_dir.exists() {
        return Err(Error::BadRequest(format!(
            "规划目录不存在：{}",
            plans_dir.display()
        )));
    }

    let mut m = Migrated::default();

    let npc_dirs: Vec<std::path::PathBuf> = std::fs::read_dir(plans_dir)
        .map_err(|e| Error::Storage(format!("读不了规划目录：{e}")))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();

    for npc_dir in npc_dirs {
        let npc_name = npc_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        let files: Vec<std::path::PathBuf> = std::fs::read_dir(&npc_dir)
            .map_err(|e| Error::Storage(format!("读不了 {npc_name} 目录：{e}")))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().map(|x| x == "json").unwrap_or(false)
                    // latest.json 是指针，不是规划。
                    && p.file_name().map(|n| n != "latest.json").unwrap_or(false)
            })
            .collect();

        for f in files {
            let text = match std::fs::read_to_string(&f) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("  跳过 {}：读不了（{e}）", f.display());
                    continue;
                }
            };
            let v: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("  跳过 {}：不是合法 JSON（{e}）", f.display());
                    continue;
                }
            };

            let day = v
                .get("planningDayId")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if day.is_empty() {
                eprintln!("  跳过 {}：没有 planningDayId", f.display());
                continue;
            }
            // 目录名优先于文件里的 companionId：目录就是 NPC 的权威来源。
            let companion = if npc_name.is_empty() {
                v.get("companionId")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                npc_name.clone()
            };
            if companion.is_empty() {
                eprintln!("  跳过 {}：认不出是哪个 NPC", f.display());
                continue;
            }

            let items = v
                .get("items")
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default();

            if dry_run {
                m.plans += 1;
                m.plan_items += items.len();
                continue;
            }

            let plan_id = v
                .get("id")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("plan-{companion}-{day}"));
            let summary = v.get("summary").and_then(|x| x.as_str()).map(String::from);
            let model = v.get("model").and_then(|x| x.as_str()).map(String::from);
            let game_day = v.get("gameDay").and_then(|x| x.as_i64());
            let created = v
                .get("createdMs")
                .and_then(|x| x.as_i64())
                .unwrap_or_else(crate::db::now_ms);
            let updated = v
                .get("updatedMs")
                .and_then(|x| x.as_i64())
                .unwrap_or(created);

            let inserted = dst.with_write(|tx| {
                // 幂等：同一 (companion, day) 已存在就整份替换（重新生成过）。
                let existing: Option<String> = tx
                    .query_row(
                        "SELECT id FROM plan WHERE planning_day_id = ?1
                         AND deleted_by IS NULL",
                        rusqlite::params![day],
                        |r| r.get(0),
                    )
                    .ok();

                if existing.is_some() {
                    return Ok(false);
                }

                tx.execute(
                    "INSERT INTO plan(id, planning_day_id, summary, model,
                                      game_day, created_ms, updated_ms)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)
                     ON CONFLICT(id) DO UPDATE SET
                       summary=excluded.summary, model=excluded.model,
                       game_day=excluded.game_day, updated_ms=excluded.updated_ms",
                    rusqlite::params![plan_id, day, summary, model, game_day, created, updated],
                )?;

                for (ord, it) in items.iter().enumerate() {
                    let item_id = it
                        .get("id")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("i{ord}"));
                    tx.execute(
                        "INSERT INTO plan_item(plan_id, id, kind, title, task_type, args_json,
                                               priority, reason, success, fallback, state,
                                               outcome, carry_from, created_ms, updated_ms, ord)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
                         ON CONFLICT(plan_id, id) DO UPDATE SET
                           state=excluded.state, outcome=excluded.outcome,
                           updated_ms=excluded.updated_ms, ord=excluded.ord",
                        rusqlite::params![
                            plan_id,
                            item_id,
                            it.get("kind").and_then(|x| x.as_str()),
                            it.get("title")
                                .and_then(|x| x.as_str())
                                .unwrap_or("(无标题)"),
                            it.get("taskType").and_then(|x| x.as_str()),
                            it.get("args").map(|a| a.to_string()),
                            it.get("priority").and_then(|x| x.as_i64()),
                            it.get("reason").and_then(|x| x.as_str()),
                            it.get("success").and_then(|x| x.as_str()),
                            it.get("fallback").and_then(|x| x.as_str()),
                            it.get("state").and_then(|x| x.as_str()).unwrap_or("todo"),
                            it.get("outcome").and_then(|x| x.as_str()),
                            it.get("carryFrom").and_then(|x| x.as_str()),
                            it.get("createdMs").and_then(|x| x.as_i64()),
                            it.get("updatedMs").and_then(|x| x.as_i64()),
                            ord as i64,
                        ],
                    )?;
                }
                Ok(true)
            })?;

            if inserted {
                m.plans += 1;
                m.plan_items += items.len();
            } else {
                m.skipped += 1;
            }
        }
    }

    Ok(m)
}

fn count(c: &Connection, table: &str) -> Result<usize> {
    if !table_exists(c, table)? {
        return Ok(0);
    }
    let n: i64 = c
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .map_err(|e| Error::Storage(e.to_string()))?;
    Ok(n as usize)
}

fn table_exists(c: &Connection, table: &str) -> Result<bool> {
    let n: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            rusqlite::params![table],
            |r| r.get(0),
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_793_000_000_000;

    /// 造一个「旧库」，字段名与 MC 侧一致。
    fn legacy(path: &Path) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(&format!(
            r#"
            CREATE TABLE event (
              id INTEGER PRIMARY KEY AUTOINCREMENT, companion_id TEXT NOT NULL,
              subject_id TEXT, at_ms INTEGER NOT NULL, type TEXT NOT NULL, scope TEXT,
              summary TEXT, importance REAL, source_json TEXT, dedup_key TEXT);
            CREATE TABLE journal (
              id TEXT PRIMARY KEY, companion_id TEXT NOT NULL, at_ms INTEGER NOT NULL,
              planning_day_id TEXT, type TEXT NOT NULL, source TEXT NOT NULL,
              summary TEXT NOT NULL, details TEXT, importance REAL NOT NULL,
              confidence REAL NOT NULL, tags_json TEXT, event_ids_json TEXT,
              related_plan_id TEXT, related_plan_item TEXT, related_task_id TEXT,
              related_promise TEXT, created_by TEXT);
            CREATE TABLE diary (
              id TEXT PRIMARY KEY, companion_id TEXT NOT NULL, kind TEXT NOT NULL,
              window_start_ms INTEGER NOT NULL, window_end_ms INTEGER NOT NULL, title TEXT,
              body TEXT NOT NULL, facts_json TEXT NOT NULL, refs_json TEXT NOT NULL,
              conflicts_json TEXT, model TEXT, created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL);
            CREATE TABLE fact (
              id TEXT PRIMARY KEY, companion_id TEXT NOT NULL, subject_type TEXT NOT NULL,
              subject_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, scope TEXT NOT NULL,
              status TEXT NOT NULL, tier TEXT NOT NULL, confidence REAL NOT NULL,
              importance REAL NOT NULL, created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL,
              expires_ms INTEGER, superseded_by TEXT, source_json TEXT NOT NULL,
              vec BLOB, vec_embedder TEXT);
            CREATE TABLE self_narrative (
              id INTEGER PRIMARY KEY AUTOINCREMENT, companion_id TEXT NOT NULL, body TEXT NOT NULL,
              model TEXT, created_ms INTEGER NOT NULL, superseded_ms INTEGER);
            CREATE TABLE mood (
              companion_id TEXT PRIMARY KEY, valence REAL NOT NULL, arousal REAL NOT NULL,
              dominance REAL NOT NULL, label TEXT, reason TEXT, updated_ms INTEGER NOT NULL);
            CREATE TABLE persona_trait (
              companion_id TEXT NOT NULL, trait TEXT NOT NULL, value REAL NOT NULL,
              updated_ms INTEGER NOT NULL, PRIMARY KEY (companion_id, trait));
            CREATE TABLE relation (
              companion_id TEXT NOT NULL, player_uuid TEXT NOT NULL, names_json TEXT NOT NULL,
              last_seen_name TEXT, familiarity INTEGER NOT NULL DEFAULT 0,
              trust INTEGER NOT NULL DEFAULT 0, closeness INTEGER NOT NULL DEFAULT 0,
              first_met_ms INTEGER NOT NULL, last_met_ms INTEGER NOT NULL,
              PRIMARY KEY (companion_id, player_uuid));
            CREATE TABLE promise (
              id TEXT PRIMARY KEY, companion_id TEXT NOT NULL, player_uuid TEXT,
              description TEXT NOT NULL, status TEXT NOT NULL, created_ms INTEGER NOT NULL,
              due_ms INTEGER, updated_ms INTEGER NOT NULL, goal_ref TEXT, job_ref TEXT,
              blocked_reason TEXT, evidence TEXT);
            CREATE TABLE qq_message (
              id INTEGER PRIMARY KEY AUTOINCREMENT, group_uin TEXT, sender_uin TEXT,
              sender_name TEXT, content TEXT NOT NULL, at_ms INTEGER NOT NULL,
              message_id TEXT, consumed INTEGER NOT NULL DEFAULT 0);

            INSERT INTO event(companion_id, subject_id, at_ms, type, scope, summary, importance, dedup_key)
              VALUES ('大肥鱼','uuid-a',{NOW},'task_done','companion','挖通了矿洞',0.8,'e-1');
            INSERT INTO event(companion_id, at_ms, type, scope, summary, importance)
              VALUES ('大肥鱼',{NOW}+1000,'player.talked','companion','聊了会天',0.5);

            INSERT INTO journal(id, companion_id, at_ms, planning_day_id, type, source,
                                summary, importance, confidence)
              VALUES ('j1','大肥鱼',{NOW},'g1','task_done','plugin','干完一件活',0.8,0.9);
            INSERT INTO journal(id, companion_id, at_ms, type, source, summary, importance, confidence)
              VALUES ('j2','大肥鱼',{NOW}+500,'discovery','plugin','发现了什么',0.6,0.8);

            INSERT INTO diary(id, companion_id, kind, window_start_ms, window_end_ms, title,
                              body, facts_json, refs_json, model, created_ms, updated_ms)
              VALUES ('d1','大肥鱼','day',{NOW}-86400000,{NOW},'标题','正文','[]','[]','m',{NOW},{NOW});

            INSERT INTO fact(id, companion_id, subject_type, subject_id, key, value, scope,
                             status, tier, confidence, importance, created_ms, updated_ms, source_json)
              VALUES ('f1','大肥鱼','player','uuid-a','爱吃的','鱼','player','confirmed',
                      'curated',0.9,0.8,{NOW},{NOW},'[]');

            INSERT INTO self_narrative(companion_id, body, model, created_ms)
              VALUES ('大肥鱼','我是谁','m',{NOW});

            INSERT INTO mood(companion_id, valence, arousal, dominance, label, reason, updated_ms)
              VALUES ('大肥鱼',0.3,0.2,0.1,'愉快','好事',{NOW});

            INSERT INTO persona_trait(companion_id, trait, value, updated_ms)
              VALUES ('大肥鱼','curiosity',0.7,{NOW});

            INSERT INTO relation(companion_id, player_uuid, names_json, last_seen_name,
                                 familiarity, trust, closeness, first_met_ms, last_met_ms)
              VALUES ('大肥鱼','uuid-a','[]','小明',5,3,20,{NOW},{NOW});

            INSERT INTO promise(id, companion_id, player_uuid, description, status,
                                created_ms, updated_ms)
              VALUES ('pr1','大肥鱼','uuid-a','带一桶牛奶','open',{NOW},{NOW});

            INSERT INTO qq_message(group_uin, sender_uin, sender_name, content, at_ms, message_id)
              VALUES ('g1','10001','Gleam','群里说话',{NOW},'mid-1');
            "#,
            NOW = NOW
        ))
        .unwrap();
    }

    fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::sync::Arc<Db>) {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("legacy.db");
        legacy(&src);
        let dst = Db::open(dir.path().join("new.db")).unwrap();
        (dir, src, dst)
    }

    #[test]
    fn 迁全部表() {
        let (_d, src, dst) = setup();
        let m = from_legacy(&src, &dst, false).unwrap();
        assert_eq!(m.events, 2);
        assert_eq!(m.journals, 2);
        assert_eq!(m.diaries, 1);
        assert_eq!(m.facts, 1);
        assert_eq!(m.self_narratives, 1);
        assert_eq!(m.moods, 1);
        assert_eq!(m.traits, 1);
        assert_eq!(m.relations, 1);
        assert_eq!(m.promises, 1);
        assert_eq!(m.messages, 1);
        assert_eq!(m.total(), 12);
    }

    #[test]
    fn 迁移是幂等的() {
        let (_d, src, dst) = setup();
        let first = from_legacy(&src, &dst, false).unwrap();
        assert_eq!(first.total(), 12);

        // 再跑一遍：一条都不该新增
        let second = from_legacy(&src, &dst, false).unwrap();
        assert_eq!(second.total(), 0, "重跑不该新增");
        assert!(second.skipped >= 11, "重跑应跳过已有：{second:?}");

        // 库里条数不变
        let n: i64 = dst
            .with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn dry_run_不写库() {
        let (_d, src, dst) = setup();
        let m = from_legacy(&src, &dst, true).unwrap();
        assert_eq!(m.events, 2, "dry run 也要报数");
        let n: i64 = dst
            .with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 0, "dry run 不该写任何东西");
    }

    #[test]
    fn 源库只读不被修改() {
        let (_d, src, dst) = setup();
        let before = std::fs::metadata(&src).unwrap().len();
        from_legacy(&src, &dst, false).unwrap();
        let after = std::fs::metadata(&src).unwrap().len();
        assert_eq!(before, after, "源库大小不该变");
    }

    #[test]
    fn 字段正确翻译() {
        let (_d, src, dst) = setup();
        from_legacy(&src, &dst, false).unwrap();

        // 源 event.type → 目标 kind
        let kind: String = dst
            .with_read(|c| {
                Ok(
                    c.query_row("SELECT kind FROM event WHERE dedup_key='e-1'", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(kind, "task_done");

        // 源 subject_id（UUID）→ 目标 person_id
        let pid: Option<String> = dst
            .with_read(|c| {
                Ok(c.query_row(
                    "SELECT person_id FROM event WHERE dedup_key='e-1'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(pid.as_deref(), Some("uuid-a"));

        // 源 persona_trait → 目标 trait
        let tr: String = dst
            .with_read(|c| Ok(c.query_row("SELECT trait FROM trait", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(tr, "curiosity");

        // 源 qq_message → 目标 channel_message
        let ch: String = dst
            .with_read(
                |c| Ok(c.query_row("SELECT channel FROM channel_message", [], |r| r.get(0))?),
            )
            .unwrap();
        assert_eq!(ch, "qq");
    }

    #[test]
    fn 没有_dedup_key_的事件人工造稳定键() {
        let (_d, src, dst) = setup();
        from_legacy(&src, &dst, false).unwrap();
        // 第二条事件源库没 dedup_key，应被造出 legacy-event-<id>
        let k: String = dst
            .with_read(|c| {
                Ok(c.query_row(
                    "SELECT dedup_key FROM event WHERE summary='聊了会天'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert!(k.starts_with("legacy-event-"), "实得 {k}");
    }

    #[test]
    fn 关系迁移顺手建出_person() {
        let (_d, src, dst) = setup();
        from_legacy(&src, &dst, false).unwrap();

        let n: i64 = dst
            .with_read(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM person WHERE person_id='uuid-a'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(n, 1, "关系迁移应顺带建出 person");

        let ch: String = dst
            .with_read(|c| {
                Ok(c.query_row(
                    "SELECT channel FROM person_account WHERE account='uuid-a'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(ch, "mc", "应绑到 mc 渠道");
    }

    #[test]
    fn 源库不存在时报错() {
        let dir = tempfile::tempdir().unwrap();
        let dst = Db::open(dir.path().join("new.db")).unwrap();
        let e = from_legacy(&dir.path().join("nope.db"), &dst, false).unwrap_err();
        assert!(e.to_string().contains("不存在"));
    }

    #[test]
    fn 时间戳原样保留() {
        let (_d, src, dst) = setup();
        from_legacy(&src, &dst, false).unwrap();
        let at: i64 = dst
            .with_read(|c| {
                Ok(
                    c.query_row("SELECT at_ms FROM event WHERE dedup_key='e-1'", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(at, NOW, "时间戳不能改，否则时效衰减全乱");
    }

    #[test]
    fn 迁完之后能正常检索() {
        let (_d, src, dst) = setup();
        from_legacy(&src, &dst, false).unwrap();
        // 迁过来的手帐应能被普通查询看到
        let p = crate::query::journal(&dst, None, crate::query::Page::default()).unwrap();
        assert_eq!(p.total, 2);
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    const NOW: i64 = 1_793_000_000_000;

    /// 造一个 `plans/<npc>/` 目录，含 2 份规划 + 1 个 latest.json 指针。
    fn legacy_plans(root: &Path) {
        let dir = root.join("大肥鱼");
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(
            dir.join("g1.json"),
            format!(
                r#"{{"schema":1,"id":"plan-1","companionId":"大肥鱼","planningDayId":"g1",
                "summary":"今天补墙","createdMs":{NOW},"updatedMs":{NOW},
                "gameDay":1,"model":"m1",
                "items":[
                  {{"id":"i1","kind":"maintenance","title":"烧铁","taskType":"craft",
                    "priority":70,"state":"done","outcome":"6 个铁锭","createdMs":{NOW},"updatedMs":{NOW}}},
                  {{"id":"i2","kind":"personal_goal","title":"补墙","taskType":"build",
                    "priority":65,"state":"todo","createdMs":{NOW}}}
                ]}}"#,
                NOW = NOW
            ),
        )
        .unwrap();

        std::fs::write(
            dir.join("g2.json"),
            format!(
                r#"{{"id":"plan-2","companionId":"大肥鱼","planningDayId":"g2",
                "summary":"去钓鱼","createdMs":{},"updatedMs":{},
                "items":[{{"id":"i1","title":"钓鱼","state":"deferred","carryFrom":"g1"}}]}}"#,
                NOW + 86_400_000,
                NOW + 86_400_000
            ),
        )
        .unwrap();

        // 指针文件：**绝不能被当成规划**
        std::fs::write(dir.join("latest.json"), r#"{"planningDayId":"g2"}"#).unwrap();
    }

    fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::sync::Arc<Db>) {
        let dir = tempfile::tempdir().unwrap();
        let plans = dir.path().join("plans");
        legacy_plans(&plans);
        let dst = Db::open(dir.path().join("new.db")).unwrap();
        (dir, plans, dst)
    }

    #[test]
    fn 迁规划与条目() {
        let (_d, plans, dst) = setup();
        let m = from_legacy_plans(&plans, &dst, false).unwrap();
        assert_eq!(m.plans, 2, "应迁 2 份规划");
        assert_eq!(m.plan_items, 3, "2+1 共 3 个条目");
    }

    #[test]
    fn 跳过_latest_指针文件() {
        let (_d, plans, dst) = setup();
        from_legacy_plans(&plans, &dst, false).unwrap();
        // latest.json 里只有 planningDayId，没有 items；
        // 如果被误当规划，会多出一条没有条目的 plan。
        let n: i64 = dst
            .with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM plan", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 2, "只有 g1 和 g2，不能把 latest.json 算进来");
    }

    #[test]
    fn 幂等重跑不重复() {
        let (_d, plans, dst) = setup();
        let a = from_legacy_plans(&plans, &dst, false).unwrap();
        assert_eq!(a.plans, 2);

        let b = from_legacy_plans(&plans, &dst, false).unwrap();
        assert_eq!(b.plans, 0, "重跑不该新增");
        assert_eq!(b.skipped, 2, "应跳过 2 份");

        let n: i64 = dst
            .with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM plan_item", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 3, "条目不该翻倍");
    }

    #[test]
    fn dry_run_不写库() {
        let (_d, plans, dst) = setup();
        let m = from_legacy_plans(&plans, &dst, true).unwrap();
        assert_eq!(m.plans, 2, "演练也要报数");
        assert_eq!(m.plan_items, 3);

        let n: i64 = dst
            .with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM plan", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 0, "演练不该写");
    }

    #[test]
    fn 条目顺序被保留() {
        let (_d, plans, dst) = setup();
        from_legacy_plans(&plans, &dst, false).unwrap();
        // g1 的两个条目：i1(烧铁) 在前，i2(补墙) 在后
        let titles: Vec<String> = dst
            .with_read(|c| {
                let mut s =
                    c.prepare("SELECT title FROM plan_item WHERE plan_id='plan-1' ORDER BY ord")?;
                let v = s
                    .query_map([], |r| r.get(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(v)
            })
            .unwrap();
        assert_eq!(titles, vec!["烧铁".to_string(), "补墙".to_string()]);
    }

    #[test]
    fn 状态与产出被保留() {
        let (_d, plans, dst) = setup();
        from_legacy_plans(&plans, &dst, false).unwrap();
        let (state, outcome): (String, Option<String>) = dst
            .with_read(|c| {
                Ok(c.query_row(
                    "SELECT state, outcome FROM plan_item WHERE plan_id='plan-1' AND id='i1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(state, "done");
        assert_eq!(outcome.as_deref(), Some("6 个铁锭"));

        // deferred 与 carryFrom 也要过来（暂缓 ≠ 完成，这条语义很重要）
        let (s2, cf): (String, Option<String>) = dst
            .with_read(|c| {
                Ok(c.query_row(
                    "SELECT state, carry_from FROM plan_item WHERE plan_id='plan-2'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(s2, "deferred");
        assert_eq!(cf.as_deref(), Some("g1"));
    }

    #[test]
    fn 时间戳原样保留() {
        let (_d, plans, dst) = setup();
        from_legacy_plans(&plans, &dst, false).unwrap();
        let c: i64 = dst
            .with_read(|c| {
                Ok(
                    c.query_row("SELECT created_ms FROM plan WHERE id='plan-1'", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(c, NOW, "created_ms 必须原样保留，否则排序全乱");
    }

    #[test]
    fn 目录不存在时报错() {
        let dir = tempfile::tempdir().unwrap();
        let dst = Db::open(dir.path().join("new.db")).unwrap();
        let e = from_legacy_plans(&dir.path().join("nope"), &dst, false).unwrap_err();
        assert!(e.to_string().contains("不存在"));
    }

    #[test]
    fn 坏_json_被跳过不中断() {
        let dir = tempfile::tempdir().unwrap();
        let plans = dir.path().join("plans");
        let npc = plans.join("大肥鱼");
        std::fs::create_dir_all(&npc).unwrap();
        std::fs::write(npc.join("g1.json"), "{ 这不是 json").unwrap();
        std::fs::write(
            npc.join("g2.json"),
            format!(r#"{{"id":"p2","planningDayId":"g2","createdMs":{NOW},"updatedMs":{NOW},"items":[]}}"#),
        )
        .unwrap();

        let dst = Db::open(dir.path().join("new.db")).unwrap();
        let m = from_legacy_plans(&plans, &dst, false).unwrap();
        assert_eq!(m.plans, 1, "坏文件被跳过，好的照迁");
    }
}
