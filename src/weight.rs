//! 记忆权重模型。
//!
//! # 为什么要权重
//!
//! 现有 MC 侧只有 `importance` 一个静态分数，它**不随时间衰减、不因被回忆而强化**。
//! 后果是「上周的一件小事」和「昨天的一件小事」权重完全一样，
//! 检索时无法把有限的位置让给真正该被想起的东西。
//!
//! # 四层模型
//!
//! ```text
//! weight = importance^importance_exp
//!        × decay(age)                 时效衰减
//!        × (1 + reinforce_gain × recall_count)   访问强化
//!        × source_trust(tier, channel)           来源可信度
//! ```
//!
//! 每一层的参数都在 [`WeightConfig`] 里，可以改而不用改代码。
//!
//! # 设计取舍
//!
//! - **衰减用半衰期，不用线性**。线性衰减会让「三周前」和「一个月前」几乎没有差别，
//!   而实际使用中最重要的区分恰恰是「最近几天」vs「更早」。
//! - **强化有上限**。被反复回忆的记忆不该无限膨胀到压过一切——
//!   那会让它反复念同一件事。见 [`WeightConfig::reinforce_cap`]。
//! - **衰减不作用于 `Curated`**。人亲手写下的事实（`tier=Curated`）不随时间变淡；
//!   推断出来的才衰减。这与既有的「淘汰只动 extracted」口径一致。

use serde::{Deserialize, Serialize};

use crate::model::FactTier;

/// 一天的毫秒数，衰减计算的基本单位。
pub const DAY_MS: i64 = 86_400_000;

/// 权重模型的全部可调参数。
///
/// 默认值是按「一个对象、几十个认识的人、每天约 100 条记忆」这个规模调的，
/// 不是通用最优值。换场景要重新调，所以它必须可配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeightConfig {
    /// `importance` 的指数。>1 会放大高分项之间的差距，<1 会压缩。
    ///
    /// 默认 1.0（线性）。调到 2.0 时，0.9 分与 0.3 分的比值从 3:1 变成 9:1，
    /// 适合「只想看到最重要的几条」的场景。
    pub importance_exp: f64,

    /// 时效衰减的半衰期（天）。每过这么多天，权重乘以 0.5。
    ///
    /// 默认 30 天：一个月前的记忆优先级减半，但不归零。
    /// 设成 0 或负数 = 关闭衰减。
    pub half_life_days: f64,

    /// 衰减的下限。无论多老，权重不会低于 importance × 这个比例。
    ///
    /// 默认 0.1：极老的记忆仍然能被显式检索到（`recall` 问「很久以前」时），
    /// 只是不会主动占据上下文预算。设成 0 会让老记忆彻底不可见。
    pub decay_floor: f64,

    /// 每次被回忆（进入过上下文）带来的强化增量。
    ///
    /// 默认 0.05：被回忆 10 次权重 ×1.5。
    pub reinforce_gain: f64,

    /// 强化倍数的上限。
    ///
    /// 默认 2.0：最多翻倍。没有上限的话，一件被反复提及的小事会压过所有新信息，
    /// 表现为它反复念同一件事。
    pub reinforce_cap: f64,

    /// `Curated`（人写的）事实的衰减倍率。1.0 = 完全不衰减。
    pub curated_decay_multiplier: f64,

    /// 各渠道的来源可信度乘数。
    ///
    /// 为什么按渠道区分：**弹幕和群聊里可能有玩笑、梗、反串**，
    /// 而 MC 里亲历的事件是直接的。同样的「玩家说 X」，
    /// 从弹幕听到和亲眼看到，可信度不该一样。
    pub channel_trust_mc: f64,
    pub channel_trust_qq: f64,
    pub channel_trust_live: f64,
    pub channel_trust_system: f64,
    /// 自定义渠道的默认可信度。内置四个以外的渠道都按这个算。
    pub channel_trust_default: f64,
}

impl Default for WeightConfig {
    fn default() -> Self {
        Self {
            importance_exp: 1.0,
            half_life_days: 30.0,
            decay_floor: 0.1,
            reinforce_gain: 0.05,
            reinforce_cap: 2.0,
            curated_decay_multiplier: 1.0,
            channel_trust_mc: 1.0,
            channel_trust_qq: 0.9,
            channel_trust_live: 0.8,
            channel_trust_system: 1.0,
            channel_trust_default: 0.8,
        }
    }
}

impl WeightConfig {
    /// 配置校验。启动期就该失败，而不是等第一条记忆算出一个诡异的分数。
    pub fn validate(&self) -> Result<(), String> {
        if !self.importance_exp.is_finite() || self.importance_exp <= 0.0 {
            return Err("importance_exp 必须是正数".into());
        }
        if !self.half_life_days.is_finite() || self.half_life_days < 0.0 {
            return Err("half_life_days 不能为负".into());
        }
        if !(0.0..=1.0).contains(&self.decay_floor) {
            return Err("decay_floor 必须在 [0,1]".into());
        }
        if !(0.0..=1.0).contains(&self.reinforce_gain) {
            return Err("reinforce_gain 必须在 [0,1]".into());
        }
        if self.reinforce_cap < 1.0 {
            return Err("reinforce_cap 不能小于 1（否则强化反而削弱）".into());
        }
        for (name, v) in [
            ("channel_trust_mc", self.channel_trust_mc),
            ("channel_trust_qq", self.channel_trust_qq),
            ("channel_trust_live", self.channel_trust_live),
            ("channel_trust_system", self.channel_trust_system),
            ("channel_trust_default", self.channel_trust_default),
        ] {
            if !v.is_finite() || v < 0.0 {
                return Err(format!("{name} 必须是非负有限数"));
            }
        }
        Ok(())
    }

    /// 渠道可信度。内置四个走各自字段；**自定义渠道**落到 `channel_trust_default`。
    ///
    /// 这样加一个新渠道（比如 `bilibili`）不需要改代码：加一行数据、
    /// 想单独调可信度就在面板上填，懒得调就用默认值。
    pub fn channel_trust(&self, channel: &str) -> f64 {
        match channel {
            "mc" => self.channel_trust_mc,
            "qq" => self.channel_trust_qq,
            "live" => self.channel_trust_live,
            "system" => self.channel_trust_system,
            _ => self.channel_trust_default,
        }
    }

    /// 时效衰减因子，落在 `[decay_floor, 1.0]`。
    ///
    /// 未来的时间戳（时钟不同步、客户端乱报）按「age = 0」处理，
    /// 不能让它算出 >1 的权重——那会让一条错误时间戳的记忆永久霸榜。
    pub fn decay(&self, age_ms: i64) -> f64 {
        if self.half_life_days <= 0.0 {
            return 1.0; // 衰减关闭
        }
        if age_ms <= 0 {
            return 1.0;
        }
        let days = age_ms as f64 / DAY_MS as f64;
        let raw = 0.5_f64.powf(days / self.half_life_days);
        raw.max(self.decay_floor)
    }

    /// 访问强化因子，落在 `[1.0, reinforce_cap]`。
    pub fn reinforce(&self, recall_count: u32) -> f64 {
        let gain = self.reinforce_gain * recall_count as f64;
        (1.0 + gain).min(self.reinforce_cap)
    }
}

/// 参与计算权重的一行输入。
///
/// 单独抽出来是因为它同时被「写入时打分」和「检索时重排」用到，
/// 两处必须用同一套算法，否则检索出来的顺序和存进去的分数会对不上。
#[derive(Debug, Clone)]
pub struct WeightInput {
    pub importance: f64,
    pub tier: FactTier,
    /// 渠道 tag（可自定义）。用字符串而不是枚举：
    /// 渠道集合现在是**可维护的数据**（`channel` 表），不是编译期常量。
    pub channel: String,
    /// 这条记忆发生的时间。
    pub at_ms: i64,
    /// 这条记忆被回忆过多少次。
    pub recall_count: u32,
}

impl WeightInput {
    /// 一次都没被回忆过的输入，用于写入时打分。
    pub fn fresh(importance: f64, tier: FactTier, channel: impl Into<String>, at_ms: i64) -> Self {
        Self {
            importance,
            tier,
            channel: channel.into(),
            at_ms,
            recall_count: 0,
        }
    }
}

/// 按当前配置算出一条记忆的综合权重。
///
/// 返回值不做归一化——它是**相对分数**，只用于同一批候选之间的排序。
/// 归一化会把「全是垃圾」和「全是珍宝」拉成一样的分布，反而丢掉信息。
pub fn compute(cfg: &WeightConfig, input: &WeightInput, now_ms: i64) -> f64 {
    let base = input.importance.clamp(0.0, 1.0).powf(cfg.importance_exp);

    let age = now_ms.saturating_sub(input.at_ms);
    let mut decay = cfg.decay(age);
    if input.tier == FactTier::Curated {
        // 人写的事实不衰减：该乘多少衰减倍率就乘上去。
        // multiplier = 1.0 时把衰减完全抵消掉。
        decay = 1.0 - (1.0 - decay) * (1.0 - cfg.curated_decay_multiplier);
    }

    let reinforce = cfg.reinforce(input.recall_count);
    let trust = cfg.channel_trust(&input.channel);

    base * decay * reinforce * trust
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> WeightConfig {
        WeightConfig::default()
    }

    fn inp(importance: f64, at_ms: i64) -> WeightInput {
        WeightInput::fresh(importance, FactTier::Extracted, "mc".to_string(), at_ms)
    }

    #[test]
    fn 新的权重就是它的重要度() {
        let c = cfg();
        let now = 1_000_000_000_000;
        let w = compute(&c, &inp(0.8, now), now);
        assert!(
            (w - 0.8).abs() < 1e-9,
            "刚发生、未被回忆、MC 渠道 → 应等于重要度"
        );
    }

    #[test]
    fn 半衰期到了权重减半() {
        let c = cfg();
        let now = 1_000_000_000_000;
        let at = now - (c.half_life_days * DAY_MS as f64) as i64;
        let w = compute(&c, &inp(1.0, at), now);
        assert!((w - 0.5).abs() < 1e-6, "过一个半衰期应恰好减半，实得 {w}");
    }

    #[test]
    fn 两个半衰期是四分之一() {
        let c = cfg();
        let now = 1_000_000_000_000;
        let at = now - (2.0 * c.half_life_days * DAY_MS as f64) as i64;
        let w = compute(&c, &inp(1.0, at), now);
        assert!((w - 0.25).abs() < 1e-6, "两个半衰期应是 0.25，实得 {w}");
    }

    #[test]
    fn 衰减有下限不会归零() {
        let c = cfg();
        let now = 1_000_000_000_000;
        let at = now - 3650 * DAY_MS; // 十年
        let w = compute(&c, &inp(1.0, at), now);
        assert!(w >= c.decay_floor - 1e-9, "十年后仍应 ≥ 下限，实得 {w}");
        assert!(w < 0.2, "但应该很低，实得 {w}");
    }

    #[test]
    fn 未来时间戳按零龄处理() {
        let c = cfg();
        let now = 1_000_000_000_000;
        let at = now + 10 * DAY_MS; // 时钟不同步，报了个未来时间
        let w = compute(&c, &inp(1.0, at), now);
        assert!(w <= 1.0 + 1e-9, "未来时间戳不能算出 >1 的权重，实得 {w}");
    }

    #[test]
    fn 关闭衰减时与时间无关() {
        let mut c = cfg();
        c.half_life_days = 0.0;
        let now = 1_000_000_000_000;
        let young = compute(&c, &inp(1.0, now), now);
        let old = compute(&c, &inp(1.0, now - 3650 * DAY_MS), now);
        assert!((young - old).abs() < 1e-9, "关闭衰减后新旧应相同");
    }

    #[test]
    fn 回忆越多权重越高但有上限() {
        let c = cfg();
        let now = 1_000_000_000_000;
        let mut a = inp(0.5, now);
        a.recall_count = 0;
        let mut b = inp(0.5, now);
        b.recall_count = 10;
        let mut z = inp(0.5, now);
        z.recall_count = 10_000;

        assert!(compute(&c, &b, now) > compute(&c, &a, now), "回忆应强化");
        assert!(
            compute(&c, &z, now) <= 0.5 * c.reinforce_cap + 1e-9,
            "强化必须有上限，否则会压过一切"
        );
    }

    #[test]
    fn 人写的事实不衰减() {
        let c = cfg();
        let now = 1_000_000_000_000;
        let old = now - 3650 * DAY_MS;
        let extracted = compute(
            &c,
            &WeightInput::fresh(1.0, FactTier::Extracted, "mc".to_string(), old),
            now,
        );
        let curated = compute(
            &c,
            &WeightInput::fresh(1.0, FactTier::Curated, "mc", old),
            now,
        );
        assert!(curated > extracted, "人写的应比推断的更抗衰减");
        assert!(
            (curated - 1.0).abs() < 1e-9,
            "curated_decay_multiplier=1 应完全不衰减，实得 {curated}"
        );
    }

    #[test]
    fn 渠道可信度影响排序() {
        let c = cfg();
        let now = 1_000_000_000_000;
        let mc = compute(
            &c,
            &WeightInput::fresh(1.0, FactTier::Extracted, "mc".to_string(), now),
            now,
        );
        let live = compute(
            &c,
            &WeightInput::fresh(1.0, FactTier::Extracted, "live".to_string(), now),
            now,
        );
        assert!(mc > live, "MC 亲历应比弹幕更可信：mc={mc} live={live}");
    }

    #[test]
    fn 重要度指数放大差距() {
        let mut c = cfg();
        c.importance_exp = 2.0;
        let now = 1_000_000_000_000;
        let hi = compute(&c, &inp(0.9, now), now);
        let lo = compute(&c, &inp(0.3, now), now);
        let ratio = hi / lo;
        assert!(ratio > 8.0, "指数=2 时 0.9/0.3 的比值应约 9，实得 {ratio}");
    }

    #[test]
    fn 配置校验拦住非法值() {
        let mut c = cfg();
        c.importance_exp = 0.0;
        assert!(c.validate().is_err());

        let mut c = cfg();
        c.half_life_days = -1.0;
        assert!(c.validate().is_err());

        let mut c = cfg();
        c.decay_floor = 1.5;
        assert!(c.validate().is_err());

        let mut c = cfg();
        c.reinforce_cap = 0.5;
        assert!(c.validate().is_err(), "cap<1 会让强化变成削弱");

        assert!(WeightConfig::default().validate().is_ok());
    }

    #[test]
    fn 权重永远非负且有限() {
        let c = cfg();
        let now = 1_000_000_000_000;
        for importance in [0.0, 0.5, 1.0] {
            for days in [0, 1, 100, 100000] {
                let w = compute(&c, &inp(importance, now - days * DAY_MS), now);
                assert!(
                    w.is_finite(),
                    "importance={importance} days={days} 算出 {w}"
                );
                assert!(w >= 0.0);
            }
        }
    }
}
