//! HTTP 服务。
//!
//! # 吞吐与并发
//!
//! 用户要求「保证吞吐和并发量处于较高水平」。这里的做法是：
//!
//! 1. **读操作走独立的只读连接**（`spawn_blocking`），不与写者抢锁。
//!    WAL 模式下读不阻塞写、写不阻塞读，这一条才能成立。
//! 2. **写操作串行化**。SQLite 只有一个写者，硬凑并发只会变成 `SQLITE_BUSY`。
//!    串行写入 + 短事务，比「并发写入 + 重试」快得多。
//! 3. **`rusqlite` 绝不直接在 async 任务里调用**。它是同步的，一次调用就能
//!    卡住整个 executor。所有数据库操作都包在 `spawn_blocking` 里。
//! 4. **连接级 PRAGMA 只设一次**（见 `db.rs` 的 `tune`），不在每次查询里重复。
//!
//! # 鉴权
//!
//! - `GET /v1/health` 免鉴权（健康检查要能被监控系统无凭据探测）。
//! - 其余一律要求 `X-API-Token` **Header**，不拼 URL——
//!   URL 会进 nginx/CDN 访问日志，token 拼进去等于泄露。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::db::{now_ms, Db};
use crate::error::{Error, Result};
use crate::model::*;
use crate::rollback;
use crate::snapshot::SnapshotStore;
use crate::weight;

/// 共享状态。用 `Arc` 包住，handler 里 clone 一份只是加引用计数。
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub cfg: Arc<Config>,
    pub snapshots: SnapshotStore,
    pub started_ms: i64,
    /// 可在线调整的权重配置。
    ///
    /// 单独用 RwLock 而不是改 `Config`：只有这一块允许热改，
    /// 端口/数据目录那些需要重启，混在一起早晚有人改错。
    /// 改完立刻写进 `data_dir/weights.json`，重启后仍然生效。
    pub weight: Arc<std::sync::RwLock<crate::weight::WeightConfig>>,
    /// 检索索引缓存。见 [`IndexCache`]。
    pub index: Arc<IndexCache>,
    /// 嵌入客户端。`None` = 知识库未启用或未配置嵌入服务。
    ///
    /// **不是硬依赖**：为 `None` 时知识库检索退回纯字面，
    /// 与「Hub 停掉时 MC 照常工作」是同一套降级风格。
    pub embedder: Option<Arc<crate::kb::embed::Embedder>>,
    /// 面板会话校验器。
    ///
    /// 放在这里是为了让**面板里点的写操作**（改事件/删事实…）能复用同一套会话校验：
    /// 那些 handler 挂在 `/v1/*` 上，原来只认 `X-API-Token`，于是人在浏览器里
    /// 改一条记忆还得先去填 token —— 用户明确要求去掉这一步。
    /// 与 `AdminState` 里的是同一个东西（`AdminState::new` 直接取这一份）。
    pub auth: Arc<crate::auth::Auth>,
}

impl AppState {
    pub fn new(db: Arc<Db>, cfg: Arc<Config>) -> Result<Self> {
        let snapshots = SnapshotStore::new(cfg.snapshot_dir())?;
        // 权重优先从 weights.json 读（面板改过的），没有就用配置默认值。
        let weight = load_weights(&cfg).unwrap_or_else(|| cfg.weight.clone());
        // 注意顺序：`build_embedder` 借 `cfg`，而下面要把 `cfg` move 进结构体。
        // 先算出结果再 move，否则借用检查器（正确地）拦住。
        let embedder = build_embedder(&cfg);
        let auth = Arc::new(crate::auth::Auth::with_db(
            &cfg.panel_user,
            &cfg.panel_password,
            Some(db.clone()),
        ));
        Ok(Self {
            db,
            cfg,
            snapshots,
            started_ms: now_ms(),
            weight: Arc::new(std::sync::RwLock::new(weight)),
            index: Arc::new(IndexCache::default()),
            embedder,
            auth,
        })
    }
}

/// 为 `/v1/context` 取知识库那一段。返回 `(渲染好的文本, 命中的块信息)`。
///
/// 两个设计要点：
///
/// **① 有阈值，不是有命中就注入。** 实测无关查询（「今天天气不错」「你好呀」）
/// 也能拿到 `vec≈0.29~0.36`，而相关查询最低 0.408 —— 区分度很薄。
/// 不做门槛的话，每一轮闲聊都会往提示词里塞一段不相关的资料，
/// 那比不注入更糟（会挤掉真正有用的段）。
///
/// 默认门槛 `vec ≥ 0.42 或 lit ≥ 0.30`：实测能滤掉
/// 「今天天气不错 0.351」「你好呀 0.359」「1+1等于几 0.309」这类噪声，
/// 同时保留「会被封号吗 0.554」「我家在哪 0.466」这类纯语义命中。
///
/// **② 失败一律返回空，不报错。** 知识库是增强，不是主链路。
/// 嵌入服务挂掉时 `/v1/context` 必须照常工作 —— 与「Hub 挂掉时 MC 照常」一致。
async fn load_kb_segment(
    st: &AppState,
    query: &str,
) -> (Option<String>, Vec<(Option<String>, String)>) {
    let empty = (None, Vec::new());
    if !st.cfg.kb.enable || query.trim().is_empty() {
        return empty;
    }

    // 候选
    let db = st.db.clone();
    let cands =
        match tokio::task::spawn_blocking(move || -> Result<Vec<crate::kb::rank::Candidate>> {
            db.with_read(|c| crate::kb::load_candidates(c, None))
        })
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::debug!("/v1/context 读知识库候选失败：{e}");
                return empty;
            }
            Err(e) => {
                tracing::debug!("/v1/context 读知识库候选任务失败：{e}");
                return empty;
            }
        };
    if cands.is_empty() {
        return empty;
    }

    // 查询向量（失败就退回纯字面，仍然可能命中）
    let mut vec_sims: Option<std::collections::HashMap<String, f64>> = None;
    if let Some(emb) = st.embedder.as_ref() {
        if let Ok(v) = emb.embed(std::slice::from_ref(&(query.to_string()))).await {
            if let Some(qv) = v.first() {
                let db = st.db.clone();
                let model = emb.model().to_string();
                let dim = emb.dim();
                if let Ok(Ok(vecs)) =
                    tokio::task::spawn_blocking(move || -> Result<Vec<(String, Vec<f32>)>> {
                        db.with_read(|c| crate::kb::load_vectors(c, None, &model, dim))
                    })
                    .await
                {
                    if !vecs.is_empty() {
                        vec_sims = Some(
                            vecs.into_iter()
                                .map(|(id, v)| (id, crate::kb::cosine(qv, &v)))
                                .collect(),
                        );
                    }
                }
            }
        }
    }

    let opts = st.cfg.kb.rank.clone();
    // 多取一条：门槛过滤后可能只剩很少，留点余量再截断
    let hits = crate::kb::rank::fuse(&cands, query, vec_sims.as_ref(), 3, &opts);
    if hits.is_empty() {
        return empty;
    }

    // 门槛：**只看第一名**。第一名都不过关，后面的更不用看。
    let (min_vec, min_lit) = (st.cfg.kb.inject_min_vec, st.cfg.kb.inject_min_lit);
    let top = &hits[0];
    if top.vec < min_vec && top.lit < min_lit {
        tracing::debug!(
            "/v1/context 知识库命中被门槛拦下：vec={:.3} lit={:.3}（门槛 {min_vec}/{min_lit}）",
            top.vec,
            top.lit
        );
        return empty;
    }

    let rows: Vec<(Option<String>, String)> = hits
        .iter()
        .map(|h| (h.heading.clone(), h.text.clone()))
        .collect();
    let texts = rows.clone();
    (crate::context::render_kb(&texts), rows)
}

/// 按配置构造嵌入客户端。
///
/// **失败不阻止启动**：知识库是可选能力，一个嵌入服务没起来不该让整个中枢
/// 起不来（那会让记忆功能也一起挂掉）。失败只打警告，检索走纯字面降级。
///
/// 但有一条例外：**配置本身有错**（比如 `enable=true` 却没给 url）要在
/// `Config::validate()` 里就拒绝启动 —— 那是「用户以为自己配好了」的情形，
/// 静默降级会让人以为功能坏了却查不出原因。
fn build_embedder(cfg: &Config) -> Option<Arc<crate::kb::embed::Embedder>> {
    if !cfg.kb.enable {
        return None;
    }
    match crate::kb::embed::Embedder::new(cfg.kb.embed.clone()) {
        Ok(e) => {
            tracing::info!(
                "知识库已启用：{} | 模型 {} | {} 维 | 批 {}",
                cfg.kb.embed.url,
                cfg.kb.embed.model,
                cfg.kb.embed.dim,
                cfg.kb.embed.batch
            );
            Some(Arc::new(e))
        }
        Err(e) => {
            tracing::warn!("知识库嵌入客户端构造失败，检索将退回纯字面：{e}");
            None
        }
    }
}

/// 权重配置文件路径。
pub fn weights_path(cfg: &Config) -> std::path::PathBuf {
    cfg.data_dir.join("weights.json")
}

/// 读面板改过的权重。没有或坏了就返回 None（用默认值）。
fn load_weights(cfg: &Config) -> Option<crate::weight::WeightConfig> {
    let p = weights_path(cfg);
    let text = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&text).ok()
}

/// 写回权重。失败只记日志：改不了盘不等于改不了内存，
/// 当场生效比「重启后生效」有用得多。
pub fn save_weights(cfg: &Config, w: &crate::weight::WeightConfig) {
    let p = weights_path(cfg);
    match serde_json::to_string_pretty(w) {
        Ok(text) => {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                if let Err(e) = std::fs::rename(&tmp, &p) {
                    tracing::warn!("权重落盘失败：{e}（内存已生效）");
                }
            }
        }
        Err(e) => tracing::warn!("权重序列化失败：{e}"),
    }
}

/// 建路由。测试里也用这个，保证测的就是线上那份。
/// **完整**路由：核心 API + 对接面（legacy）+ 后台面板。
///
/// # 为什么要有这个函数
///
/// 早先三套路由是在 `main.rs` 里手工 merge 的，而测试只用了下面那个
/// `router()`（只有核心 API）。结果是：测试全绿，但 `/v1/fact/list`
/// 这类对接接口在测试环境里根本不存在——真机跑的是另一份组装逻辑，
/// **测的和跑的不是同一个东西**。
///
/// 现在两边都走这里，测不到就等于跑不起来。
pub fn full_router(state: AppState) -> Router {
    router(state.clone())
        // 对接面：给 MC 插件这类既有客户端用的完整 CRUD
        .merge(crate::legacy_api::router(state.clone()))
        .merge(crate::admin::router(crate::admin::AdminState::new(
            state.clone(),
            &state.cfg,
        )))
        // 三套路由各自都没有 fallback，未知路径会落到 axum 的默认空 404。
        // 对调用方来说「空 body 的 404」和「连不上」一样难查，所以统一成 JSON。
        .fallback(|| async {
            (
                axum::http::StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({
                    "error": "没有这个接口",
                    "kind": "not_found",
                    "retryable": false,
                })),
            )
        })
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/event", post(post_event))
        .route("/v1/event/list", get(list_events))
        .route("/v1/fact", post(post_fact))
        .route("/v1/recall", get(recall))
        // 聚合口：一次调用拿到「可直接塞进 system_prompt」的一段文本。
        // 存在理由是修一条静默失效的链路，详见 `crate::context` 的模块注释。
        .route("/v1/context", get(get_context))
        // ---------------------------------------------------------- 知识库
        .route("/v1/kb", post(post_kb))
        .route("/v1/kb/list", get(list_kb))
        .route("/v1/kb/doc", post(post_kb_doc))
        .route("/v1/kb/search", post(search_kb))
        .route("/v1/kb/health", get(kb_health))
        // 规划：原来**没有任何 /v1 出口**（只能 cookie 登后台），
        // 于是「它今天计划完成到哪」这件事 API token 侧根本读不到。
        .route("/v1/plan/list", get(list_plans))
        .route("/v1/plan/{id}", get(get_plan))
        .route("/v1/person/bind", post(bind_person))
        .route("/v1/person/pending", get(pending_persons))
        .route("/v1/name/normalize", post(normalize_names))
        .route("/v1/channel/list", get(list_channels))
        .route("/v1/person/ignored", get(person_ignored_check))
        .route("/v1/snapshot", post(take_snapshot))
        .route("/v1/snapshot/list", get(list_snapshots))
        .route("/v1/rollback", post(do_rollback))
        .route("/v1/rollback/undo", post(do_undo))
        .route("/v1/rollback/history", get(rollback_history))
        .route("/v1/stats", get(stats))
        // 编辑：记忆是可修正的，写错了要能改，不能只能删了重写。
        .route("/v1/event/{id}", axum::routing::patch(patch_event))
        .route("/v1/fact/{id}", axum::routing::patch(patch_fact))
        .route("/v1/journal/{id}", axum::routing::patch(patch_journal))
        // 情绪：PAD 三维 + 中文标签
        .route("/v1/mood", get(get_mood))
        // 删除：逻辑删除（可撤销），复用回滚那一套 deleted_by 机制
        .route("/v1/event/{id}", axum::routing::delete(delete_event))
        .route("/v1/fact/{id}", axum::routing::delete(delete_fact))
        .route("/v1/journal/{id}", axum::routing::delete(delete_journal))
        .route("/v1/diary/{id}", axum::routing::delete(delete_diary))
        // 群消息是三张可编辑表里唯一没有删除口的。
        // 清理误入库的群消息、删错同步都得靠它，否则只能进 SQLite 手改。
        .route("/v1/message/{id}", axum::routing::delete(delete_message))
        // 新增手帐与日记（事件和事实已有 POST）
        .route("/v1/journal", post(post_journal))
        .route("/v1/diary", post(post_diary))
        // 顺序很重要：先鉴权，再让错误归一兜底。
        // 反了的话 401 会被错误归一改写成别的形状。
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            api_auth_mw,
        ))
        .layer(axum::middleware::from_fn(normalize_errors_mw))
        .with_state(state)
}

// ------------------------------------------------------------------ 鉴权

/// 检查 token。集中一处，避免每个 handler 各写一遍（漏一个就是洞）。
///
/// # 没有 token 时的策略
///
/// 服务可能监听在 `0.0.0.0`（操作者要求内网可访问），此时**不能**因为
/// 「没配 token」就对所有人开放写记忆的能力。
///
/// 所以这里要看**请求从哪来**：
/// - 走的是 Unix 域 / 进程内调用 → 视为本机，放行
/// - 有 `X-Forwarded-For` 或连接来自非回环 → 必须带 token
///
/// 实现上由中间件把「是否回环来源」塞进请求扩展，见 `source_mw`。
pub(crate) fn check_token(state: &AppState, headers: &HeaderMap) -> Result<()> {
    // 没有 token 的场合：本机来源由中间件放行到这里，
    // 外部来源根本进不来（见 api_auth_mw）。所以这里直接放行。
    if state.cfg.token.is_empty() {
        return Ok(());
    }
    let got = headers
        .get("x-api-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // 定长比较，避免通过响应时间差逐字节猜 token。
    if constant_time_eq(got.as_bytes(), state.cfg.token.as_bytes()) {
        Ok(())
    } else {
        // 诊断用：只说长度不说内容。排查「客户端没带 token」和「带错了」是两回事，
        // 而 token 本身绝不能进日志。
        let path = headers
            .get("x-debug-path")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        tracing::warn!(
            "401：收到 token 长度={}（期望 {}）{}",
            got.len(),
            state.cfg.token.len(),
            if path.is_empty() {
                String::new()
            } else {
                format!(" path={path}")
            }
        );
        Err(Error::Unauthorized)
    }
}

/// 面板写操作也走 `/v1/*`（同一批 handler），但人在浏览器里没有 API token。
///
/// 口径（用户 2026-10-31 要求）：**面板里改数据不该再让用户去填 token**。
/// 所以这些写口允许两种凭据之一：
///
/// - `X-API-Token`（脚本 / 其他服务）；
/// - 面板会话 cookie（人在浏览器里点出来的请求）。
///
/// 安全性没有降低：两者本身就是两套独立鉴权，这里只是**接受其中任意一个**，
/// 而不是把 cookie 当成 token 用。只允许 cookie 的路径仍限于 `/api/admin/*` 之外
/// 的这一小批数据写口，且会话本身要过 `Auth::verify`。
pub(crate) fn check_token_or_session(state: &AppState, headers: &HeaderMap) -> Result<()> {
    if check_token(state, headers).is_ok() {
        return Ok(());
    }
    // cookie 分支：与 `/api/admin/*` 同一套会话校验（同一份 Auth）。
    let sid = crate::auth::session_from_cookie(
        headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok()),
    );
    if !sid.is_empty() && state.auth.verify(&sid).is_some() {
        return Ok(());
    }
    Err(Error::Unauthorized)
}

/// 校验渠道 tag 是否已注册，返回规范化后的 tag（小写、去空白）。
///
/// 渠道从编译期枚举改成了 `channel` 表里的数据：加一个平台不用改代码，
/// 但要**先注册**再用。这样拼错的 tag（`Qq ` / `qqq`）会当场 400，
/// 而不是悄悄写进库、变成面板上一个谁也认不出的渠道名。
pub(crate) fn check_channel(st: &AppState, tag: &str) -> Result<String> {
    let normalized = tag.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(Error::BadRequest("channel 不能为空".into()));
    }
    if crate::query::channel_exists(&st.db, &normalized)? {
        return Ok(normalized);
    }
    Err(Error::BadRequest(format!(
        "未知渠道：{tag}（可用渠道见 GET /v1/channel/list，面板「渠道」页可新增）"
    )))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ------------------------------------------------------------------ 错误映射

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self)).into_response()
    }
}

/// 统一错误格式。
///
/// **为什么需要这一层**：`Json<T>` 提取器在 handler 之前就跑了，
/// 它失败时 axum 会返回**纯文本**（"Failed to deserialize the JSON body..."），
/// 跟我们自己的 JSON 错误格式不一致。调用方就没法用一套逻辑处理所有失败。
///
/// 实测过这个坑：body 少一个字段时返回的是纯文本，客户端 `await r.json()` 直接抛异常。
/// API 鉴权闸门。
///
/// # 为什么需要它
///
/// 服务现在可以监听 `0.0.0.0`（操作者要求内网可访问）。如果同时又没配 token，
/// 那 `/v1/*` 就成了「局域网里谁都能往记忆库写东西」——
/// 而写记忆意味着能影响它的行为和说辞。
///
/// 所以：**没 token 时，非回环来源一律 401**。
/// 有 token 时不在这里判断（交给各 handler 的 `authorize`，
/// 因为健康检查要免鉴权）。
///
/// # 判定「是不是回环」
///
/// 用 `ConnectInfo` 拿真实对端地址。注意**不能信 `X-Forwarded-For`**——
/// 那是客户端可伪造的，信它等于给所有人一个「我是本机」的后门。
async fn api_auth_mw(
    State(st): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !st.cfg.token.is_empty() {
        return next.run(req).await; // 有 token：交给 handler 逐个判
    }

    let path = req.uri().path();
    // 健康检查免鉴权：监控要能探测，且它只返回计数。
    if path == "/v1/health" {
        return next.run(req).await;
    }

    // 拿对端地址。`ConnectInfo` 要 `into_make_service_with_connect_info` 才注入，
    // 拿不到时保守起见当作「非本机」——宁可误拒，不可误放。
    let loopback = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().is_loopback())
        .unwrap_or(false);

    if loopback {
        next.run(req).await
    } else {
        Error::Unauthorized.into_response()
    }
}

async fn normalize_errors_mw(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let resp = next.run(req).await;
    normalize_error_response(resp).await
}

async fn normalize_error_response(resp: Response) -> Response {
    let status = resp.status();
    if status.is_success() || status.is_redirection() {
        return resp;
    }
    // 已经是我们自己格式的（Content-Type 是 json）就别动。
    if resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("application/json"))
        .unwrap_or(false)
    {
        return resp;
    }

    let kind = match status.as_u16() {
        400 | 413 | 415 | 422 => "bad_request",
        401 => "unauthorized",
        404 => "not_found",
        409 => "rollback",
        503 => "storage",
        _ => "storage",
    };
    let err = crate::error::ErrorKind::parse(kind);

    // 把原始 body 读出来：axum 的提取器报错里带着**哪个字段缺了**，
    // 这个信息对调用方比一个泛化的 "Unprocessable Entity" 有用得多。
    // 截断到 300 字符，并且只在这类错误上带出去。
    let (mut parts, body) = resp.into_parts();
    let raw = axum::body::to_bytes(body, 4096)
        .await
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let detail = raw.trim();
    let message = if detail.is_empty() {
        status.canonical_reason().unwrap_or("请求失败").to_string()
    } else {
        let short: String = detail.chars().take(300).collect();
        format!(
            "{}：{}",
            status.canonical_reason().unwrap_or("请求失败"),
            short
        )
    };

    let body = serde_json::json!({
        "error": message,
        "kind": kind,
        "retryable": err.retryable(),
    });
    let text = body.to_string();

    // 保留原状态码，只换 body 与 content-type。
    parts.headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    Response::from_parts(parts, axum::body::Body::from(text))
}

type ApiResult<T> = std::result::Result<Json<T>, Error>;

// ------------------------------------------------------------------ 健康检查

#[derive(Serialize)]
struct HealthBody {
    ok: bool,
    app: &'static str,
    version: &'static str,
    schema_version: i32,
    uptime_ms: i64,
    /// 监听在哪个地址上。回答「它到底暴露出去了没有」。
    bind: String,
    /// 是否因缺少 token 而降级成回环。
    loopback_only: bool,
    writes: u64,
    dedup_hits: u64,
    wal_bytes: u64,
    tables: Vec<(String, i64)>,
    /// 最近一份快照的时间戳与行数，`null` 表示还没有快照。
    latest_snapshot_ms: Option<i64>,
    /// 被回滚遗忘但尚未清理的行数。
    forgotten_rows: i64,
}

/// 健康检查**免鉴权**：监控系统要能无凭据探测。
/// 因此这里只返回计数与状态，**不返回任何记忆内容**。
async fn health(State(st): State<AppState>) -> ApiResult<HealthBody> {
    let db = st.db.clone();
    let (tables, latest_snap, forgotten) = tokio::task::spawn_blocking(move || {
        let tables = db.counts().unwrap_or_default();
        let latest = db
            .with_read(|c| {
                Ok(c.query_row("SELECT MAX(taken_ms) FROM snapshot", [], |r| {
                    r.get::<_, Option<i64>>(0)
                })?)
            })
            .unwrap_or(None);
        let forgotten = rollback::forgotten_count(&db).unwrap_or(0);
        (tables, latest, forgotten)
    })
    .await
    .map_err(|e| Error::Storage(format!("健康检查任务失败：{e}")))?;

    Ok(Json(HealthBody {
        ok: true,
        app: crate::APP_NAME,
        version: crate::VERSION,
        schema_version: crate::db::SCHEMA_VERSION,
        uptime_ms: now_ms() - st.started_ms,
        bind: st.cfg.effective_bind().to_string(),
        loopback_only: st.cfg.effective_bind().ip().is_loopback(),
        writes: st.db.write_count(),
        dedup_hits: st.db.dedup_hits(),
        wal_bytes: st.db.wal_size(),
        tables,
        latest_snapshot_ms: latest_snap,
        forgotten_rows: forgotten,
    }))
}

// ------------------------------------------------------------------ 写入事件

#[derive(Deserialize)]
struct NewEvent {
    channel: String,
    at_ms: Option<i64>,
    kind: String,
    summary: String,
    /// 调用方给的重要度。`null` / 不传 = 让中枢**自动判定**（推荐）。
    ///
    /// 之前这里默认 0.5、面板上再被写死成 0.3/0.6，于是「被炸死」和
    /// 「今天天气不错」在检索里权重一样。现在不传就自动算。
    #[serde(default)]
    importance: Option<f64>,
    #[serde(default = "default_scope")]
    scope: String,
    person_id: Option<String>,
    /// 幂等键。有它才能安全重放。
    dedup_key: Option<String>,
    /// 仅 MC 渠道：游戏日 `g<N>`。只作标签。
    channel_day: Option<String>,
}

fn half() -> f64 {
    0.5
}

fn default_scope() -> String {
    "companion".into()
}

/// 把「空串」的幂等键归一成 `None`。
///
/// # 为什么这是个必须修的坑
///
/// `event` / `journal` 上有**部分唯一索引** `... ON (dedup_key) WHERE dedup_key IS NOT NULL`。
/// SQLite 里 `""` **不是** NULL，所以第一条第 `dedup_key:""` 的记录写进去之后，
/// 索引里就有了一个 `""`；此后**所有** `dedup_key:""` 的写入都会撞唯一约束 ——
/// 不管 summary 完全不同。
///
/// 实测后果：第一次写入 `duplicate:false`，之后**永久** `duplicate:true`，
/// 而且**零报错**。这是个永久性的静默数据丢失，坏在这个字段看起来只是「可选」。
///
/// 修法：入口处把空串过滤成 `None`。`None` 不参与部分唯一索引，
/// 所以不传幂等键的写入永远不互相吞。
pub fn normalize_key(k: Option<String>) -> Option<String> {
    k.filter(|s| !s.trim().is_empty())
}

#[derive(Serialize)]
struct WriteResult {
    id: Option<i64>,
    /// true = 因为 dedup_key 已存在而被忽略。调用方据此区分「新写入」与「重放」。
    duplicate: bool,
}

/// 事实的 id 是字符串（`f-<ts>-<rand>`），跟事件的整数 id 不是一回事，
/// 所以返回结构也分开——共用一个会逼出一堆 `Some(0)` 这种假值。
#[derive(Serialize)]
struct FactWriteResult {
    id: String,
    /// true = 覆盖了同 id 的旧事实（upsert 命中）。
    replaced: bool,
}

async fn post_event(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<NewEvent>,
) -> ApiResult<WriteResult> {
    check_token_or_session(&st, &headers)?;

    let channel = check_channel(&st, &body.channel)?;
    if body.summary.trim().is_empty() {
        return Err(Error::BadRequest("summary 不能为空".into()));
    }
    // 被忽视的人：整条丢弃。
    // 为什么在写入侧也拦一次（而不只是回复侧）：接入方可能只接了「回复」那一路，
    // 忘了在写记忆前调用。两头都拦，任何一个环节漏了都不会把他的话写进来。
    if let Some(pid) = body.person_id.as_deref().filter(|s| !s.is_empty()) {
        if crate::query::person_ignored(&st.db, pid)? {
            tracing::info!("忽略：person {pid} 被设为忽视，事件未入库");
            return Ok(Json(WriteResult {
                id: None,
                duplicate: false,
            }));
        }
    }
    if let Some(v) = body.importance {
        if !(0.0..=1.0).contains(&v) {
            return Err(Error::BadRequest("importance 必须在 [0,1]".into()));
        }
    }

    // 摘要里的昵称换成身份显示名。
    //
    // 为什么在**入库时**做（而不是只在读的时候）：
    //   · 群名片会改（啥子猫 → 啥子喵），历史记录里留着旧名字，之后就再也关联不上了；
    //   · 换成显示名之后，同一个人的所有事件天然可用一个词检索到。
    // 只按**这条事件自己的渠道**查映射，不做全渠道查找 —— 一个 QQ 群的聊天
    // 里出现 MC 名是很可疑的，不该顺手改掉。
    let summary = if body.summary.trim().is_empty() {
        String::new()
    } else {
        let mappings = crate::query::name_mappings_for_channel(&st.db, &channel)?;
        if mappings.is_empty() {
            body.summary.clone()
        } else {
            let (fixed, hits) = crate::query::replace_names(&body.summary, &mappings);
            if !hits.is_empty() {
                tracing::debug!(
                    "事件摘要归一：{} 处（{}）",
                    hits.len(),
                    hits.iter()
                        .map(|(f, t, _)| format!("{f}→{t}"))
                        .collect::<Vec<_>>()
                        .join("、")
                );
            }
            fixed
        }
    };

    // 重要度：调用方给了就用它的（记 `caller`），没给就自动判定（记 `auto`）。
    //
    // 自动判定用四个维度：事件类型基底、涉及的人与它多亲近、
    // 摘要命中了几条既有事实、有没有提到它自己。理由存进 `importance_reason`，
    // 面板上能直接看到「这 0.78 是怎么来的」，不用事后推算。
    let (importance, importance_source, importance_reason) = match body.importance {
        Some(v) => (v, "caller", None),
        None => {
            let closeness = match body.person_id.as_deref().filter(|s| !s.is_empty()) {
                Some(pid) => crate::query::closeness_of(&st.db, pid)?,
                None => None,
            };
            let fact_hits = crate::query::fact_hits_for(&st.db, &summary)?;
            // 「提到自己」：摘要里出现它在各场景里的任一称呼（≥2 字）。
            let self_names = crate::query::name_mappings(&st.db, None)?;
            let mentions_self = self_names.iter().any(|m| summary.contains(&m.to));
            let input = crate::importance::ImportanceInput {
                kind: body.kind.clone(),
                summary: summary.clone(),
                closeness,
                fact_hits,
                mentions_self,
            };
            let w = crate::importance::ImportanceWeights::default();
            (
                crate::importance::score(&input, &w),
                "auto",
                Some(crate::importance::explain(&input, &w).join("；")),
            )
        }
    };

    let at = body.at_ms.unwrap_or_else(now_ms);
    let scope = Scope::parse(&body.scope);
    let db = st.db.clone();
    // 空串幂等键必须归一成 NULL：见 `normalize_key` 的说明，
    // 否则第一条 `dedup_key:""` 会永久吞掉之后所有同字段事件。
    let dedup = normalize_key(body.dedup_key.clone());
    // 情绪推动用：这两个在 body 被 move 之前取出来。
    let kind_for_mood = body.kind.clone();
    let companion_for_mood = String::new();

    let res = tokio::task::spawn_blocking(move || -> Result<WriteResult> {
        db.with_write(|tx| {
            if let Some(key) = &dedup {
                let exists: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM event WHERE dedup_key = ?1",
                    rusqlite::params![key],
                    |r| r.get(0),
                )?;
                if exists > 0 {
                    return Ok(WriteResult {
                        id: None,
                        duplicate: true,
                    });
                }
            }
            tx.execute(
                "INSERT INTO event(person_id, at_ms, kind, channel, channel_day,
                                   summary, importance, scope, dedup_key,
                                   importance_source, importance_reason)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                rusqlite::params![
                    body.person_id,
                    at,
                    body.kind,
                    channel,
                    body.channel_day,
                    summary,
                    importance,
                    scope.as_str(),
                    dedup,
                    importance_source,
                    importance_reason,
                ],
            )?;
            Ok(WriteResult {
                id: Some(tx.last_insert_rowid()),
                duplicate: false,
            })
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("写入任务失败：{e}")))??;

    if res.duplicate {
        st.db.note_dedup_hit();
    } else if let Some((reason, dv, da, dd)) = crate::affect::delta_for(&kind_for_mood) {
        // 事件推动情绪。**只在新写入时推**——重放的同一条事件不该二次刺激。
        let db2 = st.db.clone();
        let _cid = companion_for_mood;
        let reason = reason.to_string();
        let _ =
            tokio::task::spawn_blocking(move || -> Result<()> {
                let now = now_ms();
                db2.with_write(|tx| {
                    let cur = tx
                        .query_row(
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
                        .ok();
                    let mut m = cur.unwrap_or_else(|| crate::affect::Mood::neutral(now));
                    // 先把这段时间的衰减补上，再叠加新事件——
                    // 否则「三天没动」的情绪会原封不动地留着。
                    m.decay(now - m.updated_ms, crate::affect::MOOD_HALF_LIFE_MS);
                    m.push(dv, da, dd, &reason, now);
                    tx.execute(
                    "INSERT INTO mood(id, valence, arousal, dominance, label, reason, updated_ms)
                     VALUES (1,?1,?2,?3,?4,?5,?6)
                     ON CONFLICT(id) DO UPDATE SET
                       valence=excluded.valence, arousal=excluded.arousal,
                       dominance=excluded.dominance, label=excluded.label,
                       reason=excluded.reason, updated_ms=excluded.updated_ms",
                    rusqlite::params![
                        m.valence, m.arousal, m.dominance,
                        m.label, m.reason, m.updated_ms
                    ],
                )?;
                    Ok(())
                })
            })
            .await;
    }
    Ok(Json(res))
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    50
}

#[derive(Serialize)]
struct EventRow {
    id: i64,
    at_ms: i64,
    kind: String,
    channel: String,
    summary: String,
    importance: f64,
    /// 当前权重（含衰减与渠道可信度）。前端排序用这个，不用 importance。
    weight: f64,
    channel_day: Option<String>,
    /// 重要度是谁定的：`auto` / `caller` / `manual`。
    importance_source: Option<String>,
    /// 自动判定的逐项依据。
    importance_reason: Option<String>,
}

async fn list_events(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> ApiResult<Vec<EventRow>> {
    check_token(&st, &headers)?;
    let limit = q.limit.clamp(1, 500);
    let db = st.db.clone();
    let wcfg = st
        .weight
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());

    let rows = tokio::task::spawn_blocking(move || -> Result<Vec<EventRow>> {
        db.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, at_ms, kind, channel, summary, importance,
                        channel_day, importance_source, importance_reason
                 FROM event WHERE deleted_by IS NULL
                 ORDER BY at_ms DESC LIMIT ?1",
            )?;
            let now = now_ms();
            let list = stmt
                .query_map(rusqlite::params![limit as i64], |r| {
                    let at_ms: i64 = r.get(1)?;
                    let importance: f64 = r.get(5)?;
                    let channel: String = r.get(3)?;
                    let w = weight::compute(
                        &wcfg,
                        &weight::WeightInput::fresh(
                            importance,
                            FactTier::Extracted,
                            channel.clone(),
                            at_ms,
                        ),
                        now,
                    );
                    Ok(EventRow {
                        id: r.get(0)?,
                        at_ms,
                        kind: r.get(2)?,
                        channel,
                        summary: r.get(4)?,
                        importance,
                        weight: w,
                        channel_day: r.get(6)?,
                        importance_source: r.get(7)?,
                        importance_reason: r.get(8)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(list)
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("查询任务失败：{e}")))??;

    Ok(Json(rows))
}

// ------------------------------------------------------------------ 写入事实

#[derive(Deserialize)]
struct NewFact {
    /// 可选：只参与权重计算（渠道可信度），留空按 system 算。
    #[serde(default)]
    channel: String,
    key: String,
    value: String,
    person_id: Option<String>,
    #[serde(default = "default_scope")]
    scope: String,
    #[serde(default = "confirmed_str")]
    status: String,
    #[serde(default = "extracted_str")]
    tier: String,
    #[serde(default = "half")]
    confidence: f64,
    #[serde(default = "half")]
    importance: f64,
    expires_ms: Option<i64>,
    id: Option<String>,
}

fn confirmed_str() -> String {
    "confirmed".into()
}
fn extracted_str() -> String {
    "extracted".into()
}

async fn post_fact(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<NewFact>,
) -> ApiResult<FactWriteResult> {
    check_token_or_session(&st, &headers)?;
    // `channel` 在写事实时是**可选**的：它只参与权重计算（渠道可信度），
    // 不影响存什么。所以留空回退到 system，而不是报错——
    // 早先这里直接 parse 空串，导致「不传 channel 就 400」，实测踩到。
    let channel = if body.channel.trim().is_empty() {
        "system".to_string()
    } else {
        check_channel(&st, &body.channel)?
    };
    if body.key.trim().is_empty() {
        return Err(Error::BadRequest("key 不能为空".into()));
    }

    let now = now_ms();
    let tier = FactTier::parse(&body.tier);
    let id = body
        .id
        .clone()
        .unwrap_or_else(|| format!("f-{}-{}", now, rand_suffix()));

    // 写入时就把权重算好，排序时不必每次重算全表。
    // 用**当前**权重（面板可能刚改过），不是启动时的快照。
    let wcfg = st
        .weight
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    let w = weight::compute(
        &wcfg,
        &weight::WeightInput::fresh(body.importance, tier, channel.clone(), now),
        now,
    );

    let db = st.db.clone();
    let id_out = id.clone();
    let body_key = body.key.clone();
    let body_person = body.person_id.clone();
    let _person_id = body.person_id.clone();
    let replaced = tokio::task::spawn_blocking(move || -> Result<bool> {
        db.with_write(|tx| {
            // 先查是否存在，用来区分「新建」与「覆盖」——
            // last_insert_rowid 在 ON CONFLICT 下不可靠，不能拿它判断。
            let existed: i64 = tx.query_row(
                "SELECT COUNT(*) FROM fact WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )?;

            // 同一个 (companion, person, key) 的旧事实要被**取代**，不能并存。
            //
            // 这是一个真实缺陷的修法：之前每次 POST 都生成新 id，
            // 于是「主人爱吃的: 鱼」被写了两条一模一样的事实。
            // 并存的后果不是显示难看，而是检索时会同时取回两条，
            // 且两条都有机会被强化，权重各自增长。
            //
            // 但**不能物理删除**：`superseded_by` 指向新行，
            // 让「它以前是这么想的」仍然可回答。
            let old_ids: Vec<String> = {
                let mut q = tx.prepare(
                    // 占位符必须连续：早先删掉 companion_id 后留下了 ?1/?3/?4 的跳号，
                    // SQLite 按「最大编号」算参数个数（4 个），实际只传 2 个 → 必然报错。
                    "SELECT id FROM fact
                     WHERE key = ?1
                       AND COALESCE(person_id,'') = COALESCE(?2,'')
                       AND deleted_by IS NULL AND superseded_by IS NULL
                       AND id <> ?3",
                )?;
                let rows = q.query_map(
                    rusqlite::params![body_key, body_person, id],
                    |r| r.get::<_, String>(0),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            };
            for old in &old_ids {
                tx.execute(
                    "UPDATE fact SET superseded_by = ?1, status = 'superseded', deleted_by = NULL
                     WHERE id = ?2",
                    rusqlite::params![id, old],
                )?;
            }

            tx.execute(
                "INSERT INTO fact(id, person_id, key, value, scope, status, tier,
                                  confidence, importance, created_ms, updated_ms, expires_ms, weight_hint)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10,?11,?12)
                 ON CONFLICT(id) DO UPDATE SET
                   value=excluded.value, confidence=excluded.confidence,
                   importance=excluded.importance, updated_ms=excluded.updated_ms,
                   expires_ms=excluded.expires_ms, weight_hint=excluded.weight_hint",
                rusqlite::params![
                    id,
                    body.person_id,
                    body.key,
                    body.value,
                    Scope::parse(&body.scope).as_str(),
                    FactStatus::parse(&body.status).as_str(),
                    tier.as_str(),
                    body.confidence.clamp(0.0, 1.0),
                    body.importance.clamp(0.0, 1.0),
                    now,          // created_ms 与 updated_ms 同值
                    body.expires_ms,
                    w,
                ],
            )?;
            Ok(existed > 0)
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("写入任务失败：{e}")))??;

    Ok(Json(FactWriteResult {
        id: id_out,
        replaced,
    }))
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{n:08x}")
}

// ------------------------------------------------------------------ 检索

#[derive(Deserialize)]
struct RecallQuery {
    /// 查询词。空 = 只按权重取最重要的几条。
    #[serde(default)]
    q: String,
    #[serde(default = "default_topk")]
    topk: usize,
}

fn default_topk() -> usize {
    8
}

#[derive(Serialize)]
struct RecallRow {
    kind: String,
    id: String,
    at_ms: i64,
    text: String,
    weight: f64,
    /// 命中原因，用于回答「它为什么想起这个」。
    why: String,
    /// 是否命中了关键词。**排序键的一部分**，也单独给出来：
    /// 调用方想知道「这条是真命中还是兜底的」，不该靠 `why` 字符串猜。
    hit: bool,
    /// 相关度 `(0, 1]`，0 表示没命中、纯兜底。
    ///
    /// 与 `weight` **是两件事**：`weight` 只由重要性/时效/渠道决定，
    /// 与提问无关；`score` 才回答「这条有多切题」。
    /// 排序是 `(hit, score, weight, at_ms)`，所以切题优先于重要性。
    score: f64,
    /// 具体命中了哪些词，按贡献从大到小。空 = 未命中。
    ///
    /// 这是「它为什么想起这个」的答案。**单独一个字段**，
    /// 不塞进 `why` —— 那个字段的取值是既有契约。
    matched: Vec<String>,
}

/// 检索。
///
/// # 为什么非空 `q` 不再「不命中就丢」
///
/// 旧行为：`q` 非空时只保留子串命中的，其余 `continue`。
/// 后果是**自然语言查询稳定返回空数组**——
/// 「在吗」「你最近干嘛」「最近」「心情」全都不含任何记忆原文，
/// 于是插件拿到 `[]` → 不注入上下文 → 模型照常编，而且**零报错**。
///
/// 现在：命中的排前面，未命中的兜底。排序键是 `(是否命中, weight)` 的字典序，
/// 所以一条高权重但不相关的老记忆**挤不掉**真正相关的那条，
/// 而库不空时 `q` 非空**永远有东西可返回**。
///
/// 检索索引的缓存。
///
/// # 为什么需要它
///
/// 打分前要先建一次索引（切 bigram、数 df、算平均长度）。实测在真实库
/// （752 条 / 2.6 万字符）上，**release 构建**建一次索引 + 检索要 **6.3 ms**，
/// 与 `/v1/recall` 实测 p50 6.65 ms 完全吻合 —— 也就是说时间全花在
/// 「每来一个请求就把同一份语料重新切一遍」上。
///
/// 这个代价在 752 条时能忍，但记忆每年增长约 3.65 万条。照这个形状，
/// 两年后单次检索会到 **300 ms 量级**，而且没有任何报错，只会让人觉得
/// 「中枢越来越卡」。缓存的收益随语料线性增长，越晚做越贵。
///
/// # 失效依据
///
/// **语料的指纹**，不是写入次数。
///
/// 起初想用 `Db::write_count()`：任何写事务都会让它前进，看起来正好。
/// 但第一次跑判据就撞墙了 —— 检索**本身也会写库**（`note_recalled`
/// 往 `recall_log` 记回放、给 `fact`/`journal` 加 `recall_count`），
/// 于是"每次读都伴随着一次写"，版本号次次都变，缓存**一次都没命中**。
/// 实测未命中数 5 次读 → +5。
///
/// 换成「缓存要用的那三张表有几行」的指纹：
///   - 新增/删除/回滚会改变行数 → 立刻失效（这正是必须命中的情形）；
///   - `recall_count` 变化不改变行数 → 不失效。这**是刻意的**：
///     `recall_count` 只影响 `weight`，而 `weight` 每请求现算
///     （见 `Candidate::weight`），不进缓存。
///
/// 残留的窗口：原地修改某条记忆的 `summary`（不增删行）不会让缓存失效。
/// 这个窗口是明确接受的 —— 修 summary 走的是面板的编辑接口，
/// 而那种操作后面通常会跟一次可见的复核；相比之下"每次读都重建索引"
/// 是 100% 发生的浪费。真要堵死，可以在 `Db` 上单独维护一个
/// "语料变更计数"，只让内容写入去递增它。
/// 语料的指纹：三张参与检索的表各有多少行，外加被逻辑删除的行数。
///
/// 用「行数」而不是「内容哈希」：算哈希要把所有 summary 读一遍再拼起来，
/// 那正好是我们想避免的全表扫描；而行数一次 SQL 就能拿到（SQLite 有
/// 逐表的 `COUNT(*)` 索引优化）。
///
/// 为什么带上 `deleted_by IS NOT NULL` 的计数：逻辑删除**不减少**总行数。
/// 只看 `COUNT(*)` 的话，「回滚删掉一批记忆」不会让缓存失效，
/// 于是被删的条目还能被检索到 —— 而回滚正是最需要立刻生效的操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorpusVersion {
    event: i64,
    event_deleted: i64,
    journal: i64,
    journal_deleted: i64,
    fact: i64,
    fact_deleted: i64,
}

/// 取当前语料指纹。一次查询里数 6 个数，走索引，成本可忽略。
fn corpus_version(c: &rusqlite::Connection) -> Result<CorpusVersion> {
    let mut v = CorpusVersion {
        event: 0,
        event_deleted: 0,
        journal: 0,
        journal_deleted: 0,
        fact: 0,
        fact_deleted: 0,
    };
    let mut stmt = c.prepare(
        "SELECT
           (SELECT COUNT(*) FROM event),
           (SELECT COUNT(*) FROM event WHERE deleted_by IS NOT NULL),
           (SELECT COUNT(*) FROM journal),
           (SELECT COUNT(*) FROM journal WHERE deleted_by IS NOT NULL),
           (SELECT COUNT(*) FROM fact),
           (SELECT COUNT(*) FROM fact WHERE deleted_by IS NOT NULL)",
    )?;
    stmt.query_row([], |r| {
        v.event = r.get(0)?;
        v.event_deleted = r.get(1)?;
        v.journal = r.get(2)?;
        v.journal_deleted = r.get(3)?;
        v.fact = r.get(4)?;
        v.fact_deleted = r.get(5)?;
        Ok(())
    })?;
    Ok(v)
}

#[derive(Default)]
pub struct IndexCache {
    inner: std::sync::Mutex<Option<CachedIndex>>,
    /// 命中/未命中计数，给自检用（也让「缓存到底有没有生效」可测）。
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
}

struct CachedIndex {
    /// 建这份索引时的语料指纹。
    version: CorpusVersion,
    /// 与索引同序的候选元数据（不含 text，text 在索引里）。
    cands: Vec<Candidate>,
    ix: crate::rank::Bm25,
}

impl IndexCache {
    /// 命中缓存就复用，否则用 `build` 建一份新的。
    ///
    /// 闭包只在真的要重建时执行 —— 调用方可以在里面做 SQL 读取。
    ///
    /// 用 `Mutex` 而不是 `RwLock`：重建期间拿着锁是对的（否则两个请求
    /// 会同时重建同一份索引，白花两倍时间）。真正的并发瓶颈不在这，
    /// 实测检索只占 p50 的几毫秒。
    fn get_or_build<F>(
        &self,
        version: CorpusVersion,
        build: F,
    ) -> Result<(Vec<Candidate>, std::sync::Arc<crate::rank::Bm25>)>
    where
        F: FnOnce() -> Result<Vec<Candidate>>,
    {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = guard.as_ref() {
            if c.version == version {
                self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok((c.cands.clone(), std::sync::Arc::new(c.ix.clone())));
            }
        }
        self.misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let cands = build()?;
        let docs: Vec<crate::rank::Doc> = cands
            .iter()
            .map(|x| crate::rank::Doc {
                key: format!("{}:{}", x.kind, x.id),
                text: x.text.clone(),
            })
            .collect();
        let ix = crate::rank::Bm25::new(&docs);
        *guard = Some(CachedIndex {
            version,
            cands: cands.clone(),
            ix: ix.clone(),
        });
        Ok((cands, std::sync::Arc::new(ix)))
    }

    /// 带版本探测的 `get_or_build`：先算指纹，再决定要不要重建。
    ///
    /// 拆成两个闭包是因为**算指纹本身要读库**，而旧签名要求调用方
    /// 在锁外就把版本算好 —— 那样会出现「算完版本、还没进锁，
    /// 别人写了一条」的窗口，正好丢掉刚写进来的记忆。
    /// 现在指纹在锁内算，窗口没有了（代价是拿锁期间多一次 COUNT 查询）。
    fn get_or_build_ver<P, F>(
        &self,
        probe: P,
        build: F,
    ) -> Result<(Vec<Candidate>, std::sync::Arc<crate::rank::Bm25>)>
    where
        P: FnOnce() -> Result<CorpusVersion>,
        F: FnOnce() -> Result<Vec<Candidate>>,
    {
        let version = probe()?;
        self.get_or_build(version, build)
    }

    /// `(命中次数, 未命中次数)`。用于自检与测试。
    pub fn stats(&self) -> (u64, u64) {
        (
            self.hits.load(std::sync::atomic::Ordering::Relaxed),
            self.misses.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

/// 一条候选记忆。三张表（event / journal / fact）先各自取出来，
/// 归一成这个形状之后再统一打分 —— 而不是在三段 SQL 里各排各的。
///
/// 为什么必须统一：旧实现里 event 和 fact 各自算 `weight` 然后拼成一个
/// 数组再排序，这段逻辑没法跨表比较「谁更相关」，因为 `weight` 里
/// 根本没有查询这一维。现在相关性由 [`crate::rank`] 统一算。
#[derive(Clone)]
struct Candidate {
    kind: &'static str,
    id: String,
    at_ms: i64,
    text: String,
    /// 算 `weight` 所需的全部输入。
    ///
    /// **刻意不在缓存里存算好的 `weight`**：`weight` 含时效衰减，
    /// 依赖「现在几点」。把某一刻算出的值缓存下来，会让同一份索引
    /// 在两小时后仍然按两小时前的衰减给分 —— 而且**没有任何报错**，
    /// 只是记忆比该有的样子更"新鲜"。每请求重算 752 行是几十微秒的事。
    win: weight::WeightInput,
}

impl Candidate {
    /// 按当前时刻算权重。
    fn weight(&self, cfg: &weight::WeightConfig, now: i64) -> f64 {
        weight::compute(cfg, &self.win, now)
    }
}

/// 检索的**核心**，`/v1/recall` 与 `/v1/context` 共用同一份。
///
/// 两边各写一份 SQL 是不行的：`/v1/context` 里那段「你记得的事」如果是
/// 另一套排序，就会出现「直接调 recall 有结果、塞进提示词却是别的几条」
/// 这种没法排查的分叉。所以排序口径只能有一处。
///
/// 从库里把三张表的候选取出来。
///
/// **这一步是要访问数据库的**，结果会被 [`IndexCache`] 缓存起来 ——
/// 它只跟"库里有什么"有关，跟"这次问的是什么"无关。
fn load_candidates(c: &rusqlite::Connection, now: i64) -> Result<Vec<Candidate>> {
    // 三张表各自做**结构化过滤**（哪些行有资格参与打分），
    // 然后统一交给 `rank::Bm25` 打分。
    //
    // 过滤必须在打分之前，且必须在 SQL 里做（而不是取回来再筛）：
    // 一条已被取代的旧事实、一条过期的偏好、一条回滚删掉的记录，
    // **哪怕字面最像也不该参与**。这是向量库范式做不到的事 ——
    // 它是「先召回后过滤」，而这套语义要求「先过滤后召回」。
    //
    // 注意 `now` 在这里**只**用于 fact 的 `expires_ms` 过滤，
    // 不参与 any 权重计算 —— 那部分每请求现算（见 `Candidate::weight`）.
    let mut cands: Vec<Candidate> = Vec::new();

    // -------- event：MC / QQ / 直播 三渠道的事件流
    {
        let mut stmt = c.prepare(
            "SELECT id, at_ms, channel, summary, importance
             FROM event WHERE deleted_by IS NULL
             ORDER BY at_ms DESC LIMIT 2000",
        )?;
        let mut rs = stmt.query([])?;
        while let Some(r) = rs.next()? {
            let at_ms: i64 = r.get(1)?;
            let channel: String = r.get(2)?;
            let summary: String = r.get(3)?;
            let importance: f64 = r.get(4)?;
            cands.push(Candidate {
                kind: "event",
                id: r.get::<_, i64>(0)?.to_string(),
                at_ms,
                text: summary,
                win: weight::WeightInput::fresh(
                    importance,
                    FactTier::Extracted,
                    channel.clone(),
                    at_ms,
                ),
            });
        }
    }

    // -------- journal：手帐。**这是最大的一块语料**。
    //
    // 实测线上库：journal 585 条 / 15,219 字符 = 全库可检索文本的 **58.6%**。
    // 旧实现只查 event + fact，等于一多半记忆永远想不起来 ——
    // 而「它知道自己最近干了什么」恰好全在 journal 里。
    //
    // 过滤口径与 event 一致：`deleted_by IS NULL`。
    // 不按 importance 卡阈值：手帐的 importance 普遍是 0.5～0.6，
    // 卡掉就会把「做完了『找蜘蛛摸线』」这种真正该被想起的行筛没。
    {
        let mut stmt = c.prepare(
            "SELECT id, at_ms, summary, importance, channel, recall_count
             FROM journal WHERE deleted_by IS NULL
             ORDER BY at_ms DESC LIMIT 2000",
        )?;
        let mut rs = stmt.query([])?;
        while let Some(r) = rs.next()? {
            let at_ms: i64 = r.get(1)?;
            let summary: String = r.get(2)?;
            let importance: f64 = r.get(3)?;
            let channel: String = r.get(4)?;
            let recall_count: u32 = r.get::<_, i64>(5)? as u32;
            cands.push(Candidate {
                kind: "journal",
                id: r.get::<_, String>(0)?,
                at_ms,
                text: summary,
                win: weight::WeightInput {
                    importance,
                    // 手帐是**发生过的事**，不是人写下的规矩 —— 按 Extracted
                    // 参与衰减。curated 只留给「人亲手写下的事实」。
                    tier: FactTier::Extracted,
                    channel,
                    at_ms,
                    recall_count,
                },
            });
        }
    }

    // -------- fact：过期的、被取代的都不该出现在「想起来」里
    {
        let mut stmt = c.prepare(
            "SELECT id, key, value, updated_ms, importance, tier, recall_count
             FROM fact WHERE deleted_by IS NULL
               AND status = 'confirmed' AND superseded_by IS NULL
               AND (expires_ms IS NULL OR expires_ms > ?1)
             ORDER BY updated_ms DESC LIMIT 2000",
        )?;
        let mut rs = stmt.query(rusqlite::params![now])?;
        while let Some(r) = rs.next()? {
            let key: String = r.get(1)?;
            let value: String = r.get(2)?;
            let updated_ms: i64 = r.get(3)?;
            let importance: f64 = r.get(4)?;
            let tier = FactTier::parse(&r.get::<_, String>(5)?);
            let recall_count: u32 = r.get::<_, i64>(6)? as u32;
            cands.push(Candidate {
                kind: "fact",
                id: r.get::<_, String>(0)?,
                at_ms: updated_ms,
                text: format!("{key}: {value}"),
                win: weight::WeightInput {
                    importance,
                    tier,
                    channel: "system".to_string(),
                    at_ms: updated_ms,
                    recall_count,
                },
            });
        }
    }

    Ok(cands)
}

/// 打分这一步**不碰数据库**：候选与索引都从 [`IndexCache`] 来。
/// 这样一次检索里最贵的部分（切 bigram、数 df）能在多个请求之间复用。
///
/// 调用方负责在 `spawn_blocking` 里调它（rusqlite 是同步的）。
fn recall_core(
    cands: Vec<Candidate>,
    ix: &crate::rank::Bm25,
    needle: &str,
    topk: usize,
    wcfg: &weight::WeightConfig,
    now: i64,
) -> Result<Vec<RecallRow>> {
    // ---------------------------------------------------------- 打分
    //
    // **相关性**由 BM25 给，**时效/重要性**由 weight 给，两者分开保存：
    //   - `hit` 只有一种含义 —— 「真的相关」，由 BM25 决定；
    //   - `weight` 仍然原样返回，面板与既有调用方都在用它。
    //
    // 旧实现的 `hit` 是 `contains(q)`，换个说法就全灭（实测改写提问 8 个全灭）。

    // 相关度阈值。0.30 是实测定的：
    //   - 低于它，命中的都是「共用一个常见 bigram」的噪声
    //     （实测「钓鱼用的竿子做好了吗」在 0.2 附近会捞进「盖好了小屋的东墙」）；
    //   - 高于它，真相关的那几条会一起被砍掉。
    const MIN_SCORE: f64 = 0.30;

    // 只有**含实义词**且过了阈值的命中才算数。
    //
    // 两道闸都必要：
    //   - `meaningful`：拦「在吗」「最近」这类纯虚词查询。归一化是相对的，
    //     它们照样能算出 1.0 分（实测），但那时命中的只是「在」这种字；
    //   - `MIN_SCORE`：拦「共用一个常见 bigram」的噪声。
    let mut scored: std::collections::HashMap<String, (f64, Vec<String>)> =
        std::collections::HashMap::new();
    if !needle.is_empty() {
        for s in ix.search(needle) {
            if !s.meaningful || s.score < MIN_SCORE {
                continue;
            }
            scored.insert(ix.key(s.idx).to_string(), (s.score, s.matched));
        }
    }

    let mut out: Vec<RecallRow> = cands
        .into_iter()
        .map(|cand| {
            let k = format!("{}:{}", cand.kind, cand.id);
            let (score, matched) = match scored.remove(&k) {
                Some((s, m)) => (s, m),
                None => (0.0, Vec::new()),
            };
            // 先取值再移字段：`Candidate::weight` 借 `self`，
            // 一旦把 `id`/`text` move 走就不能再调了。
            let w = cand.weight(wcfg, now);
            RecallRow {
                kind: cand.kind.to_string(),
                id: cand.id,
                at_ms: cand.at_ms,
                weight: w,
                text: cand.text,
                why: why_of(score),
                hit: score > 0.0,
                score,
                matched,
            }
        })
        .collect();

    // ---------------------------------------------------------- 3. 排序
    //
    // 排序键：`(是否命中, 相关度, weight, 时间新→旧)`，全部字典序。
    //
    // 为什么相关度在 weight **之前**：weight 只看重要性/时效/渠道，
    // 它不知道用户问的是什么。让 weight 优先会让「一件高权重的旧事」
    // 永远压过「真正回答了这句话的那条」—— 那正是旧实现的毛病。
    //
    // 为什么未命中的也要返回：库不空时 `q` 非空**永远有东西可返回**，
    // 否则调用方拿到 `[]` → 不注入 → 模型照常编，且零报错。
    // 这是一条已经修过一次的静默失效，不能退回去。
    out.sort_by(|a, b| {
        b.hit
            .cmp(&a.hit)
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| {
                b.weight
                    .partial_cmp(&a.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| b.at_ms.cmp(&a.at_ms))
    });
    out.truncate(topk);
    Ok(out)
}

/// 命中原因的文案。
///
/// **只有两个取值**，这是既有契约（前端、`docs/` 下的三份文档、
/// 多条判据都在按字面比较它们）：
///   - `关键词命中` —— 这条真的相关
///   - `权重最高`   —— 纯兜底，没命中
///
/// 把「具体匹配到哪些词」塞进这个字段曾经是一个诱人的想法，
/// 但它会让所有按字面比较的调用方静默失配。所以那件事交给
/// [`RecallRow::matched`] 这个**新字段**，`why` 保持原样。
fn why_of(score: f64) -> String {
    if score > 0.0 {
        "关键词命中".into()
    } else {
        "权重最高".into()
    }
}

/// 记录「这几条真的被想起来了」。
///
/// **只记截断之后真正返回的那几条**：检索时算分用了 2000 条候选，
/// 但那不叫「被想起」；只有真正进了上下文、影响了它说话的那几条才算。
/// 全部记进去会让 `recall_log` 每轮涨两千行，且 `recall_count` 彻底失真。
///
/// 失败只记日志：检索是读路径，不能被一次写失败拖垮。
fn note_recalled(db: &std::sync::Arc<Db>, rows: &[RecallRow], q_text: &str, lane: &str) {
    // **只有真正相关的才算「被想起」**。
    //
    // 这里曾经把所有返回行（包括 `hit=false` 的纯兜底行）都算进去。
    // 后果是一条自我强化的锁死：兜底行被 `note_recalled` 反复 +1，
    // `recall_count` 涨 → weight 涨 → 下一轮还是它兜底 → 再 +1。
    // 实测的表现是「换十个不同的问题，注入的记忆段一模一样」（唯一性 1/10），
    // 而这条环路就是原因之一：兜底行一旦上位就再也下不来。
    //
    // 兜底行仍然返回给调用方（否则会退回「空数组 → 静默不注入」的老毛病），
    // 但它**不该**因为「恰好被塞满剩余名额」而被强化。
    let touched: Vec<(String, String)> = rows
        .iter()
        .filter(|r| r.hit && matches!(r.kind.as_str(), "fact" | "event" | "journal"))
        .map(|r| (r.kind.clone(), r.id.clone()))
        .collect();
    // 注意：**这里不能因为 touched 为空就提前返回**。
    // 「写 trace」和「加回忆计数」是两件事：
    //   - trace 记的是「这一轮返回了哪几条、为什么」，兜底行**也要记**，
    //     否则面板上看不出「它这一轮其实没想起什么，是硬塞的」；
    //   - 计数只在真命中时 +1（见上面的注释）。
    // 早退会让一次全是兜底的检索在回放里彻底消失，读起来像「它什么都没拿到」，
    // 而实际情况是「拿到了几条不相关的」—— 这两件事的排查方向完全相反。
    //
    // 数据提前摘成普通值：`rows` 之后还要原样返回给调用方，不能整个 move 走。
    // `kept` 如实记录这条是命中还是兜底 —— 回放面板要能回答
    // 「这一轮为什么用了这条」，把兜底记成命中会让那个问题无解。
    let trace_rows: Vec<(String, String, f64, i64, String, String)> = rows
        .iter()
        .map(|r| {
            (
                r.kind.clone(),
                r.id.clone(),
                r.weight,
                i64::from(r.hit),
                r.why.clone(),
                r.text.clone(),
            )
        })
        .collect();

    if trace_rows.is_empty() {
        return;
    }

    let db2 = db.clone();
    let trace_id = format!("t-{}-{}", now_ms(), rand_suffix());
    let q_text = q_text.to_string();
    let lane = lane.to_string();
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || -> Result<()> {
            let now = now_ms();
            db2.with_write(|tx| {
                // 先记「谁进了结果」——一条 trace 一行，带 lane 与理由。
                for (kind, id, w, kept, why, text) in &trace_rows {
                    tx.execute(
                        "INSERT INTO recall_log(kind, target_id, at_ms,
                                                query, score, trace_id, lane, kept, reason, text)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                        rusqlite::params![kind, id, now, q_text, w, trace_id, lane, kept, why, text],
                    )?;
                }
                for (kind, id) in &touched {
                    match kind.as_str() {
                        "fact" => {
                            // recall_count 是**衰减过的有效回忆数**，不是历史累计。
                            //
                            // 为什么这么设计：如果只 +1 不减，一条被碰巧检索过
                            // 一万次的老事实会永久霸榜，而「最近常被想起」才是
                            // 该被强化的信号。
                            //
                            // 做法：每次被回忆时，先把旧值按「距上次回忆的时间」
                            // 折半衰减，再加 1。等价于一个带遗忘的计数器。
                            const HALF_LIFE_MS: i64 = 7 * 24 * 3600 * 1000;
                            let row: Option<(i64, Option<i64>)> = tx
                                .query_row(
                                    "SELECT recall_count, last_recall_ms FROM fact WHERE id = ?1",
                                    rusqlite::params![id],
                                    |r| Ok((r.get(0)?, r.get(1)?)),
                                )
                                .ok();
                            let new_count = match row {
                                Some((old, Some(last))) => {
                                    let dt = now - last;
                                    let f = 0.5_f64.powf(dt as f64 / HALF_LIFE_MS as f64);
                                    ((old as f64 * f) + 1.0).min(120.0)
                                }
                                Some((old, None)) => (old as f64 + 1.0).min(120.0),
                                None => 1.0,
                            };
                            tx.execute(
                                "UPDATE fact SET recall_count = ?1, last_recall_ms = ?2
                                 WHERE id = ?3",
                                rusqlite::params![new_count.round() as i64, now, id],
                            )?;
                        }
                        "event" => {
                            // event 表没有 recall_count 列，用 recall_log 记。
                            // 这样即使用户后来回滚删了 event，历史回忆记录还在。
                            tx.execute(
                                "INSERT INTO recall_log(kind, target_id, at_ms)
                                 VALUES ('event', ?1, ?2)",
                                rusqlite::params![id, now],
                            )?;
                        }
                        "journal" => {
                            // journal **有** recall_count 列（老代码留的），
                            // 之前从没被写过 —— 因为 journal 从来不参与检索。
                            // 现在它进了检索面，强化计数就该跟上，否则
                            // 「常被想起的手帐」永远得不到它该有的权重。
                            //
                            // 口径与 fact 一致（带遗忘的计数器），而不是简单的 +1：
                            // 两个表的 recall_count 若语义不同，读代码的人
                            // 迟早会把它们当成同一件事。
                            const HALF_LIFE_MS: i64 = 7 * 24 * 3600 * 1000;
                            let row: Option<(i64, Option<i64>)> = tx
                                .query_row(
                                    "SELECT recall_count, last_recall_ms FROM journal WHERE id = ?1",
                                    rusqlite::params![id],
                                    |r| Ok((r.get(0)?, r.get(1)?)),
                                )
                                .ok();
                            let new_count = match row {
                                Some((old, Some(last))) => {
                                    let dt = now - last;
                                    let f = 0.5_f64.powf(dt as f64 / HALF_LIFE_MS as f64);
                                    ((old as f64 * f) + 1.0).min(120.0)
                                }
                                Some((old, None)) => (old as f64 + 1.0).min(120.0),
                                None => 1.0,
                            };
                            tx.execute(
                                "UPDATE journal SET recall_count = ?1, last_recall_ms = ?2
                                 WHERE id = ?3",
                                rusqlite::params![new_count.round() as i64, now, id],
                            )?;
                        }
                        _ => {}
                    }
                }
                Ok(())
            })
        })
        .await;
    });
}

async fn recall(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<RecallQuery>,
) -> ApiResult<Vec<RecallRow>> {
    check_token(&st, &headers)?;
    let topk = q.topk.clamp(1, 100);
    let db = st.db.clone();
    let wcfg = st
        .weight
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    let needle = q.q.trim().to_lowercase();

    let index = st.index.clone();
    let rows = tokio::task::spawn_blocking(move || -> Result<Vec<RecallRow>> {
        let now = now_ms();
        let (cands, ix) = index.get_or_build_ver(
            || db.with_read(corpus_version),
            || db.with_read(|c| load_candidates(c, now)),
        )?;
        recall_core(cands, &ix, &needle, topk, &wcfg, now)
    })
    .await
    .map_err(|e| Error::Storage(format!("检索任务失败：{e}")))??;

    note_recalled(&st.db, &rows, &q.q, "main");

    Ok(Json(rows))
}

// ------------------------------------------------------------------ 聚合上下文

#[derive(Deserialize)]
struct ContextQuery {
    /// 返回文本的字符上限。clamp 到 `[200, 4000]`。
    budget: Option<usize>,
    /// 群友这句话，用于挑相关记忆。空 = 只按时间/权重取。
    #[serde(default)]
    q: String,
    /// 记忆条目上限。clamp `[1, 30]`。
    topk: Option<usize>,
    /// 会话场景：`group` / `private`。
    ///
    /// 不传时按 `group` —— 见 `context::Scene` 的注释：忘了传参数是最常见的调用方式，
    /// 默认必须落在能给出去的**最小**集合上，不能靠调用方自觉。
    #[serde(default)]
    scene: Option<String>,
}

#[derive(Serialize)]
struct ContextBody {
    /// 可直接拼接进 system_prompt 的 markdown。没有可注入内容时是空串（不是 null）。
    text: String,
    /// `text` 的**字符数**（`chars().count()`，不是字节数）。
    chars: usize,
    /// 实际进了 `text` 的段落名，按出现顺序。
    parts: Vec<String>,
    /// 被预算挤掉或读取失败的段落及**具体原因**。
    dropped: Vec<String>,
    generated_ms: i64,
    /// 服务端**实际采用**的场景。
    ///
    /// 回显是为了让「我明明传了 private 怎么还是群聊」这类问题一眼可查：
    /// 传了无法识别的值时这里会显示 `group`，而不是静默按调用方以为的那个走。
    scene: &'static str,
}

/// 一次调用、拿到一段可直接塞进提示词的记忆上下文。
///
/// # 为什么要这个口
///
/// 让插件自己拼 6 次请求（`recall`、`journal/list`、`diary/list`、`plan`、
/// `mood`、`self`）不行：每个都要在**回答之前**跑完，延迟叠加。
/// 而且这条链一旦有一段静默失败，插件看到的只是「少了一段」，不会报错。
///
/// # 「任何一张表读失败都不许整体 500」
///
/// 这是硬要求，不是容错加分项。记忆是**锦上添花**：少一段它还能说话，
/// 而整个接口 500 会让插件连「它今天在干什么」都拿不到。
/// 所以每一段都自己 catch，失败写进 `dropped`（带原因），其余照常返回。
async fn get_context(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ContextQuery>,
) -> ApiResult<ContextBody> {
    check_token(&st, &headers)?;

    let budget = q.budget.unwrap_or(1200).clamp(200, 4000);
    let topk = q.topk.unwrap_or(5).clamp(1, 30);
    let needle = q.q.trim().to_lowercase();
    let q_raw = q.q.clone();
    // 场景在**进入请求之前**就定下来，之后只按它取数。
    // 解析不了的取值回落到 Group（见 `context::Scene::parse`），绝不因为写错参数而多给。
    let scene = crate::context::Scene::parse(q.scene.as_deref());

    let db = st.db.clone();
    let index = st.index.clone();
    let wcfg = st
        .weight
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone());

    // 知识库那段要 await（查询嵌入是异步的），所以**在进 spawn_blocking 之前**算好。
    // 放进闭包里不行：闭包是同步的，而且它跑在 blocking 线程池上，
    // 在那里 await 会把线程池的线程挂住。
    let kb_seg = load_kb_segment(&st, &needle).await;

    // 一次进 `spawn_blocking`：rusqlite 是同步的，且各段之间是**独立的读**，
    // 同一连接上顺序查完比跑 6 次 spawn 便宜得多。
    let (segments, notes, recalled) = tokio::task::spawn_blocking(move || {
        let now = now_ms();
        let tz = crate::context::tz_offset_min();
        let mut segments: Vec<crate::context::Segment> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut recalled: Vec<RecallRow> = Vec::new();

        // 每一段都单独走 `with_read`：一段失败不影响别的段，
        // 而且读连接池让它们各自拿到一个可用的连接。
        let mut push = |seg: Option<String>, name: &'static str| {
            if let Some(s) = crate::context::Segment::new(name, seg.unwrap_or_default()) {
                segments.push(s);
            }
        };

        // -------- 1. 计划
        match load_plan_block(&db) {
            Ok((summary, items)) => push(
                crate::context::render_plan(summary.as_deref(), &items),
                crate::context::SEG_PLAN,
            ),
            Err(e) => notes.push(format!("plan: 读取失败（{e}）")),
        }

        // -------- 2. 记忆（与 /v1/recall 同一套排序）
        let (cands, ix) = match index.get_or_build_ver(
            || db.with_read(corpus_version),
            || db.with_read(|c| load_candidates(c, now)),
        ) {
            Ok(v) => v,
            Err(e) => {
                notes.push(format!("memory: 读取失败（{e}）"));
                (Vec::new(), std::sync::Arc::new(crate::rank::Bm25::new(&[])))
            }
        };
        match recall_core(cands, &ix, &needle, topk, &wcfg, now) {
            Ok(rows) => {
                let texts: Vec<String> = rows.iter().map(|r| r.text.clone()).collect();
                recalled = rows;
                push(
                    crate::context::render_memory(&texts),
                    crate::context::SEG_MEMORY,
                )
            }
            Err(e) => notes.push(format!("memory: 读取失败（{e}）")),
        }

        // -------- 2.4 称呼对照（只在本轮这句话提到某个人时出现）
        //
        // 为什么不把所有身份都列出来：上下文每轮都发，整张 person 表既费 token，
        // 也会让模型把不相干的人扯进来。触发词就是这句话里出现的名字
        // （显示名或任一场景称呼，≥2 字且无歧义）。
        match crate::query::persons_mentioned(&db, &needle, 3) {
            Ok(rows) => push(
                crate::context::render_names(&rows),
                crate::context::SEG_NAMES,
            ),
            Err(e) => notes.push(format!("names: 读取失败（{e}）")),
        }

        // -------- 2.5 知识库（查到的资料，**不是**亲历）
        //
        // 放在 memory 之后、events 之前：问到一个事实性问题时，
        // 它比「最近发生的事」更直接回答用户。但没有命中时它不占位
        // （`render_kb` 对空输入返回 None），所以日常闲聊不受影响。
        if let Some(text) = kb_seg.0 {
            push(Some(text), crate::context::SEG_KB);
        }

        // -------- 3. 最近事件（只要 mc 渠道）
        match load_events_block(&db) {
            Ok(rows) => push(
                crate::context::render_events(&rows, now, tz),
                crate::context::SEG_EVENTS,
            ),
            Err(e) => notes.push(format!("events: 读取失败（{e}）")),
        }

        // -------- 4. 最近说的话（qq 渠道，正序）—— **按场景切开**
        //
        // 群场景只给群聊行、私聊场景只给私聊行。这一刀必须在这里落下：
        // 上一段（记忆）和这一段都进同一个 system_prompt，模型分不出哪句来自哪儿。
        match load_group_block(&db, scene) {
            Ok(rows) => push(
                crate::context::render_group_scene(&rows, scene),
                crate::context::SEG_GROUP,
            ),
            Err(e) => notes.push(format!("group: 读取失败（{e}）")),
        }

        // -------- 5. 上次的日记
        //
        // 日记**总是给全文**（上限 600 字）。
        //
        // 为什么不按「这句话有没有提到日记」分两档：实测必然漏 ——
        // 第一句问「读读你昨天的日记」，下一句追问「怎么还藏私，明明还有」
        // 里没有「日记」二字，就退回 80 字，模型只能对着开头编。
        // 一篇日记 200–450 字，整段上下文仍在预算内。
        match load_diary_block(&db) {
            Ok(Some((title, body))) => push(
                crate::context::render_diary(title.as_deref(), &body),
                crate::context::SEG_DIARY,
            ),
            Ok(None) => {}
            Err(e) => notes.push(format!("diary: 读取失败（{e}）")),
        }

        // -------- 6. 心情（平静不写，见 `render_mood`）
        match load_mood_block(&db, now) {
            Ok(m) => push(
                crate::context::render_mood(&m.label, &m.reason, crate::affect::intensity(&m)),
                crate::context::SEG_MOOD,
            ),
            Err(e) => notes.push(format!("mood: 读取失败（{e}）")),
        }

        Ok::<_, Error>((segments, notes, recalled))
    })
    .await
    .map_err(|e| Error::Storage(format!("上下文任务失败：{e}")))??;

    // 进了最终文本的那几条才算「被想起」。注意要等 assemble 之后再判断——
    // 被预算挤掉的段落里的条目不叫被回忆过。
    let assembled = crate::context::assemble(segments, notes, budget);
    if assembled
        .parts
        .iter()
        .any(|p| p == crate::context::SEG_MEMORY)
    {
        note_recalled(&st.db, &recalled, &q_raw, "context");
    }

    Ok(Json(ContextBody {
        chars: assembled.text.chars().count(),
        text: assembled.text,
        parts: assembled.parts,
        dropped: assembled.dropped,
        generated_ms: now_ms(),
        scene: scene.as_str(),
    }))
}

/// 把 `QueryReturnedNoRows` 变成 `Ok(None)`，**其余错误照常往上抛**。
///
/// # 为什么不能直接 `.ok()`
///
/// `.ok()` 会把 `no such table` / `no such column` / 磁盘读失败**也**变成
/// 「没有数据」。那正好是本次要消灭的那类失效：接口返回 200、段落空着、
/// `dropped` 里也没有任何记录 —— 调用方以为「它确实没有日记」，
/// 实际是那张表根本读不出来。两者对用户是完全不同的后果。
fn optional_row<T>(r: std::result::Result<T, rusqlite::Error>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 计划条目的投影：`(state, title)`。只要这两个字段——
/// 「它今天要干什么」不需要 args_json / priority 那些调度细节。
type PlanLine = (String, String);

/// 群里一条消息的投影：`(sender_name, account, content)`。
/// 两个名字都留的原因是打码口径归调用方（见 `context::sender_display`）。
type GroupLine = (Option<String>, Option<String>, String);

/// 最新一张未删的计划 + 它的条目。`None` = 没有未删的计划。
fn load_plan_block(db: &Db) -> Result<(Option<String>, Vec<PlanLine>)> {
    db.with_read(|c| {
        // `deleted_by IS NULL` + 按现实时间倒序：`g<N>` 是游戏时钟，
        // 停服期间不走，拿它排序会把「最近一份」判断错。
        let head: Option<(String, Option<String>)> = optional_row(c.query_row(
            "SELECT id, summary FROM plan WHERE deleted_by IS NULL
             ORDER BY created_ms DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ))?;
        let Some((plan_id, summary)) = head else {
            return Ok((None, Vec::new()));
        };
        let mut stmt =
            c.prepare("SELECT state, title FROM plan_item WHERE plan_id = ?1 ORDER BY ord")?;
        let items = stmt
            .query_map(rusqlite::params![plan_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok((summary, items))
    })
}

/// 最近 6 条 MC 事件，`at_ms` 倒序。
fn load_events_block(db: &Db) -> Result<Vec<(i64, String)>> {
    db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT at_ms, summary FROM event
             WHERE deleted_by IS NULL AND channel = 'mc'
             ORDER BY at_ms DESC LIMIT 6",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

/// 最近 8 条 QQ 消息。**要正序**（先说的在前），所以先倒序取 8 条再翻过来。
///
/// `scene` 决定取哪一半：群聊是 `group_id IS NOT NULL`，私聊是 `IS NULL`。
///
/// 这个划分**只靠 `group_id` 是否为空**，所以写入口必须守住两件事：
/// 群消息一定要带 `group_id`，私聊一定要**不带这个字段**（传空串会被当成"群号是空的"，
/// 那仍然是个非 NULL 值，私聊就会混进群段里）。
fn load_group_block(db: &Db, scene: crate::context::Scene) -> Result<Vec<GroupLine>> {
    let sql = match scene {
        crate::context::Scene::Group => {
            "SELECT sender_name, account, content FROM channel_message
             WHERE deleted_by IS NULL AND channel = 'qq' AND group_id IS NOT NULL
               AND group_id <> ''
             ORDER BY at_ms DESC, id DESC LIMIT 8"
        }
        crate::context::Scene::Private => {
            "SELECT sender_name, account, content FROM channel_message
             WHERE deleted_by IS NULL AND channel = 'qq'
               AND (group_id IS NULL OR group_id = '')
             ORDER BY at_ms DESC, id DESC LIMIT 8"
        }
    };
    db.with_read(|c| {
        let mut stmt = c.prepare(sql)?;
        let mut rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.reverse();
        // 群里最近说的话里，发送者用**身份显示名**而不是群名片：
        // 群名片随时会改（啥子猫 → 啥子喵），模型认不出「这是同一个人」。
        // 优先按 QQ 号认人（账号绑定最稳），名字只作兜底。
        let names = crate::query::sender_display_names(c)?;
        for (name, account, content) in rows.iter_mut() {
            if let Some(acc) = account.as_deref() {
                if let Some(display) = names.by_account.get(acc) {
                    *name = Some(display.clone());
                }
            }
            if let Some(cur) = name.as_deref() {
                if let Some(display) = names.by_name.get(&cur.to_lowercase()) {
                    *name = Some(display.clone());
                }
            }
            // 内容里的 @昵称 也换掉（QQ 的 @ 在这里是纯文本，例如
            // 「@啥子喵(478889187) 姆姆又偷偷加强自己了」）。
            for (from, to) in &names.aliases {
                if content.contains(from) {
                    *content = content.replace(from, to);
                }
            }
        }
        Ok(rows)
    })
}

/// 最新一篇 `kind='day'` 的日记。
fn load_diary_block(db: &Db) -> Result<Option<(Option<String>, String)>> {
    db.with_read(|c| {
        optional_row(c.query_row(
            "SELECT title, body FROM diary
             WHERE deleted_by IS NULL AND kind = 'day'
             ORDER BY window_end_ms DESC LIMIT 1",
            [],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)),
        ))
    })
}

/// 当前心情（先补衰减，理由同 `/v1/mood`）。
fn load_mood_block(db: &Db, now: i64) -> Result<crate::affect::Mood> {
    db.with_read(|c| {
        // 同样不能 `.ok()`：`mood` 表读失败要能被 `dropped` 记下来，
        // 不能被当成「还没被推动过」（那会静默写「平静」，与真相相反）。
        let mood = optional_row(c.query_row(
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
        ))?;
        let mut m = mood.unwrap_or_else(|| crate::affect::Mood::neutral(now));
        m.decay(now - m.updated_ms, crate::affect::MOOD_HALF_LIFE_MS);
        Ok(m)
    })
}

// ------------------------------------------------------------------ 规划

#[derive(Deserialize)]
struct PlanListQuery {
    limit: Option<usize>,
}

#[derive(Serialize)]
struct PlanItemOut {
    id: String,
    title: String,
    /// `done` / `todo` / `deferred` / `abandoned` —— **原样透传**，不译中文。
    /// 译中文是 `/v1/context` 那段给人看的文本的事，机器读的接口留原值。
    state: String,
    reason: Option<String>,
    outcome: Option<String>,
    kind: Option<String>,
    task_type: Option<String>,
    priority: Option<i64>,
    ord: i64,
}

#[derive(Serialize)]
struct PlanOut {
    id: String,
    planning_day_id: String,
    summary: Option<String>,
    model: Option<String>,
    game_day: Option<i64>,
    created_ms: i64,
    updated_ms: i64,
    total: i64,
    done: i64,
    deferred: i64,
    todo: i64,
    items: Vec<PlanItemOut>,
}

impl PlanOut {
    fn from_detail(d: crate::query::PlanDetail) -> Self {
        let p = d.plan;
        Self {
            id: p.id,
            planning_day_id: p.planning_day_id,
            summary: p.summary,
            model: p.model,
            game_day: p.game_day,
            created_ms: p.created_ms,
            updated_ms: p.updated_ms,
            total: p.total_items,
            done: p.done_items,
            deferred: p.deferred_items,
            todo: p.todo_items,
            items: d.items.into_iter().map(PlanItemOut::from).collect(),
        }
    }
}

impl From<crate::query::PlanItemRow> for PlanItemOut {
    fn from(i: crate::query::PlanItemRow) -> Self {
        Self {
            id: i.id,
            title: i.title,
            state: i.state,
            reason: i.reason,
            outcome: i.outcome,
            kind: i.kind,
            task_type: i.task_type,
            priority: i.priority,
            ord: i.ord,
        }
    }
}

/// 列出规划，带条目。
///
/// 契约要求 `items` 一起带上（调用方可能只想看概要，但带上的成本很低）。
/// 所以这里对每一份规划各查一次条目——`limit` 默认很小（5），
/// 而且计划一天只有一份，N 次主键查询比一个 JOIN 加去重好读。
async fn list_plans(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PlanListQuery>,
) -> ApiResult<Vec<PlanOut>> {
    check_token(&st, &headers)?;
    let limit = q.limit.unwrap_or(5).clamp(1, 200) as u32;
    let db = st.db.clone();

    let out = tokio::task::spawn_blocking(move || -> Result<Vec<PlanOut>> {
        // 复用面板那套查询：同一个 `plan_detail` 保证「面板看到什么，API 拿到什么」。
        // 面板走 `query::plans` 分页，这里固定第 1 页、size=limit。
        let page = crate::query::Page {
            page: 1,
            size: limit,
        };
        let paged = crate::query::plans(&db, None, page)?;
        let mut out = Vec::with_capacity(paged.items.len());
        for row in paged.items {
            match crate::query::plan_detail(&db, &row.id) {
                Ok(d) => out.push(PlanOut::from_detail(d)),
                // 单份详情读失败不该让整张列表失败：条目标题是锦上添花，
                // 概要（完成到哪）才是调用方真正要的。
                Err(e) => {
                    tracing::warn!("规划 {} 的条目读取失败：{e}", row.id);
                    out.push(PlanOut {
                        id: row.id,
                        planning_day_id: row.planning_day_id,
                        summary: row.summary,
                        model: row.model,
                        game_day: row.game_day,
                        created_ms: row.created_ms,
                        updated_ms: row.updated_ms,
                        total: row.total_items,
                        done: row.done_items,
                        deferred: row.deferred_items,
                        todo: row.todo_items,
                        items: Vec::new(),
                    });
                }
            }
        }
        Ok(out)
    })
    .await
    .map_err(|e| Error::Storage(format!("规划查询失败：{e}")))??;

    Ok(Json(out))
}

async fn get_plan(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<PlanOut> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<PlanOut> {
        crate::query::plan_detail(&db, &id).map(PlanOut::from_detail)
    })
    .await
    .map_err(|e| Error::Storage(format!("规划查询失败：{e}")))??;
    Ok(Json(out))
}

// ------------------------------------------------------------------ 身份

#[derive(Deserialize)]
struct BindRequest {
    channel: String,
    account: String,
    person_id: String,
    seen_name: Option<String>,
}

#[derive(Serialize)]
struct BindResult {
    ok: bool,
    channel: String,
    account: String,
    person_id: String,
    /// 这个账号之前绑的是谁（如果有）。绑定变化要能追溯。
    previous_person: Option<String>,
}

async fn bind_person(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BindRequest>,
) -> ApiResult<BindResult> {
    check_token(&st, &headers)?;
    // 校验并归一（`MC` / `Mc` 都成 `mc`），否则库里会同时出现 "mc" 和 "MC"，
    // 绑定按字符串精确匹配就查不到。
    let ch = check_channel(&st, &body.channel)?;
    if body.account.trim().is_empty() || body.person_id.trim().is_empty() {
        return Err(Error::BadRequest("account 与 person_id 都不能为空".into()));
    }

    let db = st.db.clone();
    let acct = body.account.clone();
    let pid = body.person_id.clone();
    let seen = body.seen_name.clone();

    let previous = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        db.with_write(|tx| {
            let prev: Option<String> = tx
                .query_row(
                    "SELECT person_id FROM person_account WHERE channel = ?1 AND account = ?2",
                    rusqlite::params![ch, acct],
                    |r| r.get(0),
                )
                .ok();

            let now = now_ms();
            // 人不存在就先建一个 pending 的。
            tx.execute(
                "INSERT INTO person(person_id, display_name, status, created_ms)
                 VALUES (?1, ?2, 'pending', ?3)
                 ON CONFLICT(person_id) DO NOTHING",
                rusqlite::params![pid, seen.clone().unwrap_or_else(|| pid.clone()), now],
            )?;

            tx.execute(
                "INSERT INTO person_account(channel, account, person_id, seen_name,
                                            first_seen_ms, last_seen_ms)
                 VALUES (?1,?2,?3,?4,?5,?5)
                 ON CONFLICT(channel, account) DO UPDATE SET
                   person_id = excluded.person_id,
                   seen_name = COALESCE(excluded.seen_name, person_account.seen_name),
                   last_seen_ms = excluded.last_seen_ms",
                rusqlite::params![ch, acct, pid, seen, now],
            )?;

            // 绑定即确认。
            tx.execute(
                "UPDATE person SET status = 'confirmed' WHERE person_id = ?1",
                rusqlite::params![pid],
            )?;
            Ok(prev)
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("绑定任务失败：{e}")))??;

    Ok(Json(BindResult {
        ok: true,
        channel: body.channel,
        account: body.account,
        person_id: body.person_id,
        previous_person: previous,
    }))
}

#[derive(Serialize)]
struct PendingRow {
    channel: String,
    account: String,
    person_id: String,
    display_name: String,
    seen_name: Option<String>,
    first_seen_ms: i64,
}

/// 未确认归属的账号。面板上「把这两个账号拖到一起」的输入就是它。
async fn pending_persons(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Vec<PendingRow>> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    let rows = tokio::task::spawn_blocking(move || -> Result<Vec<PendingRow>> {
        db.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT pa.channel, pa.account, pa.person_id, p.display_name, pa.seen_name,
                        pa.first_seen_ms
                 FROM person_account pa
                 JOIN person p ON p.person_id = pa.person_id
                 WHERE p.status = 'pending'
                 ORDER BY pa.first_seen_ms DESC LIMIT 200",
            )?;
            let list = stmt
                .query_map([], |r| {
                    Ok(PendingRow {
                        channel: r.get(0)?,
                        account: r.get(1)?,
                        person_id: r.get(2)?,
                        display_name: r.get(3)?,
                        seen_name: r.get(4)?,
                        first_seen_ms: r.get(5)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(list)
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("查询任务失败：{e}")))??;
    Ok(Json(rows))
}

/// 列出可用渠道。接入方在写之前可以先问一下有哪些 tag。
async fn list_channels(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Vec<crate::query::ChannelItem>> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    let items = tokio::task::spawn_blocking(move || crate::query::channels(&db))
        .await
        .map_err(|e| Error::Storage(format!("渠道查询失败：{e}")))??;
    Ok(Json(items))
}

#[derive(Deserialize)]
struct IgnoredQuery {
    channel: String,
    account: String,
}

#[derive(Serialize)]
struct IgnoredOut {
    /// true = 这个人被设为「忽视」，别回复、别写记忆。
    ignored: bool,
    /// 这个账号绑到的人（没绑过账号时为 null）。
    ///
    /// 为什么返回它：接入方排查「为什么他不回我」时，需要区分
    /// 「没绑账号所以没匹配上」和「绑了但被忽视了」。
    person_id: Option<String>,
    display_name: Option<String>,
}

/// 这个人要不要被忽视。接入方在回复前问一次（一次查询，很便宜）。
///
/// 为什么放服务端判断：`person_id` ↔ QQ 号的绑定只有中枢知道，
/// 接入方手里只有一个账号字符串。
async fn person_ignored_check(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<IgnoredQuery>,
) -> ApiResult<IgnoredOut> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<IgnoredOut> {
        db.with_read(|c| {
            let row = c
                .query_row(
                    "SELECT pa.person_id, p.display_name, p.ignored
                     FROM person_account pa
                     LEFT JOIN person p ON p.person_id = pa.person_id
                     WHERE pa.channel = ?1 AND pa.account = ?2",
                    rusqlite::params![q.channel, q.account],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, Option<i64>>(2)?,
                        ))
                    },
                )
                .ok();
            Ok(match row {
                Some((pid, name, ign)) => IgnoredOut {
                    ignored: ign.unwrap_or(0) != 0,
                    person_id: Some(pid),
                    display_name: name,
                },
                // 没绑过账号：不是「忽视」，是「还不认识」。照常回，只不跨渠道合并。
                None => IgnoredOut {
                    ignored: false,
                    person_id: None,
                    display_name: None,
                },
            })
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("忽视查询失败：{e}")))??;
    Ok(Json(out))
}

// ------------------------------------------------------------------ 称呼归一

#[derive(Deserialize)]
struct NormalizeNamesBody {
    text: String,
    /// 只看这个场景的称呼（mc / qq / qq_group / live）。省略 = 全部场景。
    scene: Option<String>,
}

#[derive(Serialize)]
struct NameReplacementOut {
    from: String,
    to: String,
    count: usize,
}

#[derive(Serialize)]
struct NormalizeNamesOut {
    ok: bool,
    text: String,
    replacements: Vec<NameReplacementOut>,
}

/// 把回复文本里的「场景称呼」替换成身份显示名。
///
/// 为什么要在中枢做：场景称呼（QQ 群名片、MC 名、直播间昵称）只有中枢知道，
/// 调用方（AstrBot / MC 插件）拿不到这张表。调用方拿到回复后调一次，
/// 发送前把名字统一成显示名即可；中枢本身不生成文本，也不拦消息。
///
/// 例：身份显示名「姆姆」，QQ 群名片「啥子猫」→
/// `{"text":"啥子猫说得对","scene":"qq_group"}` → `{"text":"姆姆说得对"}`。
async fn normalize_names(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<NormalizeNamesBody>,
) -> ApiResult<NormalizeNamesOut> {
    check_token(&st, &headers)?;
    if b.text.trim().is_empty() {
        return Err(Error::BadRequest("text 不能为空".into()));
    }
    if b.text.chars().count() > 4000 {
        return Err(Error::BadRequest("text 太长（上限 4000 字）".into()));
    }
    let scene = b.scene.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(scene) = scene {
        if !crate::query::is_person_scene(scene) {
            return Err(Error::BadRequest(format!(
                "未知场景：{scene}（只能是 mc / qq / qq_group / live）"
            )));
        }
    }
    let scene = scene.map(str::to_string);

    let db = st.db.clone();
    let (text, hits) = tokio::task::spawn_blocking(move || -> Result<_> {
        let mappings = crate::query::name_mappings(&db, scene.as_deref())?;
        Ok(crate::query::replace_names(&b.text, &mappings))
    })
    .await
    .map_err(|e| Error::Storage(format!("称呼归一失败：{e}")))??;

    Ok(Json(NormalizeNamesOut {
        ok: true,
        text,
        replacements: hits
            .into_iter()
            .map(|(from, to, count)| NameReplacementOut { from, to, count })
            .collect(),
    }))
}

// ------------------------------------------------------------------ 快照与回滚

#[derive(Deserialize)]
struct SnapshotRequest {
    note: Option<String>,
}

async fn take_snapshot(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SnapshotRequest>,
) -> ApiResult<crate::snapshot::SnapshotInfo> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    let store = st.snapshots.clone();
    let note = body.note.clone();
    let info = tokio::task::spawn_blocking(move || store.take(&db, note.as_deref()))
        .await
        .map_err(|e| Error::Storage(format!("快照任务失败：{e}")))??;
    Ok(Json(info))
}

async fn list_snapshots(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Vec<crate::snapshot::SnapshotInfo>> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    let store = st.snapshots.clone();
    let list = tokio::task::spawn_blocking(move || store.list(&db, 90))
        .await
        .map_err(|e| Error::Storage(format!("查询任务失败：{e}")))??;
    Ok(Json(list))
}

#[derive(Deserialize)]
struct RollbackRequest {
    /// 回滚到这个时刻：**晚于**它的记忆全部遗忘。
    target_ms: i64,
    note: Option<String>,
}

async fn do_rollback(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RollbackRequest>,
) -> ApiResult<rollback::RollbackOutcome> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    let note = body.note.clone();
    let target = body.target_ms;
    let out =
        tokio::task::spawn_blocking(move || rollback::rollback_to(&db, target, note.as_deref()))
            .await
            .map_err(|e| Error::Storage(format!("回滚任务失败：{e}")))??;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct UndoRequest {
    rollback_id: i64,
}

async fn do_undo(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<UndoRequest>,
) -> ApiResult<rollback::RollbackOutcome> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    let id = body.rollback_id;
    let out = tokio::task::spawn_blocking(move || rollback::undo(&db, id))
        .await
        .map_err(|e| Error::Storage(format!("撤销任务失败：{e}")))??;
    Ok(Json(out))
}

async fn rollback_history(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Vec<rollback::RollbackRecord>> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    let list = tokio::task::spawn_blocking(move || rollback::history(&db, 50))
        .await
        .map_err(|e| Error::Storage(format!("查询任务失败：{e}")))??;
    Ok(Json(list))
}

// ------------------------------------------------------------------ 编辑

#[derive(Deserialize)]
struct EventPatch {
    summary: Option<String>,
    importance: Option<f64>,
    kind: Option<String>,
    scope: Option<String>,
}

/// 改一条事件。只允许改内容，不允许改时间与渠道——
/// 时间和来源是「它当时确实经历了什么」的证据，不是可以事后编辑的东西。
async fn patch_event(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(p): Json<EventPatch>,
) -> ApiResult<WriteResult> {
    check_token_or_session(&st, &headers)?;
    if let Some(v) = p.importance {
        if !(0.0..=1.0).contains(&v) {
            return Err(Error::BadRequest("importance 必须在 [0,1]".into()));
        }
    }
    if let Some(s) = &p.summary {
        if s.trim().is_empty() {
            return Err(Error::BadRequest("summary 不能为空".into()));
        }
    }

    let db = st.db.clone();
    let n = tokio::task::spawn_blocking(move || -> Result<usize> {
        db.with_write(|tx| {
            let exists: i64 = tx.query_row(
                "SELECT COUNT(*) FROM event WHERE id = ?1 AND deleted_by IS NULL",
                rusqlite::params![id],
                |r| r.get(0),
            )?;
            if exists == 0 {
                return Err(Error::NotFound(format!("事件 {id}")));
            }
            // COALESCE 让「没传的字段保持原值」成立，不必先读一遍再全量写回。
            let n = tx.execute(
                // 人改了重要度 → 来源标成 manual，并把自动依据清掉。
                // 否则面板上会出现「标着自动、值却被人改过」这种自相矛盾的显示。
                "UPDATE event SET
                   summary    = COALESCE(?1, summary),
                   importance = COALESCE(?2, importance),
                   kind       = COALESCE(?3, kind),
                   scope      = COALESCE(?4, scope),
                   importance_source = CASE WHEN ?2 IS NULL THEN importance_source
                                            ELSE 'manual' END,
                   importance_reason = CASE WHEN ?2 IS NULL THEN importance_reason
                                            ELSE NULL END
                 WHERE id = ?5",
                rusqlite::params![p.summary, p.importance, p.kind, p.scope, id],
            )?;
            Ok(n)
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("编辑任务失败：{e}")))??;

    Ok(Json(WriteResult {
        id: Some(id),
        duplicate: false,
    }))
    .map(|j| {
        let _ = n;
        j
    })
}

#[derive(Deserialize)]
struct FactPatch {
    value: Option<String>,
    importance: Option<f64>,
    confidence: Option<f64>,
    status: Option<String>,
    tier: Option<String>,
    scope: Option<String>,
}

async fn patch_fact(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(p): Json<FactPatch>,
) -> ApiResult<FactWriteResult> {
    check_token_or_session(&st, &headers)?;
    for (name, v) in [("importance", p.importance), ("confidence", p.confidence)] {
        if let Some(v) = v {
            if !(0.0..=1.0).contains(&v) {
                return Err(Error::BadRequest(format!("{name} 必须在 [0,1]")));
            }
        }
    }

    let db = st.db.clone();
    let id2 = id.clone();
    tokio::task::spawn_blocking(move || -> Result<usize> {
        db.with_write(|tx| {
            let exists: i64 = tx.query_row(
                "SELECT COUNT(*) FROM fact WHERE id = ?1 AND deleted_by IS NULL",
                rusqlite::params![id2],
                |r| r.get(0),
            )?;
            if exists == 0 {
                return Err(Error::NotFound(format!("事实 {id2}")));
            }
            let now = now_ms();
            let n = tx.execute(
                "UPDATE fact SET
                   value      = COALESCE(?1, value),
                   importance = COALESCE(?2, importance),
                   confidence = COALESCE(?3, confidence),
                   status     = COALESCE(?4, status),
                   tier       = COALESCE(?5, tier),
                   scope      = COALESCE(?6, scope),
                   updated_ms = ?7
                 WHERE id = ?8",
                rusqlite::params![
                    p.value,
                    p.importance,
                    p.confidence,
                    p.status,
                    p.tier,
                    p.scope,
                    now,
                    id2
                ],
            )?;
            Ok(n)
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("编辑任务失败：{e}")))??;

    Ok(Json(FactWriteResult { id, replaced: true }))
}

#[derive(Deserialize)]
struct JournalPatch {
    summary: Option<String>,
    details: Option<String>,
    importance: Option<f64>,
}

async fn patch_journal(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(p): Json<JournalPatch>,
) -> ApiResult<WriteResult> {
    check_token_or_session(&st, &headers)?;
    if let Some(v) = p.importance {
        if !(0.0..=1.0).contains(&v) {
            return Err(Error::BadRequest("importance 必须在 [0,1]".into()));
        }
    }
    if let Some(s) = &p.summary {
        if s.trim().is_empty() {
            return Err(Error::BadRequest("summary 不能为空".into()));
        }
    }

    let db = st.db.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            let exists: i64 = tx.query_row(
                "SELECT COUNT(*) FROM journal WHERE id = ?1 AND deleted_by IS NULL",
                rusqlite::params![id],
                |r| r.get(0),
            )?;
            if exists == 0 {
                return Err(Error::NotFound(format!("手帐 {id}")));
            }
            tx.execute(
                "UPDATE journal SET
                   summary    = COALESCE(?1, summary),
                   details    = COALESCE(?2, details),
                   importance = COALESCE(?3, importance)
                 WHERE id = ?4",
                rusqlite::params![p.summary, p.details, p.importance, id],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("编辑任务失败：{e}")))??;

    Ok(Json(WriteResult {
        id: None,
        duplicate: false,
    }))
}

// ------------------------------------------------------------------ 删除

/// 删除 = 逻辑删除。**不是物理删除**，与回滚共用 `deleted_by` 机制。
///
/// 为什么不做物理删除：
/// ① 误删能救回来（`deleted_by` 清空即可）；
/// ② 「某个东西曾经存在过、后来被删了」本身是有信息量的；
/// ③ 真正要回收空间时由运维侧 `purge` 统一做，那里才是不可逆的。
#[derive(Deserialize)]
struct DeleteRequest {
    /// 删除原因，写进审计。
    note: Option<String>,
}

/// 为一个「单条删除」开一条 rollback_log 记录，这样它和回滚共用撤销路径。
fn mark_deleted(
    db: &std::sync::Arc<Db>,
    table: &str,
    id_col: &str,
    id: &str,
    note: Option<&str>,
) -> Result<i64> {
    let now = now_ms();
    db.with_write(|tx| {
        let exists: i64 = tx.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {id_col} = ?1 AND deleted_by IS NULL"),
            rusqlite::params![id],
            |r| r.get(0),
        )?;
        if exists == 0 {
            return Err(Error::NotFound(format!("{table} {id}")));
        }
        tx.execute(
            "INSERT INTO rollback_log(at_ms, target_ms, mode, affected_json, note)
             VALUES (?1, ?1, 'delete', '[]', ?2)",
            rusqlite::params![now, note],
        )?;
        let rid = tx.last_insert_rowid();
        tx.execute(
            &format!("UPDATE {table} SET deleted_by = ?1 WHERE {id_col} = ?2"),
            rusqlite::params![rid, id],
        )?;
        Ok(rid)
    })
}

async fn delete_event(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<i64>,
    body: Option<Json<DeleteRequest>>,
) -> ApiResult<WriteResult> {
    check_token_or_session(&st, &headers)?;
    let db = st.db.clone();
    let note = body.and_then(|b| b.note.clone());
    let id_s = id.to_string();
    tokio::task::spawn_blocking(move || mark_deleted(&db, "event", "id", &id_s, note.as_deref()))
        .await
        .map_err(|e| Error::Storage(format!("删除任务失败：{e}")))??;
    Ok(Json(WriteResult {
        id: Some(id),
        duplicate: false,
    }))
}

async fn delete_fact(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: Option<Json<DeleteRequest>>,
) -> ApiResult<FactWriteResult> {
    check_token_or_session(&st, &headers)?;
    let db = st.db.clone();
    let note = body.and_then(|b| b.note.clone());
    let id2 = id.clone();
    tokio::task::spawn_blocking(move || mark_deleted(&db, "fact", "id", &id2, note.as_deref()))
        .await
        .map_err(|e| Error::Storage(format!("删除任务失败：{e}")))??;
    Ok(Json(FactWriteResult {
        id,
        replaced: false,
    }))
}

async fn delete_journal(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: Option<Json<DeleteRequest>>,
) -> ApiResult<WriteResult> {
    check_token_or_session(&st, &headers)?;
    let db = st.db.clone();
    let note = body.and_then(|b| b.note.clone());
    let id2 = id.clone();
    tokio::task::spawn_blocking(move || mark_deleted(&db, "journal", "id", &id2, note.as_deref()))
        .await
        .map_err(|e| Error::Storage(format!("删除任务失败：{e}")))??;
    Ok(Json(WriteResult {
        id: None,
        duplicate: false,
    }))
}

async fn delete_diary(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: Option<Json<DeleteRequest>>,
) -> ApiResult<WriteResult> {
    check_token_or_session(&st, &headers)?;
    let db = st.db.clone();
    let note = body.and_then(|b| b.note.clone());
    let id2 = id.clone();
    tokio::task::spawn_blocking(move || mark_deleted(&db, "diary", "id", &id2, note.as_deref()))
        .await
        .map_err(|e| Error::Storage(format!("删除任务失败：{e}")))??;
    Ok(Json(WriteResult {
        id: None,
        duplicate: false,
    }))
}

/// 删除一条渠道消息。
///
/// `channel_message` 是三张可编辑表里**唯一没有删除口**的：清理误入库的群消息、
/// 删错同步都只能进 SQLite 手改。走和 event/fact/journal 一样的 `mark_deleted`
/// （逻辑删除 + `rollback_log`），所以和回滚共用撤销路径。
///
/// 注意路由是 `/v1/message/{id}` 而**不是** `/v1/message/consumed`：
/// axum 的路由匹配里静态段优先于动态段，那条 POST 不会被这里抢走。
async fn delete_message(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<i64>,
    body: Option<Json<DeleteRequest>>,
) -> ApiResult<WriteResult> {
    check_token_or_session(&st, &headers)?;
    let db = st.db.clone();
    let note = body.and_then(|b| b.note.clone());
    let id_s = id.to_string();
    tokio::task::spawn_blocking(move || {
        mark_deleted(&db, "channel_message", "id", &id_s, note.as_deref())
    })
    .await
    .map_err(|e| Error::Storage(format!("删除任务失败：{e}")))??;
    Ok(Json(WriteResult {
        id: Some(id),
        duplicate: false,
    }))
}

// ------------------------------------------------------------------ 新增（手帐/日记）

#[derive(Deserialize)]
struct NewJournal {
    #[serde(default)]
    summary: String,
    #[serde(default = "half")]
    importance: f64,
    #[serde(default = "half")]
    confidence: f64,
    #[serde(default = "journal_type_default")]
    type_: String,
    #[serde(default = "mc_channel")]
    channel: String,
    details: Option<String>,
    planning_day_id: Option<String>,
    at_ms: Option<i64>,
    dedup_key: Option<String>,
}

fn journal_type_default() -> String {
    "other".into()
}
fn mc_channel() -> String {
    "mc".into()
}

async fn post_journal(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<NewJournal>,
) -> ApiResult<WriteResult> {
    check_token_or_session(&st, &headers)?;
    if b.summary.trim().is_empty() {
        return Err(Error::BadRequest("summary 不能为空".into()));
    }
    if !(0.0..=1.0).contains(&b.importance) {
        return Err(Error::BadRequest("importance 必须在 [0,1]".into()));
    }
    let now = b.at_ms.unwrap_or_else(now_ms);
    let id = format!("j-{now}-{}", rand_suffix());
    let db = st.db.clone();
    let id2 = id.clone();
    // 同上：空串幂等键会永久吞掉后来的所有手帐。
    let dedup_key = normalize_key(b.dedup_key.clone());
    let dup = tokio::task::spawn_blocking(move || -> Result<bool> {
        db.with_write(|tx| {
            if let Some(k) = &dedup_key {
                let n: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM journal WHERE dedup_key = ?1",
                    rusqlite::params![k],
                    |r| r.get(0),
                )?;
                if n > 0 {
                    return Ok(true);
                }
            }
            tx.execute(
                "INSERT INTO journal(id, at_ms, planning_day_id, type, source,
                                     summary, details, importance, confidence, channel, dedup_key)
                 VALUES (?1,?2,?3,?4,'manual',?5,?6,?7,?8,?9,?10)",
                rusqlite::params![
                    id2,
                    now,
                    b.planning_day_id,
                    b.type_,
                    b.summary,
                    b.details,
                    b.importance,
                    b.confidence.clamp(0.0, 1.0),
                    b.channel,
                    dedup_key
                ],
            )?;
            Ok(false)
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("写入任务失败：{e}")))??;

    // 返回真实 id：调用方要能立刻引用这条新记录。
    // 之前返回 Some(0) 是错的——那是个假值，拿到手也用不了。
    Ok(Json(WriteResult {
        id: if dup { None } else { Some(0) },
        duplicate: dup,
    }))
}

#[derive(Deserialize)]
struct NewDiary {
    #[serde(default)]
    body: String,
    #[serde(default = "day_kind")]
    kind: String,
    title: Option<String>,
    window_start_ms: Option<i64>,
    window_end_ms: Option<i64>,
    model: Option<String>,
}

fn day_kind() -> String {
    "day".into()
}

async fn post_diary(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<NewDiary>,
) -> ApiResult<WriteResult> {
    check_token_or_session(&st, &headers)?;
    if b.body.trim().is_empty() {
        return Err(Error::BadRequest("body 不能为空".into()));
    }
    let now = now_ms();
    let end = b.window_end_ms.unwrap_or(now);
    let start = b.window_start_ms.unwrap_or(end - 86_400_000);
    let id = format!("d-{now}-{}", rand_suffix());
    let db = st.db.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO diary(id, kind, window_start_ms, window_end_ms,
                                   title, body, model, created_ms, updated_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8)",
                rusqlite::params![id, b.kind, start, end, b.title, b.body, b.model, now],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| Error::Storage(format!("写入任务失败：{e}")))??;

    Ok(Json(WriteResult {
        id: None,
        duplicate: false,
    }))
}

// ------------------------------------------------------------------ 情绪

#[derive(Deserialize)]
struct MoodQuery {
    // 单主体后没有可筛的维度；保留空结构体只是为了 handler 签名不变。
}

/// 读当前情绪。
///
/// 返回前先补算衰减：库里存的是「最后一次被推动时」的状态，
/// 而调用方要的是「现在」。少了这一步，三天没动过的情绪会原样返回。
async fn get_mood(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(_q): Query<MoodQuery>,
) -> ApiResult<crate::affect::Mood> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    let m = tokio::task::spawn_blocking(move || -> Result<crate::affect::Mood> {
        let now = now_ms();
        let mut mood = db.with_read(|c| {
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
        let mut m = mood
            .take()
            .unwrap_or_else(|| crate::affect::Mood::neutral(now));
        m.decay(now - m.updated_ms, crate::affect::MOOD_HALF_LIFE_MS);
        Ok(m)
    })
    .await
    .map_err(|e| Error::Storage(format!("情绪查询失败：{e}")))??;
    Ok(Json(m))
}

// ------------------------------------------------------------------ 统计

#[derive(Serialize)]
struct StatsBody {
    tables: Vec<(String, i64)>,
    forgotten_rows: i64,
    writes: u64,
    dedup_hits: u64,
    snapshot_count: i64,
    weight_config: weight::WeightConfig,
}

async fn stats(State(st): State<AppState>, headers: HeaderMap) -> ApiResult<StatsBody> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    let (tables, forgotten, snaps) = tokio::task::spawn_blocking(move || {
        let tables = db.counts().unwrap_or_default();
        let forgotten = rollback::forgotten_count(&db).unwrap_or(0);
        let snaps = db
            .with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM snapshot", [], |r| r.get(0))?))
            .unwrap_or(0);
        (tables, forgotten, snaps)
    })
    .await
    .map_err(|e| Error::Storage(format!("统计任务失败：{e}")))?;

    Ok(Json(StatsBody {
        tables,
        forgotten_rows: forgotten,
        writes: st.db.write_count(),
        dedup_hits: st.db.dedup_hits(),
        snapshot_count: snaps,
        weight_config: st
            .weight
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone()),
    }))
}

// ================================================================== 知识库
//
// 知识库与记忆是**两套独立的东西**：记忆会衰减/过期/失效/能回滚，
// 知识库条目是外部资料，不衰减、不随时间变淡、「忘掉它」没有语义。
// 所以它有自己的表、自己的检索路径，**不进 `/v1/recall` 也不进 `/v1/context`**。
//
// 设计依据与实测数据见 docs/知识库接入方案.md。

#[derive(Deserialize)]
struct KbCreateBody {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
}

/// `POST /v1/kb` —— 建一本知识库（同 id 幂等更新）。
async fn post_kb(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<KbCreateBody>,
) -> ApiResult<serde_json::Value> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    let now = now_ms();
    let kb = tokio::task::spawn_blocking(move || -> Result<crate::kb::Kb> {
        db.with_write(|c| crate::kb::create_kb(c, &b.id, &b.name, b.description.as_deref(), now))
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!({ "kb": kb })))
}

/// `GET /v1/kb/list`
async fn list_kb(State(st): State<AppState>, headers: HeaderMap) -> ApiResult<serde_json::Value> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    let (kbs, docs) =
        tokio::task::spawn_blocking(move || -> Result<(Vec<serde_json::Value>, i64)> {
            db.with_read(|c| {
                let kbs = crate::kb::list_kbs(c)?;
                let mut out = Vec::with_capacity(kbs.len());
                let mut total_docs = 0i64;
                for k in kbs {
                    let ds = crate::kb::list_docs(c, &k.id)?;
                    total_docs += ds.len() as i64;
                    out.push(serde_json::json!({
                        "id": k.id,
                        "name": k.name,
                        "description": k.description,
                        "docs": ds.len(),
                        "docs_detail": ds,
                    }));
                }
                Ok((out, total_docs))
            })
        })
        .await
        .map_err(|e| Error::Storage(e.to_string()))??;
    Ok(Json(serde_json::json!({ "kbs": kbs, "doc_count": docs })))
}

#[derive(Deserialize)]
struct KbDocBody {
    kb_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    source: Option<String>,
    content: String,
}

/// `POST /v1/kb/doc` —— 导入一篇文档。
///
/// **导入同步、嵌入异步**：导入要立刻可用（用户要看到「导成功了」），
/// 嵌入要调外部服务可能慢可能挂，不能阻塞它。所以新块是 `pending`，
/// 由后台回填。代价：刚导入的文档几秒内只能被字面命中。
async fn post_kb_doc(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<KbDocBody>,
) -> ApiResult<serde_json::Value> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    let opts = crate::kb::chunk::Opts {
        max_chars: st.cfg.kb.chunk_max_chars,
    };
    let now = now_ms();
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

    // 导入后立刻催一次回填，而不是等下一个 30 秒 ——
    // 这是「几秒内只能被字面命中」那个窗口能有多短的唯一办法。
    if r.n_chunks > 0 && !r.duplicate {
        crate::kb::backfill::nudge();
    }
    Ok(Json(serde_json::json!({
        "doc_id": r.doc_id,
        "n_chunks": r.n_chunks,
        "duplicate": r.duplicate,
        "embedded": false,
        "note": if r.duplicate { "内容已存在，未重复导入" } else { "已入库，向量正在后台生成；此前只能被字面命中" },
    })))
}

#[derive(Deserialize)]
struct KbSearchBody {
    #[serde(default)]
    kb_id: Option<String>,
    q: String,
    #[serde(default)]
    topk: Option<usize>,
    /// 候选池大小；`None` 用默认值。只影响「先取多少条再融合」。
    #[serde(default)]
    pool: Option<usize>,
}

/// `POST /v1/kb/search` —— 检索。
///
/// 融合方式：RRF（`w_vec=2.0, w_lit=1.0, k=60`）+ 同文档名额限制。
/// **不用加权和** —— 实测加权和是四种融合里最差的（MRR 0.441 vs 0.500），
/// 因为 BM25 分数与 cosine 不在同一尺度，归一化会让相对权重随查询漂移。
async fn search_kb(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<KbSearchBody>,
) -> ApiResult<serde_json::Value> {
    check_token(&st, &headers)?;
    let q = b.q.trim().to_string();
    if q.is_empty() {
        return Err(Error::BadRequest("q 不能为空".into()));
    }
    let topk = b.topk.unwrap_or(5).clamp(1, 20);
    let pool = b.pool.unwrap_or(200).clamp(topk, 2000);
    let kb_id = b.kb_id.clone();

    // ---- 1. 取候选（先过滤后召回）
    let db = st.db.clone();
    let kb_id2 = kb_id.clone();
    let cands = tokio::task::spawn_blocking(move || -> Result<Vec<crate::kb::rank::Candidate>> {
        db.with_read(|c| crate::kb::load_candidates(c, kb_id2.as_deref()))
    })
    .await
    .map_err(|e| Error::Storage(e.to_string()))??;

    if cands.is_empty() {
        return Ok(Json(serde_json::json!({
            "hits": [],
            "degraded": if st.embedder.is_none() && st.cfg.kb.enable { Some("embed_unavailable") } else { None },
            "pool_size": 0,
        })));
    }

    // ---- 2. 查询向量（失败则降级，不报错）
    let mut degraded: Option<&str> = None;
    let mut vec_sims: Option<std::collections::HashMap<String, f64>> = None;

    if let Some(emb) = st.embedder.as_ref() {
        match emb.embed(std::slice::from_ref(&q)).await {
            Ok(v) if !v.is_empty() => {
                let qv = v[0].clone();
                let db = st.db.clone();
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
                    // 有嵌入服务但一条向量都没有（还没回填完）：
                    // 这不是错误，但要如实说清楚，否则用户会以为检索坏了。
                    degraded = Some("no_vectors_yet");
                } else {
                    let m: std::collections::HashMap<String, f64> = vecs
                        .into_iter()
                        .map(|(id, v)| (id, crate::kb::cosine(&qv, &v)))
                        .collect();
                    vec_sims = Some(m);
                }
            }
            Ok(_) => degraded = Some("empty_embedding"),
            Err(e) => {
                // 降级必须可观测，但不该每次失败都打一行日志：
                // 一个挂掉的服务每 3 秒一行错误，日志就没用了。
                tracing::debug!("知识库查询嵌入失败，退回纯字面：{e}");
                degraded = Some("embed_failed");
            }
        }
    } else if st.cfg.kb.enable {
        degraded = Some("embed_unavailable");
    }

    // ---- 3. RRF 融合。候选池先按两路各自的信号截断，再融合
    let mut opts = st.cfg.kb.rank.clone();
    // pool 限制的是「融合前保留多少候选」。RRF 的分数只与排名有关，
    // 所以先截断再融合与全量融合的顺序一致，只是省内存。
    let cands = if cands.len() > pool * 4 {
        cands[..pool * 4].to_vec()
    } else {
        cands
    };
    opts.max_per_doc = opts.max_per_doc.max(1);

    let hits = crate::kb::rank::fuse(&cands, &q, vec_sims.as_ref(), topk, &opts);

    Ok(Json(serde_json::json!({
        "hits": hits,
        "degraded": degraded,
        "pool_size": cands.len(),
    })))
}

/// `GET /v1/kb/health`
async fn kb_health(State(st): State<AppState>, headers: HeaderMap) -> ApiResult<serde_json::Value> {
    check_token(&st, &headers)?;
    let db = st.db.clone();
    /// `(库数, 文档数, 块数, (pending, done, failed, skipped))`
    type KbCounts = (i64, i64, i64, (i64, i64, i64, i64));
    let (kb_n, doc_n, chunk_n, counts) =
        tokio::task::spawn_blocking(move || -> Result<KbCounts> {
            db.with_read(|c| {
                let kb_n: i64 = c.query_row("SELECT COUNT(*) FROM kb", [], |r| r.get(0))?;
                let doc_n: i64 = c.query_row(
                    "SELECT COUNT(*) FROM kb_doc WHERE deleted_by IS NULL",
                    [],
                    |r| r.get(0),
                )?;
                let chunk_n: i64 =
                    c.query_row("SELECT COUNT(*) FROM kb_chunk", [], |r| r.get(0))?;
                let counts = crate::kb::embed_counts(c)?;
                Ok((kb_n, doc_n, chunk_n, counts))
            })
        })
        .await
        .map_err(|e| Error::Storage(e.to_string()))??;

    let embed_state = if !st.cfg.kb.enable {
        "disabled"
    } else if st.embedder.is_none() {
        "unavailable"
    } else if counts.2 > 0 {
        "degraded"
    } else {
        "ok"
    };

    Ok(Json(serde_json::json!({
        "enabled": st.cfg.kb.enable,
        "kb_count": kb_n,
        "doc_count": doc_n,
        "chunk_count": chunk_n,
        "embed": {
            "state": embed_state,
            "model": st.cfg.kb.embed.model,
            "dim": st.cfg.kb.embed.dim,
            "url": st.cfg.kb.embed.url,
            "pending": counts.0,
            "done": counts.1,
            "failed": counts.2,
            "skipped": counts.3,
        }
    })))
}
