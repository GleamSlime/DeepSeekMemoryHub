//! 领域模型。
//!
//! 沿用 MC 侧 `P0：陪伴基础设计.md` 已经经过实测打磨的语义（手帐/日记/承诺/情绪），
//! 但**换掉两个基础假设**：
//!
//! - **身份**：不再是「MC 的 UUID」，而是跨渠道的 `person_id`（见 [`Person`]）。
//! - **时间**：不再有「游戏日」这个全局主轴，`at_ms` 是唯一排序依据；
//!   游戏日降级为 MC 渠道的附加标签（[`Channel`] + [`Event::channel_day`]）。

use serde::{Deserialize, Serialize};

/// 记忆来自哪个渠道。
///
/// 这是「跨渠道」这件事的类型级表达：任何一条记忆都必须能回答「这是在哪发生的」，
/// 否则合到一起之后就再也分不开了。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Minecraft 服务器里的角色经历。
    Mc,
    /// QQ 群聊。
    Qq,
    /// 直播间弹幕。
    Live,
    /// 由本服务自己产生（例如日记生成、回滚审计）。
    System,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Mc => "mc",
            Channel::Qq => "qq",
            Channel::Live => "live",
            Channel::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Channel> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mc" => Some(Channel::Mc),
            "qq" => Some(Channel::Qq),
            "live" => Some(Channel::Live),
            "system" => Some(Channel::System),
            _ => None,
        }
    }
}

/// 一个人（跨渠道的「谁」）。
///
/// 存在的理由：MC 玩家是 UUID、QQ 是 QQ 号、直播是平台 uid，**三套 id 天然不通**。
/// 没有这一层，「QQ 里记得 MC 的事」根本无法成立——系统不知道两个 id 是同一个人。
///
/// `status = Pending` 表示「这个账号还没跟任何已知的人是同一个」。
/// 未绑定也照常记录记忆，只是不跨渠道合并；人工绑定后两边记忆自然合并，
/// 因为记忆挂的是 `person_id` 而不是渠道 id。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Person {
    pub person_id: String,
    /// 主显示名（中文优先），进提示词的就是这个。
    pub display_name: String,
    pub status: PersonStatus,
    pub created_ms: i64,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PersonStatus {
    /// 自动创建，尚未与其他渠道的账号建立关联。
    Pending,
    /// 已人工确认归属关系。
    Confirmed,
}

impl PersonStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PersonStatus::Pending => "pending",
            PersonStatus::Confirmed => "confirmed",
        }
    }

    pub fn parse(s: &str) -> PersonStatus {
        match s.trim().to_ascii_lowercase().as_str() {
            "confirmed" => PersonStatus::Confirmed,
            _ => PersonStatus::Pending,
        }
    }
}

/// 某个渠道上的一个账号，指向某个人。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonAccount {
    pub channel: Channel,
    /// UUID / QQ 号 / 平台 uid。
    pub account: String,
    pub person_id: String,
    /// 该渠道上的名字。会变，所以和 `person.display_name` 分开存。
    pub seen_name: Option<String>,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

/// 一条原始事件：世界发生过什么。
///
/// 与 `JournalEntry` 的分工：事件是**未筛选**的观察，手帐是**筛选后**的生活经历。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    /// 涉及的人。可为空（例如「下雨了」）。
    pub person_id: Option<String>,
    /// **唯一时间主轴**，现实 UTC 毫秒。
    pub at_ms: i64,
    pub kind: String,
    pub channel: Channel,
    /// 仅 MC 渠道有值：游戏日 `g<N>`。**只作标签，不参与排序**。
    pub channel_day: Option<String>,
    pub summary: String,
    pub importance: f64,
    pub scope: Scope,
    /// 幂等键。断线重放、客户端重试都靠它去重。
    /// QQ/弹幕会重放，MC 也会重试——没有它，几万条里会混进重复事实。
    pub dedup_key: Option<String>,
}

/// 一条事实：它相信的东西。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: String,
    pub person_id: Option<String>,
    pub key: String,
    pub value: String,
    pub scope: Scope,
    pub status: FactStatus,
    pub tier: FactTier,
    pub confidence: f64,
    pub importance: f64,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub expires_ms: Option<i64>,
    /// 被哪条新事实取代。保留旧行，让「它以前是这么想的」仍然可回答。
    pub superseded_by: Option<String>,
}

impl Fact {
    /// 这条事实当前是否还有效（未被取代、未过期）。
    pub fn is_live(&self, now_ms: i64) -> bool {
        self.status == FactStatus::Confirmed
            && self.superseded_by.is_none()
            && self.expires_ms.map(|e| e > now_ms).unwrap_or(true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactStatus {
    Candidate,
    Confirmed,
    Superseded,
    Forgotten,
}

impl FactStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FactStatus::Candidate => "candidate",
            FactStatus::Confirmed => "confirmed",
            FactStatus::Superseded => "superseded",
            FactStatus::Forgotten => "forgotten",
        }
    }

    pub fn parse(s: &str) -> FactStatus {
        match s.trim().to_ascii_lowercase().as_str() {
            "confirmed" => FactStatus::Confirmed,
            "superseded" => FactStatus::Superseded,
            "forgotten" => FactStatus::Forgotten,
            _ => FactStatus::Candidate,
        }
    }
}

/// 事实的来历。`Curated` 是人写的，`Extracted` 是模型推断的。
///
/// 两者的区别是**淘汰策略**：内存压力下只淘汰推断出来的，
/// 扔掉一个人类亲手输入的事实和扔掉一个猜测不是一回事。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactTier {
    Curated,
    Extracted,
}

impl FactTier {
    pub fn as_str(self) -> &'static str {
        match self {
            FactTier::Curated => "curated",
            FactTier::Extracted => "extracted",
        }
    }

    pub fn parse(s: &str) -> FactTier {
        match s.trim().to_ascii_lowercase().as_str() {
            "curated" => FactTier::Curated,
            _ => FactTier::Extracted,
        }
    }
}

/// 一条事实能传多远。
///
/// 与「关于谁」是两个不同的问题：「关于玩家 X」和「只有玩家 X 能听到」
/// 不是一件事——关于某人的共享事实仍然是共享的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// 只属于某个玩家。
    Player,
    /// 属于它自己（性格、偏好）。
    Companion,
    /// 所有人都能看到。
    Shared,
    /// 关于世界的公共知识。
    World,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Player => "player",
            Scope::Companion => "companion",
            Scope::Shared => "shared",
            Scope::World => "world",
        }
    }

    pub fn parse(s: &str) -> Scope {
        match s.trim().to_ascii_lowercase().as_str() {
            "player" => Scope::Player,
            "shared" => Scope::Shared,
            "world" => Scope::World,
            _ => Scope::Companion,
        }
    }
}

/// 手帐条目：筛选后的生活经历。每日计划的输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: String,
    pub at_ms: i64,
    pub type_: String,
    pub source: String,
    pub summary: String,
    pub details: Option<String>,
    pub importance: f64,
    pub confidence: f64,
    /// 幂等键：同一天同一件事重复到达只写一条。
    pub dedup_key: Option<String>,
}

/// 日记：模型写的散文，给人看的。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diary {
    pub id: String,
    /// day / week / month
    pub kind: String,
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    pub title: Option<String>,
    pub body: String,
    pub model: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// 自我认知：「我是谁」的自述。每晚跟随日记改写一次，旧版本保留。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfNarrative {
    pub id: i64,
    pub body: String,
    pub model: Option<String>,
    pub created_ms: i64,
    pub superseded_ms: Option<i64>,
}
