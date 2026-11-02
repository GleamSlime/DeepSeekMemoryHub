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

const SYSTEM: &str = r#"你在帮一个角色形成对某个人的「印象」。

你会拿到：这个人和角色之间发生过的事、说过的话。

请写出**角色对这个人**的印象，一句话，40 字以内。

要求：
- 写的是**判断**，不是流水账：「嘴上爱损人，但会主动帮忙，不用跟他客气」
- 具体、有性格，不要「是个好人」「很友好」这种空话
- 只依据给的材料，**不要编造**
- 用第二人称视角写（「他」/「她」/「这个人」）
- **不要写你打算怎么对待他**，只写你怎么看他

直接输出那一句话，不要引号、不要解释。"#;

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
    let out = client
        .chat(&[ChatMessage::system(SYSTEM), ChatMessage::user(user)])
        .await?;
    let elapsed = t0.elapsed().as_millis() as i64;

    let mut imp: String = out
        .text
        .trim()
        .trim_matches('"')
        .trim_matches('「')
        .trim_matches('」')
        .to_string();
    // 去掉可能的前缀
    for p in ["印象：", "对", "他："] {
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
    use crate::generate::runner::tests_support::fresh_db;

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
