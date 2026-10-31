//! 从台词里**自动识别承诺**。
//!
//! # 为什么需要它
//!
//! 中枢跑了几天，`promise` 表一条数据都没有。查下来不是坏了：
//!
//! - MC 侧确实有 `promise` 工具，`PromiseService.make()` 也通；
//! - 但它写数据的**唯一**路径是「模型主动调用 `promise{action:make}`」；
//! - 而提示词里没有任何一处引导它做承诺，模型自然一次都没调过。
//!
//! 结果就是：它嘴上说「我等下帮你修屋顶」，这句承诺**没有落下任何痕迹** ——
//! 下次提起来，它要么忘了，要么临时编。
//!
//! # 为什么在服务端识别，而不是靠模型调工具
//!
//! 依赖模型自觉是这个系统里反复出问题的地方（同 `/v1/context` 那条注释）。
//! 「答应」是一个**语言行为**：人说了「我明天帮你」，承诺就成立了，
//! 不需要说话的人额外再做一次自我标注。所以这里从文本里认。
//!
//! # 认的边界（宁可漏，不可错）
//!
//! 承诺是会被追责的东西：认错一条（把「我明天可能去看看」当成承诺）
//! 比漏掉一条更糟 —— 它会在上下文里挂着一个根本没答应过的义务。
//! 所以规则是：
//!
//! 1. 必须同时有**第一人称意愿**（我 / 本鱼 / 咱）与**将来时**（会 / 明天 / 等下…）；
//! 2. 不能是问句、不能是否定句（「我不会」「我明天不一定」都不算）；
//! 3. 时限能抽出来就抽（明天 / 今晚 / 等会儿 → `due_ms`），抽不出就留 `None`，
//!    不瞎猜一个时间。

/// 从一句台词里识别出的承诺。
#[derive(Debug, Clone, PartialEq)]
pub struct Detected {
    /// 承诺内容（从「我」开始到句末，或整句）。
    pub description: String,
    /// 解析出的任务时限（毫秒时间戳）。抽不出就是 `None`。
    pub due_ms: Option<i64>,
}

/// 第一人称自称。
const SELF_WORDS: &[&str] = &["我", "本鱼", "咱", "人家"];

/// 将来时/意愿标记。
const FUTURE_WORDS: &[&str] = &[
    "会",
    "要",
    "准",
    "马上",
    "立刻",
    "这就",
    "等下",
    "等会",
    "等会儿",
    "一会儿",
    "稍后",
    "明天",
    "后天",
    "今晚",
    "晚上",
    "下午",
    "回头",
    "待会",
    "下回",
    "下次",
    "记得",
    "帮你",
    "给你",
    "替你",
    "拿去",
    "留着",
    "包在我",
    "交给我",
];

/// 承诺性动词：出现了才可能是承诺（否则「我会很累」不该算）。
const COMMIT_VERBS: &[&str] = &[
    "帮", "替", "给", "做", "修", "盖", "建", "挖", "找", "带", "送", "拿", "留", "看", "守", "等",
    "接", "还", "教", "告诉", "问", "算", "搞定", "弄", "准备", "安排", "答应",
];

/// 否定/不确定标记：出现就整句不作数。
const NEGATORS: &[&str] = &[
    "不",
    "别",
    "没",
    "不会",
    "不用",
    "不必",
    "算了",
    "maybe",
    "可能",
    "也许",
    "大概",
    "说不定",
    "不一定",
    "看情况",
    "尽力",
    "试试",
];

/// 疑问标记。
const QUESTION_MARKS: &[char] = &['？', '?', '吗', '呢', '吧'];

/// 相对时间 → 毫秒偏移。
///
/// 只处理**能确定到某一天**的（今晚/明天/后天），其余按「几小时内」给一个
/// 保守值。抽不出就 `None` —— 瞎猜一个 `due_ms` 会让面板上出现假的截止时间。
fn relative_due(text: &str, now_ms: i64) -> Option<i64> {
    const HOUR: i64 = 3_600_000;
    const DAY: i64 = 86_400_000;
    if text.contains("马上") || text.contains("立刻") || text.contains("这就") {
        return Some(now_ms + HOUR / 2);
    }
    if text.contains("等会儿")
        || text.contains("等会")
        || text.contains("等下")
        || text.contains("一会儿")
        || text.contains("稍后")
        || text.contains("待会")
    {
        return Some(now_ms + 2 * HOUR);
    }
    if text.contains("今晚") || text.contains("晚上") {
        return Some(now_ms + 8 * HOUR);
    }
    if text.contains("下午") {
        return Some(now_ms + 6 * HOUR);
    }
    if text.contains("明天") {
        return Some(now_ms + DAY);
    }
    if text.contains("后天") {
        return Some(now_ms + 2 * DAY);
    }
    if text.contains("下次") || text.contains("下回") || text.contains("回头") {
        return Some(now_ms + 3 * DAY);
    }
    None
}

/// 识别一段文本里的承诺。返回 `None` 表示这句不像承诺。
///
/// `now_ms` 只用来算 `due_ms`。
pub fn detect(text: &str, now_ms: i64) -> Option<Detected> {
    let raw = text.trim();
    if raw.is_empty() {
        return None;
    }
    // 只看句子级别：一段回复里可能夹着多个分句，逐句判能少误伤。
    for sentence in split_sentences(raw) {
        let s = sentence.trim();
        if s.chars().count() < 3 || s.chars().count() > 120 {
            continue;
        }
        // 问句不当承诺
        if s.chars().any(|c| QUESTION_MARKS.contains(&c)) {
            continue;
        }
        // 否定 / 不确定不当承诺
        if NEGATORS.iter().any(|w| s.contains(w)) {
            continue;
        }
        // 必须有第一人称
        if !SELF_WORDS.iter().any(|w| s.contains(w)) {
            continue;
        }
        // 必须有将来时标记
        if !FUTURE_WORDS.iter().any(|w| s.contains(w)) {
            continue;
        }
        // 必须有承诺性动词
        if !COMMIT_VERBS.iter().any(|w| s.contains(w)) {
            continue;
        }
        // 描述里去掉「本鱼/人家」，统一成「我」，让面板上的文案自然
        let mut desc = s.to_string();
        for w in ["本鱼", "人家"] {
            desc = desc.replace(w, "我");
        }
        return Some(Detected {
            description: desc,
            due_ms: relative_due(s, now_ms),
        });
    }
    None
}

/// 按中英文句读切句。保留标点，便于后续整句展示。
fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        cur.push(ch);
        if matches!(ch, '。' | '！' | '!' | '；' | ';' | '\n' | '？' | '?') {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn d(text: &str) -> Option<String> {
        detect(text, NOW).map(|x| x.description)
    }

    #[test]
    fn 典型承诺能认出来() {
        for t in [
            "我明天帮你修屋顶。",
            "本鱼等会儿去给你挖点煤！",
            "这个包在我身上，我马上就做。",
            "我回头帮你把箱子理了。",
        ] {
            assert!(d(t).is_some(), "应认出承诺：{t}");
        }
    }

    #[test]
    fn 问句不算承诺() {
        assert!(d("我明天帮你好吗？").is_none());
        assert!(d("我会帮你吗").is_none());
    }

    #[test]
    fn 否定和不确定不算承诺() {
        for t in [
            "我不会帮你做的。",
            "我明天不帮你。",
            "我可能去看看。",
            "我尽力试试。",
            "我看情况吧。",
        ] {
            assert!(d(t).is_none(), "不该当成承诺：{t}");
        }
    }

    #[test]
    fn 没有意愿或没有动作都不算() {
        assert!(d("我明天很忙。").is_none(), "有意愿词但没有承诺动作");
        assert!(d("帮我修屋顶。").is_none(), "没有第一人称");
        assert!(d("屋顶塌了。").is_none());
    }

    #[test]
    fn 时限能抽出来() {
        let x = detect("我明天帮你修屋顶。", NOW).unwrap();
        assert_eq!(x.due_ms, Some(NOW + 86_400_000));
        let y = detect("我马上去挖煤。", NOW).unwrap();
        assert_eq!(y.due_ms, Some(NOW + 1_800_000));
        assert!(
            detect("我帮你修屋顶。", NOW).unwrap().due_ms.is_none(),
            "抽不出时限就不猜"
        );
    }

    #[test]
    fn 自称统一成我() {
        assert_eq!(
            d("本鱼等会儿去给你挖点煤！").unwrap(),
            "我等会儿去给你挖点煤！"
        );
    }

    #[test]
    fn 长回复里夹着的承诺也能认出() {
        let long = "唔，好累。我明天帮你把那段墙垒完。今晚先歇着吧。";
        assert!(d(long).is_some(), "应能从多句里挑出承诺那句");
    }

    #[test]
    fn 纯闲聊不会被误认() {
        for t in [
            "今天天气不错。",
            "我饿了。",
            "你昨天挖到什么了？",
            "这图是你自己发的。",
        ] {
            assert!(d(t).is_none(), "不该认成承诺：{t}");
        }
    }
}
