//! 管理后台的集成测试：登录、会话、分页、详情接口。
//!
//! 这些测试假定的最重要一条：**没登录不能读到任何记忆**。
//! 面板监听内网时，漏一次就是所有人的隐私。

use std::sync::Arc;

use bluewhale_memory::admin::{router as admin_router, AdminState};
use bluewhale_memory::config::{BindScope, Config};
use bluewhale_memory::db::Db;
use bluewhale_memory::http::{router as api_router, AppState};

const PW: &str = "test-panel-pass";

struct S {
    base: String,
    db: Arc<Db>,
    _dir: tempfile::TempDir,
}

impl S {
    async fn start() -> Self {
        Self::start_with(PW).await
    }

    async fn start_with(pass: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            data_dir: dir.path().to_path_buf(),
            token: "test-token-0123456789abcdef".into(),
            bind_scope: BindScope::Custom,
            bind_addr: "192.168.1.10".parse().unwrap(),
            panel_user: "admin".into(),
            panel_password: pass.into(),
            ..Default::default()
        };
        cfg.validate().unwrap();
        let cfg = Arc::new(cfg);

        let db = Db::open(cfg.db_path()).unwrap();
        let app_state = AppState::new(db.clone(), cfg.clone()).unwrap();
        // 用完整路由：线上跑的就是 `full_router`（http + legacy + admin）。
        // 只 merge http/admin 两套会让 `/v1/prune` 这类接口在测试里变成 404，
        // 于是「它到底是不是 401」这种断言测的是错误的东西。
        let app = bluewhale_memory::http::full_router(app_state.clone());

        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });

        Self {
            base: format!("http://{addr}"),
            db,
            _dir: dir,
        }
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none()) // 自己看 303，不自动跟随
            .build()
            .unwrap()
    }

    /// 登录并返回 Set-Cookie 里的会话。
    async fn login(&self) -> String {
        let r = Self::client()
            .post(format!("{}/login", self.base))
            .form(&[("user", "admin"), ("pass", PW)])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 303, "登录成功应重定向");
        let raw = r
            .headers()
            .get("set-cookie")
            .expect("应设置 cookie")
            .to_str()
            .unwrap()
            .to_string();
        raw.split(';').next().unwrap().to_string()
    }

    async fn get_with(&self, path: &str, cookie: &str) -> reqwest::Response {
        Self::client()
            .get(format!("{}{}", self.base, path))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap()
    }

    async fn get_anon(&self, path: &str) -> reqwest::Response {
        Self::client()
            .get(format!("{}{}", self.base, path))
            .send()
            .await
            .unwrap()
    }
}

fn seed_events(db: &Db, n: usize, channel: &str) {
    db.with_write(|tx| {
        for i in 0..n {
            tx.execute(
                "INSERT INTO event(at_ms, kind, channel, summary, importance, scope)
                 VALUES (?1, 'test', ?2, ?3, 0.5, 'companion')",
                rusqlite::params![
                    bluewhale_memory::db::now_ms() + i as i64,
                    channel,
                    format!("{channel} 事件 {i}")
                ],
            )?;
        }
        Ok(())
    })
    .unwrap();
}

// ================================================================== 登录

#[tokio::test]
async fn 未登录访问首页被重定向到登录页() {
    let s = S::start().await;
    let r = s.get_anon("/").await;
    assert_eq!(r.status(), 303);
    assert!(r
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("/login"));
}

#[tokio::test]
async fn 登录页本身无需登录() {
    let s = S::start().await;
    let r = s.get_anon("/login").await;
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    assert!(html.contains("蓝鲸记忆中枢"));
    assert!(html.contains("password"), "应有口令输入框");
}

#[tokio::test]
async fn 未登录读不到任何后台数据接口() {
    let s = S::start().await;
    seed_events(&s.db, 3, "mc");
    for ep in [
        "/api/admin/overview",
        "/api/admin/events",
        "/api/admin/facts",
        "/api/admin/journal",
        "/api/admin/diaries",
        "/api/admin/persons",
        "/api/admin/snapshots",
        "/api/admin/rollbacks",
    ] {
        let r = s.get_anon(ep).await;
        assert_eq!(r.status(), 401, "{ep} 未登录时必须 401");
    }
}

#[tokio::test]
async fn 错误口令登录失败() {
    let s = S::start().await;
    let r = S::client()
        .post(format!("{}/login", s.base))
        .form(&[("user", "admin"), ("pass", "wrong")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    assert!(
        r.headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("e=1"),
        "应带错误标记"
    );
    assert!(r.headers().get("set-cookie").is_none(), "失败不该发会话");
}

#[tokio::test]
async fn 错误用户名登录失败() {
    let s = S::start().await;
    let r = S::client()
        .post(format!("{}/login", s.base))
        .form(&[("user", "root"), ("pass", PW)])
        .send()
        .await
        .unwrap();
    assert!(r.headers().get("set-cookie").is_none());
}

#[tokio::test]
async fn 登录后可以访问首页与数据() {
    let s = S::start().await;
    seed_events(&s.db, 5, "mc");
    let ck = s.login().await;

    let r = s.get_with("/", &ck).await;
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    assert!(html.contains("管理后台"), "应是后台页面");

    let r = s.get_with("/api/admin/overview", &ck).await;
    assert_eq!(r.status(), 200);
    let j: serde_json::Value = r.json().await.unwrap();
    assert!(j["total_rows"].as_i64().unwrap() >= 5);
}

#[tokio::test]
async fn cookie_带_httponly() {
    let s = S::start().await;
    let r = S::client()
        .post(format!("{}/login", s.base))
        .form(&[("user", "admin"), ("pass", PW)])
        .send()
        .await
        .unwrap();
    let sc = r.headers().get("set-cookie").unwrap().to_str().unwrap();
    assert!(sc.contains("HttpOnly"), "页面脚本不该读得到会话：{sc}");
    assert!(sc.contains("SameSite=Strict"), "防 CSRF：{sc}");
}

#[tokio::test]
async fn 登出后会话失效() {
    let s = S::start().await;
    let ck = s.login().await;
    assert_eq!(s.get_with("/api/admin/overview", &ck).await.status(), 200);

    let r = S::client()
        .post(format!("{}/logout", s.base))
        .header("cookie", &ck)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);

    // 旧的 cookie 从此无效
    assert_eq!(s.get_with("/api/admin/overview", &ck).await.status(), 401);
}

#[tokio::test]
async fn 伪造的会话_id_无效() {
    let s = S::start().await;
    let r = s.get_with("/api/admin/overview", "bw_panel=deadbeef").await;
    assert_eq!(r.status(), 401);
}

// ================================================================== 数据接口

#[tokio::test]
async fn 事件分页可用() {
    let s = S::start().await;
    seed_events(&s.db, 25, "mc");
    let ck = s.login().await;

    let j: serde_json::Value = s
        .get_with("/api/admin/events?page=1&size=10", &ck)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["items"].as_array().unwrap().len(), 10);
    assert_eq!(j["total"], 25);
    assert_eq!(j["pages"], 3);

    let j2: serde_json::Value = s
        .get_with("/api/admin/events?page=3&size=10", &ck)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j2["items"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn 事件按渠道筛选() {
    let s = S::start().await;
    seed_events(&s.db, 4, "mc");
    seed_events(&s.db, 2, "qq");
    let ck = s.login().await;

    let j: serde_json::Value = s
        .get_with("/api/admin/events?channel=qq", &ck)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["total"], 2);
}

#[tokio::test]
async fn 事件关键词搜索() {
    let s = S::start().await;
    seed_events(&s.db, 10, "mc");
    let ck = s.login().await;
    let j: serde_json::Value = s
        .get_with("/api/admin/events?q=事件 3", &ck)
        .await
        .json()
        .await
        .unwrap();
    assert!(j["total"].as_i64().unwrap() >= 1);
}

#[tokio::test]
async fn 分页参数越界被夹住() {
    let s = S::start().await;
    seed_events(&s.db, 5, "mc");
    let ck = s.login().await;
    // size 太大
    let j: serde_json::Value = s
        .get_with("/api/admin/events?page=0&size=99999", &ck)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["page"], 1, "页码 0 应被夹成 1");
    assert_eq!(j["size"], 500, "size 上限 500");
}

#[tokio::test]
async fn 概览给出可下钻的分布() {
    let s = S::start().await;
    seed_events(&s.db, 3, "mc");
    seed_events(&s.db, 2, "qq");
    let ck = s.login().await;
    let j: serde_json::Value = s
        .get_with("/api/admin/overview", &ck)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["by_channel"].as_array().unwrap().len(), 2);
    assert!(!j["daily"].as_array().unwrap().is_empty());
    assert!(!j["tables"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn 身份详情带出全部渠道账号() {
    let s = S::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms)
             VALUES ('p1','小明','confirmed',1)",
            [],
        )?;
        for ch in ["mc", "qq", "live"] {
            tx.execute(
                "INSERT INTO person_account(channel, account, person_id, first_seen_ms, last_seen_ms)
                 VALUES (?1, ?2, 'p1', 1, 1)",
                rusqlite::params![ch, format!("{ch}-1")],
            )?;
        }
        Ok(())
    })
    .unwrap();

    let ck = s.login().await;
    let j: serde_json::Value = s
        .get_with("/api/admin/persons", &ck)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["total"], 1);
    assert_eq!(
        j["items"][0]["accounts"].as_array().unwrap().len(),
        3,
        "同一个人应有 3 个渠道账号"
    );
}

// ================================================================== 隔离

#[tokio::test]
async fn 面板会话不能用于危险运维接口() {
    // 口径（2026-11-01 调整）：
    //   · **数据增删改**（event/fact/journal/diary/message）→ 接受 token **或**面板会话，
    //     这样人在浏览器里改一条记忆不用先去填 token；
    //   · **危险运维**（回滚 / 清理 / 快照 / 绑定身份）→ 仍然只认 token。
    //     这几种一次就能把一批记忆抹掉或改归属，不该由一个可能被复用的浏览器会话触发。
    let s = S::start().await;
    let ck = s.login().await;

    let r = S::client()
        .post(format!("{}/v1/rollback", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"target_ms": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "回滚不该被 cookie 触发");

    let r = S::client()
        .post(format!("{}/v1/prune", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "清理不该被 cookie 触发");

    let r = S::client()
        .post(format!("{}/v1/person/bind", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"channel":"qq","account":"1","person_id":"p1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "身份绑定不该被 cookie 触发");
}

#[tokio::test]
async fn 监听内网且未配口令时后台拒绝访问() {
    // 最危险的一种配置：监听 0.0.0.0 却没设面板口令。
    // 此时必须一律 401，而不是「打开就能看」。
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        data_dir: dir.path().to_path_buf(),
        token: "test-token-0123456789abcdef".into(),
        bind_scope: BindScope::Lan,
        panel_user: String::new(),
        panel_password: String::new(),
        ..Default::default()
    };
    cfg.validate().unwrap();
    let cfg = Arc::new(cfg);
    let db = Db::open(cfg.db_path()).unwrap();
    let app_state = AppState::new(db.clone(), cfg.clone()).unwrap();
    let admin = AdminState::new(app_state.clone(), &cfg);
    let app = api_router(app_state).merge(admin_router(admin));

    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });

    let r = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(format!("http://{addr}/api/admin/overview"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "内网 + 无口令 = 必须拒绝，不能裸奔");
}

// ================================================================== 后台自己的写操作

/// 后台**不需要 API token** 就能拍快照。
///
/// 之前面板让人去填 API token 才能拍，那是把两条鉴权链混为一谈了：
/// 会话 cookie 就是后台的凭据，不该再要一个脚本用的 token。
#[tokio::test]
async fn 后台会话即可拍快照() {
    let s = S::start().await;
    let ck = s.login().await;

    let r = S::client()
        .post(format!("{}/api/admin/snapshot", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"note":"后台测试"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "会话应足够，不该要 API token");
    let j: serde_json::Value = r.json().await.unwrap();
    assert!(j["bytes"].as_i64().unwrap() > 0);
    assert_eq!(j["sha256"].as_str().unwrap().len(), 64);
}

#[tokio::test]
async fn 未登录不能拍快照() {
    let s = S::start().await;
    let r = S::client()
        .post(format!("{}/api/admin/snapshot", s.base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn 身份可以改名与改状态() {
    let s = S::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms)
             VALUES ('p1','旧名字','pending',1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let ck = s.login().await;
    let r = S::client()
        .patch(format!("{}/api/admin/person/p1", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"display_name":"新名字","status":"confirmed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let j: serde_json::Value = s
        .get_with("/api/admin/persons", &ck)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["items"][0]["display_name"], "新名字");
    assert_eq!(j["items"][0]["status"], "confirmed");
}

#[tokio::test]
async fn 身份显示名不能改成空() {
    let s = S::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms)
             VALUES ('p1','名字','pending',1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let ck = s.login().await;
    let r = S::client()
        .patch(format!("{}/api/admin/person/p1", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"display_name":"   "}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn 可以给身份绑渠道账号() {
    let s = S::start().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms)
             VALUES ('p1','小明','confirmed',1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let ck = s.login().await;
    let r = S::client()
        .post(format!("{}/api/admin/person/p1/account", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"channel":"qq","account":"10001","seen_name":"小明"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let j: serde_json::Value = s
        .get_with("/api/admin/persons", &ck)
        .await
        .json()
        .await
        .unwrap();
    let accts = j["items"][0]["accounts"].as_array().unwrap();
    assert_eq!(accts.len(), 1);
    assert_eq!(accts[0]["channel"], "qq");
}

#[tokio::test]
async fn 身份不存在时报_404() {
    let s = S::start().await;
    let ck = s.login().await;
    let r = S::client()
        .patch(format!("{}/api/admin/person/nope", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"display_name":"x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

// ================================================================== 关系

/// 造一个身份（可选带渠道账号）。
fn seed_person(db: &Db, id: &str, name: &str, accounts: &[(&str, &str, Option<&str>)]) {
    db.with_write(|tx| {
        tx.execute(
            "INSERT INTO person(person_id, display_name, status, created_ms)
             VALUES (?1, ?2, 'confirmed', 1)",
            rusqlite::params![id, name],
        )?;
        for (ch, acct, seen) in accounts {
            tx.execute(
                "INSERT INTO person_account(channel, account, person_id, seen_name,
                                            first_seen_ms, last_seen_ms)
                 VALUES (?1, ?2, ?3, ?4, 1, 1)",
                rusqlite::params![ch, acct, id, seen],
            )?;
        }
        Ok(())
    })
    .unwrap();
}

fn seed_group_message(db: &Db, account: &str, sender: &str, at_ms: i64) {
    db.with_write(|tx| {
        tx.execute(
            "INSERT INTO channel_message(channel, group_id, account, sender_name, content, at_ms)
             VALUES ('qq','g1',?1,?2,'hi',?3)",
            rusqlite::params![account, sender, at_ms],
        )?;
        Ok(())
    })
    .unwrap();
}

async fn relation_items(s: &S, ck: &str) -> serde_json::Value {
    s.get_with("/api/admin/relations", ck)
        .await
        .json()
        .await
        .unwrap()
}

fn scene<'a>(j: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
    j["items"][0]["scenes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["scene"] == key)
        .unwrap_or_else(|| panic!("场景 {key} 不在返回里：{j}"))
}

#[tokio::test]
async fn 关系可以新建并带场景称呼() {
    let s = S::start().await;
    let ck = s.login().await;

    let r = S::client()
        .post(format!("{}/api/admin/relations", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({
            "display_name": "小明",
            "closeness": 45,
            "familiarity": 12,
            "trust": 7,
            "note": "一起盖过房",
            "scenes": {
                "mc": "小明MC",
                "qq": "小明QQ",
                "qq_group": "群里的小明"
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "新建关系应成功");
    let j: serde_json::Value = r.json().await.unwrap();
    let pid = j["person_id"].as_str().unwrap().to_string();
    assert!(!pid.is_empty());

    let j = relation_items(&s, &ck).await;
    assert_eq!(j["items"][0]["display_name"], "小明");
    assert_eq!(j["items"][0]["closeness"], 45);
    assert_eq!(j["items"][0]["note"], "一起盖过房");
    assert_eq!(j["items"][0]["stage"], "熟识", "45 应落在熟识");
    assert_eq!(scene(&j, "mc")["name"], "小明MC");
    assert_eq!(scene(&j, "mc")["source"], "manual");
    assert_eq!(scene(&j, "qq_group")["name"], "群里的小明");
    assert_eq!(scene(&j, "live")["source"], "none", "没填也没来源就是 none");
}

#[tokio::test]
async fn 关系可以挂在已有身份上() {
    let s = S::start().await;
    seed_person(&s.db, "p1", "小红", &[]);
    let ck = s.login().await;

    let r = S::client()
        .post(format!("{}/api/admin/relations", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"person_id": "p1", "closeness": 3}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let j = relation_items(&s, &ck).await;
    assert_eq!(j["items"][0]["person_id"], "p1");
    assert_eq!(j["items"][0]["display_name"], "小红");
    let n: i64 =
        s.db.with_read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM person WHERE person_id = 'p1'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(n, 1, "不该重复建人");
}

#[tokio::test]
async fn 同一个身份不能建两条关系() {
    let s = S::start().await;
    seed_person(&s.db, "p1", "小红", &[]);
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO relation(person_id, familiarity, trust, closeness,
                                  first_met_ms, last_met_ms)
             VALUES ('p1', 0, 0, 0, 1, 1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let ck = s.login().await;

    let r = S::client()
        .post(format!("{}/api/admin/relations", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"person_id": "p1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "重复新建应被拒，而不是覆盖历史");
}

#[tokio::test]
async fn 关系不存在时改不动() {
    let s = S::start().await;
    let ck = s.login().await;
    let r = S::client()
        .patch(format!("{}/api/admin/relations/nope", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"closeness": 10}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn 关系数值被夹到_0_到_100() {
    let s = S::start().await;
    seed_person(&s.db, "p1", "小红", &[]);
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO relation(person_id, familiarity, trust, closeness,
                                  first_met_ms, last_met_ms)
             VALUES ('p1', 0, 0, 0, 1, 1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let ck = s.login().await;

    let r = S::client()
        .patch(format!("{}/api/admin/relations/p1", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"closeness": 150, "familiarity": -5, "trust": 42}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let j = relation_items(&s, &ck).await;
    assert_eq!(j["items"][0]["closeness"], 100, "大于 100 夹到 100");
    assert_eq!(j["items"][0]["familiarity"], 0, "小于 0 夹到 0");
    assert_eq!(j["items"][0]["trust"], 42);
    assert_eq!(j["items"][0]["stage"], "交心");
}

#[tokio::test]
async fn 场景称呼可以清空回到自动推导() {
    let s = S::start().await;
    seed_person(&s.db, "p1", "小红", &[("mc", "uuid-1", Some("小红MC"))]);
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO relation(person_id, familiarity, trust, closeness,
                                  first_met_ms, last_met_ms)
             VALUES ('p1', 0, 0, 0, 1, 1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let ck = s.login().await;

    // 没填过 → 从渠道账号自动推导
    let j = relation_items(&s, &ck).await;
    assert_eq!(scene(&j, "mc")["name"], "小红MC");
    assert_eq!(scene(&j, "mc")["source"], "derived");
    assert_eq!(scene(&j, "mc")["hint"], "uuid-1", "提示里要有渠道账号");

    // 人工覆盖
    let r = S::client()
        .patch(format!("{}/api/admin/relations/p1", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"scenes": {"mc": "游戏里的小红"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let j = relation_items(&s, &ck).await;
    assert_eq!(scene(&j, "mc")["name"], "游戏里的小红");
    assert_eq!(scene(&j, "mc")["source"], "manual");

    // 传空串 = 删掉人工值，回到自动
    let r = S::client()
        .patch(format!("{}/api/admin/relations/p1", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"scenes": {"mc": ""}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let j = relation_items(&s, &ck).await;
    assert_eq!(scene(&j, "mc")["name"], "小红MC");
    assert_eq!(scene(&j, "mc")["source"], "derived");
}

#[tokio::test]
async fn 群名片取最近一条群消息() {
    let s = S::start().await;
    seed_person(&s.db, "p1", "小红", &[("qq", "10001", Some("小红"))]);
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO relation(person_id, familiarity, trust, closeness,
                                  first_met_ms, last_met_ms)
             VALUES ('p1', 0, 0, 0, 1, 1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    seed_group_message(&s.db, "10001", "旧群名片", 1_000);
    seed_group_message(&s.db, "10001", "新群名片", 2_000);

    let ck = s.login().await;
    let j = relation_items(&s, &ck).await;
    assert_eq!(scene(&j, "qq_group")["name"], "新群名片");
    assert_eq!(scene(&j, "qq_group")["source"], "derived");
    assert_eq!(scene(&j, "qq")["name"], "小红", "私聊名字仍来自账号");
}

#[tokio::test]
async fn 未知场景被拒绝() {
    let s = S::start().await;
    seed_person(&s.db, "p1", "小红", &[]);
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO relation(person_id, familiarity, trust, closeness,
                                  first_met_ms, last_met_ms)
             VALUES ('p1', 0, 0, 0, 1, 1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let ck = s.login().await;

    let r = S::client()
        .patch(format!("{}/api/admin/relations/p1", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"scenes": {"telegram": "x"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "拼错的场景键必须当场报，不能静默写进库");
}

#[tokio::test]
async fn 未登录不能改关系() {
    let s = S::start().await;
    let r = S::client()
        .post(format!("{}/api/admin/relations", s.base))
        .json(&serde_json::json!({"display_name": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    let r = S::client()
        .patch(format!("{}/api/admin/relations/p1", s.base))
        .json(&serde_json::json!({"closeness": 10}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn 新建关系时显示名不能为空() {
    let s = S::start().await;
    let ck = s.login().await;
    let r = S::client()
        .post(format!("{}/api/admin/relations", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"display_name": "   "}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn 身份可以改各场景称呼() {
    // 场景称呼的编辑入口在「身份」页；关系页只读展示同一份数据。
    let s = S::start().await;
    seed_person(&s.db, "p1", "小红", &[("mc", "uuid-1", Some("小红MC"))]);
    let ck = s.login().await;

    // 初始：MC 自动推导，QQ 群没有来源
    let j: serde_json::Value = s
        .get_with("/api/admin/persons", &ck)
        .await
        .json()
        .await
        .unwrap();
    let scenes = j["items"][0]["scenes"].as_array().unwrap();
    let mc = scenes.iter().find(|x| x["scene"] == "mc").unwrap();
    assert_eq!(mc["name"], "小红MC");
    assert_eq!(mc["source"], "derived");

    // 人工填两个场景；空串保持自动
    let r = S::client()
        .patch(format!("{}/api/admin/person/p1", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"scenes": {
            "mc": "游戏里的小红",
            "qq_group": "群名片小红",
            "qq": ""
        }}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let j: serde_json::Value = s
        .get_with("/api/admin/persons", &ck)
        .await
        .json()
        .await
        .unwrap();
    let scenes = j["items"][0]["scenes"].as_array().unwrap();
    let pick = |k: &str| scenes.iter().find(|x| x["scene"] == k).unwrap().clone();
    assert_eq!(pick("mc")["name"], "游戏里的小红");
    assert_eq!(pick("mc")["source"], "manual");
    assert_eq!(pick("qq_group")["name"], "群名片小红");
    assert_eq!(pick("qq")["source"], "none", "空串不该写进库");
}

#[tokio::test]
async fn 身份场景称呼拒绝未知键与未登录() {
    let s = S::start().await;
    seed_person(&s.db, "p1", "小红", &[]);
    let ck = s.login().await;

    let r = S::client()
        .patch(format!("{}/api/admin/person/p1", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"scenes": {"telegram": "x"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    let r = S::client()
        .patch(format!("{}/api/admin/person/p1", s.base))
        .json(&serde_json::json!({"scenes": {"mc": "x"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn 可以新建身份() {
    let s = S::start().await;
    let ck = s.login().await;

    let r = S::client()
        .post(format!("{}/api/admin/persons", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({
            "display_name": "姆姆",
            "status": "pending",
            "note": "还没出现过的人",
            "scenes": {"qq_group": "啥子猫"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "新建身份应成功");
    let j: serde_json::Value = r.json().await.unwrap();
    let pid = j["person_id"].as_str().unwrap().to_string();
    assert!(!pid.is_empty());

    let j: serde_json::Value = s
        .get_with("/api/admin/persons", &ck)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["total"], 1);
    let p = &j["items"][0];
    assert_eq!(p["display_name"], "姆姆");
    assert_eq!(p["status"], "pending");
    assert_eq!(p["note"], "还没出现过的人");
    let sc = p["scenes"].as_array().unwrap();
    let g = sc.iter().find(|x| x["scene"] == "qq_group").unwrap();
    assert_eq!(g["name"], "啥子猫");
    assert_eq!(g["source"], "manual");
}

#[tokio::test]
async fn 新建身份时显示名不能为空也不能未登录() {
    let s = S::start().await;
    let ck = s.login().await;

    let r = S::client()
        .post(format!("{}/api/admin/persons", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"display_name": "   "}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    let r = S::client()
        .post(format!("{}/api/admin/persons", s.base))
        .json(&serde_json::json!({"display_name": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

// ================================================================== 面板写操作不再要 token

#[tokio::test]
async fn 面板会话可以直接改事件不需要_api_token() {
    // 用户口径：面板里改数据不该再让填 token。
    // 这批写口（/v1/event 等）现在同时接受 token 或面板会话。
    let s = S::start().await;
    let ck = s.login().await;
    s.db.with_write(|tx| {
        tx.execute(
            "INSERT INTO event(at_ms, kind, channel, summary, importance, scope)
             VALUES (1,'test','mc','原始摘要',0.5,'companion')",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let r = S::client()
        .patch(format!("{}/v1/event/1", s.base))
        .header("cookie", &ck)
        .json(&serde_json::json!({"summary":"面板改过的","importance":0.9}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "带会话就该能改");

    let j: serde_json::Value = s
        .get_with("/api/admin/events", &ck)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["items"][0]["summary"], "面板改过的");
}

#[tokio::test]
async fn 没有会话也没有_token_仍然拒绝() {
    // 放宽的是「接受 cookie」，不是「谁都能写」。
    let s = S::start().await;
    let r = S::client()
        .post(format!("{}/v1/event", s.base))
        .json(&serde_json::json!({"channel":"mc","kind":"x","summary":"y"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // 伪造一个 cookie 也不行
    let r = S::client()
        .patch(format!("{}/v1/event/1", s.base))
        .header("cookie", "bw_panel=deadbeef")
        .json(&serde_json::json!({"summary":"x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "假会话必须被拒");
}
