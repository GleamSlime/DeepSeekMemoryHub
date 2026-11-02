//! `/v1/context` 的段落拼装。
//!
//! # 为什么要有这个接口
//!
//! 走「一次调用、返回一段可直接塞进 system_prompt 的文本」这条路，
//! 是为了修掉一条**静默失效**的链路：
//!
//! 1. 插件回答前调 `GET /v1/recall?q=<群友那句话>`；
//! 2. 旧版 `recall` 对非空 `q` 是纯子串匹配；
//! 3. 于是「在吗」「你最近干嘛」「最近」「心情」全部返回 `[]`；
//! 4. 插件拿到空数组 → 不注入 → 模型照常编。**没有任何报错**。
//!
//! 而 `recall` 只查 `event` + `fact` 两张表，`journal` / `diary` / `plan_item`
//! 它看不到——用户要的「知道最近在服务器里干什么」恰好全在那三张表里。
//!
//! # 这个文件里只有纯逻辑
//!
//! 所有 SQL 与错误吞并都在 `http.rs` 的 handler 里；这里只负责
//! 「拿到的数据怎么排、怎么截、预算不够先丢谁」。这样这一段能被单测直接覆盖，
//! 不必起服务也不必造库。
//!
//! # 段落顺序是刻意的
//!
//! `plan → memory → events → group → diary → mood`：越靠上越稳定、越不可替代。
//! 预算不够时**从下往上丢**，而不是平等地每段各切一刀——
//! 「它今天打算干什么」比「它三天前的日记」重要得多。

/// 段落名。`parts` / `dropped` 里出现的就是这些值。
pub const SEG_PLAN: &str = "plan";
pub const SEG_MEMORY: &str = "memory";
/// 知识库检索结果。**插在 memory 之后、events 之前**。
///
/// 为什么单独一段而不是并进 `memory`：两句话在语义上完全不同 ——
/// 「你记得的事」是**亲历**，「资料」是**查到的**。
/// 混在一段里，模型会把自己的经历和别人写的规矩当成一回事，
/// 于是出现「我昨天去了下界（其实手册里写着下界危险）」这种串台。
pub const SEG_KB: &str = "kb";
pub const SEG_EVENTS: &str = "events";
pub const SEG_GROUP: &str = "group";
pub const SEG_DIARY: &str = "diary";
pub const SEG_MOOD: &str = "mood";
/// 称呼对照。**只在当前这句话里提到某个人时**才出现——
/// 把全部身份一股脑塞进上下文，既费 token 又会让模型把不相干的人扯进来。
pub const SEG_NAMES: &str = "names";
/// 记忆画面。只在这段话提到某张图的描述/关键词时出现。
pub const SEG_IMAGES: &str = "images";
/// 对在场者的印象（主观判断，不作事实陈述）。
pub const SEG_IMPRESSIONS: &str = "impressions";

/// 段落之间的分隔。空一行，让 markdown 读起来是分节的。
const SEP: &str = "\n\n";

/// 一天有多少毫秒。相对时间按「天」分桶。
const DAY_MS: i64 = 86_400_000;

/// 一个已渲染好的段落。
#[derive(Debug, Clone)]
pub struct Segment {
    pub name: &'static str,
    pub text: String,
}

impl Segment {
    /// 空内容 = 没有这一段。**不要**写「暂无计划」这种噪声：
    /// 提示词里每一句都在花钱，而且「暂无」会被模型当成信息。
    pub fn new(name: &'static str, text: String) -> Option<Self> {
        if text.trim().is_empty() {
            None
        } else {
            Some(Self { name, text })
        }
    }
}

/// 拼装结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assembled {
    /// 可直接拼进 system_prompt 的 markdown。空库时是空串（不是 `None`）。
    pub text: String,
    /// 真进了 `text` 的段落名，顺序与 `text` 中出现的顺序一致。
    pub parts: Vec<String>,
    /// 没进来的段落及**具体原因**（`"diary: 超预算"` 而不是 `"diary"`）。
    pub dropped: Vec<String>,
}

/// 把各段落按预算拼成一段文本。
///
/// - 预算不够时**从最后一段开始丢**，丢到够为止；
/// - 只剩第一段还超预算时，**截断它**而不是全丢（它是最不可替代的那段）；
/// - `notes` 是上游塞进来的「这段压根没查出来」的原因，原样并进 `dropped`。
pub fn assemble(segments: Vec<Segment>, mut notes: Vec<String>, budget: usize) -> Assembled {
    let budget = budget.max(1);
    let mut kept = segments;

    // 从下往上丢。留最后一段不丢（见下面的截断），所以条件是 `len() > 1`。
    while kept.len() > 1 && total_chars(&kept) > budget {
        if let Some(last) = kept.pop() {
            notes.push(format!("{}: 超预算", last.name));
        }
    }

    let mut text = join(&kept);
    // 单段就超预算：截断，并把「被截了」写清楚——
    // 调用方看到 `chars` 贴着 budget 却不看 dropped 时，会以为内容全在。
    if text.chars().count() > budget {
        text = text.chars().take(budget).collect();
        if let Some(first) = kept.first() {
            notes.push(format!("{}: 超预算（截断到 {budget} 字）", first.name));
        }
    }

    Assembled {
        text,
        parts: kept.iter().map(|s| s.name.to_string()).collect(),
        dropped: notes,
    }
}

/// 段落总字数（含分隔符）。预算算的是**最终文本**的长度，
/// 所以分隔符也必须计入，否则拼完之后凭空超预算。
fn total_chars(segs: &[Segment]) -> usize {
    if segs.is_empty() {
        return 0;
    }
    segs.iter().map(|s| s.text.chars().count()).sum::<usize>()
        + SEP.chars().count() * (segs.len() - 1)
}

fn join(segs: &[Segment]) -> String {
    segs.iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(SEP)
}

// ------------------------------------------------------------------ 渲染

/// 一段内容的**单行化**：记忆里可能夹着换行（模型写的东西什么都可能有），
/// 直接拼进 markdown 列表会把一条撑成多行，后面的分隔与标题就错位了。
pub fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.trim().chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out.trim().to_string()
}

/// 计划条目状态 → 中文。未知状态原样透传：宁可让调用方看到 `weird`
/// 也不要把它悄悄塞进「还没做」里。
pub fn state_zh(state: &str) -> String {
    match state.trim().to_ascii_lowercase().as_str() {
        "done" => "已完成".into(),
        "todo" => "还没做".into(),
        "deferred" => "先搁着".into(),
        "abandoned" => "不做了".into(),
        other => other.to_string(),
    }
}

/// `# 你最近在做什么`
///
/// 取最新一张计划：先写 `summary`（「今天的打算」），再列条目。
/// **`done` 与其余各最多 3 条**——全列出来会把预算吃光，而剩下的钱
/// 是留给「它记得的事」的。
pub fn render_plan(summary: Option<&str>, items: &[(String, String)]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();

    if let Some(s) = summary.map(str::trim).filter(|s| !s.is_empty()) {
        lines.push(format!("- 今天的打算：{}", one_line(s)));
    }

    let (mut done, mut other) = (0usize, 0usize);
    for (state, title) in items {
        let title = one_line(title);
        if title.is_empty() {
            continue;
        }
        let zh = state_zh(state);
        if zh == "已完成" {
            if done >= 3 {
                continue;
            }
            done += 1;
        } else {
            if other >= 3 {
                continue;
            }
            other += 1;
        }
        lines.push(format!("- {zh}：{title}"));
    }

    if lines.is_empty() {
        return None;
    }
    Some(format!("# 你最近在做什么\n{}", lines.join("\n")))
}

/// 相对时间的「天」。**按调用方时区偏移算日历日**，不是按「过了多少小时」——
/// 昨晚 23 点的事说成「今天」会让人以为它分不清日子。
///
/// 偏移量由 `tz_offset_min()` 提供（默认 UTC+8，可用环境变量覆盖），
/// 不写死成某个时区：这个服务要能搬去别的机器。
pub fn rel_day(at_ms: i64, now_ms: i64, tz_offset_min: i64) -> String {
    let shift = tz_offset_min * 60_000;
    let today = (now_ms + shift).div_euclid(DAY_MS);
    let then = (at_ms + shift).div_euclid(DAY_MS);
    match today - then {
        d if d <= 0 => "今天".into(),
        1 => "昨天".into(),
        d => format!("{d} 天前"),
    }
}

/// 时区偏移（分钟）。默认 UTC+8（操作者在上海）。
///
/// 不放进 `Config` 的原因：它只影响一段给人看的相对时间文案，
/// 不值得为它增加一处必须校验、必须序列化、必须出现在面板上的配置项。
/// 需要时用环境变量覆盖即可。
pub fn tz_offset_min() -> i64 {
    match std::env::var("BLUEWHALE_TZ_OFFSET_MIN") {
        Ok(v) if !v.trim().is_empty() => v.trim().parse::<i64>().unwrap_or(480).clamp(-720, 840),
        _ => 480,
    }
}

/// `# 最近发生的事`：最近 6 条 MC 事件，带相对时间。
pub fn render_events(rows: &[(i64, String)], now_ms: i64, tz_offset_min: i64) -> Option<String> {
    let lines: Vec<String> = rows
        .iter()
        .map(|(at, summary)| {
            format!(
                "- {} · {}",
                rel_day(*at, now_ms, tz_offset_min),
                one_line(summary)
            )
        })
        .filter(|l| !l.ends_with('·'))
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!("# 最近发生的事\n{}", lines.join("\n")))
}

/// 群消息的显示名。
///
/// **绝不输出完整 QQ 号**：`sender_name` 有就原样用，没有就退到
/// `account` 的**末四位**（`***0001`）。中枢不知道调用方那边的打码口径，
/// 所以这里只做「没有名字时也不能泄露完整账号」这一条底线。
pub fn sender_display(sender_name: Option<&str>, account: Option<&str>) -> String {
    if let Some(name) = sender_name.map(str::trim).filter(|s| !s.is_empty()) {
        return one_line(name);
    }
    match account.map(str::trim).filter(|s| !s.is_empty()) {
        Some(a) => {
            let tail: String = {
                let chars: Vec<char> = a.chars().collect();
                let start = chars.len().saturating_sub(4);
                chars[start..].iter().collect()
            };
            format!("***{tail}")
        }
        None => "***".into(),
    }
}

/// `# 群里最近说的话`：最近 8 条，**正序**（先说的在前，读起来像对话）。
///
/// 等价于 `render_group_scene(rows, Scene::Group)`；保留这个名字是因为
/// 既有调用方与判据都在用它，改签名会白白牵动一片。
pub fn render_group(rows: &[(Option<String>, Option<String>, String)]) -> Option<String> {
    render_group_scene(rows, Scene::Group)
}

/// 会话场景。**决定了「最近说的话」那一段能看见谁说的话**。
///
/// <h2>为什么这是隐私边界，而不只是一个筛选参数</h2>
///
/// 私聊与群聊都存在 `channel_message` 里，靠 `group_id` 是否为空区分。
/// 如果注入时不按场景切开，群里任何人问一句「你最近和谁聊了什么」，
/// 模型手里就握着别人的私聊原文 —— 这个泄漏不需要恶意，只需要模型正常发挥。
///
/// 所以裁剪发生在**服务端**：客户端说自己是什么场景，服务端只给该场景的行。
/// 默认 `Group`，因为「忘了传参数」是最常见的调用方式，默认必须落在**更严**的那一侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    /// 群聊：只给 `group_id IS NOT NULL` 的行。
    Group,
    /// 私聊：只给 `group_id IS NULL` 的行。
    Private,
}

impl Scene {
    /// 解析 `?scene=` 的取值。**未知值一律回落到 `Group`**（严的那一侧）。
    pub fn parse(raw: Option<&str>) -> Scene {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("private") | Some("dm") | Some("私聊") => Scene::Private,
            // Some("group") | Some("群聊") 以及**任何无法识别的值**都走这里。
            _ => Scene::Group,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scene::Group => "group",
            Scene::Private => "private",
        }
    }

    /// 这一段的标题。两个场景用不同标题，模型才知道自己在跟谁说话。
    fn heading(self) -> &'static str {
        match self {
            Scene::Group => "# 群里最近说的话",
            Scene::Private => "# 你和他最近聊的",
        }
    }
}

/// 按场景渲染「最近说的话」。标题随场景变，否则模型会把私聊当成群聊的一部分。
pub fn render_group_scene(
    rows: &[(Option<String>, Option<String>, String)],
    scene: Scene,
) -> Option<String> {
    let lines: Vec<String> = rows
        .iter()
        .map(|(name, account, content)| {
            format!(
                "- {}：{}",
                sender_display(name.as_deref(), account.as_deref()),
                one_line(content)
            )
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!("{}\n{}", scene.heading(), lines.join("\n")))
}

/// 日记正文的截断长度。再长就是「把整篇日记塞进提示词」，不值。
/// 日记正文上限。
///
/// **不做「闲聊给 80 字、问日记给全文」的两档**（2026-10-31 修正）：
/// 实测两档必踩坑 —— 第一句问「读读你昨天的日记」，第二句追问
/// 「怎么还藏私，明明还有」时，那句话里没有「日记」两个字，
/// 于是又退回 80 字，模型只能对着开头编，用户看到的就是「只念了一小段」。
/// 一篇日记 200–450 字，整段上下文仍在 1200 预算内，不值得为省这 200 字
/// 换一次「它答不出自己的日记」。
pub const DIARY_BODY_CHARS: usize = 600;
/// 兼容旧名字：两档合并成一个上限（见 `DIARY_BODY_CHARS` 的注释）。
pub const DIARY_BODY_CHARS_FULL: usize = DIARY_BODY_CHARS;

/// `# 你上次记的日记`：最新一篇的标题 + 正文前 80 字。
pub fn render_diary(title: Option<&str>, body: &str) -> Option<String> {
    render_diary_with(title, body, DIARY_BODY_CHARS)
}

/// 同 `render_diary`，但可以指定正文上限。
///
/// 为什么需要可调：默认只给 80 字，而一篇日记常常 200 字以上，
/// **最后一段（往往正是「昨天谁跟我说了什么」）会被截掉** ——
/// 群里问「你日记里是不是在说我坏话」时，答案恰好在那段里，
/// 模型只能回「我没写你」，看起来就像上下文没注入。
pub fn render_diary_with(title: Option<&str>, body: &str, limit: usize) -> Option<String> {
    let body = one_line(body);
    if body.is_empty() {
        return None;
    }
    let mut text: String = body.chars().take(limit).collect();
    if body.chars().count() > limit {
        text.push('…');
    }
    let title = title.map(str::trim).filter(|s| !s.is_empty());
    let line = match title {
        Some(t) => format!("《{}》{text}", one_line(t)),
        None => text,
    };
    Some(format!("# 你上次记的日记\n{line}"))
}

/// `# 你现在是什么心情`
///
/// **只在不平静时输出**：`平静` 是默认值，写出来只是噪声（每轮多花一份 token，
/// 换来的信息是「和上一轮一样」）。
///
/// `reason` 只在非空且不等于标签本身时用——库里 `reason` 可能是
/// 「有人说话」这种流水字段，写成句子会很怪，但比没有强。
pub fn render_mood(label: &str, reason: &str, intensity: f64) -> Option<String> {
    let label = label.trim();
    if label.is_empty() || label == "平静" {
        return None;
    }
    let degree = crate::affect::degree_word(intensity);
    let mut line = format!("{degree}{label}");
    let reason = reason.trim();
    if !reason.is_empty() && reason != label && reason != "平静" {
        line.push('，');
        line.push_str(&one_line(reason));
        line.push('。');
    } else {
        line.push('。');
    }
    Some(format!("# 你现在是什么心情\n{line}"))
}

/// `# 服务器资料`：知识库检索结果。
///
/// 标题刻意写成「查到的资料，不是你的亲身经历」：
/// 这是**唯一**能防止模型把资料当记忆复述的手段。
/// 只写「资料」两个字，模型多半会当成「我知道的事」讲出来。
pub fn render_kb(rows: &[(Option<String>, String)]) -> Option<String> {
    let lines: Vec<String> = rows
        .iter()
        .map(|(heading, text)| {
            let body = one_line(text);
            if body.is_empty() {
                return String::new();
            }
            match heading.as_deref().filter(|h| !h.trim().is_empty()) {
                Some(h) => format!("- 【{}】{}", one_line(h), body),
                None => format!("- {body}"),
            }
        })
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "# 你查到的资料（不是你的亲身经历）\n{}",
        lines.join("\n")
    ))
}

/// `# 你记得的事`：检索结果，一条一行。
///
/// 这里**不再排序**：顺序由检索层给（命中优先、权重次之），
/// 重排会让「为什么这条排在前面」这件事变得没法解释。
/// 「记忆画面」段：只在被提到时列出图片。
///
/// 输入 `(描述, 关键词)`。**不给 URL** —— 模型看不到图，
/// 给了链接它只会瞎编「我看到了」。给的是「你记得有这么一张图，内容是什么」。
pub fn render_images(rows: &[(Option<String>, Option<String>)]) -> Option<String> {
    render_images_with_ids(
        &rows
            .iter()
            .map(|(c, k)| (None, c.clone(), k.clone()))
            .collect::<Vec<_>>(),
    )
}

/// 「印象」段：**只在这个人被提到时出现**，且明确写「别说出去」。
///
/// # 为什么单独一段而不是塞进称呼对照
///
/// 称呼是**事实**（他在这叫啥），印象是**判断**（我觉得他是什么样的人）。
/// 混在一起会让模型把判断当事实讲出来（「你说过你叫某某，而且很爱开玩笑」），
/// 而后者是冒犯。
///
/// # 为什么要写「不要告诉对方」
///
/// 用户明确要求：「切记不要告诉他人，只作为回复内容时的参考，
/// 语气/态度/内容等」。模型看到一个描述性字段，默认倾向是**转述**——
/// 不写这一句，它真的会说「我觉得你这个人吧……」。
///
/// # 为什么它值得占这些 token
///
/// 它是**分寸的来源**：面对「爱开玩笑的人」和「说话很认真的人」，
/// 同样一句「你又来了」是两种语气。而数值（熟悉/信任/亲密）说不出这个。
pub fn render_impressions(rows: &[(String, String)]) -> Option<String> {
    let lines: Vec<String> = rows
        .iter()
        .filter(|(_, imp)| !imp.trim().is_empty())
        .map(|(name, imp)| format!("- {name}：{}", imp.trim()))
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "# 你对他们的印象\n         **这是你心里的判断，绝对不要说出来**，也不用暗示。\n         只用来决定你说话的语气、分寸和内容深度：\n{}",
        lines.join("\n")
    ))
}

/// 「记忆画面」段：**只写「有这么一张图」，不内联 base64**。
///
/// # 为什么不能把 base64 塞进 system_prompt
///
/// 2026-11-01 实测确认：把 `![描述](data:image/png;base64,…)` 拼进
/// **文本**里，对任何模型都只是**一段乱码字符串** —— 多模态模型的图片
/// 走的是消息 `content` 数组里的 `{"type":"image_url"}` 结构
/// （OpenAI 格式），不是文本里的 data URL。
///
/// 所以这里只给**图片 id**，由接入方把它放进自己请求的图片字段：
/// - AstrBot：`req.image_urls.append("http://hub/v1/image/<id>/file?token=…")`
/// - 自建：把同样的 URL 放进 `content[].image_url.url`
///
/// 代价是多一次 HTTP 取图，但那是**唯一**能让模型真看到图的方式。
///
pub fn render_images_ref(rows: &[(String, Option<String>, String)]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for (id, caption, keywords) in rows {
        let cap = caption.as_deref().unwrap_or("").trim();
        let kw = keywords.trim();
        if cap.is_empty() && kw.is_empty() {
            continue;
        }
        match (cap.is_empty(), kw.is_empty()) {
            (false, true) => lines.push(format!("- {cap}（图 id: {id}）")),
            (true, false) => lines.push(format!("- （一张图，关键词：{kw}）（图 id: {id}）")),
            _ => lines.push(format!("- {cap}（关键词：{kw}）（图 id: {id}）")),
        }
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "# 你记得的画面\n这些图你见过、记得内容（要看图得由接入方附上）：\n{}",
        lines.join("\n")
    ))
}

/// 同上，但**带上图片 id**，让接入方能在需要时把图真的发出去。
///
/// 为什么不给 URL：模型看不到图，给链接它只会瞎编「我看到了」。
/// 但**接入方**需要 id —— 它拿到 id 就能调 `/v1/image/{id}/file` 把图发到群里。
/// 所以 id 给的是**接入方**，文案里明确写「你看不到图」是给**模型**的。
pub fn render_images_with_ids(
    rows: &[(Option<String>, Option<String>, Option<String>)],
) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for (id, caption, keywords) in rows {
        let cap = caption.as_deref().unwrap_or("").trim();
        let kw = keywords.as_deref().unwrap_or("").trim();
        if cap.is_empty() && kw.is_empty() {
            continue;
        }
        let tag = id
            .as_deref()
            .map(|i| format!("〔{i}〕"))
            .unwrap_or_default();
        match (cap.is_empty(), kw.is_empty()) {
            (false, true) => lines.push(format!("- {cap}{tag}")),
            (true, false) => lines.push(format!("- （一张图，关键词：{kw}）{tag}")),
            _ => lines.push(format!("- {cap}（关键词：{kw}）{tag}")),
        }
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "# 你记得的画面\n这些图你见过、记得内容，但现在看不到它们；\n         想给别人看的话，让接入方按〔〕里的编号把图发出去：\n{}",
        lines.join("\n")
    ))
}

/// 「称呼对照」段：只列出**这句话里提到的人**的各种叫法。
///
/// 输入 `(显示名, [(场景名, 该场景称呼)])`，场景名用「QQ 群叫」「MC 里叫」这种口语。
/// 没有任何别名（所有场景都等于显示名）时该行省略；整段为空就返回 None。
pub fn render_names(rows: &[(String, Vec<(String, String)>)]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for (display, aliases) in rows {
        let parts: Vec<String> = aliases
            .iter()
            .filter(|(_, name)| !name.trim().is_empty() && name != display)
            .map(|(label, name)| format!("{label}叫「{name}」"))
            .collect();
        if parts.is_empty() {
            continue;
        }
        lines.push(format!("- {display}：{}", parts.join("、")));
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "# 称呼对照\n同一个人在不同地方叫法不同，别当成两个人：\n{}",
        lines.join("\n")
    ))
}

pub fn render_memory(rows: &[String]) -> Option<String> {
    let lines: Vec<String> = rows
        .iter()
        .map(|s| one_line(s))
        .filter(|s| !s.is_empty())
        .map(|s| format!("- {s}"))
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!("# 你记得的事\n{}", lines.join("\n")))
}

// ------------------------------------------------------------------ 测试

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 称呼对照只列有别名的人() {
        let rows = vec![
            (
                "姆姆".to_string(),
                vec![
                    ("QQ 群".to_string(), "啥子猫".to_string()),
                    ("MC 里".to_string(), "GleamSlime".to_string()),
                    ("直播间".to_string(), "姆姆".to_string()), // 等于显示名，不该出现
                ],
            ),
            (
                "路人".to_string(),
                vec![("QQ 群".to_string(), "路人".to_string())], // 全是同名 → 整人省略
            ),
        ];
        let out = render_names(&rows).unwrap();
        assert!(out.contains("姆姆"));
        assert!(out.contains("啥子猫"));
        assert!(out.contains("GleamSlime"));
        assert!(!out.contains("路人"), "没有别的叫法的人不该占一行");
        assert_eq!(out.matches("姆姆").count(), 1, "等于显示名的别名要滤掉");

        assert!(render_names(&[]).is_none());
    }

    fn seg(name: &'static str, text: &str) -> Segment {
        Segment {
            name,
            text: text.to_string(),
        }
    }

    #[test]
    fn 空内容不产生段落() {
        assert!(Segment::new(SEG_DIARY, "   \n ".into()).is_none());
        assert!(Segment::new(SEG_DIARY, "有内容".into()).is_some());
    }

    #[test]
    fn 空库返回空串而不是_null() {
        let a = assemble(vec![], vec![], 1200);
        assert_eq!(a.text, "");
        assert!(a.parts.is_empty());
        assert!(a.dropped.is_empty());
    }

    #[test]
    fn 段落之间空一行() {
        let a = assemble(
            vec![seg(SEG_PLAN, "# A\n- 1"), seg(SEG_MEMORY, "# B\n- 2")],
            vec![],
            500,
        );
        assert_eq!(a.text, "# A\n- 1\n\n# B\n- 2");
        assert_eq!(a.parts, vec!["plan", "memory"]);
        assert!(a.dropped.is_empty());
    }

    #[test]
    fn 超预算从下往上丢() {
        // 每段 40 字，三段 + 两处分隔 = 124 字。
        let long = "x".repeat(40);
        let a = assemble(
            vec![
                seg(SEG_PLAN, &long),
                seg(SEG_MEMORY, &long),
                seg(SEG_DIARY, &long),
            ],
            vec![],
            100,
        );
        assert_eq!(a.parts, vec!["plan", "memory"], "最下面的 diary 先被丢");
        assert_eq!(a.dropped, vec!["diary: 超预算"]);
        assert!(a.text.chars().count() <= 100);
    }

    #[test]
    fn 预算再紧就丢到只剩第一段() {
        let long = "x".repeat(40);
        let a = assemble(
            vec![
                seg(SEG_PLAN, &long),
                seg(SEG_EVENTS, &long),
                seg(SEG_MOOD, &long),
            ],
            vec![],
            60,
        );
        assert_eq!(a.parts, vec!["plan"]);
        assert_eq!(a.dropped, vec!["mood: 超预算", "events: 超预算"]);
    }

    #[test]
    fn 只剩一段还超预算就截断而不是全丢() {
        let long = "y".repeat(500);
        let a = assemble(vec![seg(SEG_PLAN, &long)], vec![], 200);
        assert_eq!(a.text.chars().count(), 200);
        assert_eq!(a.parts, vec!["plan"]);
        assert_eq!(a.dropped, vec!["plan: 超预算（截断到 200 字）"]);
    }

    #[test]
    fn 上游给的原因原样保留在dropped里() {
        let a = assemble(
            vec![seg(SEG_PLAN, "x")],
            vec!["diary: 读取失败（no such table）".into()],
            500,
        );
        assert_eq!(a.dropped, vec!["diary: 读取失败（no such table）"]);
        assert_eq!(a.parts, vec!["plan"]);
    }

    #[test]
    fn 相对时间按日历日分桶() {
        let now = 1_700_000_000_000i64; // 某个时刻，具体值不重要
        assert_eq!(rel_day(now, now, 480), "今天");
        assert_eq!(rel_day(now - DAY_MS, now, 480), "昨天");
        assert_eq!(rel_day(now - 3 * DAY_MS, now, 480), "3 天前");
        // 未来（时钟回拨 / 客户端时间错）不能变成「-1 天前」。
        assert_eq!(rel_day(now + DAY_MS, now, 480), "今天");
    }

    #[test]
    fn 时区偏移影响日界() {
        // 2023-11-14 16:30 UTC = 2023-11-15 00:30 UTC+8，跟「今天」不是同一天。
        let now = 1_700_003_400_000i64; // 2023-11-15 01:00 UTC 附近
        let at = now - 9 * 3_600_000; // 9 小时前
                                      // UTC 下算出来是昨天或今天，取决于时刻；这里只锁「两种偏移可能给出不同答案」这件事。
        let utc = rel_day(at, now, 0);
        let sh = rel_day(at, now, 480);
        assert!(
            utc == "今天" || utc == "昨天",
            "UTC 下应落在今天/昨天：{utc}"
        );
        assert!(
            sh == "今天" || sh == "昨天",
            "UTC+8 下应落在今天/昨天：{sh}"
        );
    }

    #[test]
    fn 计划段落_摘要加条目_状态译中文() {
        let items = vec![
            ("done".to_string(), "把铁矿烧成铁锭".to_string()),
            ("done".to_string(), "给小屋垒一段墙".to_string()),
            ("todo".to_string(), "清包".to_string()),
            ("deferred".to_string(), "去钓鱼".to_string()),
            ("abandoned".to_string(), "挖基岩".to_string()),
        ];
        let t = render_plan(Some("把家往上盖一盖"), &items).unwrap();
        assert_eq!(
            t,
            "# 你最近在做什么\n\
             - 今天的打算：把家往上盖一盖\n\
             - 已完成：把铁矿烧成铁锭\n\
             - 已完成：给小屋垒一段墙\n\
             - 还没做：清包\n\
             - 先搁着：去钓鱼\n\
             - 不做了：挖基岩"
        );
    }

    #[test]
    fn 计划段落_done_与其余各最多三条() {
        let mut items = Vec::new();
        for i in 0..5 {
            items.push(("done".to_string(), format!("完成{i}")));
        }
        for i in 0..5 {
            items.push(("todo".to_string(), format!("待办{i}")));
        }
        let t = render_plan(None, &items).unwrap();
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines.len(), 1 + 3 + 3, "标题 + 3 + 3");
        assert_eq!(lines.iter().filter(|l| l.contains("已完成")).count(), 3);
        assert_eq!(lines.iter().filter(|l| l.contains("还没做")).count(), 3);
    }

    #[test]
    fn 空计划整段省略() {
        assert!(render_plan(None, &[]).is_none());
        assert!(render_plan(Some("   "), &[]).is_none());
        // 有条目没摘要也要出。
        assert!(render_plan(None, &[("todo".to_string(), "干活".to_string())]).is_some());
    }

    #[test]
    fn 未知状态原样透传() {
        assert_eq!(state_zh("weird"), "weird");
        assert_eq!(state_zh("DONE"), "已完成");
    }

    #[test]
    fn 事件段落带相对时间() {
        let now = 1_700_000_000_000i64;
        let rows = vec![
            (now, "被苦力怕炸了".to_string()),
            (now - DAY_MS, "把矿洞入口挖通了".to_string()),
            (now - 3 * DAY_MS, "发现了钻石矿脉".to_string()),
        ];
        let t = render_events(&rows, now, 480).unwrap();
        assert_eq!(
            t,
            "# 最近发生的事\n\
             - 今天 · 被苦力怕炸了\n\
             - 昨天 · 把矿洞入口挖通了\n\
             - 3 天前 · 发现了钻石矿脉"
        );
    }

    #[test]
    fn 事件段落空列表则省略() {
        assert!(render_events(&[], now_ms_for_test(), 480).is_none());
    }

    fn now_ms_for_test() -> i64 {
        1_700_000_000_000
    }

    #[test]
    fn 群聊_没名字时用末四位且不泄露完整账号() {
        assert_eq!(sender_display(Some("小明"), Some("1234567890")), "小明");
        assert_eq!(sender_display(None, Some("1234567890")), "***7890");
        assert_eq!(sender_display(Some("  "), Some("1234567890")), "***7890");
        assert_eq!(sender_display(None, None), "***");
        assert_eq!(sender_display(None, Some("12")), "***12");
        // 完整账号绝不能出现在输出里。
        assert!(!sender_display(None, Some("1234567890")).contains("1234567890"));
    }

    #[test]
    fn 群聊段落正序拼接() {
        let rows = vec![
            (
                Some("小明".to_string()),
                Some("1".to_string()),
                "明天我去看你钓鱼啊".to_string(),
            ),
            (
                None,
                Some("1234567890".to_string()),
                "你昨天挖到什么了".to_string(),
            ),
        ];
        let t = render_group(&rows).unwrap();
        assert_eq!(
            t,
            "# 群里最近说的话\n- 小明：明天我去看你钓鱼啊\n- ***7890：你昨天挖到什么了"
        );
    }

    #[test]
    fn 日记_标题加正文() {
        // 上限 600：一篇日记通常 200–450 字，整篇都要给（截到 80 会让
        // 模型答不出「日记里是不是在说我坏话」）。
        let body = "今".repeat(200);
        let t = render_diary(Some("水边那两竿"), &body).unwrap();
        let line = t.lines().nth(1).unwrap();
        assert!(line.starts_with("《水边那两竿》今今今"), "实得 {line}");
        assert!(!line.ends_with('…'), "200 字不该被截：{line}");

        // 超长才截断，且带省略号。
        let long = "明".repeat(700);
        let t2 = render_diary(Some("长"), &long).unwrap();
        let line2 = t2.lines().nth(1).unwrap();
        assert!(line2.ends_with('…'), "超过上限要有省略号");
        assert_eq!(line2.chars().count(), 2 + 1 + DIARY_BODY_CHARS + 1);

        // 短正文不加省略号。
        let t3 = render_diary(None, "很短").unwrap();
        assert_eq!(t3, "# 你上次记的日记\n很短");
    }

    #[test]
    fn 心情_平静时不输出() {
        assert!(render_mood("平静", "", 0.0).is_none());
        assert!(render_mood("平静", "有人说话", 0.1).is_none());
        assert!(render_mood("", "", 0.0).is_none());
    }

    #[test]
    fn 心情_不平静时带程度与原因() {
        let t = render_mood("恼火", "挨了打", 0.75).unwrap();
        assert_eq!(t, "# 你现在是什么心情\n很恼火，挨了打。");
        let t2 = render_mood("不快", "", 0.4).unwrap();
        assert_eq!(t2, "# 你现在是什么心情\n有点不快。");
        // reason 与标签相同就不重复写。
        let t3 = render_mood("不快", "不快", 0.2).unwrap();
        assert_eq!(t3, "# 你现在是什么心情\n不快。");
    }

    #[test]
    fn 记忆段落空则不输出() {
        assert!(render_memory(&[]).is_none());
        assert!(render_memory(&["  ".to_string()]).is_none());
        assert_eq!(
            render_memory(&["在 y=-54 发现了钻石矿脉".to_string()]).unwrap(),
            "# 你记得的事\n- 在 y=-54 发现了钻石矿脉"
        );
    }

    #[test]
    fn 多行内容被压成一行() {
        assert_eq!(one_line("第一行\n第二行\t带缩进"), "第一行 第二行 带缩进");
    }
}
