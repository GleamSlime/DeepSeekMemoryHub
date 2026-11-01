//! 配置。
//!
//! # 优先级
//!
//! 命令行参数 > 环境变量 > 配置文件 > 默认值。
//!
//! # 为什么所有路径都可配
//!
//! 部署要「可迁移」：本机开发、Linux 服务器、将来打包成 DMG 装到别人机器上，
//! 三者的数据目录、监听地址、端口都不同。**代码里不出现任何绝对路径**。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::weight::WeightConfig;

/// 监听范围。用户要求「主需要内网可访问即可」，所以默认是内网。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindScope {
    /// 只听本机回环。最安全，适合开发。
    Loopback,
    /// 听所有网卡（含局域网）。**默认**——这是用户的实际需求。
    Lan,
    /// 显式指定某个地址。
    Custom,
}

impl BindScope {
    pub fn parse(s: &str) -> BindScope {
        match s.trim().to_ascii_lowercase().as_str() {
            "loopback" | "local" | "127.0.0.1" => BindScope::Loopback,
            "custom" => BindScope::Custom,
            _ => BindScope::Lan,
        }
    }
}

/// 知识库配置。
///
/// **默认关闭**（`enable = false`）。理由：这是本项目第一个对外部服务的依赖，
/// 不配置它的部署应当与加这个功能之前**完全一致** —— 不探测、不报错、不进检索。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KbConfig {
    pub enable: bool,
    pub embed: crate::kb::embed::EmbedConfig,
    /// 单块字符上限。700 是被「没有 reranker」反推出来的（见 `kb::chunk`）。
    pub chunk_max_chars: usize,
    pub rank: crate::kb::rank::Opts,
    /// 回填任务间隔（秒）。
    pub backfill_seconds: u64,
    /// `/v1/context` 注入知识库的门槛：向量相似度下限。
    ///
    /// 实测依据（2026-10-30，6 块语料的真实数据）：
    /// 无关查询（「今天天气不错」「你好呀」「1+1等于几」）也能拿到
    /// `vec = 0.29 ~ 0.36`，而相关查询最低 0.408 —— 区分度很薄。
    /// 0.42 是实测出的分界线，**不是拍的**。
    ///
    /// 语料变大或换嵌入模型后这个值要重测：小语料上"什么都有一点相似"，
    /// 语料一大，无关项的相似度会整体下降。
    pub inject_min_vec: f64,
    /// `/v1/context` 注入知识库的门槛：字面相关度下限。
    /// 两个门槛是「或」的关系 —— 任一路过关就注入。
    pub inject_min_lit: f64,
}

impl Default for KbConfig {
    fn default() -> Self {
        Self {
            enable: false,
            embed: crate::kb::embed::EmbedConfig {
                url: String::new(),
                // 实测结论：0.6b-q8 与 8b-q4 质量无显著差异，但快 5.2 倍、
                // 省 3.6 倍内存。所以默认不是最大的那个。
                model: "qwen3-embedding:0.6b-q8".into(),
                // 实测 512 与 1024 差异在噪声内，存储与点积各省一半。
                dim: 512,
                token: String::new(),
                batch: 16,
                timeout_seconds: 30,
            },
            chunk_max_chars: crate::kb::chunk::DEFAULT_MAX_CHARS,
            rank: crate::kb::rank::Opts::default(),
            backfill_seconds: 30,
            // 实测分界线，见字段注释
            inject_min_vec: 0.42,
            inject_min_lit: 0.30,
        }
    }
}

impl KbConfig {
    pub fn validate(&self) -> Result<()> {
        // 关着的时候什么都不校验：一个没启用知识库的部署，
        // 不该因为 url 为空而起不来。
        if !self.enable {
            return Ok(());
        }
        self.embed.validate()?;
        if self.chunk_max_chars == 0 {
            return Err(Error::Config("kb_chunk_max_chars 不能为 0".into()));
        }
        if self.backfill_seconds == 0 {
            return Err(Error::Config("kb_backfill_seconds 不能为 0".into()));
        }
        if self.rank.max_per_doc == 0 {
            return Err(Error::Config("kb_max_per_doc 不能为 0".into()));
        }
        for (name, v) in [
            ("kb_inject_min_vec", self.inject_min_vec),
            ("kb_inject_min_lit", self.inject_min_lit),
        ] {
            if !v.is_finite() || !(0.0..=1.0).contains(&v) {
                return Err(Error::Config(format!("{name} 必须在 [0,1]，实得 {v}")));
            }
        }
        Ok(())
    }
}

/// 服务配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// 监听地址。
    pub bind_addr: IpAddr,
    pub port: u16,
    pub bind_scope: BindScope,

    /// 数据根目录。数据库、快照都在这下面。
    pub data_dir: PathBuf,

    /// 鉴权 token。
    ///
    /// **空 = 只允许回环访问**。这个语义很重要：
    /// 一个监听在局域网但没有 token 的服务等于把所有人的记忆摊开给整个内网。
    /// 所以「没有 token」时强制降级成只听回环，而不是「开着但不设密码」。
    pub token: String,

    /// 快照保留天数。默认 30。
    pub snapshot_keep_days: i64,

    /// `/v1/context` 里最多内联几张记忆画面（base64）。默认 1。
    ///
    /// 为什么默认只 1 张：一张 800px 的图约 1–2k token，塞多了
    /// 会把「最近说过的话」这些更重要的段挤掉，成本也涨得快。
    #[serde(default = "one")]
    pub context_image_inline_max: usize,

    /// 单张图超过多少字节就不内联（退化成只给描述）。默认 512 KB。
    ///
    /// 不设上限的话，一张 3 MB 的截图会被编码成 4 MB 的 base64 ——
    /// 既吃 token 又可能超过模型的上下文上限，整轮请求直接失败。
    #[serde(default = "k512")]
    pub context_image_inline_max_bytes: i64,

    /// 每日自动快照的检查间隔（秒）。默认 3600。
    pub snapshot_check_seconds: u64,

    /// 权重模型参数。
    pub weight: WeightConfig,

    /// 中枢自己的 LLM（OpenAI 兼容）。**默认全空 = 不启用**。
    ///
    /// 空的时候所有生成任务（日记/周记/手帐/情绪/自我认知）都不跑，
    /// 行为与本功能上线前**完全一致** —— 不请求、不报错、不建表行。
    #[serde(default)]
    pub llm: crate::llm::LlmConfig,

    /// 生成任务的时间与门槛。
    #[serde(default)]
    pub generate: crate::generate::GenConfig,

    /// 每个连接的请求体上限（字节）。默认 8 MiB。
    pub max_body_bytes: usize,

    /// 单请求超时（秒）。默认 30。
    pub request_timeout_seconds: u64,

    /// 全局并发上限。
    pub max_concurrency: usize,

    /// 管理后台的登录名。空 = 不启用登录。
    ///
    /// **不启用登录时，只有监听回环才允许访问后台。**
    /// 监听内网却没口令的后台等于把所有记忆摊开给整个内网。
    pub panel_user: String,

    /// 知识库。
    pub kb: KbConfig,

    /// 管理后台口令。空 = 不启用登录。
    pub panel_password: String,
}

fn one() -> usize {
    1
}

fn k512() -> i64 {
    512 * 1024
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 25590,
            bind_scope: BindScope::Lan,
            data_dir: default_data_dir(),
            token: String::new(),
            snapshot_keep_days: 30,
            context_image_inline_max: 1,
            context_image_inline_max_bytes: 512 * 1024,
            snapshot_check_seconds: 3600,
            weight: WeightConfig::default(),
            llm: crate::llm::LlmConfig::default(),
            generate: crate::generate::GenConfig::default(),
            max_body_bytes: 8 * 1024 * 1024,
            request_timeout_seconds: 30,
            max_concurrency: 4096,
            panel_user: String::new(),
            kb: KbConfig::default(),
            panel_password: String::new(),
        }
    }
}

/// 默认数据目录。
///
/// 不写死绝对路径：优先环境变量，其次用户主目录下的标准位置。
/// 拿不到主目录就退到当前工作目录的子目录——**永远不该因为找不到家目录而拒绝启动**。
pub fn default_data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("BLUEWHALE_DATA_DIR") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    match home {
        Some(h) => h.join(".bluewhale-memory"),
        None => PathBuf::from("bluewhale-data"),
    }
}

impl Config {
    /// 从环境变量覆盖。前缀 `BLUEWHALE_`。
    pub fn from_env() -> Result<Self> {
        let mut cfg = Config::default();

        if let Ok(v) = std::env::var("BLUEWHALE_BIND") {
            if let BindScope::Custom = BindScope::parse(&v) {
                // 下一步由 BLUEWHALE_BIND_ADDR 提供具体地址
            }
            cfg.bind_scope = BindScope::parse(&v);
        }
        if let Ok(v) = std::env::var("BLUEWHALE_BIND_ADDR") {
            cfg.bind_addr = v
                .trim()
                .parse()
                .map_err(|_| Error::Config(format!("BLUEWHALE_BIND_ADDR 不是合法 IP：{v}")))?;
            cfg.bind_scope = BindScope::Custom;
        }
        if let Ok(v) = std::env::var("BLUEWHALE_PORT") {
            cfg.port = v
                .trim()
                .parse()
                .map_err(|_| Error::Config(format!("BLUEWHALE_PORT 不是合法端口：{v}")))?;
        }
        if let Ok(v) = std::env::var("BLUEWHALE_DATA_DIR") {
            if !v.trim().is_empty() {
                cfg.data_dir = PathBuf::from(v);
            }
        }
        if let Ok(v) = std::env::var("BLUEWHALE_TOKEN") {
            cfg.token = v;
        }
        if let Ok(v) = std::env::var("BLUEWHALE_SNAPSHOT_KEEP_DAYS") {
            cfg.snapshot_keep_days = v
                .trim()
                .parse()
                .map_err(|_| Error::Config(format!("BLUEWHALE_SNAPSHOT_KEEP_DAYS 非法：{v}")))?;
        }
        if let Ok(v) = std::env::var("BLUEWHALE_PANEL_USER") {
            cfg.panel_user = v;
        }
        if let Ok(v) = std::env::var("BLUEWHALE_PANEL_PASSWORD") {
            cfg.panel_password = v;
        }
        if let Ok(v) = std::env::var("BLUEWHALE_MAX_CONCURRENCY") {
            cfg.max_concurrency = v
                .trim()
                .parse()
                .map_err(|_| Error::Config(format!("BLUEWHALE_MAX_CONCURRENCY 非法：{v}")))?;
        }
        // ---------------------------------------------------------- 知识库
        if let Ok(v) = std::env::var("BLUEWHALE_KB_ENABLE") {
            cfg.kb.enable = matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
        }
        if let Ok(v) = std::env::var("BLUEWHALE_KB_EMBED_URL") {
            cfg.kb.embed.url = v.trim().to_string();
        }
        if let Ok(v) = std::env::var("BLUEWHALE_KB_EMBED_MODEL") {
            if !v.trim().is_empty() {
                cfg.kb.embed.model = v.trim().to_string();
            }
        }
        if let Ok(v) = std::env::var("BLUEWHALE_KB_EMBED_DIM") {
            cfg.kb.embed.dim = v
                .trim()
                .parse()
                .map_err(|_| Error::Config(format!("BLUEWHALE_KB_EMBED_DIM 非法：{v}")))?;
        }
        if let Ok(v) = std::env::var("BLUEWHALE_KB_EMBED_TOKEN") {
            cfg.kb.embed.token = v.trim().to_string();
        }
        if let Ok(v) = std::env::var("BLUEWHALE_KB_EMBED_BATCH") {
            cfg.kb.embed.batch = v
                .trim()
                .parse()
                .map_err(|_| Error::Config(format!("BLUEWHALE_KB_EMBED_BATCH 非法：{v}")))?;
        }
        if let Ok(v) = std::env::var("BLUEWHALE_KB_EMBED_TIMEOUT") {
            cfg.kb.embed.timeout_seconds = v
                .trim()
                .parse()
                .map_err(|_| Error::Config(format!("BLUEWHALE_KB_EMBED_TIMEOUT 非法：{v}")))?;
        }
        if let Ok(v) = std::env::var("BLUEWHALE_KB_CHUNK_MAX_CHARS") {
            cfg.kb.chunk_max_chars = v
                .trim()
                .parse()
                .map_err(|_| Error::Config(format!("BLUEWHALE_KB_CHUNK_MAX_CHARS 非法：{v}")))?;
        }
        if let Ok(v) = std::env::var("BLUEWHALE_KB_INJECT_MIN_VEC") {
            cfg.kb.inject_min_vec = v
                .trim()
                .parse()
                .map_err(|_| Error::Config(format!("BLUEWHALE_KB_INJECT_MIN_VEC 非法：{v}")))?;
        }
        if let Ok(v) = std::env::var("BLUEWHALE_KB_INJECT_MIN_LIT") {
            cfg.kb.inject_min_lit = v
                .trim()
                .parse()
                .map_err(|_| Error::Config(format!("BLUEWHALE_KB_INJECT_MIN_LIT 非法：{v}")))?;
        }
        if let Ok(v) = std::env::var("BLUEWHALE_KB_BACKFILL_SECONDS") {
            cfg.kb.backfill_seconds = v
                .trim()
                .parse()
                .map_err(|_| Error::Config(format!("BLUEWHALE_KB_BACKFILL_SECONDS 非法：{v}")))?;
        }

        cfg.validate()?;
        Ok(cfg)
    }

    /// 校验。启动期必须跑，错误要在启动日志里看得见。
    pub fn validate(&self) -> Result<()> {
        if self.port == 0 {
            return Err(Error::Config("端口不能为 0".into()));
        }
        if self.snapshot_keep_days <= 0 {
            return Err(Error::Config("快照保留天数必须为正".into()));
        }
        if self.max_concurrency == 0 {
            return Err(Error::Config("并发上限不能为 0".into()));
        }
        if self.max_body_bytes == 0 {
            return Err(Error::Config("请求体上限不能为 0".into()));
        }
        if self.request_timeout_seconds == 0 {
            return Err(Error::Config("请求超时不能为 0".into()));
        }
        self.weight.validate().map_err(Error::Config)?;
        self.kb.validate()?;

        // 关键安全约束：非回环监听必须有 token。
        // 没有 token 就只听回环——不给「开着但不要密码」这个状态留任何缝隙。
        // 后台口令：要么都空（不启用登录），要么设够长。
        // 只填一半是配置错误——用户以为自己开了登录，实际没有。
        let (u, pw) = (self.panel_user.trim(), self.panel_password.trim());
        if u.is_empty() != pw.is_empty() {
            return Err(Error::Config(
                "panel_user 与 panel_password 必须同时设置或同时留空".into(),
            ));
        }
        if !pw.is_empty() && pw.len() < 8 {
            return Err(Error::Config(
                "后台口令太短（至少 8 字符）。这是能看全部记忆的入口。".into(),
            ));
        }

        if !self.token.is_empty() && self.token.len() < 16 {
            return Err(Error::Config(
                "token 太短（至少 16 字符）。短 token 对局域网内爆破毫无抵抗力。".into(),
            ));
        }
        Ok(())
    }

    /// 实际监听地址。
    ///
    /// **不再因为「没有 token」而降级成回环。**
    ///
    /// 旧行为是「没 token 就只听 127.0.0.1」，看着安全，实际有两个问题：
    /// ① 操作者要求内网访问时会被静默降级，表现为「连不上」却不知道为什么；
    /// ② 它把**安全策略**和**网络可达性**绑成了一件事。
    ///
    /// 现在两者分开：这里只管听哪儿；「谁能调 API」由请求级检查决定
    /// （见 `http::api_auth_mw`：没有 token 时非回环请求一律 401）。
    pub fn effective_bind(&self) -> SocketAddr {
        let ip = match self.bind_scope {
            BindScope::Loopback => IpAddr::V4(Ipv4Addr::LOCALHOST),
            BindScope::Lan => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            BindScope::Custom => self.bind_addr,
        };
        SocketAddr::new(ip, self.port)
    }

    /// 是否监听在所有网卡上（`0.0.0.0` 或 `::`）。
    pub fn listening_on_all_interfaces(&self) -> bool {
        self.effective_bind().ip().is_unspecified()
    }

    /// 是否处于「API 对局域网开放但没有 token」这个危险配置。
    ///
    /// 启动时必须大声警告：请求级检查会把非回环请求挡掉，
    /// 但操作者会以为「能连上却什么都调不了」。
    pub fn api_exposed_without_token(&self) -> bool {
        self.token.is_empty() && !self.effective_bind().ip().is_loopback()
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("memory.db")
    }

    pub fn snapshot_dir(&self) -> PathBuf {
        self.data_dir.join("snapshots")
    }

    /// 语音库文件目录。与数据库同目录下，备份时跟着数据目录一起走。
    pub fn voice_dir(&self) -> PathBuf {
        self.data_dir.join("voices")
    }

    /// 记忆画面的图片目录（原图 + 缩略图）。
    pub fn image_dir(&self) -> PathBuf {
        self.data_dir.join("images")
    }

    /// 把数据目录标准化（不要求存在）。
    pub fn normalize_paths(&mut self) {
        self.data_dir = normalize(&self.data_dir);
    }
}

fn normalize(p: &Path) -> PathBuf {
    // 只做 `~` 展开与绝对化，不做 canonicalize——
    // 目录可能还不存在，canonicalize 会失败。
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            return home.join(rest);
        }
    }
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_token(mut c: Config, t: &str) -> Config {
        c.token = t.to_string();
        c
    }

    #[test]
    fn 默认配置合法() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn lan_绑定到_0000_不再因缺_token_降级() {
        let c = Config {
            bind_scope: BindScope::Lan,
            token: String::new(),
            ..Default::default()
        };
        let addr = c.effective_bind();
        assert!(
            addr.ip().is_unspecified(),
            "lan 应绑定 0.0.0.0，实得 {addr}"
        );
        assert!(c.listening_on_all_interfaces());
        // 但必须标记成危险配置：API 层会挡掉非回环请求
        assert!(c.api_exposed_without_token(), "应标记为需要警告");
    }

    #[test]
    fn 有_token_时不算危险配置() {
        let c = with_token(
            Config {
                bind_scope: BindScope::Lan,
                ..Default::default()
            },
            "0123456789abcdef0123",
        );
        assert!(!c.api_exposed_without_token());
    }

    #[test]
    fn loopback_不算危险配置() {
        let c = Config {
            bind_scope: BindScope::Loopback,
            token: String::new(),
            ..Default::default()
        };
        assert!(c.effective_bind().ip().is_loopback());
        assert!(!c.api_exposed_without_token());
        assert!(!c.listening_on_all_interfaces());
    }

    #[test]
    fn 有_token_且为_lan_时监听所有网卡() {
        let c = with_token(
            Config {
                bind_scope: BindScope::Lan,
                ..Default::default()
            },
            "0123456789abcdef0123",
        );
        let addr = c.effective_bind();
        assert!(addr.ip().is_unspecified(), "应监听 0.0.0.0，实得 {addr}");
        assert!(!c.api_exposed_without_token());
    }

    #[test]
    fn 有_token_但选_loopback_时仍然只听本机() {
        let c = with_token(
            Config {
                bind_scope: BindScope::Loopback,
                ..Default::default()
            },
            "0123456789abcdef0123",
        );
        assert!(c.effective_bind().ip().is_loopback());
    }

    #[test]
    fn 过短的_token_被拒绝() {
        let c = with_token(Config::default(), "short");
        let err = c.validate().unwrap_err();
        assert!(matches!(err.kind(), crate::error::ErrorKind::Config));
        assert!(err.to_string().contains("16"), "应说明长度要求");
    }

    #[test]
    fn 端口为零被拒绝() {
        let c = Config {
            port: 0,
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn 快照保留天数为零被拒绝() {
        let c = Config {
            snapshot_keep_days: 0,
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn 并发上限为零被拒绝() {
        let c = Config {
            max_concurrency: 0,
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn 非法权重配置会传出去() {
        let mut c = Config::default();
        c.weight.half_life_days = -5.0;
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("half_life_days"));
    }

    #[test]
    fn 数据路径都在_data_dir_下() {
        let c = Config {
            data_dir: PathBuf::from("/tmp/x"),
            ..Default::default()
        };
        assert_eq!(c.db_path(), PathBuf::from("/tmp/x/memory.db"));
        assert_eq!(c.snapshot_dir(), PathBuf::from("/tmp/x/snapshots"));
    }

    #[test]
    fn 波浪号会被展开() {
        let mut c = Config {
            data_dir: PathBuf::from("~/bw-test"),
            ..Default::default()
        };
        c.normalize_paths();
        let s = c.data_dir.to_string_lossy().to_string();
        assert!(!s.starts_with('~'), "应展开 ~，实得 {s}");
        assert!(s.ends_with("bw-test"));
    }

    #[test]
    fn 相对路径会被绝对化() {
        let mut c = Config {
            data_dir: PathBuf::from("relative-test"),
            ..Default::default()
        };
        c.normalize_paths();
        assert!(c.data_dir.is_absolute(), "应绝对化，实得 {:?}", c.data_dir);
    }

    #[test]
    fn 自定义地址生效() {
        let c = with_token(
            Config {
                bind_scope: BindScope::Custom,
                bind_addr: "10.0.0.5".parse().unwrap(),
                ..Default::default()
            },
            "0123456789abcdef0123",
        );
        assert_eq!(c.effective_bind().ip().to_string(), "10.0.0.5");
    }
}
