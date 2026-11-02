//! 一次性回溯：按**历史事件与日记**为已有关系生成「印象」。
//!
//! # 为什么单独一个入口
//!
//! 印象是这次新加的字段，之前的关系都没有。靠 tick 慢慢积累要等很久
//! （一次 tick 只微调一点），而这台机器上已经躺着几百条真实互动 ——
//! 直接读一遍比等快得多，也更准（素材更全）。
//!
//! # 口径
//!
//! - **按人聚合**：把这个人在事件/对话里出现过的摘要拼起来，一次问一个模型；
//! - **一次一个人**：合着问会让模型把 A 的性格写到 B 头上；
//! - 素材上限 `MAX_MATERIAL` 条：够形成判断即可，再多只是重复；
//! - 已有印象**不覆盖**（除非显式 `overwrite`）—— 回溯是用来补空缺的，
//!   不是拿来洗掉线上已经积累的判断。

use std::sync::Arc;

use crate::db::Db;
use crate::error::{Error, Result};
use crate::generate::runner::{self, Record};
use crate::llm::{ChatMessage, Purpose};

/// 每个人的素材上限（条）。
const MAX_MATERIAL: usize = 60;

/// 提示词模板。`{persona}` 处填入**这个角色是谁**。
///
/// # 为什么必须带人设（2026-11-02 实测踩过）
///
/// 第一版提示词写的是「你在帮**一个角色**形成印象」，没有告诉模型
/// 这个角色是谁、什么性格、跟对方什么关系。结果生成出来的是
/// **旁观者的客观评价**（「她是个爱下指令、爱改参数的主人」），
/// 而不是**鲸鲸对这个人的看法** —— 用户的原话是「不符合人设」。
///
/// 差别在哪：「爱下指令」是观察，「主人又在瞎指挥，但我还是会去做」
/// 才是带态度、带性格的印象。后者才会影响回复的语气。
///
/// # 人称也改了
///
/// 第一版要求「用第二人称视角写（他/她/这个人）」—— 这是**报道口吻**。
/// 印象是「我心里怎么看他」，应该是**第一人称的心里话**：
/// 可以叫「主人」「这家伙」，可以用角色自己的说话习惯。
const SYSTEM_TMPL: &str = r#"你就是下面这个人，现在在心里琢磨「{who}」这个人。

## 你是谁
{persona}

## 称呼规矩（很重要）

- 人设里说的「主人」是**一个人**，不是所有玩家都叫主人。
  材料里那个和你关系最深、你叫「主人」的人只有一个；
  **其他人就是群友/朋友，直接叫名字**（「鸡蛋大人」「小螺」），不要也叫主人。
- 不确定时：叫名字永远不错，叫错「主人」很尴尬。

## 要求

写一句**你心里对他的印象**，40 字以内。

- 用**你自己的口吻**写 —— 想叫「主人」就叫，想损就损，带上你的脾气。
  这不是给外人看的评价，是你私下的判断。
- 写**态度**，不是观察。
  ❌「她是个爱下指令的人」（这是旁观者的观察）
  ✅「主人又在瞎指挥，可我还是会去做的」（这是你的想法）
- 只依据给的材料，**不要编造**没发生过的事。
- 不要写成你要怎么对待他，只写你怎么看他。
- 不要用「他/她/这个人」这种第三人称；用你最自然的称呼。
- **自称统一用「我」或「鲸鲸」**，不要用「人家」——
  那是回复阶段会被替换掉的词，写进印象会前后不一致。

## 语气参照

素材里的对话用的是角色平时的说话方式，**照着那个语气写**，
不要切换成书面语或旁白腔。

直接输出那一句话，不要引号、不要解释、不要「印象：」前缀。"#;

/// 没有配置人设时的退化模板。
///
/// 保留它是因为：中枢刚接入 MC 时人格表可能是空的，
/// 那时宁可给一个中性但能用的印象，也不要直接报错不生成。
const SYSTEM_NO_PERSONA: &str = r#"你在为「{who}」写一句印象。

你没有具体的人设资料，请用**第一人称**写一句你对他/她的看法，
40 字以内，要有态度（不是客观观察），只依据给的材料、不要编造。
直接输出那一句话。"#;

/// 回溯一个人。返回 `Ok(true)` 表示真的写入了。
pub async fn one(
    db: &Arc<Db>,
    cfg: &crate::config::Config,
    person_id: &str,
    overwrite: bool,
) -> Result<bool> {
    // 已有印象且不覆盖 → 跳过
    if !overwrite {
        let has: Option<String> = db.with_read(|c| {
            Ok(c.query_row(
                "SELECT impression FROM relation WHERE person_id = ?1",
                rusqlite::params![person_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten())
        })?;
        if has.filter(|s| !s.trim().is_empty()).is_some() {
            return Ok(false);
        }
    }

    // 这个角色是谁 —— 印象必须站在**它自己**的立场上写，
    // 而人设就存在 `persona` 表里（面板「人格」页编辑）。
    //
    // 取优先级最高的那条可用人格。和 `/v1/context` 用的是同一张表，
    // 但不按模型正则匹配：这是后台生成任务，跟哪个模型在跑无关，
    // 要的是「这个角色是什么样的人」。
    let persona_text = db.with_read(|c| {
        Ok(c.query_row(
            "SELECT body FROM persona WHERE enabled = 1
             ORDER BY priority DESC, created_ms DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok())
    })?;

    let (name, material) = db.with_read(|c| {
        let name: String = c
            .query_row(
                "SELECT display_name FROM person WHERE person_id = ?1",
                rusqlite::params![person_id],
                |r| r.get(0),
            )
            .map_err(|_| Error::NotFound(format!("找不到这个人：{person_id}")))?;

        // 这个人的各种叫法（场景称呼 + 观测到的名字），用来在文本里找他
        let mut names: Vec<String> = vec![name.clone()];
        let mut st =
            c.prepare("SELECT name FROM person_scene WHERE person_id = ?1 AND name IS NOT NULL")?;
        names.extend(
            st.query_map(rusqlite::params![person_id], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
        let mut st2 = c.prepare(
            "SELECT seen_name FROM person_account WHERE person_id = ?1 AND seen_name IS NOT NULL",
        )?;
        names.extend(
            st2.query_map(rusqlite::params![person_id], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
        names.retain(|n| !n.trim().is_empty());
        names.dedup();

        // 事件里提到他的
        let like_clause = names
            .iter()
            .map(|_| "summary LIKE ?")
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT summary FROM event
             WHERE deleted_by IS NULL AND ({like_clause})
             ORDER BY at_ms DESC LIMIT {MAX_MATERIAL}"
        );
        let mut st3 = c.prepare(&sql)?;
        let mut rows: Vec<String> = st3
            .query_map(
                rusqlite::params_from_iter(names.iter().map(|n| format!("%{n}%"))),
                |r| r.get::<_, String>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // 对话里他说的话
        let mut st4 = c.prepare(
            "SELECT content FROM channel_message
             WHERE deleted_by IS NULL AND sender_name = ?1
             ORDER BY at_ms DESC LIMIT 40",
        )?;
        rows.extend(
            st4.query_map(rusqlite::params![&name], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
        Ok((name, rows))
    })?;

    if material.is_empty() {
        return Err(Error::BadRequest(format!(
            "{name} 没有任何历史素材，跳过（不编造印象）"
        )));
    }

    let mut user = format!("这个人的名字：{name}\n\n## 你们之间发生过的事\n");
    for m in material.iter().take(MAX_MATERIAL) {
        let one_line: String = m.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        user.push_str(&format!(
            "- {}\n",
            one_line.chars().take(120).collect::<String>()
        ));
    }
    user.push_str("\n请写出你对他/她的印象（一句话，40 字内）。");

    let client = runner::client(cfg);
    let t0 = std::time::Instant::now();
    let sys = match persona_text
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        Some(p) => SYSTEM_TMPL.replace("{who}", &name).replace("{persona}", p),
        None => SYSTEM_NO_PERSONA.replace("{who}", &name),
    };
    let out = client
        .chat(&[ChatMessage::system(sys), ChatMessage::user(user)])
        .await?;
    let elapsed = t0.elapsed().as_millis() as i64;

    let mut imp: String = out
        .text
        .trim()
        .trim_matches('"')
        .trim_matches('「')
        .trim_matches('」')
        .to_string();
    // 去掉可能的前缀（模型偶尔还是会加）
    for p in ["印象：", "对", "他：", "她：", "我："] {
        if let Some(rest) = imp.strip_prefix(p) {
            if !rest.trim().is_empty() {
                imp = rest.trim().to_string();
            }
        }
    }
    if imp.is_empty() {
        return Err(Error::Storage("模型返回空印象".into()));
    }
    if imp.chars().count() > 60 {
        imp = imp.chars().take(60).collect::<String>() + "…";
    }

    let now = crate::db::now_ms();
    db.with_write(|tx| {
        tx.execute(
            "UPDATE relation SET impression = ?1, impression_ms = ?2 WHERE person_id = ?3",
            rusqlite::params![imp, now, person_id],
        )?;
        Ok(())
    })?;

    runner::log(
        db,
        Record {
            purpose: Purpose::Tick,
            ok: true,
            trigger: "backfill_impression",
            model: &cfg.llm.model,
            prompt_tokens: out.prompt_tokens,
            completion_tokens: out.completion_tokens,
            elapsed_ms: elapsed,
            result: Some(format!("{name}：{imp}")),
            error: None,
        },
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{SYSTEM_NO_PERSONA, SYSTEM_TMPL};
    use crate::generate::runner::tests_support::fresh_db;

    #[test]
    fn 提示词必须带人设和称呼规矩() {
        // 回归（2026-11-02 用户报「印象不符合人设」）：
        // 第一版提示词只说「你在帮一个角色形成印象」，没告诉模型**这个角色是谁**，
        // 生成出来是旁观者的客观评价，不是鲸鲸的看法。
        assert!(SYSTEM_TMPL.contains("{persona}"), "要留人设插槽");
        assert!(SYSTEM_TMPL.contains("{who}"), "要留名字插槽");
        assert!(
            SYSTEM_TMPL.contains("主人") && SYSTEM_TMPL.contains("不是所有玩家都叫主人"),
            "必须区分「主人只有一个」——实测 5 个群友全被叫成主人"
        );
        assert!(
            SYSTEM_TMPL.contains("人家"),
            "要禁掉「人家」：回复阶段会被替换成「本鱼」，写进印象会前后不一致"
        );
        assert!(
            SYSTEM_TMPL.contains("用**你自己的口吻**"),
            "必须要求第一人称口吻，不能是报道腔"
        );
    }

    #[test]
    fn 没人设时有退化模板() {
        // 人格表可能是空的（刚接入时），那时宁可给中性印象也不要报错不生成
        assert!(SYSTEM_NO_PERSONA.contains("{who}"));
        assert!(SYSTEM_NO_PERSONA.contains("第一人称"));
    }

    #[test]
    fn 没有素材时拒绝而不是编造() {
        let (db, _g) = fresh_db();
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO person(person_id, display_name, status, created_ms, ignored)
                 VALUES ('p1','某人','confirmed',1,0)",
                [],
            )?;
            tx.execute(
                "INSERT INTO relation(person_id, familiarity, trust, closeness,
                                      first_met_ms, last_met_ms)
                 VALUES ('p1',5,3,20,1,1)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.llm.base_url = "http://127.0.0.1:1/v1".into();
        cfg.llm.api_key = "k".into();
        cfg.llm.model = "m".into();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(super::one(&db, &cfg, "p1", false));
        let msg = match r {
            Ok(_) => panic!("没素材要拒绝，而不是编造"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("历史素材"), "错误要说清原因：{msg}");
    }
}
