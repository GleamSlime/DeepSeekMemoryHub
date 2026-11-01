//! tick 的节流：**攒批 + 最小间隔**。
//!
//! # 为什么不能收到事件就触发
//!
//! 群里一分钟能来几十条消息。每条都触发一次 LLM 会在几分钟内烧掉一天的量，
//! 而且模型看到的是「一句话的世界」，判断出来的情绪全是噪声。
//!
//! 用户原话：「被动触发的经历加一下节流方法，无需立马触发」。
//!
//! # 两道闸
//!
//! 1. **最小间隔**（`tick_interval_seconds`，默认 600s）：
//!    不管攒了多少，两次 LLM 调用之间至少隔这么久。这是省钱的**硬闸**。
//! 2. **攒批**（`tick_batch`，默认 8 条）：够多可以**提前**触发，
//!    但仍然要过一个短的「冷却」（`MIN_GAP`），否则爆发期间会连续打。
//!
//! # 为什么要「攒」而不是「采样」
//!
//! 与大多数限流不同，这里被丢下的东西**不会消失**：
//! 事件留在库里，下次 tick 一起喂给模型。只是「晚一点知道」，
//! 不是「忘掉」。所以节流只影响**延迟**，不影响**完整性**。

/// 节流状态。放内存即可 —— 重启丢失最坏的后果是「立刻多跑一次」。
#[derive(Debug, Default)]
pub struct Throttle {
    /// 上次真正调用的时间（毫秒）。`None` = 从没调过。
    last_run_ms: Option<i64>,
    /// 上次调用时水位线（已处理到哪个事件 id / 时间）。
    watermark_ms: i64,
    /// 自上次调用以来攒了几个「够格」的事件。
    pending: usize,
}

/// 攒批触发的**最短冷却**：即使批够了也不能比这更密。
///
/// 60s 的理由：一次 tick 最便宜也要几秒 + 几千 token。
/// 爆发式对话（群里吵架 5 分钟 100 条）下这个值把调用压到最多 5 次。
const MIN_GAP_MS: i64 = 60_000;

/// 该不该跑？
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 跑，且给出原因（写进 generation_log，面板上能看到「为什么这次跑了」）。
    Run(RunReason),
    /// 不跑，说明为什么（同样入库，排错时能回答「为什么没生成」）。
    Skip(SkipReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunReason {
    /// 攒够了批次。
    BatchFull,
    /// 隔得够久（即使没攒够也跑一次，否则安静期永远不更新情绪）。
    IntervalElapsed,
    /// 手动触发（面板按钮）。
    Manual,
}

impl RunReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RunReason::BatchFull => "batch_full",
            RunReason::IntervalElapsed => "interval",
            RunReason::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 还没到最小间隔。
    TooSoon,
    /// 没有新东西。
    NothingNew,
    /// 总开关关了。
    Disabled,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::TooSoon => "too_soon",
            SkipReason::NothingNew => "nothing_new",
            SkipReason::Disabled => "disabled",
        }
    }
}

impl Throttle {
    /// 记一条「够格」的新事件，返回当前攒了几条。
    pub fn note_event(&mut self, at_ms: i64) -> usize {
        if at_ms > self.watermark_ms {
            self.watermark_ms = at_ms;
            self.pending += 1;
        }
        self.pending
    }

    /// 现在该不该跑。
    ///
    /// `enabled` 来自配置；`batch`/`interval` 也是。
    pub fn decide(&self, now_ms: i64, enabled: bool, batch: usize, interval_secs: u64) -> Decision {
        if !enabled {
            return Decision::Skip(SkipReason::Disabled);
        }
        if self.pending == 0 {
            return Decision::Skip(SkipReason::NothingNew);
        }
        let Some(last) = self.last_run_ms else {
            // 从没跑过：只要有东西就立刻跑一次（把基线建起来）
            return Decision::Run(RunReason::BatchFull);
        };
        let since = now_ms.saturating_sub(last);
        let interval_ms = (interval_secs as i64).saturating_mul(1000);

        // 硬闸：不到最小间隔一律不跑，哪怕批满了
        if since < MIN_GAP_MS {
            return Decision::Skip(SkipReason::TooSoon);
        }
        if self.pending >= batch.max(1) {
            return Decision::Run(RunReason::BatchFull);
        }
        if since >= interval_ms {
            return Decision::Run(RunReason::IntervalElapsed);
        }
        Decision::Skip(SkipReason::TooSoon)
    }

    /// 跑完之后调用：清空待处理、记下时间。
    pub fn mark_run(&mut self, now_ms: i64) {
        self.last_run_ms = Some(now_ms);
        self.pending = 0;
    }

    /// 上次跑的时间（面板显示）。
    pub fn last_run_ms(&self) -> Option<i64> {
        self.last_run_ms
    }

    /// 当前攒了几条（面板显示）。
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// 水位线：下次只取这之后的事件。
    pub fn watermark_ms(&self) -> i64 {
        self.watermark_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_800_000_000_000;

    #[test]
    fn 没东西就不跑() {
        let t = Throttle::default();
        assert_eq!(
            t.decide(T0, true, 8, 600),
            Decision::Skip(SkipReason::NothingNew)
        );
    }

    #[test]
    fn 关掉就永远不跑() {
        let mut t = Throttle::default();
        t.note_event(T0);
        assert_eq!(
            t.decide(T0, false, 8, 600),
            Decision::Skip(SkipReason::Disabled)
        );
    }

    #[test]
    fn 第一次有东西就立刻跑() {
        // 从没跑过时不该等 10 分钟 —— 否则重启后要空转很久
        let mut t = Throttle::default();
        t.note_event(T0);
        assert_eq!(
            t.decide(T0, true, 8, 600),
            Decision::Run(RunReason::BatchFull)
        );
    }

    #[test]
    fn 批满了也要过冷却() {
        let mut t = Throttle::default();
        t.note_event(T0);
        t.mark_run(T0);

        // 刚跑完就有 10 条 —— 但还不到 MIN_GAP
        for i in 1..=10 {
            t.note_event(T0 + i);
        }
        assert_eq!(
            t.decide(T0 + 30_000, true, 8, 600),
            Decision::Skip(SkipReason::TooSoon),
            "30 秒内不许再跑"
        );
        // 过了冷却，批满 → 跑
        assert_eq!(
            t.decide(T0 + MIN_GAP_MS, true, 8, 600),
            Decision::Run(RunReason::BatchFull)
        );
    }

    #[test]
    fn 没攒够但隔得够久也要跑() {
        // 安静期如果不更新情绪，它会一直停在很久以前的情绪
        let mut t = Throttle::default();
        t.note_event(T0);
        t.mark_run(T0);
        t.note_event(T0 + 1000); // 就 1 条
        assert_eq!(
            t.decide(T0 + 600_000, true, 8, 600),
            Decision::Run(RunReason::IntervalElapsed)
        );
    }

    #[test]
    fn 隔得不够久也没攒够就不跑() {
        let mut t = Throttle::default();
        t.note_event(T0);
        t.mark_run(T0);
        t.note_event(T0 + 1000);
        assert_eq!(
            t.decide(T0 + 120_000, true, 8, 600),
            Decision::Skip(SkipReason::TooSoon)
        );
    }

    #[test]
    fn 跑完清空待处理() {
        let mut t = Throttle::default();
        for i in 0..5 {
            t.note_event(T0 + i);
        }
        assert_eq!(t.pending(), 5);
        t.mark_run(T0);
        assert_eq!(t.pending(), 0);
        assert_eq!(
            t.decide(T0 + 600_000, true, 8, 600),
            Decision::Skip(SkipReason::NothingNew),
            "跑完没新东西就不该再跑"
        );
    }

    #[test]
    fn 重复事件不重复计数() {
        // 同一条事件被 note 两次（比如接入方重试），只算一条
        let mut t = Throttle::default();
        t.note_event(T0);
        t.note_event(T0); // 同一个时间 = 同一条
        assert_eq!(t.pending(), 1);
    }

    #[test]
    fn 水位线前进() {
        let mut t = Throttle::default();
        t.note_event(T0);
        t.note_event(T0 + 500);
        assert_eq!(t.watermark_ms(), T0 + 500);
        // 更早的事件不会让水位倒退
        t.note_event(T0 - 100);
        assert_eq!(t.watermark_ms(), T0 + 500);
    }
}
