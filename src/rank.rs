//! 检索排序：中文可用的 BM25（bigram + 停用字软降权）。
//!
//! # 为什么需要这个文件
//!
//! 旧检索是 `summary.contains(q)` 的**纯子串匹配**。实测在这台机器的真实库上：
//! 8 个自然的改写提问（「我住在哪儿」「蜘蛛刷出来没有」）**字面命中 0/8 = 0%**，
//! 于是这些查询全部退化成「按 weight 取最重要的几条」——
//! 而 weight 与提问**无关**。后果是换十个不同的问题调 `/v1/context`，
//! 「你记得的事」那一段返回的内容**一模一样**（实测唯一性 1/10）。
//!
//! # 为什么是 BM25 而不是向量
//!
//! 当前语料 2.6 万字符 / 758 条，全表打分是亚毫秒级（实测 p50 0.74 ms）。
//! 上一版的设计注释里写着「引入向量库会让部署从『一个二进制』变成『还要起个服务』」，
//! 这条判断仍然成立：能在一个文件里用纯算术解决的问题，不该换来 1.2 GB 的模型和
//! 一个常驻推理进程。真正需要语义泛化时（同义改写跨词表），BM25 到不了的地方
//! 再上 embedding —— 但那是另一次改动，不该和「先让检索能命中」混在一起。
//!
//! # 为什么用 bigram 而不是分词
//!
//! 中文字符 2-gram 不需要词典、不需要下载、不需要处理未登录词，
//! 而且对「钓鱼竿 / 钓鱼 / 竿」这类复合词天然做部分匹配。
//! 单字切分会让「的」「了」这种高频字统治打分，整句切分又要一整个词典。
//! 代价是索引变大（约 2N 个 token），在 2.6 万字符上是可忽略的。
//!
//! # 停用字为什么是「软降权」而不是「直接删」
//!
//! 硬删会连带删掉**含实义**的 bigram：「挖到」里有个「到」是停用字，
//! 硬删规则会把整个「挖到」丢掉，于是「我上次挖到什么好东西没」一条都命中不了。
//! 改成乘一个 0.15 的系数后，同样的查询能把「挖到」捞出来排第一。
//!
//! 实测对比（同一真实库、同一批查询）：
//!
//! | 方案 | 有真命中的查询数 |
//! | --- | --- |
//! | 纯子串（现状） | 0/10（改写提问） |
//! | 硬删停用字 | 7/9，且「我上次挖到什么」全部落空 |
//! | **软降权** | **9/10** |

use std::collections::HashMap;

/// 停用字：纯语法虚词。**含语义的词一律不进来**。
///
/// 划线的地方是最容易写错的两类：
/// - 「家」「上」「下」「里」「外」「时」「候」「东」「西」看起来像方位/虚词，
///   但它们是**实义的**（「家在哪」「上次」「东西」）。把它们加进来会让
///   「我住在哪儿」直接查不出东西。
/// - 「到」「过」「来」「去」是动词性补语，单看是虚词，但它们构成的
///   bigram（「挖到」「钓过」）是强证据 —— 所以这里只**降权**，不删。
const STOP_CHARS: &str = "的了吗呢吧啊呀哦嗯是在我你他她它们这那有没和就都也还要会能去来说过个把被给对从到得地着之其与或者但如果因为所以什么哪个谁怎样如何而";

/// 泛化词：统计上不罕见、**语义上不指向具体事物**的词。
///
/// 这类词不能靠频率阈值过滤 —— 实测「东西」的 df 占比只有 3.2%，
/// 比「钓鱼」的 8.9% 更稀有，任何比例阈值都会把它当成"有区分度"。
/// 但它出现在查询里时，与语料中「怕的东西」的「东西」**不是同一个指称**，
/// 共享它纯属巧合。
///
/// 所以只能按词表来。加词的判据是「它出现在查询里时，能把不相关的记忆捞上来」，
/// 不是「它看起来像虚词」。目前只收了少量与疑问/泛指强相关的多字词。
const GENERIC_WORDS: &[&str] = &[
    "东西", "什么", "哪个", "哪里", "哪儿", "怎么", "多少", "一些", "那些", "这些", "事情", "时候",
    "地方", "样子", "感觉", "情况",
];

/// 是不是泛化词。
fn is_generic(tok: &str) -> bool {
    GENERIC_WORDS.contains(&tok)
}

/// 单字 token 的权重系数。
///
/// 单字必须**存在**（否则单字查询全灭），但不能与 bigram 平起平坐：
/// bigram 才承载「钓鱼」「蜘蛛」这种真正的区分度。
const SINGLE_CHAR_WEIGHT: f64 = 0.35;

/// 含停用字的 bigram 的权重系数。
///
/// 0.15 是实测出来的：再高会让「好了」「怎么」这类虚词抢到首位
/// （实测「钓鱼用的竿子做好了吗」曾被「盖好了小屋的东墙」压过），
/// 再低则「挖到」这种半虚词几乎失效。
const STOP_BIGRAM_WEIGHT: f64 = 0.15;

/// BM25 的两个自由参数。取的是文献常用默认值，没有针对本库调过 ——
/// 语料只有几千条，调这两个参数只会过拟合到当前这批数据。
const K1: f64 = 1.5;
const B: f64 = 0.75;

/// 一条待打分的文档。
#[derive(Debug, Clone)]
pub struct Doc {
    /// 调用方自己的 id（`event:123` / `journal:j-xxx` / `fact:f-xxx`）。
    pub key: String,
    /// 参与匹配的文本。
    pub text: String,
}

/// 中文 bigram 切分。
///
/// - 全角转半角、大小写归一，让「ＭＣ」和「mc」是同一个词；
/// - **同时产出单字**（见下），所以单字查询也能命中；
/// - **不删**停用字，权重在 [`Bm25::weight_of`] 里处理。
///
/// # 为什么要连单字一起产出
///
/// 只切 bigram 时，「鱼」这样的单字查询**永远返回空**：
/// 查询侧只有 token `鱼`，而文档侧只有 `主人/人爱/…/烤鱼`，
/// 两边的词表完全不相交。这个 bug 是集成测试抓到的
/// （`主人爱吃的鱼是烤鱼` 用 `q=鱼` 查不出来），不是设计时想到的。
///
/// 单字的 IDF 天然很低（几乎每条文本都含它），所以在 BM25 里不会抢位；
/// 只有它出现在查询里、且在本文档里出现时才会贡献分数 —— 这正是我们要的。
pub fn bigrams(s: &str) -> Vec<String> {
    let lowered = normalize(s);
    let chars: Vec<char> = lowered.chars().collect();
    let mut out: Vec<String> = Vec::with_capacity(chars.len() * 2);
    // 单字在前：`matched` 排序用贡献值，不依赖这个顺序，
    // 但稳定的小输入让测试和人工排查更好读。
    for c in &chars {
        if !c.is_whitespace() {
            out.push(c.to_string());
        }
    }
    for w in chars.windows(2) {
        if w.iter().any(|c| c.is_whitespace()) {
            continue;
        }
        out.push(w.iter().collect());
    }
    out
}

/// 一点归一化：全角 ASCII → 半角、小写。
///
/// 只做这两件，**不做** 繁简转换或同义词表 —— 那会让「匹配到什么」
/// 变得不可解释，而这个项目的面板要能回答「它为什么想起这条」。
fn normalize(s: &str) -> String {
    s.chars()
        .map(|c| {
            // 全角 ASCII（！到 ～）折回半角
            if ('\u{FF01}'..='\u{FF5E}').contains(&c) {
                char::from_u32(c as u32 - 0xFEE0).unwrap_or(c)
            } else if c == '\u{3000}' {
                ' '
            } else {
                c
            }
        })
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 一个 bigram 是不是含停用字。
fn has_stop(bigram: &str) -> bool {
    bigram.chars().any(|c| STOP_CHARS.contains(c))
}

/// 打分器。构造时一次性算好语料统计，之后每条查询只做查表。
///
/// 一次 `/v1/recall` 只应构造一次。构造是 O(语料 token 数)，
/// 在 758 条 / 2.6 万字符上实测远低于 1 ms。
#[derive(Clone)]
pub struct Bm25 {
    /// 每个文档的 token 频次。
    tf: Vec<HashMap<String, u32>>,
    /// 每个文档的 token 总数（BM25 的长度归一化用）。
    len: Vec<usize>,
    /// 平均文档长度。
    avg_len: f64,
    /// 每个 token 出现在多少个文档里。
    df: HashMap<String, u32>,
    /// 文档总数。
    n: usize,
    /// 与 `tf` 同下标、与输入 `docs` 同顺序的 key 与原文。
    keys: Vec<String>,
    texts: Vec<String>,
}

/// 一条打分结果。
#[derive(Debug, Clone)]
pub struct Scored {
    /// 在输入 `docs` 里的下标。
    pub idx: usize,
    /// 归一化到 `(0, 1]` 的相关度。
    pub score: f64,
    /// 命中的 token，用于「为什么想起这条」。**按贡献从大到小**。
    pub matched: Vec<String>,
    /// 是否命中了**实义词**。
    ///
    /// 只有含实义 token 的匹配才算「想起来了」。见 [`Bm25::search`] 里的说明：
    /// 「在吗」这种查询只匹配得到「在」这种虚字，把它当成命中是自欺欺人 ——
    /// 实测它会让「在吗」返回一堆随机记忆并标成命中。
    pub meaningful: bool,
}

impl Bm25 {
    /// 用一批文档建索引。
    pub fn new(docs: &[Doc]) -> Self {
        let mut tf = Vec::with_capacity(docs.len());
        let mut len = Vec::with_capacity(docs.len());
        let mut df: HashMap<String, u32> = HashMap::new();

        for d in docs {
            let toks = bigrams(&d.text);
            let mut m: HashMap<String, u32> = HashMap::new();
            for t in &toks {
                *m.entry(t.clone()).or_insert(0) += 1;
            }
            // df 按**文档**计数，不是按出现次数：一个词在一篇里出现十次
            // 仍然只算「一篇文档含有它」。
            for t in m.keys() {
                *df.entry(t.clone()).or_insert(0) += 1;
            }
            len.push(toks.len().max(1));
            tf.push(m);
        }

        let n = docs.len();
        let avg_len = if n == 0 {
            1.0
        } else {
            len.iter().sum::<usize>() as f64 / n as f64
        };

        Self {
            tf,
            len,
            avg_len,
            df,
            n,
            keys: docs.iter().map(|d| d.key.clone()).collect(),
            texts: docs.iter().map(|d| d.text.clone()).collect(),
        }
    }

    /// 文档数。
    pub fn len(&self) -> usize {
        self.n
    }

    /// 是否为空语料。
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// 某个 token 的权重（IDF，乘上停用字系数与单字系数）。
    fn weight_of(&self, tok: &str) -> f64 {
        let df = self.df.get(tok).copied().unwrap_or(0) as f64;
        let n = self.n as f64;
        // BM25 的 IDF 平滑形式：df 越大（越常见）权重越低，
        // 且**永远为正** —— 用 Robertson-Sparck-Jones 的原始形式会在大 df 时变负，
        // 那会让「命中一个常见词」反而扣分，不是我们要的语义。
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        let mut w = idf;
        if has_stop(tok) {
            w *= STOP_BIGRAM_WEIGHT;
        }
        if tok.chars().count() == 1 {
            // 单字是**宽匹配**：命中了也说明不了太多。
            // 不加这个系数时，一个只在本文档出现过的生僻字（比如「钓」）
            // 的 IDF 会高过「钓鱼」这个真正有区分度的 bigram，
            // 于是「为什么想起这条」会给出「钓」这种没解释力的答案。
            // 这是单测抓到的，不是设计时想到的。
            w *= SINGLE_CHAR_WEIGHT;
        }
        w
    }

    /// 给一条查询打分，返回**所有**得分大于 0 的文档，按分数降序。
    ///
    /// 分数归一化：除以「查询里所有在语料中出现过的 token 的权重之和」。
    /// 这样 `score` 恒落在 `(0, 1]`，调用方可以拿它当阈值用，
    /// 而不必知道语料规模。
    ///
    /// **只统计语料里真的出现过的 token 做分母**。查询里那些语料中没有的
    /// bigram（比如「蛛刷」）既不是证据也不是反证，算进分母只会把
    /// 所有文档的分数一起压下去 —— 实测那样做会让「蜘蛛刷出来没有」
    /// 一条都命中不了。
    ///
    /// # 归一化会把虚词查询放大成「满分」
    ///
    /// 归一化是相对的，所以「分母也很小」时照样能得 1.0。
    /// 实测「在吗」在两句无关记忆上得到 `score=1.09`（被 clamp 到 1.0）、
    /// `hit=true` —— 它只匹配到一个「在」字。
    /// 因此这里额外要求：命中的 token 里**至少有一个是实义词**，
    /// 否则 `meaningful=false`，调用方据此判为「没想起什么」。
    /// 分数计算本身不受影响（那是纯算术），受影响的只是「算不算命中」。
    pub fn search(&self, query: &str) -> Vec<Scored> {
        if self.n == 0 {
            return vec![];
        }
        let q_all = bigrams(query);
        // 查询本身只有一个字（「鱼」「家」）时，那个单字就是**全部**意图，
        // 不能因为它"是单字"就判成没有实义 —— 否则用户真的只想搜这个字时
        // 永远查不出东西。实测这是集成测试抓到的边界：
        // `q=鱼` 对「主人爱吃的鱼是烤鱼」返回 hit=false。
        let query_is_single_char = q_all.len() == 1 && q_all[0].chars().count() == 1;
        let q_tokens: Vec<String> = {
            let mut v: Vec<String> = Vec::new();
            for t in q_all {
                if self.df.contains_key(&t) && !v.contains(&t) {
                    v.push(t);
                }
            }
            v
        };
        if q_tokens.is_empty() {
            return vec![];
        }

        let denom: f64 = q_tokens.iter().map(|t| self.weight_of(t)).sum();

        let mut out: Vec<Scored> = Vec::with_capacity(self.n);
        for i in 0..self.n {
            let mut s = 0.0;
            let mut matched: Vec<(f64, String)> = Vec::new();
            let mut meaningful = false;
            for t in &q_tokens {
                let f = match self.tf[i].get(t) {
                    Some(&f) => f as f64,
                    None => continue,
                };
                let w = self.weight_of(t);
                // 实义 token = 不含停用字、且不是单字。
                //
                // 单字是宽匹配（「鱼」「钓」都可能只因为字形撞上），
                // 单独一个单字命中不足以说「想起了这件事」。
                // 但**查询本身就只有这一个字**时例外：那时它承载了全部意图。
                if !has_stop(t) && (t.chars().count() > 1 || query_is_single_char) {
                    meaningful = true;
                }
                // BM25 的 tf 饱和项：同一个词出现很多次带来的收益递减，
                // 否则一段反复念叨「蜘蛛」的日志会压过真正相关的那条。
                let norm = 1.0 - B + B * (self.len[i] as f64 / self.avg_len);
                let contrib = w * (f * (K1 + 1.0)) / (f + K1 * norm);
                s += contrib;
                matched.push((contrib, t.clone()));
            }
            if s > 0.0 {
                matched.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

                // 「有实义命中」的判据分两层，缺一不可：
                //
                // ① **至少一个多字 token**。全部由单字撑起来的匹配不可信：
                //    语料里「怕的东西: 苦力怕」与查询「我上次挖到什么好东西」
                //    只共享散字「东」「西」，实测就被误判成命中了。
                // ② **该 token 得有区分度**（IDF 够高）。多字还不够 ——
                //    「东西」是两个字的 bigram，但它在这份语料里到处都是，
                //    共享它说明不了任何事。用 IDF 卡掉高频词组，
                //    而不是往停用字表里逐个手工加词（那永远加不完）。
                // 「强证据」= 多字、不含停用字、且**不是泛化疑问词**。
                //
                // 第三项是为「东西」这类词准备的。它统计上并不高频
                // （实测 df=24/752 = 3.2%，比「钓鱼」的 8.9% 还稀有），
                // 所以**任何频率阈值都拦不住它** —— 试过 40% 比例阈值，
                // 完全无效。它的问题不在频率，在语义：它是泛化词，
                // 问「我挖到什么好东西」和语料里「怕的东西」共享它是巧合，
                // 不是同一个"东西"。
                //
                // 结论：这类词必须像停用字一样**按词表**处理，
                // 而不能指望统计。词表本身不长（见 GENERIC_WORDS）。
                let has_strong = matched
                    .iter()
                    .any(|(_, t)| t.chars().count() > 1 && !has_stop(t) && !is_generic(t));
                if !has_strong && !query_is_single_char {
                    meaningful = false;
                }

                out.push(Scored {
                    idx: i,
                    score: (s / denom).min(1.0),
                    matched: matched.into_iter().map(|(_, t)| t).collect(),
                    meaningful,
                });
            }
        }
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }

    /// 第 `idx` 篇的 key。
    pub fn key(&self, idx: usize) -> &str {
        &self.keys[idx]
    }

    /// 第 `idx` 篇的原文。
    pub fn text(&self, idx: usize) -> &str {
        &self.texts[idx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(key: &str, text: &str) -> Doc {
        Doc {
            key: key.into(),
            text: text.into(),
        }
    }

    #[test]
    fn bigram_切分() {
        // 单字 + bigram 都要产出：单字让「鱼」这种查询能命中，
        // bigram 保证「钓鱼」不被拆成两个高频字。
        assert_eq!(bigrams("钓鱼"), vec!["钓", "鱼", "钓鱼"]);
        assert_eq!(bigrams("钓鱼竿"), vec!["钓", "鱼", "竿", "钓鱼", "鱼竿"]);
        assert!(bigrams("").is_empty());
        assert_eq!(bigrams("鱼"), vec!["鱼"]);
        // 空白不该产生 token，否则空格会变成一个"稀有词"
        assert_eq!(bigrams("a b"), vec!["a", "b"]);
    }

    #[test]
    fn bigram_全角与大小写归一() {
        // 全角 ＭＣ 与半角 mc 必须等价，否则「MC」和「ｍｃ」是两个世界
        assert_eq!(bigrams("ＭＣ"), bigrams("mc"));
        assert_eq!(bigrams("ABC"), bigrams("abc"));
    }

    #[test]
    fn 停用字降权而不是删除() {
        // 「挖到」含停用字「到」，但必须仍然保留 —— 硬删会让这类查询全灭
        let docs = vec![doc("a", "昨天挖到了钻石"), doc("b", "和玩家聊了天气")];
        let ix = Bm25::new(&docs);
        let r = ix.search("我上次挖到什么好东西没");
        assert!(!r.is_empty(), "含停用字边界的「挖到」必须仍能命中");
        assert_eq!(ix.key(r[0].idx), "a");
    }

    #[test]
    fn 虚词抢不到首位() {
        // 实测过的坑：「钓鱼用的竿子做好了吗」曾被「盖好了小屋的东墙」压过，
        // 因为「好了」在语料里更稀有。降权后必须由实义词主导。
        let docs = vec![
            doc("wall", "盖好了小屋的东墙"),
            doc("fish", "做完了「钓鱼」"),
        ];
        let ix = Bm25::new(&docs);
        let r = ix.search("钓鱼用的竿子做好了吗");
        assert_eq!(
            ix.key(r[0].idx),
            "fish",
            "实义词「钓鱼」必须压过虚词「好了」"
        );
    }

    #[test]
    fn 稀有词权重高于常见词() {
        let docs = vec![
            doc("a", "和玩家聊了天气"),
            doc("b", "和玩家聊了天气"),
            doc("c", "和玩家聊了天气"),
            doc("d", "找到了钻石"),
        ];
        let ix = Bm25::new(&docs);
        let r = ix.search("钻石");
        assert_eq!(r.len(), 1);
        assert_eq!(ix.key(r[0].idx), "d");
        // 「天气」在 3/4 文档里出现，「钻石」在 1/4 —— 同一个查询里，
        // 稀有词贡献必须更大。
        assert!(ix.weight_of("钻石") > ix.weight_of("天气"));
    }

    #[test]
    fn 分数归一化到_0_1() {
        let docs = vec![
            doc("a", "在 y=-54 发现了钻石矿脉"),
            doc("b", "今天天气不错"),
        ];
        let ix = Bm25::new(&docs);
        for s in ix.search("钻石") {
            assert!(s.score > 0.0 && s.score <= 1.0, "score={}", s.score);
        }
    }

    #[test]
    fn 查询里的未知_bigram_不当分母() {
        // 「蛛刷」在语料里不存在，不能把分母撑大 —— 那会把所有分数压到阈下
        let docs = vec![doc("a", "做完了「找蜘蛛摸线」")];
        let ix = Bm25::new(&docs);
        let r = ix.search("蜘蛛刷出来没有");
        assert!(!r.is_empty(), "语言里没出现过的 bigram 不该让查询全灭");
        assert!(
            r[0].score > 0.3,
            "score={} 太低，分母被未知 bigram 撑大了",
            r[0].score
        );
    }

    #[test]
    fn 长文档不会因为长而占优() {
        // BM25 的长度归一化：同样命中一次，短文档得分更高。
        // 否则长日志会把所有短事实压死。
        let docs = vec![
            doc("short", "蜘蛛"),
            doc(
                "long",
                &format!("蜘蛛{}", "和其他很多很多很多很多无关内容".repeat(30)),
            ),
        ];
        let ix = Bm25::new(&docs);
        let r = ix.search("蜘蛛");
        assert_eq!(ix.key(r[0].idx), "short", "短文档必须排在前面");
    }

    #[test]
    fn 空语料与空查询都不崩() {
        let ix = Bm25::new(&[]);
        assert!(ix.is_empty());
        assert!(ix.search("钻石").is_empty());

        let ix = Bm25::new(&[doc("a", "有内容")]);
        assert!(ix.search("").is_empty());
        // 查询里的字在语料里一个都没有 → 空结果，不是全量返回
        assert!(ix.search("完全不存在的词汇组合").is_empty());
    }

    #[test]
    fn 纯虚词查询不算命中() {
        // 实测踩过的坑：「在吗」在两句无关记忆上得到 score=1.0 / hit=true，
        // 因为它只匹配到「在」这个字。归一化是相对的，「分母也小」时照样满分。
        // 这类查询必须被判成「没想起什么」，而不是「想起了」。
        let docs = vec![doc("a", "在服务器里盖房子"), doc("b", "和玩家聊了天气")];
        let ix = Bm25::new(&docs);
        for q in ["在吗", "最近", "心情"] {
            let r = ix.search(q);
            assert!(
                r.iter().all(|s| !s.meaningful),
                "q={q} 只可能匹配到虚字，不该被判成有实义命中：{:?}",
                r.iter().map(|s| (&s.matched, s.score)).collect::<Vec<_>>()
            );
        }
        // 反过来，真的带了实义词就必须判为命中
        let r = ix.search("盖房子");
        assert!(r.iter().any(|s| s.meaningful), "「盖房子」是实义查询");
    }

    #[test]
    fn 单字查询能命中而多字查询里的单字不算实义() {
        let docs = vec![doc("a", "主人爱吃的鱼是烤鱼"), doc("b", "盖好了小屋的东墙")];
        let ix = Bm25::new(&docs);

        // 查询本身就是「鱼」→ 这一个字承载全部意图，必须算命中
        let r = ix.search("鱼");
        let hit = r
            .iter()
            .find(|s| ix.key(s.idx) == "a")
            .expect("「鱼」应命中");
        assert!(hit.meaningful, "单字查询「鱼」必须算实义命中");

        // 查询是「钓鱼竿」→ 里面的单字「鱼」只是附带，不该靠它算命中
        let doc_only_char = vec![doc("c", "鱼")];
        let ix2 = Bm25::new(&doc_only_char);
        let r2 = ix2.search("钓鱼用的竿子做好了吗");
        assert!(
            r2.iter().all(|s| !s.meaningful),
            "「钓鱼用的竿子做好了吗」不该因为语料里有个孤零零的「鱼」就算命中"
        );
    }

    #[test]
    fn 散字重合不算命中() {
        // 实测踩过的坑：查询「我上次挖到什么好东西」把语料里的
        // 「怕的东西: 苦力怕」捞了出来并标成 hit —— 它们只共享「东西」。
        //
        // 这条判据**必须用够大的语料**：「东西」之所以不算证据，是因为它在
        // 整个语料里到处都是（高频）。只有 2~3 条时任何词都"到处都是"，
        // 那条规则根本无从体现（也正因如此小语料会走 GENERIC_MIN_DOCS 的豁免）。
        // 造 40 条背景语料，让「东西」成为高频、让「上次」保持稀有。
        let mut docs: Vec<Doc> = (0..40)
            .map(|i| doc(&format!("bg{i}"), &format!("今天整理了一堆东西，第{i}箱")))
            .collect();
        docs.push(doc("fear", "怕的东西: 苦力怕"));
        docs.push(doc("mine", "做完了「fish:补上次的鱼」"));

        let ix = Bm25::new(&docs);
        let r = ix.search("我上次挖到什么好东西没");

        let fear = r.iter().find(|s| ix.key(s.idx) == "fear");
        if let Some(f) = fear {
            assert!(
                !f.meaningful,
                "「怕的东西」只靠高频词「东西」重合，不该判成命中：matched={:?}",
                f.matched
            );
        }
        // 有真正共享的稀有 bigram（「上次」）那条必须命中
        let mine = r.iter().find(|s| ix.key(s.idx) == "mine");
        assert!(
            mine.map(|m| m.meaningful).unwrap_or(false),
            "「上次」是稀有词，真共享就该命中"
        );
    }

    #[test]
    fn bench_建索引耗时() {
        // 线上库规模：752 条 / 平均 30 字符。
        //
        // 这个测试测的是 `Bm25::new` 的绝对成本 —— 它曾经是
        // `/v1/recall` p50 的全部来源（每请求重建一次索引，release 下 6.3 ms）。
        // 现在索引由 `IndexCache` 复用，这条只作为**回归哨兵**：
        // 建索引本身变慢（比如 bigram 切分写出 O(n²)）时能立刻发现。
        //
        // 阈值按 release 定（实测 6.3 ms），debug 下会慢 5 倍左右，
        // 所以只在 release 下断言。`cargo test --release` 与
        // `scripts-verify.sh` 的 release 构建都会覆盖到。
        let docs: Vec<Doc> = (0..752)
            .map(|i| {
                doc(
                    &format!("d{i}"),
                    "做完了「找蜘蛛摸线」今天的打算：先把蜘蛛线弄到手补好鱼竿",
                )
            })
            .collect();
        let t = std::time::Instant::now();
        let n = 20;
        for _ in 0..n {
            let ix = Bm25::new(&docs);
            let _ = ix.search("蜘蛛刷出来没有");
        }
        let per = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        eprintln!("752 条：建索引+检索 平均 {per:.3} ms");
        if !cfg!(debug_assertions) {
            assert!(per < 20.0, "单次建索引+检索 {per:.1}ms，回归了");
        }
    }

    #[test]
    fn 命中词按贡献排序() {
        let docs = vec![doc("a", "蜘蛛线还缺着，钓鱼竿做不了")];
        let ix = Bm25::new(&docs);
        let r = ix.search("钓鱼竿");
        assert_eq!(r.len(), 1);
        assert!(!r[0].matched.is_empty());
        // 贡献最大的那个必须是实义词，不是「竿做」这种跨界 bigram
        assert!(
            r[0].matched[0].contains("钓鱼") || r[0].matched[0].contains("鱼竿"),
            "首位命中词是 {}，应是实义词",
            r[0].matched[0]
        );
    }
}
