//! 管理后台的 HTTP 层：登录、会话、以及给各详情页用的分页接口。
//!
//! # 鉴权与 API 的分工
//!
//! - `/api/admin/*` 用 **cookie 会话**（人在浏览器里）
//! - `/v1/*` 用 **`X-API-Token`**（脚本/其他服务）
//!
//! 两者互不替代。面板的 cookie 不能调 `/v1/*`，脚本的 token 也不该被塞进浏览器。

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::{self, Auth};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::http::AppState;
use crate::query::{self, Page};

/// 管理后台的状态：在 `AppState` 之外多一个 `Auth`。
#[derive(Clone)]
pub struct AdminState {
    pub app: AppState,
    pub auth: std::sync::Arc<Auth>,
}

impl AdminState {
    pub fn new(app: AppState, _cfg: &Config) -> Self {
        // 复用 `AppState` 里那一份 Auth：面板与「面板发出的数据写操作」
        // 必须是同一套会话校验，否则会出现「能登录却改不了」这种怪现象。
        let auth = app.auth.clone();
        Self { app, auth }
    }
}

/// 管理后台路由。
pub fn router(state: AdminState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/login", get(login_page).post(do_login))
        .route("/logout", post(do_logout).get(do_logout))
        .route("/api/admin/session", get(session_info))
        .route("/api/admin/overview", get(api_overview))
        .route("/api/admin/events", get(api_events))
        .route("/api/admin/facts", get(api_facts))
        .route("/api/admin/journal", get(api_journal))
        .route("/api/admin/diaries", get(api_diaries))
        .route(
            "/api/admin/persons",
            get(api_persons).post(api_create_person),
        )
        .route("/api/admin/moods", get(api_moods))
        .route("/api/admin/messages", get(api_messages))
        .route("/api/admin/recall-traces", get(api_recall_traces))
        .route("/api/admin/injections", get(api_injections))
        .route(
            "/api/admin/settings",
            get(api_settings).post(api_save_settings),
        )
        .route("/api/admin/llm", get(api_get_llm).post(api_save_llm))
        .route("/api/admin/llm/test", axum::routing::post(api_test_llm))
        .route("/api/admin/generate/log", get(api_gen_log))
        .route("/api/admin/generate/stats", get(api_gen_stats))
        .route("/api/admin/mail", get(api_get_mail).post(api_save_mail))
        .route(
            "/api/admin/relation/impressions/backfill",
            axum::routing::post(api_backfill_impressions),
        )
        .route("/api/admin/mail/test", axum::routing::post(api_test_mail))
        .route(
            "/api/admin/generate/tick",
            axum::routing::post(api_force_tick),
        )
        .route(
            "/api/admin/generate/backfill",
            axum::routing::post(api_backfill),
        )
        .route("/api/admin/plans", get(api_plans))
        .route("/api/admin/plan/{id}", get(api_plan_detail))
        .route("/api/admin/timeline", get(api_timeline))
        .route("/api/admin/self", get(api_self))
        .route(
            "/api/admin/relations",
            get(api_relations).post(api_create_relation),
        )
        .route(
            "/api/admin/channels",
            get(api_channels).post(api_create_channel),
        )
        .route(
            "/api/admin/personas",
            get(api_personas).post(api_create_persona),
        )
        .route(
            "/api/admin/personas/{id}",
            axum::routing::patch(api_patch_persona).delete(api_delete_persona),
        )
        .route("/api/admin/persona/preview", get(api_persona_preview))
        .route(
            "/api/admin/persona/import",
            axum::routing::post(api_import_persona),
        )
        .route(
            "/api/admin/channels/{tag}",
            axum::routing::patch(api_patch_channel).delete(api_delete_channel),
        )
        .route(
            "/api/admin/relations/{person_id}",
            axum::routing::patch(api_patch_relation),
        )
        .route("/api/admin/traits", get(api_traits))
        .route("/api/admin/promises", get(api_promises))
        .route("/api/admin/snapshots", get(api_snapshots))
        .route("/api/admin/rollbacks", get(api_rollbacks))
        // 后台自己的写操作：用会话鉴权，不需要 API token。
        // 之前面板让人去填 token 才能拍快照，那是把两条鉴权链混为一谈了。
        .route("/api/admin/snapshot", post(api_take_snapshot))
        .route(
            "/api/admin/person/{id}",
            axum::routing::patch(api_patch_person),
        )
        .route("/api/admin/person/{id}/account", post(api_bind_account))
        // ---------------------------------------------------------- 知识库
        //
        // 走会话鉴权，与 `/v1/kb/*`（token 鉴权）是两条链，互不替代：
        // 人在浏览器里用前者，脚本和其他服务用后者。
        .route("/api/admin/kb", get(api_kb_list).post(api_kb_create))
        .route("/api/admin/kb/health", get(api_kb_health))
        .route("/api/admin/kb/doc", post(api_kb_import))
        .route(
            "/api/admin/kb/doc/{id}",
            get(api_kb_get_doc)
                .patch(api_kb_patch_doc)
                .delete(api_kb_delete_doc),
        )
        .route("/api/admin/kb/chunk/{id}", get(api_kb_chunk_detail))
        .route("/api/admin/kb/search", get(api_kb_search))
        .route("/api/admin/kb/reembed", post(api_kb_reembed))
        .with_state(state)
}

// ------------------------------------------------------------------ 会话

/// 取出并校验会话。失败即 401（前端会跳转登录页）。
fn require_session(st: &AdminState, headers: &HeaderMap) -> Result<String> {
    if !st.auth.login_required() {
        // 未配置登录：只在「监听回环」时才允许裸奔。
        // 监听内网却没口令的后台等于把所有记忆摊开给整个内网。
        if !st.app.cfg.effective_bind().ip().is_loopback() {
            return Err(Error::Unauthorized);
        }
        return Ok("(未启用登录)".to_string());
    }
    let sid = auth::session_from_cookie(
        headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok()),
    );
    st.auth.verify(&sid).ok_or(Error::Unauthorized)
}

#[derive(Serialize)]
struct SessionInfo {
    /// 是否启用了登录。
    login_required: bool,
    /// 当前登录用户，未登录为 null。
    user: Option<String>,
    active_sessions: usize,
}

async fn session_info(State(st): State<AdminState>, headers: HeaderMap) -> Json<SessionInfo> {
    let sid = auth::session_from_cookie(
        headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok()),
    );
    Json(SessionInfo {
        login_required: st.auth.login_required(),
        user: st.auth.verify(&sid),
        active_sessions: st.auth.active_sessions(),
    })
}

// ------------------------------------------------------------------ 登录页

async fn index(State(st): State<AdminState>, headers: HeaderMap) -> Response {
    require_session(&st, &headers)
        .map(|_| Html(crate::webui::INDEX_HTML).into_response())
        .unwrap_or_else(|_| Redirect::to("/login").into_response())
}

async fn login_page(State(st): State<AdminState>, headers: HeaderMap) -> Response {
    // 已经登录就直接进后台，省一次点击。
    if require_session(&st, &headers).is_ok() {
        return Redirect::to("/").into_response();
    }
    Html(crate::webui::LOGIN_HTML).into_response()
}

#[derive(Deserialize)]
struct LoginForm {
    user: String,
    pass: String,
}

async fn do_login(State(st): State<AdminState>, Form(f): Form<LoginForm>) -> Response {
    if st.auth.check(&f.user, &f.pass) {
        let sid = st.auth.issue(if f.user.is_empty() {
            "operator"
        } else {
            &f.user
        });
        let mut resp = Redirect::to("/").into_response();
        if let Ok(v) = axum::http::HeaderValue::from_str(&auth::cookie_header(&sid)) {
            resp.headers_mut().insert(axum::http::header::SET_COOKIE, v);
        }
        resp
    } else {
        // 固定延时：让脚本化猜测变得没意义，又不需要锁定表。
        tokio::time::sleep(std::time::Duration::from_millis(auth::FAIL_DELAY_MS)).await;
        let mut resp = Redirect::to("/login?e=1").into_response();
        *resp.status_mut() = StatusCode::SEE_OTHER;
        resp
    }
}

async fn do_logout(State(st): State<AdminState>, headers: HeaderMap) -> Response {
    let sid = auth::session_from_cookie(
        headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok()),
    );
    st.auth.logout(&sid);
    let mut resp = Redirect::to("/login").into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&auth::clear_cookie_header()) {
        resp.headers_mut().insert(axum::http::header::SET_COOKIE, v);
    }
    resp
}

// ------------------------------------------------------------------ 分页查询

/// 分页 + 过滤的查询参数。所有字段可选。
#[derive(Deserialize, Default)]
struct ListParams {
    page: Option<u32>,
    size: Option<u32>,
    q: Option<String>,
    channel: Option<String>,
    kind: Option<String>,
    status: Option<String>,
    tier: Option<String>,
    /// `1` = 只看被回滚遗忘的。
    forgotten: Option<u8>,
}

impl ListParams {
    fn page(&self) -> Page {
        Page::new(self.page, self.size)
    }
}

type ApiResult<T> = std::result::Result<Json<T>, Error>;

async fn api_overview(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<query::Overview> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let o = tokio::task::spawn_blocking(move || query::overview(&db))
        .await
        .map_err(|e| Error::Storage(format!("概览查询失败：{e}")))??;
    Ok(Json(o))
}

async fn api_events(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(p): Query<ListParams>,
) -> ApiResult<query::Paged<query::EventItem>> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let f = query::EventFilter {
        channel: p.channel.clone(),
        kind: p.kind.clone(),
        q: p.q.clone(),
        from_ms: None,
        to_ms: None,
        forgotten: p.forgotten == Some(1),
    };
    let page = p.page();
    let r = tokio::task::spawn_blocking(move || query::events(&db, &f, page))
        .await
        .map_err(|e| Error::Storage(format!("事件查询失败：{e}")))??;
    Ok(Json(r))
}

async fn api_facts(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(p): Query<ListParams>,
) -> ApiResult<query::Paged<query::FactItem>> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let f = query::FactFilter {
        q: p.q.clone(),
        status: p.status.clone(),
        tier: p.tier.clone(),
        show_superseded: p.forgotten == Some(1),
    };
    let page = p.page();
    let r = tokio::task::spawn_blocking(move || query::facts(&db, &f, page))
        .await
        .map_err(|e| Error::Storage(format!("事实查询失败：{e}")))??;
    Ok(Json(r))
}

async fn api_journal(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(p): Query<ListParams>,
) -> ApiResult<query::Paged<query::JournalItem>> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let q = p.q.clone();
    let page = p.page();
    let r = tokio::task::spawn_blocking(move || query::journal(&db, q.as_deref(), page))
        .await
        .map_err(|e| Error::Storage(format!("手帐查询失败：{e}")))??;
    Ok(Json(r))
}

async fn api_diaries(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(p): Query<ListParams>,
) -> ApiResult<query::Paged<query::DiaryItem>> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let k = p.kind.clone();
    let page = p.page();
    let r = tokio::task::spawn_blocking(move || query::diaries(&db, k.as_deref(), page))
        .await
        .map_err(|e| Error::Storage(format!("日记查询失败：{e}")))??;
    Ok(Json(r))
}

async fn api_persons(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(p): Query<ListParams>,
) -> ApiResult<query::Paged<query::PersonItem>> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let q = p.q.clone();
    let page = p.page();
    let r = tokio::task::spawn_blocking(move || query::persons(&db, q.as_deref(), page))
        .await
        .map_err(|e| Error::Storage(format!("身份查询失败：{e}")))??;
    Ok(Json(r))
}

#[derive(Deserialize)]
struct NewPerson {
    display_name: String,
    status: Option<String>,
    note: Option<String>,
    /// 可选：建的时候顺手填各场景称呼（等同随后在抽屉里填）。
    scenes: Option<std::collections::HashMap<String, String>>,
}

/// 新建一个身份。
///
/// 为什么需要它：身份页原先只能看到「已经出现过的人」（由渠道写入时自动建），
/// 想手动登记一个还没出现过的人无处可点。关系页的新建能顺手建人，
/// 但那要求你同时建一条关系——只想先登记身份时不该被逼着造关系。
async fn api_create_person(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Json(b): Json<NewPerson>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;

    let name = b.display_name.trim().to_string();
    if name.is_empty() {
        return Err(Error::BadRequest("显示名不能为空".into()));
    }
    if name.chars().count() > 32 {
        return Err(Error::BadRequest("显示名最长 32 字".into()));
    }
    let status = match b.status.as_deref().map(str::trim) {
        None | Some("") => "confirmed",
        Some(s @ ("pending" | "confirmed")) => s,
        Some(other) => {
            return Err(Error::BadRequest(format!(
                "状态只能是 pending / confirmed，收到 {other}"
            )))
        }
    }
    .to_string();
    let note = b
        .note
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let scenes = b.scenes.unwrap_or_default();
    check_scenes(&scenes)?;

    let db = st.app.db.clone();
    let person_id = tokio::task::spawn_blocking(move || -> Result<String> {
        db.with_write(|tx| {
            let now = crate::db::now_ms();
            let pid = new_person_id(tx, &name)?;
            tx.execute(
                "INSERT INTO person(person_id, display_name, status, created_ms, note)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![pid, name, status, now, note],
            )?;
            write_scenes(tx, &pid, &scenes, now)?;
            Ok(pid)
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("新建身份失败：{e}")))??;

    Ok(Json(
        serde_json::json!({ "ok": true, "person_id": person_id }),
    ))
}

async fn api_moods(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let items = tokio::task::spawn_blocking(move || query::moods(&db))
        .await
        .map_err(|e| Error::Storage(format!("情绪查询失败：{e}")))??;
    Ok(Json(serde_json::json!({ "items": items })))
}

async fn api_plans(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(p): Query<ListParams>,
) -> ApiResult<query::Paged<query::PlanRow>> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let q = p.q.clone();
    let page = p.page();
    let r = tokio::task::spawn_blocking(move || query::plans(&db, q.as_deref(), page))
        .await
        .map_err(|e| Error::Storage(format!("规划查询失败：{e}")))??;
    Ok(Json(r))
}

/// 单份规划的全部内容。列表页不拉条目，点开才拉——
/// 30 份规划 × 122 个条目，一次全拉是没必要的。
async fn api_plan_detail(
    State(st): State<AdminState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<query::PlanDetail> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let d = tokio::task::spawn_blocking(move || query::plan_detail(&db, &id))
        .await
        .map_err(|e| Error::Storage(format!("规划详情失败：{e}")))??;
    Ok(Json(d))
}

async fn api_messages(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(p): Query<ListParams>,
) -> ApiResult<query::Paged<query::MessageRow>> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let (ch, q) = (p.channel.clone(), p.q.clone());
    let page = p.page();
    let r = tokio::task::spawn_blocking(move || {
        query::messages(&db, ch.as_deref(), q.as_deref(), page)
    })
    .await
    .map_err(|e| Error::Storage(format!("消息查询失败：{e}")))??;
    Ok(Json(r))
}

async fn api_recall_traces(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let items = tokio::task::spawn_blocking(move || query::recall_traces(&db, 30))
        .await
        .map_err(|e| Error::Storage(format!("回放查询失败：{e}")))??;
    Ok(Json(serde_json::json!({ "items": items })))
}

async fn api_injections(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(_q): Query<CompanionQ>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let items = tokio::task::spawn_blocking(move || query::injection_replays(&db, 60))
        .await
        .map_err(|e| Error::Storage(format!("回放查询失败：{e}")))??;
    Ok(Json(serde_json::json!({ "items": items })))
}

#[derive(Deserialize)]
struct PersonaImportReq {
    /// MC 那边的 NPC 档案路径（`shared/deepseek_mc/npcs/<名字>.json`）。
    path: String,
    /// 用这个正则匹配模型（留空 = 兜底）。
    #[serde(default)]
    model_regex: String,
    /// 覆盖已有的同名人格。
    #[serde(default)]
    overwrite: bool,
}

/// 从 MC 的 NPC 档案导入人设。
///
/// # 为什么需要它
///
/// 中枢的 `persona` 表现在只有**标签**（`CETACEA_LOLI`、`PERSONALITY_TSUNDERE_SWEET`…），
/// 而真正的人设在 MC 档案的顶层 `persona` 字段里 —— **1926 字**，
/// 包括外貌、性子、对主人的态度、说话方式。手抄一遍既费事又会走样。
///
/// # 为什么不让生成任务直接读 MC 文件
///
/// 那会把中枢重新绑死到 MC 上 —— 而这一整轮改造的目的正是**解开这个依赖**
/// （MC 不开机，日记/手帐/情绪全停）。所以只做一次性导入：
/// 导入之后中枢自带人设，MC 关着也能正确生成。
async fn api_import_persona(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Json(req): Json<PersonaImportReq>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;

    let path = std::path::PathBuf::from(req.path.trim());
    if !path.is_file() {
        return Err(Error::BadRequest(format!(
            "文件不存在：{}（要填 MC 那边的 npcs/<名字>.json）",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Error::BadRequest(format!("读不了这个文件：{e}")))?;
    let j: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Error::BadRequest(format!("不是合法的 JSON：{e}")))?;

    let body = j
        .get("persona")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::BadRequest("这个档案里没有 persona 字段（可能是个旧版档案）".into())
        })?;
    if body.chars().count() < 50 {
        return Err(Error::BadRequest(
            "persona 太短了（<50 字），可能是占位内容，不导入".into(),
        ));
    }
    // 名字：优先 displayName，否则用文件名
    let name = j
        .get("displayName")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "导入的人格".into())
        });

    let db = st.app.db.clone();
    let body = body.to_string();
    let re = req.model_regex.trim().to_string();
    let overwrite = req.overwrite;
    let path_display = path.display().to_string();
    let out = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let now = crate::db::now_ms();
        db.with_write(|tx| {
            // 同名的：覆盖或拒绝
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM persona WHERE name = ?1 LIMIT 1",
                    rusqlite::params![&name],
                    |r| r.get(0),
                )
                .ok();
            if let Some(id) = existing {
                if !overwrite {
                    return Err(Error::BadRequest(format!(
                        "已经有一条叫「{name}」的人格了。要覆盖就把 overwrite 打开"
                    )));
                }
                tx.execute(
                    "UPDATE persona SET body = ?1, model_regex = ?2, updated_ms = ?3 WHERE id = ?4",
                    rusqlite::params![body, re, now, id],
                )?;
                return Ok(serde_json::json!({
                    "ok": true, "id": id, "name": name, "chars": body.chars().count(),
                    "updated": true,
                }));
            }
            let id = format!("per-{}-{}", now, crate::http::rand_suffix());
            tx.execute(
                "INSERT INTO persona(id, name, model_regex, body, priority, enabled,
                                     note, created_ms, updated_ms)
                 VALUES (?1,?2,?3,?4,0,1,?5,?6,?6)",
                rusqlite::params![
                    id,
                    name,
                    re,
                    body,
                    format!("从 MC 档案导入：{}", path_display),
                    now
                ],
            )?;
            Ok(serde_json::json!({
                "ok": true, "id": id, "name": name, "chars": body.chars().count(),
                "updated": false,
            }))
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(out))
}

// ================================================================== LLM 配置

async fn api_get_llm(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let llm = st
        .app
        .llm
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    let gen = st
        .app
        .gen_cfg
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    Ok(Json(serde_json::json!({
        "llm": llm.public_view(),
        "generate": gen,
    })))
}

#[derive(Deserialize)]
struct LlmPatch {
    base_url: Option<String>,
    /// 只在**非空**时覆盖：面板不回传明文 key，留空表示「不改」。
    api_key: Option<String>,
    model: Option<String>,
    timeout_seconds: Option<u64>,
    max_tokens: Option<u32>,
    temperature: Option<f64>,
    retries: Option<u32>,
    /// 生成任务配置（可选，一起提交省一次请求）。
    generate: Option<crate::generate::GenConfig>,
}

async fn api_save_llm(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Json(p): Json<LlmPatch>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;

    let mut llm = st
        .app
        .llm
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    if let Some(v) = p.base_url {
        llm.base_url = v.trim().to_string();
    }
    // 空 key = 不改（面板不回传明文，留空是常态）
    if let Some(v) = p.api_key.as_deref().map(str::trim) {
        if !v.is_empty() {
            llm.api_key = v.to_string();
        }
    }
    if let Some(v) = p.model {
        llm.model = v.trim().to_string();
    }
    if let Some(v) = p.timeout_seconds {
        if !(5..=600).contains(&v) {
            return Err(Error::BadRequest("timeout_seconds 要在 5..600".into()));
        }
        llm.timeout_seconds = v;
    }
    if let Some(v) = p.max_tokens {
        if !(64..=8192).contains(&v) {
            return Err(Error::BadRequest("max_tokens 要在 64..8192".into()));
        }
        llm.max_tokens = v;
    }
    if let Some(v) = p.temperature {
        if !(0.0..=2.0).contains(&v) {
            return Err(Error::BadRequest("temperature 要在 0..2".into()));
        }
        llm.temperature = v;
    }
    if let Some(v) = p.retries {
        llm.retries = v.min(5);
    }

    let mut gen = st
        .app
        .gen_cfg
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    if let Some(g) = p.generate {
        gen = g;
    }
    gen.validate()?;

    // 先落盘再改内存（同权重那套理由：落盘失败不该变成「重启就丢」）
    crate::http::save_llm(&st.app.cfg, &llm, &gen);
    if let Ok(mut g) = st.app.llm.write() {
        *g = llm.clone();
    }
    if let Ok(mut g) = st.app.gen_cfg.write() {
        *g = gen.clone();
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "llm": llm.public_view(),
        "generate": gen,
    })))
}

/// 试一下 LLM 通不通。
///
/// 用户配完最想知道的是「能不能用」——等到第二天才发现 401 太晚了。
/// 这个接口发一句最短的请求，把**原始错误**带回来（含 401/404 的区分）。
async fn api_test_llm(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let llm = st
        .app
        .llm
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    if !llm.is_ready() {
        return Err(Error::BadRequest(
            "还没配齐：base_url / api_key / model 都要填".into(),
        ));
    }
    let client = crate::llm::ChatClient::new(llm);
    let t0 = std::time::Instant::now();
    match client
        .chat(&[crate::llm::ChatMessage::user("回复两个字：收到")])
        .await
    {
        Ok(o) => Ok(Json(serde_json::json!({
            "ok": true,
            "reply": o.text,
            "elapsed_ms": t0.elapsed().as_millis() as i64,
            "prompt_tokens": o.prompt_tokens,
            "completion_tokens": o.completion_tokens,
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "ok": false,
            "error": e.to_string(),
            "elapsed_ms": t0.elapsed().as_millis() as i64,
        }))),
    }
}

#[derive(Deserialize)]
struct GenLogQuery {
    limit: Option<usize>,
}

async fn api_gen_log(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(q): Query<GenLogQuery>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let limit = q.limit.unwrap_or(50);
    let (items, fails) = tokio::task::spawn_blocking(move || {
        let items = crate::generate::runner::recent(&db, limit)?;
        // 各类任务的连续失败数：面板用它在页头显示红点
        let mut fails = serde_json::Map::new();
        for p in [
            crate::llm::Purpose::Tick,
            crate::llm::Purpose::Diary,
            crate::llm::Purpose::Weekly,
        ] {
            fails.insert(
                p.as_str().to_string(),
                serde_json::json!(crate::generate::runner::consecutive_failures(&db, p)),
            );
        }
        Ok::<_, crate::error::Error>((items, fails))
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))?
    .map_err(|e| Error::Storage(e.to_string()))?;

    let st2 = st.app.gen_state.throttle.lock().map(|g| {
        serde_json::json!({
            "pending": g.pending(),
            "last_run_ms": g.last_run_ms(),
        })
    });
    Ok(Json(serde_json::json!({
        "items": items,
        "consecutive_failures": fails,
        "tick_state": st2.unwrap_or(serde_json::Value::Null),
    })))
}

#[derive(Deserialize)]
struct ImpressionBackfillReq {
    /// 只处理这些人；空 = 所有还没有印象的关系。
    person_ids: Option<Vec<String>>,
    /// 已有印象也重算（默认不覆盖）。
    #[serde(default)]
    overwrite: bool,
}

/// 按历史事件/日记为已有关系回溯生成「印象」。
///
/// 用户要求「增加后先按照历史事件/日记做一次已有关系等印象数据生成」。
/// 手动触发而不是后台自动跑：一次回溯要花 N 个请求（N = 人数），
/// 该由人决定什么时候花。
async fn api_backfill_impressions(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Json(req): Json<ImpressionBackfillReq>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let llm = st
        .app
        .llm
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    if !llm.is_ready() {
        return Err(Error::BadRequest("LLM 还没配置，先在「模型」页填好".into()));
    }
    let gen = st
        .app
        .gen_cfg
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    let mut c = (*st.app.cfg).clone();
    c.llm = llm;
    c.generate = gen;

    let db = st.app.db.clone();
    let targets: Vec<String> = match req.person_ids {
        Some(v) if !v.is_empty() => v,
        _ => {
            // 没指定人时选谁：
            // - 默认（overwrite=false）→ 只补**还没有印象**的
            // - overwrite=true → **全部重算**（用户想拿新的人设重刷一遍）
            //   ⚠️ 之前这里恒为「只补空的」，于是 `{"overwrite":true}`
            //   什么都不做、还返回 ok —— 最容易让人以为「生成成功了」。
            let db2 = db.clone();
            let ow = req.overwrite;
            tokio::task::spawn_blocking(move || -> crate::error::Result<Vec<String>> {
                db2.with_read(|cc| {
                    let sql = if ow {
                        "SELECT person_id FROM relation"
                    } else {
                        "SELECT person_id FROM relation
                         WHERE impression IS NULL OR TRIM(impression) = ''"
                    };
                    let mut stmt = cc.prepare(sql)?;
                    let v = stmt
                        .query_map([], |r| r.get::<_, String>(0))?
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    Ok(v)
                })
            })
            .await
            .map_err(|e| Error::Storage(e.to_string()))??
        }
    };

    let mut done = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    for pid in targets {
        match crate::generate::backfill_impression::one(&db, &c, &pid, req.overwrite).await {
            Ok(true) => {
                let imp: Option<String> = db
                    .with_read(|cc| {
                        Ok(cc
                            .query_row(
                                "SELECT impression FROM relation WHERE person_id = ?1",
                                rusqlite::params![&pid],
                                |r| r.get::<_, Option<String>>(0),
                            )
                            .ok()
                            .flatten())
                    })
                    .unwrap_or(None);
                done.push(serde_json::json!({ "person_id": pid, "impression": imp }));
            }
            Ok(false) => skipped.push(serde_json::json!({ "person_id": pid, "why": "已有印象" })),
            Err(e) => failed.push(serde_json::json!({ "person_id": pid, "error": e.to_string() })),
        }
        // 每个人之间歇一下：连续打会被网关限流，而回溯不着急
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    }
    Ok(Json(serde_json::json!({
        "ok": true, "done": done, "skipped": skipped, "failed": failed,
    })))
}

// ================================================================== SMTP

async fn api_get_mail(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let mc = st
        .app
        .mail
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    Ok(Json(mc.public_view()))
}

#[derive(Deserialize)]
struct MailPatch {
    enable: Option<bool>,
    host: Option<String>,
    port: Option<u16>,
    username: Option<String>,
    /// 空 = 不改（面板不回显）。
    password: Option<String>,
    from: Option<String>,
    to: Option<String>,
    encrypt: Option<bool>,
    passphrase: Option<String>,
    timeout_seconds: Option<u64>,
    on_snapshot: Option<bool>,
}

async fn api_save_mail(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Json(p): Json<MailPatch>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let mut mc = st
        .app
        .mail
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    if let Some(v) = p.enable {
        mc.enable = v;
    }
    if let Some(v) = p.host {
        mc.host = v.trim().to_string();
    }
    if let Some(v) = p.port {
        mc.port = v;
    }
    if let Some(v) = p.username {
        mc.username = v.trim().to_string();
    }
    // 空密码/口令 = 不改
    if let Some(v) = p.password.as_deref() {
        if !v.is_empty() {
            mc.password = v.to_string();
        }
    }
    if let Some(v) = p.passphrase.as_deref() {
        if !v.is_empty() {
            mc.passphrase = v.to_string();
        }
    }
    if let Some(v) = p.from {
        mc.from = v.trim().to_string();
    }
    if let Some(v) = p.to {
        mc.to = v.trim().to_string();
    }
    if let Some(v) = p.encrypt {
        mc.encrypt = v;
    }
    if let Some(v) = p.timeout_seconds {
        mc.timeout_seconds = v.clamp(5, 300);
    }
    if let Some(v) = p.on_snapshot {
        mc.on_snapshot = v;
    }

    // 落盘前先校验：开了加密没口令这种事，保存时就要拦住
    if mc.enable {
        mc.validate_for_send()
            .map_err(|e| Error::BadRequest(e.to_string()))?;
    }

    let llm = st
        .app
        .llm
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    let gen = st
        .app
        .gen_cfg
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    crate::http::save_all(&st.app.cfg, &llm, &gen, &mc);
    if let Ok(mut g) = st.app.mail.write() {
        *g = mc.clone();
    }
    Ok(Json(
        serde_json::json!({ "ok": true, "mail": mc.public_view() }),
    ))
}

/// 发一封测试邮件（不带附件）。配完立刻知道通不通。
async fn api_test_mail(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let mc = st
        .app
        .mail
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    let to = mc
        .recipients()
        .iter()
        .map(|a| crate::mail::mask_addr(a))
        .collect::<Vec<_>>()
        .join(", ");
    let body = "这是一封测试邮件。\n\n如果你收到它，说明 SMTP 配置可用；\n                真正的快照会在每天生成后自动带上附件发出。\n";
    let r = tokio::task::spawn_blocking(move || {
        crate::mail::send(&mc, "蓝鲸记忆中枢 SMTP 测试", body, None)
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))?;
    match r {
        Ok(ms) => Ok(Json(serde_json::json!({
            "ok": true, "elapsed_ms": ms, "to": to,
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "ok": false, "error": e.to_string(), "to": to,
        }))),
    }
}

#[derive(Deserialize)]
struct StatsQuery {
    /// 明细返回多少条（默认 30）。
    recent: Option<usize>,
}

/// AI 调用统计：调了哪些模型、1/7/30 天 token 消耗。
async fn api_gen_stats(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(q): Query<StatsQuery>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let recent = q.recent.unwrap_or(30);
    let now = crate::db::now_ms();
    let stats = tokio::task::spawn_blocking(move || crate::generate::stats::load(&db, now, recent))
        .await
        .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!(stats)))
}

/// 手动触发一次 tick（面板按钮）。调试与「刚改完配置想立刻看到效果」用。
async fn api_force_tick(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let cfg = st.app.cfg.clone();
    let llm = st
        .app
        .llm
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    let gen = st
        .app
        .gen_cfg
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    if !llm.is_ready() {
        return Err(Error::BadRequest("LLM 还没配置，先在「模型」页填好".into()));
    }
    // 用当前的热配置造一份临时 Config（不动全局）
    let mut c = (*cfg).clone();
    c.llm = llm;
    c.generate = gen;
    let summary = crate::generate::exec::run_tick(&db, &c, &st.app.gen_state, true).await?;
    Ok(Json(serde_json::json!({ "ok": true, "result": summary })))
}

/// 当前的运行配置（只读部分 + 可改的权重）。
///
/// **注意这里不放 token / 口令**：面板能读配置不等于能读凭据。
/// 那两条一旦出现在这里，任何一次截图或日志转发都会泄露。
#[derive(Deserialize)]
struct BackfillReq {
    /// `YYYY-MM-DD` 列表。空 = 自动找最近 7 天的缺口。
    days: Option<Vec<String>>,
}

/// 回填历史日记。
///
/// 用户要求「要做，以测试效果」——所以这个接口是**手动触发 + 明确报进度**，
/// 不做成后台自动跑：一次回填会花好几个请求，该由人决定什么时候花。
async fn api_backfill(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Json(req): Json<BackfillReq>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let llm = st
        .app
        .llm
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    if !llm.is_ready() {
        return Err(Error::BadRequest("LLM 还没配置".into()));
    }
    let gen = st
        .app
        .gen_cfg
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    let mut c = (*st.app.cfg).clone();
    c.llm = llm;
    c.generate = gen;

    // 要补哪些天：显式给的，否则最近 7 天里没有 day 日记的
    let db = st.app.db.clone();
    let want: Vec<i64> = match req.days {
        Some(ds) => {
            let mut out = Vec::new();
            for d in ds {
                match crate::generate::collect::parse_day(&d) {
                    Some(ms) => out.push(ms),
                    None => {
                        return Err(Error::BadRequest(format!(
                            "日期格式要 YYYY-MM-DD，收到 {d}"
                        )))
                    }
                }
            }
            out
        }
        None => {
            let db2 = db.clone();
            tokio::task::spawn_blocking(move || crate::generate::collect::missing_days(&db2, 7))
                .await
                .map_err(|e| Error::Storage(e.to_string()))??
        }
    };

    let mut done = Vec::new();
    let mut failed = Vec::new();
    for day in want {
        let label = crate::generate::collect::fmt_day(day);
        match crate::generate::daily::backfill_day(&db, &c, day).await {
            Ok(id) => done.push(serde_json::json!({"day": label, "id": id})),
            Err(e) => failed.push(serde_json::json!({"day": label, "error": e.to_string()})),
        }
        // 每次之间歇一下：连续打会被网关限流（429），而回填不着急
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "done": done,
        "failed": failed,
    })))
}

async fn api_settings(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let cfg = &st.app.cfg;
    Ok(Json(serde_json::json!({
        "bind": cfg.effective_bind().to_string(),
        "loopback_only": cfg.effective_bind().ip().is_loopback(),
        "data_dir": cfg.data_dir.to_string_lossy(),
        "db_path": cfg.db_path().to_string_lossy(),
        "snapshot_dir": cfg.snapshot_dir().to_string_lossy(),
        "snapshot_keep_days": cfg.snapshot_keep_days,
        "snapshot_check_seconds": cfg.snapshot_check_seconds,
        "max_concurrency": cfg.max_concurrency,
        "max_body_bytes": cfg.max_body_bytes,
        "request_timeout_seconds": cfg.request_timeout_seconds,
        "token_configured": !cfg.token.is_empty(),
        "panel_login": !cfg.panel_user.is_empty(),
        "panel_user": cfg.panel_user,
        "weight": st.app
            .weight
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone()),
    })))
}

/// 保存权重配置。
///
/// 只允许改权重模型这一块：它决定检索排序，改错了能立刻看到效果，
/// 也随时能改回来。端口、数据目录这类**重新绑定或迁移**的配置不在面板里改——
/// 那些需要重启，而「面板上点一下就换数据目录」是个危险的能力。
#[derive(Deserialize)]
struct WeightPatch {
    importance_exp: Option<f64>,
    half_life_days: Option<f64>,
    decay_floor: Option<f64>,
    reinforce_gain: Option<f64>,
    reinforce_cap: Option<f64>,
    channel_trust_mc: Option<f64>,
    channel_trust_qq: Option<f64>,
    channel_trust_live: Option<f64>,
    channel_trust_system: Option<f64>,
}

async fn api_save_settings(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Json(p): Json<WeightPatch>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;

    // 基于当前值打补丁，然后**整体校验**：权重的各个参数互相约束
    // （比如 reinforce_cap < 1 会让强化变成削弱），逐字段校验抓不到这种组合错误。
    let mut w = st
        .app
        .weight
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());

    if let Some(v) = p.importance_exp {
        w.importance_exp = v;
    }
    if let Some(v) = p.half_life_days {
        w.half_life_days = v;
    }
    if let Some(v) = p.decay_floor {
        w.decay_floor = v;
    }
    if let Some(v) = p.reinforce_gain {
        w.reinforce_gain = v;
    }
    if let Some(v) = p.reinforce_cap {
        w.reinforce_cap = v;
    }
    if let Some(v) = p.channel_trust_mc {
        w.channel_trust_mc = v;
    }
    if let Some(v) = p.channel_trust_qq {
        w.channel_trust_qq = v;
    }
    if let Some(v) = p.channel_trust_live {
        w.channel_trust_live = v;
    }
    if let Some(v) = p.channel_trust_system {
        w.channel_trust_system = v;
    }

    w.validate().map_err(Error::BadRequest)?;

    // 先落盘再改内存：反过来的话，落盘失败会让「内存生效了但重启就丢」
    // 变成一件没人知道的事。
    crate::http::save_weights(&st.app.cfg, &w);
    if let Ok(mut g) = st.app.weight.write() {
        *g = w.clone();
    }

    Ok(Json(serde_json::json!({ "ok": true, "weight": w })))
}

#[derive(Deserialize)]
struct TimelineQ {
    days: Option<usize>,
}

async fn api_timeline(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(q): Query<TimelineQ>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let days = q.days.unwrap_or(12);
    let items = tokio::task::spawn_blocking(move || query::timeline(&db, days))
        .await
        .map_err(|e| Error::Storage(format!("时间线查询失败：{e}")))??;
    Ok(Json(serde_json::json!({ "items": items })))
}

#[derive(Deserialize)]
struct CompanionQ {}

async fn api_self(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(_q): Query<CompanionQ>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let items = tokio::task::spawn_blocking(move || query::self_narratives(&db))
        .await
        .map_err(|e| Error::Storage(format!("自述查询失败：{e}")))??;
    Ok(Json(serde_json::json!({ "items": items })))
}

async fn api_relations(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(_q): Query<CompanionQ>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let items = tokio::task::spawn_blocking(move || query::relations(&db))
        .await
        .map_err(|e| Error::Storage(format!("关系查询失败：{e}")))??;
    Ok(Json(serde_json::json!({ "items": items })))
}

/// 校验场景键。场景集合只在 `query::PERSON_SCENES` 定义一处，
/// 这里也用它 —— 两处各写一套迟早会对不上（面板能选、后端不认）。
fn check_scenes(scenes: &std::collections::HashMap<String, String>) -> Result<()> {
    for key in scenes.keys() {
        if !query::is_person_scene(key) {
            return Err(Error::BadRequest(format!(
                "未知场景：{key}（只能是 mc / qq / qq_group / live）"
            )));
        }
    }
    Ok(())
}

/// 把场景称呼写进库。空串 = 删掉这一行（回到自动推导），不写空名字。
fn write_scenes(
    tx: &rusqlite::Connection,
    person_id: &str,
    scenes: &std::collections::HashMap<String, String>,
    now: i64,
) -> Result<()> {
    check_scenes(scenes)?;
    for (scene, name) in scenes {
        let name = name.trim();
        if name.is_empty() {
            tx.execute(
                "DELETE FROM person_scene WHERE person_id = ?1 AND scene = ?2",
                rusqlite::params![person_id, scene],
            )?;
            continue;
        }
        if name.chars().count() > 64 {
            return Err(Error::BadRequest(format!("场景 {scene} 的称呼最长 64 字")));
        }
        tx.execute(
            "INSERT INTO person_scene(person_id, scene, name, updated_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(person_id, scene) DO UPDATE SET
               name = excluded.name, updated_ms = excluded.updated_ms",
            rusqlite::params![person_id, scene, name, now],
        )?;
    }
    Ok(())
}

/// 亲密度/熟悉/信任一律夹到 0–100：填 120 是手滑，不是想要 120 倍关系。
fn score(v: i64) -> i64 {
    v.clamp(0, 100)
}

/// 给人建一个可读的 id。中文名没法转 slug，就用时间戳兜底。
fn new_person_id(tx: &rusqlite::Connection, display_name: &str) -> Result<String> {
    let slug: String = display_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect();
    let base = if slug.is_empty() {
        format!("p-{:x}", crate::db::now_ms())
    } else {
        format!("p-{slug}")
    };
    for n in 0..1000 {
        let id = if n == 0 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        let used: i64 = tx.query_row(
            "SELECT COUNT(*) FROM person WHERE person_id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )?;
        if used == 0 {
            return Ok(id);
        }
    }
    Err(Error::BadRequest("生成身份 id 失败（重名太多）".into()))
}

#[derive(Deserialize)]
struct NewRelation {
    /// 关联一个已有身份。给了它就不用 `display_name`。
    person_id: Option<String>,
    /// 没有 `person_id` 时用它新建一个身份。
    display_name: Option<String>,
    familiarity: Option<i64>,
    trust: Option<i64>,
    closeness: Option<i64>,
    note: Option<String>,
    /// 场景键 → 称呼。空串表示「按渠道数据自动推导」。
    scenes: Option<std::collections::HashMap<String, String>>,
}

/// 新建关系。
///
/// 两种入口：挂在已有身份上（面板上点选一个人），或者顺手建一个新身份。
/// 关系记录本身没有「新建」之外的形态——同一对人只有一条。
async fn api_create_relation(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Json(b): Json<NewRelation>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;

    let scenes = b.scenes.unwrap_or_default();
    check_scenes(&scenes)?;
    let existing = b
        .person_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let display_name = b
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if existing.is_none() && display_name.is_none() {
        return Err(Error::BadRequest(
            "新建关系要指定已有身份，或者填一个显示名".into(),
        ));
    }
    if let Some(n) = &display_name {
        if n.chars().count() > 32 {
            return Err(Error::BadRequest("显示名最长 32 字".into()));
        }
    }
    let (fam, tru, clo) = (
        score(b.familiarity.unwrap_or(0)),
        score(b.trust.unwrap_or(0)),
        score(b.closeness.unwrap_or(0)),
    );
    let note = b
        .note
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let db = st.app.db.clone();
    let person_id = tokio::task::spawn_blocking(move || -> Result<String> {
        db.with_write(|tx| {
            let now = crate::db::now_ms();
            let pid = match existing {
                Some(id) => {
                    let n: i64 = tx.query_row(
                        "SELECT COUNT(*) FROM person WHERE person_id = ?1",
                        rusqlite::params![id],
                        |r| r.get(0),
                    )?;
                    if n == 0 {
                        return Err(Error::NotFound(format!("身份 {id}")));
                    }
                    let has: i64 = tx.query_row(
                        "SELECT COUNT(*) FROM relation WHERE person_id = ?1",
                        rusqlite::params![id],
                        |r| r.get(0),
                    )?;
                    if has > 0 {
                        return Err(Error::BadRequest(format!(
                            "{id} 已经有关系记录，直接编辑它"
                        )));
                    }
                    id
                }
                None => {
                    let name =
                        display_name.expect("上面已校验：没有 person_id 就必须有 display_name");
                    let pid = new_person_id(tx, &name)?;
                    // 人主动建的关系算确认过的身份；pending 的含义是
                    // 「这个账号还没跟别的渠道对上」，与这里不是一回事。
                    tx.execute(
                        "INSERT INTO person(person_id, display_name, status, created_ms)
                         VALUES (?1, ?2, 'confirmed', ?3)",
                        rusqlite::params![pid, name, now],
                    )?;
                    pid
                }
            };
            tx.execute(
                "INSERT INTO relation(person_id, owner_id, familiarity, trust, closeness,
                                      note, first_met_ms, last_met_ms)
                 VALUES (?1, 'me', ?2, ?3, ?4, ?5, ?6, ?6)",
                rusqlite::params![pid, fam, tru, clo, note, now],
            )?;
            write_scenes(tx, &pid, &scenes, now)?;
            Ok(pid)
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("新建关系失败：{e}")))??;

    Ok(Json(
        serde_json::json!({ "ok": true, "person_id": person_id }),
    ))
}

#[derive(Deserialize)]
struct RelationPatch {
    /// 改的是这个人的显示名（与「身份」页同一份数据）。
    display_name: Option<String>,
    familiarity: Option<i64>,
    trust: Option<i64>,
    closeness: Option<i64>,
    /// `Some("")` = 清空；没有这个字段 = 不动。
    note: Option<String>,
    /// 对他的**印象**（主观判断）。同样三态：没传=不动 / 空串=清空 / 有值=覆盖。
    ///
    /// 面板上能改它是必要的：模型生成的印象未必准，而它是会影响回复语气的 ——
    /// 人必须能纠正「它误会我了」。
    impression: Option<String>,
    scenes: Option<std::collections::HashMap<String, String>>,
}

/// 改关系。只改传上来的字段，没传的保持原样。
async fn api_patch_relation(
    State(st): State<AdminState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(b): Json<RelationPatch>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    if let Some(scenes) = &b.scenes {
        check_scenes(scenes)?;
    }
    if let Some(n) = b.display_name.as_deref() {
        if n.trim().is_empty() {
            return Err(Error::BadRequest("显示名不能为空".into()));
        }
        if n.trim().chars().count() > 32 {
            return Err(Error::BadRequest("显示名最长 32 字".into()));
        }
    }

    let db = st.app.db.clone();
    let id2 = id.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            let n: i64 = tx.query_row(
                "SELECT COUNT(*) FROM relation WHERE person_id = ?1",
                rusqlite::params![id2],
                |r| r.get(0),
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("关系 {id2}")));
            }
            let now = crate::db::now_ms();
            if let Some(name) = b.display_name.as_deref() {
                tx.execute(
                    "UPDATE person SET display_name = ?1 WHERE person_id = ?2",
                    rusqlite::params![name.trim(), id2],
                )?;
            }
            // note 是三态：没传 = 不动；传空串 = 清空；传值 = 覆盖。
            let note_set = b.note.is_some();
            let note_val = b.note.as_deref().map(str::trim).filter(|s| !s.is_empty());
            let imp_set = b.impression.is_some();
            let imp_val = b
                .impression
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            // 手工改的印象要查长度：它会每轮进上下文，长段落是长期负担
            if let Some(v) = imp_val {
                if v.chars().count() > 120 {
                    return Err(Error::BadRequest("印象最长 120 字".into()));
                }
            }
            tx.execute(
                "UPDATE relation SET
                   familiarity = COALESCE(?1, familiarity),
                   trust       = COALESCE(?2, trust),
                   closeness   = COALESCE(?3, closeness),
                   note        = CASE WHEN ?4 THEN ?5 ELSE note END,
                   impression  = CASE WHEN ?6 THEN ?7 ELSE impression END,
                   impression_ms = CASE WHEN ?6 AND ?7 IS NOT NULL THEN ?8 ELSE impression_ms END
                 WHERE person_id = ?9",
                rusqlite::params![
                    b.familiarity.map(score),
                    b.trust.map(score),
                    b.closeness.map(score),
                    note_set,
                    note_val,
                    imp_set,
                    imp_val,
                    now,
                    id2
                ],
            )?;
            if let Some(scenes) = &b.scenes {
                write_scenes(tx, &id2, scenes, now)?;
            }
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("编辑关系失败：{e}")))??;

    Ok(Json(serde_json::json!({ "ok": true, "person_id": id })))
}

async fn api_personas(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<Vec<query::PersonaItem>> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let items = tokio::task::spawn_blocking(move || query::personas(&db))
        .await
        .map_err(|e| Error::Storage(format!("人格查询失败：{e}")))??;
    Ok(Json(items))
}

#[derive(Deserialize)]
struct NewPersona {
    name: String,
    #[serde(default)]
    model_regex: String,
    body: String,
    #[serde(default)]
    priority: i64,
    #[serde(default = "yes")]
    enabled: bool,
    note: Option<String>,
}

fn yes() -> bool {
    true
}

/// 校验正则能编译。存进去一条写坏的正则，等于这条人格永远不生效 ——
/// 而面板上看起来一切正常，这是最难查的一类配置错。
fn check_regex(r: &str) -> Result<()> {
    let t = r.trim();
    if t.is_empty() {
        return Ok(()); // 空 = 兜底，合法
    }
    regex::Regex::new(t).map_err(|e| Error::BadRequest(format!("正则不合法：{e}")))?;
    Ok(())
}

async fn api_create_persona(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Json(b): Json<NewPersona>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let name = b.name.trim().to_string();
    if name.is_empty() {
        return Err(Error::BadRequest("名字不能为空".into()));
    }
    if b.body.trim().is_empty() {
        return Err(Error::BadRequest("人格正文不能为空".into()));
    }
    check_regex(&b.model_regex)?;

    // id 必须带随机后缀：只靠毫秒的话，同一毫秒里建两条就撞主键
    // （面板上连点两次「新增」，或者脚本批量导入时很容易发生）。
    let id = format!("per-{}-{}", crate::db::now_ms(), crate::http::rand_suffix());
    let db = st.app.db.clone();
    let id2 = id.clone();
    let body = b.body.clone();
    let name2 = name.clone();
    let re = b.model_regex.trim().to_string();
    let note = b.note.clone();
    let prio = b.priority;
    let enabled = b.enabled;
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            let now = crate::db::now_ms();
            tx.execute(
                "INSERT INTO persona(id, name, model_regex, body, priority, enabled,
                                     note, created_ms, updated_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8)",
                rusqlite::params![id2, name2, re, body, prio, enabled as i64, note, now],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("新建人格失败：{e}")))??;
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

#[derive(Deserialize)]
struct PersonaPatch {
    name: Option<String>,
    model_regex: Option<String>,
    body: Option<String>,
    priority: Option<i64>,
    enabled: Option<bool>,
    note: Option<String>,
}

async fn api_patch_persona(
    State(st): State<AdminState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(b): Json<PersonaPatch>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    if let Some(r) = &b.model_regex {
        check_regex(r)?;
    }
    if let Some(v) = &b.body {
        if v.trim().is_empty() {
            return Err(Error::BadRequest("人格正文不能为空".into()));
        }
    }
    let db = st.app.db.clone();
    let id2 = id.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            let n: i64 = tx.query_row(
                "SELECT COUNT(*) FROM persona WHERE id = ?1",
                rusqlite::params![id2],
                |r| r.get(0),
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("人格 {id2}")));
            }
            tx.execute(
                "UPDATE persona SET
                   name = COALESCE(?1, name),
                   model_regex = COALESCE(?2, model_regex),
                   body = COALESCE(?3, body),
                   priority = COALESCE(?4, priority),
                   enabled = COALESCE(?5, enabled),
                   note = COALESCE(?6, note),
                   updated_ms = ?7
                 WHERE id = ?8",
                rusqlite::params![
                    b.name,
                    b.model_regex.map(|s| s.trim().to_string()),
                    b.body,
                    b.priority,
                    b.enabled.map(|x| x as i64),
                    b.note,
                    crate::db::now_ms(),
                    id2
                ],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("编辑人格失败：{e}")))??;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn api_delete_persona(
    State(st): State<AdminState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            let n = tx.execute("DELETE FROM persona WHERE id = ?1", rusqlite::params![id])?;
            if n == 0 {
                return Err(Error::NotFound(format!("人格 {id}")));
            }
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("删除人格失败：{e}")))??;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct PersonaPreview {
    model: Option<String>,
    /// 只测**这一条正则**（面板编辑时输入框里的内容），不管它存没存库。
    ///
    /// 为什么需要：原来的「试匹配」只会测**库里已有的人格**。
    /// 编辑一条新正则时点它，看到的是「旧正则能匹配什么」——
    /// 用户 2026-11-02 就是这么困惑的：填了
    /// `.*?(dfmodel|deepseek-v4.1-flash)`、测 `trae/deepseek-v4.1-flash`，
    /// 后端其实匹配成功了，但面板上测的是别的条目，看起来像「匹配不到」。
    regex: Option<String>,
}

/// 给定模型 id，返回**会命中哪一条**人格。面板上填完正则能立刻验。
///
/// 传了 `regex` 就只测这一条（含空串 = 测兜底），不查库。
async fn api_persona_preview(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(q): Query<PersonaPreview>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let model = q.model.unwrap_or_default();

    // 只测给定的这一条正则（面板输入框里的），不查库
    if let Some(re) = q.regex {
        let re = re.trim().to_string();
        if re.is_empty() {
            // 空 = 兜底：按定义永远命中（优先级最低，但确实命中）
            return Ok(Json(serde_json::json!({
                "model": model,
                "matched": { "name": "（兜底）", "model_regex": "", "priority": 0,
                             "fallback": true },
                "body": null,
            })));
        }
        let compiled =
            regex::Regex::new(&re).map_err(|e| Error::BadRequest(format!("正则写错了：{e}")))?;
        let hit = compiled.is_match(&model);
        return Ok(Json(serde_json::json!({
            "model": model,
            "regex": re,
            "tested_only": true,
            "matched": if hit {
                serde_json::json!({ "name": "（这条正则）", "model_regex": re,
                                    "priority": 0, "regex_only": true })
            } else {
                serde_json::Value::Null
            },
        })));
    }

    let db = st.app.db.clone();
    let m2 = model.clone();
    let hit = tokio::task::spawn_blocking(move || query::pick_persona(&db, &m2))
        .await
        .map_err(|e| Error::Storage(format!("匹配失败：{e}")))??;
    Ok(Json(serde_json::json!({
        "model": model,
        "matched": hit.as_ref().map(|p| serde_json::json!({
            "id": p.id, "name": p.name, "model_regex": p.model_regex, "priority": p.priority,
        })),
        "body": hit.map(|p| p.body),
    })))
}

async fn api_channels(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<Vec<query::ChannelItem>> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let items = tokio::task::spawn_blocking(move || query::channels(&db))
        .await
        .map_err(|e| Error::Storage(format!("渠道查询失败：{e}")))??;
    Ok(Json(items))
}

#[derive(Deserialize)]
struct NewChannel {
    tag: String,
    label: Option<String>,
    trust: Option<f64>,
}

/// 规范化渠道 tag：小写、去空白，只允许 `[a-z0-9_-]`。
///
/// 允许的字符集是刻意窄的：tag 会出现在 URL、SQL 参数和面板的筛选按钮上，
/// 放开中文/空格会让每个使用点都要额外转义一次。
fn norm_tag(raw: &str) -> Result<String> {
    let t = raw.trim().to_ascii_lowercase();
    if t.is_empty() {
        return Err(Error::BadRequest("tag 不能为空".into()));
    }
    if t.len() > 32 {
        return Err(Error::BadRequest("tag 最长 32 个字符".into()));
    }
    if !t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(Error::BadRequest(
            "tag 只能用小写字母、数字、下划线和连字符".into(),
        ));
    }
    Ok(t)
}

async fn api_create_channel(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Json(b): Json<NewChannel>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let tag = norm_tag(&b.tag)?;
    let label = b
        .label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&tag)
        .to_string();
    let trust = b.trust.unwrap_or(0.8);
    if !(0.0..=1.0).contains(&trust) {
        return Err(Error::BadRequest("trust 必须在 [0,1]".into()));
    }
    let db = st.app.db.clone();
    let tag2 = tag.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            let exists: i64 = tx.query_row(
                "SELECT COUNT(*) FROM channel WHERE tag = ?1",
                rusqlite::params![tag2],
                |r| r.get(0),
            )?;
            if exists > 0 {
                return Err(Error::BadRequest(format!("渠道 {tag2} 已存在")));
            }
            tx.execute(
                "INSERT INTO channel(tag, label, trust, builtin, created_ms)
                 VALUES (?1, ?2, ?3, 0, ?4)",
                rusqlite::params![tag2, label, trust, crate::db::now_ms()],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("新建渠道失败：{e}")))??;
    Ok(Json(serde_json::json!({ "ok": true, "tag": tag })))
}

#[derive(Deserialize)]
struct ChannelPatch {
    label: Option<String>,
    trust: Option<f64>,
}

async fn api_patch_channel(
    State(st): State<AdminState>,
    headers: HeaderMap,
    axum::extract::Path(tag): axum::extract::Path<String>,
    Json(b): Json<ChannelPatch>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    if let Some(t) = b.trust {
        if !(0.0..=1.0).contains(&t) {
            return Err(Error::BadRequest("trust 必须在 [0,1]".into()));
        }
    }
    let db = st.app.db.clone();
    let tag2 = tag.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            let n: i64 = tx.query_row(
                "SELECT COUNT(*) FROM channel WHERE tag = ?1",
                rusqlite::params![tag2],
                |r| r.get(0),
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("渠道 {tag2}")));
            }
            tx.execute(
                "UPDATE channel SET
                   label = COALESCE(?1, label),
                   trust = COALESCE(?2, trust)
                 WHERE tag = ?3",
                rusqlite::params![b.label, b.trust, tag2],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("编辑渠道失败：{e}")))??;
    Ok(Json(serde_json::json!({ "ok": true, "tag": tag })))
}

async fn api_delete_channel(
    State(st): State<AdminState>,
    headers: HeaderMap,
    axum::extract::Path(tag): axum::extract::Path<String>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            let builtin: i64 = tx.query_row(
                "SELECT COALESCE(MAX(builtin), 0) FROM channel WHERE tag = ?1",
                rusqlite::params![tag],
                |r| r.get(0),
            )?;
            if builtin == 1 {
                return Err(Error::BadRequest(
                    "内置渠道不能删除（历史数据全靠这四个 tag）".into(),
                ));
            }
            // 有数据就不让删：重建一个同 tag 的渠道很容易，但记忆里的
            // 「这条来自哪个渠道」一旦指向不存在的 tag，面板上就只剩一个
            // 谁也认不出的字符串。先让人把数据改掉再删。
            // 只算**还有效**的事件：被逻辑删除的不算「在用」，
            // 否则删过一次的渠道就永远删不掉了。
            let events: i64 = tx.query_row(
                "SELECT COUNT(*) FROM event WHERE channel = ?1 AND deleted_by IS NULL",
                rusqlite::params![tag],
                |r| r.get(0),
            )?;
            if events > 0 {
                return Err(Error::BadRequest(format!(
                    "渠道 {tag} 还有 {events} 条事件在用，先改掉或删掉它们"
                )));
            }
            tx.execute("DELETE FROM channel WHERE tag = ?1", rusqlite::params![tag])?;
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("删除渠道失败：{e}")))??;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn api_traits(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(_q): Query<CompanionQ>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let items = tokio::task::spawn_blocking(move || query::traits(&db))
        .await
        .map_err(|e| Error::Storage(format!("特质查询失败：{e}")))??;
    Ok(Json(serde_json::json!({ "items": items })))
}

async fn api_promises(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(_q): Query<CompanionQ>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let items = tokio::task::spawn_blocking(move || query::promises(&db, None))
        .await
        .map_err(|e| Error::Storage(format!("承诺查询失败：{e}")))??;
    Ok(Json(serde_json::json!({ "items": items })))
}

#[derive(Deserialize)]
struct SnapshotBody {
    note: Option<String>,
}

/// 后台拍快照。**用会话鉴权**，不要求 API token。
async fn api_take_snapshot(
    State(st): State<AdminState>,
    headers: HeaderMap,
    body: Option<Json<SnapshotBody>>,
) -> ApiResult<crate::snapshot::SnapshotInfo> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let store = st.app.snapshots.clone();
    let note = body.and_then(|b| b.note.clone());
    let info = tokio::task::spawn_blocking(move || store.take(&db, note.as_deref()))
        .await
        .map_err(|e| Error::Storage(format!("快照任务失败：{e}")))??;
    Ok(Json(info))
}

#[derive(Deserialize)]
struct PersonPatch {
    display_name: Option<String>,
    status: Option<String>,
    note: Option<String>,
    /// 忽视：true = 这个人的消息不回复、不入库。
    ignored: Option<bool>,
    /// 各场景里的称呼（mc/qq/qq_group/live）。空串 = 删掉人工值、回落自动推导。
    /// 场景称呼的编辑入口在这里（身份），关系页只读展示。
    scenes: Option<std::collections::HashMap<String, String>>,
}

/// 改身份。可以改显示名、状态、备注。
async fn api_patch_person(
    State(st): State<AdminState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(p): Json<PersonPatch>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    if let Some(n) = &p.display_name {
        if n.trim().is_empty() {
            return Err(Error::BadRequest("显示名不能为空".into()));
        }
    }
    if let Some(scenes) = &p.scenes {
        check_scenes(scenes)?;
    }
    let db = st.app.db.clone();
    let id2 = id.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            let n: i64 = tx.query_row(
                "SELECT COUNT(*) FROM person WHERE person_id = ?1",
                rusqlite::params![id2],
                |r| r.get(0),
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("身份 {id2}")));
            }
            tx.execute(
                "UPDATE person SET
                   display_name = COALESCE(?1, display_name),
                   status       = COALESCE(?2, status),
                   note         = COALESCE(?3, note),
                   ignored      = COALESCE(?5, ignored)
                 WHERE person_id = ?4",
                rusqlite::params![
                    p.display_name,
                    p.status,
                    p.note,
                    id2,
                    p.ignored.map(|b| b as i64)
                ],
            )?;
            if let Some(scenes) = &p.scenes {
                write_scenes(tx, &id2, scenes, crate::db::now_ms())?;
            }
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("编辑任务失败：{e}")))??;

    Ok(Json(serde_json::json!({ "ok": true, "person_id": id })))
}

#[derive(Deserialize)]
struct BindAccount {
    channel: String,
    account: String,
    seen_name: Option<String>,
}

/// 给一个人绑一个渠道账号。
async fn api_bind_account(
    State(st): State<AdminState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(b): Json<BindAccount>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let ch = crate::model::Channel::parse(&b.channel)
        .ok_or_else(|| Error::BadRequest(format!("未知渠道：{}", b.channel)))?;
    if b.account.trim().is_empty() {
        return Err(Error::BadRequest("account 不能为空".into()));
    }
    let db = st.app.db.clone();
    let now = crate::db::now_ms();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            let n: i64 = tx.query_row(
                "SELECT COUNT(*) FROM person WHERE person_id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("身份 {id}")));
            }
            tx.execute(
                "INSERT INTO person_account(channel, account, person_id, seen_name,
                                            first_seen_ms, last_seen_ms)
                 VALUES (?1,?2,?3,?4,?5,?5)
                 ON CONFLICT(channel, account) DO UPDATE SET
                   person_id = excluded.person_id,
                   seen_name = COALESCE(excluded.seen_name, person_account.seen_name),
                   last_seen_ms = excluded.last_seen_ms",
                rusqlite::params![ch.as_str(), b.account, id, b.seen_name, now],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("绑定任务失败：{e}")))??;

    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn api_snapshots(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<Vec<crate::snapshot::SnapshotInfo>> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let store = st.app.snapshots.clone();
    let r = tokio::task::spawn_blocking(move || store.list(&db, 200))
        .await
        .map_err(|e| Error::Storage(format!("快照查询失败：{e}")))??;
    Ok(Json(r))
}

async fn api_rollbacks(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<Vec<crate::rollback::RollbackRecord>> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let r = tokio::task::spawn_blocking(move || crate::rollback::history(&db, 100))
        .await
        .map_err(|e| Error::Storage(format!("回滚查询失败：{e}")))??;
    Ok(Json(r))
}

// ================================================================== 知识库
//
// 面板用的管理接口。全部走会话鉴权（`require_session`），
// 与 `/v1/kb/*` 的 token 鉴权是两条互不替代的链。
//
// 这一组比 `/v1/kb/*` 多的是**运维视角**：
//   - 每个块嵌入了没有、失败原因是什么（否则用户只知道「检索不准」但查不出为什么）
//   - 删除文档
//   - 重建向量（换了模型/维度之后必须能一键重来）

#[derive(Debug, Serialize)]
struct KbSummary {
    id: String,
    name: String,
    description: Option<String>,
    created_ms: i64,
    updated_ms: i64,
    docs: usize,
    chunks: usize,
    embedded: usize,
    pending: usize,
    failed: usize,
    /// 文档明细。面板要能删除某篇、看到某篇的块数 ——
    /// 只给「1 篇文档」这个数字没法管理。
    docs_detail: Vec<serde_json::Value>,
}

async fn api_kb_list(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let v = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        db.with_read(|c| {
            let kbs = crate::kb::list_kbs(c)?;
            let mut out = Vec::with_capacity(kbs.len());
            for k in kbs {
                let docs = crate::kb::list_docs(c, &k.id)?;
                // 按库统计各嵌入状态
                let mut stmt = c.prepare(
                    "SELECT ch.embed_state, COUNT(*) FROM kb_chunk ch
                     JOIN kb_doc d ON d.id = ch.doc_id
                     WHERE ch.kb_id = ?1 AND d.deleted_by IS NULL
                     GROUP BY ch.embed_state",
                )?;
                let mut counts = std::collections::HashMap::new();
                let rows = stmt.query_map(rusqlite::params![&k.id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?;
                for row in rows {
                    let (s, n) = row?;
                    counts.insert(s, n);
                }
                let embedded = counts.get("done").copied().unwrap_or(0);
                let pending = counts.get("pending").copied().unwrap_or(0);
                let failed = counts.get("failed").copied().unwrap_or(0);
                let docs_detail: Vec<serde_json::Value> = docs
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "id": d.id,
                            "title": d.title,
                            "source": d.source,
                            "n_chunks": d.n_chunks,
                            "created_ms": d.created_ms,
                        })
                    })
                    .collect();
                out.push(KbSummary {
                    id: k.id,
                    name: k.name,
                    description: k.description,
                    created_ms: k.created_ms,
                    updated_ms: k.updated_ms,
                    docs: docs.len(),
                    chunks: (embedded + pending + failed) as usize,
                    embedded: embedded as usize,
                    pending: pending as usize,
                    failed: failed as usize,
                    docs_detail,
                });
            }
            Ok(serde_json::json!({ "kbs": out }))
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct KbCreateReq {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
}

async fn api_kb_create(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Json(b): Json<KbCreateReq>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let now = crate::db::now_ms();
    let kb = tokio::task::spawn_blocking(move || -> Result<crate::kb::Kb> {
        db.with_write(|c| crate::kb::create_kb(c, &b.id, &b.name, b.description.as_deref(), now))
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!({ "kb": kb })))
}

async fn api_kb_health(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let cfg = st.app.cfg.clone();
    let has_embedder = st.app.embedder.is_some();
    let v = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        db.with_read(|c| {
            let kbs: i64 = c.query_row("SELECT COUNT(*) FROM kb", [], |r| r.get(0))?;
            let docs: i64 = c.query_row(
                "SELECT COUNT(*) FROM kb_doc WHERE deleted_by IS NULL",
                [],
                |r| r.get(0),
            )?;
            let chunks: i64 = c.query_row("SELECT COUNT(*) FROM kb_chunk", [], |r| r.get(0))?;
            let (pending, done, failed, skipped) = crate::kb::embed_counts(c)?;
            // 最近一条失败原因：只说「有几个失败」而不给原因，等于没说
            let last_err: Option<String> = c
                .query_row(
                    "SELECT embed_err FROM kb_chunk
                     WHERE embed_state='failed' AND embed_err IS NOT NULL
                     ORDER BY created_ms DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .ok();
            let state = if !cfg.kb.enable {
                "disabled"
            } else if !has_embedder {
                "unavailable"
            } else if failed > 0 {
                "degraded"
            } else {
                "ok"
            };
            Ok(serde_json::json!({
                "enabled": cfg.kb.enable,
                "kb_count": kbs,
                "doc_count": docs,
                "chunk_count": chunks,
                "embed": {
                    "state": state,
                    "model": cfg.kb.embed.model,
                    "dim": cfg.kb.embed.dim,
                    "url": cfg.kb.embed.url,
                    "pending": pending, "done": done,
                    "failed": failed, "skipped": skipped,
                    "last_error": last_err,
                }
            }))
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct KbImportReq {
    kb_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    source: Option<String>,
    content: String,
}

async fn api_kb_import(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Json(b): Json<KbImportReq>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let opts = crate::kb::chunk::Opts {
        max_chars: st.app.cfg.kb.chunk_max_chars,
    };
    let now = crate::db::now_ms();
    let r = tokio::task::spawn_blocking(move || -> Result<crate::kb::ImportResult> {
        db.with_write(|c| {
            crate::kb::import_doc(
                c,
                &b.kb_id,
                b.title.as_deref(),
                b.source.as_deref(),
                &b.content,
                &opts,
                now,
            )
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;

    if r.n_chunks > 0 && !r.duplicate {
        crate::kb::backfill::nudge();
    }
    Ok(Json(serde_json::json!({
        "doc_id": r.doc_id, "n_chunks": r.n_chunks, "duplicate": r.duplicate,
    })))
}

/// 删除文档（逻辑删除）。它的块随之不再进检索候选。
async fn api_kb_delete_doc(
    State(st): State<AdminState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let now = crate::db::now_ms();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|c| {
            let n = c.execute(
                "UPDATE kb_doc SET deleted_by = ?1, updated_ms = ?2
                 WHERE id = ?3 AND deleted_by IS NULL",
                rusqlite::params![now, now, id],
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("文档 {id}")));
            }
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 单个块的详情：正文、向量状态、失败原因。
async fn api_kb_chunk_detail(
    State(st): State<AdminState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let v = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        db.with_read(|c| {
            c.query_row(
                "SELECT ch.id, ch.doc_id, ch.ord, ch.heading, ch.text,
                        ch.embed_state, ch.embed_tries, ch.embed_err,
                        ch.vec_dim, ch.vec_embedder, LENGTH(ch.vec)
                 FROM kb_chunk ch WHERE ch.id = ?1",
                rusqlite::params![id],
                |r| {
                    Ok(serde_json::json!({
                        "id": r.get::<_, String>(0)?,
                        "doc_id": r.get::<_, String>(1)?,
                        "ord": r.get::<_, i64>(2)?,
                        "heading": r.get::<_, Option<String>>(3)?,
                        "text": r.get::<_, String>(4)?,
                        "embed_state": r.get::<_, String>(5)?,
                        "embed_tries": r.get::<_, i64>(6)?,
                        "embed_err": r.get::<_, Option<String>>(7)?,
                        "vec_dim": r.get::<_, Option<i64>>(8)?,
                        "vec_embedder": r.get::<_, Option<String>>(9)?,
                        "vec_bytes": r.get::<_, Option<i64>>(10)?,
                    }))
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Error::NotFound(format!("块 {id}")),
                other => other.into(),
            })
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct KbSearchParams {
    q: Option<String>,
    kb_id: Option<String>,
    #[serde(default)]
    topk: Option<usize>,
}

/// 面板内检索。**和线上检索走同一套融合逻辑**，只是用会话鉴权。
///
/// 为什么不复用 `/v1/kb/search`：那条要 API token，而面板的 cookie 不能调它
/// （两条鉴权链刻意互不替代）。逻辑本身是同一份 `kb::rank::fuse`。
async fn api_kb_search(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(p): Query<KbSearchParams>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let q = p.q.unwrap_or_default().trim().to_string();
    if q.is_empty() {
        return Ok(Json(
            serde_json::json!({ "hits": [], "degraded": "empty_query" }),
        ));
    }
    let topk = p.topk.unwrap_or(5).clamp(1, 20);

    let db = st.app.db.clone();
    let kb_id = p.kb_id.clone();
    let kb_id2 = kb_id.clone();
    let cands = tokio::task::spawn_blocking(move || -> Result<Vec<crate::kb::rank::Candidate>> {
        db.with_read(|c| crate::kb::load_candidates(c, kb_id2.as_deref()))
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;

    let mut degraded: Option<&str> = None;
    let mut last_error: Option<String> = None;
    let mut vec_sims: Option<std::collections::HashMap<String, f64>> = None;

    if let Some(emb) = st.app.embedder.as_ref() {
        match emb.embed(std::slice::from_ref(&q)).await {
            Ok(v) if !v.is_empty() => {
                let qv = v[0].clone();
                let db = st.app.db.clone();
                let model = emb.model().to_string();
                let dim = emb.dim();
                let kb_id3 = kb_id.clone();
                let vecs =
                    tokio::task::spawn_blocking(move || -> Result<Vec<(String, Vec<f32>)>> {
                        db.with_read(|c| crate::kb::load_vectors(c, kb_id3.as_deref(), &model, dim))
                    })
                    .await
                    .map_err(|e| Error::Storage(e.to_string()))??;
                if vecs.is_empty() {
                    degraded = Some("no_vectors_yet");
                } else {
                    vec_sims = Some(
                        vecs.into_iter()
                            .map(|(id, v)| (id, crate::kb::cosine(&qv, &v)))
                            .collect(),
                    );
                }
            }
            Ok(_) => degraded = Some("empty_embedding"),
            // 记下**具体错误**而不只是一个 "embed_failed" 标签。
            // 没有它，面板上只能看到「降级了」，查不出是超时、DNS、还是 4xx ——
            // 这次就是靠它定位的。
            Err(e) => {
                tracing::warn!("面板检索嵌入失败：{e}");
                degraded = Some("embed_failed");
                last_error = Some(format!("{e}"));
            }
        }
    } else if st.app.cfg.kb.enable {
        degraded = Some("embed_unavailable");
    }

    let opts = st.app.cfg.kb.rank.clone();
    let hits = crate::kb::rank::fuse(&cands, &q, vec_sims.as_ref(), topk, &opts);
    Ok(Json(serde_json::json!({
        "hits": hits,
        "degraded": degraded,
        "embed_error": last_error,
        "pool_size": cands.len(),
    })))
}

/// 把某个库（或全部）的块重置成 `pending`，让回填重新嵌入。
///
/// **换模型或换维度之后必须用这个**：旧向量与新的不匹配，会被 `load_vectors`
/// 全部剔除，表现为「检索突然只能字面命中」而没有任何报错。
async fn api_kb_reembed(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(p): Query<KbReembedParams>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    if st.app.embedder.is_none() {
        return Err(Error::BadRequest(
            "嵌入服务未启用，无法重建向量（先在启动配置里开启知识库）".into(),
        ));
    }
    let db = st.app.db.clone();
    let kb_id = p.kb_id.clone();
    let n = tokio::task::spawn_blocking(move || -> Result<usize> {
        db.with_write(|c| {
            let n = match kb_id.as_deref() {
                Some(id) => c.execute(
                    "UPDATE kb_chunk SET embed_state='pending', embed_tries=0, embed_err=NULL,
                                         vec=NULL, vec_dim=NULL, vec_embedder=NULL
                     WHERE kb_id = ?1",
                    rusqlite::params![id],
                )?,
                None => c.execute(
                    "UPDATE kb_chunk SET embed_state='pending', embed_tries=0, embed_err=NULL,
                                         vec=NULL, vec_dim=NULL, vec_embedder=NULL",
                    [],
                )?,
            };
            Ok(n)
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    crate::kb::backfill::nudge();
    Ok(Json(
        serde_json::json!({ "reset": n, "note": "已重置，后台正在重新嵌入" }),
    ))
}

#[derive(Deserialize)]
struct KbReembedParams {
    #[serde(default)]
    kb_id: Option<String>,
}

/// 取一篇文档的完整内容与块明细。面板的「查看/编辑」用它。
async fn api_kb_get_doc(
    State(st): State<AdminState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let db = st.app.db.clone();
    let d = tokio::task::spawn_blocking(move || -> Result<crate::kb::DocDetail> {
        db.with_read(|c| crate::kb::get_doc(c, &id))
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(
        serde_json::to_value(d).map_err(|e| Error::Storage(format!("序列化失败：{e}")))?,
    ))
}

#[derive(Deserialize)]
struct KbPatchDocReq {
    /// `None` = 不改；`Some(None)` = 清空标题。
    #[serde(default, deserialize_with = "double_option")]
    title: Option<Option<String>>,
    /// `None` = 不改原文。改了原文会重新切块并重新嵌入。
    #[serde(default)]
    content: Option<String>,
}

/// 区分「没传这个字段」和「传了 null」。
///
/// `Option<Option<T>>` 默认的 serde 行为是把 `null` 和「字段缺失」都变成 `None`，
/// 那样就分不清「不改标题」和「把标题清空」。
fn double_option<'de, D, T>(de: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    let v: Option<T> = Option::deserialize(de)?;
    Ok(Some(v))
}

/// 改文档。只改标题时不动块；改了原文则**重新切块 + 重新嵌入**。
async fn api_kb_patch_doc(
    State(st): State<AdminState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(b): Json<KbPatchDocReq>,
) -> ApiResult<serde_json::Value> {
    require_session(&st, &headers)?;
    let title_owned: Option<Option<String>> = b.title.clone();
    let db = st.app.db.clone();
    let opts = crate::kb::chunk::Opts {
        max_chars: st.app.cfg.kb.chunk_max_chars,
    };
    let now = crate::db::now_ms();
    let (n, actually_rechunked) = tokio::task::spawn_blocking(move || -> Result<(usize, bool)> {
        db.with_write(|c| {
            let title_ref = match &title_owned {
                None => None,
                Some(None) => Some(None),
                Some(Some(t)) => Some(Some(t.as_str())),
            };
            crate::kb::update_doc(c, &id, title_ref, b.content.as_deref(), &opts, now)
        })
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;

    // 只有**真的重切了**才催回填：内容没变时块还是原来那些、可能早就嵌好了，
    // 催一次纯属空转。这里用 `actually_rechunked` 而不是 `changed_content`：
    // 后者只说明「客户端传了内容」，不知道后端是否真重切（哈希没变会跳过）。
    if actually_rechunked {
        crate::kb::backfill::nudge();
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "n_chunks": n,
        "rechunked": actually_rechunked,
        "note": if actually_rechunked {
            "已重新切块，向量在后台重新生成；完成前这些块只能被字面命中"
        } else {
            "已更新"
        },
    })))
}
