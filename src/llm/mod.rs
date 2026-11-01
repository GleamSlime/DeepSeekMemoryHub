//! 记忆中枢自己的 LLM 客户端。
//!
//! # 为什么中枢要有 LLM
//!
//! 2026-11-01 之前，日记 / 周记 / 手帐 / 情绪 / 自我认知**全部由 MC 插件生成**，
//! 中枢只负责存。后果：MC 不开机，这些功能就全停 —— 实测 `journal` 停在
//! 10-30（两天前），而系统里其他部分还在跑。
//!
//! 所以生成逻辑要搬到中枢：**它才是那个 7×24 一直在的服务**。
//!
//! # 协议
//!
//! 只用 **OpenAI 兼容**的 `/chat/completions`（最常见、各家都支持），
//! 不做多厂商适配 —— 用户要的是「自定义 URL + key + 模型 id」，
//! 那正好就是 OpenAI 格式的形状。任何兼容网关（DeepSeek / vLLM / Ollama /
//! one-api…）都能直接填进来。
//!
//! # 省请求
//!
//! 中枢自己调模型，最容易干的事就是「每条事件都问一次」。
//! 这里有两条硬约束（见 `crate::tick`）：
//! 1. **一次请求算多件事**：情绪 + 手帐 + 自我认知**一次**给出，
//!    而不是三个请求各问一遍；
//! 2. **攒批 + 节流**：不是每条事件都触发。

pub mod chat;

pub use chat::{ChatClient, ChatMessage, LlmConfig};

/// 一次生成的用途，用于记账与限流。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// 被动触发的「感知 tick」：情绪 + 手帐 + 自我认知 一次算完。
    Tick,
    /// 每日日记。
    Diary,
    /// 每周周记。
    Weekly,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Tick => "tick",
            Purpose::Diary => "diary",
            Purpose::Weekly => "weekly",
        }
    }
}
