//! 被动触发的「感知 tick」：**一次请求同时更新 情绪 / 手帐 / 自我认知**。
//!
//! # 为什么合成一次
//!
//! 用户原话：「触发式的最好将大部分情况一起返回给 ai，比如现在的情绪/对话/事件等等，
//! 然后触发式的几个数据一起返回，既能节省请求还能对齐数据」。
//!
//! 分开问的三个具体坏处：
//! 1. **三倍花费** —— 上下文（最近事件 + 最近对话）要传三遍；
//! 2. **互相矛盾** —— 情绪说「雀跃」，手帐说「今天很丧」；
//! 3. **时序错乱** —— 三次请求之间又来了新事件，三条结论基于不同的世界状态。
//!
//! # 情绪为什么交给 AI
//!
//! 用户原话：「情绪过于机械化、可预测，没有不确定性，且很难体现群聊中的情绪变化」。
//! 旧实现是查表（挨打→0.65 愤怒），同样的事永远同样的强度。
//!
//! 所以这里给模型的是**情境**，让它自己判断：
//! - 今天都发生了什么（含群聊里的对话，不再只有 MC 的加减分）；
//! - 现在的情绪是什么、有多强；
//! - **允许它说「没什么波动」** —— 不能每次聊天都开心。

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::llm::{ChatClient, ChatMessage};

/// 一次 tick 的输入：最近发生了什么。
#[derive(Debug, Default, Clone)]
pub struct TickInput {
    /// 最近的事件（按时间正序），已过滤掉低重要度的噪声。
    pub events: Vec<TickEvent>,
    /// 最近的对话（群聊 + 私聊），正序。
    pub talks: Vec<TickTalk>,
    /// 当前情绪（给模型做**惯性**参考：情绪是有惯性的，不该 180° 翻转）。
    pub mood_label: String,
    pub mood_valence: f64,
    pub mood_arousal: f64,
    /// 现在的心情强度（0..1）。
    pub mood_intensity: f64,
    /// 当前自我认知（让模型在此基础上「微调」而不是每次重写）。
    pub self_narrative: Option<String>,
    /// 在场的人及其当前关系。模型据此**小幅**调整，而不是凭空打分。
    pub relations: Vec<TickRelation>,
    /// 距上次 tick 多久（秒）。让模型知道「这是一天的总结」还是「刚过十分钟」。
    pub since_last_seconds: i64,
}

#[derive(Debug, Clone)]
pub struct TickEvent {
    pub at_ms: i64,
    pub channel: String,
    pub kind: String,
    pub summary: String,
    pub importance: f64,
}

/// 一个人当前的关系状态（给模型做增量参考）。
#[derive(Debug, Clone)]
pub struct TickRelation {
    pub person_id: String,
    pub display_name: String,
    pub familiarity: i64,
    pub trust: i64,
    pub closeness: i64,
    /// 这次对话里他说了几句话（参与度的直接证据）。
    pub said: usize,
    /// 对这个人的**印象**（主观判断，不是数值）。空 = 还没有。
    pub impression: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TickTalk {
    pub at_ms: i64,
    pub who: String,
    pub content: String,
    pub is_self: bool,
}

/// 模型对一次 tick 的**全部**回答。
#[derive(Debug, Deserialize)]
pub struct TickOutput {
    pub mood: MoodOut,
    /// 手帐条目（可以一条都没有 —— 这十分钟确实没什么可记的）。
    #[serde(default)]
    pub journal: Vec<JournalOut>,
    /// 只在自我认知确实需要更新时才给；`null` = 不用动。
    #[serde(default)]
    pub self_narrative: Option<String>,
    /// 关系变化（**增量**，不是绝对值）。
    ///
    /// 为什么要增量而不是绝对值：模型看不到完整的历史，
    /// 让它给绝对值就会出现「昨天 60 今天 20」的漂移。
    /// 增量则天然有界：一次最多 +3，想刷也刷不动。
    #[serde(default)]
    pub relations: Vec<RelationDelta>,
}

#[derive(Debug, Deserialize)]
pub struct RelationDelta {
    /// 模型经常只给数字不给 id（实测踩过：整个 tick 因此解析失败，
    /// 连情绪和手帐一起丢了）。所以这里给默认空串，解析后再**过滤掉**
    /// 认不出人的条目 —— 少一条关系增量，远好过整次调用作废。
    #[serde(default)]
    pub person_id: String,
    /// 每次增量都在 `-5..=5` 内（解析时再夹一次）。
    #[serde(default)]
    pub familiarity: i64,
    #[serde(default)]
    pub trust: i64,
    #[serde(default)]
    pub closeness: i64,
    /// 为什么变（一句话，写进面板让关系变化可解释）。
    #[serde(default)]
    pub reason: String,
    /// 更新后的**印象**。只有真的形成了新认识才给；`null` = 不动。
    #[serde(default)]
    pub impression: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MoodOut {
    /// 情绪名，`平静` 表示没什么波动。
    pub label: String,
    /// 效价 [-1, 1]：负=难受，正=舒服。
    pub valence: f64,
    /// 唤醒度 [0, 1]：低=懒散，高=兴奋。
    pub arousal: f64,
    /// 支配感 [-1, 1]：低=无力，高=掌控。
    #[serde(default)]
    pub dominance: f64,
    /// 为什么是这个情绪（一句话，面板上要能看）。
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct JournalOut {
    /// 类型：task_done / said / died / met / mood / discovery / note
    #[serde(default = "d_kind")]
    pub kind: String,
    pub summary: String,
    #[serde(default = "d_imp")]
    pub importance: f64,
}

fn d_kind() -> String {
    "note".into()
}
fn d_imp() -> f64 {
    0.5
}

/// 系统提示词。
///
/// 三个要点，每个都是被线上问题逼出来的：
/// 1. **不许编**：只依据给的事实，别补细节（手帐是「经历记录」，编了就污染记忆）；
/// 2. **情绪允许平淡**：明确写「多数时候应该是平静或轻微」，否则模型会天天开心；
/// 3. **自我认知少改**：它是慢变量，每十分钟重写一次等于没有自我。
const SYSTEM: &str = r#"你是「蓝鲸记忆中枢」的内部整理器，负责在后台把最近发生的事整理成结构化记录。

你会拿到：最近的事件、最近的对话、当前情绪、当前自我认知。

请**同时**给出三样东西，用 JSON 回答（不要 markdown 代码块、不要解释）：

1. **mood** —— 现在的心情。
   - `label`：一个词，如 平静/雀跃/低落/烦躁/好奇/疲惫/期待/满足
   - `valence`：-1（难受）到 1（舒服）
   - `arousal`：0（懒散）到 1（兴奋）
   - `dominance`：-1（无力）到 1（掌控）
   - `reason`：一句话说明为什么（会显示在面板上）
   - **情绪有惯性**：除非发生了明显的事，否则应该朝当前情绪**小幅**变化，
     不要每次 180 度翻转。
   - **多数时候应该是「平静」或轻微波动**。不能每次聊天都开心 ——
     普通闲聊没有情绪价值。只有真的发生了什么才推高。
   - 群聊里的对话**同样**能影响情绪：被夸、被冷落、被追问、有人说难过，都算。

2. **journal** —— 值得记下的经历（0 到 3 条，宁少勿多）。
   - `kind`：`task_done`（做完事）/ `said`（说了重要的话）/ `died`（死亡）/
     `met`（遇见）/ `mood`（情绪事件）/ `discovery`（发现）/ `note`（其它）
   - `summary`：一句话，**只写事实**，不写感受
   - `importance`：0 到 1。闲聊 ≤0.35、日常 ≤0.5、有明显的第一次/损失/承诺 ≥0.7
   - **普通寒暄不要记**。没东西可记就给空数组。

3. **self_narrative** —— 自我认知。
   - **多数时候给 null**（不用改）。
   - 只有当最近的经历确实改变了「我是谁 / 我在意什么」时才给一句新表述。
   - 它要能覆盖旧的，而不是叠加；一句话，30 字以内。

4. **relations** —— 在场人的关系变化（**增量**）。
   - **每条必须带 `person_id`**，值从上面「在场的人」那份清单里**原样抄**，
     不要自己编、不要漏。格式像 `img-…` 或 `p-…` 那种字符串。
   - 只给**这次真的变了**的人；没变化的人不要出现（给空数组）。
   - 三个维度，每次增量都在 **-3 到 +3** 之间，绝大多数情况应该是 0 或 1：
     - `familiarity`（熟悉度）：见面、聊过天就会慢慢涨
     - `trust`（信任）：被托付事、对方守约、说到做到 → 涨；被欺骗、被放鸽子 → 降
     - `closeness`（亲密）：一起经历了什么、被关心 → 涨；被冷落、被冒犯 → 降
   - **不要因为对方说了句好话就给高分**。普通闲聊 familiarity +1 到顶，
     trust/closeness 基本不动。
   - 只有**真的发生了什么**（一起扛过事、被真诚对待、被伤害）才动 trust/closeness。
   - 关系**不会自动衰减**：淡了也不扣分，除非发生了具体的事。
   - 可选 `impression`：对这个人**更新后的印象**，一句话、40 字以内。
     - 它写的是「我觉得他是个什么样的人」——不是他做过什么，是**判断**。
       例：「嘴上爱损人，但会主动帮忙，不用跟他客气」。
     - **多数时候给 null**。只有真的形成了新的认识才改；
       一次普通聊天不该重写你对一个人的看法。
     - 旧印象会拿给你参考：**在它基础上微调**，不要从头重写。

只输出 JSON。没事时的形状：
{"mood":{"label":"平静","valence":0.05,"arousal":0.3,"dominance":0.0,"reason":"普通闲聊"},
 "journal":[],"self_narrative":null,"relations":[]}

有人真的帮了大忙时的形状（注意 person_id 要从清单原样抄）：
{"mood":{"label":"雀跃","valence":0.4,"arousal":0.5,"dominance":0.1,"reason":"有人帮我搭了机器"},
 "journal":[{"kind":"met","summary":"鸡蛋大人帮我搭好刷铁机","importance":0.7}],
 "self_narrative":null,
 "relations":[{"person_id":"p-1a192349897","familiarity":1,"trust":2,"closeness":1,"reason":"他主动帮我搭了刷铁机还出了材料"}]}"#;

/// 跑一次 tick。返回 `(解析结果, 本次用量)`。
///
/// 为什么要带上用量：面板要统计「1/7/30 天花了多少 token」。
/// 之前 tick 的用量**一直是 0**（`run` 只返回解析结果，把 `ChatOutcome`
/// 里的 usage 丢了），于是 tick 占总调用量的一大半却完全不可见。
pub async fn run(
    client: &ChatClient,
    input: &TickInput,
) -> Result<(TickOutput, crate::llm::chat::ChatOutcome)> {
    let user = render_input(input);
    let out = client
        .chat(&[ChatMessage::system(SYSTEM), ChatMessage::user(user)])
        .await?;
    let parsed = parse(&out.text).map_err(|e| Error::Storage(format!("tick 输出解析失败：{e}")))?;
    Ok((parsed, out))
}

/// 把输入渲染成给模型看的一段文本。
///
/// 刻意**不用 JSON 喂输入**：自然语言更好读，模型也更少把它当「要照抄的结构」。
pub fn render_input(i: &TickInput) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "距上次整理已经过去 {} 分钟。\n\n",
        i.since_last_seconds.max(0) / 60
    ));

    s.push_str("## 当前情绪\n");
    s.push_str(&format!(
        "{}（效价 {:.2}，唤醒 {:.2}，强度 {:.2}）\n\n",
        i.mood_label, i.mood_valence, i.mood_arousal, i.mood_intensity
    ));

    if let Some(n) = i.self_narrative.as_deref().filter(|x| !x.trim().is_empty()) {
        s.push_str("## 当前自我认知\n");
        s.push_str(n.trim());
        s.push_str("\n\n");
    }

    s.push_str("## 最近发生的事\n");
    if i.events.is_empty() {
        s.push_str("（没有值得记的事）\n");
    } else {
        for e in &i.events {
            s.push_str(&format!(
                "- [{}] {} · {}（重要度 {:.2}）\n",
                e.channel,
                e.kind,
                one_line(&e.summary, 120),
                e.importance
            ));
        }
    }

    if !i.relations.is_empty() {
        s.push_str("\n## 在场的人（当前关系）\n");
        for r in &i.relations {
            s.push_str(&format!(
                "- {}（id: {}）熟悉 {} / 信任 {} / 亲密 {}，这次说了 {} 句\n",
                r.display_name, r.person_id, r.familiarity, r.trust, r.closeness, r.said
            ));
            if let Some(imp) = r.impression.as_deref().filter(|x| !x.trim().is_empty()) {
                s.push_str(&format!("    我对他的印象：{}\n", imp.trim()));
            }
        }
    }

    s.push_str("\n## 最近的对话\n");
    if i.talks.is_empty() {
        s.push_str("（没有新的对话）\n");
    } else {
        for t in &i.talks {
            // 自己的话标出来：模型判断情绪时「我说了什么」和「别人说了什么」分量不同
            let who = if t.is_self {
                "我".to_string()
            } else {
                t.who.clone()
            };
            s.push_str(&format!("- {}：{}\n", who, one_line(&t.content, 100)));
        }
    }

    s.push_str("\n请按系统提示的 JSON 格式回答。");
    s
}

fn one_line(s: &str, max: usize) -> String {
    let t: String = s.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    let t = t.trim();
    if t.chars().count() <= max {
        t.to_string()
    } else {
        let cut: String = t.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// 解析模型输出。**容忍 markdown 代码块**：很多模型会不听话地包一层 ```json。
pub fn parse(text: &str) -> std::result::Result<TickOutput, String> {
    let t = text.trim();
    // 剥 ```json … ```
    let t = if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        rest.trim_start().strip_suffix("```").unwrap_or(rest).trim()
    } else {
        t
    };
    // 再兜一次：从第一个 `{` 到最后一个 `}`
    let t = match (t.find('{'), t.rfind('}')) {
        (Some(a), Some(b)) if b > a => &t[a..=b],
        _ => t,
    };
    let mut out: TickOutput = serde_json::from_str(t).map_err(|e| e.to_string())?;
    // 数值收敛：模型偶尔给 5 或 -3
    out.mood.valence = out.mood.valence.clamp(-1.0, 1.0);
    out.mood.arousal = out.mood.arousal.clamp(0.0, 1.0);
    out.mood.dominance = out.mood.dominance.clamp(-1.0, 1.0);
    for j in &mut out.journal {
        j.importance = j.importance.clamp(0.0, 1.0);
    }
    // 关系增量**硬夹到 -5..=5**：模型偶尔会一次给 +20（"我们关系很好"），
    // 那一步就能把陌生人变熟人，关系曲线就没有意义了。
    for r in &mut out.relations {
        r.familiarity = r.familiarity.clamp(-5, 5);
        r.trust = r.trust.clamp(-5, 5);
        r.closeness = r.closeness.clamp(-5, 5);
        // 印象要短：它每轮都进上下文，写成长段落是长期负担
        if let Some(imp) = r.impression.as_mut() {
            let t = imp.trim();
            if t.chars().count() > 60 {
                *imp = t.chars().take(60).collect::<String>() + "…";
            } else {
                *imp = t.to_string();
            }
            if imp.trim().is_empty() {
                r.impression = None;
            }
        }
    }
    // 认不出人的条目直接丢：宁可少记一条关系变化，也不能让整次 tick 失败
    // （情绪/手帐/自我认知都在同一次回答里，一起丢代价太大）
    out.relations.retain(|r| !r.person_id.trim().is_empty());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析标准输出() {
        let t = r#"{"mood":{"label":"雀跃","valence":0.6,"arousal":0.7,"dominance":0.2,"reason":"被夸了"},"journal":[{"kind":"said","summary":"有人夸画得好","importance":0.55}],"self_narrative":null}"#;
        let o = parse(t).unwrap();
        assert_eq!(o.mood.label, "雀跃");
        assert_eq!(o.journal.len(), 1);
        assert!(o.self_narrative.is_none());
    }

    #[test]
    fn 容忍_markdown_代码块() {
        let t = "```json\n{\"mood\":{\"label\":\"平静\",\"valence\":0,\"arousal\":0.2,\"reason\":\"没事发生\"},\"journal\":[]}\n```";
        let o = parse(t).unwrap();
        assert_eq!(o.mood.label, "平静");
        assert!(o.journal.is_empty());
    }

    #[test]
    fn 容忍前后废话() {
        let t = "好的，这是结果：\n{\"mood\":{\"label\":\"低落\",\"valence\":-0.4,\"arousal\":0.2,\"reason\":\"死了\"},\"journal\":[]}\n希望有帮助";
        let o = parse(t).unwrap();
        assert_eq!(o.mood.label, "低落");
    }

    #[test]
    fn 数值越界会被收敛() {
        let t = r#"{"mood":{"label":"极乐","valence":5,"arousal":9,"dominance":-7,"reason":"x"},"journal":[{"summary":"a","importance":3}]}"#;
        let o = parse(t).unwrap();
        assert_eq!(o.mood.valence, 1.0);
        assert_eq!(o.mood.arousal, 1.0);
        assert_eq!(o.mood.dominance, -1.0);
        assert_eq!(o.journal[0].importance, 1.0);
    }

    #[test]
    fn 缺字段时给默认值() {
        // 模型经常只给 mood 就交差
        let t = r#"{"mood":{"label":"平静","valence":0,"arousal":0.1,"reason":"x"}}"#;
        let o = parse(t).unwrap();
        assert!(o.journal.is_empty());
        assert!(o.self_narrative.is_none());
        assert_eq!(o.mood.dominance, 0.0);
    }

    #[test]
    fn 关系缺_person_id_只丢这条而不是整次失败() {
        // 实测踩过：模型只给数字不给 id，`person_id` 是必填 →
        // 整个 TickOutput 解析失败 → 情绪和手帐一起丢。
        // 一次可选字段不该让整次调用作废。
        let t = r#"{"mood":{"label":"雀跃","valence":0.4,"arousal":0.5,"reason":"被帮了"},
                    "journal":[{"summary":"他帮了忙","importance":0.6}],
                    "relations":[{"familiarity":1,"trust":2,"closeness":1,"reason":"帮忙"}]}"#;
        let o = parse(t).unwrap();
        assert_eq!(o.mood.label, "雀跃");
        assert_eq!(o.journal.len(), 1, "手帐不能因为关系条目坏了就丢");
        assert!(o.relations.is_empty(), "认不出人的关系条目要丢掉");
    }

    #[test]
    fn 关系增量会被夹到_5() {
        let t = r#"{"mood":{"label":"x","valence":0,"arousal":0,"reason":"x"},
                    "journal":[],
                    "relations":[{"person_id":"p1","familiarity":50,"trust":-30,"closeness":99}]}"#;
        let o = parse(t).unwrap();
        assert_eq!(o.relations[0].familiarity, 5);
        assert_eq!(o.relations[0].trust, -5);
        assert_eq!(o.relations[0].closeness, 5, "一步不许把陌生人变熟人");
    }

    #[test]
    fn 坏输入要报错而不是静默空() {
        assert!(parse("我做不到").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn 渲染输入包含三块() {
        let i = TickInput {
            events: vec![TickEvent {
                at_ms: 1,
                channel: "qq".into(),
                kind: "said".into(),
                summary: "姆姆说想吃饭".into(),
                importance: 0.6,
            }],
            talks: vec![TickTalk {
                at_ms: 2,
                who: "姆姆".into(),
                content: "今天好累啊".into(),
                is_self: false,
            }],
            mood_label: "平静".into(),
            since_last_seconds: 900,
            ..Default::default()
        };
        let t = render_input(&i);
        assert!(t.contains("当前情绪"));
        assert!(t.contains("最近发生的事"));
        assert!(t.contains("最近的对话"));
        assert!(t.contains("15 分钟"), "要说清间隔：{t}");
        assert!(t.contains("今天好累啊"));
    }

    #[test]
    fn 自己的话标成我() {
        let i = TickInput {
            talks: vec![TickTalk {
                at_ms: 1,
                who: "啥子猫".into(),
                content: "晚安".into(),
                is_self: true,
            }],
            ..Default::default()
        };
        let t = render_input(&i);
        assert!(t.contains("- 我：晚安"), "{t}");
    }
}
