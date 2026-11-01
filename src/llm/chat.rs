//! OpenAI 兼容的 `/chat/completions` 客户端。
//!
//! 刻意做得**很薄**：只要 JSON 进、文本出，不做 function calling、
//! 不做流式、不重试花活。生成日记/手帐是**一次性**任务，薄客户端更好排错。

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// LLM 配置。**全部可在面板里改**（参考常见 agent 工具的配置项）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// 形如 `https://api.deepseek.com/v1`（**不含** `/chat/completions`）。
    ///
    /// 为什么要用户填完整 base：各家路径不一样（有的 `/v1`、有的 `/api/v1`、
    /// 局域网网关可能连前缀都没有），猜不对就一直是 404。
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub model: String,
    /// 单次请求超时（秒）。默认 90 —— 生成一篇日记比聊天慢得多。
    #[serde(default = "d_timeout")]
    pub timeout_seconds: u64,
    /// 输出上限。日记 800 字大约 1200 token，留点余量。
    #[serde(default = "d_max_tokens")]
    pub max_tokens: u32,
    /// 采样温度。生成类任务用 0.8：太低会千篇一律（用户原话「过于机械、可预测」）。
    #[serde(default = "d_temp")]
    pub temperature: f64,
    /// 失败重试次数（指数退避）。默认 2。
    #[serde(default = "d_retries")]
    pub retries: u32,
}

/// ⚠️ **必须手写**，不能 `#[derive(Default)]`：
/// derive 会给所有字段塞零值，把 `#[serde(default = "d_timeout")]` 那些函数
/// 完全绕过去 —— 结果是「没配过 LLM 时 max_tokens=0、temperature=0」，
/// 面板上一片零，真跑起来还会因为 max_tokens=0 直接被网关拒。
impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            timeout_seconds: d_timeout(),
            max_tokens: d_max_tokens(),
            temperature: d_temp(),
            retries: d_retries(),
        }
    }
}

fn d_timeout() -> u64 {
    90
}
fn d_max_tokens() -> u32 {
    1200
}
fn d_temp() -> f64 {
    0.8
}
fn d_retries() -> u32 {
    2
}

impl LlmConfig {
    /// 配好了吗？没配就**不要**去请求（避免每次 tick 都撞 401）。
    pub fn is_ready(&self) -> bool {
        !self.base_url.trim().is_empty()
            && !self.model.trim().is_empty()
            && !self.api_key.trim().is_empty()
    }

    /// 面板要显示的东西：key 只给「设没设」，绝不给值。
    pub fn public_view(&self) -> serde_json::Value {
        serde_json::json!({
            "base_url": self.base_url,
            "model": self.model,
            "timeout_seconds": self.timeout_seconds,
            "max_tokens": self.max_tokens,
            "temperature": self.temperature,
            "retries": self.retries,
            "api_key": { "secret": true, "set": !self.api_key.trim().is_empty() },
            "ready": self.is_ready(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(s: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: s.into(),
        }
    }
    pub fn user(s: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: s.into(),
        }
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f64,
    max_tokens: u32,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: Msg,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Msg {
    #[serde(default)]
    content: String,
}

#[derive(Deserialize, Default)]
struct Usage {
    #[serde(default)]
    prompt_tokens: i64,
    #[serde(default)]
    completion_tokens: i64,
}

/// 一次调用的结果。
#[derive(Debug, Clone)]
pub struct ChatOutcome {
    pub text: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    /// 被模型自己截断了（`finish_reason == "length"`）。
    ///
    /// 单独标出来：日记写到一半被截断，读起来是「突然没了」，
    /// 而 `Ok` 的返回会让人以为是完整的一篇。
    pub truncated: bool,
}

/// 客户端。每次调用现建，不持有连接池 —— 生成任务频率很低（十几分钟一次）。
pub struct ChatClient {
    cfg: LlmConfig,
}

impl ChatClient {
    pub fn new(cfg: LlmConfig) -> Self {
        Self { cfg }
    }

    /// 拼出完整的 chat/completions 地址。
    ///
    /// 容忍用户把 `/chat/completions` 也贴进来（很常见），
    /// 但**不猜** `/v1`：有的网关没有它，加错了反而 404。
    fn endpoint(&self) -> String {
        let base = self.cfg.base_url.trim().trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }

    /// 发一次请求，拿文本回来。
    pub async fn chat(&self, messages: &[ChatMessage]) -> Result<ChatOutcome> {
        if !self.cfg.is_ready() {
            return Err(Error::BadRequest(
                "LLM 还没配置（需要 base_url / api_key / model），在面板「模型」页填".into(),
            ));
        }
        let url = self.endpoint();
        let body = ChatRequest {
            model: self.cfg.model.trim(),
            messages,
            temperature: self.cfg.temperature,
            max_tokens: self.cfg.max_tokens,
        };

        let mut last_err: Option<Error> = None;
        let attempts = self.cfg.retries.min(5) + 1;
        for attempt in 0..attempts {
            match self.try_once(&url, &body).await {
                Ok(o) => return Ok(o),
                Err(e) => {
                    // 4xx（除 429）重试没意义：key 错、模型名错、参数错都不会自己好。
                    let retriable = e.retryable();
                    if !retriable || attempt + 1 >= attempts {
                        return Err(e);
                    }
                    // 指数退避：1s, 2s, 4s…（上限 8s）
                    let wait = Duration::from_secs((1u64 << attempt).min(8));
                    tracing::warn!(
                        "LLM 调用失败（第 {} 次），{:?} 后重试：{e}",
                        attempt + 1,
                        wait
                    );
                    tokio::time::sleep(wait).await;
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| Error::Storage("LLM 调用失败（未知原因）".into())))
    }

    async fn try_once(&self, url: &str, body: &ChatRequest<'_>) -> Result<ChatOutcome> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.cfg.timeout_seconds.max(5)))
            .build()
            .map_err(|e| Error::Storage(format!("HTTP 客户端构造失败：{e}")))?;

        let resp = client
            .post(url)
            .bearer_auth(self.cfg.api_key.trim())
            .json(body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    Error::Storage(format!("LLM 超时（{}s）", self.cfg.timeout_seconds))
                } else {
                    Error::Storage(format!("LLM 请求失败：{e}"))
                }
            })?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Storage(format!("读 LLM 响应失败：{e}")))?;

        if !status.is_success() {
            // 200 字以内：够看清原因，又不会把整页 HTML 灌进日志
            let brief: String = text.chars().take(200).collect();
            return Err(match status.as_u16() {
                401 | 403 => {
                    Error::BadRequest(format!("LLM 鉴权失败（{}）：{brief}", status.as_u16()))
                }
                404 => Error::BadRequest(format!(
                    "LLM 地址不对（404）：{url} —— base_url 一般填到 /v1 为止"
                )),
                429 => Error::Storage(format!("LLM 限流（429）：{brief}")),
                _ => Error::Storage(format!("LLM 返回 {}：{brief}", status.as_u16())),
            });
        }

        let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
            Error::Storage(format!(
                "LLM 响应不是预期的 OpenAI 格式（{e}）：{}",
                text.chars().take(200).collect::<String>()
            ))
        })?;

        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| Error::Storage("LLM 返回里没有 choices".into()))?;

        let truncated = choice.finish_reason.as_deref() == Some("length");
        if truncated {
            tracing::warn!("LLM 输出被 max_tokens 截断（当前 {}）", self.cfg.max_tokens);
        }

        let usage = parsed.usage.unwrap_or_default();
        Ok(ChatOutcome {
            text: choice.message.content.trim().to_string(),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 地址拼接容忍用户贴全路径() {
        let mk = |b: &str| LlmConfig {
            base_url: b.into(),
            api_key: "k".into(),
            model: "m".into(),
            ..Default::default()
        };
        assert_eq!(
            ChatClient::new(mk("https://api.deepseek.com/v1")).endpoint(),
            "https://api.deepseek.com/v1/chat/completions"
        );
        // 末尾斜杠
        assert_eq!(
            ChatClient::new(mk("https://x/v1/")).endpoint(),
            "https://x/v1/chat/completions"
        );
        // 用户把整条也贴进来（很常见）
        assert_eq!(
            ChatClient::new(mk("https://x/v1/chat/completions")).endpoint(),
            "https://x/v1/chat/completions"
        );
        // **不猜 /v1**：没写 /v1 就不加
        assert_eq!(
            ChatClient::new(mk("http://192.168.1.5:8000")).endpoint(),
            "http://192.168.1.5:8000/chat/completions"
        );
    }

    #[test]
    fn 默认值不是零() {
        // derive(Default) 会绕过 serde 的 default 函数，全给零 —— 真踩过
        let c = LlmConfig::default();
        assert_eq!(c.timeout_seconds, 90);
        assert_eq!(c.max_tokens, 1200);
        assert!(c.temperature > 0.0, "temperature=0 会让输出千篇一律");
        assert_eq!(c.retries, 2);
        assert!(c.base_url.is_empty() && c.model.is_empty());
    }

    #[test]
    fn 没配好就不算_ready() {
        let mut c = LlmConfig::default();
        assert!(!c.is_ready());
        c.base_url = "https://x".into();
        assert!(!c.is_ready(), "缺 key 和 model");
        c.api_key = "k".into();
        c.model = "m".into();
        assert!(c.is_ready());
        // 只有空格不算
        c.api_key = "   ".into();
        assert!(!c.is_ready());
    }

    #[test]
    fn 面板视图不泄露_key() {
        let c = LlmConfig {
            base_url: "https://x".into(),
            api_key: "sk-SECRET".into(),
            model: "m".into(),
            ..Default::default()
        };
        let v = c.public_view().to_string();
        assert!(!v.contains("SECRET"), "key 不能出现在面板数据里：{v}");
        assert!(v.contains("\"set\":true"));
    }
}
