//! 嵌入回填：把 `pending` 的块嵌成向量。
//!
//! # 为什么是后台任务而不是同步嵌入
//!
//! 导入要**立刻可用**（用户要看到「导成功了」），而嵌入要调外部服务、可能慢可能挂。
//! 让嵌入阻塞导入，会让一次导文档变成一次跨网络的等待。
//! 代价是：刚导入的文档几秒内只能被字面命中 —— 这个行为写进了对接文档。
//!
//! # 为什么必须有退避
//!
//! 没有退避时，一个挂掉的嵌入服务会被每 30 秒固定重试一次，
//! 在日志里留下固定频率的错误，而且**永远不会自愈到正常节奏**。
//! 退避让「服务恢复了」这件事能被更快发现，也让日志安静下来。
//!
//! # 为什么要有 `failed` 终态
//!
//! 一条永远嵌不进去的条目（比如内容里有服务拒绝的字符）会被无限重试，
//! 把每一批都拖成一次全表扫描。重试上限到了就落 `failed`，不再自动重试。

use super::embed::Embedder;
use crate::db::Db;
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::Notify;

/// 单条最多试几次。超过就落 `failed`。
pub const MAX_TRIES: i64 = 5;

/// 一次取多少条回填。
pub const BATCH: i64 = 64;

/// 连续失败多少批之后开始退避。
const BACKOFF_AFTER: u32 = 3;

static NUDGE: OnceLock<Notify> = OnceLock::new();

fn notify() -> &'static Notify {
    NUDGE.get_or_init(Notify::new)
}

/// 催促回填立刻跑一轮。
///
/// 导入新文档后调它，避免「等下一个 30 秒」的空窗。
pub fn nudge() {
    notify().notify_one();
}

/// 启动回填任务。
pub fn spawn(db: Arc<Db>, embedder: Option<Arc<Embedder>>, interval_seconds: u64) {
    let Some(embedder) = embedder else {
        // 没有嵌入服务就没有回填这回事。**不能**把 pending 标成 failed ——
        // 那会在服务临时不可用时把数据永久标记成坏。
        return;
    };
    let interval = std::time::Duration::from_secs(interval_seconds.max(1));
    tokio::spawn(async move {
        let mut fails = 0u32;
        loop {
            // 退避：连续失败时把等待拉长，上限 10 分钟
            let wait = if fails >= BACKOFF_AFTER {
                interval * (fails - BACKOFF_AFTER + 1).min(20)
            } else {
                interval
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = notify().notified() => {}
            }

            match run_once(&db, &embedder).await {
                Ok(0) => {
                    fails = 0;
                }
                Ok(n) => {
                    fails = 0;
                    tracing::debug!("知识库回填：本次嵌入 {n} 条");
                }
                Err(e) => {
                    fails = fails.saturating_add(1);
                    // 只在状态翻转的那一次打 WARN，不是每次失败都打。
                    // 一个挂掉的服务每 30 秒一行错误，日志就失去价值了。
                    if fails == BACKOFF_AFTER || fails == 1 {
                        tracing::warn!("知识库回填失败（累计 {fails} 次）：{e}");
                    } else {
                        tracing::debug!("知识库回填失败（累计 {fails} 次）：{e}");
                    }
                }
            }
        }
    });
}

/// 跑一轮回填，返回本次成功嵌入的条数。
pub async fn run_once(db: &Arc<Db>, embedder: &Embedder) -> crate::Result<usize> {
    // 取一批待嵌入的
    let rows: Vec<(String, String)> = db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT id, COALESCE(heading || char(10), '') || text
             FROM kb_chunk
             WHERE embed_state = 'pending' AND embed_tries < ?1
             ORDER BY created_ms LIMIT ?2",
        )?;
        let v = stmt
            .query_map(rusqlite::params![MAX_TRIES, BATCH], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    })?;

    if rows.is_empty() {
        return Ok(0);
    }

    let ids: Vec<String> = rows.iter().map(|(id, _)| id.clone()).collect();
    let texts: Vec<String> = rows.iter().map(|(_, t)| t.clone()).collect();

    let vecs = match embedder.embed(&texts).await {
        Ok(v) => v,
        Err(e) => {
            // 整批失败：把这几条的 tries +1，到上限落 failed。
            // 不在这里判 failed 的话，一条永久失败的条目会一直占着批次开头。
            db.with_write(|c| {
                let mut stmt = c.prepare(
                    "UPDATE kb_chunk
                     SET embed_tries = embed_tries + 1,
                         embed_err = ?1,
                         embed_state = CASE WHEN embed_tries + 1 >= ?2 THEN 'failed'
                                            ELSE embed_state END
                     WHERE id = ?3",
                )?;
                let msg = format!("{e}");
                for id in &ids {
                    stmt.execute(rusqlite::params![msg, MAX_TRIES, id])?;
                }
                Ok(())
            })?;
            return Err(e);
        }
    };

    if vecs.len() != ids.len() {
        return Err(crate::Error::Storage(format!(
            "嵌入返回条数不符：请求 {} 条，返回 {} 条",
            ids.len(),
            vecs.len()
        )));
    }

    let model = embedder.model().to_string();
    let dim = embedder.dim();
    db.with_write(|c| {
        let mut stmt = c.prepare(
            "UPDATE kb_chunk
             SET vec = ?1, vec_dim = ?2, vec_embedder = ?3,
                 embed_state = 'done', embed_tries = 0, embed_err = NULL
             WHERE id = ?4",
        )?;
        for (id, v) in ids.iter().zip(vecs.iter()) {
            stmt.execute(rusqlite::params![
                super::f32_to_blob(v),
                dim as i64,
                model,
                id
            ])?;
        }
        Ok(())
    })?;

    Ok(ids.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::embed::{EmbedConfig, Embedder};
    use crate::kb::{chunk, create_kb, embed_counts, import_doc};

    fn db() -> (tempfile::TempDir, Arc<Db>) {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(d.path().join("t.db")).unwrap();
        (d, db)
    }

    fn embedder() -> Embedder {
        Embedder::new(EmbedConfig {
            url: "http://127.0.0.1:9/v1/embeddings".into(),
            model: "m".into(),
            dim: crate::kb::DIM_MIN,
            token: String::new(),
            batch: 8,
            timeout_seconds: 2,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn 没有待嵌入的条目时返回零() {
        let (_g, db) = db();
        assert_eq!(run_once(&db, &embedder()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn 服务不可用时累加重试次数并在上限落_failed() {
        // 这是防「永久失败的条目占着批次开头、把每批拖成全表扫描」的关键。
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            import_doc(c, "k", None, None, "内容甲乙", &chunk::Opts::default(), 2)?;
            Ok(())
        })
        .unwrap();

        let e = embedder();
        for _ in 0..MAX_TRIES {
            let r = run_once(&db, &e).await;
            assert!(r.is_err(), "服务不可达必须报错");
        }

        let (_p, _d, f, _s) = db.with_read(embed_counts).unwrap();
        assert!(f > 0, "重试到上限应落 failed，实际 failed={f}");

        // 落 failed 之后不再被取走 —— 否则它会一直重试
        let n: i64 = db
            .with_read(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM kb_chunk WHERE embed_state='pending'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(n, 0, "failed 之后不该再有 pending");
    }

    #[tokio::test]
    async fn 失败不改变已成功的条目() {
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            import_doc(c, "k", None, None, "甲乙", &chunk::Opts::default(), 2)?;
            // 手工造一条已完成
            c.execute(
                "UPDATE kb_chunk SET embed_state='done', vec_dim=3, vec_embedder='other' WHERE rowid=1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let before = db.with_read(embed_counts).unwrap();
        let _ = run_once(&db, &embedder()).await;
        let after = db.with_read(embed_counts).unwrap();
        assert_eq!(before.1, after.1, "已 done 的条目数不该因失败而变化");
    }
}
