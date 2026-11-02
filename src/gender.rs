//! 代词：按性别选「他 / 她 / TA」。
//!
//! # 为什么需要单独一个模块
//!
//! 中文里「他/她」必须选一个，而选错比不选更难受 ——
//! 既冒犯人，也会让模型对一个人的语气跑偏（对「她」和对「他」，
//! 同样一句「你别闹」分寸不同）。
//!
//! 之前没有性别字段，模型只能**随机**挑，用户的原话是
//! 「目前上下文的他完全随机的特别奇怪」。
//!
//! # 为什么未知时用「TA」
//!
//! 三选一里，「不知道」是**明确的一种状态**，不该退化成猜。
//! 「TA」在中文语境里是通用写法，读起来不别扭，
//! 而且它向模型传递了正确的信息：**这个人性别未定，别预设**。
//!
//! 用 `TA` 而不是「他/她」：后者在句子里出现会打断阅读节奏
//! （「他/她昨天说」比「TA 昨天说」更啰嗦），也不适合交给 TTS 念。

use serde::{Deserialize, Serialize};

/// 性别。存库与 API 都用这套值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Gender {
    /// 不知道 —— 代词用 TA。**这是默认值**。
    #[default]
    Unknown,
    Male,
    Female,
    /// 明确的其他/不愿说 —— 同样用 TA。
    Other,
}

impl Gender {
    /// 从库里/请求里读。认不出的一律当 `Unknown`（不报错、不猜）。
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "male" | "m" | "男" => Gender::Male,
            "female" | "f" | "女" => Gender::Female,
            "other" | "其它" | "其他" => Gender::Other,
            _ => Gender::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Gender::Unknown => "unknown",
            Gender::Male => "male",
            Gender::Female => "female",
            Gender::Other => "other",
        }
    }

    /// 中文标签（面板下拉用）。
    pub fn label(self) -> &'static str {
        match self {
            Gender::Unknown => "未知",
            Gender::Male => "男",
            Gender::Female => "女",
            Gender::Other => "其它",
        }
    }

    /// 第三人称代词。
    ///
    /// 只有 `Male`/`Female` 才给具体代词；`Unknown`/`Other` 一律 `TA`。
    pub fn pronoun(self) -> &'static str {
        match self {
            Gender::Male => "他",
            Gender::Female => "她",
            Gender::Unknown | Gender::Other => "TA",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 默认是未知() {
        assert_eq!(Gender::default(), Gender::Unknown);
        assert_eq!(Gender::Unknown.pronoun(), "TA");
    }

    #[test]
    fn 未知用_ta_不猜() {
        // 用户明确要求：「未知情况下用 TA」
        assert_eq!(Gender::Unknown.pronoun(), "TA");
        assert_eq!(Gender::Other.pronoun(), "TA", "其它也用 TA");
    }

    #[test]
    fn 只有明确性别才用他她() {
        assert_eq!(Gender::Male.pronoun(), "他");
        assert_eq!(Gender::Female.pronoun(), "她");
    }

    #[test]
    fn 解析容忍各种写法() {
        assert_eq!(Gender::parse("male"), Gender::Male);
        assert_eq!(Gender::parse("M"), Gender::Male);
        assert_eq!(Gender::parse("男"), Gender::Male);
        assert_eq!(Gender::parse("female"), Gender::Female);
        assert_eq!(Gender::parse("F "), Gender::Female);
        assert_eq!(Gender::parse("女"), Gender::Female);
        assert_eq!(Gender::parse("other"), Gender::Other);
        // 认不出的一律 unknown，不报错也不猜
        assert_eq!(Gender::parse(""), Gender::Unknown);
        assert_eq!(Gender::parse("未知"), Gender::Unknown);
        assert_eq!(Gender::parse("???"), Gender::Unknown);
    }

    #[test]
    fn 往返一致() {
        for g in [Gender::Unknown, Gender::Male, Gender::Female, Gender::Other] {
            assert_eq!(Gender::parse(g.as_str()), g);
        }
    }
}
