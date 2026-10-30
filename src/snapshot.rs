//! 每日快照：可回滚的基础。
//!
//! # 为什么每天一份就够
//!
//! 用户的要求是「每天有快照可以回滚，回滚相当于忘记目标天数之后的所有记忆」。
//! 这个粒度是**用户自己定的**，不是我们偷懒——「忘掉某一天之后的记忆」
//! 本身就是一个以天为单位的操作。
//!
//! # 快照必须能回答的两个问题
//!
//! 1. **它是好的吗？** 每次生成后立刻 `PRAGMA quick_check`，
//!    并且记录 `sha256`。一个「生成了但打不开」的快照比没有快照更危险，
//!    因为它会让人以为备份是好的。
//! 2. **它是什么时候的？** `snapshot` 表记录 `taken_ms` + 行数，
//!    回滚时按时间挑目标，不靠文件名。

use std::path::{Path, PathBuf};

use crate::db::{now_ms, Db};
use crate::error::{Error, Result};

/// 快照元信息。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SnapshotInfo {
    pub id: i64,
    pub taken_ms: i64,
    pub path: String,
    pub bytes: i64,
    pub rows: i64,
    pub sha256: String,
    pub note: Option<String>,
}

/// 一天有多少毫秒。快照按「自然日」去重时用。
const DAY_MS: i64 = 86_400_000;

impl SnapshotInfo {
    /// 这份快照属于哪一天（UTC 天序号）。
    pub fn day_index(&self) -> i64 {
        self.taken_ms.div_euclid(DAY_MS)
    }
}

/// 快照仓库：一个目录 + 命名规则。
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    dir: PathBuf,
}

impl SnapshotStore {
    pub fn new(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 生成一份快照。
    ///
    /// 文件名带时间戳，天然不会覆盖；同一天多次快照都保留（用户可能手动多打几次）。
    /// 「一天一份」的去重由 [`Self::ensure_daily`] 负责，不在这一层做——
    /// 手动快照和自动快照的语义不同，混在一起会互相干扰。
    pub fn take(&self, db: &Db, note: Option<&str>) -> Result<SnapshotInfo> {
        let taken = now_ms();
        let name = format!("snapshot-{taken}.db");
        let path = self.dir.join(&name);

        db.snapshot_to(&path)?;

        let bytes = std::fs::metadata(&path)?.len() as i64;
        let sha = sha256_file(&path)?;
        let rows: i64 = crate::db::TABLES
            .iter()
            .map(|t| {
                db.with_read(|c| {
                    Ok(c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))?)
                })
                .unwrap_or(0)
            })
            .sum();

        let info = db.with_write(|tx| {
            tx.execute(
                "INSERT INTO snapshot(taken_ms, path, bytes, rows, sha256, note)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![taken, path.to_string_lossy(), bytes, rows, sha, note],
            )?;
            let id = tx.last_insert_rowid();
            Ok(SnapshotInfo {
                id,
                taken_ms: taken,
                path: path.to_string_lossy().into_owned(),
                bytes,
                rows,
                sha256: sha.clone(),
                note: note.map(|s| s.to_string()),
            })
        })?;

        // 立刻验一次：能打开、quick_check 通过。
        verify_file(&path)?;
        Ok(info)
    }

    /// 保证「今天有一份快照」。
    ///
    /// 若今天已经拍过，直接返回已有的那份，不重复拍——
    /// 这是给定时任务用的，每小时的 tick 不应产生 24 份快照。
    pub fn ensure_daily(&self, db: &Db) -> Result<(SnapshotInfo, bool)> {
        let today = now_ms().div_euclid(DAY_MS);
        if let Some(existing) = self.latest(db)? {
            if existing.day_index() == today {
                return Ok((existing, false));
            }
        }
        Ok((self.take(db, Some("每日自动快照"))?, true))
    }

    /// 最近一份快照。
    pub fn latest(&self, db: &Db) -> Result<Option<SnapshotInfo>> {
        db.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, taken_ms, path, bytes, rows, sha256, note
                 FROM snapshot ORDER BY taken_ms DESC LIMIT 1",
            )?;
            let mut rows = stmt.query_map([], row_to_info)?;
            Ok(rows.next().transpose()?)
        })
    }

    /// 全部快照，最新的在前。
    pub fn list(&self, db: &Db, limit: usize) -> Result<Vec<SnapshotInfo>> {
        db.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, taken_ms, path, bytes, rows, sha256, note
                 FROM snapshot ORDER BY taken_ms DESC LIMIT ?1",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![limit as i64], row_to_info)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// 找出「包含某个时刻」的那份快照，用于把时间点映射到具体文件。
    ///
    /// 回滚只需要时间戳，不需要文件——但恢复演练和「从快照重建」需要文件。
    pub fn at_or_before(&self, db: &Db, target_ms: i64) -> Result<Option<SnapshotInfo>> {
        db.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, taken_ms, path, bytes, rows, sha256, note
                 FROM snapshot WHERE taken_ms <= ?1 ORDER BY taken_ms DESC LIMIT 1",
            )?;
            let mut rows = stmt.query_map(rusqlite::params![target_ms], row_to_info)?;
            Ok(rows.next().transpose()?)
        })
    }

    /// 校验一份快照文件是否仍然完好（文件可能被外部删改）。
    pub fn verify(&self, info: &SnapshotInfo) -> Result<bool> {
        if !Path::new(&info.path).exists() {
            return Ok(false);
        }
        verify_file(Path::new(&info.path))?;
        let actual = sha256_file(Path::new(&info.path))?;
        Ok(actual == info.sha256)
    }

    /// 删除超过保留期的快照（文件 + 元数据）。
    ///
    /// 返回被删的条数。**不删最近一份**，否则一次误配置会把唯一的回滚点清光。
    pub fn prune(&self, db: &Db, keep_days: i64) -> Result<usize> {
        if keep_days <= 0 {
            return Err(Error::Config("快照保留天数必须为正".into()));
        }
        let cutoff = now_ms() - keep_days * DAY_MS;
        let doomed: Vec<SnapshotInfo> = db.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, taken_ms, path, bytes, rows, sha256, note
                 FROM snapshot WHERE taken_ms < ?1 ORDER BY taken_ms ASC",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![cutoff], row_to_info)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;

        // 永远留一份：如果全都超期，至少保住最新的那条。
        let newest = self.latest(db)?;
        let newest_id = newest.as_ref().map(|s| s.id);

        let mut removed = 0;
        for info in doomed {
            if Some(info.id) == newest_id {
                continue;
            }
            let _ = std::fs::remove_file(&info.path);
            db.with_write(|tx| {
                tx.execute(
                    "DELETE FROM snapshot WHERE id = ?1",
                    rusqlite::params![info.id],
                )?;
                Ok(())
            })?;
            removed += 1;
        }
        Ok(removed)
    }
}

fn row_to_info(r: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotInfo> {
    Ok(SnapshotInfo {
        id: r.get(0)?,
        taken_ms: r.get(1)?,
        path: r.get(2)?,
        bytes: r.get(3)?,
        rows: r.get(4)?,
        sha256: r.get(5)?,
        note: r.get(6)?,
    })
}

/// 打开快照文件做实检：能 open、`quick_check` 为 ok。
pub fn verify_file(path: &Path) -> Result<()> {
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| Error::Storage(format!("快照打不开 {}：{e}", path.display())))?;
    let status: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|e| Error::Storage(format!("快照校验失败：{e}")))?;
    if status != "ok" {
        return Err(Error::Storage(format!(
            "快照完整性检查未通过（{}）：{status}",
            path.display()
        )));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    fn store() -> (tempfile::TempDir, std::sync::Arc<Db>, SnapshotStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        let s = SnapshotStore::new(dir.path().join("snaps")).unwrap();
        (dir, db, s)
    }

    fn insert_person(db: &Db, id: &str) {
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO person(person_id, display_name, status, created_ms)
                 VALUES (?1, ?2, 'pending', ?3)",
                rusqlite::params![id, id, now_ms()],
            )?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 拍快照后元数据被记录() {
        let (_d, db, s) = store();
        insert_person(&db, "p-1");
        let info = s.take(&db, Some("手动")).unwrap();
        assert!(info.id > 0);
        assert!(info.bytes > 0);
        assert!(info.rows > 0, "行数应 > 0");
        assert_eq!(info.sha256.len(), 64, "sha256 应为 64 位十六进制");
        assert_eq!(info.note.as_deref(), Some("手动"));
        assert!(Path::new(&info.path).exists());
    }

    #[test]
    fn 快照文件可独立打开且含数据() {
        let (_d, db, s) = store();
        insert_person(&db, "p-1");
        let info = s.take(&db, None).unwrap();

        let c = rusqlite::Connection::open(&info.path).unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM person", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn 每日保证同一天只拍一份() {
        let (_d, db, s) = store();
        let (a, created_a) = s.ensure_daily(&db).unwrap();
        assert!(created_a, "第一次应真拍");
        let (b, created_b) = s.ensure_daily(&db).unwrap();
        assert!(!created_b, "同一天第二次不该再拍");
        assert_eq!(a.id, b.id);
    }

    #[test]
    fn 快照可被校验() {
        let (_d, db, s) = store();
        let info = s.take(&db, None).unwrap();
        assert!(s.verify(&info).unwrap(), "刚拍的快照应校验通过");
    }

    #[test]
    fn 文件被删后校验失败() {
        let (_d, db, s) = store();
        let info = s.take(&db, None).unwrap();
        std::fs::remove_file(&info.path).unwrap();
        assert!(!s.verify(&info).unwrap(), "文件没了应报不可用");
    }

    #[test]
    fn 文件被篡改后校验失败() {
        let (_d, db, s) = store();
        insert_person(&db, "p-1");
        let info = s.take(&db, None).unwrap();
        // 往文件尾部追加垃圾，sha256 必然变。
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&info.path)
            .unwrap();
        f.write_all(b"corrupt").unwrap();
        drop(f);
        assert!(!s.verify(&info).unwrap(), "被改过的文件不该校验通过");
    }

    #[test]
    fn 列表按时间倒序() {
        let (_d, db, s) = store();
        let a = s.take(&db, Some("a")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = s.take(&db, Some("b")).unwrap();
        let list = s.list(&db, 10).unwrap();
        assert_eq!(list[0].id, b.id, "最新的应在最前");
        assert_eq!(list[1].id, a.id);
    }

    #[test]
    fn 按时间点能找到对应快照() {
        let (_d, db, s) = store();
        let a = s.take(&db, None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = s.take(&db, None).unwrap();

        let found = s.at_or_before(&db, b.taken_ms).unwrap().unwrap();
        assert_eq!(found.id, b.id);

        let found = s.at_or_before(&db, a.taken_ms).unwrap().unwrap();
        assert_eq!(found.id, a.id);

        // 比所有快照都早 → 没有。
        assert!(s.at_or_before(&db, a.taken_ms - 10_000).unwrap().is_none());
    }

    #[test]
    fn 清理保留最近一份() {
        let (_d, db, s) = store();
        // 造一份「很老」的快照记录（不改文件时间，只改元数据时间）。
        let old = s.take(&db, Some("老")).unwrap();
        db.with_write(|tx| {
            tx.execute(
                "UPDATE snapshot SET taken_ms = ?1 WHERE id = ?2",
                rusqlite::params![now_ms() - 100 * DAY, old.id],
            )?;
            Ok(())
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let fresh = s.take(&db, Some("新")).unwrap();

        let removed = s.prune(&db, 30).unwrap();
        assert_eq!(removed, 1, "应只删掉超期的那份");
        let left = s.list(&db, 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, fresh.id, "最新的必须留下");
    }

    #[test]
    fn 全部超期时也至少留一份() {
        let (_d, db, s) = store();
        let only = s.take(&db, None).unwrap();
        db.with_write(|tx| {
            tx.execute(
                "UPDATE snapshot SET taken_ms = ?1",
                rusqlite::params![now_ms() - 999 * DAY],
            )?;
            Ok(())
        })
        .unwrap();
        let removed = s.prune(&db, 1).unwrap();
        assert_eq!(removed, 0, "唯一一份不该被删");
        assert!(s.latest(&db).unwrap().is_some());
        let _ = only;
    }

    #[test]
    fn 保留天数为零被拒绝() {
        let (_d, db, s) = store();
        assert!(s.prune(&db, 0).is_err());
        assert!(s.prune(&db, -1).is_err());
    }

    #[test]
    fn 快照包含回滚所需的所有表() {
        let (_d, db, s) = store();
        insert_person(&db, "p-1");
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO event(at_ms, kind, channel, summary, importance, scope)
                 VALUES (?1, 'k', 'mc', 's', 0.5, 'companion')",
                rusqlite::params![now_ms()],
            )?;
            Ok(())
        })
        .unwrap();
        let info = s.take(&db, None).unwrap();

        let c = rusqlite::Connection::open(&info.path).unwrap();
        for t in crate::db::TABLES {
            let n: i64 = c
                .query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
                .unwrap();
            if *t == "event" || *t == "person" {
                assert!(n > 0, "表 {t} 应有数据");
            }
        }
    }

    #[test]
    fn 快照的日归属按_utc_天算() {
        let (_d, db, s) = store();
        let info = s.take(&db, None).unwrap();
        assert_eq!(info.day_index(), info.taken_ms.div_euclid(DAY));
    }
}
