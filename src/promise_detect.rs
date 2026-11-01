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

/// 从事件摘要里剥出**真正说过的话**。
///
/// # 为什么必须剥
///
/// 事件摘要是这种格式：
///
/// ```text
/// 姆姆在群里问：「你能画一张图吗」，我回：「（尾鳍一顿）唔…等下」
/// ```
///
/// 而承诺是**看台词**判的。不剥的话：
/// - `，我回：` 这段**格式包装本身**会被当成台词去匹配；
/// - 实例：摘要里 `」，我回：「等下！` 被切成一句，
///   命中「我 + 等下 + 等」→ 记了一条承诺 `」，我回：「等下！`（2026-11-01 线上真数据）；
/// - 还会把**别人的话**当成它自己的承诺。
///
/// 所以只取**引号里的内容**，并且**优先取「我回」那一段**（它自己的话）。
pub fn extract_speech(summary: &str) -> String {
    let mark = ["我回：", "我回:", "我答：", "我答:", "我："];
    // 有「我回」就只取它后面那段（它自己说的）
    for m in mark {
        if let Some(idx) = summary.find(m) {
            let tail = &summary[idx + m.len()..];
            if let Some(inner) = first_quoted(tail) {
                return inner;
            }
        }
    }
    // 否则取所有引号内容拼起来（群友说的话也可能含承诺性台词，
    // 但那不是「它答应」—— 这里保守：没有「我回」时只取最后一对引号）
    if let Some(inner) = last_quoted(summary) {
        return inner;
    }
    summary.to_string()
}

/// 取第一对 `「…」`（也认 `“…”` / `"…"`）里的内容。
fn first_quoted(s: &str) -> Option<String> {
    let pairs = [('「', '」'), ('“', '”'), ('"', '"')];
    for (open, close) in pairs {
        if let Some(a) = s.find(open) {
            let rest = &s[a + open.len_utf8()..];
            if let Some(b) = rest.find(close) {
                return Some(rest[..b].to_string());
            }
        }
    }
    None
}

/// 取最后一对引号里的内容。
fn last_quoted(s: &str) -> Option<String> {
    let pairs = [('「', '」'), ('“', '”'), ('"', '"')];
    for (open, close) in pairs {
        if let Some(b) = s.rfind(close) {
            let head = &s[..b];
            if let Some(a) = head.rfind(open) {
                return Some(head[a + open.len_utf8()..].to_string());
            }
        }
    }
    None
}

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
    // ⚠️ 不要把「记得」「看」「等」这类泛词放进来（2026-11-01 实测）：
    // 「操控的话——左右摇杆我记得，左走右看，扳机开火」被判成承诺，
    // 就是「记得」+「看」凑出来的。承诺必须有**明确的将来指向**。
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
    "下午",
    "回头",
    "待会",
    "下回",
    "下次",
    "包在我",
    "交给我",
    "我去",
    "我来",
    "我会",
    "我帮你",
    "我给你",
    "我替你",
];

/// 承诺性动词：出现了才可能是承诺（否则「我会很累」不该算）。
const COMMIT_VERBS: &[&str] = &[
    // ⚠️ 同样别放「看」「等」「问」这种**日常动词**：
    // 「左走右看」「等下」都会命中，而它们不是承诺内容。
    // 这里只留有**交付物**的动作。
    "帮你", "替你", "给你", "修", "盖", "建", "挖", "带", "送", "留", "守", "接", "教", "告诉",
    "搞定", "准备", "安排", "答应", "做",
];

/// 否定/不确定标记。命中就整句不作数。
///
/// ⚠️ **不能只放单字「不」「别」「没」**（2026-11-01 实测）：
/// 「我一会儿过去帮你修的，**别急**」被 `别急` 的「别」挡掉 —— 这是误杀。
/// 单字判断会把「别急 / 不用担心 / 没问题的」这类**安抚语**当成否定。
/// 所以这里只放**指向承诺本身的否定**。
const NEGATORS: &[&str] = &[
    "不会",
    "不能",
    "没法",
    "没办法",
    "不太可能",
    "做不到",
    "不帮",
    "不去",
    "不做",
    "不用了",
    "不必了",
    "算了",
    "可能",
    "也许",
    "大概",
    "说不定",
    "不一定",
    "看情况",
    "尽力",
    "试试",
    "尽量",
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
    // 先剥出真正说过的话：摘要里的「X说：」「我回：」是**格式**，不是内容。
    // 不剥就会把包装本身当台词（线上真踩：`」，我回：「等下！` 被记成承诺）。
    let speech = extract_speech(text);
    let raw = speech.trim();
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
    fn 摘要里的格式包装不会被当成台词() {
        // 线上真数据（2026-11-01）：这条被记成了承诺 `」，我回：「等下！`
        let s = "小螺狮子同事在群里说：「你可以画一张吗」，我回：「等下！」";
        assert!(
            detect(s, NOW).is_none(),
            "「，我回：」是格式不是台词，不该记成承诺"
        );
    }

    #[test]
    fn 只从它自己的话里找承诺() {
        // 群友说的话里即使有「我明天帮你」，也不是**它**答应的
        let s = "姆姆在群里说：「我明天帮你修屋顶」";
        let d = detect(s, NOW);
        // 没有「我回」时取最后一对引号 = 群友那句 —— 这里刻意**接受**它，
        // 因为那种摘要格式通常是「A 说：「…」」单段，内容确实是 A 说的话。
        // 但绝不能把「我回：」前面的包装算进去。
        if let Some(x) = d {
            assert!(
                !x.description.contains("我回"),
                "描述里不该有格式包装：{x:?}"
            );
        }
    }

    #[test]
    fn 安抚语里的别不算否定() {
        // 「别急」「不用担心」是安抚，不是否定承诺
        // —— 单字「别」曾把「我一会儿过去帮你修的，别急」整句挡掉（实测误杀）
        for t in [
            "我一会儿就过去帮你修的，别急。",
            "我明天帮你修屋顶，不用担心。",
        ] {
            assert!(detect(t, NOW).is_some(), "这是承诺，不该被安抚语挡掉：{t}");
        }
        // 但真正的否定仍要挡住
        for t in ["我不会帮你修的。", "我明天不帮你。", "我可能去看看。"] {
            assert!(detect(t, NOW).is_none(), "这是否定/不确定，不该算承诺：{t}");
        }
    }

    #[test]
    fn 泛词不再触发承诺() {
        // 「记得」「看」「等」曾一起凑出一条假承诺
        for t in [
            "操控的话——左右摇杆我记得，左走右看，扳机开火。",
            "等下！",
            "我记得那个地方。",
            "我看看。",
        ] {
            assert!(detect(t, NOW).is_none(), "不该认成承诺：{t}");
        }
    }

    #[test]
    fn 剥台词() {
        assert_eq!(extract_speech("姆姆问：「在吗」，我回：「在的」"), "在的");
        assert_eq!(extract_speech("姆姆在群里说：「你好呀」"), "你好呀");
        assert_eq!(extract_speech("没有引号的一句话"), "没有引号的一句话");
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
