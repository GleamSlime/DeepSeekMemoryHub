//! SMTP：把快照压缩（可选加密）后发到指定邮箱。
//!
//! # 用途
//!
//! 本机磁盘坏了、或者误删了数据目录，快照也跟着没了 —— 那快照就白做了。
//! 发到邮箱等于**异地多一份**，且是最省事的那种（不用自建对象存储）。
//!
//! # 安全边界（必须写清楚）
//!
//! 快照里有**全部私聊内容与日记**。所以：
//! - 默认**加密发送**（`encrypt = true`），口令不写进配置文件之外的地方、
//!   不进日志、不回传给面板；
//! - 邮件正文只写「这是哪天、多大、怎么解开」，**不写任何记忆内容**；
//! - 发送失败只记日志与 `mail_log`，**不影响快照本身** ——
//!   邮件是备份的备份，不能因为它挂了把主流程拖下水。

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// SMTP 配置（面板可改）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailConfig {
    #[serde(default)]
    pub enable: bool,
    /// 服务器地址。按端口自动选加密方式：
    /// - 465 → 隐式 TLS
    /// - 其他（25/587/1025…）→ STARTTLS
    #[serde(default)]
    pub host: String,
    #[serde(default = "d_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// 发件人。多数服务要求与 `username` 相同，但别名场景下可以不同。
    #[serde(default)]
    pub from: String,
    /// 收件人，逗号分隔。
    #[serde(default)]
    pub to: String,

    /// 发信前是否加密快照包。
    #[serde(default = "t")]
    pub encrypt: bool,
    /// 加密口令。空且 `encrypt=true` 时**拒绝发送**（不能静默降级成明文）。
    #[serde(default)]
    pub passphrase: String,

    /// 超时（秒）。
    #[serde(default = "d_timeout")]
    pub timeout_seconds: u64,
    /// 每次快照都发。
    #[serde(default = "t")]
    pub on_snapshot: bool,
}

fn t() -> bool {
    true
}
fn d_port() -> u16 {
    465
}
fn d_timeout() -> u64 {
    30
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            enable: false,
            host: String::new(),
            port: d_port(),
            username: String::new(),
            password: String::new(),
            from: String::new(),
            to: String::new(),
            encrypt: true,
            passphrase: String::new(),
            timeout_seconds: d_timeout(),
            on_snapshot: true,
        }
    }
}

impl MailConfig {
    pub fn is_ready(&self) -> bool {
        self.enable
            && !self.host.trim().is_empty()
            && !self.username.trim().is_empty()
            && !self.password.trim().is_empty()
            && !self.to.trim().is_empty()
    }

    /// 面板视图：口令只给「设没设」。
    pub fn public_view(&self) -> serde_json::Value {
        serde_json::json!({
            "enable": self.enable,
            "host": self.host,
            "port": self.port,
            "username": self.username,
            "from": self.from,
            "to": self.to,
            "encrypt": self.encrypt,
            "on_snapshot": self.on_snapshot,
            "timeout_seconds": self.timeout_seconds,
            "password": { "secret": true, "set": !self.password.trim().is_empty() },
            "passphrase": { "secret": true, "set": !self.passphrase.trim().is_empty() },
            "ready": self.is_ready(),
        })
    }

    /// 发信前的完整校验。**任何一条不过就拒绝发**，不静默降级。
    pub fn validate_for_send(&self) -> Result<()> {
        if !self.enable {
            return Err(Error::BadRequest("SMTP 没启用".into()));
        }
        if self.host.trim().is_empty() {
            return Err(Error::BadRequest("SMTP 服务器地址没填".into()));
        }
        if self.username.trim().is_empty() {
            return Err(Error::BadRequest("SMTP 用户名没填".into()));
        }
        if self.password.is_empty() {
            return Err(Error::BadRequest("SMTP 密码没填".into()));
        }
        if self.recipients().is_empty() {
            return Err(Error::BadRequest("收件人没填".into()));
        }
        // 关键是这条：开了加密却没口令，不能悄悄发明文发出去
        if self.encrypt && self.passphrase.trim().is_empty() {
            return Err(Error::BadRequest(
                "开了「加密发送」但没设口令 —— 拒绝发送（不会静默降级成明文）".into(),
            ));
        }
        Ok(())
    }

    /// 解析收件人列表。
    pub fn recipients(&self) -> Vec<String> {
        self.to
            .split([',', ';', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.contains('@'))
            .map(str::to_string)
            .collect()
    }

    fn sender(&self) -> String {
        let f = self.from.trim();
        if f.is_empty() {
            self.username.trim().to_string()
        } else {
            f.to_string()
        }
    }
}

/// 发一封带附件的信。
///
/// 返回 `(message_id 或摘要, 耗时毫秒)`。
pub fn send(
    cfg: &MailConfig,
    subject: &str,
    body: &str,
    attachment: Option<(&str, Vec<u8>, &str)>,
) -> Result<i64> {
    cfg.validate_for_send()?;
    let t0 = std::time::Instant::now();

    // lettre 0.11 的签名（看源码确认过，别猜）：
    //   `Message::builder() -> MessageBuilder`
    //   `.from(Mailbox) -> MessageBuilder`（不包 Result）
    //   `.to(Mailbox) -> MessageBuilder`
    //   `.subject(impl Into<String>) -> MessageBuilder`
    //   `.body(String) -> Result<Message, _>`
    // 之前写错是因为把 `.to()` 当成返回 Result 了。
    let from_addr = cfg
        .sender()
        .parse()
        .map_err(|e| Error::BadRequest(format!("发件人地址不合法：{e}")))?;
    let mut builder = lettre::Message::builder().from(from_addr).subject(subject);
    for to in cfg.recipients() {
        let a = to
            .parse()
            .map_err(|e| Error::BadRequest(format!("收件人 {to} 不合法：{e}")))?;
        builder = builder.to(a);
    }

    let email: lettre::Message = match attachment {
        Some((name, bytes, mime)) => {
            let ct = lettre::message::header::ContentType::parse(mime).unwrap_or_else(|_| {
                lettre::message::header::ContentType::parse("application/octet-stream").unwrap()
            });
            let part = lettre::message::Attachment::new(name.to_string()).body(bytes, ct);
            let text = lettre::message::SinglePart::plain(body.to_string());
            let mp = lettre::message::MultiPart::mixed()
                .singlepart(text)
                .singlepart(part);
            // `multipart()` 自己就返回 `Result<Message>`，不用再 `.body()`
            builder
                .multipart(mp)
                .map_err(|e| Error::BadRequest(format!("邮件组装失败：{e}")))?
        }
        None => builder
            .body(body.to_string())
            .map_err(|e| Error::BadRequest(format!("邮件组装失败：{e}")))?,
    };

    // 端口 465 用隐式 TLS，其余用 STARTTLS。
    // 不提供「明文」选项：凭据和附件都不该裸奔。
    let creds = lettre::transport::smtp::authentication::Credentials::new(
        cfg.username.trim().to_string(),
        cfg.password.clone(),
    );
    let timeout = Duration::from_secs(cfg.timeout_seconds.clamp(5, 300));
    let mailer = if cfg.port == 465 {
        lettre::SmtpTransport::relay(cfg.host.trim())
            .map_err(|e| Error::Storage(format!("连不上 SMTP（TLS）：{e}")))?
            .port(cfg.port)
            .credentials(creds)
            .timeout(Some(timeout))
            .build()
    } else {
        lettre::SmtpTransport::starttls_relay(cfg.host.trim())
            .map_err(|e| Error::Storage(format!("连不上 SMTP（STARTTLS）：{e}")))?
            .port(cfg.port)
            .credentials(creds)
            .timeout(Some(timeout))
            .build()
    };

    use lettre::Transport;
    mailer.send(&email).map_err(|e| {
        // 把常见的 SMTP 码翻成人话：原始错误里全是英文协议文本，
        // 面板上看到「permanent error (535)」根本不知道该改哪儿。
        let raw = e.to_string();
        let hint = if raw.contains("(535)") || raw.contains("(534)") {
            " —— 账号或密码/授权码不对。QQ/163 这类邮箱要用「SMTP 授权码」，不是登录密码"
        } else if raw.contains("(550)") || raw.contains("(553)") {
            " —— 发件人地址被拒绝。多数服务要求发件人与登录用户名一致"
        } else if raw.contains("(530)") {
            " —— 服务器要求先加密再认证（这种情况不该出现，我们没有明文模式）"
        } else if raw.contains("Certificate") || raw.contains("certificate") {
            " —— 证书校验失败。自签证书/内网服务器默认会被拒绝（这是对的）"
        } else if raw.contains("timed out") || raw.contains("timeout") {
            " —— 连接超时。检查端口（465 隐式 TLS / 587 STARTTLS）与防火墙"
        } else {
            ""
        };
        Error::Storage(format!("发信失败：{raw}{hint}"))
    })?;
    Ok(t0.elapsed().as_millis() as i64)
}

/// 收件人打码：日志里不该出现完整邮箱。
pub fn mask_addr(s: &str) -> String {
    match s.split_once('@') {
        Some((a, d)) => {
            let keep: String = a.chars().take(2).collect();
            format!("{keep}***@{d}")
        }
        None => "***".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MailConfig {
        MailConfig {
            enable: true,
            host: "smtp.example.com".into(),
            port: 465,
            username: "a@example.com".into(),
            password: "pw".into(),
            to: "b@example.com".into(),
            encrypt: true,
            passphrase: "口令".into(),
            ..Default::default()
        }
    }

    #[test]
    fn 默认值不是零() {
        // 同 LlmConfig 的教训：derive(Default) 会绕过 serde 默认函数
        let c = MailConfig::default();
        assert_eq!(c.port, 465);
        assert_eq!(c.timeout_seconds, 30);
        assert!(c.encrypt, "默认必须加密");
        assert!(!c.enable, "默认不启用");
    }

    #[test]
    fn 开了加密却没口令要拒绝() {
        // 最关键的一条：不能静默降级成明文发出去
        let mut c = cfg();
        c.passphrase = String::new();
        let e = c.validate_for_send().unwrap_err().to_string();
        assert!(e.contains("拒绝发送"), "{e}");
    }

    #[test]
    fn 关掉加密就可以没口令() {
        let mut c = cfg();
        c.encrypt = false;
        c.passphrase = String::new();
        assert!(c.validate_for_send().is_ok());
    }

    #[test]
    fn 缺必填项要逐条报() {
        for (c, want) in [
            (
                {
                    let mut c = cfg();
                    c.host = String::new();
                    c
                },
                "地址",
            ),
            (
                {
                    let mut c = cfg();
                    c.username = String::new();
                    c
                },
                "用户名",
            ),
            (
                {
                    let mut c = cfg();
                    c.password = String::new();
                    c
                },
                "密码",
            ),
            (
                {
                    let mut c = cfg();
                    c.to = String::new();
                    c
                },
                "收件人",
            ),
        ] {
            let e = c.validate_for_send().unwrap_err().to_string();
            assert!(e.contains(want), "要报「{want}」，实际：{e}");
        }
    }

    #[test]
    fn 没启用直接拒() {
        let mut c = cfg();
        c.enable = false;
        assert!(c.validate_for_send().is_err());
        assert!(!c.is_ready());
    }

    #[test]
    fn 收件人解析() {
        let mut c = cfg();
        c.to = " a@x.com, b@y.com ; 坏地址  c@z.com".into();
        assert_eq!(c.recipients(), vec!["a@x.com", "b@y.com", "c@z.com"]);
    }

    #[test]
    fn 面板不泄露密码() {
        let v = cfg().public_view().to_string();
        assert!(!v.contains("\"pw\""), "密码不能出现在面板数据：{v}");
        assert!(!v.contains("口令"), "加密口令也不能：{v}");
        assert!(v.contains("\"set\":true"));
    }

    #[test]
    fn 邮箱打码() {
        assert_eq!(mask_addr("abcdef@qq.com"), "ab***@qq.com");
        assert_eq!(mask_addr("坏输入"), "***");
    }

    #[test]
    fn 发件人留空就用用户名() {
        let mut c = cfg();
        c.from = String::new();
        assert_eq!(c.sender(), "a@example.com");
        c.from = "别名@example.com".into();
        assert_eq!(c.sender(), "别名@example.com");
    }
}
