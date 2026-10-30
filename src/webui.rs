//! 内置网页面板。
//!
//! # 为什么要有它
//!
//! 只有 JSON 接口的话，「打开浏览器看看它现在是什么状态」这件事做不到——
//! 得会 curl、会拼 token、会读 JSON。而这类服务最常被问的问题恰恰是
//! 「现在有多少条记忆/快照是不是好的/上次回滚是什么时候」。
//!
//! # 安全
//!
//! 页面本身**不含任何记忆内容**，只有计数与状态。token 存在浏览器的
//! localStorage 里，由页面自己带 `X-API-Token` 去请求 JSON 接口。
//! 页面静态部分可以公开——它只是一段 HTML，没有凭据就没有数据。

/// 管理后台主页面。单文件、无 CDN 依赖（内网可能没有外网）。
pub const INDEX_HTML: &str = include_str!("webui.html");

/// 登录页。纯表单提交，不依赖 JavaScript——
/// 一个需要 JS 才能渲染的登录框，在最需要它的时候恰好用不了。
pub const LOGIN_HTML: &str = include_str!("webui_login.html");
