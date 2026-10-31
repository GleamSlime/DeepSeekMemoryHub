//! 事件重要度的**自动判定**。
//!
//! # 为什么需要它
//!
//! 之前重要度完全由调用方拍一个常数：QQ 群消息一律 0.3、被 @ 的对话一律 0.6。
//! 结果是「今天天气不错」和「有人在群里骂它」在检索里权重一样 ——
//! 记忆量一上来，真正要紧的事就被闲聊淹掉了。
//!
//! 这个模块把它换成几个**可解释的维度**相加，权重都能在面板上看到：
//!
//! | 维度 | 含义 | 为什么它该影响重要度 |
//! | --- | --- | --- |
//! | 基底 | 事件类型（死亡 / 被攻击 / 承诺 / 闲聊…） | 死亡天然比一句寒暄重要 |
//! | 关系 | 涉及的人与它有多亲近 | 越是亲近的人做的事，越值得记住 |
//! | 命中 | 摘要命中了多少条既有事实 / 知识库 | 触到已知的事说明这条在「补充/推翻」某个认知 |
//! | 自我 | 摘要里提到它自己 | 关于自己的事当然比路人甲的事重要 |
//! | 噪声 | 太短、纯表情、纯标点 | 这类东西记住了也没用 |
//!
//! # 为什么不做成 AI 判断
//!
//! 中枢的定位是**存储与检索**，不调模型（见 `docs/架构方案.md`）。
//! 每写一条事件都过一次模型，写吞吐会掉两个数量级，而且会把
//! 「记忆的可靠性」绑在模型的可用性上。
//!
//! 但**给 AI 留了口子**：算出来的值只是「默认值」，调用方可以用
//! `PATCH /v1/event/{id}` 覆盖它（`importance_source` 会记成 `manual`），
//! 所以想接一个更强的模型来判重要度，随时可以在调用方那一侧做。

/// 一个事件参与打分的全部已知信息。
#[derive(Debug, Clone, Default)]
pub struct ImportanceInput {
    /// 事件类型，如 `player.join` / `group.said` / `died`。
    pub kind: String,
    /// 摘要原文。
    pub summary: String,
    /// 涉及的人与它的亲密度 `[0,100]`（`relation.closeness`）。没有关联人时是 `None`。
    pub closeness: Option<i64>,
    /// 摘要里有几个词命中了既有事实 / 知识库（由调用方数好传进来）。
    pub fact_hits: usize,
    /// 摘要里是否提到它自己的名字。
    pub mentions_self: bool,
}

/// 各维度的权重。全部可调 —— 这些数字是**判断**，不是物理常数，
/// 不同场景该有不同的配比。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImportanceWeights {
    /// 关系维度最多加多少。亲密度 100 时拿满。
    pub relation: f64,
    /// 命中维度最多加多少（命中 3 条及以上拿满）。
    pub facts: f64,
    /// 提到自己加多少。
    pub self_mention: f64,
    /// 噪声惩罚（最多扣多少）。
    pub noise: f64,
    /// 事件类型的基底值。
    pub kind_base_default: f64,
}

impl Default for ImportanceWeights {
    fn default() -> Self {
        Self {
            relation: 0.20,
            facts: 0.15,
            self_mention: 0.10,
            noise: 0.10,
            kind_base_default: 0.40,
        }
    }
}

/// 事件类型的基底值。
///
/// 顺序是从「最不可替代」到「最可有可无」。没列到的类型走
/// `weights.kind_base_default`，而不是猜一个 —— 新类型由接入方自己定义，
/// 中枢不认识它，给中位数最不偏。
pub fn kind_base(kind: &str, weights: &ImportanceWeights) -> f64 {
    let k = kind.trim().to_ascii_lowercase();
    // 前缀匹配：`task.done` / `task_done` / `task.failed` 都落到同一族。
    let hit =
        |p: &str| k == p || k.starts_with(&format!("{p}.")) || k.starts_with(&format!("{p}_"));
    if hit("died") || hit("death") || k.contains("died") {
        0.78
    } else if hit("attacked") || hit("hurt") || hit("damaged") {
        0.72
    } else if hit("rescued") || hit("saved") {
        0.70
    } else if hit("promise") {
        0.62
    } else if hit("player.join") || hit("player.leave") || hit("player.talked") {
        0.55
    } else if hit("task") {
        0.50
    } else if hit("discovery") || hit("found") {
        0.52
    } else if hit("said") || hit("message") || hit("danmaku") || hit("chat") {
        0.30
    } else if hit("activity") || hit("idle") {
        0.28
    } else {
        weights.kind_base_default
    }
}

/// 这条摘要是不是「噪声」：太短、或者几乎没有文字。
///
/// 判得很保守：只有**明显没有信息量**的才扣分（「？」「好的」「666」），
/// 短句本身不一定没意义（「它死了」只有三个字）。所以要求「短」**且**
/// 「字符种类单一」两条同时成立。
fn is_noise(summary: &str) -> bool {
    let s = summary.trim();
    if s.is_empty() {
        return true;
    }
    let chars: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.len() >= 6 {
        return false;
    }
    // 6 字以内：只有「几乎全是同一类字符」才算噪声
    let mut has_cjk = false;
    let mut has_alnum = false;
    let mut punct = 0usize;
    for c in &chars {
        if ('\u{4e00}'..='\u{9fff}').contains(c) {
            has_cjk = true;
        } else if c.is_alphanumeric() {
            has_alnum = true;
        } else {
            punct += 1;
        }
    }
    // 纯标点 / 纯表情
    if punct == chars.len() {
        return true;
    }
    // 全是同一个字重复（「啊啊啊啊」「666666」）
    if chars.windows(2).all(|w| w[0] == w[1]) {
        return true;
    }
    // 极短的纯符号+数字组合（「666」「233」）
    if !has_cjk && has_alnum && chars.len() <= 3 {
        return true;
    }
    // 常见应答词：几乎不携带信息，但确实会有人发
    const FILLERS: &[&str] = &[
        "好的",
        "好",
        "嗯",
        "嗯嗯",
        "哦",
        "噢",
        "啊",
        "哈哈",
        "哈哈哈",
        "hhh",
        "hehe",
        "在吗",
        "在",
        "收到",
        "了解",
        "知道了",
        "行",
        "可以",
        "对",
        "是",
        "没事",
        "谢谢",
        "谢啦",
        "拜拜",
        "晚安",
        "早安",
        "早上好",
    ];
    let lowered = s.to_lowercase();
    FILLERS.contains(&lowered.as_str())
}

/// 算一个事件的重要度，落在 `[0.05, 0.98]`。
///
/// 下限不是 0：一条记忆再琐碎，记得住总比记不住强；上限不取 1.0：
/// 没有任何自动判断配得上「绝对重要」这四个字。
pub fn score(input: &ImportanceInput, weights: &ImportanceWeights) -> f64 {
    let mut v = kind_base(&input.kind, weights);

    // 关系：亲密度 0..100 → 0..weights.relation
    if let Some(c) = input.closeness {
        let f = (c.clamp(0, 100) as f64) / 100.0;
        v += weights.relation * f;
    }

    // 命中：0/1/2/3+ 条 → 0 / 1/3 / 2/3 / 1 满额
    if input.fact_hits > 0 {
        let f = (input.fact_hits.min(3) as f64) / 3.0;
        v += weights.facts * f;
    }

    // 提到自己
    if input.mentions_self {
        v += weights.self_mention;
    }

    // 噪声：扣分，但**扣不死** —— 一条「好」被记住也许正是需要的。
    if is_noise(&input.summary) {
        v -= weights.noise;
    }

    v.clamp(0.05, 0.98)
}

/// 给面板/日志用的解释：为什么是这个分数。
pub fn explain(input: &ImportanceInput, weights: &ImportanceWeights) -> Vec<String> {
    let mut out = vec![format!(
        "类型 {} → 基底 {:.2}",
        if input.kind.is_empty() {
            "（空）"
        } else {
            &input.kind
        },
        kind_base(&input.kind, weights)
    )];
    if let Some(c) = input.closeness {
        out.push(format!(
            "关系亲密度 {c} → +{:.2}",
            weights.relation * (c.clamp(0, 100) as f64) / 100.0
        ));
    }
    if input.fact_hits > 0 {
        out.push(format!(
            "命中 {n} 条已有认知 → +{:.2}",
            weights.facts * (input.fact_hits.min(3) as f64) / 3.0,
            n = input.fact_hits
        ));
    }
    if input.mentions_self {
        out.push(format!("提到自己 → +{:.2}", weights.self_mention));
    }
    if is_noise(&input.summary) {
        out.push(format!("看着像噪声 → −{:.2}", weights.noise));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(kind: &str, summary: &str) -> ImportanceInput {
        ImportanceInput {
            kind: kind.into(),
            summary: summary.into(),
            ..Default::default()
        }
    }

    #[test]
    fn 死亡比闲聊重要() {
        let w = ImportanceWeights::default();
        let died = score(&base("died", "被苦力怕炸死了"), &w);
        let chat = score(&base("group.said", "今天天气不错"), &w);
        assert!(died > chat + 0.3, "died={died:.2} chat={chat:.2}");
    }

    #[test]
    fn 亲近的人权重更高() {
        let w = ImportanceWeights::default();
        let a = base("player.talked", "姆姆跟我说了句话");
        let far = score(&a, &w);
        let mut b = a.clone();
        b.closeness = Some(95);
        let near = score(&b, &w);
        assert!(near > far, "near={near:.2} far={far:.2}");
        assert!(near - far > 0.15, "亲密度差应有明显差别");
    }

    #[test]
    fn 命中既有认知会加分() {
        let w = ImportanceWeights::default();
        let mut i = base("group.said", "主人爱吃的其实是烤鱼不是鱼");
        let before = score(&i, &w);
        i.fact_hits = 3;
        let after = score(&i, &w);
        assert!(after > before, "{before:.2} → {after:.2}");
    }

    #[test]
    fn 提到自己会加分() {
        let w = ImportanceWeights::default();
        let mut i = base("group.said", "姆姆今天好可爱");
        let plain = score(&i, &w);
        i.mentions_self = true;
        assert!(score(&i, &w) > plain);
    }

    #[test]
    fn 噪声会被扣分但不会归零() {
        let w = ImportanceWeights::default();
        for s in ["？", "666", "啊啊啊啊", "...", "好的"] {
            let v = score(&base("group.said", s), &w);
            let normal = score(&base("group.said", "明天一起去挖矿吧"), &w);
            assert!(v < normal, "{s:?} 应该低于正常句：{v:.2} vs {normal:.2}");
            assert!(v >= 0.05, "{s:?} 不该被扣到 0：{v:.2}");
        }
    }

    #[test]
    fn 短但有事实的句子不算噪声() {
        // 「它死了」只有三个字，但显然不是噪声
        let w = ImportanceWeights::default();
        let v = score(&base("died", "它死了"), &w);
        assert!(v > 0.7, "短句不等于噪声：{v:.2}");
    }

    #[test]
    fn 结果永远落在合理区间() {
        let w = ImportanceWeights::default();
        let extreme = ImportanceInput {
            kind: "died".into(),
            summary: "姆姆".repeat(50),
            closeness: Some(100),
            fact_hits: 99,
            mentions_self: true,
        };
        let v = score(&extreme, &w);
        assert!((0.05..=0.98).contains(&v), "{v}");
    }

    #[test]
    fn 解释能说清每一分从哪来() {
        let w = ImportanceWeights::default();
        let i = ImportanceInput {
            kind: "player.talked".into(),
            summary: "姆姆问我在干嘛".into(),
            closeness: Some(80),
            fact_hits: 2,
            mentions_self: true,
        };
        let e = explain(&i, &w);
        assert!(e.len() >= 4, "{e:?}");
        assert!(e[0].contains("基底"));
        assert!(e.iter().any(|x| x.contains("关系")));
        assert!(e.iter().any(|x| x.contains("命中")));
        assert!(e.iter().any(|x| x.contains("自己")));
    }

    #[test]
    fn 未知类型走默认基底() {
        let w = ImportanceWeights::default();
        assert_eq!(kind_base("something.weird", &w), w.kind_base_default);
        // 前缀族要能命中
        assert_eq!(kind_base("task.done", &w), 0.50);
        assert_eq!(kind_base("task_failed", &w), 0.50);
        assert_eq!(kind_base("promise.kept", &w), 0.62);
    }
}
