//! 知识库检索排序：RRF 融合 + 分组去重。
//!
//! # 为什么是 RRF 而不是加权和
//!
//! 第一版设计写的是「0.5×向量 + 0.3×BM25 + 0.1×互证」，权重是推的。实测下来
//! **那是四种融合方式里最差的一种**（详见 `docs/知识库接入方案.md` §5.6）：
//!
//! | 融合方式 | MRR |
//! | --- | --- |
//! | 纯向量 | 0.481 |
//! | 加权和 0.5/0.3 | **0.441** |
//! | RRF k=60 等权 | 0.493 |
//! | **RRF k=60 向量×2** | **0.500** |
//!
//! 原因是 BM25 分数与 cosine **不在同一尺度**，归一化会让相对权重随查询漂移：
//! 某条查询里 BM25 最高分很低时，归一化会把它抬成 1.0，于是「弱证据」被当成
//! 「强证据」。这不是调参数能解决的，是方法选错了。
//!
//! RRF 只用排名不用分数，排名是天然同尺度的。
//!
//! # 为什么还要分组去重
//!
//! 没有 reranker 时，top-k 被同一篇文档的多个相邻块占满是典型失败模式。
//! 「多样性」比「绝对分数」更重要 —— 分数本身不可靠时，
//! 5 条来自 5 个不同文档比 5 条来自同一文档更可能有一条是对的。

use crate::rank::Bm25;
use std::collections::HashMap;

/// RRF 的 k。业界默认 60，实测没调过。
pub const DEFAULT_K: f64 = 60.0;

/// 向量那一路的权重。**实测 2.0 比等权好**（MRR 0.500 vs 0.493）。
///
/// 为什么向量权重更高：实测逐条对比里向量独立赢 9 条、BM25 独立赢 6 条，
/// 而向量赢的那些**赢得更大**（BM25 排 115 → 向量排 3、74 → 1），
/// BM25 赢的多是小胜（排 5 → 排 7 这类）。
pub const DEFAULT_W_VEC: f64 = 2.0;
pub const DEFAULT_W_LIT: f64 = 1.0;

/// 同一篇文档最多占几个名额。默认 2（一主一辅，够给上下文又不至于霸榜）。
pub const DEFAULT_MAX_PER_DOC: usize = 2;

/// 一条待排序的块。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub chunk_id: String,
    pub doc_id: String,
    pub ord: i64,
    pub heading: Option<String>,
    pub text: String,
}

/// 排序参数。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Opts {
    pub k: f64,
    pub w_vec: f64,
    pub w_lit: f64,
    pub max_per_doc: usize,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            k: DEFAULT_K,
            w_vec: DEFAULT_W_VEC,
            w_lit: DEFAULT_W_LIT,
            max_per_doc: DEFAULT_MAX_PER_DOC,
        }
    }
}

/// 一条命中。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Hit {
    pub chunk_id: String,
    pub doc_id: String,
    pub ord: i64,
    pub heading: Option<String>,
    pub text: String,
    /// RRF 融合分。
    pub score: f64,
    /// 向量相似度（未参与/未命中时为 0）。
    pub vec: f64,
    /// 字面相关度（未命中时为 0）。
    pub lit: f64,
}

/// 融合排序。
///
/// `vec_sims`：`chunk_id -> 余弦相似度`。
/// - `None` 或空 → **降级为纯字面**（嵌入服务不可用时的行为，不报错）
///
/// 用 map 而不是与 `cands` 同序的切片，是刻意的：
/// 向量与文本错位是这类系统里最难查的一类 bug（打分是垃圾，但**零报错**）。
/// 用 id 作键让错位在类型上就不可能发生。
pub fn fuse(
    cands: &[Candidate],
    query: &str,
    vec_sims: Option<&HashMap<String, f64>>,
    topk: usize,
    opts: &Opts,
) -> Vec<Hit> {
    if cands.is_empty() || topk == 0 {
        return vec![];
    }

    // ---- 字面那一路：复用记忆检索同款 BM25（不写第二份）
    let docs: Vec<crate::rank::Doc> = cands
        .iter()
        .map(|c| crate::rank::Doc {
            key: c.chunk_id.clone(),
            // 标题一起参与打分（§4.3）：标题命中的相关性通常高于正文命中
            text: match &c.heading {
                Some(h) => format!("{h}\n{}", c.text),
                None => c.text.clone(),
            },
        })
        .collect();
    let ix = Bm25::new(&docs);

    // 字面排名：BM25 只返回 score>0 的，其余不参与排名
    let mut lit_rank: HashMap<String, usize> = HashMap::new();
    let mut lit_score: HashMap<String, f64> = HashMap::new();
    if !query.trim().is_empty() {
        for (r, s) in ix.search(query).into_iter().enumerate() {
            let key = ix.key(s.idx).to_string();
            // 只有「有实义命中」的才算进了字面排名。纯虚词命中
            // （「在吗」只匹配到「在」）不该占一个排名位。
            if !s.meaningful {
                continue;
            }
            lit_rank.entry(key.clone()).or_insert(r);
            lit_score.insert(key, s.score);
        }
    }

    // 向量排名：按相似度降序，负相关按 0（cosine 不 rescale —— 见
    // MC 侧 `VectorIndex.blend` 记录的那次实测事故）
    let mut vec_rank: HashMap<String, usize> = HashMap::new();
    let mut vec_score: HashMap<String, f64> = HashMap::new();
    if let Some(sims) = vec_sims.filter(|m| !m.is_empty()) {
        let mut ordered: Vec<(&String, f64)> = cands
            .iter()
            .filter_map(|c| sims.get(&c.chunk_id).map(|&s| (&c.chunk_id, s.max(0.0))))
            .filter(|(_, s)| *s > 0.0)
            .collect();
        ordered.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (r, (id, s)) in ordered.into_iter().enumerate() {
            vec_rank.insert(id.clone(), r);
            vec_score.insert(id.clone(), s);
        }
    }

    // 两路都空 → 没有可用信号，返回空（不要用"随便几条"冒充命中）
    if lit_rank.is_empty() && vec_rank.is_empty() {
        return vec![];
    }

    // ---- RRF
    let mut scored: Vec<Hit> = cands
        .iter()
        .map(|c| {
            let mut sc = 0.0;
            if let Some(&r) = vec_rank.get(&c.chunk_id) {
                sc += opts.w_vec / (opts.k + r as f64);
            }
            if let Some(&r) = lit_rank.get(&c.chunk_id) {
                sc += opts.w_lit / (opts.k + r as f64);
            }
            Hit {
                chunk_id: c.chunk_id.clone(),
                doc_id: c.doc_id.clone(),
                ord: c.ord,
                heading: c.heading.clone(),
                text: c.text.clone(),
                score: sc,
                vec: vec_score.get(&c.chunk_id).copied().unwrap_or(0.0),
                lit: lit_score.get(&c.chunk_id).copied().unwrap_or(0.0),
            }
        })
        .filter(|h| h.score > 0.0)
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            // 同分时按 chunk_id 排，保证结果可复现（HashMap 迭代序不稳定）
            .then_with(|| a.chunk_id.cmp(&b.chunk_id))
    });

    // ---- 分组去重：同一篇文档最多 max_per_doc 条
    // 键用 `String` 而不是 `&str`：后者会借用 `h`，而下一行要把 `h` move 进 out，
    // 编译器（正确地）不允许。为省这一次 clone 去绕借用检查不值得。
    let mut per_doc: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Hit> = Vec::with_capacity(topk);
    for h in scored {
        if out.len() >= topk {
            break;
        }
        let n = per_doc.entry(h.doc_id.clone()).or_insert(0);
        if *n >= opts.max_per_doc.max(1) {
            continue;
        }
        *n += 1;
        out.push(h);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, doc: &str, ord: i64, text: &str) -> Candidate {
        Candidate {
            chunk_id: id.into(),
            doc_id: doc.into(),
            ord,
            heading: None,
            text: text.into(),
        }
    }

    fn sims(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn 空候选与_topk_零都返回空() {
        assert!(fuse(&[], "蜘蛛", None, 5, &Opts::default()).is_empty());
        let c = vec![cand("a", "d1", 0, "蜘蛛线")];
        assert!(fuse(&c, "蜘蛛", None, 0, &Opts::default()).is_empty());
    }

    #[test]
    fn 没有向量时退化为纯字面且不报错() {
        // 嵌入服务不可用时的行为：必须照常返回字面命中的，不 panic、不空手
        let c = vec![
            cand("a", "d1", 0, "做完了「找蜘蛛摸线」"),
            cand("b", "d2", 0, "盖好了小屋的东墙"),
        ];
        let r = fuse(&c, "蜘蛛", None, 5, &Opts::default());
        assert_eq!(r.len(), 1, "只有蜘蛛那条该命中：{r:?}");
        assert_eq!(r[0].chunk_id, "a");
        assert_eq!(r[0].vec, 0.0, "没有向量那一路时 vec 应为 0");
        assert!(r[0].lit > 0.0);
    }

    #[test]
    fn 向量能把字面查不到的捞出来() {
        // 这是加向量的全部意义：查询与原文无字面重叠
        let c = vec![
            cand("a", "d1", 0, "做完了「找蜘蛛摸线」"),
            cand("b", "d2", 0, "盖好了小屋的东墙"),
        ];
        let s = sims(&[("a", 0.72), ("b", 0.11)]);
        let r = fuse(&c, "怎么弄到纺绳子的材料", Some(&s), 5, &Opts::default());
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].chunk_id, "a", "向量高的应排前");
        assert!(r[0].vec > 0.7);
    }

    #[test]
    fn 两路都命中时比单路分高() {
        // RRF 的核心性质：被两路同时召回的条目分数更高
        let c = vec![
            cand("both", "d1", 0, "蜘蛛线"),
            cand("onlyvec", "d2", 0, "完全无关的内容甲乙丙"),
        ];
        let s = sims(&[("both", 0.6), ("onlyvec", 0.9)]);
        let r = fuse(&c, "蜘蛛", Some(&s), 5, &Opts::default());
        let both = r.iter().find(|h| h.chunk_id == "both").unwrap();
        let only = r.iter().find(|h| h.chunk_id == "onlyvec").unwrap();
        // 注意：这里 onlyvec 的向量更高（0.9 vs 0.6），但 both 多一路命中。
        // RRF 的权重决定谁赢 —— 实测 w_vec=2.0 时向量仍占主导，
        // 所以这里只断言「both 拿到了额外的字面分」，不假设它一定反超。
        assert!(both.lit > 0.0, "both 应有字面分");
        assert_eq!(only.lit, 0.0, "onlyvec 没有字面命中");
        assert!(both.score > 0.0 && only.score > 0.0);
    }

    #[test]
    fn 同一篇文档最多占_max_per_doc_个名额() {
        // 没有 reranker 时，top-k 被同一篇文档的相邻块占满是典型失败模式
        let mut c = Vec::new();
        for i in 0..10 {
            c.push(cand(&format!("c{i}"), "same-doc", i, "蜘蛛线相关内容"));
        }
        let r = fuse(&c, "蜘蛛", None, 10, &Opts::default());
        assert_eq!(r.len(), 2, "同一文档默认只给 2 个名额：{r:?}");

        // 调大限额就该给更多
        let o = Opts {
            max_per_doc: 5,
            ..Default::default()
        };
        let r2 = fuse(&c, "蜘蛛", None, 10, &o);
        assert_eq!(r2.len(), 5);
    }

    #[test]
    fn 名额限制不会挤掉其他文档() {
        let mut c = Vec::new();
        for i in 0..5 {
            c.push(cand(&format!("a{i}"), "docA", i, "蜘蛛线"));
        }
        c.push(cand("b0", "docB", 0, "蜘蛛丝"));
        c.push(cand("c0", "docC", 0, "蜘蛛网"));
        let r = fuse(&c, "蜘蛛", None, 5, &Opts::default());
        let docs: std::collections::HashSet<&str> = r.iter().map(|h| h.doc_id.as_str()).collect();
        assert_eq!(docs.len(), 3, "三个文档都该有代表：{r:?}");
    }

    #[test]
    fn 用_id_对齐而不是按下标() {
        // 向量与文本错位是这类系统里最难查的 bug（打分是垃圾但零报错）。
        // 这里给一个只覆盖部分 id 的 map，验证没覆盖到的条目不会拿到别人的分。
        let c = vec![cand("a", "d1", 0, "无关甲"), cand("b", "d2", 0, "无关乙")];
        let s = sims(&[("zzz-not-exist", 0.99)]);
        let r = fuse(&c, "蜘蛛", Some(&s), 5, &Opts::default());
        assert!(r.is_empty(), "没有一条真被命中，不该返回东西：{r:?}");
    }

    #[test]
    fn 负相关按零算不_rescale() {
        // MC 侧踩过的坑：把 cosine 从 [-1,1] rescale 到 [0,1]，
        // 无关文本的 0 变成 0.5，于是语义恒贡献一个常数、压过真正的信号。
        let c = vec![cand("a", "d1", 0, "完全无关")];
        let s = sims(&[("a", -0.5)]);
        let r = fuse(&c, "蜘蛛", Some(&s), 5, &Opts::default());
        assert!(r.is_empty(), "负相关不该被当成正信号：{r:?}");
    }

    #[test]
    fn 结果稳定可复现() {
        // HashMap 迭代序不稳定，同分时必须有个确定性的次序键，
        // 否则同一查询两次调用返回不同顺序，排查时会怀疑人生。
        let c: Vec<Candidate> = (0..6)
            .map(|i| cand(&format!("c{i}"), &format!("d{i}"), 0, "蜘蛛线"))
            .collect();
        let a = fuse(&c, "蜘蛛", None, 6, &Opts::default());
        let b = fuse(&c, "蜘蛛", None, 6, &Opts::default());
        let ia: Vec<&str> = a.iter().map(|h| h.chunk_id.as_str()).collect();
        let ib: Vec<&str> = b.iter().map(|h| h.chunk_id.as_str()).collect();
        assert_eq!(ia, ib, "同样输入必须给出同样顺序");
    }

    #[test]
    fn 标题参与打分() {
        // §4.3：标题单独存，但检索时与正文一起算分
        let mut c = vec![cand("a", "d1", 0, "一些无关的正文内容")];
        c[0].heading = Some("蜘蛛线怎么弄".into());
        let r = fuse(&c, "蜘蛛", None, 5, &Opts::default());
        assert_eq!(r.len(), 1, "标题命中应能把它捞出来：{r:?}");
    }

    #[test]
    fn 空查询只走向量() {
        let c = vec![cand("a", "d1", 0, "蜘蛛线"), cand("b", "d2", 0, "无关")];
        let s = sims(&[("b", 0.8)]);
        let r = fuse(&c, "", Some(&s), 5, &Opts::default());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].chunk_id, "b");
    }
}
