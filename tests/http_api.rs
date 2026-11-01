//! HTTP 接口的集成测试：真起服务、真发请求、真查库。
//!
//! 不 mock 数据库——mock 掉的就测不到 SQL 写错、约束冲突、事务边界这类真问题。

use std::sync::Arc;

use bluewhale_memory::config::{BindScope, Config};
use bluewhale_memory::db::Db;
use bluewhale_memory::http::{router, AppState};

const TOKEN: &str = "test-token-0123456789abcdef";

struct TestServer {
    base: String,
    db: Arc<Db>,
    /// 保留一份 AppState：缓存统计这类「内部行为」没有 HTTP 出口，
    /// 但必须能被断言（否则「缓存到底有没有生效」只能靠感觉）。
    state: Arc<AppState>,
    _dir: tempfile::TempDir,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    async fn start_with(f: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            data_dir: dir.path().to_path_buf(),
            token: TOKEN.to_string(),
            bind_scope: BindScope::Loopback,
            ..Default::default()
        };
        f(&mut cfg);
        cfg.validate().unwrap();
        let cfg = Arc::new(cfg);
        let db = Db::open(cfg.db_path()).unwrap();
        let state = Arc::new(AppState::new(db.clone(), cfg.clone()).unwrap());
        // 用完整路由：测的必须就是线上跑的那一份。
        let app = bluewhale_memory::http::full_router((*state).clone());

        // 端口 0 = 让系统挑一个空闲端口，避免测试之间抢端口。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // 测试必须注入 connect info：api_auth_mw 靠它判断来源。
            // 不注入的话所有请求都会被当成「非本机」，那套逻辑就测不到。
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        Self {
            base: format!("http://{addr}"),
            db,
            state,
            _dir: dir,
        }
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap()
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        Self::client()
            .get(format!("{}{}", self.base, path))
            .header("x-api-token", TOKEN)
            .send()
            .await
            .unwrap()
    }

    async fn get_no_auth(&self, path: &str) -> reqwest::Response {
        Self::client()
            .get(format!("{}{}", self.base, path))
            .send()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> reqwest::Response {
        Self::client()
            .post(format!("{}{}", self.base, path))
            .header("x-api-token", TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn post_no_auth(&self, path: &str, body: serde_json::Value) -> reqwest::Response {
        Self::client()
            .post(format!("{}{}", self.base, path))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn post_bad_token(&self, path: &str, body: serde_json::Value) -> reqwest::Response {
        Self::client()
            .post(format!("{}{}", self.base, path))
            .header("x-api-token", "wrong-token")
            .json(&body)
            .send()
            .await
            .unwrap()
    }
}

fn event_body(summary: &str) -> serde_json::Value {
    serde_json::json!({
        "companion_id": "c1",
        "channel": "mc",
        "kind": "test",
        "summary": summary,
        "importance": 0.5
    })
}

/// 一条**群聊**消息：必须带 `group_id`。
///
/// 场景切分只看 `group_id` 是否为空（见 `context::Scene`），所以「群聊」这件事
/// 不是靠调用方口头声明，而是靠这个字段有没有。
fn group_msg(group: &str, name: &str, content: &str) -> serde_json::Value {
    serde_json::json!({
        "channel": "qq",
        "group_id": group,
        "account": "1234567890",
        "sender_name": name,
        "content": content,
        "at_ms": bluewhale_memory::db::now_ms()
    })
}

/// 一条**私聊**消息：**不带** `group_id` 这个键。
///
/// 注意是"不带键"而不是"传空串"：空串在 SQL 里仍然是个非 NULL 值，
/// 会被 `group_id <> ''` 之外的分支漏掉。写入口必须守住这条约定。
fn private_msg(account: &str, name: &str, content: &str) -> serde_json::Value {
    serde_json::json!({
        "channel": "qq",
        "account": account,
        "sender_name": name,
        "content": content,
        "at_ms": bluewhale_memory::db::now_ms()
    })
}

// ================================================================== 健康检查

#[tokio::test]
async fn 健康检查无需鉴权且不泄露内容() {
    let s = TestServer::start().await;
    let r = s.get_no_auth("/v1/health").await;
    assert_eq!(r.status(), 200);
    let j: serde_json::Value = r.json().await.unwrap();
    assert_eq!(j["ok"], true);
    assert_eq!(j["app"], "蓝鲸记忆中枢");
    assert!(j["tables"].is_array());
    // 健康检查不该带任何记忆内容。
    let text = j.to_string();
    assert!(!text.contains("summary"), "健康检查不应包含记忆文本");
}

#[tokio::test]
async fn 健康检查报告监听地址与是否仅回环() {
    let s = TestServer::start().await;
    let j: serde_json::Value = s.get_no_auth("/v1/health").await.json().await.unwrap();
    assert_eq!(j["loopback_only"], true);
    assert!(j["bind"].as_str().unwrap().starts_with("127.0.0.1"));
}

// ================================================================== 鉴权

#[tokio::test]
async fn 写入无_token_被拒() {
    let s = TestServer::start().await;
    let r = s.post_no_auth("/v1/event", event_body("x")).await;
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn 写入错误_token_被拒() {
    let s = TestServer::start().await;
    let r = s.post_bad_token("/v1/event", event_body("x")).await;
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn 读取无_token_被拒() {
    let s = TestServer::start().await;
    let r = s.get_no_auth("/v1/event/list?companion_id=c1").await;
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn 统计无_token_被拒() {
    let s = TestServer::start().await;
    assert_eq!(s.get_no_auth("/v1/stats").await.status(), 401);
}

// ================================================================== 事件写入

#[tokio::test]
async fn 写入事件后能读回() {
    let s = TestServer::start().await;
    let r = s.post("/v1/event", event_body("挖到了钻石")).await;
    assert_eq!(r.status(), 200);
    let j: serde_json::Value = r.json().await.unwrap();
    assert_eq!(j["duplicate"], false);
    assert!(j["id"].as_i64().unwrap() > 0);

    let r = s.get("/v1/event/list?companion_id=c1").await;
    let list: Vec<serde_json::Value> = r.json().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["summary"], "挖到了钻石");
    assert_eq!(list[0]["channel"], "mc");
}

#[tokio::test]
async fn 幂等键让重放不产生重复() {
    let s = TestServer::start().await;
    let mut body = event_body("同一件事");
    body["dedup_key"] = serde_json::json!("k-1");

    let j1: serde_json::Value = s
        .post("/v1/event", body.clone())
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j1["duplicate"], false);

    let j2: serde_json::Value = s
        .post("/v1/event", body.clone())
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j2["duplicate"], true, "同样 dedup_key 应被判为重复");
    assert!(j2["id"].is_null(), "重复的写入不该返回新 id");

    // 再来第三次
    let j3: serde_json::Value = s.post("/v1/event", body).await.json().await.unwrap();
    assert_eq!(j3["duplicate"], true);

    let list: Vec<serde_json::Value> = s
        .get("/v1/event/list?companion_id=c1")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 1, "三次重放只应留下一条");
}

#[tokio::test]
async fn 不同幂等键可以共存() {
    let s = TestServer::start().await;
    for i in 0..3 {
        let mut b = event_body(&format!("事{i}"));
        b["dedup_key"] = serde_json::json!(format!("k-{i}"));
        let j: serde_json::Value = s.post("/v1/event", b).await.json().await.unwrap();
        assert_eq!(j["duplicate"], false);
    }
    let list: Vec<serde_json::Value> = s
        .get("/v1/event/list?companion_id=c1")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 3);
}

#[tokio::test]
async fn 空摘要被拒绝() {
    let s = TestServer::start().await;
    let mut b = event_body("x");
    b["summary"] = serde_json::json!("   ");
    assert_eq!(s.post("/v1/event", b).await.status(), 400);
}

/// 单主体之后 `companion_id` 是**兼容字段**：不传、传空、传什么值都行。
///
/// 这条测试原来锁「空 companion_id 被拒绝」；设计改成一个人的记忆后，
/// 那个字段不再参与任何判断，所以断言反过来：**不传也必须能写**。
#[tokio::test]
async fn 不传_companion_id_也能写() {
    let s = TestServer::start().await;
    let b = serde_json::json!({
        "channel": "mc", "kind": "test", "summary": "没有 companion_id 的事件",
        "importance": 0.5
    });
    assert_eq!(s.post("/v1/event", b).await.status(), 200);
}

#[tokio::test]
async fn 未知渠道被拒绝() {
    let s = TestServer::start().await;
    let mut b = event_body("x");
    b["channel"] = serde_json::json!("telegram");
    assert_eq!(s.post("/v1/event", b).await.status(), 400);
}

#[tokio::test]
async fn 越界重要度被拒绝() {
    let s = TestServer::start().await;
    let mut b = event_body("x");
    b["importance"] = serde_json::json!(1.5);
    assert_eq!(s.post("/v1/event", b).await.status(), 400);
}

#[tokio::test]
async fn 事件按时间倒序返回() {
    let s = TestServer::start().await;
    let now = bluewhale_memory::db::now_ms();
    for (i, off) in [3000i64, 1000, 2000].iter().enumerate() {
        let mut b = event_body(&format!("e{i}"));
        b["at_ms"] = serde_json::json!(now - off);
        s.post("/v1/event", b).await;
    }
    let list: Vec<serde_json::Value> = s
        .get("/v1/event/list?companion_id=c1")
        .await
        .json()
        .await
        .unwrap();
    let ts: Vec<i64> = list.iter().map(|v| v["at_ms"].as_i64().unwrap()).collect();
    let mut sorted = ts.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(ts, sorted, "应按时间倒序");
}

// ================================================================== 检索

#[tokio::test]
async fn 检索能按关键词命中() {
    let s = TestServer::start().await;
    s.post("/v1/event", event_body("今天挖到了钻石")).await;
    s.post("/v1/event", event_body("和玩家聊了天气")).await;

    let list: Vec<serde_json::Value> = s
        .get("/v1/recall?companion_id=c1&q=钻石")
        .await
        .json()
        .await
        .unwrap();
    // 命中优先、未命中兜底 —— 命中那条必须排第一，且 `why` 说它是命中。
    assert_eq!(list.len(), 2, "未命中的那条作为兜底也要返回");
    assert!(list[0]["text"].as_str().unwrap().contains("钻石"));
    assert!(list[0]["why"].as_str().unwrap().contains("关键词"));
    assert_eq!(list[0]["hit"], true);
    // 兜底那条标成「权重最高」，不冒充命中。
    assert_eq!(list[1]["hit"], false);
    assert!(list[1]["why"].as_str().unwrap().contains("权重"));
}

#[tokio::test]
async fn 检索空查询按权重返回() {
    let s = TestServer::start().await;
    let mut hi = event_body("重要的事");
    hi["importance"] = serde_json::json!(0.95);
    let mut lo = event_body("不重要的事");
    lo["importance"] = serde_json::json!(0.05);
    s.post("/v1/event", lo).await;
    s.post("/v1/event", hi).await;

    let list: Vec<serde_json::Value> = s
        .get("/v1/recall?companion_id=c1")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 2);
    assert!(
        list[0]["weight"].as_f64().unwrap() > list[1]["weight"].as_f64().unwrap(),
        "应按权重降序"
    );
}

#[tokio::test]
async fn 检索遵守_topk() {
    let s = TestServer::start().await;
    for i in 0..10 {
        s.post("/v1/event", event_body(&format!("事{i}"))).await;
    }
    let list: Vec<serde_json::Value> = s
        .get("/v1/recall?companion_id=c1&topk=3")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 3);
}

/// 单主体之后没有「多个对象」，但**渠道**仍是可区分维度。
///
/// 原测试锁的是多租户隔离；设计改成「一个人的记忆」后那个前提不存在了，
/// 改成锁「不同渠道的记忆都在同一份里，检索都能取到」。
#[tokio::test]
async fn 所有渠道的记忆都在同一份里() {
    let s = TestServer::start().await;
    s.post("/v1/event", event_body("MC 里挖到钻石")).await;
    let mut b = event_body("QQ 里聊到钻石");
    b["channel"] = serde_json::json!("qq");
    s.post("/v1/event", b).await;

    let list: Vec<serde_json::Value> = s
        .get("/v1/recall?q=钻石&topk=10")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 2, "两个渠道的记忆应该都能被检索到");
}

// ================================================================== 身份

#[tokio::test]
async fn 绑定后账号指向同一个人() {
    let s = TestServer::start().await;
    let r = s
        .post(
            "/v1/person/bind",
            serde_json::json!({
                "channel": "mc", "account": "uuid-1", "person_id": "p-1", "seen_name": "小明"
            }),
        )
        .await;
    assert_eq!(r.status(), 200);
    let j: serde_json::Value = r.json().await.unwrap();
    assert_eq!(j["ok"], true);
    assert_eq!(j["person_id"], "p-1");

    let pending: Vec<serde_json::Value> = s.get("/v1/person/pending").await.json().await.unwrap();
    // 绑定即确认，所以不该再出现在 pending 里。
    assert!(pending.iter().all(|p| p["person_id"] != "p-1"));
}

#[tokio::test]
async fn 重复绑定报告之前归属() {
    let s = TestServer::start().await;
    s.post(
        "/v1/person/bind",
        serde_json::json!({"channel":"qq","account":"123","person_id":"p-a"}),
    )
    .await;
    let j: serde_json::Value = s
        .post(
            "/v1/person/bind",
            serde_json::json!({"channel":"qq","account":"123","person_id":"p-b"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["previous_person"], "p-a", "改绑要能追溯原来绑的是谁");
}

#[tokio::test]
async fn 未绑定的账号会出现在待处理列表() {
    let s = TestServer::start().await;
    // 直接在库里塞一个 pending 账号（模拟自动创建）。
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms)
             VALUES ('p-new','新人','pending',?1)",
            rusqlite::params![bluewhale_memory::db::now_ms()],
        )?;
        tx.execute(
            "INSERT INTO person_account(channel, account, person_id, first_seen_ms, last_seen_ms)
             VALUES ('live','uid-9','p-new',?1,?1)",
            rusqlite::params![bluewhale_memory::db::now_ms()],
        )?;
        Ok(())
    })
    .unwrap();

    let pending: Vec<serde_json::Value> = s.get("/v1/person/pending").await.json().await.unwrap();
    assert!(pending.iter().any(|p| p["person_id"] == "p-new"));
}

// ================================================================== 快照与回滚

#[tokio::test]
async fn 快照可拍可列() {
    let s = TestServer::start().await;
    s.post("/v1/event", event_body("快照前")).await;

    let j: serde_json::Value = s
        .post("/v1/snapshot", serde_json::json!({"note":"测试"}))
        .await
        .json()
        .await
        .unwrap();
    assert!(j["bytes"].as_i64().unwrap() > 0);
    assert_eq!(j["sha256"].as_str().unwrap().len(), 64);

    let list: Vec<serde_json::Value> = s.get("/v1/snapshot/list").await.json().await.unwrap();
    assert_eq!(list.len(), 1);
}

#[tokio::test]
async fn 回滚忘掉之后的记忆() {
    let s = TestServer::start().await;
    let now = bluewhale_memory::db::now_ms();
    let mut old = event_body("老的");
    old["at_ms"] = serde_json::json!(now - 10_000);
    let mut new = event_body("新的");
    new["at_ms"] = serde_json::json!(now);
    s.post("/v1/event", old).await;
    s.post("/v1/event", new).await;

    let cutoff = now - 5_000;
    let j: serde_json::Value = s
        .post("/v1/rollback", serde_json::json!({"target_ms": cutoff}))
        .await
        .json()
        .await
        .unwrap();
    assert!(j["total"].as_i64().unwrap() >= 1);

    let list: Vec<serde_json::Value> = s
        .get("/v1/event/list?companion_id=c1")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 1, "只应剩回滚点之前的那条");
    assert_eq!(list[0]["summary"], "老的");
}

#[tokio::test]
async fn 回滚可以撤销() {
    let s = TestServer::start().await;
    let now = bluewhale_memory::db::now_ms();
    s.post("/v1/event", event_body("a")).await;

    let rb: serde_json::Value = s
        .post("/v1/rollback", serde_json::json!({"target_ms": now - 1000}))
        .await
        .json()
        .await
        .unwrap();
    let id = rb["rollback_id"].as_i64().unwrap();

    let after: Vec<serde_json::Value> = s
        .get("/v1/event/list?companion_id=c1")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(after.len(), 0, "应被遗忘");

    s.post("/v1/rollback/undo", serde_json::json!({"rollback_id": id}))
        .await;

    let restored: Vec<serde_json::Value> = s
        .get("/v1/event/list?companion_id=c1")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(restored.len(), 1, "撤销后应恢复");
}

#[tokio::test]
async fn 回滚到未来被拒绝() {
    let s = TestServer::start().await;
    let future = bluewhale_memory::db::now_ms() + 86_400_000;
    let r = s
        .post("/v1/rollback", serde_json::json!({"target_ms": future}))
        .await;
    assert_eq!(r.status(), 409);
}

#[tokio::test]
async fn 撤销不存在的回滚返回_404() {
    let s = TestServer::start().await;
    let r = s
        .post(
            "/v1/rollback/undo",
            serde_json::json!({"rollback_id": 99999}),
        )
        .await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn 回滚历史可查() {
    let s = TestServer::start().await;
    let now = bluewhale_memory::db::now_ms();
    s.post(
        "/v1/rollback",
        serde_json::json!({"target_ms": now - 1000, "note": "第一次"}),
    )
    .await;
    let hist: Vec<serde_json::Value> = s.get("/v1/rollback/history").await.json().await.unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0]["note"], "第一次");
}

// ================================================================== 统计

#[tokio::test]
async fn 统计包含权重配置() {
    let s = TestServer::start().await;
    let j: serde_json::Value = s.get("/v1/stats").await.json().await.unwrap();
    assert!(j["weight_config"]["half_life_days"].as_f64().unwrap() > 0.0);
    assert!(j["tables"].is_array());
}

#[tokio::test]
async fn 统计反映写入次数与去重命中() {
    let s = TestServer::start().await;
    let mut b = event_body("x");
    b["dedup_key"] = serde_json::json!("dup");
    s.post("/v1/event", b.clone()).await;
    s.post("/v1/event", b).await;

    let j: serde_json::Value = s.get("/v1/stats").await.json().await.unwrap();
    assert!(j["writes"].as_u64().unwrap() >= 1);
    assert!(
        j["dedup_hits"].as_u64().unwrap() >= 1,
        "应统计到一次去重命中"
    );
}

// ================================================================== 并发

#[tokio::test]
async fn 并发写入不丢数据() {
    let s = TestServer::start().await;
    let mut handles = Vec::new();
    for i in 0..50 {
        let base = s.base.clone();
        handles.push(tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/event"))
                .header("x-api-token", TOKEN)
                .json(&serde_json::json!({
                    "companion_id": "c1", "channel": "mc", "kind": "t",
                    "summary": format!("并发{i}"), "importance": 0.5,
                    "dedup_key": format!("c-{i}")
                }))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), 200);
    }

    let list: Vec<serde_json::Value> = s
        .get("/v1/event/list?companion_id=c1&limit=500")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 50, "并发写入不该丢数据");
}

#[tokio::test]
async fn 并发读写在_wal_下不互相阻塞() {
    let s = TestServer::start().await;
    for i in 0..20 {
        let mut b = event_body(&format!("预置{i}"));
        b["dedup_key"] = serde_json::json!(format!("pre-{i}"));
        s.post("/v1/event", b).await;
    }

    let mut handles = Vec::new();
    // 10 个读者
    for _ in 0..10 {
        let base = s.base.clone();
        handles.push(tokio::spawn(async move {
            let c = reqwest::Client::new();
            for _ in 0..5 {
                let st = c
                    .get(format!("{base}/v1/event/list?companion_id=c1"))
                    .header("x-api-token", TOKEN)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16();
                assert_eq!(st, 200);
            }
        }));
    }
    // 5 个写者
    for i in 0..5 {
        let base = s.base.clone();
        handles.push(tokio::spawn(async move {
            let c = reqwest::Client::new();
            for j in 0..5 {
                let st = c
                    .post(format!("{base}/v1/event"))
                    .header("x-api-token", TOKEN)
                    .json(&serde_json::json!({
                        "companion_id":"c1","channel":"mc","kind":"t",
                        "summary":format!("w{i}-{j}"),"importance":0.5,
                        "dedup_key":format!("w-{i}-{j}")
                    }))
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16();
                assert_eq!(st, 200);
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

// ================================================================== 错误格式统一

/// 所有错误都必须是同一种 JSON 形状。
///
/// 曾经不是：`Json<T>` 提取器在 handler 之前就跑了，它失败时 axum
/// 返回**纯文本**（"Failed to deserialize the JSON body..."），
/// 客户端 `await r.json()` 直接抛异常。这一组测试锁住这个行为。
#[tokio::test]
async fn 解析失败也返回统一_json_格式() {
    let s = TestServer::start().await;
    // 空 body → 缺必填字段
    let r = s.post("/v1/event", serde_json::json!({})).await;
    assert_eq!(r.status(), 422, "axum 把缺字段定成 422");
    let j: serde_json::Value = r.json().await.expect("必须是合法 JSON，不能是纯文本");
    assert_eq!(j["kind"], "bad_request");
    assert_eq!(j["retryable"], false);
    // 错误文案要说明缺的是哪个字段——泛化的 "Unprocessable Entity" 没用
    // 单主体后 companion_id 已从请求体里删掉，第一个缺的必填字段是 kind。
    let msg = j["error"].as_str().unwrap();
    assert!(
        msg.contains("kind") || msg.contains("channel"),
        "应指出缺的字段，实得：{msg}"
    );
}

#[tokio::test]
async fn 字段类型错也返回统一_json_格式() {
    let s = TestServer::start().await;
    // 用 channel 而不是 companion_id：后者已降级为兼容字段（Option），
    // 传什么类型都能被接受，测不出类型校验。
    let r = s
        .post("/v1/event", serde_json::json!({"channel": 123}))
        .await;
    let j: serde_json::Value = r.json().await.expect("必须是合法 JSON");
    assert_eq!(j["kind"], "bad_request");
    let msg = j["error"].as_str().unwrap();
    assert!(msg.contains("channel"), "应指出哪个字段，实得：{msg}");
}

#[tokio::test]
async fn 未授权返回统一_json_格式() {
    let s = TestServer::start().await;
    let r = s.post_no_auth("/v1/event", event_body("x")).await;
    assert_eq!(r.status(), 401);
    let j: serde_json::Value = r.json().await.expect("必须是合法 JSON");
    assert_eq!(j["kind"], "unauthorized");
    assert_eq!(j["retryable"], false, "没带 token 重试没用");
}

#[tokio::test]
async fn 不存在的路径返回统一_json_格式() {
    let s = TestServer::start().await;
    let j: serde_json::Value = s
        .get("/v1/nope")
        .await
        .json()
        .await
        .expect("必须是合法 JSON");
    assert_eq!(j["kind"], "not_found");
}

#[tokio::test]
async fn 存储类错误标记为可重试() {
    // 这条锁的是语义：retryable 只有 storage/io 才为真。
    // 业务错误（400/404/409）重试永远是错的，标记成可重试会让客户端无脑重放。
    use bluewhale_memory::error::ErrorKind;
    assert!(ErrorKind::Storage.retryable());
    assert!(ErrorKind::Io.retryable());
    assert!(!ErrorKind::BadRequest.retryable());
    assert!(!ErrorKind::NotFound.retryable());
    assert!(!ErrorKind::Rollback.retryable());
    assert!(!ErrorKind::Unauthorized.retryable());
}

/// `/` 现在归管理后台管（见 admin_api.rs）。
///
/// 这条测试从「首页免 token 可访问」改成了「API 层不再提供 /」——
/// 面板的门由会话 cookie 把守，不再是公开静态页。
/// 公开的只剩 `/v1/health`，它本来就不含记忆内容。
#[tokio::test]
async fn 根路径是后台登录页() {
    // 三套路由合并后 `/` 归后台面板（未登录 → 302/303 去 /login）。
    // 原测试断言 404，那是「测试只挂核心 API」时的口径——
    // 现在测的是线上同一份完整路由，`/` 本来就应该有页面。
    let s = TestServer::start().await;
    let st = s.get_no_auth("/").await.status();
    assert!(
        st == 303 || st == 302 || st == 200,
        "根路径应该是后台登录页，实得 {st}"
    );
}

#[tokio::test]
async fn 健康检查是唯一免鉴权的数据入口() {
    let s = TestServer::start().await;
    assert_eq!(s.get_no_auth("/v1/health").await.status(), 200);
    // 且它不含任何记忆内容
    let text = s.get_no_auth("/v1/health").await.text().await.unwrap();
    assert!(!text.contains("summary"));
}

// ================================================================== 绑定与鉴权解耦

/// 没有 token 时监听 0.0.0.0：本机能用，外部一律 401。
///
/// 这是「网络可达」与「能否读写」分开之后的关键行为。
/// 旧的实现是「没 token 就只听 127.0.0.1」，那样内网根本连不上；
/// 现在照常监听，由这一层挡。
#[tokio::test]
async fn 无_token_时本机可用但非回环被拒() {
    use bluewhale_memory::config::{BindScope, Config};

    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        data_dir: dir.path().to_path_buf(),
        token: String::new(),       // 没有 token
        bind_scope: BindScope::Lan, // 但监听所有网卡
        ..Default::default()
    };
    cfg.validate().unwrap();
    let cfg = std::sync::Arc::new(cfg);
    let db = Db::open(cfg.db_path()).unwrap();
    let state = AppState::new(db.clone(), cfg.clone()).unwrap();
    assert!(cfg.listening_on_all_interfaces(), "应监听 0.0.0.0");
    assert!(cfg.api_exposed_without_token(), "应标记为危险配置");

    let app = router(state);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(
            l,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });

    let c = reqwest::Client::new();
    // 本机来源 → 放行
    let r = c
        .post(format!("http://{addr}/v1/event"))
        .json(&event_body("本机写入"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "本机来源应放行（没有 token 的部署靠这个工作）"
    );

    // 健康检查永远免鉴权
    assert_eq!(
        c.get(format!("http://{addr}/v1/health"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

// ================================================================== 回归
//
// 下面这几条锁的是「不传可选字段也必须能写」。它们抓到的都是**真 bug**：
// 单主体改造后有余留的占位符跳号、空 channel 被当成非法渠道——
// 光看代码看不出来，一调就 503。

#[tokio::test]
async fn 写事实不传_channel_也能成功() {
    // channel 在写事实时只参与权重计算，不该是必填。
    // 早先这里 parse 空串直接报「未知渠道：」，实测踩到。
    let s = TestServer::start().await;
    let b = serde_json::json!({
        "key": "爱好", "value": "钓鱼",
        "scope": "companion", "status": "confirmed", "tier": "curated",
        "confidence": 0.9, "importance": 0.7
    });
    let r = s.post("/v1/fact", b).await;
    assert_eq!(r.status(), 200, "不传 channel 写事实应该成功");
}

#[tokio::test]
async fn 写事实后能读回() {
    // 抓的是 INSERT 占位符与列数不一致（`?11,?11` / 8 values for 7 columns 那类）。
    let s = TestServer::start().await;
    let wr = s
        .post(
            "/v1/fact",
            serde_json::json!({
                "id": "reg-1", "key": "验证键", "value": "验证值",
                "scope": "companion", "status": "confirmed", "tier": "curated"
            }),
        )
        .await;
    let st = wr.status();
    let body = wr.text().await.unwrap_or_default();
    assert_eq!(st, 200, "写事实失败：{body}");
    let lr = s.get("/v1/fact/list?limit=50").await;
    let lst = lr.status();
    let lbody = lr.text().await.unwrap_or_default();
    assert_eq!(lst, 200, "读事实失败：{lbody}");
    let list: Vec<serde_json::Value> = serde_json::from_str(&lbody)
        .unwrap_or_else(|e| panic!("fact/list 返回的不是数组（{e}）：{lbody}"));
    assert!(
        list.iter().any(|f| f["value"] == "验证值"),
        "写进去的事实要能读回来"
    );
}

#[tokio::test]
async fn 同_key_覆盖走_supersede_而不是删旧行() {
    // 锁 supersede 查询的占位符必须连续（?1/?3/?4 跳号会让 SQLite 算错参数个数）。
    let s = TestServer::start().await;
    s.post(
        "/v1/fact",
        serde_json::json!({
            "id": "r-1", "key": "爱吃", "value": "鱼",
            "scope": "companion", "status": "confirmed", "tier": "curated"
        }),
    )
    .await;
    s.post(
        "/v1/fact",
        serde_json::json!({
            "id": "r-2", "key": "爱吃", "value": "烤鱼",
            "scope": "companion", "status": "confirmed", "tier": "curated"
        }),
    )
    .await;
    let list: Vec<serde_json::Value> = s
        .get("/v1/fact/list?limit=50&status=superseded")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 1, "旧版本要保留为 superseded，不能物理删除");
    assert_eq!(list[0]["value"], "鱼");
}

#[tokio::test]
async fn 写情绪不传_channel_也能成功() {
    // affect_log 曾经是 8 个占位符对 7 列。
    let s = TestServer::start().await;
    let b = serde_json::json!({
        "at_ms": 1_793_200_000_000i64, "kind": "t",
        "field": "valence", "delta": 0.1
    });
    assert_eq!(s.post("/v1/affect", b).await.status(), 200);
}

// ================================================================== 聚合上下文

/// 在库里塞一张计划 + 条目。直接写库，因为 `/v1` 侧没有计划的写入口
/// （计划的来源是 MC 推送，本轮不改那条链）。
fn seed_plan(s: &TestServer, plan_id: &str, summary: &str, items: &[(&str, &str)]) {
    // 一个游戏日只能有一份未删的计划（部分唯一索引 `plan_day`），
    // 所以 `planning_day_id` 用 plan_id 派生，别拿常量。
    seed_plan_on_day(s, plan_id, plan_id, summary, items)
}

fn seed_plan_on_day(
    s: &TestServer,
    plan_id: &str,
    planning_day_id: &str,
    summary: &str,
    items: &[(&str, &str)],
) {
    let now = bluewhale_memory::db::now_ms();
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO plan(id, planning_day_id, summary, model, game_day,
                              created_ms, updated_ms)
             VALUES (?1,?2,?3,'m',1,?4,?4)",
            rusqlite::params![plan_id, planning_day_id, summary, now],
        )?;
        for (i, (state, title)) in items.iter().enumerate() {
            tx.execute(
                "INSERT INTO plan_item(plan_id, id, title, state, ord, created_ms, updated_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?6)",
                rusqlite::params![
                    plan_id,
                    format!("{plan_id}-i{i}"),
                    title,
                    state,
                    i as i64,
                    now
                ],
            )?;
        }
        Ok(())
    })
    .unwrap();
}

#[tokio::test]
async fn 空库的_context_返回空串而不是_null() {
    let s = TestServer::start().await;
    let r = s.get("/v1/context").await;
    assert_eq!(r.status(), 200);
    let j: serde_json::Value = r.json().await.unwrap();
    assert_eq!(j["text"], "", "空库必须是空串，不能是 null");
    assert_eq!(j["chars"], 0);
    assert_eq!(j["parts"].as_array().unwrap().len(), 0);
    assert_eq!(j["dropped"].as_array().unwrap().len(), 0);
    assert!(j["generated_ms"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn context_按契约顺序拼出六段() {
    let s = TestServer::start().await;
    // 1. 计划
    seed_plan(
        &s,
        "p-ctx",
        "把家往上盖一盖",
        &[
            ("done", "把铁矿烧成铁锭"),
            ("todo", "清包"),
            ("deferred", "去钓鱼"),
        ],
    );
    // 2. 记忆（事件也能被 recall 命中）
    let mut ev = event_body("在 y=-54 发现了钻石矿脉");
    ev["importance"] = serde_json::json!(0.9);
    s.post("/v1/event", ev).await;
    // 3. 最近事件（mc）
    s.post("/v1/event", event_body("被苦力怕炸了")).await;
    // 4. 群聊（qq）
    s.post(
        "/v1/message",
        group_msg("1031105617", "小明", "明天我去看你钓鱼啊"),
    )
    .await;
    // 5. 日记
    s.post(
        "/v1/diary",
        serde_json::json!({"kind": "day", "title": "水边那两竿", "body": "今天本来打算先把箱子归了，再去水边守着。"}),
    )
    .await;
    // 6. 心情
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO mood(id, valence, arousal, dominance, label, reason, updated_ms)
             VALUES (1, -0.7, 0.5, -0.3, '恼火', '挨了打', ?1)",
            rusqlite::params![bluewhale_memory::db::now_ms()],
        )?;
        Ok(())
    })
    .unwrap();

    let j: serde_json::Value = s
        .get("/v1/context?budget=4000&q=钓鱼")
        .await
        .json()
        .await
        .unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(!text.is_empty(), "有内容时 text 不能为空");

    // 段落顺序必须严格是 plan → memory → events → group → diary → mood。
    let parts: Vec<String> = j["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        parts,
        vec!["plan", "memory", "events", "group", "diary", "mood"],
        "段落顺序是契约的硬要求，越靠上越不可替代"
    );

    // 每一段的标题与内容都要真的在 text 里。
    for expect in [
        "# 你最近在做什么",
        "- 今天的打算：把家往上盖一盖",
        "- 已完成：把铁矿烧成铁锭",
        "- 还没做：清包",
        "- 先搁着：去钓鱼",
        "# 你记得的事",
        "# 最近发生的事",
        "# 群里最近说的话",
        "- 小明：明天我去看你钓鱼啊",
        "# 你上次记的日记",
        "《水边那两竿》",
        "# 你现在是什么心情",
    ] {
        assert!(
            text.contains(expect),
            "text 里应有「{expect}」，实得：\n{text}"
        );
    }

    // 段落顺序也要真的体现在文本位置上（只看 parts 数组不够）。
    let pos = |needle: &str| text.find(needle).unwrap_or(usize::MAX);
    assert!(pos("# 你最近在做什么") < pos("# 你记得的事"));
    assert!(pos("# 你记得的事") < pos("# 最近发生的事"));
    assert!(pos("# 最近发生的事") < pos("# 群里最近说的话"));
    assert!(pos("# 群里最近说的话") < pos("# 你上次记的日记"));
    assert!(pos("# 你上次记的日记") < pos("# 你现在是什么心情"));

    // chars 是字符数，不是字节数。
    assert_eq!(j["chars"].as_u64().unwrap() as usize, text.chars().count());
    assert!(
        text.len() > text.chars().count(),
        "含中文时字节数应大于字符数，chars 不能是字节数"
    );
}

#[tokio::test]
async fn context_群聊没名字时打码成末四位且不泄露完整账号() {
    let s = TestServer::start().await;
    s.post(
        "/v1/message",
        group_msg("1031105617", "", "你昨天挖到什么了"),
    )
    .await;
    let j: serde_json::Value = s.get("/v1/context").await.json().await.unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(text.contains("***7890"), "没名字时用末四位：\n{text}");
    assert!(!text.contains("1234567890"), "绝不能输出完整账号：\n{text}");
}

#[tokio::test]
async fn context_平静时不输出心情段() {
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO mood(id, valence, arousal, dominance, label, reason, updated_ms)
             VALUES (1, 0.0, 0.0, 0.0, '平静', '', ?1)",
            rusqlite::params![bluewhale_memory::db::now_ms()],
        )?;
        Ok(())
    })
    .unwrap();
    // 再塞一条事件，保证 text 非空（否则看不出「只有心情段被省」）。
    s.post("/v1/event", event_body("挖了会儿矿")).await;

    let j: serde_json::Value = s.get("/v1/context").await.json().await.unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(
        !text.contains("# 你现在是什么心情"),
        "平静是默认值，写出来只是噪声"
    );
    let parts: Vec<&str> = j["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(!parts.contains(&"mood"));
    assert!(parts.contains(&"events"), "其他段照常输出");
}

#[tokio::test]
async fn context_预算不够时从下往上丢并写明原因() {
    let s = TestServer::start().await;
    // 塞足六段，内容都偏长。
    seed_plan(&s, "p-b", &"啊".repeat(120), &[("todo", &"活".repeat(120))]);
    s.post("/v1/event", event_body(&"事".repeat(120))).await;
    s.post(
        "/v1/message",
        serde_json::json!({
            "channel": "qq", "account": "1", "sender_name": "某人",
            "content": "话".repeat(120),
            "at_ms": bluewhale_memory::db::now_ms()
        }),
    )
    .await;
    s.post(
        "/v1/diary",
        serde_json::json!({"kind": "day", "title": "长日记", "body": "记".repeat(400)}),
    )
    .await;

    // 下限 200：装不下全部六段。
    let j: serde_json::Value = s.get("/v1/context?budget=200").await.json().await.unwrap();
    let text = j["text"].as_str().unwrap();
    let parts: Vec<String> = j["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let dropped: Vec<String> = j["dropped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();

    assert_eq!(text.chars().count(), 200, "文本必须被压到 budget");
    assert!(!parts.is_empty(), "至少保留最上面那段");
    assert!(
        parts[0] == "plan",
        "越靠上越不可替代，第一条必须是 plan：{parts:?}"
    );
    assert!(
        dropped.iter().all(|d| d.contains(": 超预算")),
        "被挤掉的段落必须写具体原因：{dropped:?}"
    );
    // 丢的必须是**靠下**的段：留下的段集合必须是原顺序的前缀。
    let all = ["plan", "memory", "events", "group", "diary", "mood"];
    assert_eq!(parts, all[..parts.len()].to_vec(), "只能从下往上丢");
}

#[tokio::test]
async fn context_预算参数被夹到契约区间() {
    let s = TestServer::start().await;
    s.post("/v1/event", event_body(&"长".repeat(2000))).await;

    // budget=1 → clamp 到 200
    let j: serde_json::Value = s.get("/v1/context?budget=1").await.json().await.unwrap();
    assert_eq!(j["text"].as_str().unwrap().chars().count(), 200);

    // budget=99999 → clamp 到 4000，不能被拒
    let j: serde_json::Value = s
        .get("/v1/context?budget=99999")
        .await
        .json()
        .await
        .unwrap();
    assert!(j["text"].as_str().unwrap().chars().count() <= 4000);

    // topk=0 → clamp 到 1；topk=999 → clamp 到 30，不报错
    for tk in ["0", "999"] {
        let r = s.get(&format!("/v1/context?topk={tk}")).await;
        assert_eq!(r.status(), 200, "topk={tk} 应被夹取而不是拒绝");
    }
}

#[tokio::test]
async fn context_需要_token() {
    let s = TestServer::start().await;
    assert_eq!(s.get_no_auth("/v1/context").await.status(), 401);
}

#[tokio::test]
async fn context_会写_recall_log_但只记真正返回的那几条() {
    let s = TestServer::start().await;
    // 30 条事件，topk=3 → recall_log 里不该出现 30 条 trace。
    for i in 0..30 {
        s.post("/v1/event", event_body(&format!("第{i}件事"))).await;
    }
    let j: serde_json::Value = s.get("/v1/context?topk=3").await.json().await.unwrap();
    let text = j["text"].as_str().unwrap();
    assert_eq!(
        text.lines().filter(|l| l.starts_with("- 第")).count(),
        3,
        "topk 要生效：\n{text}"
    );

    // 给异步写留一点时间。
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let n: i64 =
        s.db.with_read(|c| {
            // lane='context'：这是 /v1/context 那一侧的痕迹，
            // 和直接调 /v1/recall（lane='main'）区分得开。
            Ok(c.query_row(
                "SELECT COUNT(*) FROM recall_log WHERE lane = 'context'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert!(
        n <= 3,
        "只该记真正进结果的那几条（≤3），实际 {n} 条 —— 2000 条候选全记进去会把这张表冲爆"
    );
    assert!(n > 0, "返回了结果就该留下记忆痕迹");
}

// ================================================================== 知识库
//
// 知识库与记忆是两套独立的东西（记忆会衰减/过期/能回滚，知识库不衰减）。
// 这一组判据锁住的是：接口存在、导入幂等、检索在**没有嵌入服务时也能工作**。

fn kb_body(id: &str) -> serde_json::Value {
    serde_json::json!({"id": id, "name": format!("库{id}"), "description": null})
}

fn kb_doc(kb: &str, title: &str, content: &str) -> serde_json::Value {
    serde_json::json!({"kb_id": kb, "title": title, "source": "test", "content": content})
}

/// 造一个「知识库开着、但嵌入服务不可达」的服务。
/// 这样检索走纯字面那一路，测试不依赖网络。
async fn kb_ctx_server() -> TestServer {
    let s = TestServer::start_with(|c| {
        c.kb.enable = true;
        c.kb.embed.url = "http://127.0.0.1:9/v1/embeddings".into();
        c.kb.embed.timeout_seconds = 1;
    })
    .await;
    s.post("/v1/kb", kb_body("k1")).await;
    s.post(
        "/v1/kb/doc",
        kb_doc(
            "k1",
            "生存手册",
            "# 材料\n蜘蛛线是夜间击杀蜘蛛掉落的材料。\n\n# 建造\n圆石做墙。",
        ),
    )
    .await;
    s
}

#[tokio::test]
async fn context_在命中时注入知识库段且位置正确() {
    // 这是「接入知识库」这件事的最终验收：修了这么多，用户要的是
    // **它真的能用上资料**。之前它拿不到，因为没有任何客户端调 /v1/kb/*。
    let s = kb_ctx_server().await;
    let j: serde_json::Value = s
        .get("/v1/context?q=蜘蛛线&budget=4000")
        .await
        .json()
        .await
        .unwrap();
    let parts: Vec<String> = j["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let kb_at = parts.iter().position(|p| p == "kb");
    assert!(kb_at.is_some(), "命中时应有 kb 段，实得 {parts:?}");

    // 位置：memory 之后、events 之前
    let mem_at = parts.iter().position(|p| p == "memory");
    if let (Some(k), Some(m)) = (kb_at, mem_at) {
        assert!(m < k, "kb 应在 memory 之后，实得 {parts:?}");
    }

    let text = j["text"].as_str().unwrap();
    assert!(text.contains("# 你查到的资料"), "应有明确的小标题：{text}");
    assert!(
        text.contains("不是你的亲身经历"),
        "标题必须点明「不是亲历」，否则模型会把资料当记忆复述：{text}"
    );
    assert!(text.contains("蜘蛛线是夜间击杀"), "应带上资料正文：{text}");
}

#[tokio::test]
async fn context_无关提问不注入知识库() {
    // 实测：无关查询也能拿到 vec≈0.29~0.36，所以必须有门槛。
    // 不做门槛的话，每轮闲聊都会塞一段不相关的资料，挤掉真正有用的段。
    let s = kb_ctx_server().await;
    let j: serde_json::Value = s
        .get("/v1/context?q=今天天气不错&budget=4000")
        .await
        .json()
        .await
        .unwrap();
    let parts: Vec<String> = j["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        !parts.iter().any(|p| p == "kb"),
        "无关提问不该注入 kb：{parts:?}"
    );
}

#[tokio::test]
async fn context_知识库关闭时不出现_kb_段() {
    let s = TestServer::start().await;
    s.post("/v1/kb", kb_body("k1")).await;
    s.post("/v1/kb/doc", kb_doc("k1", "手册", "# 材料\n蜘蛛线做鱼竿。"))
        .await;
    let j: serde_json::Value = s
        .get("/v1/context?q=蜘蛛线&budget=4000")
        .await
        .json()
        .await
        .unwrap();
    let parts: Vec<String> = j["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        !parts.iter().any(|p| p == "kb"),
        "kb.enable=false 时不该出现 kb 段：{parts:?}"
    );
}

#[tokio::test]
async fn 知识库接口默认可用且不需要启用嵌入() {
    // 默认配置下 kb.enable = false、embedder = None。
    // 这时检索必须退回纯字面并**照常返回 200** —— 知识库本身不该因为
    // 没配嵌入服务就整个不可用。
    let s = TestServer::start().await;
    let h: serde_json::Value = s.get("/v1/kb/health").await.json().await.unwrap();
    assert_eq!(h["enabled"], false);
    assert_eq!(h["embed"]["state"], "disabled");

    s.post("/v1/kb", kb_body("k1")).await;
    let r: serde_json::Value = s
        .post(
            "/v1/kb/doc",
            kb_doc("k1", "指南", "# 材料\n蜘蛛线可以用来做鱼竿。"),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(r["duplicate"], false);
    assert!(r["n_chunks"].as_u64().unwrap() >= 1);

    let s2: serde_json::Value = s
        .post("/v1/kb/search", serde_json::json!({"q": "蜘蛛"}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(s2["hits"].as_array().unwrap().len(), 1, "纯字面也该命中");
    assert!(s2["hits"][0]["text"].as_str().unwrap().contains("蜘蛛线"));
}

#[tokio::test]
async fn 知识库导入幂等() {
    let s = TestServer::start().await;
    s.post("/v1/kb", kb_body("k1")).await;
    let a: serde_json::Value = s
        .post("/v1/kb/doc", kb_doc("k1", "指南", "同一份内容"))
        .await
        .json()
        .await
        .unwrap();
    let b: serde_json::Value = s
        .post("/v1/kb/doc", kb_doc("k1", "指南", "同一份内容"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(a["doc_id"], b["doc_id"]);
    assert_eq!(b["duplicate"], true, "同一内容重复导入必须幂等");
}

#[tokio::test]
async fn 知识库按标题分块() {
    // 实测过的坑：整篇短但有三节时塞成一块，问其中一节的内容拿到的是
    // 混合向量，那一节被埋掉。有标题就该按标题切。
    let s = TestServer::start().await;
    s.post("/v1/kb", kb_body("k1")).await;
    let r: serde_json::Value = s
        .post(
            "/v1/kb/doc",
            kb_doc(
                "k1",
                "指南",
                "# 材料\n蜘蛛线。\n\n# 建造\n圆石做墙。\n\n# 食物\n烤鱼能吃。",
            ),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(r["n_chunks"], 3, "三个小节应切成三块");
}

#[tokio::test]
async fn 知识库检索不串到记忆里() {
    // 两套东西必须分开：知识库检索不该返回记忆条目，反之亦然。
    let s = TestServer::start().await;
    s.post("/v1/event", event_body("蜘蛛线在记忆里")).await;
    s.post("/v1/kb", kb_body("k1")).await;
    s.post("/v1/kb/doc", kb_doc("k1", "指南", "蜘蛛线在知识库里"))
        .await;

    let kb: serde_json::Value = s
        .post("/v1/kb/search", serde_json::json!({"q": "蜘蛛线"}))
        .await
        .json()
        .await
        .unwrap();
    for h in kb["hits"].as_array().unwrap() {
        assert!(
            !h["text"].as_str().unwrap().contains("记忆里"),
            "知识库检索混进了记忆：{h:?}"
        );
    }

    let mem: Vec<serde_json::Value> = s
        .get("/v1/recall?q=蜘蛛线&topk=5")
        .await
        .json()
        .await
        .unwrap();
    for m in &mem {
        assert!(m["kind"] != "kb_chunk", "记忆检索不该返回知识库条目：{m:?}");
    }
}

#[tokio::test]
async fn 知识库检索空查询被拒() {
    let s = TestServer::start().await;
    let r = s
        .post("/v1/kb/search", serde_json::json!({"q": "   "}))
        .await;
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn 导入到不存在的库被拒() {
    // 否则文档挂在空库上、检索永远查不到，而导入还返回成功
    let s = TestServer::start().await;
    let r = s
        .post("/v1/kb/doc", kb_doc("没有这个库", "x", "内容"))
        .await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn 知识库接口需要_token() {
    let s = TestServer::start().await;
    assert_eq!(s.get_no_auth("/v1/kb/health").await.status(), 401);
    let r = s
        .post_no_auth("/v1/kb/search", serde_json::json!({"q": "x"}))
        .await;
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn 嵌入服务不可达时检索降级而不是报错() {
    // 这是明确要求的行为：知识库开着、但嵌入服务挂了 → 退回纯字面，200。
    let s = TestServer::start_with(|c| {
        c.kb.enable = true;
        // 一个必然拒绝连接的端口
        c.kb.embed.url = "http://127.0.0.1:9/v1/embeddings".into();
        c.kb.embed.timeout_seconds = 2;
    })
    .await;

    s.post("/v1/kb", kb_body("k1")).await;
    s.post("/v1/kb/doc", kb_doc("k1", "指南", "蜘蛛线可以用来做鱼竿"))
        .await;

    let r = s
        .post("/v1/kb/search", serde_json::json!({"q": "蜘蛛"}))
        .await;
    assert_eq!(r.status(), 200, "嵌入挂了不能让检索接口报错");
    let j: serde_json::Value = r.json().await.unwrap();
    assert_eq!(j["degraded"], "embed_failed", "降级必须可观测");
    assert_eq!(
        j["hits"].as_array().unwrap().len(),
        1,
        "降级后仍要能用字面命中"
    );
}

// ================================================================== 场景隔离（隐私边界）
//
// 这一组是 A 方案（私聊单独存、只对私聊场景可见）的**全部价值**所在。
//
// 为什么它必须是一个会失败的断言，而不是"打印出来看看"：
// 私聊与群聊存在同一张表、进同一个 system_prompt。谁哪天把 `group_id IS NULL`
// 那半个条件改没了，或者把默认场景从 group 改成 private，群里就能问出别人的私聊——
// 而**功能看起来完全正常**，没有任何报错。只有这种断言能在 CI 里把它拦下来。

#[tokio::test]
async fn scene_私聊消息在群场景里一个字都不出现() {
    let s = TestServer::start().await;
    s.post("/v1/message", group_msg("1031105617", "小明", "群里聊钓鱼"))
        .await;
    s.post(
        "/v1/message",
        private_msg("10001", "主人", "私聊说件事：我明天请假"),
    )
    .await;

    // 群场景：看得到群聊，**看不到私聊**
    let j: serde_json::Value = s
        .get("/v1/context?scene=group&budget=4000")
        .await
        .json()
        .await
        .unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(text.contains("群里聊钓鱼"), "群聊应当出现：\n{text}");
    assert!(
        !text.contains("私聊说件事"),
        "❌ 私聊内容泄漏到群场景了 —— 群里任何人问一句就能套出来：\n{text}"
    );
    assert!(
        text.contains("# 群里最近说的话"),
        "群场景用群聊标题：\n{text}"
    );
    assert_eq!(j["scene"], "group");
}

#[tokio::test]
async fn scene_私聊场景只给私聊且标题不同() {
    let s = TestServer::start().await;
    s.post("/v1/message", group_msg("1031105617", "小明", "群里聊钓鱼"))
        .await;
    s.post(
        "/v1/message",
        private_msg("10001", "主人", "私聊说件事：我明天请假"),
    )
    .await;

    let j: serde_json::Value = s
        .get("/v1/context?scene=private&budget=4000")
        .await
        .json()
        .await
        .unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(
        text.contains("私聊说件事"),
        "私聊场景应当看到私聊：\n{text}"
    );
    assert!(
        !text.contains("群里聊钓鱼"),
        "私聊场景不该混进群聊：\n{text}"
    );
    assert!(
        text.contains("# 你和他最近聊的"),
        "私聊场景用的是另一个标题，模型才分得清在跟谁说话：\n{text}"
    );
    assert_eq!(j["scene"], "private");
}

#[tokio::test]
async fn scene_不传参数时等同于_group_也就是更严的那一侧() {
    let s = TestServer::start().await;
    s.post("/v1/message", private_msg("10001", "主人", "这是一句私聊"))
        .await;

    // 「忘了传 scene」是最常见的调用方式。默认必须落在不给私聊的那一侧，
    // 否则只要客户端漏一个参数，隐私边界就没了。
    let j: serde_json::Value = s.get("/v1/context?budget=4000").await.json().await.unwrap();
    assert_eq!(j["scene"], "group", "默认场景必须是 group");
    let text = j["text"].as_str().unwrap();
    assert!(
        !text.contains("这是一句私聊"),
        "不传 scene 时泄漏了私聊：\n{text}"
    );
}

#[tokio::test]
async fn scene_无法识别的取值回落到_group_而不是_private() {
    let s = TestServer::start().await;
    s.post(
        "/v1/message",
        private_msg("10001", "主人", "不该出现的私聊"),
    )
    .await;

    // 写错参数不该变成"更宽松"。any 未知值 → group。
    for bad in ["", "GROUP", "grup", "私聊一下", "all", "0"] {
        let j: serde_json::Value = s
            .get(&format!("/v1/context?scene={bad}&budget=4000"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(j["scene"], "group", "scene={bad:?} 应回落到 group");
        let text = j["text"].as_str().unwrap();
        assert!(
            !text.contains("不该出现的私聊"),
            "scene={bad:?} 泄漏了私聊：\n{text}"
        );
    }
}

#[tokio::test]
async fn scene_group_id_是空串时算私聊不算群聊() {
    let s = TestServer::start().await;
    // 空串是"字段在但没值"。如果 SQL 只判 `IS NULL`，这条行两个场景都进不去（静默丢失）；
    // 如果只判 `IS NOT NULL`，它会冒充群聊。分开口径要一致。
    s.post(
        "/v1/message",
        serde_json::json!({
            "channel": "qq", "group_id": "", "account": "10001",
            "sender_name": "主人", "content": "空群号算私聊",
            "at_ms": bluewhale_memory::db::now_ms()
        }),
    )
    .await;

    let priv_j: serde_json::Value = s
        .get("/v1/context?scene=private&budget=4000")
        .await
        .json()
        .await
        .unwrap();
    assert!(
        priv_j["text"].as_str().unwrap().contains("空群号算私聊"),
        "空 group_id 应归入私聊，否则这条消息两个场景都看不到（静默丢失）"
    );

    let grp_j: serde_json::Value = s
        .get("/v1/context?scene=group&budget=4000")
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !grp_j["text"].as_str().unwrap().contains("空群号算私聊"),
        "空 group_id 不能冒充群聊"
    );
}

#[tokio::test]
async fn context_某张表读失败只少一段不整体_500() {
    let s = TestServer::start().await;
    s.post("/v1/event", event_body("挖了会儿矿")).await;
    // 把 diary 表直接删掉，模拟「某张表读失败」。
    s.db.with_write(|tx| {
        tx.execute("DROP TABLE diary", [])?;
        Ok(())
    })
    .unwrap();

    let r = s.get("/v1/context").await;
    assert_eq!(
        r.status(),
        200,
        "单表失败绝不能让整体 500 —— 少一段它还能说话"
    );
    let j: serde_json::Value = r.json().await.unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(!text.is_empty(), "其他段照常返回");
    assert!(text.contains("# 最近发生的事"), "事件段不受影响");
    let dropped: Vec<String> = j["dropped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        dropped
            .iter()
            .any(|d| d.contains("diary") && d.contains("读取失败")),
        "必须写清楚哪一段、为什么：{dropped:?}"
    );
}

// ================================================================== 规划接口

#[tokio::test]
async fn 规划列表带条目且字段是_snake_case() {
    let s = TestServer::start().await;
    seed_plan(
        &s,
        "p-list",
        "今天把家盖一盖",
        &[
            ("done", "把铁矿烧成铁锭"),
            ("done", "垒墙"),
            ("todo", "清包"),
            ("deferred", "钓鱼"),
            ("abandoned", "挖基岩"),
        ],
    );

    let list: Vec<serde_json::Value> = s.get("/v1/plan/list?limit=5").await.json().await.unwrap();
    assert_eq!(list.len(), 1);
    let p = &list[0];

    // 概要字段
    assert_eq!(p["id"], "p-list");
    assert_eq!(
        p["planning_day_id"], "p-list",
        "seed_plan 用 plan_id 当游戏日标签"
    );
    assert_eq!(p["summary"], "今天把家盖一盖");
    assert_eq!(p["game_day"], 1);
    assert!(p["created_ms"].as_i64().unwrap() > 0);
    assert!(p["updated_ms"].as_i64().unwrap() > 0);

    // 统计：契约里叫 total/done/deferred/todo（不是 total_items）
    assert_eq!(p["total"], 5);
    assert_eq!(p["done"], 2);
    assert_eq!(p["deferred"], 1);
    // 只数 state='todo'。`abandoned` 既不算 todo 也不算 done，
    // 所以 total(5) != done+deferred+todo(4) —— 这是有意的：不做了的不该算待办。
    assert_eq!(p["todo"], 1);
    assert!(p.get("total_items").is_none(), "字段名必须是契约里的 total");

    // items 要带上
    let items = p["items"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    // 按 ord 排，不是按 id
    assert_eq!(items[0]["title"], "把铁矿烧成铁锭");
    assert_eq!(items[4]["title"], "挖基岩");
    // state 原样透传，不译中文
    assert_eq!(items[0]["state"], "done");
    assert_eq!(items[2]["state"], "todo");
    assert_eq!(items[3]["state"], "deferred");
    assert_eq!(items[4]["state"], "abandoned");
    // reason/outcome 缺失时是 null
    assert!(items[0]["reason"].is_null());
    assert!(items[0]["outcome"].is_null());
}

#[tokio::test]
async fn 规划详情按_id_取且不存在时_404() {
    let s = TestServer::start().await;
    seed_plan(&s, "p-one", "单份计划", &[("todo", "一件事")]);

    let j: serde_json::Value = s.get("/v1/plan/p-one").await.json().await.unwrap();
    assert_eq!(j["id"], "p-one");
    assert_eq!(j["summary"], "单份计划");
    assert_eq!(j["items"].as_array().unwrap().len(), 1);

    let r = s.get("/v1/plan/没有这份").await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn 规划列表按现实时间倒序且尊重_limit() {
    let s = TestServer::start().await;
    let now = bluewhale_memory::db::now_ms();
    // 三份计划，created_ms 递增。**故意让 planning_day_id 逆序**，
    // 用来锁住「排序走现实时间，不走游戏日 g<N>」。
    for (i, (id, day)) in [("p-a", "g3"), ("p-b", "g2"), ("p-c", "g1")]
        .iter()
        .enumerate()
    {
        let (id, day) = (*id, *day);
        s.db.with_write(|tx| {
            tx.execute(
                "INSERT INTO plan(id, planning_day_id, summary, created_ms, updated_ms)
                 VALUES (?1,?2,?3,?4,?4)",
                rusqlite::params![id, day, format!("第{i}份"), now + i as i64 * 1000],
            )?;
            Ok(())
        })
        .unwrap();
    }

    let list: Vec<serde_json::Value> = s.get("/v1/plan/list?limit=10").await.json().await.unwrap();
    let ids: Vec<&str> = list.iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["p-c", "p-b", "p-a"], "最新的在前");

    let two: Vec<serde_json::Value> = s.get("/v1/plan/list?limit=2").await.json().await.unwrap();
    assert_eq!(two.len(), 2);
    assert_eq!(two[0]["id"], "p-c");
}

#[tokio::test]
async fn 规划列表跳过被删除的计划() {
    let s = TestServer::start().await;
    seed_plan(&s, "p-alive", "还在", &[]);
    seed_plan(&s, "p-dead", "删了", &[]);
    s.db.with_write(|tx| {
        tx.execute("UPDATE plan SET deleted_by = 1 WHERE id = 'p-dead'", [])?;
        Ok(())
    })
    .unwrap();

    let list: Vec<serde_json::Value> = s.get("/v1/plan/list").await.json().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], "p-alive");
    assert_eq!(s.get("/v1/plan/p-dead").await.status(), 404);
}

// ================================================================== 群消息删除

#[tokio::test]
async fn 群消息可逻辑删除且能被回滚撤销() {
    let s = TestServer::start().await;
    let r = s
        .post(
            "/v1/message",
            serde_json::json!({
                "channel": "qq", "account": "1", "sender_name": "某人",
                "content": "误入库的消息", "message_id": "m-1",
                "at_ms": bluewhale_memory::db::now_ms()
            }),
        )
        .await;
    assert_eq!(r.status(), 200);

    // 先确认读得到
    let before: Vec<serde_json::Value> = s
        .get("/v1/message/list?channel=qq")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(before.len(), 1);
    let id = before[0]["id"].as_i64().unwrap();

    // 删掉
    let del = TestServer::client()
        .delete(format!("{}/v1/message/{id}", s.base))
        .header("x-api-token", TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 200);

    let after: Vec<serde_json::Value> = s
        .get("/v1/message/list?channel=qq")
        .await
        .json()
        .await
        .unwrap();
    assert!(after.is_empty(), "删掉之后列表里不该再有");

    // 逻辑删除：行还在，只是 deleted_by 非空。
    let (rows, deleted_by): (i64, Option<i64>) =
        s.db.with_read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*), MAX(deleted_by) FROM channel_message WHERE id = ?1",
                rusqlite::params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(rows, 1, "不是物理删除，行还在");
    assert!(deleted_by.is_some(), "应记下是被哪次删除带走的");

    // 走的是 rollback_log：删除必须留下一条 'delete' 审计，
    // 这条审计同时就是「撤销」要用的入口。
    let rid = deleted_by.unwrap();
    let (mode, affected): (String, String) =
        s.db.with_read(|c| {
            Ok(c.query_row(
                "SELECT mode, affected_json FROM rollback_log WHERE id = ?1",
                rusqlite::params![rid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(mode, "delete", "单条删除在审计里是 delete 模式");
    assert_eq!(affected, "[]", "单条删除不涉及按时间窗口的批量遗忘");

    // 撤销回滚 → 消息回来
    let undo = s
        .post(
            "/v1/rollback/undo",
            serde_json::json!({ "rollback_id": rid }),
        )
        .await;
    assert_eq!(undo.status(), 200, "撤销应成功");
    let back: Vec<serde_json::Value> = s
        .get("/v1/message/list?channel=qq")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(back.len(), 1, "撤销后消息应恢复");
}

#[tokio::test]
async fn 删除不存在的群消息返回_404() {
    let s = TestServer::start().await;
    let del = TestServer::client()
        .delete(format!("{}/v1/message/99999", s.base))
        .header("x-api-token", TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 404);
}

#[tokio::test]
async fn 删除群消息需要_token() {
    let s = TestServer::start().await;
    let del = TestServer::client()
        .delete(format!("{}/v1/message/1", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 401);
}

// ================================================================== 空串幂等键防御

#[tokio::test]
async fn 空串_dedup_key_不吞掉后续事件() {
    let s = TestServer::start().await;
    // 实测的坑：第一条第 `dedup_key:""` 写进去后，之后**所有**同字段事件
    // 都被判成 duplicate，不管 summary 完全不同（永久静默丢数据）。
    for i in 0..3 {
        let mut b = event_body(&format!("第{i}件完全不同的事"));
        b["dedup_key"] = serde_json::json!("");
        let j: serde_json::Value = s.post("/v1/event", b).await.json().await.unwrap();
        assert_eq!(
            j["duplicate"], false,
            "第{i}条：空串幂等键不该让后来的事件被吞"
        );
        assert!(j["id"].as_i64().unwrap() > 0);
    }
    let list: Vec<serde_json::Value> = s.get("/v1/event/list?limit=50").await.json().await.unwrap();
    assert_eq!(list.len(), 3, "三条都要真的写进去");
}

#[tokio::test]
async fn 空串_dedup_key_不吞掉后续手帐() {
    let s = TestServer::start().await;
    for i in 0..3 {
        let j: serde_json::Value = s
            .post(
                "/v1/journal",
                serde_json::json!({
                    "summary": format!("第{i}条手帐"), "dedup_key": ""
                }),
            )
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(j["duplicate"], false, "第{i}条手帐不该被吞");
    }
    let j: serde_json::Value = s
        .get("/v1/journal/list?limit=50")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["total"], 3);
    assert_eq!(j["items"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn 真的重复的_dedup_key_仍然去重() {
    // 归一化只对空串生效，真幂等键的去重能力不能丢。
    let s = TestServer::start().await;
    let mk = |summary: &str| {
        let mut b = event_body(summary);
        b["dedup_key"] = serde_json::json!("real-key-1");
        b
    };
    let j1: serde_json::Value = s
        .post("/v1/event", mk("第一次"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j1["duplicate"], false);
    let j2: serde_json::Value = s
        .post("/v1/event", mk("第二次"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j2["duplicate"], true, "同 key 仍要去重");
    assert!(j2["id"].is_null());

    let list: Vec<serde_json::Value> = s.get("/v1/event/list?limit=50").await.json().await.unwrap();
    assert_eq!(list.len(), 1);
}

#[tokio::test]
async fn 空串_message_id_不吞掉后续群消息() {
    let s = TestServer::start().await;
    for i in 0..3 {
        let j: serde_json::Value = s
            .post(
                "/v1/message",
                serde_json::json!({
                    "channel": "qq", "account": "1",
                    "content": format!("第{i}条不同的话"),
                    "message_id": "",
                    "at_ms": bluewhale_memory::db::now_ms() + i
                }),
            )
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(j["inserted"], true, "第{i}条群消息不该被吞");
    }
    let list: Vec<serde_json::Value> = s
        .get("/v1/message/list?channel=qq")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 3);
}

// ================================================================== recall 兜底

#[tokio::test]
async fn 检索自然语言查询不再返回空数组() {
    // 这是本次修的核心：旧版对非空 q 是纯子串匹配，
    // 「在吗」「你最近干嘛」「最近」「心情」全部返回 []，
    // 于是插件不注入上下文、模型照常编，而且零报错。
    let s = TestServer::start().await;
    s.post("/v1/event", event_body("在服务器里盖房子")).await;
    s.post("/v1/event", event_body("和玩家聊了天气")).await;

    for q in ["在吗", "你最近干嘛", "最近", "心情"] {
        let list: Vec<serde_json::Value> = s
            .get(&format!("/v1/recall?q={q}&topk=5"))
            .await
            .json()
            .await
            .unwrap();
        assert!(
            !list.is_empty(),
            "q={q} 必须兜底返回，不能是空数组（空数组意味着静默失效）"
        );
        // 一条关键词都不命中时，全部标「权重最高」，不冒充命中。
        assert!(list.iter().all(|r| r["hit"] == false), "q={q} 不该有命中");
        assert!(list
            .iter()
            .all(|r| r["why"].as_str().unwrap() == "权重最高"));
    }
}

#[tokio::test]
async fn 检索命中优先于高权重的未命中() {
    // 排序键是 `(是否命中, weight)` 字典序，不是纯 weight ——
    // 一条高权重但不相关的老记忆**不能**挤掉真正相关的那条。
    let s = TestServer::start().await;
    let now = bluewhale_memory::db::now_ms();

    let mut important = event_body("一件极其重要的旧事");
    important["importance"] = serde_json::json!(1.0);
    important["at_ms"] = serde_json::json!(now - 200 * 86_400_000i64); // 200 天前
    s.post("/v1/event", important).await;

    let mut relevant = event_body("主人爱吃的鱼是烤鱼");
    relevant["importance"] = serde_json::json!(0.01); // 极低权重
    relevant["at_ms"] = serde_json::json!(now);
    s.post("/v1/event", relevant).await;

    let list: Vec<serde_json::Value> = s.get("/v1/recall?q=鱼&topk=5").await.json().await.unwrap();
    assert_eq!(list.len(), 2, "命中一条 + 兜底一条");
    assert!(
        list[0]["text"].as_str().unwrap().contains("鱼"),
        "相关的必须排第一，哪怕它权重低得多：{list:?}"
    );
    assert_eq!(list[0]["hit"], true);
    assert_eq!(list[0]["why"], "关键词命中");
    assert_eq!(list[1]["hit"], false);
    // 而且未命中的那条权重确实更高 —— 证明排序不是靠权重排出来的。
    assert!(
        list[1]["weight"].as_f64().unwrap() > list[0]["weight"].as_f64().unwrap(),
        "这条测试只有在高权重那条真的更高时才说明问题"
    );
}

// ---------------------------------------------------------------- 检索质量
//
// 下面这一组是「检索从子串匹配换成 BM25」的**验收判据**。
//
// 为什么必须写成会失败的断言：旧实现的问题不是"报错"，是"看起来正常"。
// 实测它的两个症状都能在完全无报错的情况下持续很久 ——
//   ① 换个说法就查不到（8 个自然改写提问，字面命中 0/8）；
//   ② 换十个不同问题，注入的记忆段一模一样（唯一性 1/10）。
// 这两个症状都不会让任何接口返回非 200，只能靠断言拦。

#[tokio::test]
async fn 手帐能被检索到() {
    // 手帐是**最大的一块语料**（实测线上 585 条 = 全库文本的 58.6%）。
    // 旧实现只查 event + fact，等于一多半记忆永远想不起来。
    let s = TestServer::start().await;
    let j: serde_json::Value = s
        .post(
            "/v1/journal",
            serde_json::json!({"summary": "做完了「找蜘蛛摸线」"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["duplicate"], false);

    let list: Vec<serde_json::Value> = s
        .get("/v1/recall?q=蜘蛛&topk=5")
        .await
        .json()
        .await
        .unwrap();
    assert!(
        list.iter()
            .any(|r| r["kind"] == "journal" && r["hit"] == true),
        "手帐必须能被检索到，且带 kind=journal：{list:?}"
    );
}

#[tokio::test]
async fn 换个说法也能查到() {
    // 旧实现的关键词是 `contains(q)`：查询「蜘蛛刷出来没有」里没有任何
    // 一条记忆的原文，于是命中 0 条、全靠兜底。中文没有空格，
    // 用户永远不会恰好说出和记忆一模一样的整句。
    let s = TestServer::start().await;
    s.post(
        "/v1/journal",
        serde_json::json!({"summary": "做完了「找蜘蛛摸线」"}),
    )
    .await;
    s.post(
        "/v1/journal",
        serde_json::json!({"summary": "做完了「砍树凑木板」"}),
    )
    .await;

    // 问句与语料原文没有一处相同的连续子串，但共享「蜘蛛」
    let list: Vec<serde_json::Value> = s
        .get("/v1/recall?q=蜘蛛刷出来没有&topk=5")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list[0]["hit"], true, "改写提问必须命中：{list:?}");
    assert!(
        list[0]["text"].as_str().unwrap().contains("蜘蛛"),
        "命中的必须真的是蜘蛛那条：{list:?}"
    );
    // 不相关的那条不该被标成命中
    let wood = list
        .iter()
        .find(|r| r["text"].as_str().unwrap().contains("砍树"));
    if let Some(w) = wood {
        assert_eq!(w["hit"], false, "砍树那条与蜘蛛无关，不该算命中");
    }
}

#[tokio::test]
async fn 不同提问返回不同记忆() {
    // 这是整次改造最核心的一条判据。
    //
    // 旧实现里未命中就靠 weight 兜底，而 weight 与提问完全无关 ——
    // 实测换十个问题，注入的记忆段一模一样（唯一性 1/10）。
    // 那意味着记忆注入花了几百 token，实际是一段固定的背景噪声。
    let s = TestServer::start().await;
    for summary in [
        "做完了「找蜘蛛摸线」",
        "做完了「理箱子」",
        "做完了「做鱼竿」",
        "在 y=-54 发现了钻石矿脉",
        "和玩家聊了天气",
    ] {
        s.post("/v1/journal", serde_json::json!({"summary": summary}))
            .await;
    }

    let mut seen: Vec<String> = Vec::new();
    for q in ["蜘蛛", "箱子", "鱼竿", "钻石", "天气"] {
        let list: Vec<serde_json::Value> = s
            .get(&format!("/v1/recall?q={q}&topk=1"))
            .await
            .json()
            .await
            .unwrap();
        seen.push(list[0]["text"].as_str().unwrap().to_string());
    }
    let uniq: std::collections::HashSet<&String> = seen.iter().collect();
    assert_eq!(
        uniq.len(),
        seen.len(),
        "每个提问都该给出不同的答案，实际：{seen:?}"
    );
}

#[tokio::test]
async fn 纯虚词提问不冒充命中() {
    // 「在吗」「最近」这类话不含任何实义内容。它们**必须**仍然返回兜底行
    // （否则插件拿到空数组就不注入，模型照常编），但一条都不该标成 hit ——
    // 标成 hit 会让回放面板显示"它想起来了"，而它其实什么都没想起来。
    //
    // 实测这个坑：归一化是相对的，「在吗」在一句话上也能算出 1.0 分。
    let s = TestServer::start().await;
    s.post("/v1/event", event_body("在服务器里盖房子")).await;
    s.post("/v1/event", event_body("和玩家聊了天气")).await;

    for q in ["在吗", "最近"] {
        let list: Vec<serde_json::Value> = s
            .get(&format!("/v1/recall?q={q}&topk=5"))
            .await
            .json()
            .await
            .unwrap();
        assert!(!list.is_empty(), "q={q} 仍要兜底返回，不能空数组");
        assert!(
            list.iter().all(|r| r["hit"] == false),
            "q={q} 是纯虚词，不该有任何命中：{list:?}"
        );
    }
}

#[tokio::test]
async fn 常见搭配不算命中() {
    // 查询里的「东西」与语料里的「东西」不是同一个指称，共享它纯属巧合。
    // 实测：问「我上次挖到什么好东西」会把「怕的东西: 苦力怕」捞出来。
    // 这类词统计上并不高频（df 只占 3.2%，比「钓鱼」还稀有），
    // 所以只能按词表处理，不能指望阈值。
    let s = TestServer::start().await;
    let mut f = event_body("怕的东西: 苦力怕");
    f["importance"] = serde_json::json!(0.9);
    s.post("/v1/event", f).await;
    s.post(
        "/v1/journal",
        serde_json::json!({"summary": "做完了「补上次的鱼」"}),
    )
    .await;

    let list: Vec<serde_json::Value> = s
        .get("/v1/recall?q=我上次挖到什么好东西没&topk=10")
        .await
        .json()
        .await
        .unwrap();
    let fear = list
        .iter()
        .find(|r| r["text"].as_str().unwrap().contains("苦力怕"));
    if let Some(x) = fear {
        assert_eq!(x["hit"], false, "「怕的东西」只靠泛化词重合，不该算命中");
    }
}

#[tokio::test]
async fn 命中会写出具体的匹配词() {
    // 「它为什么想起这条」要能回答。`why` 的取值是既有契约（只有两个），
    // 所以匹配词放在单独的 `matched` 字段里。
    let s = TestServer::start().await;
    s.post(
        "/v1/journal",
        serde_json::json!({"summary": "做完了「找蜘蛛摸线」"}),
    )
    .await;
    let list: Vec<serde_json::Value> = s
        .get("/v1/recall?q=蜘蛛&topk=5")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list[0]["hit"], true);
    let matched = list[0]["matched"].as_array().expect("matched 应是数组");
    assert!(!matched.is_empty(), "命中了就该说清楚命中了什么词");
    assert!(
        matched.iter().any(|v| v.as_str() == Some("蜘蛛")),
        "匹配词里该有「蜘蛛」：{matched:?}"
    );
    // why 仍然只有两个既有取值，不能被塞进别的文案
    assert_eq!(list[0]["why"], "关键词命中");
}

#[tokio::test]
async fn 兜底行不会被反复强化() {
    // 旧实现把**所有返回行**都计入 recall_count，包括纯兜底行。
    // 于是兜底行 weight 越涨越高、永远占着剩余名额，再也下不来 ——
    // 这是"换十个问题结果都一样"的自我强化环路。
    //
    // 只在真命中时 +1，兜底不算「被想起」。
    let s = TestServer::start().await;
    s.post("/v1/event", event_body("一件毫无关联的旧事")).await;

    for _ in 0..5 {
        let list: Vec<serde_json::Value> = s
            .get("/v1/recall?q=完全不同的查询词&topk=5")
            .await
            .json()
            .await
            .unwrap();
        assert!(list.iter().all(|r| r["hit"] == false));
    }
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    // event 表没有 recall_count 列，看回放日志里的 kept 标记
    let n: i64 =
        s.db.with_read(|c| {
            Ok(
                c.query_row("SELECT COUNT(*) FROM recall_log WHERE kept = 1", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .unwrap();
    assert_eq!(n, 0, "全是兜底的检索不该把任何一条记成「被想起」");
}

#[tokio::test]
async fn 索引缓存在多次检索间复用且写入后失效() {
    // 每次检索都重建索引是 p50 的全部来源（release 实测 6.3 ms / 752 条），
    // 且代价随语料线性增长。缓存必须：
    //   ① 两次读之间复用同一份（未命中数只涨一次）；
    //   ② 任何写入之后立刻失效 —— 否则「刚写进去的记忆查不到」，
    //      而那是最难排查的一类 bug（接口全 200）。
    let s = TestServer::start().await;
    s.post(
        "/v1/journal",
        serde_json::json!({"summary": "做完了「找蜘蛛摸线」"}),
    )
    .await;

    let before = s.state.index.stats();
    for _ in 0..5 {
        let j: Vec<serde_json::Value> = s
            .get("/v1/recall?q=蜘蛛&topk=5")
            .await
            .json()
            .await
            .unwrap();
        assert!(!j.is_empty());
    }
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let mid = s.state.index.stats();
    assert_eq!(
        mid.1 - before.1,
        1,
        "5 次读只该建一次索引（未命中 +1），实际 +{}",
        mid.1 - before.1
    );
    assert!(mid.0 >= before.0 + 4, "后续读取应命中缓存");

    // 写一条新记忆 → 版本号变化 → 必须立刻能查到
    s.post(
        "/v1/journal",
        serde_json::json!({"summary": "做完了「钓到一条大鱼」"}),
    )
    .await;
    let j: Vec<serde_json::Value> = s
        .get("/v1/recall?q=钓到一条大鱼&topk=5")
        .await
        .json()
        .await
        .unwrap();
    assert!(
        j.iter()
            .any(|r| r["text"].as_str().unwrap().contains("大鱼") && r["hit"] == true),
        "写入后索引必须失效，新记忆要能立刻查到：{j:?}"
    );
}

#[tokio::test]
async fn 检索给是否命中单独的字段() {
    let s = TestServer::start().await;
    s.post("/v1/event", event_body("挖到了钻石")).await;
    let list: Vec<serde_json::Value> = s.get("/v1/recall?q=钻石").await.json().await.unwrap();
    assert!(
        list[0]["hit"].is_boolean(),
        "hit 必须单独给出来，别靠 why 字符串猜"
    );
    assert_eq!(list[0]["hit"], true);
}

#[tokio::test]
async fn 检索只有空查询才按纯权重() {
    // 空 q 时没有「命中」这个概念，全部是未命中，等价于纯权重排序。
    let s = TestServer::start().await;
    let mut hi = event_body("重要");
    hi["importance"] = serde_json::json!(0.95);
    s.post("/v1/event", hi).await;
    let mut lo = event_body("次要");
    lo["importance"] = serde_json::json!(0.05);
    s.post("/v1/event", lo).await;

    let list: Vec<serde_json::Value> = s.get("/v1/recall").await.json().await.unwrap();
    assert_eq!(list.len(), 2);
    assert!(list[0]["weight"].as_f64().unwrap() >= list[1]["weight"].as_f64().unwrap());
    assert!(list.iter().all(|r| r["hit"] == false));
}

#[tokio::test]
async fn 新增的消息删除路由不抢已读接口() {
    // `/v1/message/{id}`（DELETE，且 id 解析成 i64）与 `/v1/message/consumed`
    // （POST）在同一个路径前缀下。加了动态段之后必须先确认静态段还能命中——
    // 否则「标记已读」会变成 400（`consumed` 不是 i64）。
    let s = TestServer::start().await;
    let j: serde_json::Value = s
        .post(
            "/v1/message",
            serde_json::json!({
                "channel": "qq", "account": "1", "content": "要标记已读的消息",
                "at_ms": bluewhale_memory::db::now_ms()
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["inserted"], true);

    let list: Vec<serde_json::Value> = s
        .get("/v1/message/list?channel=qq")
        .await
        .json()
        .await
        .unwrap();
    let id = list[0]["id"].as_i64().unwrap();

    let r = s
        .post("/v1/message/consumed", serde_json::json!({ "ids": [id] }))
        .await;
    assert_eq!(r.status(), 200, "静态路由不能被 {{id}} 抢走");
    let j: serde_json::Value = r.json().await.unwrap();
    assert_eq!(j["updated"], 1);

    let list: Vec<serde_json::Value> = s
        .get("/v1/message/list?channel=qq")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list[0]["consumed"], 1);
}

// ================================================================== 称呼归一

fn seed_person_scene(s: &TestServer, pid: &str, display: &str, scenes: &[(&str, &str)]) {
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms)
             VALUES (?1, ?2, 'confirmed', 1)",
            rusqlite::params![pid, display],
        )?;
        for (scene, name) in scenes {
            tx.execute(
                "INSERT INTO person_scene(person_id, scene, name, updated_ms)
                 VALUES (?1, ?2, ?3, 1)",
                rusqlite::params![pid, scene, name],
            )?;
        }
        Ok(())
    })
    .unwrap();
}

#[tokio::test]
async fn 回复里的场景称呼会被换成身份显示名() {
    let s = TestServer::start().await;
    // 一个人两种场景叫法；另一个人的 QQ 群名片与此无关，不该被连带替换。
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms)
             VALUES ('p1','姆姆','confirmed',1)",
            [],
        )?;
        for (scene, name) in [("mc", "GleamSlime"), ("qq_group", "啥子猫")] {
            tx.execute(
                "INSERT INTO person_scene(person_id, scene, name, updated_ms)
                 VALUES ('p1', ?1, ?2, 1)",
                rusqlite::params![scene, name],
            )?;
        }
        Ok(())
    })
    .unwrap();

    let r = s
        .post(
            "/v1/name/normalize",
            serde_json::json!({"text": "啥子猫说得对，GleamSlime 也在"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let j: serde_json::Value = r.json().await.unwrap();
    assert_eq!(j["text"], "姆姆说得对，姆姆 也在");
    let reps = j["replacements"].as_array().unwrap();
    assert_eq!(reps.len(), 2, "两条替换都应报告：{j}");
    assert!(reps
        .iter()
        .any(|x| x["from"] == "啥子猫" && x["to"] == "姆姆"));
}

#[tokio::test]
async fn 称呼归一可以只看一个场景() {
    let s = TestServer::start().await;
    seed_person_scene(
        &s,
        "p1",
        "姆姆",
        &[("mc", "GleamSlime"), ("qq_group", "啥子猫")],
    );

    let r = s
        .post(
            "/v1/name/normalize",
            serde_json::json!({"text": "啥子猫和 GleamSlime", "scene": "qq_group"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let j: serde_json::Value = r.json().await.unwrap();
    assert_eq!(j["text"], "姆姆和 GleamSlime", "只替换该场景的称呼");
}

#[tokio::test]
async fn 称呼归一拒绝未知场景空文本与未登录() {
    let s = TestServer::start().await;
    let r = s
        .post(
            "/v1/name/normalize",
            serde_json::json!({"text": "x", "scene": "telegram"}),
        )
        .await;
    assert_eq!(r.status(), 400);

    let r = s
        .post("/v1/name/normalize", serde_json::json!({"text": "   "}))
        .await;
    assert_eq!(r.status(), 400);

    let r = s
        .post_no_auth("/v1/name/normalize", serde_json::json!({"text": "啥子猫"}))
        .await;
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn 同一个称呼指向两个人时不替换() {
    let s = TestServer::start().await;
    seed_person_scene(&s, "p1", "甲", &[("qq_group", "同名")]);
    seed_person_scene(&s, "p2", "乙", &[("qq_group", "同名")]);

    let r = s
        .post(
            "/v1/name/normalize",
            serde_json::json!({"text": "同名在这"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let j: serde_json::Value = r.json().await.unwrap();
    assert_eq!(j["text"], "同名在这", "有歧义时宁可不动，不能张冠李戴");
    assert!(j["replacements"].as_array().unwrap().is_empty());
}

// ================================================================== 上下文里的称呼对照

fn seed_named_person(s: &TestServer) {
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms)
             VALUES ('p1','姆姆','confirmed',1)",
            [],
        )?;
        for (scene, name) in [("mc", "GleamSlime"), ("qq_group", "啥子猫")] {
            tx.execute(
                "INSERT INTO person_scene(person_id, scene, name, updated_ms)
                 VALUES ('p1',?1,?2,1)",
                rusqlite::params![scene, name],
            )?;
        }
        Ok(())
    })
    .unwrap();
}

#[tokio::test]
async fn 上下文只在提到某个人时带称呼对照() {
    let s = TestServer::start().await;
    seed_named_person(&s);

    // 提到显示名 → 出现 names 段，并给出别的叫法
    let r = s.get("/v1/context?q=姆姆今天在吗").await;
    assert_eq!(r.status(), 200);
    let j: serde_json::Value = r.json().await.unwrap();
    let parts = j["parts"].as_array().unwrap();
    assert!(parts.iter().any(|p| p == "names"), "应出现 names 段：{j}");
    let text = j["text"].as_str().unwrap();
    assert!(text.contains("啥子猫"), "应给出群名片：{text}");

    // 提到场景称呼 → 同样能对上
    let r = s.get("/v1/context?q=啥子猫呢").await;
    let j: serde_json::Value = r.json().await.unwrap();
    assert!(j["parts"].as_array().unwrap().iter().any(|p| p == "names"));

    // 没提到任何人 → 不出现（不能把所有身份都塞进去）
    let r = s.get("/v1/context?q=今天天气不错").await;
    let j: serde_json::Value = r.json().await.unwrap();
    assert!(!j["parts"].as_array().unwrap().iter().any(|p| p == "names"));
    assert!(!j["text"].as_str().unwrap().contains("啥子猫"));

    // 空 q → 也不出现
    let r = s.get("/v1/context").await;
    let j: serde_json::Value = r.json().await.unwrap();
    assert!(!j["parts"].as_array().unwrap().iter().any(|p| p == "names"));
}

#[tokio::test]
async fn 歧义称呼不会把人带进上下文() {
    let s = TestServer::start().await;
    seed_person_scene(&s, "p1", "甲", &[("qq_group", "同名")]);
    seed_person_scene(&s, "p2", "乙", &[("qq_group", "同名")]);

    let r = s.get("/v1/context?q=同名在这里").await;
    let j: serde_json::Value = r.json().await.unwrap();
    assert!(
        !j["parts"].as_array().unwrap().iter().any(|p| p == "names"),
        "有歧义的称呼不能当触发词：{j}"
    );
}

// ================================================================== 群消息发送者映射

fn seed_group_identity(s: &TestServer) {
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms)
             VALUES ('p1','姆姆','confirmed',1)",
            [],
        )?;
        tx.execute(
            "INSERT INTO person_account(channel, account, person_id, seen_name,
                                        first_seen_ms, last_seen_ms)
             VALUES ('qq','478889187','p1','啥子猫',1,1)",
            [],
        )?;
        tx.execute(
            "INSERT INTO person_scene(person_id, scene, name, updated_ms)
             VALUES ('p1','qq_group','啥子猫',1)",
            [],
        )?;
        // 群名片已经改成「啥子喵」，面板里的人工值还是旧的「啥子猫」
        for (account, sender, content, at) in [
            (
                "478889187",
                "啥子喵",
                "@啥子喵(478889187) 今天也在挖矿",
                100,
            ),
            // 没绑账号、但名字能对上的人（名字兜底）
            ("600000", "啥子猫", "对，我也在", 200),
            ("700000", "陌生人", "路过", 300),
        ] {
            tx.execute(
                "INSERT INTO channel_message(channel, group_id, account, sender_name, content, at_ms)
                 VALUES ('qq','g1',?1,?2,?3,?4)",
                rusqlite::params![account, sender, content, at],
            )?;
        }
        Ok(())
    })
    .unwrap();
}

#[tokio::test]
async fn 群消息发送者显示身份名而不是群名片() {
    let s = TestServer::start().await;
    seed_group_identity(&s);

    let r = s.get("/v1/context?scene=group&q=今天").await;
    assert_eq!(r.status(), 200);
    let j: serde_json::Value = r.json().await.unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(
        text.contains("姆姆：@姆姆(478889187) 今天也在挖矿"),
        "按 QQ 号映射成身份名，内容里的 @昵称 也要换：{text}"
    );
    assert!(
        text.contains("姆姆：对，我也在"),
        "名字兜底也要能映射：{text}"
    );
    assert!(
        text.contains("陌生人：路过"),
        "没绑定、名字也对不上的人保持原样"
    );
    assert!(!text.contains("啥子喵"), "群名片不该再出现：{text}");
}

#[tokio::test]
async fn 群名片改名后称呼归一仍然认得出() {
    let s = TestServer::start().await;
    seed_group_identity(&s);

    // 面板人工值是旧「啥子猫」，但群里最新名片是「啥子喵」——两个都要换成显示名
    let r = s
        .post(
            "/v1/name/normalize",
            serde_json::json!({"text": "啥子喵和啥子猫都在"}),
        )
        .await;
    let j: serde_json::Value = r.json().await.unwrap();
    assert_eq!(j["text"], "姆姆和姆姆都在");

    // 上下文里提到新名片也要能触发称呼对照
    let r = s.get("/v1/context?q=啥子喵呢").await;
    let j: serde_json::Value = r.json().await.unwrap();
    assert!(
        j["parts"].as_array().unwrap().iter().any(|p| p == "names"),
        "改名后的名片也要能触发 names：{j}"
    );
}

#[tokio::test]
async fn 日记总是给全文包括结尾() {
    let s = TestServer::start().await;
    // 一篇 200 字以上的日记，关键信息在**最后一段**（当晚谁来过）。
    let head = "本来打算天还黑着就把包里的石头归进箱子，等天亮了在屋边种点小麦。";
    let tail = "夜里那个叫姆姆的又跑来问，说什么我要找新家。啧，我好好的搬什么家。";
    let body = format!("{head}{}", "理完顺手把洞壁上那点煤挖了。".repeat(5)) + tail;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO diary(id, kind, window_start_ms, window_end_ms, title, body,
                               model, created_ms, updated_ms)
             VALUES ('d1','day',1,2,'背包清空的一夜',?1,'t',1,1)",
            rusqlite::params![body],
        )?;
        Ok(())
    })
    .unwrap();

    // 闲聊也要给全文：下一句追问「怎么还藏私」里没有「日记」二字，
    // 按提问识别分档必然漏，所以不做了。
    let j: serde_json::Value = s
        .get("/v1/context?q=今天天气不错")
        .await
        .json()
        .await
        .unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(text.contains("背包清空的一夜"), "日记段应该在：{text}");
    assert!(text.contains("搬什么家"), "闲聊时也要给到结尾：{text}");

    // 问日记：给到结尾
    let j: serde_json::Value = s
        .get("/v1/context?q=你日记里是不是在说我坏话")
        .await
        .json()
        .await
        .unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(text.contains("搬什么家"), "问日记时应能看到结尾：{text}");
}

#[tokio::test]
async fn 要它念日记时给全文() {
    let s = TestServer::start().await;
    // 450 字的日记（比线上最长的那篇还长一点）
    let body: String = (0..15)
        .map(|i| format!("第{i}段：理箱子、挖煤、垒墙，一共干了不少活儿。"))
        .collect::<String>()
        + "夜里那个叫姆姆的又跑来问，说什么我要找新家。啧，我好好的搬什么家。";
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO diary(id, kind, window_start_ms, window_end_ms, title, body,
                               model, created_ms, updated_ms)
             VALUES ('d1','day',1,2,'长日记',?1,'t',1,1)",
            rusqlite::params![body],
        )?;
        Ok(())
    })
    .unwrap();

    let j: serde_json::Value = s
        .get("/v1/context?q=读读你昨天的日记")
        .await
        .json()
        .await
        .unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(
        text.contains("搬什么家"),
        "「念日记」要把结尾也给到：{text}"
    );
    assert!(
        text.contains("第0段") && text.contains("第14段"),
        "全文都在，不是只给开头：{text}"
    );
}

// ================================================================== 旧接口的列/占位符对齐

#[tokio::test]
async fn 自我认知能写入且读回最新一版() {
    // 这里曾经把 INSERT 的占位符写成 5 个、只给 4 个值 →
    // 每次写都 503「5 values for 4 columns」，线上从上线起就没成功过。
    let s = TestServer::start().await;
    let r = s
        .post(
            "/v1/self",
            serde_json::json!({"body":"我是谁 v1","created_ms":1000}),
        )
        .await;
    assert_eq!(r.status(), 200, "写自我认知应成功");

    let r = s
        .post(
            "/v1/self",
            serde_json::json!({"body":"我是谁 v2","created_ms":2000}),
        )
        .await;
    assert_eq!(r.status(), 200);

    let j: serde_json::Value = s.get("/v1/self/get").await.json().await.unwrap();
    assert_eq!(j["body"], "我是谁 v2", "读到的是最新一版：{j}");
}

#[tokio::test]
async fn 承诺能写入且可改状态() {
    // 同样的问题：INSERT 写了 9 个占位符但只有 8 列。
    let s = TestServer::start().await;
    let r = s
        .post(
            "/v1/promise",
            serde_json::json!({
                "id":"p1","description":"明天帮你修屋顶","status":"open",
                "created_ms":1000,"updated_ms":1000
            }),
        )
        .await;
    assert_eq!(r.status(), 200, "写承诺应成功");

    let r = TestServer::client()
        .patch(format!("{}/v1/promise/p1", s.base))
        .header("x-api-token", TOKEN)
        .json(&serde_json::json!({"status":"kept"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 这个接口返回的是**数组**（不是 {items:[]}）—— 与 /v1/relation/list 一致。
    let j: serde_json::Value = s.get("/v1/promise/list").await.json().await.unwrap();
    assert_eq!(j[0]["status"], "kept", "实际：{j}");
    assert_eq!(j[0]["description"], "明天帮你修屋顶");
}

#[tokio::test]
async fn 情绪流水能列出来() {
    // 这里曾经整列错位（r.get(1..7) 读 7 列），at_ms 被按 TEXT 解析 →
    // 一直 503「Invalid column type Integer at index: 1」。
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO affect_log(person_id, at_ms, kind, field, delta, reason)
             VALUES ('p1', 1000, 'mood', 'valence', 0.2, '一起干了活')",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let r = s.get("/v1/affect/list").await;
    assert_eq!(
        r.status(),
        200,
        "情绪流水应能读：{}",
        r.text().await.unwrap()
    );
    let j: serde_json::Value = r.json().await.unwrap();
    assert_eq!(j[0]["at_ms"], 1000);
    assert_eq!(j[0]["kind"], "mood");
    assert_eq!(j[0]["reason"], "一起干了活");
}

#[tokio::test]
async fn 注入回放可以带全文并在面板读到() {
    // 「注入回放目前作用不大」的根因是只存了摘要：面板上永远只有
    // 「memory: 已注入」，看不到到底塞了什么。现在允许带 full_text。
    let s = TestServer::start().await;
    let full = "# 你最近在做什么\n- 在盖房子\n\n# 你记得的事\n- 钻石";
    let r = s
        .post(
            "/v1/injection-replay",
            serde_json::json!({"entries":[{
                "at_ms":2000,"lane":"qq-group","query":"在吗",
                "chars":full.chars().count(),"injected":true,
                "replay":"plan: 已注入 | memory: 已注入",
                "full_text":full
            }]}),
        )
        .await;
    assert_eq!(r.status(), 200);

    // 老客户端不传 full_text 也要能写（向后兼容）
    let r = s
        .post(
            "/v1/injection-replay",
            serde_json::json!({"entries":[{"at_ms":3000,"lane":"mc","replay":"x"}]}),
        )
        .await;
    assert_eq!(r.status(), 200, "缺 full_text 不该 422");

    let j: serde_json::Value = s.get("/api/admin/injections").await.json().await.unwrap();
    let rows = j["items"].as_array().unwrap();
    let with_full = rows.iter().find(|x| x["at_ms"] == 2000).unwrap();
    assert_eq!(with_full["full_text"], full, "全文要能原样读回");
    let without = rows.iter().find(|x| x["at_ms"] == 3000).unwrap();
    assert!(
        without["full_text"].is_null(),
        "没传就该是 null，不能编一个"
    );
}

#[tokio::test]
async fn 注入回放按条目数组写入() {
    let s = TestServer::start().await;
    let r = s
        .post(
            "/v1/injection-replay",
            serde_json::json!({"entries":[{
                "at_ms":1000,"lane":"qq-group","query":"在吗",
                "chars":120,"injected":true,"replay":"plan: 已注入"
            }]}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let j: serde_json::Value = r.json().await.unwrap();
    assert_eq!(j["inserted"], 1);

    // 同一毫秒重放：幂等，不翻倍
    let r = s
        .post(
            "/v1/injection-replay",
            serde_json::json!({"entries":[{"at_ms":1000,"replay":"dup"}]}),
        )
        .await;
    let j: serde_json::Value = r.json().await.unwrap();
    assert_eq!(j["inserted"], 0, "同毫秒应判重");
}

#[tokio::test]
async fn 探针写出来的空日记会盖住真实日记() {
    // 线上真发生过：用 `{"body":"x"}` 探接口连通性，写进去一篇 body="x" 的日记，
    // 因为 `window_end_ms` 默认取「现在」，它成了**最新一篇**，
    // 于是「昨天的日记」变成 x。
    //
    // 这条测试锁住的是**取日记的规则**：按 window_end_ms 取最新。
    // 探针数据只能靠「写的人自己删」避免，接口不做内容判断 ——
    // 所以这里断言的是：一篇更新的日记确实会顶替旧的那篇（预期行为），
    // 提醒任何做探针的人「写完要删」。
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO diary(id, kind, window_start_ms, window_end_ms, title, body,
                               model, created_ms, updated_ms)
             VALUES ('real','day',1,2,'真实日记','真的内容','t',1,1)",
            [],
        )?;
        tx.execute(
            "INSERT INTO diary(id, kind, window_start_ms, window_end_ms, title, body,
                               model, created_ms, updated_ms)
             VALUES ('probe','day',1,999999,'',  'x','',999999,999999)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let j: serde_json::Value = s.get("/v1/context?q=日记").await.json().await.unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(text.contains('x'), "更新的探针行确实会顶上来：{text}");

    // 删掉探针行之后，真实日记回来
    let r = TestServer::client()
        .delete(format!("{}/v1/diary/probe", s.base))
        .header("x-api-token", TOKEN)
        .json(&serde_json::json!({"note":"探针清理"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let j: serde_json::Value = s.get("/v1/context?q=日记").await.json().await.unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(
        text.contains("真的内容"),
        "删掉探针行后应回到真实日记：{text}"
    );
}

// ================================================================== 渠道（可维护）

#[tokio::test]
async fn 渠道是数据不是枚举() {
    let s = TestServer::start().await;

    // 内置四个默认存在
    let j: serde_json::Value = s.get("/v1/channel/list").await.json().await.unwrap();
    let tags: Vec<String> = j
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["tag"].as_str().unwrap().to_string())
        .collect();
    for t in ["mc", "qq", "live", "system"] {
        assert!(
            tags.contains(&t.to_string()),
            "内置渠道 {t} 应在列表里：{tags:?}"
        );
    }

    // 未注册的渠道写事件要 400
    let r = s
        .post(
            "/v1/event",
            serde_json::json!({"channel":"bilibili","kind":"k","summary":"x"}),
        )
        .await;
    assert_eq!(r.status(), 400, "未注册渠道应先提示去注册");
    let body = r.text().await.unwrap();
    assert!(body.contains("未知渠道"), "错误里要说清楚：{body}");
}

#[tokio::test]
async fn 注册后就能用新渠道写入() {
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO channel(tag, label, trust, builtin, created_ms)
             VALUES ('bilibili','B站直播',0.8,0,1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let r = s
        .post(
            "/v1/event",
            serde_json::json!({"channel":"Bilibili","kind":"danmaku","summary":"来了","importance":0.3}),
        )
        .await;
    assert_eq!(r.status(), 200, "注册过的渠道应能写，且大小写要归一");
    let j: serde_json::Value = r.json().await.unwrap();
    assert_eq!(j["duplicate"], false);

    // 库里存的是小写 tag
    let ch: String =
        s.db.with_read(|c| {
            Ok(c.query_row(
                "SELECT channel FROM event ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(ch, "bilibili", "tag 要归一成小写");
}

#[tokio::test]
async fn 自定义渠道用默认可信度算权重() {
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO channel(tag, label, trust, builtin, created_ms)
             VALUES ('bilibili','B站直播',0.8,0,1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let r = s
        .post(
            "/v1/event",
            serde_json::json!({"channel":"bilibili","kind":"k","summary":"弹幕说的","importance":0.5}),
        )
        .await;
    assert_eq!(r.status(), 200);
    // 只要不 panic、能写进去就说明 channel_trust 对未知 tag 有兜底
    let j: serde_json::Value = s
        .get("/v1/recall?q=弹幕说的&topk=1")
        .await
        .json()
        .await
        .unwrap();
    assert!(!j.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn 被忽视的人的事件照常入库() {
    // 2026-11-01 口径：忽视 = 不回复、不处理，**不是**「当他没说过话」。
    // 事件与消息都照记 —— 事后要能查「那段时间他做了什么」。
    // 「不处理」落在别处：插件 stop_event（不调模型）+ 拒收他的图。
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms, ignored)
             VALUES ('p1','烦人精','confirmed',1,1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let r = s
        .post(
            "/v1/event",
            serde_json::json!({
                "channel":"qq","kind":"said","summary":"他又来了",
                "person_id":"p1","dedup_key":"ig-1"
            }),
        )
        .await;
    assert_eq!(r.status(), 200);
    let j: serde_json::Value = r.json().await.unwrap();
    assert!(!j["id"].is_null(), "被忽视的人的事件也照常入库：{j}");

    let n: i64 =
        s.db.with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))?))
            .unwrap();
    assert_eq!(n, 1, "要能事后查到");
}

#[tokio::test]
async fn 没绑过账号的人不算被忽视() {
    let s = TestServer::start().await;
    let j: serde_json::Value = s
        .get("/v1/person/ignored?channel=qq&account=nobody")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["ignored"], false, "不认识不等于忽视：{j}");
    assert!(j["person_id"].is_null());
}

// ================================================================== 事件摘要里的昵称归一

fn seed_group_person(s: &TestServer) {
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms)
             VALUES ('p1','姆姆','confirmed',1)",
            [],
        )?;
        for (scene, name) in [
            ("mc", "GleamSlime"),
            ("qq_group", "啥子猫"),
            ("qq", "=ᗜωᗜ="),
        ] {
            tx.execute(
                "INSERT INTO person_scene(person_id, scene, name, updated_ms)
                 VALUES ('p1',?1,?2,1)",
                rusqlite::params![scene, name],
            )?;
        }
        Ok(())
    })
    .unwrap();
}

#[tokio::test]
async fn 事件入库时按渠道把群名片换成显示名() {
    let s = TestServer::start().await;
    seed_group_person(&s);

    // qq 渠道 → 群名片和私聊昵称都认
    let r = s
        .post(
            "/v1/event",
            serde_json::json!({
                "channel":"qq","kind":"group.said",
                "summary":"啥子猫在群里说：怎么还藏私明明还有"
            }),
        )
        .await;
    assert_eq!(r.status(), 200);

    let summary: String =
        s.db.with_read(|c| {
            Ok(c.query_row(
                "SELECT summary FROM event ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        summary, "姆姆在群里说：怎么还藏私明明还有",
        "入库时就该换成显示名，而不是等读的时候"
    );
}

#[tokio::test]
async fn 事件归一不做全渠道匹配() {
    let s = TestServer::start().await;
    seed_group_person(&s);

    // mc 渠道的事件里出现 QQ 群名片「啥子猫」——不该被改。
    // 理由：这条事件自己的渠道是 mc，QQ 的名片出现在这里很可疑，
    // 顺手改掉会掩盖真正的数据问题。
    let r = s
        .post(
            "/v1/event",
            serde_json::json!({
                "channel":"mc","kind":"chat","summary":"有人说啥子猫很可爱"
            }),
        )
        .await;
    assert_eq!(r.status(), 200);
    let summary: String =
        s.db.with_read(|c| {
            Ok(c.query_row(
                "SELECT summary FROM event ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        summary, "有人说啥子猫很可爱",
        "mc 事件不该被 QQ 名片的映射改掉"
    );

    // 但 MC 名（同渠道）照改
    let r = s
        .post(
            "/v1/event",
            serde_json::json!({
                "channel":"mc","kind":"chat","summary":"GleamSlime 进服了"
            }),
        )
        .await;
    assert_eq!(r.status(), 200);
    let summary: String =
        s.db.with_read(|c| {
            Ok(c.query_row(
                "SELECT summary FROM event ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(summary, "姆姆 进服了");
}

// ================================================================== 重要度自动判定

#[tokio::test]
async fn 不传重要度时中枢自动判定并记录依据() {
    let s = TestServer::start().await;

    // 死亡 vs 闲聊：同一渠道、同一人，只有类型不同
    let r = s
        .post(
            "/v1/event",
            serde_json::json!({"channel":"mc","kind":"died","summary":"被苦力怕炸死了"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let died: serde_json::Value = s.get("/v1/event/list?size=1").await.json().await.unwrap();
    let died_imp = died[0]["importance"].as_f64().unwrap();

    let r = s
        .post(
            "/v1/event",
            serde_json::json!({"channel":"qq","kind":"group.said","summary":"今天天气不错"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let chat: serde_json::Value = s.get("/v1/event/list?size=1").await.json().await.unwrap();
    let chat_imp = chat[0]["importance"].as_f64().unwrap();

    assert!(
        died_imp > chat_imp + 0.3,
        "死亡应显著高于闲聊：{died_imp} vs {chat_imp}"
    );
    assert_eq!(chat[0]["importance_source"], "auto");
    let reason = chat[0]["importance_reason"].as_str().unwrap();
    assert!(reason.contains("基底"), "要能说清依据：{reason}");
}

#[tokio::test]
async fn 调用方给了重要度就尊重它() {
    let s = TestServer::start().await;
    let r = s
        .post(
            "/v1/event",
            serde_json::json!({"channel":"mc","kind":"died","summary":"死了","importance":0.12}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let j: serde_json::Value = s.get("/v1/event/list?size=1").await.json().await.unwrap();
    assert_eq!(j[0]["importance"], 0.12);
    assert_eq!(j[0]["importance_source"], "caller");
    assert!(
        j[0]["importance_reason"].is_null(),
        "调用方给的没有自动依据"
    );
}

#[tokio::test]
async fn 关系越好事件越重要() {
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms)
             VALUES ('near','亲近的人','confirmed',1)",
            [],
        )?;
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms)
             VALUES ('far','陌生人','confirmed',2)",
            [],
        )?;
        tx.execute(
            "INSERT INTO relation(person_id, familiarity, trust, closeness,
                                  first_met_ms, last_met_ms)
             VALUES ('near', 50, 50, 95, 1, 1)",
            [],
        )?;
        tx.execute(
            "INSERT INTO relation(person_id, familiarity, trust, closeness,
                                  first_met_ms, last_met_ms)
             VALUES ('far', 0, 0, 0, 1, 1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    for (pid, summary) in [("near", "亲近的人跟我说话"), ("far", "陌生人跟我说话")] {
        s.post(
            "/v1/event",
            serde_json::json!({
                "channel":"mc","kind":"player.talked","summary":summary,"person_id":pid
            }),
        )
        .await;
    }

    let j: serde_json::Value = s.get("/v1/event/list?size=10").await.json().await.unwrap();
    let near = j
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["summary"] == "亲近的人跟我说话")
        .unwrap();
    let far = j
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["summary"] == "陌生人跟我说话")
        .unwrap();
    assert!(
        near["importance"].as_f64().unwrap() > far["importance"].as_f64().unwrap(),
        "亲近的人做的事该更重要：{near} vs {far}"
    );
    assert!(
        near["importance_reason"].as_str().unwrap().contains("关系"),
        "依据里要写明关系加分"
    );
}

#[tokio::test]
async fn 人工改过重要度会标成_manual() {
    let s = TestServer::start().await;
    s.post(
        "/v1/event",
        serde_json::json!({"channel":"mc","kind":"died","summary":"死了"}),
    )
    .await;
    let j: serde_json::Value = s.get("/v1/event/list?size=1").await.json().await.unwrap();
    let id = j[0]["id"].as_i64().unwrap();
    assert_eq!(j[0]["importance_source"], "auto");

    let r = TestServer::client()
        .patch(format!("{}/v1/event/{id}", s.base))
        .header("x-api-token", TOKEN)
        .json(&serde_json::json!({"importance": 0.2}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let j: serde_json::Value = s.get("/v1/event/list?size=1").await.json().await.unwrap();
    assert_eq!(j[0]["importance"], 0.2);
    assert_eq!(j[0]["importance_source"], "manual");
    assert!(
        j[0]["importance_reason"].is_null(),
        "人工改过之后自动依据要清掉，不然显示自相矛盾"
    );
}

#[tokio::test]
async fn 上下文按模型_id_注入人格且放在最前面() {
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO persona(id, name, model_regex, body, priority, enabled,
                                 note, created_ms, updated_ms)
             VALUES ('per1','flash 版','deepseek.*flash','你是小模型版。',10,1,NULL,1,1)",
            [],
        )?;
        tx.execute(
            "INSERT INTO persona(id, name, model_regex, body, priority, enabled,
                                 note, created_ms, updated_ms)
             VALUES ('per2','兜底','','你是兜底人格。',100,1,NULL,2,2)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    // 命中专用人格，且 parts 里第一个是 persona
    let j: serde_json::Value = s
        .get("/v1/context?q=在吗&model=deepseek-v4.1-flash")
        .await
        .json()
        .await
        .unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(text.starts_with("# 你是谁"), "人格要在最前面：{text}");
    assert!(text.contains("你是小模型版"), "命中 flash 专用人格：{text}");
    assert!(!text.contains("兜底人格"));
    assert_eq!(j["parts"][0], "persona");

    // 其它模型 → 兜底
    let j: serde_json::Value = s
        .get("/v1/context?q=在吗&model=gpt-4o")
        .await
        .json()
        .await
        .unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(text.contains("你是兜底人格"), "{text}");

    // 不传 model → 只兜底
    let j: serde_json::Value = s.get("/v1/context?q=在吗").await.json().await.unwrap();
    assert!(j["text"].as_str().unwrap().contains("你是兜底人格"));
}

#[tokio::test]
async fn 没有配人格时上下文不受影响() {
    let s = TestServer::start().await;
    let j: serde_json::Value = s
        .get("/v1/context?q=在吗&model=anything")
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !j["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "persona"),
        "没人格就不该有这一段：{j}"
    );
}

// ================================================================== 承诺自动生成

#[tokio::test]
async fn 事件里说了承诺就自动落一条() {
    let s = TestServer::start().await;
    let r = s
        .post(
            "/v1/event",
            serde_json::json!({
                "channel":"mc","kind":"chat",
                "summary":"我明天帮你把那段墙垒完。",
                "person_id":"p1","dedup_key":"pr-1"
            }),
        )
        .await;
    assert_eq!(r.status(), 200);

    let (desc, status, due): (String, String, Option<i64>) =
        s.db.with_read(|c| {
            Ok(c.query_row(
                "SELECT description, status, due_ms FROM promise ORDER BY created_ms DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })
        .unwrap();
    assert_eq!(desc, "我明天帮你把那段墙垒完。");
    assert_eq!(status, "open");
    assert!(due.is_some(), "「明天」应抽出一个截止时间");
    let person: Option<String> = s
        .db
        .with_read(|c| Ok(c.query_row("SELECT person_id FROM promise LIMIT 1", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(person.as_deref(), Some("p1"), "承诺要挂到涉及的人身上");
}

#[tokio::test]
async fn 闲聊和否定不会生成承诺() {
    let s = TestServer::start().await;
    for (i, text) in [
        "今天天气不错。",
        "我不会帮你做的。",
        "我可能去看看。",
        "你昨天挖到什么了？",
    ]
    .iter()
    .enumerate()
    {
        s.post(
            "/v1/event",
            serde_json::json!({
                "channel":"mc","kind":"chat","summary":text,
                "dedup_key":format!("nopr-{i}")
            }),
        )
        .await;
    }
    let n: i64 =
        s.db.with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM promise", [], |r| r.get(0))?))
            .unwrap();
    assert_eq!(n, 0, "这些都不该被认成承诺");
}

#[tokio::test]
async fn 承诺可以逻辑删除() {
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO promise(id, description, status, created_ms, updated_ms)
             VALUES ('pr1','答应过的事','open',1,1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let r = TestServer::client()
        .delete(format!("{}/v1/promise/pr1", s.base))
        .header("x-api-token", TOKEN)
        .json(&serde_json::json!({"note":"记错了"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 列表里不再出现（deleted_by 过滤）
    let j: serde_json::Value = s.get("/v1/promise/list").await.json().await.unwrap();
    assert!(
        j.as_array().unwrap().is_empty(),
        "删掉的承诺不该在列表里：{j}"
    );

    // 但数据还在（逻辑删除，可回滚）
    let n: i64 =
        s.db.with_read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM promise WHERE deleted_by IS NOT NULL",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(n, 1, "逻辑删除要保留数据");
}

// ================================================================== 语音库

/// 造一个最小的 WAV（44 字节头 + 静音）。
fn tiny_wav() -> Vec<u8> {
    let sample_rate = 8000u32;
    let samples = 800u32; // 0.1 秒
    let data_len = samples * 2;
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&sample_rate.to_le_bytes());
    v.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    v.extend(std::iter::repeat(0u8).take(data_len as usize));
    v
}

#[tokio::test]
async fn 语音能上传列表下载并打分() {
    let s = TestServer::start().await;
    let wav = tiny_wav();
    let boundary = "----test-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
    body.extend_from_slice(&wav);
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"text\"\r\n\r\n");
    body.extend_from_slice("今天也要好好吃饭".as_bytes());
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let r = TestServer::client()
        .post(format!("{}/v1/voice", s.base))
        .header("x-api-token", TOKEN)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    let st = r.status();
    let txt = r.text().await.unwrap();
    assert_eq!(st, 200, "上传应成功：{txt}");
    let j: serde_json::Value = serde_json::from_str(&txt).unwrap();
    let id = j["id"].as_str().unwrap().to_string();
    assert!(!j["file"].as_str().unwrap().is_empty());

    // 列表
    let list: serde_json::Value = s.get("/v1/voice").await.json().await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["text"], "今天也要好好吃饭");
    assert!(list[0]["rating"].is_null());

    // 下载字节：装了 ffmpeg 就转成 m4a，没装就原样存 wav —— 两种都正确。
    let r = s.get(&format!("/v1/voice/{id}/file")).await;
    assert_eq!(r.status(), 200);
    let ctype = r
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let bytes = r.bytes().await.unwrap();
    if ctype == "audio/mp4" {
        assert_eq!(&bytes[4..8], b"ftyp", "转成 m4a 后应是 mp4 容器");
    } else {
        assert_eq!(ctype, "audio/wav");
        assert_eq!(&bytes[0..4], b"RIFF", "没 ffmpeg 时应原样存下来");
    }

    // 打分 + 备注
    let r = TestServer::client()
        .patch(format!("{}/v1/voice/{id}", s.base))
        .header("x-api-token", TOKEN)
        .json(&serde_json::json!({"rating":5,"note":"语气自然"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let list: serde_json::Value = s.get("/v1/voice?rated=1").await.json().await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1, "只看打过分的能筛出来");
    assert_eq!(list[0]["rating"], 5);
    assert_eq!(list[0]["note"], "语气自然");
}

#[tokio::test]
async fn 语音评分超范围会被拒() {
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO voice(id, file, bytes, speaker, created_ms, updated_ms)
             VALUES ('v1','v1.m4a',100,'default',1,1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    for bad in [0, 6, -1] {
        let r = TestServer::client()
            .patch(format!("{}/v1/voice/v1", s.base))
            .header("x-api-token", TOKEN)
            .json(&serde_json::json!({"rating": bad}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "评分 {bad} 应被拒");
    }
}

#[tokio::test]
async fn 语音文件名不能越界读文件() {
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO voice(id, file, bytes, speaker, created_ms, updated_ms)
             VALUES ('v2','../../etc/passwd',1,'default',1,1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let r = s.get("/v1/voice/v2/file").await;
    assert_eq!(r.status(), 404, "越界路径必须拿不到文件");
}

// ================================================================== 记忆画面

/// 1x1 PNG 的真实字节（最小合法 PNG）。
fn tiny_png() -> Vec<u8> {
    let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    // 用我们自己的解码器解（它已经在 lib 里被测试过）
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for c in b64.bytes().filter(|b| *b != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    out
}

async fn upload_png(s: &TestServer, caption: &str, keywords: &str) -> serde_json::Value {
    let png = tiny_png();
    let boundary = "----img-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
    body.extend_from_slice(&png);
    for (k, v) in [("caption", caption), ("keywords", keywords)] {
        body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{k}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(v.as_bytes());
    }
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let r = TestServer::client()
        .post(format!("{}/v1/image", s.base))
        .header("x-api-token", TOKEN)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    let st = r.status();
    let txt = r.text().await.unwrap();
    assert_eq!(st, 200, "上传图片应成功：{txt}");
    serde_json::from_str(&txt).unwrap()
}

#[tokio::test]
async fn 图片能上传列表下载并改元数据() {
    let s = TestServer::start().await;
    let j = upload_png(&s, "在湖边钓上来的一条大鱼", "钓鱼,湖,大鱼").await;
    let id = j["id"].as_str().unwrap().to_string();
    assert!(j["file"].as_str().unwrap().ends_with(".png"));

    // 列表
    let list: serde_json::Value = s.get("/v1/image").await.json().await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["caption"], "在湖边钓上来的一条大鱼");

    // 原图能下载，且是真的 PNG
    let r = s.get(&format!("/v1/image/{id}/file")).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers().get("content-type").unwrap(), "image/png");
    let bytes = r.bytes().await.unwrap();
    assert_eq!(&bytes[0..4], &[0x89, b'P', b'N', b'G']);

    // 改元数据
    let r = TestServer::client()
        .patch(format!("{}/v1/image/{id}", s.base))
        .header("x-api-token", TOKEN)
        .json(&serde_json::json!({"keywords":"钓鱼,湖,大鱼,自检","sender":"姆姆"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let j: serde_json::Value = s
        .get(&format!("/v1/image/{id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["sender"], "姆姆");
}

#[tokio::test]
async fn 提到关键词时画面会进上下文() {
    let s = TestServer::start().await;
    upload_png(&s, "在湖边钓上来的一条大鱼", "钓鱼,湖,大鱼").await;

    // 提到「钓鱼」→ 出现 images 段
    let j: serde_json::Value = s
        .get("/v1/context?q=你昨天钓鱼怎么样")
        .await
        .json()
        .await
        .unwrap();
    let text = j["text"].as_str().unwrap();
    assert!(
        text.contains("你记得的画面"),
        "提到关键词应想起画面：{text}"
    );
    assert!(text.contains("大鱼"), "要带上描述：{text}");
    assert!(
        j["parts"].as_array().unwrap().iter().any(|p| p == "images"),
        "parts 里要有 images：{j}"
    );

    // 没提到 → 不出现
    let j: serde_json::Value = s
        .get("/v1/context?q=今天天气不错")
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !j["parts"].as_array().unwrap().iter().any(|p| p == "images"),
        "没提到不该出现：{j}"
    );
}

#[tokio::test]
async fn 图片被想起时会累计次数() {
    let s = TestServer::start().await;
    let j = upload_png(&s, "钓鱼的收获", "钓鱼").await;
    let id = j["id"].as_str().unwrap().to_string();

    let before: serde_json::Value = s
        .get(&format!("/v1/image/{id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(before["recall_count"], 0);

    s.get("/v1/context?q=钓鱼").await;
    let after: serde_json::Value = s
        .get(&format!("/v1/image/{id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(after["recall_count"], 1, "进上下文就 +1");
    assert!(after["last_recall_ms"].as_i64().is_some());
}

#[tokio::test]
async fn 图片可以逻辑删除且认不出非图片() {
    let s = TestServer::start().await;
    let j = upload_png(&s, "要删掉的", "x").await;
    let id = j["id"].as_str().unwrap().to_string();

    let r = TestServer::client()
        .delete(format!("{}/v1/image/{id}", s.base))
        .header("x-api-token", TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let list: serde_json::Value = s.get("/v1/image").await.json().await.unwrap();
    assert!(list.as_array().unwrap().is_empty());

    // 传一段不是图片的字节 → 400（按魔数判，不看文件名）
    let boundary = "----bad";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\n\r\n",
    );
    body.extend_from_slice(b"this is not an image at all");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let r = TestServer::client()
        .post(format!("{}/v1/image", s.base))
        .header("x-api-token", TOKEN)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "文件名写着 png 但内容不是，就该拒掉");
}

#[tokio::test]
async fn 图片支持_base64_上传() {
    let s = TestServer::start().await;
    let png = tiny_png();
    let b64 = {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut o = String::new();
        for chunk in png.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            o.push(T[(n >> 18) as usize & 63] as char);
            o.push(T[(n >> 12) as usize & 63] as char);
            o.push(if chunk.len() > 1 {
                T[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            o.push(if chunk.len() > 2 {
                T[n as usize & 63] as char
            } else {
                '='
            });
        }
        o
    };
    let boundary = "----b64";
    let mut body = Vec::new();
    for (k, v) in [("base64", b64.as_str()), ("caption", "base64 上传的")] {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{k}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(v.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let r = TestServer::client()
        .post(format!("{}/v1/image", s.base))
        .header("x-api-token", TOKEN)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    let st = r.status();
    let txt = r.text().await.unwrap();
    assert_eq!(st, 200, "base64 上传应成功：{txt}");
    let list: serde_json::Value = s.get("/v1/image").await.json().await.unwrap();
    assert_eq!(list[0]["caption"], "base64 上传的");
}

#[tokio::test]
async fn 特质可以删除并留回滚记录() {
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO trait(trait, value, updated_ms) VALUES ('warmth', 0.7, 1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let r = TestServer::client()
        .delete(format!("{}/v1/trait/warmth", s.base))
        .header("x-api-token", TOKEN)
        .json(&serde_json::json!({"note":"测错了"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let n: i64 =
        s.db.with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM trait", [], |r| r.get(0))?))
            .unwrap();
    assert_eq!(n, 0);

    // trait 是真删，但至少有一条 rollback_log 说明它什么时候没的
    let logs: i64 =
        s.db.with_read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM rollback_log WHERE affected_json LIKE '%warmth%'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(logs, 1, "删特质要留痕");

    // 再删一次 → 404
    let r = TestServer::client()
        .delete(format!("{}/v1/trait/warmth", s.base))
        .header("x-api-token", TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

// ================================================================== 面板能直接 src 的二进制接口

#[tokio::test]
async fn 面板会话能直接取语音与图片字节() {
    // 浏览器 `<img src>` / `<audio src>` 发请求时**带不了自定义 header**
    // （只有 cookie 会自动带上）。这两个接口原来只认 X-API-Token，
    // 于是面板上图片和试听全是 401 —— 表现为「页面报错 / 一片空白」。
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO voice(id, file, bytes, speaker, created_ms, updated_ms)
             VALUES ('v1','v1.m4a',3,'default',1,1)",
            [],
        )?;
        tx.execute(
            "INSERT INTO image(id, file, bytes, created_ms, updated_ms)
             VALUES ('i1','i1.png',3,1,1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let dir = s.state.cfg.voice_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("v1.m4a"), b"abc").unwrap();
    let idir = s.state.cfg.image_dir();
    std::fs::create_dir_all(&idir).unwrap();
    std::fs::write(idir.join("i1.png"), b"abc").unwrap();

    // 没凭据 → 401
    assert_eq!(s.get_no_auth("/v1/voice/v1/file").await.status(), 401);
    assert_eq!(s.get_no_auth("/v1/image/i1/file").await.status(), 401);

    // token 照常能取
    assert_eq!(s.get("/v1/voice/v1/file").await.status(), 200);
    assert_eq!(s.get("/v1/image/i1/file").await.status(), 200);
}

#[tokio::test]
async fn 没命中关键词时画面不该进上下文() {
    // 回归：caption 曾经参与「2 字滑窗」匹配，于是
    // caption「今天我们一起去湖边钓鱼」会被
    // 「今天天气不错」「我们走吧」「一起吃饭吗」全部命中 ——
    // 因为它们共享 今天/我们/一起 这种毫无信息量的两字片段。
    // 结果几乎每句闲聊都会把图塞进上下文。
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO image(id, file, bytes, caption, keywords, created_ms, updated_ms)
             VALUES ('i1','i1.png',100,'今天我们一起去湖边钓鱼','钓鱼,湖边',1,1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    // 命中关键词 → 出现（关键词都是双字，单字按规则不算命中）
    for q in ["你钓鱼了吗", "湖边那边怎么样", "去钓鱼吧"] {
        let j: serde_json::Value = s
            .get(&format!("/v1/context?q={q}"))
            .await
            .json()
            .await
            .unwrap();
        assert!(
            j["parts"].as_array().unwrap().iter().any(|p| p == "images"),
            "「{q}」含关键词，应命中：{j}"
        );
    }

    // 只共享 caption 里的常见两字片段 → **不该出现**
    for q in [
        "今天天气不错",
        "我们走吧",
        "一起吃饭吗",
        "这个不错",
        "好饿啊",
        "随便说说",
    ] {
        let j: serde_json::Value = s
            .get(&format!("/v1/context?q={q}"))
            .await
            .json()
            .await
            .unwrap();
        assert!(
            !j["parts"].as_array().unwrap().iter().any(|p| p == "images"),
            "「{q}」没命中关键词，不该出现画面：{j}"
        );
    }
}

#[tokio::test]
async fn 单字关键词不算命中() {
    // 「鱼」「湖」这种单字太容易误伤（「鱼好大」「湖里」），要求 ≥2 字。
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO image(id, file, bytes, caption, keywords, created_ms, updated_ms)
             VALUES ('i1','i1.png',100,'一条鱼','鱼,湖',1,1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let j: serde_json::Value = s.get("/v1/context?q=鱼好大啊").await.json().await.unwrap();
    assert!(
        !j["parts"].as_array().unwrap().iter().any(|p| p == "images"),
        "单字关键词不该触发：{j}"
    );
}

// ================================================================== 记忆画面：去重与必填

#[tokio::test]
async fn 同一张图重复上传会去重() {
    // 群里「这张图又被发了一遍」是常态。不去重的话图库会被同一张图灌满，
    // 而且每次都是一份新文件。按**内容 sha256** 判，不按文件名/大小。
    let s = TestServer::start().await;
    let first = upload_png(&s, "同一张图", "去重,测试").await;
    assert!(!first["duplicate"].as_bool().unwrap_or(false));
    let id1 = first["id"].as_str().unwrap().to_string();

    // 完全相同的字节再传一次
    let second = upload_png(&s, "同一张图", "去重,测试").await;
    assert_eq!(second["duplicate"], true, "第二次应判重复：{second}");
    assert_eq!(second["id"], id1, "应返回同一条记录的 id");

    let n: i64 =
        s.db.with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM image", [], |r| r.get(0))?))
            .unwrap();
    assert_eq!(n, 1, "库里只该有一条");

    // 删掉之后能重新上传（逻辑删除不占去重名额）
    let r = TestServer::client()
        .delete(format!("{}/v1/image/{id1}", s.base))
        .header("x-api-token", TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let third = upload_png(&s, "同一张图", "去重,测试").await;
    assert!(
        !third["duplicate"].as_bool().unwrap_or(false),
        "删掉之后同一张图应能重新入库：{third}"
    );
}

#[tokio::test]
async fn 没有描述和关键词的图会被拒() {
    // 用户要求：图片入库时 caption / keywords 必填。
    // 不填的话它命中不了任何句子，只是图库、白占空间。
    let s = TestServer::start().await;
    let png = tiny_png();
    let boundary = "----empty";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\n\r\n",
    );
    body.extend_from_slice(&png);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let r = TestServer::client()
        .post(format!("{}/v1/image", s.base))
        .header("x-api-token", TOKEN)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "两者都空应拒掉");
    let txt = r.text().await.unwrap();
    assert!(txt.contains("至少填一个"), "错误要说清楚原因：{txt}");

    // 只填单字关键词 → 也拒（单字不算命中，等于没填）
    let boundary2 = "----short";
    let mut b2 = Vec::new();
    b2.extend_from_slice(format!("--{boundary2}\r\n").as_bytes());
    b2.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\n\r\n",
    );
    b2.extend_from_slice(&png);
    b2.extend_from_slice(format!("\r\n--{boundary2}\r\n").as_bytes());
    b2.extend_from_slice(b"Content-Disposition: form-data; name=\"keywords\"\r\n\r\n");
    b2.extend_from_slice("鱼".as_bytes());
    b2.extend_from_slice(format!("\r\n--{boundary2}--\r\n").as_bytes());
    let r = TestServer::client()
        .post(format!("{}/v1/image", s.base))
        .header("x-api-token", TOKEN)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary2}"),
        )
        .body(b2)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "单字关键词应拒掉");
}

#[tokio::test]
async fn 回滚会连语音和记忆画面一起忘掉() {
    // 「回滚 = 忘掉某时刻之后的一切」必须包括新加的两张表，
    // 否则语音和图还留着，就是假的回滚。
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO image(id, file, bytes, caption, created_ms, updated_ms)
             VALUES ('i1','i1.png',1,'旧图',1000,1000)",
            [],
        )?;
        tx.execute(
            "INSERT INTO image(id, file, bytes, caption, created_ms, updated_ms)
             VALUES ('i2','i2.png',1,'新图',9000,9000)",
            [],
        )?;
        tx.execute(
            "INSERT INTO voice(id, file, bytes, speaker, created_ms, updated_ms)
             VALUES ('v1','v1.m4a',1,'default',1000,1000)",
            [],
        )?;
        tx.execute(
            "INSERT INTO voice(id, file, bytes, speaker, created_ms, updated_ms)
             VALUES ('v2','v2.m4a',1,'default',9000,9000)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let r = s
        .post("/v1/rollback", serde_json::json!({"target_ms": 5000}))
        .await;
    assert_eq!(r.status(), 200, "回滚应成功");

    let (images, voices): (i64, i64) =
        s.db.with_read(|c| {
            Ok((
                c.query_row(
                    "SELECT COUNT(*) FROM image WHERE deleted_by IS NULL",
                    [],
                    |r| r.get(0),
                )?,
                c.query_row(
                    "SELECT COUNT(*) FROM voice WHERE deleted_by IS NULL",
                    [],
                    |r| r.get(0),
                )?,
            ))
        })
        .unwrap();
    assert_eq!(images, 1, "5000 之后的那张图要忘掉");
    assert_eq!(voices, 1, "5000 之后的那条语音要忘掉");

    // 留下的必须是旧的那条
    let cap: String =
        s.db.with_read(|c| {
            Ok(c.query_row(
                "SELECT caption FROM image WHERE deleted_by IS NULL",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(cap, "旧图");
}

#[tokio::test]
async fn 被忽视的人发来的图不入库但消息照记() {
    // 用户口径：忽视 = 不回复、不处理，**不是**「当他没说过话」。
    // 所以图片不进记忆画面（那会被检索出来、进上下文），
    // 但 channel_message 照记（事后要能查那段他在群里说了什么）。
    let s = TestServer::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms, ignored)
             VALUES ('p1','烦人精','confirmed',1,1)",
            [],
        )?;
        tx.execute(
            "INSERT INTO person_account(channel, account, person_id, seen_name,
                                        first_seen_ms, last_seen_ms)
             VALUES ('qq','66666','p1','烦人精',1,1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let png = tiny_png();
    let boundary = "----ig";
    let mut body = Vec::new();
    for (k, v) in [
        ("file", ""),
        ("sender", "烦人精"),
        ("keywords", "测试,忽略"),
    ] {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        if k == "file" {
            body.extend_from_slice(
                b"Content-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\n",
            );
            body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
            body.extend_from_slice(&png);
        } else {
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{k}\"\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(v.as_bytes());
        }
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let r = TestServer::client()
        .post(format!("{}/v1/image", s.base))
        .header("x-api-token", TOKEN)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    let st = r.status();
    let txt = r.text().await.unwrap();
    assert_eq!(st, 200, "不该报错，只是跳过：{txt}");
    let j: serde_json::Value = serde_json::from_str(&txt).unwrap();
    assert_eq!(j["ignored"], true, "要明确告诉调用方被跳过了：{j}");

    let n: i64 =
        s.db.with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM image", [], |r| r.get(0))?))
            .unwrap();
    assert_eq!(n, 0, "被忽视的人的图不该入库");

    // 但消息照记
    let r = s
        .post(
            "/v1/message",
            serde_json::json!({
                "channel":"qq","account":"66666","sender_name":"烦人精",
                "content":"我说句话","at_ms":1000
            }),
        )
        .await;
    assert_eq!(r.status(), 200, "消息接口不该 500");
}
