//! 供 MC 插件（及任何既有客户端）对接的完整 CRUD 接口。
//!
//! # 为什么和 `/v1` 那批分开
//!
//! `/v1` 是「新写的东西该用的形状」：事件、事实、检索、身份。
//! 这里这批是**迁移对接面**：MC 插件的 `MemoryRepository` 有 46 个方法，
//! 要让它整块切过来，就得把它读写的每一类实体都开到 HTTP 上。
//!
//! 分成两个文件而不是混在一起，是因为它们的**演进节奏不同**：
//! `/v1` 可以随 Hub 自己的设计改；这里必须与 Java 侧接口保持一一对应，
//! 改了就要同步改客户端。
//!
//! # 一致性约定
//!
//! - 所有写接口幂等（靠主键或唯一键）
//! - 所有读接口默认过滤 `deleted_by IS NULL`
//! - 时间一律 UTC 毫秒

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::db::now_ms;
use crate::error::{Error, Result};
use crate::http::AppState;

// ================================================================== 类型

#[derive(Debug, Serialize, Deserialize)]
pub struct RelationDto {
    /// Java 侧叫 `player_uuid`，Hub 侧统一叫 `person_id`。
    pub person_id: String,
    pub familiarity: i64,
    pub trust: i64,
    pub closeness: i64,
    pub first_met_ms: i64,
    pub last_met_ms: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PromiseDto {
    pub id: String,
    pub person_id: Option<String>,
    pub description: String,
    pub status: String,
    pub created_ms: i64,
    pub due_ms: Option<i64>,
    pub updated_ms: i64,
    pub blocked_reason: Option<String>,
    pub goal_ref: Option<String>,
    pub job_ref: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TraitDto {
    pub trait_: String,
    pub value: f64,
    pub updated_ms: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AffectLogDto {
    pub person_id: Option<String>,
    pub at_ms: i64,
    pub kind: String,
    pub field: String,
    pub delta: Option<f64>,
    pub reason: Option<String>,
    pub event_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MessageDto {
    pub channel: String,
    pub group_id: Option<String>,
    pub account: Option<String>,
    pub sender_name: Option<String>,
    pub content: String,
    pub at_ms: i64,
    pub message_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SelfNarrativeDto {
    pub body: String,
    pub model: Option<String>,
    pub created_ms: i64,
    pub superseded_ms: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FactFullDto {
    pub id: String,
    pub person_id: Option<String>,
    pub key: String,
    pub value: String,
    pub scope: String,
    pub status: String,
    pub tier: String,
    pub confidence: f64,
    pub importance: f64,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub expires_ms: Option<i64>,
    pub superseded_by: Option<String>,
    pub recall_count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DiaryFullDto {
    pub id: String,
    pub kind: String,
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    pub title: Option<String>,
    pub body: String,
    pub model: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Default, Serialize)]
pub struct PruneResultDto {
    pub events: i64,
    pub qq_messages: i64,
    pub affect_log: i64,
    pub facts: i64,
    pub journal: i64,
}

type A<T> = std::result::Result<Json<T>, Error>;

// ================================================================== 路由

pub fn router(state: AppState) -> Router {
    Router::new()
        // 关系
        .route("/v1/relation", post(put_relation))
        .route("/v1/relation/list", get(list_relations))
        // 承诺
        .route("/v1/promise", post(post_promise))
        .route(
            "/v1/promise/{id}",
            patch(patch_promise).delete(delete_promise),
        )
        .route("/v1/promise/list", get(list_promises))
        // 特质
        .route("/v1/trait", post(put_trait))
        .route("/v1/trait/list", get(list_traits))
        // 情绪流水
        .route("/v1/affect", post(post_affect))
        .route("/v1/affect/list", get(list_affect))
        // 渠道消息
        .route("/v1/message", post(post_message))
        .route("/v1/message/list", get(list_messages))
        .route("/v1/message/consumed", post(mark_consumed))
        // 自我认知
        .route("/v1/self", post(put_self))
        .route("/v1/self/get", get(get_self))
        // 事实（完整字段 + 向量）
        .route("/v1/fact/list", get(list_facts_full))
        .route("/v1/fact/{id}/vector", post(put_fact_vector))
        .route("/v1/fact/vectors", get(get_fact_vectors))
        // 手帐
        .route("/v1/journal/list", get(list_journal_full))
        // 日记
        .route("/v1/diary/list", get(list_diaries_full))
        // 注入回放（由插件上报：决策是插件做的，Hub 只是存）
        .route("/v1/injection-replay", post(post_injection_replay))
        // 维护
        .route("/v1/prune", post(prune))
        .with_state(state)
}

// ================================================================== 关系

async fn put_relation(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(r): Json<RelationDto>,
) -> A<serde_json::Value> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO relation(person_id, familiarity, trust, closeness,
                                      first_met_ms, last_met_ms)
                 VALUES (?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(person_id) DO UPDATE SET
                   familiarity=excluded.familiarity, trust=excluded.trust,
                   closeness=excluded.closeness, last_met_ms=excluded.last_met_ms",
                rusqlite::params![
                    r.person_id,
                    r.familiarity,
                    r.trust,
                    r.closeness,
                    r.first_met_ms,
                    r.last_met_ms
                ],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct CompanionQuery {}

async fn list_relations(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(_q): Query<CompanionQuery>,
) -> A<Vec<RelationDto>> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    let items = tokio::task::spawn_blocking(move || -> Result<Vec<RelationDto>> {
        db.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT person_id, familiarity, trust, closeness,
                        first_met_ms, last_met_ms FROM relation ORDER BY closeness DESC",
            )?;
            let v = stmt
                .query_map([], |r| {
                    Ok(RelationDto {
                        person_id: r.get(0)?,
                        familiarity: r.get(1)?,
                        trust: r.get(2)?,
                        closeness: r.get(3)?,
                        first_met_ms: r.get(4)?,
                        last_met_ms: r.get(5)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(v)
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(items))
}

// ================================================================== 承诺

async fn post_promise(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(p): Json<PromiseDto>,
) -> A<serde_json::Value> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            // 8 列 = 8 个占位符（多写一个 `?9` 会 503「9 values for 8 columns」）。
            tx.execute(
                "INSERT INTO promise(id, person_id, description, status,
                                     created_ms, due_ms, updated_ms, blocked_reason)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(id) DO UPDATE SET
                   description=excluded.description, status=excluded.status,
                   due_ms=excluded.due_ms, updated_ms=excluded.updated_ms,
                   blocked_reason=excluded.blocked_reason",
                rusqlite::params![
                    p.id,
                    p.person_id,
                    p.description,
                    p.status,
                    p.created_ms,
                    p.due_ms,
                    p.updated_ms,
                    p.blocked_reason
                ],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 删一条承诺（逻辑删除，可回滚）。
///
/// 之前只有 `POST` 和 `PATCH` —— 于是「记错了一条承诺」只能留着，
/// 面板上也没法清理。误记的承诺比没记更糟：它会一直挂在上下文里，
/// 变成一个根本没答应过的义务。
async fn delete_promise(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: Option<Json<DeleteNote>>,
) -> A<serde_json::Value> {
    crate::http::check_token(&st, &headers)?;
    let note = body.and_then(|b| b.note.clone());
    let db = st.db.clone();
    let id2 = id.clone();
    let n = tokio::task::spawn_blocking(move || -> Result<usize> {
        db.with_write(|tx| {
            let now = now_ms();
            let log = tx.execute(
                "INSERT INTO rollback_log(at_ms, target_ms, mode, affected_json, note)
                 VALUES (?1, ?1, 'delete', ?2, ?3)",
                rusqlite::params![
                    now,
                    format!("[\"{id2}\"]"),
                    note.unwrap_or_else(|| "删除单条承诺".into())
                ],
            )?;
            let log_id = tx.last_insert_rowid();
            let _ = log;
            let n = tx.execute(
                "UPDATE promise SET deleted_by = ?1, updated_ms = ?2
                 WHERE id = ?3 AND deleted_by IS NULL",
                rusqlite::params![log_id, now, id2],
            )?;
            Ok(n)
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    if n == 0 {
        return Err(Error::NotFound(format!("承诺 {id}")));
    }
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

#[derive(Deserialize)]
struct DeleteNote {
    note: Option<String>,
}

#[derive(Deserialize)]
struct PromisePatch {
    status: Option<String>,
    blocked_reason: Option<String>,
    due_ms: Option<i64>,
}

async fn patch_promise(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(p): Json<PromisePatch>,
) -> A<serde_json::Value> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    let now = now_ms();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            let n: i64 = tx.query_row(
                "SELECT COUNT(*) FROM promise WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("承诺 {id}")));
            }
            tx.execute(
                "UPDATE promise SET
                   status=COALESCE(?1,status), blocked_reason=COALESCE(?2,blocked_reason),
                   due_ms=COALESCE(?3,due_ms), updated_ms=?4
                 WHERE id=?5",
                rusqlite::params![p.status, p.blocked_reason, p.due_ms, now, id],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct PromiseQuery {
    status: Option<String>,
}

async fn list_promises(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PromiseQuery>,
) -> A<Vec<PromiseDto>> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    let items = tokio::task::spawn_blocking(move || -> Result<Vec<PromiseDto>> {
        db.with_read(|c| {
            let mut where_sql = String::from("WHERE deleted_by IS NULL");
            let mut args: Vec<String> = Vec::new();
            if let Some(s) = q.status.clone().filter(|s| !s.is_empty()) {
                where_sql.push_str(" AND status = ?");
                args.push(s);
            }
            let mut stmt = c.prepare(&format!(
                "SELECT id, person_id, description, status, created_ms,
                        due_ms, updated_ms, blocked_reason, goal_ref, job_ref
                 FROM promise {where_sql} ORDER BY created_ms DESC LIMIT 500"
            ))?;
            let v = stmt
                .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                    // 索引必须与 SELECT 的 10 列一一对应（从 0 起）。
                    // 这里曾经整体偏 1 且多了第 10 个 get：读出来的每个字段
                    // 都是**后一列**的值 —— status 拿到 updated_ms、description
                    // 拿到 status，而且最后一列会越界。线上表现为「列表里
                    // 承诺内容是数字、状态永远不对」。
                    Ok(PromiseDto {
                        id: r.get(0)?,
                        person_id: r.get(1)?,
                        description: r.get(2)?,
                        status: r.get(3)?,
                        created_ms: r.get(4)?,
                        due_ms: r.get(5)?,
                        updated_ms: r.get(6)?,
                        blocked_reason: r.get(7)?,
                        goal_ref: r.get(8)?,
                        job_ref: r.get(9)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(v)
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(items))
}

// ================================================================== 特质

async fn put_trait(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(t): Json<TraitDto>,
) -> A<serde_json::Value> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO trait(trait, value, updated_ms) VALUES (?1,?2,?3)
                 ON CONFLICT(trait) DO UPDATE SET
                   value=excluded.value, updated_ms=excluded.updated_ms",
                rusqlite::params![t.trait_, t.value, t.updated_ms],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn list_traits(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(_q): Query<CompanionQuery>,
) -> A<Vec<TraitDto>> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    let items = tokio::task::spawn_blocking(move || -> Result<Vec<TraitDto>> {
        db.with_read(|c| {
            let mut stmt =
                c.prepare("SELECT trait, value, updated_ms FROM trait ORDER BY value DESC")?;
            let v = stmt
                .query_map([], |r| {
                    Ok(TraitDto {
                        trait_: r.get(0)?,
                        value: r.get(1)?,
                        updated_ms: r.get(2)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(v)
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(items))
}

// ================================================================== 情绪流水

async fn post_affect(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(a): Json<AffectLogDto>,
) -> A<serde_json::Value> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO affect_log(person_id, at_ms, kind, field, delta,
                                        reason, event_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![
                    a.person_id,
                    a.at_ms,
                    a.kind,
                    a.field,
                    a.delta,
                    a.reason,
                    a.event_id
                ],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct AffectQuery {
    person_id: Option<String>,
    limit: Option<u32>,
}

async fn list_affect(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AffectQuery>,
) -> A<Vec<AffectLogDto>> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    let limit = q.limit.unwrap_or(50).clamp(1, 1000) as i64;
    let items = tokio::task::spawn_blocking(move || -> Result<Vec<AffectLogDto>> {
        db.with_read(|c| {
            let mut where_sql = String::from("WHERE 1=1");
            let mut args: Vec<String> = Vec::new();
            if let Some(p) = q.person_id.clone().filter(|s| !s.is_empty()) {
                where_sql.push_str(" AND person_id = ?");
                args.push(p);
            }
            args.push(limit.to_string());
            let mut stmt = c.prepare(&format!(
                "SELECT person_id, at_ms, kind, field, delta, reason, event_id
                 FROM affect_log {where_sql} ORDER BY at_ms DESC LIMIT ?"
            ))?;
            let v = stmt
                .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                    // 索引从 0 起，与 SELECT 的列顺序一一对应。
                    // 这里曾经整体偏 1：把 at_ms 读成 person_id，于是 `at_ms` 那一列
                    // 拿到的是 INTEGER 型的 at_ms 却按 TEXT 解析 → 503
                    // 「Invalid column type Integer at index: 1」。
                    // 线上实测：/v1/affect/list 一直 503，调用方只能看到「存储不可用」。
                    Ok(AffectLogDto {
                        person_id: r.get(0)?,
                        at_ms: r.get(1)?,
                        kind: r.get(2)?,
                        field: r.get(3)?,
                        delta: r.get(4)?,
                        reason: r.get(5)?,
                        event_id: r.get(6)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(v)
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(items))
}

// ================================================================== 渠道消息

async fn post_message(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(m): Json<MessageDto>,
) -> A<serde_json::Value> {
    crate::http::check_token(&st, &headers)?;
    // 被忽视的人说的话：不入库。
    // 判断按「渠道 + 账号」查 —— 对方手里只有 QQ 号，没有 person_id。
    if let Some(acc) = m.account.as_deref().filter(|s| !s.trim().is_empty()) {
        if crate::query::is_ignored(&st.db, &m.channel, acc)? {
            tracing::info!("忽略：{}/{} 被设为忽视，消息未入库", m.channel, acc);
            return Ok(Json(
                serde_json::json!({"ok": true, "inserted": false, "ignored": true}),
            ));
        }
    }
    let db = st.db.clone();
    // 空串 message_id 必须归一成 NULL：`channel_message` 上有
    // `UNIQUE(channel, message_id) WHERE message_id IS NOT NULL`，
    // 而 SQLite 里 `""` 不是 NULL —— 第一条第 `message_id:""` 的消息写进去之后，
    // 之后所有同渠道的 `message_id:""` 都会被当成重复**静默丢弃**。
    // 与 `/v1/event` 的 `dedup_key` 是同一类坑，修法也同一处。
    let message_id = crate::http::normalize_key(m.message_id.clone());
    let inserted = tokio::task::spawn_blocking(move || -> Result<bool> {
        db.with_write(|tx| {
            if let Some(mid) = &message_id {
                let n: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM channel_message WHERE channel=?1 AND message_id=?2",
                    rusqlite::params![m.channel, mid],
                    |r| r.get(0),
                )?;
                if n > 0 {
                    return Ok(false);
                }
            }
            tx.execute(
                "INSERT INTO channel_message(channel, group_id, account, sender_name,
                                             content, at_ms, message_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![
                    m.channel,
                    m.group_id,
                    m.account,
                    m.sender_name,
                    m.content,
                    m.at_ms,
                    message_id
                ],
            )?;
            Ok(true)
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(
        serde_json::json!({ "ok": true, "inserted": inserted }),
    ))
}

#[derive(Deserialize)]
struct MessageQuery {
    channel: Option<String>,
    group_id: Option<String>,
    limit: Option<u32>,
}

async fn list_messages(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MessageQuery>,
) -> A<Vec<serde_json::Value>> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    let limit = q.limit.unwrap_or(10).clamp(1, 500) as i64;
    let items = tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>> {
        db.with_read(|c| {
            let mut where_sql = String::from("WHERE deleted_by IS NULL");
            let mut args: Vec<String> = Vec::new();
            if let Some(ch) = q.channel.clone().filter(|s| !s.is_empty()) {
                where_sql.push_str(" AND channel = ?");
                args.push(ch);
            }
            if let Some(g) = q.group_id.clone().filter(|s| !s.is_empty()) {
                where_sql.push_str(" AND group_id = ?");
                args.push(g);
            }
            args.push(limit.to_string());
            let mut stmt = c.prepare(&format!(
                "SELECT id, channel, group_id, account, sender_name, content, at_ms,
                        message_id, consumed
                 FROM channel_message {where_sql} ORDER BY at_ms DESC LIMIT ?"
            ))?;
            let v = stmt
                .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                    Ok(serde_json::json!({
                        "id": r.get::<_, i64>(0)?,
                        "channel": r.get::<_, String>(1)?,
                        "group_id": r.get::<_, Option<String>>(2)?,
                        "account": r.get::<_, Option<String>>(3)?,
                        "sender_name": r.get::<_, Option<String>>(4)?,
                        "content": r.get::<_, String>(5)?,
                        "at_ms": r.get::<_, i64>(6)?,
                        "message_id": r.get::<_, Option<String>>(7)?,
                        "consumed": r.get::<_, i64>(8)?,
                    }))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(v)
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(items))
}

#[derive(Deserialize)]
struct ConsumedBody {
    ids: Vec<i64>,
}

async fn mark_consumed(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<ConsumedBody>,
) -> A<serde_json::Value> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    let n = tokio::task::spawn_blocking(move || -> Result<usize> {
        db.with_write(|tx| {
            let mut n = 0;
            for id in &b.ids {
                n += tx.execute(
                    "UPDATE channel_message SET consumed = 1 WHERE id = ?1",
                    rusqlite::params![id],
                )?;
            }
            Ok(n)
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!({ "ok": true, "updated": n })))
}

// ================================================================== 自我认知

async fn put_self(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(s): Json<SelfNarrativeDto>,
) -> A<serde_json::Value> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            // 旧版本标 superseded，新版本单独一行 —— 与 Java 侧的语义一致。
            tx.execute(
                "UPDATE self_narrative SET superseded_ms = ?1
                 WHERE superseded_ms IS NULL AND deleted_by IS NULL",
                rusqlite::params![s.created_ms],
            )?;
            // 4 列 = 4 个占位符。这里曾经写成 5 个 `?` 只给 4 个值，
            // 于是每次写自我认知都 503「5 values for 4 columns」——
            // 线上实测：/v1/self 从上线起就没成功过。
            tx.execute(
                "INSERT INTO self_narrative(body, model, created_ms, superseded_ms)
                 VALUES (?1,?2,?3,?4)",
                rusqlite::params![s.body, s.model, s.created_ms, s.superseded_ms],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn get_self(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(_q): Query<CompanionQuery>,
) -> A<Option<SelfNarrativeDto>> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    let item = tokio::task::spawn_blocking(move || -> Result<Option<SelfNarrativeDto>> {
        db.with_read(|c| {
            Ok(c.query_row(
                "SELECT body, model, created_ms, superseded_ms
                 FROM self_narrative
                 WHERE superseded_ms IS NULL AND deleted_by IS NULL
                 ORDER BY created_ms DESC LIMIT 1",
                [],
                |r| {
                    Ok(SelfNarrativeDto {
                        body: r.get(0)?,
                        model: r.get(1)?,
                        created_ms: r.get(2)?,
                        superseded_ms: r.get(3)?,
                    })
                },
            )
            .ok())
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(item))
}

// ================================================================== 事实（完整）

#[derive(Deserialize)]
struct FactQuery {
    person_id: Option<String>,
    status: Option<String>,
    limit: Option<u32>,
}

async fn list_facts_full(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<FactQuery>,
) -> A<Vec<FactFullDto>> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    let limit = q.limit.unwrap_or(2000).clamp(1, 10000) as i64;
    let items = tokio::task::spawn_blocking(move || -> Result<Vec<FactFullDto>> {
        db.with_read(|c| {
            let mut where_sql = String::from("WHERE deleted_by IS NULL");
            let mut args: Vec<String> = Vec::new();
            if let Some(pid) = q.person_id.clone().filter(|s| !s.is_empty()) {
                where_sql.push_str(" AND person_id = ?");
                args.push(pid);
            }
            if let Some(s) = q.status.clone().filter(|s| !s.is_empty()) {
                where_sql.push_str(" AND status = ?");
                args.push(s);
            }
            args.push(limit.to_string());
            let mut stmt = c.prepare(&format!(
                "SELECT id, person_id, key, value, scope, status, tier,
                        confidence, importance, created_ms, updated_ms, expires_ms,
                        superseded_by, recall_count
                 FROM fact {where_sql} ORDER BY updated_ms DESC LIMIT ?"
            ))?;
            let v = stmt
                .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                    Ok(FactFullDto {
                        id: r.get(0)?,
                        person_id: r.get(1)?,
                        key: r.get(2)?,
                        value: r.get(3)?,
                        scope: r.get(4)?,
                        status: r.get(5)?,
                        tier: r.get(6)?,
                        confidence: r.get(7)?,
                        importance: r.get(8)?,
                        created_ms: r.get(9)?,
                        updated_ms: r.get(10)?,
                        expires_ms: r.get(11)?,
                        superseded_by: r.get(12)?,
                        recall_count: r.get(13)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(v)
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(items))
}

#[derive(Deserialize)]
struct VectorBody {
    /// base64 编码的 f32 小端字节。用 base64 而不是 JSON 数组：
    /// 768 维 float 用 JSON 数组是 8KB 文本，base64 只有 4KB。
    vector_b64: String,
    embedder: String,
}

async fn put_fact_vector(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(b): Json<VectorBody>,
) -> A<serde_json::Value> {
    crate::http::check_token(&st, &headers)?;
    let bytes = b64_decode(&b.vector_b64)
        .ok_or_else(|| Error::BadRequest("vector_b64 不是合法 base64".into()))?;
    let db = st.db.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            let n = tx.execute(
                "UPDATE fact SET vec = ?1, vec_embedder = ?2 WHERE id = ?3",
                rusqlite::params![bytes, b.embedder, id],
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("事实 {id}")));
            }
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn get_fact_vectors(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(_q): Query<CompanionQuery>,
) -> A<serde_json::Value> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>> {
        db.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, vec, vec_embedder FROM fact
                 WHERE vec IS NOT NULL AND deleted_by IS NULL",
            )?;
            let v = stmt
                .query_map([], |r| {
                    let id: String = r.get(0)?;
                    let blob: Vec<u8> = r.get(1)?;
                    let emb: Option<String> = r.get(2)?;
                    Ok(serde_json::json!({
                        "id": id,
                        "vector_b64": b64_encode(&blob),
                        "embedder": emb,
                    }))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(v)
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!({ "vectors": out })))
}

// ================================================================== 手帐

#[derive(Debug, Serialize)]
pub struct JournalFullDto {
    pub id: String,
    pub at_ms: i64,
    pub planning_day_id: Option<String>,
    pub type_: String,
    pub source: String,
    pub summary: String,
    pub details: Option<String>,
    pub importance: f64,
    pub confidence: f64,
    pub channel: String,
}

#[derive(Deserialize)]
struct JournalQuery {
    planning_day_id: Option<String>,
    type_: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn list_journal_full(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<JournalQuery>,
) -> A<serde_json::Value> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    let limit = q.limit.unwrap_or(200).clamp(1, 2000) as i64;
    let offset = q.offset.unwrap_or(0) as i64;

    let out = tokio::task::spawn_blocking(move || -> Result<(Vec<JournalFullDto>, i64)> {
        db.with_read(|c| {
            let mut where_sql = String::from("WHERE deleted_by IS NULL");
            let mut args: Vec<String> = Vec::new();
            if let Some(d) = q.planning_day_id.clone().filter(|s| !s.is_empty()) {
                where_sql.push_str(" AND planning_day_id = ?");
                args.push(d);
            }
            if let Some(t) = q.type_.clone().filter(|s| !s.is_empty()) {
                where_sql.push_str(" AND type = ?");
                args.push(t);
            }

            let total: i64 = c.query_row(
                &format!("SELECT COUNT(*) FROM journal {where_sql}"),
                rusqlite::params_from_iter(args.iter()),
                |r| r.get(0),
            )?;

            let mut all: Vec<Box<dyn rusqlite::ToSql>> = args
                .into_iter()
                .map(|s| Box::new(s) as Box<dyn rusqlite::ToSql>)
                .collect();
            all.push(Box::new(limit));
            all.push(Box::new(offset));

            let mut stmt = c.prepare(&format!(
                "SELECT id, at_ms, planning_day_id, type, source, summary,
                        details, importance, confidence, channel
                 FROM journal {where_sql} ORDER BY at_ms DESC LIMIT ? OFFSET ?"
            ))?;
            let items = stmt
                .query_map(
                    rusqlite::params_from_iter(all.iter().map(|b| b.as_ref())),
                    |r| {
                        Ok(JournalFullDto {
                            id: r.get(0)?,
                            at_ms: r.get(1)?,
                            planning_day_id: r.get(2)?,
                            type_: r.get(3)?,
                            source: r.get(4)?,
                            summary: r.get(5)?,
                            details: r.get(6)?,
                            importance: r.get(7)?,
                            confidence: r.get(8)?,
                            channel: r.get(9)?,
                        })
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok((items, total))
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;

    Ok(Json(serde_json::json!({
        "items": out.0,
        "total": out.1,
    })))
}

// ================================================================== 日记

#[derive(Deserialize)]
struct DiaryQuery {
    kind: Option<String>,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    limit: Option<u32>,
}

async fn list_diaries_full(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DiaryQuery>,
) -> A<Vec<DiaryFullDto>> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    let limit = q.limit.unwrap_or(200).clamp(1, 1000) as i64;
    let items = tokio::task::spawn_blocking(move || -> Result<Vec<DiaryFullDto>> {
        db.with_read(|c| {
            let mut where_sql = String::from("WHERE deleted_by IS NULL");
            let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if let Some(k) = q.kind.clone().filter(|s| !s.is_empty()) {
                where_sql.push_str(" AND kind = ?");
                args.push(Box::new(k));
            }
            if let Some(f) = q.from_ms {
                where_sql.push_str(" AND window_start_ms >= ?");
                args.push(Box::new(f));
            }
            if let Some(t) = q.to_ms {
                where_sql.push_str(" AND window_end_ms <= ?");
                args.push(Box::new(t));
            }
            args.push(Box::new(limit));
            let mut stmt = c.prepare(&format!(
                "SELECT id, kind, window_start_ms, window_end_ms, title, body,
                        model, created_ms, updated_ms
                 FROM diary {where_sql} ORDER BY window_start_ms DESC LIMIT ?"
            ))?;
            let v = stmt
                .query_map(
                    rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
                    |r| {
                        Ok(DiaryFullDto {
                            id: r.get(0)?,
                            kind: r.get(1)?,
                            window_start_ms: r.get(2)?,
                            window_end_ms: r.get(3)?,
                            title: r.get(4)?,
                            body: r.get(5)?,
                            model: r.get(6)?,
                            created_ms: r.get(7)?,
                            updated_ms: r.get(8)?,
                        })
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(v)
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(items))
}

// ================================================================== 注入回放

#[derive(Deserialize)]
struct ReplayEntry {
    at_ms: i64,
    lane: Option<String>,
    query: Option<String>,
    chars: Option<i64>,
    injected: Option<bool>,
    replay: Option<String>,
    /// 这一轮真正拼进提示词的上下文全文（可选）。
    ///
    /// 为什么不强制：老客户端没有这个字段，强制会让它们整体 422。
    /// 但**有就该存下来** —— 面板「注入回放」点开要看的就是它。
    #[serde(default)]
    full_text: Option<String>,
}

#[derive(Deserialize)]
struct ReplayBody {
    entries: Vec<ReplayEntry>,
}

async fn post_injection_replay(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<ReplayBody>,
) -> A<serde_json::Value> {
    crate::http::check_token(&st, &headers)?;
    if b.entries.is_empty() {
        return Ok(Json(serde_json::json!({ "ok": true, "inserted": 0 })));
    }
    let db = st.db.clone();
    let n = tokio::task::spawn_blocking(move || -> Result<usize> {
        db.with_write(|tx| {
            let mut n = 0;
            for e in &b.entries {
                // 幂等键：同一毫秒视为同一条（单主体，不再需要人作为前缀）。
                // 插件重试/重启不会让回放表翻倍。
                let key = format!("inj-{}", e.at_ms);
                let exists: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM injection_replay WHERE dedup_key = ?1",
                    rusqlite::params![key],
                    |r| r.get(0),
                )?;
                if exists > 0 {
                    continue;
                }
                tx.execute(
                    "INSERT INTO injection_replay(at_ms, lane, query, chars,
                                                  injected, replay, dedup_key, full_text)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                    rusqlite::params![
                        e.at_ms,
                        e.lane,
                        e.query,
                        e.chars,
                        e.injected.unwrap_or(false) as i64,
                        e.replay,
                        key,
                        e.full_text
                    ],
                )?;
                n += 1;
            }
            Ok(n)
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!({ "ok": true, "inserted": n })))
}

// ================================================================== 维护

#[derive(Deserialize)]
struct PruneBody {
    now_ms: Option<i64>,
    keep_journal: Option<i64>,
    fact_ceiling: Option<i64>,
}

async fn prune(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<PruneBody>,
) -> A<PruneResultDto> {
    crate::http::check_token(&st, &headers)?;
    let db = st.db.clone();
    let now = b.now_ms.unwrap_or_else(now_ms);
    let keep_journal = b.keep_journal.unwrap_or(1000);
    let ceiling = b.fact_ceiling.unwrap_or(2000);

    let r = tokio::task::spawn_blocking(move || -> Result<PruneResultDto> {
        db.with_write(|tx| {
            const D90: i64 = 90 * 24 * 3600 * 1000;
            const D7: i64 = 7 * 24 * 3600 * 1000;
            let events = tx.execute(
                "DELETE FROM event WHERE at_ms < ?1",
                rusqlite::params![now - D90],
            )? as i64;
            let qq = tx.execute(
                "DELETE FROM channel_message WHERE at_ms < ?1",
                rusqlite::params![now - D7],
            )? as i64;
            let affect = tx.execute(
                "DELETE FROM affect_log WHERE at_ms < ?1",
                rusqlite::params![now - D90],
            )? as i64;

            // 事实超上限：只淘汰 extracted（推断的），curated 是人写的，不动。
            //
            // 单主体之后不再按人循环：整库就是一份记忆，直接对全表算。
            let mut facts = 0i64;
            let n: i64 = tx.query_row("SELECT COUNT(*) FROM fact", [], |r| r.get(0))?;
            if n > ceiling {
                facts = tx.execute(
                    "DELETE FROM fact WHERE id IN (
                       SELECT id FROM fact
                       WHERE tier = 'extracted' AND status <> 'confirmed'
                       ORDER BY importance ASC, updated_ms ASC LIMIT ?1)",
                    rusqlite::params![n - ceiling],
                )? as i64;
            }

            // 手帐按条数封顶（不是按天数）——「我写过的」应该全留，超了才丢最老。
            let keep_journal = keep_journal.max(1);
            let journal_n: i64 = tx.query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))?;
            let mut journal = 0i64;
            if journal_n > keep_journal {
                journal = tx.execute(
                    "DELETE FROM journal WHERE id IN (
                       SELECT id FROM journal
                       ORDER BY at_ms ASC LIMIT ?1)",
                    rusqlite::params![journal_n - keep_journal],
                )? as i64;
            }

            Ok(PruneResultDto {
                events,
                qq_messages: qq,
                affect_log: affect,
                facts,
                journal,
            })
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(r))
}

// ================================================================== base64

fn b64_encode(b: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for chunk in b.chunks(3) {
        let n = chunk.len();
        let b0 = chunk[0] as u32;
        let b1 = if n > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if n > 2 { chunk[2] as u32 } else { 0 };
        let v = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(v >> 18) as usize & 63] as char);
        out.push(T[(v >> 12) as usize & 63] as char);
        out.push(if n > 1 {
            T[(v >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if n > 2 {
            T[v as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let bytes = s.as_bytes();
    for chunk in bytes.chunks(4) {
        if chunk.len() < 2 {
            return None;
        }
        let c0 = val(chunk[0])?;
        let c1 = val(chunk[1])?;
        let c2 = if chunk.len() > 2 && chunk[2] != b'=' {
            val(chunk[2])?
        } else {
            0
        };
        let c3 = if chunk.len() > 3 && chunk[3] != b'=' {
            val(chunk[3])?
        } else {
            0
        };
        let v = (c0 << 18) | (c1 << 12) | (c2 << 6) | c3;
        out.push((v >> 16) as u8);
        if chunk.len() > 2 && chunk[2] != b'=' {
            out.push((v >> 8) as u8);
        }
        if chunk.len() > 3 && chunk[3] != b'=' {
            out.push(v as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_往返() {
        for case in [
            vec![],
            vec![0u8],
            vec![1u8, 2],
            vec![1u8, 2, 3],
            vec![255u8, 0, 128, 7],
            (0u8..=255).collect::<Vec<_>>(),
        ] {
            let enc = b64_encode(&case);
            let dec = b64_decode(&enc).expect("解码失败");
            assert_eq!(dec, case, "往返不一致：len={}", case.len());
        }
    }

    #[test]
    fn base64_拒绝非法字符() {
        assert!(b64_decode("!!!!").is_none());
        assert!(b64_decode("A").is_none());
    }

    #[test]
    fn base64_编码结果正确() {
        // 已知答案对：base64("hello") = "aGVsbG8="
        assert_eq!(b64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(b64_encode(b"a"), "YQ==");
        assert_eq!(b64_encode(b"ab"), "YWI=");
    }
}
