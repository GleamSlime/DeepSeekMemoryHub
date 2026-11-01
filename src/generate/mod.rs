//! 生成任务：日记 / 周记（定时）+ 手帐 / 情绪 / 自我认知（被动触发）。
//!
//! # 为什么搬到中枢
//!
//! 2026-11-01 之前这些全在 MC 插件里生成，中枢只存。MC 不开机 → 全停
//! （实测 `journal` 停在 10-30）。中枢才是 7×24 一直在的那个服务。
//!
//! # 省请求的三条规矩
//!
//! 1. **一次 tick 算三件事**（`Tick`）：情绪 + 手帐 + 自我认知**同一次请求**给出。
//!    分三次问不仅三倍花费，还会互相矛盾 —— 情绪说「开心」、手帐说「沮丧」。
//! 2. **攒批 + 节流**：不是每条事件都触发。攒到阈值或等到最小间隔才发一次，
//!    一次喂一批事件。见 `Throttle`。
//! 3. **定时任务错峰**：日记 04:00、周记周一 04:00，且**漏跑要补**。
//!
//! # 情绪为什么要 AI 判断
//!
//! 用户原话：「情绪过于机械化、可预测，没有不确定性，且很难体现群聊中的情绪变化」。
//! 之前的 `EmotionState` 是查表：挨打→生气 0.65、活干完→开心 0.40。
//! 结果是**同样的事永远同样的反应**，人不是这样。
//!
//! 现在由模型读「最近发生了什么」自己判断该有什么情绪、多强，
//! 并显式允许它给出「没什么情绪波动」（`label: "平静"`）——
//! 不能每次聊天都开心。

pub mod collect;
pub mod daily;
pub mod exec;
pub mod r#loop;
pub mod runner;
pub mod throttle;
pub mod tick;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// 生成任务配置（面板可改）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenConfig {
    /// 被动触发的「感知 tick」总开关。
    #[serde(default = "t")]
    pub tick_enabled: bool,
    /// 两次 tick 的最小间隔（秒）。默认 600（10 分钟）。
    ///
    /// 这是省请求的**主闸**：不管来多少事件，10 分钟内最多一次 LLM 调用。
    #[serde(default = "d_tick_interval")]
    pub tick_interval_seconds: u64,
    /// 攒够多少条事件可以提前触发（但仍然受 `tick_interval_seconds` 约束）。
    #[serde(default = "d_tick_batch")]
    pub tick_batch: usize,
    /// 低于这个重要度的事件不进 tick 的上下文（纯噪声不值得花 token）。
    #[serde(default = "d_tick_min_imp")]
    pub tick_min_importance: f64,

    /// 日记：每天几点（本地时间 `HH:MM`）。空 = 关。
    #[serde(default = "d_diary_at")]
    pub diary_at: String,
    /// 周记：星期几（1=周一…7=周日）+ 几点。空 = 关。
    #[serde(default = "d_weekly_at")]
    pub weekly_at: String,
    #[serde(default = "d_weekly_day")]
    pub weekly_day: u32,

    /// 生成出来的内容要不要立刻写库（关掉就只记 `generation_log`，便于先看效果）。
    #[serde(default = "t")]
    pub write_back: bool,
}

fn t() -> bool {
    true
}
fn d_tick_interval() -> u64 {
    600
}
fn d_tick_batch() -> usize {
    8
}
fn d_tick_min_imp() -> f64 {
    0.35
}
fn d_diary_at() -> String {
    "04:00".into()
}
fn d_weekly_at() -> String {
    "04:30".into()
}
fn d_weekly_day() -> u32 {
    1
}

impl Default for GenConfig {
    fn default() -> Self {
        Self {
            tick_enabled: true,
            tick_interval_seconds: d_tick_interval(),
            tick_batch: d_tick_batch(),
            tick_min_importance: d_tick_min_imp(),
            diary_at: d_diary_at(),
            weekly_at: d_weekly_at(),
            weekly_day: d_weekly_day(),
            write_back: true,
        }
    }
}

impl GenConfig {
    pub fn validate(&self) -> Result<()> {
        if self.tick_batch == 0 {
            return Err(crate::error::Error::Config("tick_batch 不能为 0".into()));
        }
        if self.tick_interval_seconds < 60 {
            return Err(crate::error::Error::Config(
                "tick_interval_seconds 至少 60 —— 再短就失去节流的意义".into(),
            ));
        }
        for (name, v) in [("diary_at", &self.diary_at), ("weekly_at", &self.weekly_at)] {
            if v.trim().is_empty() {
                continue;
            }
            parse_hhmm(v).ok_or_else(|| {
                crate::error::Error::Config(format!("{name} 格式要 HH:MM，收到 {v}"))
            })?;
        }
        if self.weekly_day < 1 || self.weekly_day > 7 {
            return Err(crate::error::Error::Config(
                "weekly_day 要在 1..7（1=周一）".into(),
            ));
        }
        Ok(())
    }
}

/// 解析 `HH:MM` 成 `(时, 分)`。
pub fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 时间格式() {
        assert_eq!(parse_hhmm("04:00"), Some((4, 0)));
        assert_eq!(parse_hhmm(" 23:59 "), Some((23, 59)));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("4:5"), Some((4, 5)));
        assert_eq!(parse_hhmm("0400"), None);
        assert_eq!(parse_hhmm(""), None);
    }

    #[test]
    fn 配置校验() {
        let mut c = GenConfig::default();
        assert!(c.validate().is_ok());
        c.tick_batch = 0; // 下面逐项改，属于测同一份配置的多个约束
        assert!(c.validate().is_err(), "批量为 0 就永远攒不满");
        c.tick_batch = 8;
        c.tick_interval_seconds = 10;
        assert!(c.validate().is_err(), "间隔太短等于没节流");
        c.tick_interval_seconds = 600;
        c.weekly_day = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn 空时间表示关闭且不报错() {
        let c = GenConfig {
            diary_at: String::new(),
            weekly_at: "  ".into(),
            ..Default::default()
        };
        assert!(c.validate().is_ok(), "空 = 关，不该拦住启动");
    }
}
