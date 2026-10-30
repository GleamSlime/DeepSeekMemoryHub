//! 情绪系统。PAD 三维模型 + 中文标签。
//!
//! # 为什么是 PAD 而不是一个「情绪枚举」
//!
//! 一个枚举（开心/难过/生气）没法回答「有多开心」和「开心同时在紧张」。
//! PAD 用三个连续维度描述：
//!
//! - **valence（愉悦度）**：好受 ←→ 难受
//! - **arousal（唤醒度）**：激动 ←→ 平静
//! - **dominance（支配感）**：掌控 ←→ 被支配
//!
//! 三者组合成 8 个象限，每个象限有中文名字。这比一个枚举表达力强得多，
//! 而且「淡出」天然就是数值朝 0 收敛。
//!
//! # 两个刻意的设计
//!
//! 1. **阈值 0.15**：低于它就算「平静」。没有这个死区，
//!    微小的数值抖动会让标签在两个相邻象限之间反复横跳。
//! 2. **标签用中文**：一个它说不出口的标签不是标签，是调试字段。

use serde::{Deserialize, Serialize};

/// 情绪半衰期：30 分钟。
///
/// 取这个值的理由：一次挨打应该在「这一会儿」影响它说话，
/// 但不该让它三天后还记仇。原系统按秒淡 0.02（约 25 分钟到中性），同一量级。
pub const MOOD_HALF_LIFE_MS: i64 = 30 * 60 * 1000;

/// PAD 情绪状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mood {
    pub valence: f64,
    pub arousal: f64,
    pub dominance: f64,
    /// 由三元组算出的中文标签。**存下来**，这样面板显示的是
    /// 「当时它管自己叫什么」，而不是用今天的算法重算一遍。
    pub label: String,
    pub reason: String,
    pub updated_ms: i64,
}

impl Mood {
    pub fn neutral(now_ms: i64) -> Self {
        Self {
            valence: 0.0,
            arousal: 0.0,
            dominance: 0.0,
            label: "平静".into(),
            reason: String::new(),
            updated_ms: now_ms,
        }
    }

    /// PAD 八分仪 → 中文标签。
    ///
    /// 阈值 0.15 是原系统调出来的：AByte 以下算「没有倾向」。
    pub fn label_for(v: f64, a: f64, _d: f64) -> &'static str {
        const T: f64 = 0.15;
        let pv = v >= T;
        let nv = v <= -T;
        let pa = a >= T;
        let na = a <= -T;
        if !pv && !nv && !pa && !na {
            return "平静";
        }
        if pv && pa {
            return "雀跃";
        }
        if pv && na {
            return "满足";
        }
        if pv {
            return "愉快";
        }
        if nv && pa {
            return "恼火";
        }
        if nv && na {
            return "低落";
        }
        if nv {
            return "不快";
        }
        if pa {
            return "警觉";
        }
        if na {
            return "倦怠";
        }
        "平静"
    }

    pub fn relabel(&mut self) {
        self.label = Self::label_for(self.valence, self.arousal, self.dominance).to_string();
    }

    /// 朝中性淡出。每过 `elapsed_ms` 按半衰期衰减。
    ///
    /// 用指数衰减而不是线性：线性衰减在长时间不推动时会突然「归零」，
    /// 而指数永远不会真的到 0，观感上是「慢慢平静下来」。
    pub fn decay(&mut self, elapsed_ms: i64, half_life_ms: i64) {
        if elapsed_ms <= 0 || half_life_ms <= 0 {
            return;
        }
        let factor = 0.5_f64.powf(elapsed_ms as f64 / half_life_ms as f64);
        self.valence *= factor;
        self.arousal *= factor;
        self.dominance *= factor;
        // 小于死区就归零，避免无限逼近带来的浮点残留。
        for x in [&mut self.valence, &mut self.arousal, &mut self.dominance] {
            if x.abs() < 0.01 {
                *x = 0.0;
            }
        }
        self.relabel();
    }

    /// 被一个事件推动。
    ///
    /// `dv/da/dd` 是三个维度各自的增量，最终 clamp 到 `[-1, 1]`。
    pub fn push(&mut self, dv: f64, da: f64, dd: f64, reason: &str, now_ms: i64) {
        self.valence = (self.valence + dv).clamp(-1.0, 1.0);
        self.arousal = (self.arousal + da).clamp(-1.0, 1.0);
        self.dominance = (self.dominance + dd).clamp(-1.0, 1.0);
        self.reason = reason.to_string();
        self.updated_ms = now_ms;
        self.relabel();
    }
}

/// 把一条事件翻译成情绪增量。
///
/// 返回 `None` 表示这类事件不推动情绪。
///
/// 数值参考原系统的量级（挨打 → 生气 .65、被塞东西 → 开心 .60 等），
/// 但表达成 PAD 三轴：生气是「愉悦↓ 唤醒↑ 支配↑」，开心是「愉悦↑ 唤醒↑」。
pub fn delta_for(kind: &str) -> Option<(&'static str, f64, f64, f64)> {
    // (说明, Δvalence, Δarousal, Δdominance)
    Some(match kind {
        // 好事
        "task.done" | "task_done" => ("干完了一件活", 0.25, 0.15, 0.20),
        "player.together" | "player_together" => ("有人陪着", 0.30, 0.10, 0.05),
        "gift.received" | "gift" => ("收到了东西", 0.35, 0.20, 0.05),
        "discovery" => ("发现了新东西", 0.20, 0.25, 0.15),
        "promise.kept" | "promise_kept" => ("守住了承诺", 0.30, 0.10, 0.25),

        // 坏事
        "attacked" | "damage" => ("挨了打", -0.35, 0.45, -0.30),
        "task.failed" | "task_failed" => ("活没干成", -0.30, 0.15, -0.25),
        "bag.taken" | "stolen" => ("包被拿走了", -0.40, 0.35, -0.35),
        "hungry" => ("饿了", -0.20, -0.10, -0.15),
        "death" => ("死了", -0.50, 0.40, -0.45),
        "promise.broken" | "promise_broken" => ("没守住承诺", -0.35, 0.15, -0.30),

        // 中性偏动
        "player.talked" | "message" => ("有人说话", 0.05, 0.05, 0.0),
        _ => return None,
    })
}

/// 情绪的强度：三轴绝对值里最大的那个。
///
/// 为什么不取平均：情绪可以「很不快但很平静」（valence 强、arousal 弱），
/// 平均会把这种状态压成「轻微」，而实际它挺强烈的。
pub fn intensity(mood: &Mood) -> f64 {
    mood.valence
        .abs()
        .max(mood.arousal.abs())
        .max(mood.dominance.abs())
}

/// 程度词。**集中在一处**：提示词那一行（`prompt_line`）和
/// `/v1/context` 的心情段用的是同一套口径，两处各写一份必然会漂。
pub fn degree_word(intensity: f64) -> &'static str {
    if intensity >= 0.6 {
        "很"
    } else if intensity >= 0.35 {
        "有点"
    } else {
        ""
    }
}

/// 情绪对提示词的那一行。
///
/// 平静时不加任何字——省 token，也避免每轮都提醒模型「它很平静」。
pub fn prompt_line(mood: &Mood) -> Option<String> {
    if mood.label == "平静" {
        return None;
    }
    let degree = degree_word(intensity(mood));
    Some(format!("心情:{degree}{}", mood.label))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000_000_000;

    #[test]
    fn 中性状态的标签是平静() {
        let m = Mood::neutral(NOW);
        assert_eq!(m.label, "平静");
        assert_eq!(m.valence, 0.0);
    }

    #[test]
    fn 八分仪映射正确() {
        // 都在阈值内 → 平静
        assert_eq!(Mood::label_for(0.1, 0.1, 0.1), "平静");
        assert_eq!(Mood::label_for(-0.1, -0.1, -0.1), "平静");
        // 正负象限
        assert_eq!(Mood::label_for(0.5, 0.5, 0.0), "雀跃");
        assert_eq!(Mood::label_for(0.5, -0.5, 0.0), "满足");
        assert_eq!(Mood::label_for(-0.5, 0.5, 0.0), "恼火");
        assert_eq!(Mood::label_for(-0.5, -0.5, 0.0), "低落");
        // 单轴
        assert_eq!(Mood::label_for(0.5, 0.0, 0.0), "愉快");
        assert_eq!(Mood::label_for(-0.5, 0.0, 0.0), "不快");
    }

    #[test]
    fn 推动会被夹在正负一之间() {
        let mut m = Mood::neutral(NOW);
        for _ in 0..50 {
            m.push(0.5, 0.5, 0.5, "一直推", NOW);
        }
        assert!(m.valence <= 1.0 && m.arousal <= 1.0 && m.dominance <= 1.0);
        assert_eq!(m.valence, 1.0);
    }

    #[test]
    fn 衰减朝中性收敛但不越界() {
        let mut m = Mood::neutral(NOW);
        m.push(0.8, 0.6, 0.4, "高兴", NOW);
        let before = m.valence;
        m.decay(30 * 60 * 1000, 30 * 60 * 1000); // 一个半衰期
        assert!(m.valence < before);
        assert!((m.valence - before / 2.0).abs() < 1e-6, "一个半衰期应减半");
        // 长期衰减会归零但不变负
        for _ in 0..100 {
            m.decay(60 * 60 * 1000, 30 * 60 * 1000);
        }
        assert_eq!(m.valence, 0.0, "应有死区归零");
        assert_eq!(m.label, "平静");
    }

    #[test]
    fn 零或负的衰减参数是安全的() {
        let mut m = Mood::neutral(NOW);
        m.push(0.5, 0.5, 0.5, "x", NOW);
        let before = m.valence;
        m.decay(0, 1000);
        m.decay(1000, 0);
        m.decay(-5, -5);
        assert_eq!(m.valence, before, "无效参数不该改变状态");
    }

    #[test]
    fn 事件翻译成情绪增量() {
        let (why, v, _, _) = delta_for("task_done").unwrap();
        assert!(v > 0.0, "干完活应该是好事：{why}");

        let (why, v, a, _) = delta_for("attacked").unwrap();
        assert!(v < 0.0, "挨打应该是坏事：{why}");
        assert!(a > 0.0, "挨打应该让唤醒度上升");

        assert!(delta_for("完全未知的类型").is_none());
    }

    #[test]
    fn 平静时不输出提示词行() {
        let m = Mood::neutral(NOW);
        assert!(prompt_line(&m).is_none(), "平静不该花 token");
    }

    #[test]
    fn 有情绪时提示词带程度词() {
        let mut m = Mood::neutral(NOW);
        m.push(-0.7, 0.5, -0.3, "挨打", NOW);
        let line = prompt_line(&m).unwrap();
        assert!(line.starts_with("心情:"), "实得 {line}");
        assert!(
            line.contains("很") || line.contains("有点"),
            "应带程度：{line}"
        );
        assert!(line.contains(&m.label), "应包含标签：{line}");
    }

    #[test]
    fn 标签在推动后自动更新() {
        let mut m = Mood::neutral(NOW);
        assert_eq!(m.label, "平静");
        m.push(0.5, 0.5, 0.0, "好事", NOW);
        assert_ne!(m.label, "平静");
        assert_eq!(m.label, "雀跃");
    }
}
