//! 蓝鲸记忆中枢 — BlueWhale Memory Hub
//!
//! 跨渠道 AI 记忆服务：把记忆从单个应用里抽出来，做成独立进程，
//! 让 MC 插件里的角色、QQ 群机器人、直播弹幕机器人共用同一份记忆。
//!
//! # 三条不可违背的铁律
//!
//! 1. **备份只能走 `VACUUM INTO`**。直接 `cp`/`rsync`/`tar` 抓 `.db` 会静默丢
//!    已提交但尚未 checkpoint 的 WAL 数据（实测丢过 5/408 条，零报错）。
//! 2. **不跨机器共享 `.db` 文件**。WAL 依赖共享内存，网络文件系统上会损坏。
//!    多机共享只能走本服务的 HTTP 接口。
//! 3. **时间轴以 `at_ms`（现实 UTC 毫秒）为唯一主轴**。游戏日 `g<N>` 只是
//!    MC 渠道的附加标签，不参与排序与窗口。

pub mod admin;
pub mod affect;
pub mod auth;
pub mod config;
pub mod context;
pub mod db;
pub mod error;
pub mod http;
pub mod importance;
pub mod kb;
pub mod legacy_api;
pub mod migrate;
pub mod model;
pub mod promise_detect;
pub mod query;
pub mod rank;
pub mod rollback;
pub mod snapshot;
pub mod webui;
pub mod weight;

pub use error::{Error, Result};

/// 服务版本，随 `/v1/health` 一起返回，用于区分「卡住」和「换了个新版本」。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 应用名（中文），出现在日志、健康检查与打包产物里。
pub const APP_NAME: &str = "蓝鲸记忆中枢";

/// 机器可读的应用标识，用于数据目录名、DMG 名、launchd label。
pub const APP_SLUG: &str = "bluewhale-memory";
