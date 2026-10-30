//! 知识库：外部资料的存储与检索。
//!
//! # 为什么它和记忆是两套东西
//!
//! 记忆（`event` / `journal` / `fact`）和知识库在**四个语义维度**上都不同：
//!
//! | 维度 | 记忆 | 知识库 |
//! | --- | --- | --- |
//! | 时效 | 半衰期 30 天衰减 | **不衰减**：三年前写的设定和三年前一样真 |
//! | 失效 | `expires_ms` / `superseded_by` / 回滚 | 只有版本覆盖，没有「忘了它」这回事 |
//! | 权重 | importance × 衰减 × 强化 × 渠道可信度 | importance 概念不成立 |
//! | 来源 | 三渠道，可信度不同 | 单一来源：人喂进去的资料 |
//!
//! 混进 `fact` 表（它已经有 `vec` 列）的后果是具体的：资料会被衰减悄悄淡掉，
//! 回滚「忘掉某时刻之后的记忆」会把资料一起抹掉。两者都不是用户的意思。
//!
//! 设计依据与实测数据见 `docs/知识库接入方案.md`。

pub mod backfill;
pub mod chunk;
pub mod embed;
pub mod rank;

use crate::{Error, Result};
use rusqlite::Connection;

/// 允许的嵌入维度范围。超出就是配置错了，启动期该失败。
pub const DIM_MIN: usize = 32;
pub const DIM_MAX: usize = 8192;

/// 一个知识库。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Kb {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// 一篇文档。
#[derive(Debug, Clone, serde::Serialize)]
pub struct KbDoc {
    pub id: String,
    pub kb_id: String,
    pub title: Option<String>,
    pub source: Option<String>,
    pub n_chunks: i64,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// 嵌入状态。**必须是显式状态机**：没有 `Failed` 的话，
/// 一条永远嵌不进去的条目会被每轮回填反复重试，把每批都拖成全表扫描。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedState {
    Pending,
    Done,
    Failed,
    Skipped,
}

impl EmbedState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "done" => Self::Done,
            "failed" => Self::Failed,
            "skipped" => Self::Skipped,
            _ => Self::Pending,
        }
    }
}

/// 建一本知识库。同 id 已存在时更新名字/描述（幂等）。
pub fn create_kb(c: &Connection, id: &str, name: &str, desc: Option<&str>, now: i64) -> Result<Kb> {
    let id = id.trim();
    if id.is_empty() {
        return Err(Error::BadRequest("知识库 id 不能为空".into()));
    }
    if name.trim().is_empty() {
        return Err(Error::BadRequest("知识库 name 不能为空".into()));
    }
    c.execute(
        "INSERT INTO kb(id, name, description, created_ms, updated_ms)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name,
                                       description = excluded.description,
                                       updated_ms = excluded.updated_ms",
        rusqlite::params![id, name.trim(), desc, now],
    )?;
    get_kb(c, id)
}

/// 取一本知识库。
pub fn get_kb(c: &Connection, id: &str) -> Result<Kb> {
    c.query_row(
        "SELECT id, name, description, created_ms, updated_ms FROM kb WHERE id = ?1",
        rusqlite::params![id],
        |r| {
            Ok(Kb {
                id: r.get(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                created_ms: r.get(3)?,
                updated_ms: r.get(4)?,
            })
        },
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Error::NotFound(format!("知识库 {id}")),
        other => other.into(),
    })
}

/// 列出所有知识库。
pub fn list_kbs(c: &Connection) -> Result<Vec<Kb>> {
    let mut stmt = c.prepare(
        "SELECT id, name, description, created_ms, updated_ms FROM kb ORDER BY created_ms",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Kb {
            id: r.get(0)?,
            name: r.get(1)?,
            description: r.get(2)?,
            created_ms: r.get(3)?,
            updated_ms: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// 某本库下的文档列表。
pub fn list_docs(c: &Connection, kb_id: &str) -> Result<Vec<KbDoc>> {
    let mut stmt = c.prepare(
        "SELECT id, kb_id, title, source, n_chunks, created_ms, updated_ms
         FROM kb_doc WHERE kb_id = ?1 AND deleted_by IS NULL
         ORDER BY created_ms DESC",
    )?;
    let rows = stmt.query_map(rusqlite::params![kb_id], |r| {
        Ok(KbDoc {
            id: r.get(0)?,
            kb_id: r.get(1)?,
            title: r.get(2)?,
            source: r.get(3)?,
            n_chunks: r.get(4)?,
            created_ms: r.get(5)?,
            updated_ms: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// 统计各嵌入状态的条目数：`(pending, done, failed, skipped)`。
///
/// **只统计未被删除的文档**。这个口径很重要：被删文档的块还留在表里
/// （逻辑删除），把它们算进来会让面板显示「已嵌入 10 块」而实际只有 5 块可用 ——
/// 运维会以为自己导了两份，或者以为删除没生效。
pub fn embed_counts(c: &Connection) -> Result<(i64, i64, i64, i64)> {
    let mut out = [0i64; 4];
    let mut stmt = c.prepare(
        "SELECT ch.embed_state, COUNT(*)
         FROM kb_chunk ch
         JOIN kb_doc d ON d.id = ch.doc_id
         WHERE d.deleted_by IS NULL
         GROUP BY ch.embed_state",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (state, n) = row?;
        match EmbedState::parse(&state) {
            EmbedState::Pending => out[0] = n,
            EmbedState::Done => out[1] = n,
            EmbedState::Failed => out[2] = n,
            EmbedState::Skipped => out[3] = n,
        }
    }
    Ok((out[0], out[1], out[2], out[3]))
}

// ------------------------------------------------------------------ 导入

/// 导入结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportResult {
    pub doc_id: String,
    pub n_chunks: usize,
    /// 同一份内容之前已经导过（`content_hash` 命中），没有再插一遍。
    pub duplicate: bool,
}

/// 内容的稳定指纹。用于导入幂等：同一份资料重复导入不会产生第二篇。
pub fn content_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    // 归一化行尾：Windows 粘贴过来的 CRLF 与 LF 是同一份内容，
    // 不归一化会出现「明明一样却被当成两篇」。
    for line in content.replace("\r\n", "\n").lines() {
        h.update(line.trim_end().as_bytes());
        h.update(b"\n");
    }
    let d = h.finalize();
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// 导入一篇文档（幂等）。
///
/// **导入是同步的，但嵌入是异步的**：导入要立刻可用（用户要看到「导成功了」），
/// 嵌入要调外部服务、可能慢可能挂，不能让它阻塞导入。
/// 所以新块的状态是 `pending`，由后台任务回填。
/// 代价是：刚导入的文档几秒内只能被字面命中 —— 这个行为要写进对接文档。
pub fn import_doc(
    c: &rusqlite::Connection,
    kb_id: &str,
    title: Option<&str>,
    source: Option<&str>,
    content: &str,
    chunk_opts: &chunk::Opts,
    now: i64,
) -> Result<ImportResult> {
    // 库必须存在：否则文档会挂在一个不存在的 kb 上，检索永远查不到，
    // 而导入接口还返回成功 —— 这是最容易被忽略的一种静默错误。
    get_kb(c, kb_id)?;

    let hash = content_hash(content);
    if let Ok((id, n)) = c.query_row(
        "SELECT id, n_chunks FROM kb_doc
         WHERE kb_id = ?1 AND content_hash = ?2 AND deleted_by IS NULL",
        rusqlite::params![kb_id, hash],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    ) {
        return Ok(ImportResult {
            doc_id: id,
            n_chunks: n as usize,
            duplicate: true,
        });
    }

    let chunks = chunk::split(content, chunk_opts);
    if chunks.is_empty() {
        return Err(Error::BadRequest("内容为空，没有可导入的文本".into()));
    }

    let doc_id = format!("kd-{now}-{}", short_rand());
    c.execute(
        "INSERT INTO kb_doc(id, kb_id, title, source, content_hash, content,
                            n_chunks, created_ms, updated_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        rusqlite::params![
            doc_id,
            kb_id,
            title,
            source,
            hash,
            content,
            chunks.len() as i64,
            now
        ],
    )?;

    let mut stmt = c.prepare(
        "INSERT INTO kb_chunk(id, doc_id, kb_id, ord, heading, text,
                              embed_state, embed_tries, created_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 0, ?7)",
    )?;
    for ch in &chunks {
        stmt.execute(rusqlite::params![
            format!("kc-{now}-{}-{}", short_rand(), ch.ord),
            doc_id,
            kb_id,
            ch.ord as i64,
            ch.heading,
            ch.text,
            now
        ])?;
    }
    drop(stmt);

    Ok(ImportResult {
        doc_id,
        n_chunks: chunks.len(),
        duplicate: false,
    })
}

/// 取可参与检索的候选块。
///
/// `kb_id = None` 表示所有库。
///
/// **只取 `deleted_by IS NULL` 的文档下的块**。这一步在 SQL 里做，
/// 不能取回来再筛 —— 「先过滤后召回」是这个项目的既定语义。
pub fn load_candidates(
    c: &rusqlite::Connection,
    kb_id: Option<&str>,
) -> Result<Vec<rank::Candidate>> {
    let sql = "SELECT ch.id, ch.doc_id, ch.ord, ch.heading, ch.text
               FROM kb_chunk ch
               JOIN kb_doc d ON d.id = ch.doc_id
               WHERE d.deleted_by IS NULL";
    let map = |r: &rusqlite::Row| -> rusqlite::Result<rank::Candidate> {
        Ok(rank::Candidate {
            chunk_id: r.get(0)?,
            doc_id: r.get(1)?,
            ord: r.get(2)?,
            heading: r.get(3)?,
            text: r.get(4)?,
        })
    };
    match kb_id {
        Some(id) => {
            let mut stmt = c.prepare(&format!(
                "{sql} AND ch.kb_id = ?1 ORDER BY ch.doc_id, ch.ord"
            ))?;
            let rows = stmt.query_map(rusqlite::params![id], map)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        }
        None => {
            let mut stmt = c.prepare(&format!("{sql} ORDER BY ch.doc_id, ch.ord"))?;
            let rows = stmt.query_map([], map)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        }
    }
}

/// 取已嵌入块的向量：`chunk_id -> Vec<f32>`。
///
/// `want_embedder` / `want_dim` 传当前配置：**不匹配的块直接剔除，绝不「先比再说」**。
/// 不同 embedder 产生的同维向量形状完全一样，拿来比较会得到垃圾分数且零报错。
pub fn load_vectors(
    c: &rusqlite::Connection,
    kb_id: Option<&str>,
    want_embedder: &str,
    want_dim: usize,
) -> Result<Vec<(String, Vec<f32>)>> {
    let sql = "SELECT ch.id, ch.vec
               FROM kb_chunk ch
               JOIN kb_doc d ON d.id = ch.doc_id
               WHERE d.deleted_by IS NULL
                 AND ch.embed_state = 'done'
                 AND ch.vec IS NOT NULL
                 AND ch.vec_embedder = ?1
                 AND ch.vec_dim = ?2";
    let dim = want_dim as i64;
    let map = |r: &rusqlite::Row| -> rusqlite::Result<(String, Vec<f32>)> {
        let id: String = r.get(0)?;
        let blob: Vec<u8> = r.get(1)?;
        Ok((id, blob_to_f32(&blob)))
    };
    match kb_id {
        Some(id) => {
            let mut stmt = c.prepare(&format!("{sql} AND ch.kb_id = ?3"))?;
            let rows = stmt.query_map(rusqlite::params![want_embedder, dim, id], map)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        }
        None => {
            let mut stmt = c.prepare(sql)?;
            let rows = stmt.query_map(rusqlite::params![want_embedder, dim], map)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        }
    }
}

/// f32 切片 → 小端字节。存 BLOB 而不是 JSON 数组：
/// 1024 维用 JSON 是 8 KB 文本，小端字节只有 4 KB。
pub fn f32_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// 小端字节 → f32 切片。长度不是 4 的倍数时返回空（坏数据不该 panic）。
pub fn blob_to_f32(b: &[u8]) -> Vec<f32> {
    if b.len() % 4 != 0 {
        return vec![];
    }
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// 余弦相似度。两边都应是 L2 归一化的（Ollama 保证），
/// 但仍然做除法：万一存进来的是未归一化向量，不除会得到 >1 的分数。
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let (x, y) = (*x as f64, *y as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn short_rand() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{n:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn db() -> (tempfile::TempDir, std::sync::Arc<Db>) {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(d.path().join("t.db")).unwrap();
        (d, db)
    }

    #[test]
    fn 导入是幂等的() {
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "mc-lore", "MC 设定", None, 1000)?;
            let a = import_doc(
                c,
                "mc-lore",
                Some("指南"),
                Some("paste"),
                "# 一\n内容甲",
                &chunk::Opts::default(),
                2000,
            )?;
            assert!(!a.duplicate);
            assert!(a.n_chunks >= 1);
            let b = import_doc(
                c,
                "mc-lore",
                Some("指南"),
                Some("paste"),
                "# 一\n内容甲",
                &chunk::Opts::default(),
                3000,
            )?;
            assert!(b.duplicate, "同一内容重复导入必须幂等");
            assert_eq!(a.doc_id, b.doc_id);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 行尾差异不算不同内容() {
        // Windows 粘贴的 \r\n 与 \n 是同一份内容，不归一化会变成两篇
        assert_eq!(content_hash("甲乙\n丙丁"), content_hash("甲乙\r\n丙丁"));
    }

    #[test]
    fn 导入不存在的库会报错() {
        // 否则文档会挂在空库上、检索永远查不到，而导入接口还返回成功
        let (_g, db) = db();
        let e = db.with_write(|c| {
            import_doc(
                c,
                "没有这个库",
                None,
                None,
                "内容",
                &chunk::Opts::default(),
                1,
            )
        });
        assert!(matches!(e, Err(Error::NotFound(_))), "应报 NotFound");
    }

    #[test]
    fn 空内容被拒绝() {
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            let e = import_doc(c, "k", None, None, "   \n\n  ", &chunk::Opts::default(), 2);
            assert!(matches!(e, Err(Error::BadRequest(_))));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 删除文档后其块不再进候选() {
        // 「先过滤后召回」：删掉的文档不该还能被检索到
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            let r = import_doc(
                c,
                "k",
                None,
                None,
                "蜘蛛线在矿洞深处",
                &chunk::Opts::default(),
                2,
            )?;
            assert_eq!(load_candidates(c, Some("k"))?.len(), r.n_chunks);
            c.execute(
                "UPDATE kb_doc SET deleted_by = 9 WHERE id = ?1",
                rusqlite::params![r.doc_id],
            )?;
            assert_eq!(
                load_candidates(c, Some("k"))?.len(),
                0,
                "逻辑删除的文档其块必须不再进候选"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 维度或嵌入器不匹配的向量被剔除() {
        // 不同 embedder 的同维向量形状一样，拿来比较是垃圾且零报错。
        // 这里断言它们**根本不进候选**。
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            let r = import_doc(c, "k", None, None, "内容甲乙丙", &chunk::Opts::default(), 2)?;
            let ids: Vec<String> = c
                .prepare("SELECT id FROM kb_chunk WHERE doc_id = ?1")?
                .query_map(rusqlite::params![r.doc_id], |x| x.get(0))?
                .filter_map(std::result::Result::ok)
                .collect();
            let v = f32_to_blob(&[1.0, 0.0, 0.0]);
            c.execute(
                "UPDATE kb_chunk SET vec = ?1, vec_dim = 3, vec_embedder = 'other-model',
                                    embed_state = 'done' WHERE id = ?2",
                rusqlite::params![v, ids[0]],
            )?;

            // 用不匹配的 embedder 取 → 空
            assert!(load_vectors(c, Some("k"), "want-model", 3)?.is_empty());
            // 用不匹配的维度取 → 空
            assert!(load_vectors(c, Some("k"), "other-model", 9)?.is_empty());
            // 都对上 → 才拿得到
            assert_eq!(load_vectors(c, Some("k"), "other-model", 3)?.len(), 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 未嵌入的块不进向量候选() {
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            import_doc(c, "k", None, None, "内容", &chunk::Opts::default(), 2)?;
            // 刚导入是 pending，即使手工塞了向量也不该被取（状态优先）
            c.execute(
                "UPDATE kb_chunk SET vec = ?1, vec_dim = 2, vec_embedder = 'm'",
                rusqlite::params![f32_to_blob(&[1.0, 0.0])],
            )?;
            assert!(
                load_vectors(c, None, "m", 2)?.is_empty(),
                "pending 不该进候选"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 余弦相似度正确() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-9);
        assert!((cosine(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-9);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-9);
        // 维度不同 → 0，不是 panic 也不是错算
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
        // 未归一化的向量也要算对（王在内积/模长）
        assert!((cosine(&[2.0, 0.0], &[5.0, 0.0]) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn 向量字节往返() {
        let v = vec![0.5f32, -1.25, 3.0];
        let b = f32_to_blob(&v);
        assert_eq!(b.len(), 12);
        assert_eq!(blob_to_f32(&b), v);
        // 坏数据返回空而不是 panic
        assert!(blob_to_f32(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn 能取回文档原文与块明细() {
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            let src = "# 材料\n蜘蛛线做鱼竿。\n\n# 食物\n烤鱼能吃。";
            let r = import_doc(
                c,
                "k",
                Some("手册"),
                Some("t.md"),
                src,
                &chunk::Opts::default(),
                2,
            )?;
            let d = get_doc(c, &r.doc_id)?;
            assert_eq!(d.title.as_deref(), Some("手册"));
            assert_eq!(d.content, src, "原文必须能原样取回");
            assert!(!d.content_reassembled, "有原文时不该标记为重建");
            assert_eq!(d.chunks.len(), r.n_chunks);
            assert_eq!(d.chunks[0].ord, 0);
            assert!(d.chunks[0].text.contains("蜘蛛线"));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 老库没存原文时从块拼回来() {
        // schema 9 的老文档 content 为 NULL。不拼回来，面板上就是一片空白，
        // 用户会以为文档内容丢了。
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            let r = import_doc(
                c,
                "k",
                None,
                None,
                "# 甲\n内容一",
                &chunk::Opts::default(),
                2,
            )?;
            // 模拟老库：把 content 抹掉
            c.execute(
                "UPDATE kb_doc SET content = NULL WHERE id = ?1",
                rusqlite::params![r.doc_id],
            )?;
            let d = get_doc(c, &r.doc_id)?;
            assert!(d.content_reassembled, "应标记为重建");
            assert!(
                d.content.contains("内容一"),
                "应能把内容拼回来：{:?}",
                d.content
            );
            assert!(d.content.contains("甲"), "标题也要拼回来：{:?}", d.content);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 改标题不动块() {
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            let r = import_doc(
                c,
                "k",
                Some("旧名"),
                None,
                "# 甲\n内容一",
                &chunk::Opts::default(),
                2,
            )?;
            let before: Vec<String> = c
                .prepare("SELECT id FROM kb_chunk WHERE doc_id = ?1 ORDER BY ord")?
                .query_map(rusqlite::params![r.doc_id], |x| x.get(0))?
                .filter_map(std::result::Result::ok)
                .collect();
            let (n, rechunked) = update_doc(
                c,
                &r.doc_id,
                Some(Some("新名")),
                None,
                &chunk::Opts::default(),
                3,
            )?;
            assert_eq!(n, r.n_chunks);
            assert!(!rechunked, "只改标题不该重切");
            let d = get_doc(c, &r.doc_id)?;
            assert_eq!(d.title.as_deref(), Some("新名"));
            let after: Vec<String> = c
                .prepare("SELECT id FROM kb_chunk WHERE doc_id = ?1 ORDER BY ord")?
                .query_map(rusqlite::params![r.doc_id], |x| x.get(0))?
                .filter_map(std::result::Result::ok)
                .collect();
            assert_eq!(before, after, "只改标题不该重建块");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 改原文会重新切块并置为待嵌入() {
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            let r = import_doc(c, "k", None, None, "# 甲\n内容一", &chunk::Opts::default(), 2)?;
            // 先假装已嵌入
            c.execute(
                "UPDATE kb_chunk SET embed_state='done', vec_dim=3, vec_embedder='m' WHERE doc_id=?1",
                rusqlite::params![r.doc_id],
            )?;

            let new_src = "# 乙\n新的内容二\n\n# 丙\n还有更多";
            let (n, rechunked) =
                update_doc(c, &r.doc_id, None, Some(new_src), &chunk::Opts::default(), 5)?;
            assert_eq!(n, 2, "两个小节应切成两块");
            assert!(rechunked, "改了内容必须报告真的重切了");

            let d = get_doc(c, &r.doc_id)?;
            assert_eq!(d.content, new_src, "原文应被更新");
            assert_eq!(d.n_chunks, 2);
            assert!(
                d.chunks.iter().all(|x| x.embed_state == "pending"),
                "新块必须重新嵌入：{:?}",
                d.chunks.iter().map(|x| &x.embed_state).collect::<Vec<_>>()
            );
            // 旧块应被物理删除，不是留着占空间。
            // 注意：标题在 `heading` 字段、不在 `text` 里（§4.3 分开存），
            // 所以两处都要查。
            let heads: Vec<&str> = d.chunks.iter().filter_map(|x| x.heading.as_deref()).collect();
            assert_eq!(heads, vec!["乙", "丙"], "新块应带新标题");
            assert!(d.chunks.iter().all(|x| !x.text.contains("内容一")), "旧正文不该还在");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 改成与另一篇相同会被拒绝() {
        // 两篇内容一样的文档没有意义；撞唯一索引的报错也看不懂
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            let a = import_doc(
                c,
                "k",
                None,
                None,
                "# 甲\n内容一",
                &chunk::Opts::default(),
                2,
            )?;
            import_doc(
                c,
                "k",
                None,
                None,
                "# 乙\n内容二",
                &chunk::Opts::default(),
                3,
            )?;
            let e = update_doc(
                c,
                &a.doc_id,
                None,
                Some("# 乙\n内容二"),
                &chunk::Opts::default(),
                4,
            );
            assert!(matches!(e, Err(Error::BadRequest(_))), "应报可读的错误");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 内容没变时不白重切() {
        // 重切会让所有块重新嵌入（几十秒的活），内容没变就不该付这个代价
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            let src = "# 甲\n内容一";
            let r = import_doc(c, "k", None, None, src, &chunk::Opts::default(), 2)?;
            c.execute(
                "UPDATE kb_chunk SET embed_state='done' WHERE doc_id=?1",
                rusqlite::params![r.doc_id],
            )?;
            update_doc(
                c,
                &r.doc_id,
                Some(Some("新名")),
                Some(src),
                &chunk::Opts::default(),
                3,
            )?;
            let d = get_doc(c, &r.doc_id)?;
            assert!(
                d.chunks.iter().all(|x| x.embed_state == "done"),
                "内容没变不该把块重置成 pending"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 空内容不能保存() {
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            let r = import_doc(c, "k", None, None, "# 甲\n内容", &chunk::Opts::default(), 2)?;
            let e = update_doc(
                c,
                &r.doc_id,
                None,
                Some("   \n\n "),
                &chunk::Opts::default(),
                3,
            );
            assert!(matches!(e, Err(Error::BadRequest(_))));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 嵌入计数不含已删文档() {
        // 被删文档的块还在表里（逻辑删除），算进来会让面板显示
        // 「已嵌入 10 块」而实际只有 5 块可用 —— 运维会以为删除没生效。
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            let a = import_doc(
                c,
                "k",
                None,
                None,
                "# 甲\n内容一",
                &chunk::Opts::default(),
                2,
            )?;
            let b = import_doc(
                c,
                "k",
                None,
                None,
                "# 乙\n内容二",
                &chunk::Opts::default(),
                3,
            )?;
            let before = embed_counts(c)?;
            assert_eq!(before.0 + before.1 + before.2, 2, "两篇各一块");

            // 删掉一篇
            c.execute(
                "UPDATE kb_doc SET deleted_by = 9 WHERE id = ?1",
                rusqlite::params![a.doc_id],
            )?;
            let after = embed_counts(c)?;
            assert_eq!(after.0 + after.1 + after.2, 1, "删掉一篇后计数应只剩 1 块");
            let _ = b;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 嵌入计数按状态归类() {
        let (_g, db) = db();
        db.with_write(|c| {
            create_kb(c, "k", "K", None, 1)?;
            import_doc(
                c,
                "k",
                None,
                None,
                "甲乙丙丁戊",
                &chunk::Opts { max_chars: 2 },
                2,
            )?;
            let (p, d, f, s) = embed_counts(c)?;
            assert!(p > 0 && d == 0 && f == 0 && s == 0, "刚导入应全是 pending");
            c.execute("UPDATE kb_chunk SET embed_state='done' WHERE rowid=1", [])?;
            c.execute("UPDATE kb_chunk SET embed_state='failed' WHERE rowid=2", [])?;
            let (p2, d2, f2, _) = embed_counts(c)?;
            assert_eq!((d2, f2), (1, 1));
            assert_eq!(p2, p - 2);
            Ok(())
        })
        .unwrap();
    }
}

// ------------------------------------------------------------------ 查看与编辑

/// 一篇文档的完整内容（含块明细）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DocDetail {
    pub id: String,
    pub kb_id: String,
    pub title: Option<String>,
    pub source: Option<String>,
    /// 原文。老库（schema 9）没有 content 列时**从块拼回来**。
    pub content: String,
    /// 原文是不是从块重建的（即库里没存过）。
    ///
    /// 面板要据此提示「这份是重建的，格式可能有细微差异」——
    /// 否则用户改了重建出来的内容再保存，会以为自己在改原文。
    pub content_reassembled: bool,
    pub n_chunks: i64,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub chunks: Vec<ChunkRow>,
}

/// 块的展示信息（不含向量本体 —— 那东西在 JSON 里既没用又巨大）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChunkRow {
    pub id: String,
    pub ord: i64,
    pub heading: Option<String>,
    pub text: String,
    pub embed_state: String,
    pub embed_tries: i64,
    pub embed_err: Option<String>,
    pub vec_dim: Option<i64>,
}

/// 从块拼回原文。
///
/// 给老库（schema 9 及以前，`content` 为 NULL）用。拼法是
/// `# 标题\n正文`，与切分器的输入格式一致，所以再切一次得到的块
/// 与原块基本相同（除非原文档里有连续的多个空行）。
pub fn reassemble(chunks: &[ChunkRow]) -> String {
    let mut out = String::new();
    for c in chunks {
        if let Some(h) = c.heading.as_deref().filter(|h| !h.trim().is_empty()) {
            out.push_str("# ");
            out.push_str(h.trim());
            out.push_str("\n\n");
        }
        out.push_str(c.text.trim());
        out.push_str("\n\n");
    }
    out.trim_end().to_string()
}

/// 取一篇文档的完整内容 + 块明细。
pub fn get_doc(c: &Connection, doc_id: &str) -> Result<DocDetail> {
    let (id, kb_id, title, source, stored, n_chunks, created, updated) = c
        .query_row(
            "SELECT id, kb_id, title, source, content, n_chunks, created_ms, updated_ms
             FROM kb_doc WHERE id = ?1 AND deleted_by IS NULL",
            rusqlite::params![doc_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                ))
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Error::NotFound(format!("文档 {doc_id}")),
            other => other.into(),
        })?;

    let mut stmt = c.prepare(
        "SELECT id, ord, heading, text, embed_state, embed_tries, embed_err, vec_dim
         FROM kb_chunk WHERE doc_id = ?1 ORDER BY ord",
    )?;
    let chunks: Vec<ChunkRow> = stmt
        .query_map(rusqlite::params![doc_id], |r| {
            Ok(ChunkRow {
                id: r.get(0)?,
                ord: r.get(1)?,
                heading: r.get(2)?,
                text: r.get(3)?,
                embed_state: r.get(4)?,
                embed_tries: r.get(5)?,
                embed_err: r.get(6)?,
                vec_dim: r.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let (content, reassembled) = match stored {
        Some(s) => (s, false),
        None => (reassemble(&chunks), true),
    };

    Ok(DocDetail {
        id,
        kb_id,
        title,
        source,
        content,
        content_reassembled: reassembled,
        n_chunks,
        created_ms: created,
        updated_ms: updated,
        chunks,
    })
}

/// 更新一篇文档：标题 / 原文。
///
/// 原文变了就**重新切块并重新嵌入**。旧块是**物理删除**的：
/// 它们是从旧原文派生出来的数据，不是用户资产，留着只会占空间，
/// 而且它们的向量对应的是已经不存在的文本。
///
/// 返回 `(块数, 是否真的重新切了块)`。
///
/// 第二个值必须如实反映**实际发生了什么**，而不是「调用方有没有传内容」。
/// 面板拿它决定提示文案；把它写成「传了 content 就是 true」会出现
/// 「只改了标题，却告诉用户已重新切块」这种假报告 —— 用户会等着
/// 嵌入重新跑完，而其实什么都没重跑。
pub fn update_doc(
    c: &Connection,
    doc_id: &str,
    title: Option<Option<&str>>,
    new_content: Option<&str>,
    chunk_opts: &chunk::Opts,
    now: i64,
) -> Result<(usize, bool)> {
    // 文档必须存在且未被删
    let (kb_id, old_hash): (String, String) = c
        .query_row(
            "SELECT kb_id, content_hash FROM kb_doc
             WHERE id = ?1 AND deleted_by IS NULL",
            rusqlite::params![doc_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Error::NotFound(format!("文档 {doc_id}")),
            other => other.into(),
        })?;

    // 只改标题：不动块
    let Some(content) = new_content else {
        if let Some(t) = title {
            c.execute(
                "UPDATE kb_doc SET title = ?1, updated_ms = ?2 WHERE id = ?3",
                rusqlite::params![t, now, doc_id],
            )?;
        }
        let n: i64 = c.query_row(
            "SELECT n_chunks FROM kb_doc WHERE id = ?1",
            rusqlite::params![doc_id],
            |r| r.get(0),
        )?;
        return Ok((n as usize, false));
    };

    let content = content.trim();
    if content.is_empty() {
        return Err(Error::BadRequest("内容不能为空".into()));
    }

    let hash = content_hash(content);
    // 内容没变就只更新标题，不白重切一遍（重切会让所有块重新嵌入，
    // 那是几十秒的活，只因为改了标题不值得）
    if hash == old_hash {
        if let Some(t) = title {
            c.execute(
                "UPDATE kb_doc SET title = ?1, updated_ms = ?2 WHERE id = ?3",
                rusqlite::params![t, now, doc_id],
            )?;
        }
        let n: i64 = c.query_row(
            "SELECT n_chunks FROM kb_doc WHERE id = ?1",
            rusqlite::params![doc_id],
            |r| r.get(0),
        )?;
        return Ok((n as usize, false));
    }

    // 改完之后内容与同库另一篇撞了：明确报错，而不是撞唯一索引给出难懂的 SQL 错误
    let clash: Option<String> = c
        .query_row(
            "SELECT id FROM kb_doc
             WHERE kb_id = ?1 AND content_hash = ?2 AND deleted_by IS NULL AND id <> ?3",
            rusqlite::params![kb_id, hash, doc_id],
            |r| r.get(0),
        )
        .ok();
    if let Some(other) = clash {
        return Err(Error::BadRequest(format!(
            "改完的内容与库里另一篇文档完全相同（{other}）。\
             两篇内容一样的文档没有意义 —— 要么改得不一样，要么把那一篇删掉。"
        )));
    }

    let chunks = chunk::split(content, chunk_opts);
    if chunks.is_empty() {
        return Err(Error::BadRequest("内容为空，没有可切分的文本".into()));
    }

    // 旧块物理删除
    c.execute(
        "DELETE FROM kb_chunk WHERE doc_id = ?1",
        rusqlite::params![doc_id],
    )?;

    c.execute(
        "UPDATE kb_doc SET content = ?1, content_hash = ?2, n_chunks = ?3, updated_ms = ?4
         WHERE id = ?5",
        rusqlite::params![content, hash, chunks.len() as i64, now, doc_id],
    )?;
    if let Some(t) = title {
        c.execute(
            "UPDATE kb_doc SET title = ?1 WHERE id = ?2",
            rusqlite::params![t, doc_id],
        )?;
    }

    let mut stmt = c.prepare(
        "INSERT INTO kb_chunk(id, doc_id, kb_id, ord, heading, text,
                              embed_state, embed_tries, created_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 0, ?7)",
    )?;
    for ch in &chunks {
        stmt.execute(rusqlite::params![
            format!("kc-{now}-{}-{}", short_rand(), ch.ord),
            doc_id,
            kb_id,
            ch.ord as i64,
            ch.heading,
            ch.text,
            now
        ])?;
    }
    drop(stmt);

    Ok((chunks.len(), true))
}
