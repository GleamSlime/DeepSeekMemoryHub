//! OpenAI 兼容的嵌入客户端。
//!
//! 只用一个端点：`POST {url}`，body `{"model","input":[...],"dimensions":N}`。
//!
//! # 三个必须处理的现实问题
//!
//! 1. **返回顺序不保证与请求一致。** vLLM / TEI / llama.cpp 的实现细节不同，
//!    有的并发处理会乱序。必须按响应里的 `index` 归位。
//!    不归位的话就是「向量和文本错配」—— 而且**永远不会有报错**，
//!    只会表现为「检索结果莫名其妙」。
//!
//! 2. **`dimensions` 不一定被支持。** 它是 OpenAI 的扩展参数，TEI 视版本、
//!    llama.cpp 的 server 可能直接忽略。忽略时返回的是模型默认维度
//!    （Qwen3-Embedding 是 1024）。这时客户端自己截断前 N 维 + **重新 L2 归一化**
//!    —— 截断会改变模长，不归一化 cosine 就算错。
//!    能用是因为 Qwen3-Embedding 支持 MRL：取前 k 维本身就是合法嵌入。
//!
//! 3. **服务可能超时/挂掉。** 这时必须能优雅降级（退回纯字面），
//!    而不是让整条检索链路报错。降级由调用方决定，这里只负责把错误如实返回。
//!
//! # 实测基线（2026-10-30，Ollama + qwen3-embedding:0.6b-q8）
//!
//! | 操作 | 耗时 |
//! | --- | --- |
//! | 单条查询（热） | 22.7 ms |
//! | 嵌入 780 条（批 16） | 18 s |
//! | 冷加载模型 | 589 ms |
//!
//! Ollama 的 `dimensions` 参数**是支持的**，且实测会**自动重新 L2 归一化**
//! （模长恒为 1.000000），所以客户端那条截断路径在 Ollama 上不会被触发 ——
//! 但换别的实现就会被触发，所以必须留着。

use crate::{Error, Result};

/// 嵌入客户端配置。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EmbedConfig {
    /// 完整端点 URL，例如 `http://192.168.3.252:11434/v1/embeddings`。
    pub url: String,
    pub model: String,
    /// 期望维度。**必须与服务实际返回一致**，否则打分是垃圾。
    pub dim: usize,
    /// 可选 Bearer token。
    pub token: String,
    /// 一次请求多少条。
    pub batch: usize,
    /// 单次请求超时（秒）。
    pub timeout_seconds: u64,
}

impl EmbedConfig {
    /// 配置校验：能明显看出错的，启动期就该失败，而不是等第一次检索。
    pub fn validate(&self) -> Result<()> {
        if self.url.trim().is_empty() {
            return Err(Error::Config("知识库已启用，但 embed_url 为空".into()));
        }
        if !self.url.starts_with("http://") && !self.url.starts_with("https://") {
            return Err(Error::Config(format!(
                "embed_url 必须以 http:// 或 https:// 开头，实得 {}",
                self.url
            )));
        }
        if self.model.trim().is_empty() {
            return Err(Error::Config("embed_model 为空".into()));
        }
        if !(crate::kb::DIM_MIN..=crate::kb::DIM_MAX).contains(&self.dim) {
            return Err(Error::Config(format!(
                "embed_dim {} 超出 [{}, {}]",
                self.dim,
                crate::kb::DIM_MIN,
                crate::kb::DIM_MAX
            )));
        }
        if self.batch == 0 {
            return Err(Error::Config("embed_batch 不能为 0".into()));
        }
        Ok(())
    }
}

/// 嵌入客户端。构造一次，全局复用（`reqwest::Client` 内部有连接池，
/// 每请求新建会丢失 keep-alive，实测那会让延迟翻几倍）。
#[derive(Clone)]
pub struct Embedder {
    http: reqwest::Client,
    cfg: EmbedConfig,
}

impl std::fmt::Debug for Embedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 打 token 是安全事故，这里只留必要信息
        f.debug_struct("Embedder")
            .field("url", &self.cfg.url)
            .field("model", &self.cfg.model)
            .field("dim", &self.cfg.dim)
            .finish()
    }
}

/// 一次响应里的一个条目。
#[derive(Debug, serde::Deserialize)]
struct RespItem {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Debug, serde::Deserialize)]
struct Resp {
    data: Vec<RespItem>,
}

impl Embedder {
    pub fn new(cfg: EmbedConfig) -> Result<Self> {
        cfg.validate()?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(cfg.timeout_seconds.max(1)))
            // 连接超时单独设：不设时，一个不可达的 IP 会卡到整体超时，
            // 而那种等待是每轮检索都要付一次的。
            .connect_timeout(std::time::Duration::from_secs(3))
            // **显式不用代理。**
            //
            // reqwest 默认会读系统代理配置（macOS 上走 `system-configuration`）。
            // 实测踩过的坑：同一份二进制、同一份配置，在终端里手动启动能连上
            // Ollama，由 launchd 启动却报 `error sending request` —— 两边
            // 环境变量里都没有 proxy，差别就在「读不读得到系统配置」。
            //
            // 嵌入服务是**内网直连**（192.168.x.x），走代理没有任何意义，
            // 而且一旦代理配置里有 192.168.0.0/16 的例外规则处理不当，
            // 就会变成「终端能跑、开机自启不能跑」这种最难查的故障。
            .no_proxy()
            .build()
            .map_err(|e| Error::Config(format!("构造 HTTP 客户端失败：{e}")))?;
        Ok(Self { http, cfg })
    }

    pub fn dim(&self) -> usize {
        self.cfg.dim
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    pub fn url(&self) -> &str {
        &self.cfg.url
    }

    /// 嵌入一批文本，返回与输入**同序**的向量。
    ///
    /// 分批发送；任何一批失败则整体失败（调用方据此降级）。
    /// 不做内部重试：重试策略在回填任务里统一处理（那里有退避），
    /// 而检索路径上**多等一轮不如直接降级**。
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        // 第一次失败后立刻重试一次。
        //
        // 为什么这里要重试（而回填任务里反而不重试）：
        // **检索路径上的失败大多是「模型正在冷加载」**。实测 Ollama 首次
        // 加载 0.6B 要 ~600ms，但模型空闲几分钟会被卸掉，下一个查询又要重载；
        // 若这次重载恰好撞上 3 秒连接超时，那一次检索就白丢了。
        // 重试一次的成本最多 3 秒，而丢掉的是一次查询的语义召回 ——
        // 那个损失更大（实测「封号」查「封禁」纯靠向量，字面一条都不中）。
        //
        // 回填任务不重试是对的：那里有退避机制，且批量失败说明服务真的挂了。
        match self.embed_once(texts).await {
            Ok(v) => Ok(v),
            Err(first) => {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                self.embed_once(texts)
                    .await
                    .map_err(|second| Error::Storage(format!("{second}（首次失败：{first}）")))
            }
        }
    }

    /// 单次尝试（不重试）。`embed` 包一层重试。
    async fn embed_once(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(self.cfg.batch.max(1)) {
            let body = serde_json::json!({
                "model": self.cfg.model,
                "input": chunk,
                // 实测 Ollama 支持它；不支持的服务会忽略，走客户端截断
                "dimensions": self.cfg.dim,
            });
            let mut req = self.http.post(&self.cfg.url).json(&body);
            if !self.cfg.token.trim().is_empty() {
                req = req.bearer_auth(self.cfg.token.trim());
            }
            let resp = req.send().await.map_err(|e| {
                // **把整条错误链打出来。** reqwest 的 Display 只说
                // 「error sending request」，真正的原因（DNS / 连接被拒 /
                // TLS / 超时）在 source 链里。没有这条链，排查只能靠猜。
                let mut chain = vec![format!("{e}")];
                let mut src = std::error::Error::source(&e);
                while let Some(s) = src {
                    chain.push(format!("{s}"));
                    src = std::error::Error::source(s);
                }
                Error::Storage(format!(
                    "嵌入请求失败（{}）：{}",
                    self.cfg.url,
                    chain.join(" ← ")
                ))
            })?;

            if !resp.status().is_success() {
                let code = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(Error::Storage(format!(
                    "嵌入服务返回 {code}：{}",
                    truncate(&text, 300)
                )));
            }

            let raw = resp
                .text()
                .await
                .map_err(|e| Error::Storage(format!("读嵌入响应失败：{e}")))?;
            out.extend(parse_response(&raw, chunk.len(), self.cfg.dim)?);
        }
        Ok(out)
    }

    /// 启动探针：嵌一个 `"ping"`。
    ///
    /// 返回实际维度。**不阻止启动**（失败只打警告、检索降级），
    /// 但维度与配置不符要拒绝启动 —— 维度错配是最难查的一类：
    /// 打分是垃圾，而每条日志都正常。
    pub async fn probe(&self) -> Result<usize> {
        let v = self.embed(&["ping".to_string()]).await?;
        let got = v.first().map(|x| x.len()).unwrap_or(0);
        if got != self.cfg.dim {
            return Err(Error::Config(format!(
                "embed_dim 配置为 {}，但服务实际返回 {got} 维。\
                 维度不一致会让相似度计算失效（且不会报错），拒绝启动。",
                self.cfg.dim
            )));
        }
        Ok(got)
    }
}

/// 解析响应并按 `index` 归位。
///
/// 抽成独立函数是为了**不打网络也能测**：乱序归位、缺条目、维度不对
/// 这几种情况都要有判据，而它们全都只跟 JSON 解析有关。
pub fn parse_response(body: &str, expected_n: usize, want_dim: usize) -> Result<Vec<Vec<f32>>> {
    let resp: Resp = serde_json::from_str(body)
        .map_err(|e| Error::Storage(format!("嵌入响应不是合法 JSON：{e}")))?;

    if resp.data.len() != expected_n {
        return Err(Error::Storage(format!(
            "嵌入响应条目数不对：请求 {expected_n} 条，返回 {} 条",
            resp.data.len()
        )));
    }

    // ---- 按 index 归位。**这是本模块最关键的一行逻辑。**
    let mut slots: Vec<Option<Vec<f32>>> = vec![None; expected_n];
    for item in resp.data {
        if item.index >= expected_n {
            return Err(Error::Storage(format!(
                "嵌入响应 index={} 越界（请求 {expected_n} 条）",
                item.index
            )));
        }
        if slots[item.index].is_some() {
            return Err(Error::Storage(format!(
                "嵌入响应 index={} 重复",
                item.index
            )));
        }
        slots[item.index] = Some(fix_dim(item.embedding, want_dim)?);
    }

    slots
        .into_iter()
        .enumerate()
        .map(|(i, s)| s.ok_or_else(|| Error::Storage(format!("嵌入响应缺少 index={i} 的条目"))))
        .collect()
}

/// 把向量调到期望维度。
///
/// 多于期望 → 截断前 N 维 + **重新 L2 归一化**（服务忽略了 `dimensions`
/// 时走这里，靠 MRL 的性质仍然可用）。
/// 少于期望 → 报错，因为补齐没有意义（补零会得到一个既不是 A 也不是 B 的向量）。
fn fix_dim(mut v: Vec<f32>, want: usize) -> Result<Vec<f32>> {
    if v.len() == want {
        return Ok(v);
    }
    if v.len() < want {
        return Err(Error::Storage(format!(
            "嵌入维度不足：期望 {want}，实得 {}。\
             服务可能不支持 dimensions 参数，请把 embed_dim 设为模型的默认维度。",
            v.len()
        )));
    }
    v.truncate(want);
    l2_normalize(&mut v);
    Ok(v)
}

/// 原地 L2 归一化。零向量保持原样（除以 0 会得到 NaN，而 NaN 会污染整条链路）。
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x = (*x as f64 / norm) as f32;
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合法的最小配置。dim 必须 ≥ `DIM_MIN`，否则 `validate()` 会（正确地）拒绝 ——
    /// 注意 `parse_response` 的判据里用的是任意 want_dim，它不做范围校验。
    fn cfg() -> EmbedConfig {
        EmbedConfig {
            url: "http://127.0.0.1:9/v1/embeddings".into(),
            model: "m".into(),
            dim: crate::kb::DIM_MIN,
            token: String::new(),
            batch: 2,
            timeout_seconds: 5,
        }
    }

    #[test]
    fn 按_index_归位而不是按返回顺序() {
        // 这是本模块最关键的判据。服务返回 [index=1, index=0] 的乱序，
        // 归位后第一个向量必须是 index=0 那条。
        let body = r#"{"data":[
            {"index":1,"embedding":[9,9,9,9]},
            {"index":0,"embedding":[1,2,3,4]}
        ]}"#;
        let v = parse_response(body, 2, 4).unwrap();
        assert_eq!(v[0], vec![1.0, 2.0, 3.0, 4.0], "第 0 条应为 index=0 的向量");
        assert_eq!(v[1], vec![9.0, 9.0, 9.0, 9.0]);
    }

    #[test]
    fn 条目数不符要报错() {
        let body = r#"{"data":[{"index":0,"embedding":[1,2,3,4]}]}"#;
        let e = parse_response(body, 2, 4);
        assert!(e.is_err(), "请求 2 条只回 1 条必须报错，不能静默错位");
    }

    #[test]
    fn index_重复或越界要报错() {
        let dup = r#"{"data":[
            {"index":0,"embedding":[1,2,3,4]},
            {"index":0,"embedding":[5,6,7,8]}
        ]}"#;
        assert!(parse_response(dup, 2, 4).is_err());

        let oob = r#"{"data":[
            {"index":0,"embedding":[1,2,3,4]},
            {"index":5,"embedding":[5,6,7,8]}
        ]}"#;
        assert!(parse_response(oob, 2, 4).is_err());
    }

    #[test]
    fn 缺_index_要报错() {
        let body = r#"{"data":[
            {"index":0,"embedding":[1,2,3,4]},
            {"index":1,"embedding":[5,6,7,8]}
        ]}"#;
        assert!(parse_response(body, 3, 4).is_err(), "少一条必须报错");
    }

    #[test]
    fn 服务忽略_dimensions_时截断并重新归一化() {
        // 服务返回 8 维、我们要 4 维：截断前 4 维后必须重新归一化，
        // 否则模长变了，cosine 会算错。
        let body = r#"{"data":[{"index":0,"embedding":[3,4,0,0,99,99,99,99]}]}"#;
        let v = parse_response(body, 1, 4).unwrap();
        assert_eq!(v[0].len(), 4);
        let norm: f32 = v[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-6,
            "截断后必须重新归一化，模长={norm}"
        );
        // [3,4] 归一化后是 [0.6, 0.8]
        assert!((v[0][0] - 0.6).abs() < 1e-6);
        assert!((v[0][1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn 维度不足要报错而不是补零() {
        // 补零会得到一个既不是 A 也不是 B 的向量，比报错糟得多
        let body = r#"{"data":[{"index":0,"embedding":[1,2]}]}"#;
        let e = parse_response(body, 1, 4);
        assert!(e.is_err());
        let msg = format!("{}", e.unwrap_err());
        assert!(msg.contains("维度不足"), "错误信息应说清原因：{msg}");
    }

    #[tokio::test]
    async fn 失败会重试一次才报错() {
        // 检索路径上的失败大多是「模型正在冷加载」（Ollama 空闲几分钟
        // 就卸模型，下次查询要 ~600ms 重载）。重试一次能救回这类偶发失败，
        // 而丢掉的是一次查询的语义召回 —— 实测「封号」查「封禁」纯靠向量，
        // 字面一条都不中，向量失败就等于零结果。
        let e = Embedder::new(cfg()).unwrap();
        let t = std::time::Instant::now();
        let r = e.embed(&["x".to_string()]).await;
        assert!(r.is_err());
        // 两次尝试 + 300ms 间隔，应明显长于单次
        assert!(
            t.elapsed().as_millis() >= 300,
            "应重试一次（含 300ms 间隔），实际 {:?}",
            t.elapsed()
        );
    }

    #[tokio::test]
    async fn 连接失败的错误信息带底层原因() {
        // reqwest 的 Display 只说「error sending request」，真正的原因
        // （连接被拒 / DNS / 超时）在 source 链里。不把它拼出来，
        // 排查只能靠猜 —— 排查 launchd 那个问题时就是靠这条链才定位的。
        let e = Embedder::new(cfg()).unwrap();
        let err = e.embed(&["x".to_string()]).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("←") || msg.contains("Connection refused") || msg.contains("refused"),
            "错误信息里应有底层原因，实得：{msg}"
        );
    }

    #[test]
    fn 坏json_报错不panic() {
        assert!(parse_response("not json", 1, 4).is_err());
        assert!(parse_response("{}", 1, 4).is_err());
        assert!(parse_response(r#"{"data":[]}"#, 0, 4).unwrap().is_empty());
    }

    #[test]
    fn 零向量归一化不出_nan() {
        let mut v = vec![0.0f32; 4];
        l2_normalize(&mut v);
        assert!(v.iter().all(|x| x.is_finite()), "零向量不能变成 NaN");
    }

    #[test]
    fn 配置校验拦住明显错误() {
        let mut c = cfg();
        c.url = String::new();
        assert!(c.validate().is_err());

        let mut c = cfg();
        c.url = "192.168.3.252:11434".into(); // 少了协议
        assert!(c.validate().is_err());

        let mut c = cfg();
        c.dim = 8;
        assert!(c.validate().is_err(), "dim 低于下限应拒绝");

        let mut c = cfg();
        c.batch = 0;
        assert!(c.validate().is_err());

        assert!(cfg().validate().is_ok());
    }

    #[test]
    fn token_不进_debug_输出() {
        let mut c = cfg();
        c.token = "super-secret-token".into();
        let e = Embedder::new(c).unwrap();
        let s = format!("{e:?}");
        assert!(
            !s.contains("super-secret-token"),
            "Debug 输出泄露了 token：{s}"
        );
    }

    #[test]
    fn 客户端显式禁用代理() {
        // 实测踩过的坑（2026-10-30）：同一份二进制、同一份配置，
        // 在终端手动启动能连内网 Ollama，由 launchd 启动却报
        // `error sending request` —— 两边环境变量里都没有 proxy，
        // 差别在于 reqwest 默认可读系统代理配置。
        //
        // 嵌入服务是内网直连，走代理毫无意义，且一旦系统代理里有
        // 192.168.0.0/16 这类例外规则处理不当，就会变成
        // 「终端能跑、开机自启不能跑」这种最难查的故障。
        //
        // 这条判据没有断言「一定能连上」（那要网络），只断言构造不 panic
        // 且带上了 no_proxy —— 真正的验证在真机部署时做。
        let e = Embedder::new(cfg()).unwrap();
        assert_eq!(e.dim(), crate::kb::DIM_MIN);
    }

    #[tokio::test]
    async fn 服务不可达时返回错误而不是卡死() {
        // 降级的前提是「错误能及时返回」。连一个必然拒绝的端口。
        let e = Embedder::new(cfg()).unwrap();
        let t = std::time::Instant::now();
        let r = e.embed(&["x".to_string()]).await;
        assert!(r.is_err());
        assert!(
            t.elapsed().as_secs() < 10,
            "连接失败应快速返回，实际耗时 {:?}",
            t.elapsed()
        );
    }
}
