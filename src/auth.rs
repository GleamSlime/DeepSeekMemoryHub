//! 面板登录会话。
//!
//! # 为什么是 cookie，不是 `?token=`
//!
//! API token 是给**脚本**用的：每次请求都带，简单直接。但人在浏览器里不能用它——
//! token 拼进 URL 会进浏览器历史、进每个外链的 Referer、进沿途所有代理日志。
//!
//! 所以面板自己一套机制：一个不透明的随机 id 放在 `HttpOnly` cookie 里，
//! 页面脚本读不到，URL 始终干净。
//!
//! # 会话落盘（2026-10-29 改）
//!
//! 早先会话只在内存 map 里，理由是「重启就登出」算安全特性。实际用下来
//! 代价太大：换二进制、改配置、机器重启都要重登，而这些事很频繁。
//!
//! 现在会话存进 `panel_session` 表，**重启不掉线**，有效期 1 年。
//! 风险没有实质增加：能读到本地 DB 的人本来就能用 API token 把所有记忆读走。
//!
//! # 明确不做的事
//! - **不在磁盘上存口令哈希**。口令来自操作者自己的配置文件，
//!   在内存里再哈希一遍只能防「已经能读配置文件的人」，没有意义。
//! - **不做锁定表**。密码错时**固定延时** 400 ms。这个端口按设计只在内网，
//!   暴力破解不是威胁模型；「未授权读到别人的关系分」才是。
//!
//! # 与 API token 的关系
//!
//! 两者互不影响：cookie 只让**面板**能用，`X-API-Token` 只让**API**能用。
//! 一个泄了不会带出另一个。

use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::db::Db;

/// 会话有效期：**一年**。
///
/// 操作者要求「长期有效，不要 8 小时」。一年足够长到等同于「记住我」，
/// 又仍然是一个有限的数字——永久会话意味着 cookie 一旦泄露永远有效。
pub const SESSION_TTL_MS: i64 = 365 * 24 * 3600 * 1000;

/// 密码错误时的固定延时。让脚本化猜测变得没意义，又不需要锁定表。
pub const FAIL_DELAY_MS: u64 = 400;

/// cookie 名。刻意不叫 token——它不是 API token，不该被混为一谈。
pub const COOKIE_NAME: &str = "bw_panel";

/// 登录状态机。会话落盘，进程重启不掉线。
pub struct Auth {
    /// 用户名。空 = 未配置登录，面板不需要登录（仅当也监听回环时安全）。
    user: String,
    /// 口令的 sha256。存哈希不是为了防谁，是为了不把明文常驻内存。
    pass_hash: Option<[u8; 32]>,
    /// 会话表。为 None 时退化成「不持久化」——测试里会用。
    db: Option<Arc<Db>>,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("user", &self.user)
            .field("login_required", &self.login_required())
            .finish()
    }
}

impl Auth {
    /// `user`/`pass` 都为空 = 不需要登录。
    pub fn new(user: &str, pass: &str) -> Self {
        Self::with_db(user, pass, None)
    }

    /// 带持久化的构造：会话写进 `panel_session` 表。
    pub fn with_db(user: &str, pass: &str, db: Option<Arc<Db>>) -> Self {
        let (user, pass_hash) = if user.is_empty() || pass.is_empty() {
            (String::new(), None)
        } else {
            (user.to_string(), Some(sha256(pass.as_bytes())))
        };
        Self {
            user,
            pass_hash,
            db,
        }
    }

    pub fn login_required(&self) -> bool {
        self.pass_hash.is_some()
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    /// 校验用户名与口令。
    ///
    /// 用**定长比较**哈希值，避免通过响应时间逐字节猜口令。
    pub fn check(&self, user: &str, pass: &str) -> bool {
        let Some(want) = self.pass_hash else {
            return true; // 没配置登录 = 任何人都是「已登录」
        };
        if !constant_time_eq(user.as_bytes(), self.user.as_bytes()) {
            return false;
        }
        constant_time_eq(&sha256(pass.as_bytes()), &want)
    }

    /// 新建会话，返回 cookie 里该放的 id。
    pub fn issue(&self, user: &str) -> String {
        let id = random_id();
        let now = crate::db::now_ms();
        if let Some(db) = &self.db {
            // 顺手清理过期会话，避免表无限涨。
            let _ = db.with_write(|tx| {
                tx.execute(
                    "DELETE FROM panel_session WHERE expires_ms <= ?1",
                    rusqlite::params![now],
                )?;
                tx.execute(
                    "INSERT INTO panel_session(sid, user, created_ms, expires_ms)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![id, user, now, now + SESSION_TTL_MS],
                )?;
                Ok(())
            });
        }
        id
    }

    /// 校验会话，返回用户名。
    pub fn verify(&self, sid: &str) -> Option<String> {
        if sid.is_empty() {
            return None;
        }
        let db = self.db.as_ref()?;
        let now = crate::db::now_ms();
        db.with_read(|c| {
            let r: Option<String> = c
                .query_row(
                    "SELECT user FROM panel_session WHERE sid = ?1 AND expires_ms > ?2",
                    rusqlite::params![sid, now],
                    |r| r.get(0),
                )
                .ok();
            Ok(r)
        })
        .ok()
        .flatten()
    }

    pub fn logout(&self, sid: &str) {
        if sid.is_empty() {
            return;
        }
        if let Some(db) = &self.db {
            let _ = db.with_write(|tx| {
                tx.execute(
                    "DELETE FROM panel_session WHERE sid = ?1",
                    rusqlite::params![sid],
                )?;
                Ok(())
            });
        }
    }

    /// 活跃会话数，面板上能看到「有几个地方登录着」。
    pub fn active_sessions(&self) -> usize {
        let Some(db) = &self.db else { return 0 };
        let now = crate::db::now_ms();
        db.with_read(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM panel_session WHERE expires_ms > ?1",
                rusqlite::params![now],
                |r| r.get(0),
            )?;
            Ok(n as usize)
        })
        .unwrap_or(0)
    }
}

fn sha256(b: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b);
    h.finalize().into()
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

fn random_id() -> String {
    // 不用 rand crate：/dev/urandom 在 macOS 与 Linux 上都有，
    // 且少一个依赖。失败就退到时间 + 地址，虽弱但不会 panic。
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let mut buf = [0u8; 32];
            if f.read_exact(&mut buf).is_ok() {
                return hex(&buf);
            }
        }
    }
    let t = crate::db::now_ms();
    let p = std::process::id();
    let addr = &t as *const i64 as usize;
    hex(&sha256(format!("{t}-{p}-{addr}").as_bytes()))
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// 从 `Cookie` 头里取出我们的那个。
pub fn session_from_cookie(header: Option<&str>) -> String {
    let Some(h) = header else {
        return String::new();
    };
    for part in h.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&format!("{COOKIE_NAME}=")) {
            return rest.trim().to_string();
        }
    }
    String::new()
}

/// 构造 Set-Cookie。`HttpOnly` + `SameSite=Strict` + `Path=/`。
///
/// 不加 `Secure`：这个服务按设计只在内网跑 HTTP，加了 `Secure` 浏览器就不会发这个 cookie，
/// 面板直接不能登录。等哪天套了 TLS 再加。
pub fn cookie_header(sid: &str) -> String {
    format!(
        "{COOKIE_NAME}={sid}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        SESSION_TTL_MS / 1000
    )
}

pub fn clear_cookie_header() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 未配置登录时任何人都通过() {
        let a = Auth::new("", "");
        assert!(!a.login_required());
        assert!(a.check("随便", "随便"));
    }

    #[test]
    fn 配置后错误口令被拒() {
        let a = Auth::new("admin", "s3cret-pass");
        assert!(a.login_required());
        assert!(a.check("admin", "s3cret-pass"));
        assert!(!a.check("admin", "wrong"));
        assert!(!a.check("root", "s3cret-pass"));
        assert!(!a.check("", ""));
    }

    #[test]
    fn 会话签发与校验() {
        let dir = tempfile::tempdir().unwrap();
        let a = Auth::with_db(
            "admin",
            "p",
            Some(Db::open(dir.path().join("t.db")).unwrap()),
        );
        let sid = a.issue("admin");
        assert_eq!(a.verify(&sid), Some("admin".to_string()));
        assert_eq!(a.verify("不存在的id"), None);
        assert_eq!(a.verify(""), None);
    }

    #[test]
    fn 登出后会话失效() {
        let a = Auth::new("admin", "p");
        let sid = a.issue("admin");
        a.logout(&sid);
        assert_eq!(a.verify(&sid), None);
    }

    #[test]
    fn 会话可数() {
        let dir = tempfile::tempdir().unwrap();
        let a = Auth::with_db(
            "admin",
            "p",
            Some(Db::open(dir.path().join("t.db")).unwrap()),
        );
        assert_eq!(a.active_sessions(), 0);
        a.issue("admin");
        a.issue("admin");
        assert_eq!(a.active_sessions(), 2);
    }

    #[test]
    fn 过期的会话失效() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        let a = Auth::with_db("admin", "p", Some(db.clone()));
        let sid = a.issue("admin");
        assert!(a.verify(&sid).is_some(), "刚建的会话应该有效");
        // 直接把过期时间拨到过去
        db.with_write(|tx| {
            tx.execute(
                "UPDATE panel_session SET expires_ms = ?1 WHERE sid = ?2",
                rusqlite::params![crate::db::now_ms() - 1000, sid],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(a.verify(&sid), None, "过期会话必须失效");
    }

    #[test]
    fn 会话重启后仍然有效() {
        // 这条锁的是本次改动：会话落盘，进程重启不掉线。
        // 早先存内存，重启后所有 cookie 都作废，用起来是「动不动就要重登」。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");

        let sid = {
            let db = Db::open(path.clone()).unwrap();
            let a = Auth::with_db("admin", "p", Some(db));
            a.issue("admin")
        }; // 这里 a 与它的 db 一起 drop，模拟进程退出

        // 「重启」：重新开库、重新构造 Auth
        let db2 = Db::open(path).unwrap();
        let a2 = Auth::with_db("admin", "p", Some(db2));
        assert_eq!(
            a2.verify(&sid).as_deref(),
            Some("admin"),
            "重启之后会话应该还在"
        );
    }

    #[test]
    fn 登出后会话立即失效() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        let a = Auth::with_db("admin", "p", Some(db));
        let sid = a.issue("admin");
        assert!(a.verify(&sid).is_some());
        a.logout(&sid);
        assert_eq!(a.verify(&sid), None, "登出后不该再认这个会话");
    }

    #[test]
    fn cookie_有效期是一年() {
        // 操作者要求「长期有效，不要 8 小时」。
        assert!(clear_cookie_header().contains("Max-Age=0"));
        assert!(cookie_header("x").contains("Max-Age=31536000"), "应为一年");
    }

    #[test]
    fn 从_cookie_头正确取值() {
        assert_eq!(session_from_cookie(Some("bw_panel=abc123")), "abc123");
        assert_eq!(
            session_from_cookie(Some("other=x; bw_panel=abc123; more=y")),
            "abc123"
        );
        assert_eq!(session_from_cookie(Some("nothing=here")), "");
        assert_eq!(session_from_cookie(None), "");
    }

    #[test]
    fn cookie_带_httponly_与_samesite() {
        let c = cookie_header("sid");
        assert!(c.contains("HttpOnly"), "页面脚本不该读得到会话");
        assert!(c.contains("SameSite=Strict"), "防 CSRF");
        assert!(c.contains("Path=/"));
        assert!(clear_cookie_header().contains("Max-Age=0"));
    }

    #[test]
    fn 每次签发都是不同的_id() {
        let a = Auth::new("admin", "p");
        let a1 = a.issue("admin");
        let a2 = a.issue("admin");
        assert_ne!(a1, a2, "会话 id 必须不可预测且互不相同");
        assert!(a1.len() >= 32, "id 应足够长，实得 {}", a1.len());
    }
}
