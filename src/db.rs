//! 存储层。
//!
//! # 为什么是一个写线程
//!
//! SQLite 在 WAL 模式下**只有一个写者**。给它一个连接池是「面向对象上正确、实际上错误」
//! 的做法——每个多出来的写者都只是把并行工作转换成 `SQLITE_BUSY`。
//! 所以这里：**写走单连接、读走独立连接**。
//!
//! # 为什么读写分开连接
//!
//! WAL 允许「读不阻塞写、写不阻塞读」。但 `rusqlite::Connection` 不是 `Sync`，
//! 一个连接不能跨线程共享。所以读用 `spawn_blocking` + 各自开连接（只读），
//! 写用一条专用线程串行执行。这样读写真正并行，且没有锁竞争。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};

use crate::error::{Error, Result};

/// 当前 schema 版本。
///
/// 变更规则：**只增不减**。老版本读到比自己新的库必须拒绝启动，
/// 而不是「尽力而为」——用旧代码写新库会静默丢字段。
pub const SCHEMA_VERSION: i32 = 21;

/// 数据库句柄。
///
/// 内部持有：
/// - 一个写连接（串行化，由调用方保证不并发使用）
/// - 一个读连接池的路径（每次 `spawn_blocking` 里现开现关，开销远小于跨线程共享的复杂度）
pub struct Db {
    path: PathBuf,
    write: parking::Mutex<Connection>,
    /// 读连接池。
    ///
    /// 每次读都 `Connection::open` 看似无害，实测代价很大：
    /// 打开文件 + 解析 WAL 索引约 100–200 µs，32 并发下 p99 从 9 ms 飙到 246 ms。
    /// 池化之后读路径只剩一次查询的时间。
    readers: parking::Mutex<Vec<Connection>>,
    /// 池容量上限，防止长时间空闲后攒一堆连接。
    max_readers: usize,
    /// 写入计数器，用于 `/v1/health` 与测试断言。
    writes: AtomicU64,
    /// 幂等命中计数（重复写入被忽略的次数），用来回答「重放到底有多频繁」。
    dedup_hits: AtomicU64,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.path)
            .field("schema", &SCHEMA_VERSION)
            .finish()
    }
}

impl Db {
    /// 打开（必要时创建）数据库。
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Self>> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Self::open_rw(&path)?;
        let db = Arc::new(Self {
            path,
            write: parking::Mutex::new(conn),
            readers: parking::Mutex::new(Vec::new()),
            max_readers: 32,
            writes: AtomicU64::new(0),
            dedup_hits: AtomicU64::new(0),
        });
        db.migrate()?;
        Ok(db)
    }

    fn open_rw(path: &Path) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Self::tune(&conn)?;
        Ok(conn)
    }

    fn open_ro(path: &Path) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    /// 连接级 PRAGMA。只在这里设一次，别散落在各查询里。
    fn tune(conn: &Connection) -> Result<()> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // WAL：读不阻塞写。`.db-wal` 是持久状态的一部分，备份必须连带（见 README 铁律 1）。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // NORMAL：掉电可能丢最后一个事务，但不会损坏库。
        // 对这个场景（记忆）可接受——丢一条记忆比每次提交都 fsync 强。
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // 外键要显式开，SQLite 默认是关的。
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // 负数 = 让 SQLite 在内存里缓存约这么多 KB，而不是页数。
        conn.pragma_update(None, "cache_size", -64_000)?;
        // 自动 checkpoint 阈值。SQLite 默认 1000 页（约 4 MB），实测这里会稳定
        // 停在 4.1 MB —— 那意味着 WAL 里的页一直没被重置回去，RSS 与读延迟都受影响。
        // 降到 512 页（约 2 MB）：写放大略增，但 WAL 小一半，读更快、快照更快。
        conn.pragma_update(None, "wal_autocheckpoint", 512)?;
        // 临时表放内存。
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 在一个写事务里执行。串行化由内部 Mutex 保证。
    pub fn with_write<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let mut guard = self.write.lock();
        let conn = &mut *guard;
        let tx = conn.transaction()?;
        // rusqlite 0.41 的 Transaction 自带 Deref<Target = Connection>，
        // 传 &*tx 即可在事务内执行语句。
        let out = f(&tx)?;
        tx.commit()?;
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(out)
    }

    /// 在一个读连接上执行（不加事务隔离，单条查询自己就是一致的）。
    pub fn with_read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        // borrow → 用 → 归还。拿不到就现开一个（池空说明并发高，不该让调用者等）。
        let pooled = {
            let mut pool = self.readers.lock();
            pool.pop()
        };
        let conn = match pooled {
            Some(c) => c,
            None => Self::open_ro(&self.path)?,
        };
        let out = f(&conn);
        // 出错也归还：连接本身没坏，坏的是这次查询。
        let mut pool = self.readers.lock();
        if pool.len() < self.max_readers {
            pool.push(conn);
        }
        out
    }

    /// 主动做一次 checkpoint 并尽量重置 WAL。
    ///
    /// 日常靠自动 checkpoint 就够；这个方法给「备份前」和「维护任务」用——
    /// 先把 WAL 合并进主库，`VACUUM INTO` 要拷贝的增量更小。
    ///
    /// 返回 `(checkpointed_pages, remaining_pages)`。
    /// `remaining > 0` 说明有长读事务挡着，checkpoint 没能跑完——值得告警。
    pub fn checkpoint(&self) -> Result<(i64, i64)> {
        let guard = self.write.lock();
        let (busy, log_pages, done_pages): (i64, i64, i64) =
            guard.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
        drop(guard);
        // busy=1 表示被读者挡住了；此时 done_pages < log_pages。
        let remaining = if busy != 0 { log_pages - done_pages } else { 0 };
        Ok((done_pages, remaining.max(0)))
    }

    pub fn write_count(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    pub fn dedup_hits(&self) -> u64 {
        self.dedup_hits.load(Ordering::Relaxed)
    }

    pub(crate) fn note_dedup_hit(&self) {
        self.dedup_hits.fetch_add(1, Ordering::Relaxed);
    }

    // ------------------------------------------------------------------ schema

    fn migrate(&self) -> Result<()> {
        self.with_write(|tx| {
            tx.execute_batch(SCHEMA_SQL)?;

            let found: i32 = tx
                .query_row(
                    "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);

            // 老代码读到新库必须拒绝启动，而不是尽力而为。
            if found > SCHEMA_VERSION {
                return Err(Error::Config(format!(
                    "数据库 schema 版本是 {found}，比本程序支持的 {SCHEMA_VERSION} 更新。\
                     请升级程序，不要用旧版本打开新库。"
                )));
            }

            // ---------------------------------------------------------- 列级迁移
            //
            // `CREATE TABLE IF NOT EXISTS` **不会**给已存在的表加列。
            // 所以新增列必须在这里显式 ALTER，否则老库永远缺列，
            // 而接口一查就是 503「no such column」。
            //
            // 每一条都必须幂等：先查 pragma_table_info，没有才加。
            add_column_if_missing(tx, "fact", "vec", "BLOB")?;
            add_column_if_missing(tx, "fact", "vec_embedder", "TEXT")?;
            add_column_if_missing(tx, "fact", "recall_count", "INTEGER NOT NULL DEFAULT 0")?;
            add_column_if_missing(tx, "fact", "last_recall_ms", "INTEGER")?;
            add_column_if_missing(tx, "fact", "weight_hint", "REAL")?;
            add_column_if_missing(tx, "relation", "note", "TEXT")?;
            // relation 是唯一一张「单主体迁移」与「建表语句」不一致的表：
            // 迁移过的新库有 owner_id，全新安装建出来的没有。补一列让两边一致。
            add_column_if_missing(tx, "relation", "owner_id", "TEXT NOT NULL DEFAULT 'me'")?;
            add_column_if_missing(tx, "promise", "goal_ref", "TEXT")?;
            add_column_if_missing(tx, "promise", "job_ref", "TEXT")?;
            add_column_if_missing(tx, "journal", "channel", "TEXT NOT NULL DEFAULT 'mc'")?;
            add_column_if_missing(tx, "journal", "dedup_key", "TEXT")?;
            add_column_if_missing(tx, "journal", "recall_count", "INTEGER NOT NULL DEFAULT 0")?;
            // journal 进入检索面之后才需要它：`recall_count` 得配一个
            // 「上次被想起的时刻」才能做带遗忘的衰减（见 `note_recalled`）。
            // 老库没有这一列，不补就会在第一次检索时撞 "no such column"。
            add_column_if_missing(tx, "journal", "last_recall_ms", "INTEGER")?;
            // 知识库文档的原文。老库（schema 9）没有这一列，
            // 补上之后旧文档的 content 为 NULL —— 展示与编辑时
            // 从块拼回来（见 `kb::doc_content`）。
            add_column_if_missing(tx, "kb_doc", "content", "TEXT")?;
            add_column_if_missing(tx, "event", "deleted_by", "INTEGER")?;
            add_column_if_missing(tx, "fact", "deleted_by", "INTEGER")?;
            add_column_if_missing(tx, "journal", "deleted_by", "INTEGER")?;
            add_column_if_missing(tx, "diary", "deleted_by", "INTEGER")?;
            add_column_if_missing(tx, "promise", "deleted_by", "INTEGER")?;
            add_column_if_missing(tx, "channel_message", "deleted_by", "INTEGER")?;
            add_column_if_missing(tx, "self_narrative", "deleted_by", "INTEGER")?;
            // 注入回放：一次检索内的所有候选共享 trace_id，
            // 这样能重建「这一轮用了哪条、丢了哪条、为什么」。
            add_column_if_missing(tx, "recall_log", "trace_id", "TEXT")?;
            add_column_if_missing(tx, "recall_log", "lane", "TEXT")?;
            add_column_if_missing(tx, "recall_log", "kept", "INTEGER")?;
            add_column_if_missing(tx, "recall_log", "reason", "TEXT")?;
            add_column_if_missing(tx, "recall_log", "text", "TEXT")?;
            // 注入回放要能「点开看具体内容」。replay 那一列存的是**摘要**
            // （哪段进了、哪段超预算），真正的文本在插件手里 ——
            // 不存下来的话，面板上永远只能看到一行摘要，排查时要翻日志。
            add_column_if_missing(tx, "injection_replay", "full_text", "TEXT")?;
            // 「忽视」：这个人的请求不回复、不入库。
            // 放在身份上而不是关系上 —— 关系是「我怎么看这个人」（数值），
            // 忽视是「要不要处理这个人」（开关），两个维度，混在一起会让
            // 「陌生但不想理」和「亲近但拉黑」都表达不出来。
            add_column_if_missing(tx, "person", "ignored", "INTEGER NOT NULL DEFAULT 0")?;
            // 重要度是「谁定的」：`auto` = 中枢自动判定，`caller` = 调用方传的，
            // `manual` = 人后来在面板上改的。不记这个的话，
            // 「为什么这条 0.3 那条 0.75」永远说不清。
            add_column_if_missing(tx, "event", "importance_source", "TEXT")?;
            // 自动判定的依据（「类型 died → 基底 0.78；关系 80 → +0.16」），
            // 面板上直接显示，不用二次推算。
            add_column_if_missing(tx, "event", "importance_reason", "TEXT")?;
            // 图片内容哈希（sha256）。同一张图被反复转发时不该存 N 份 ——
            // 群里「图又被发了一遍」是常态，不去重的话图库会被同一张图灌满。
            add_column_if_missing(tx, "image", "sha256", "TEXT")?;
            // 逻辑删除列：回滚要把 image 也包含进来（原本没有这一列）。
            add_column_if_missing(tx, "image", "deleted_by", "INTEGER")?;
            // 语音同理：回滚要能「忘掉某天之后录的语音」。
            add_column_if_missing(tx, "voice", "deleted_by", "INTEGER")?;
            // 语音内容哈希：同一段文本合成的语音也可能被重复上传。
            add_column_if_missing(tx, "voice", "sha256", "TEXT")?;
            // 图片用途（emoji/photo）。老库补列后一律为空 = photo，
            // 也就是「不自动发」——安全的一侧。
            add_column_if_missing(tx, "image", "kind", "TEXT")?;

            // ---------------------------------------------------------- 去重索引
            //
            // **必须放在 add_column 之后**：`SCHEMA_SQL` 是在 migrate 一开头
            // 整段 `execute_batch` 的，那时老库还没有 `sha256` 列 ——
            // 索引写在那里会直接报 `no such column: sha256`，**服务起不来**。
            // 第一次上 schema 19 时线上就是这么挂的（实测）。
            //
            // 用 IF NOT EXISTS + 忽略错误：老库里可能已经存在重复数据，
            // 建索引会失败 —— 那样宁可少一个索引也不要让服务起不来。
            let _ = tx.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS image_digest
                 ON image(sha256) WHERE sha256 IS NOT NULL AND deleted_by IS NULL",
                [],
            );
            let _ = tx.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS voice_digest
                 ON voice(sha256) WHERE sha256 IS NOT NULL AND deleted_by IS NULL",
                [],
            );

            // ---------------------------------------------------------- 结构迁移 v7
            //
            // 旧版按「多个主体」建表，现在改成单主体：旧的主体列改名 owner_id。
            //
            // SQLite **不能 DROP COLUMN、不能改主键**，所以只能重建表。
            // 这里用官方的 12 步流程里最小的一组：
            //   建新表 → 搬数据 → 删旧表 → 改名 → 重建索引。
            //
            // 搬数据时 owner_id 填一个固定值（'me'）：历史数据本来就只有一个人，
            // 那些 `hub-selftest` / `大肥鱼.locations` 是脏数据，不该保留多个身份。
            migrate_to_single_owner(tx)?;

            // 内置渠道必须存在：存量数据里到处是这四个 tag，
            // 删掉它们会让历史记录在面板上显示成「未知渠道」。
            for (tag, label, trust) in [
                ("mc", "Minecraft", 1.0),
                ("qq", "QQ", 0.9),
                ("live", "直播", 0.8),
                ("system", "系统", 1.0),
            ] {
                tx.execute(
                    "INSERT INTO channel(tag, label, trust, builtin, created_ms)
                     VALUES (?1, ?2, ?3, 1, ?4)
                     ON CONFLICT(tag) DO UPDATE SET builtin = 1",
                    rusqlite::params![tag, label, trust, now_ms()],
                )?;
            }

            if found < SCHEMA_VERSION {
                tx.execute(
                    "INSERT INTO schema_version(version, applied_ms) VALUES (?1, ?2)",
                    rusqlite::params![SCHEMA_VERSION, now_ms()],
                )?;
            }
            Ok(())
        })
    }

    /// 生成一份事务一致的快照。
    ///
    /// **这是唯一正确的备份方式。** 直接拷 `.db` 会漏掉 WAL 里尚未 checkpoint 的已提交事务
    /// （实测丢过 5/408 条，且零报错）。`VACUUM INTO` 产出的文件是自洽的、可独立打开的。
    pub fn snapshot_to(&self, dest: impl AsRef<Path>) -> Result<()> {
        let dest = dest.as_ref();
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // VACUUM INTO 要求目标不存在。
        let _ = std::fs::remove_file(dest);

        let guard = self.write.lock();
        guard.execute("VACUUM INTO ?1", rusqlite::params![dest.to_string_lossy()])?;
        drop(guard);

        // 立刻校验：一个「生成了但打不开」的快照比没有快照更危险，
        // 因为它会让人以为备份是好的。
        let check = Connection::open(dest)?;
        let status: String = check.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if status != "ok" {
            return Err(Error::Storage(format!("快照完整性检查失败：{status}")));
        }
        Ok(())
    }

    /// 数据目录里各表的行数，供 `/v1/health` 与面板展示。
    pub fn counts(&self) -> Result<Vec<(String, i64)>> {
        self.with_read(|c| {
            let mut out = Vec::new();
            for table in TABLES {
                let n: i64 =
                    c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
                out.push((table.to_string(), n));
            }
            Ok(out)
        })
    }

    /// WAL 文件大小（字节）。异常增大说明 checkpoint 饿死，值得告警。
    pub fn wal_size(&self) -> u64 {
        let mut p = self.path.clone().into_os_string();
        p.push("-wal");
        std::fs::metadata(PathBuf::from(p))
            .map(|m| m.len())
            .unwrap_or(0)
    }
}

/// 所有业务表。`counts()` 与快照校验都按这个列表走。
pub const TABLES: &[&str] = &[
    "channel",
    "persona",
    "voice",
    "image",
    "generation_log",
    "gen_state",
    "person",
    "person_account",
    "person_scene",
    "event",
    "fact",
    "journal",
    "diary",
    "self_narrative",
    "mood",
    "trait",
    "promise",
    "relation",
    "channel_message",
    "recall_log",
    "snapshot",
    "rollback_log",
    "plan",
    "plan_item",
    "affect_log",
    "injection_replay",
    "panel_session",
    // 知识库三张表。放进这个列表是为了 `counts()` 与快照校验能覆盖到它们 ——
    // 新表不加进这里，快照校验会「通过」但实际没检查它们。
    "kb",
    "kb_doc",
    "kb_chunk",
];

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
  version    INTEGER NOT NULL,
  applied_ms INTEGER NOT NULL
);

-- ============================================================ 身份
-- 跨渠道的「谁」。MC=UUID / QQ=QQ号 / 直播=平台uid，三套 id 天然不通，
-- 这一层就是让它们能指向同一个人。
-- 渠道注册表。原本 `channel` 是编译期枚举（mc/qq/live/system），
-- 加一个平台就要改代码、改枚举、改权重字段。改成数据之后：
--   · 接入方直接用一个新 tag（`bilibili`、`wechat`…），只要它在表里就合法；
--   · 面板能增删改，可信度也挂在这里；
--   · `builtin = 1` 的四行不允许删除（存量数据里到处是它们）。
CREATE TABLE IF NOT EXISTS channel (
  tag         TEXT PRIMARY KEY,
  label       TEXT NOT NULL,
  trust       REAL NOT NULL DEFAULT 0.8,
  builtin     INTEGER NOT NULL DEFAULT 0,
  created_ms  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS person (
  person_id    TEXT PRIMARY KEY,
  display_name TEXT NOT NULL,
  status       TEXT NOT NULL,          -- pending / confirmed
  created_ms   INTEGER NOT NULL,
  note         TEXT,
  -- 1 = 忽视：这个人的消息不回复、不入库（面板「身份」页勾选）
  ignored      INTEGER NOT NULL DEFAULT 0
);

-- 人格卡片：按**模型 id 正则**匹配，命中的那条注入上下文。
--
-- 为什么按模型匹配而不是全局一份：同一个角色跑在不同模型上要微调
-- （小模型要更强的格式约束，推理模型要更少的「慢慢想」提示）。
-- 一个角色只能有一位人格，但可以给不同模型挂不同的人格变体。
CREATE TABLE IF NOT EXISTS persona (
  id          TEXT PRIMARY KEY,
  -- 给人看的名字，比如「鲸鲸（flash 版）」
  name        TEXT NOT NULL,
  -- 匹配模型 id 的正则（Rust regex 语法）。空 = 匹配所有模型（兜底人格）。
  model_regex TEXT NOT NULL DEFAULT '',
  -- 这段文本会拼进 system prompt 的最前面（人设本身）
  body        TEXT NOT NULL,
  -- 数字越大越优先；同优先级时取**后**创建的那条（方便覆盖）
  priority    INTEGER NOT NULL DEFAULT 0,
  enabled     INTEGER NOT NULL DEFAULT 1,
  note        TEXT,
  created_ms  INTEGER NOT NULL,
  updated_ms  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS persona_priority ON persona(priority DESC, created_ms DESC);

-- 语音库：发出去的语音存下来，供人工筛选后训练更好的音源。
--
-- 为什么存 m4a 而不是原格式：训练工具链普遍吃 m4a/wav，而 m4a
-- 比 wav 小一个数量级（1 分钟约 0.5MB vs 10MB），几十条就能听出好坏，
-- 上千条也不会把盘塞满。
CREATE TABLE IF NOT EXISTS voice (
  id          TEXT PRIMARY KEY,
  -- 文件名（不含目录）。实际文件在 data_dir/voices/ 下。
  file        TEXT NOT NULL,
  -- 时长毫秒；拿不到时是 NULL，不瞎填。
  duration_ms INTEGER,
  bytes       INTEGER NOT NULL,
  -- 这段语音对应的文本（合成前的那句话），试听时一起看
  text        TEXT,
  -- 谁说的：`owner` = 它自己；或接入方自定义（主播、某角色…）
  speaker     TEXT NOT NULL DEFAULT 'default',
  -- 用的声音/风格（feiyu:cute 之类），方便对比
  voice       TEXT,
  style       TEXT,
  -- 人工打分 1..5，NULL = 还没评
  rating      INTEGER,
  note        TEXT,
  -- 来源渠道 tag，与 event.channel 同一套
  channel     TEXT,
  sha256      TEXT,
  created_ms  INTEGER NOT NULL,
  updated_ms  INTEGER NOT NULL,
  deleted_by  INTEGER
);
CREATE INDEX IF NOT EXISTS voice_recent ON voice(created_ms DESC);
CREATE INDEX IF NOT EXISTS voice_rating ON voice(rating DESC) WHERE rating IS NOT NULL;

-- 记忆画面：图片也是记忆，靠关键词触发、按关系关联人。
--
-- 为什么单独一张表而不是塞进 event：图片要能被**检索命中**（关键词）、
-- 要有自己的元数据（谁发的、关联谁、被想起几次），还要能人工编辑。
-- 塞进 event 的话这些字段都得塞进 summary 字符串里，那就没法查了。
CREATE TABLE IF NOT EXISTS image (
  id          TEXT PRIMARY KEY,
  -- `data_dir/images/` 下的文件名
  file        TEXT NOT NULL,
  -- 缩略图（列表页用，避免一次拉几十张原图）
  thumb       TEXT,
  width       INTEGER,
  height      INTEGER,
  bytes       INTEGER NOT NULL,
  -- 图片内容的一句话描述（接入方给的，或人工补的）—— 检索主要靠它
  caption     TEXT,
  -- 额外的关键词，逗号分隔；命中任一个就算触发
  keywords    TEXT,
  -- 谁发来的：渠道账号或人名
  sender      TEXT,
  channel     TEXT,
  -- 关联的人（逗号分隔的 person_id）
  person_ids  TEXT,
  -- 被想起过多少次（进了上下文就 +1）
  recall_count INTEGER NOT NULL DEFAULT 0,
  last_recall_ms INTEGER,
  -- 内容 sha256：同一张图重复发来只留一份（见 `image::digest`）
  sha256       TEXT,
  created_ms  INTEGER NOT NULL,
  updated_ms  INTEGER NOT NULL,
  deleted_by  INTEGER
);
CREATE INDEX IF NOT EXISTS image_recent ON image(created_ms DESC);

-- 生成记录：每次调 LLM 都留一行（成功也留）。
--
-- 为什么要这张表而不是只打日志：
-- 1. 「日记为什么三天没生成」必须能回答 —— 日志滚没了就再也查不到；
-- 2. 面板要能显示成功率、耗时、token 花费；
-- 3. 失败原因要让人看得懂（key 错 / 超时 / 格式不对），而不是只有 exit code。
CREATE TABLE IF NOT EXISTS generation_log (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  at_ms         INTEGER NOT NULL,
  -- tick / diary / weekly
  purpose       TEXT NOT NULL,
  ok            INTEGER NOT NULL,
  -- 为什么触发（batch_full/interval/manual）；跳过时是跳过的原因
  trigger       TEXT,
  model         TEXT,
  prompt_tokens INTEGER,
  completion_tokens INTEGER,
  elapsed_ms    INTEGER,
  -- 成功时的产出摘要（例如「情绪:雀跃 · 手帐 2 条」），面板直接显示
  result        TEXT,
  error         TEXT
);
CREATE INDEX IF NOT EXISTS gen_log_recent ON generation_log(at_ms DESC);
CREATE INDEX IF NOT EXISTS gen_log_fail ON generation_log(purpose, at_ms DESC) WHERE ok = 0;

-- 定时任务的水位线：日记/周记最后一次成功生成到哪。
--
-- 为什么不复用 `diary` 表反推：生成失败时表里没有新行，
-- 反推会得出「从没生成过」→ 每次重启都补跑一遍，反复烧钱。
CREATE TABLE IF NOT EXISTS gen_state (
  key        TEXT PRIMARY KEY,
  last_ok_ms INTEGER NOT NULL,
  updated_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS person_account (
  channel       TEXT NOT NULL,         -- mc / qq / live / system
  account       TEXT NOT NULL,
  person_id     TEXT NOT NULL REFERENCES person(person_id) ON DELETE CASCADE,
  seen_name     TEXT,
  first_seen_ms INTEGER NOT NULL,
  last_seen_ms  INTEGER NOT NULL,
  PRIMARY KEY (channel, account)
);
CREATE INDEX IF NOT EXISTS person_account_person ON person_account(person_id);

-- ============================================================ 经历
CREATE TABLE IF NOT EXISTS event (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  owner_id TEXT NOT NULL DEFAULT 'me',
  person_id    TEXT,
  at_ms        INTEGER NOT NULL,       -- 唯一时间主轴（现实 UTC 毫秒）
  kind         TEXT NOT NULL,
  channel      TEXT NOT NULL,
  channel_day  TEXT,                   -- 仅 MC：g<N>，只作标签
  summary      TEXT NOT NULL,
  importance   REAL NOT NULL,
  scope        TEXT NOT NULL,
  dedup_key    TEXT,
  -- 逻辑回滚用：这条记录是被哪一次回滚「遗忘」的。NULL = 仍然有效。
  deleted_by   INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS event_dedup ON event(dedup_key) WHERE dedup_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS event_recent ON event(at_ms DESC);
CREATE INDEX IF NOT EXISTS event_live ON event(at_ms DESC) WHERE deleted_by IS NULL;

-- ============================================================ 信念
CREATE TABLE IF NOT EXISTS fact (
  id            TEXT PRIMARY KEY,
  owner_id  TEXT NOT NULL DEFAULT 'me',
  person_id     TEXT,
  key           TEXT NOT NULL,
  value         TEXT NOT NULL,
  scope         TEXT NOT NULL,
  status        TEXT NOT NULL,
  tier          TEXT NOT NULL,         -- curated / extracted
  confidence    REAL NOT NULL,
  importance    REAL NOT NULL,
  created_ms    INTEGER NOT NULL,
  updated_ms    INTEGER NOT NULL,
  expires_ms    INTEGER,
  superseded_by TEXT,
  -- 权重模型的输入：被回忆的次数。检索命中时 +1。
  recall_count  INTEGER NOT NULL DEFAULT 0,
  last_recall_ms INTEGER,
  -- 写入时算好的权重快照，便于排序时不必每次重算全表。
  weight_hint   REAL,
  -- 向量。Java 侧的 fact 表有这两列，对接要能存能取。
  -- Hub 自己暂不做语义检索，但**存**是必须的：不然 MC 迁过来的
  -- 向量要么丢、要么得在两边各算一遍。
  vec           BLOB,
  vec_embedder  TEXT,
  deleted_by    INTEGER
);
CREATE INDEX IF NOT EXISTS fact_lookup ON fact(person_id, status, key);
CREATE INDEX IF NOT EXISTS fact_live ON fact(updated_ms DESC) WHERE deleted_by IS NULL;

CREATE TABLE IF NOT EXISTS journal (
  id           TEXT PRIMARY KEY,
  owner_id TEXT NOT NULL DEFAULT 'me',
  at_ms        INTEGER NOT NULL,
  planning_day_id TEXT,                -- 仅 MC：g<N>
  type         TEXT NOT NULL,
  source       TEXT NOT NULL,
  summary      TEXT NOT NULL,
  details      TEXT,
  importance   REAL NOT NULL,
  confidence   REAL NOT NULL,
  channel      TEXT NOT NULL DEFAULT 'mc',
  dedup_key    TEXT,
  -- 被回忆的次数与上次回忆时刻。检索命中时 +1（带遗忘衰减），
  -- 供权重模型的「强化」一项使用。
  recall_count INTEGER NOT NULL DEFAULT 0,
  last_recall_ms INTEGER,
  deleted_by   INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS journal_dedup ON journal(dedup_key) WHERE dedup_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS journal_recent ON journal(at_ms DESC);
CREATE INDEX IF NOT EXISTS journal_live ON journal(at_ms DESC) WHERE deleted_by IS NULL;

CREATE TABLE IF NOT EXISTS diary (
  id              TEXT PRIMARY KEY,
  owner_id    TEXT NOT NULL DEFAULT 'me',
  kind            TEXT NOT NULL,       -- day / week / month
  window_start_ms INTEGER NOT NULL,
  window_end_ms   INTEGER NOT NULL,
  title           TEXT,
  body            TEXT NOT NULL,
  model           TEXT,
  created_ms      INTEGER NOT NULL,
  updated_ms      INTEGER NOT NULL,
  deleted_by      INTEGER
);
CREATE INDEX IF NOT EXISTS diary_recent ON diary(kind, window_start_ms DESC);

CREATE TABLE IF NOT EXISTS self_narrative (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  owner_id  TEXT NOT NULL DEFAULT 'me',
  body          TEXT NOT NULL,
  model         TEXT,
  created_ms    INTEGER NOT NULL,
  superseded_ms INTEGER,
  deleted_by    INTEGER
);
-- 单主体：全表最多一条「当前自述」。
CREATE UNIQUE INDEX IF NOT EXISTS self_narrative_current
  ON self_narrative((1)) WHERE superseded_ms IS NULL AND deleted_by IS NULL;

-- ============================================================ 情绪与关系
-- 单主体：情绪只有一行，用 `id = 1` 约束住。
CREATE TABLE IF NOT EXISTS mood (
  id           INTEGER PRIMARY KEY CHECK (id = 1),
  owner_id     TEXT NOT NULL DEFAULT 'me',
  valence      REAL NOT NULL,
  arousal      REAL NOT NULL,
  dominance    REAL NOT NULL,
  label        TEXT,
  reason       TEXT,
  updated_ms   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS trait (
  trait        TEXT PRIMARY KEY,
  value        REAL NOT NULL,
  updated_ms   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS relation (
  person_id      TEXT PRIMARY KEY,
  -- 单主体之后所有记忆表都有这一列，relation 之前漏了；
  -- 老库由 `add_column_if_missing` 补，新库靠这里。
  owner_id       TEXT NOT NULL DEFAULT 'me',
  familiarity    INTEGER NOT NULL DEFAULT 0,
  trust          INTEGER NOT NULL DEFAULT 0,
  closeness      INTEGER NOT NULL DEFAULT 0,
  note           TEXT,
  first_met_ms   INTEGER NOT NULL,
  last_met_ms    INTEGER NOT NULL
);

-- 一个人在某个「场景」里被叫什么。
--
-- 为什么需要它：同一个人在不同场景的名字不一样，而 `person_account`
-- 的主键是 (渠道, 账号)，一个 QQ 号只能存一个 `seen_name` ——
-- 私聊昵称和群名片是两套名字，一张表装不下。
-- 这里按 (人, 场景) 记人工确认过的称呼；没有行就按渠道账号/群消息自动推导
-- （见 `query::relations`），所以「不填」不等于「不知道」。
CREATE TABLE IF NOT EXISTS person_scene (
  person_id   TEXT NOT NULL REFERENCES person(person_id) ON DELETE CASCADE,
  scene       TEXT NOT NULL,      -- mc / qq / qq_group / live
  name        TEXT NOT NULL,
  updated_ms  INTEGER NOT NULL,
  PRIMARY KEY (person_id, scene)
);

CREATE TABLE IF NOT EXISTS promise (
  id             TEXT PRIMARY KEY,
  owner_id   TEXT NOT NULL DEFAULT 'me',
  person_id      TEXT,
  description    TEXT NOT NULL,
  status         TEXT NOT NULL,
  created_ms     INTEGER NOT NULL,
  due_ms         INTEGER,
  updated_ms     INTEGER NOT NULL,
  blocked_reason TEXT,
  -- Java 侧的 Promise 有这两个字段（承诺可以挂到目标/任务上），
  -- 建表时漏了，导致对接接口一查就 503。补上。
  goal_ref       TEXT,
  job_ref        TEXT,
  deleted_by     INTEGER
);

-- ============================================================ 渠道原始消息
CREATE TABLE IF NOT EXISTS channel_message (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  channel     TEXT NOT NULL,           -- qq / live
  group_id    TEXT,
  account     TEXT,
  sender_name TEXT,
  content     TEXT NOT NULL,
  at_ms       INTEGER NOT NULL,
  message_id  TEXT,
  consumed    INTEGER NOT NULL DEFAULT 0,
  deleted_by  INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS channel_message_dedup
  ON channel_message(channel, message_id) WHERE message_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS channel_message_recent ON channel_message(channel, at_ms DESC);

-- ============================================================ 检索与审计
-- 每次「这条记忆进了上下文」都留一行。既用于强化权重，也用于回答
-- 「它为什么会这么说」。
CREATE TABLE IF NOT EXISTS recall_log (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  owner_id TEXT NOT NULL DEFAULT 'me',
  kind        TEXT NOT NULL,           -- event / fact / journal
  target_id   TEXT NOT NULL,
  at_ms       INTEGER NOT NULL,
  query       TEXT,
  score       REAL
);
CREATE INDEX IF NOT EXISTS recall_log_recent ON recall_log(target_id, at_ms DESC);

-- 每次快照留一行，供回滚挑选目标。
CREATE TABLE IF NOT EXISTS snapshot (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  taken_ms   INTEGER NOT NULL,
  path       TEXT NOT NULL,
  bytes      INTEGER NOT NULL,
  rows       INTEGER NOT NULL,
  sha256     TEXT NOT NULL,
  note       TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS snapshot_at ON snapshot(taken_ms);

-- ============================================================ 情绪流水
-- 每次情绪变化留一行。mood 是「现在是什么」，这里是「怎么变成这样的」。
-- 分开的理由：mood 会被反复覆盖，而「它今天被什么事弄高兴了」只有流水记得。
CREATE TABLE IF NOT EXISTS affect_log (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  owner_id TEXT NOT NULL DEFAULT 'me',
  person_id    TEXT,
  at_ms        INTEGER NOT NULL,
  kind         TEXT NOT NULL,
  field        TEXT NOT NULL,
  delta        REAL,
  reason       TEXT,
  event_id     TEXT
);
CREATE INDEX IF NOT EXISTS affect_log_recent ON affect_log(at_ms DESC);

-- ============================================================ 面板会话
-- 后台登录会话（2026-10-29 从内存搬到库里）。
--
-- 早先会话只在内存 map 里，理由是「重启就登出」算安全特性。实际用下来代价太大：
-- 换二进制、改配置、机器重启都要重登，而这些事很频繁。
-- 现在重启不掉线，有效期由 `auth::SESSION_TTL_MS` 决定。
--
-- 风险没有实质增加：能读到本地 DB 的人本来就能用 API token 把所有记忆读走。
CREATE TABLE IF NOT EXISTS panel_session (
  sid        TEXT PRIMARY KEY,
  user       TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  expires_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS panel_session_expiry ON panel_session(expires_ms);

-- ============================================================ 注入回放
-- 每一轮「往提示词里塞了哪几条记忆、丢了哪几条、为什么」。
--
-- 为什么它不在 recall_log 里：recall_log 记的是**Hub 自己的检索**，
-- 而 MC 侧的实际注入走的是 facts()/recentEvents() 这些列表接口，
-- 决策是插件做的。所以回放必须由插件推上来，不能靠 Hub 自己记。
CREATE TABLE IF NOT EXISTS injection_replay (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  owner_id TEXT NOT NULL DEFAULT 'me',
  at_ms        INTEGER NOT NULL,
  lane         TEXT,
  query        TEXT,
  chars        INTEGER,
  injected     INTEGER NOT NULL DEFAULT 0,
  replay       TEXT,
  dedup_key    TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS injection_replay_dedup
  ON injection_replay(dedup_key) WHERE dedup_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS injection_replay_recent
  ON injection_replay(at_ms DESC);

-- ============================================================ 规划
-- 一个规划日一份计划。**注意 `planning_day_id` 是游戏时钟 `g<N>`，
-- 它只是 MC 渠道的标签，不参与排序**——排序一律走 created_ms（现实时间）。
-- 这条与 §5「时间轴以 at_ms 为唯一主轴」一致：QQ/直播没有游戏时钟，
-- 拿 g<N> 当全局主轴会让各渠道窗口越错越远。
CREATE TABLE IF NOT EXISTS plan (
  id              TEXT PRIMARY KEY,
  owner_id    TEXT NOT NULL DEFAULT 'me',
  planning_day_id TEXT NOT NULL,      -- 游戏日标签，如 g24
  summary         TEXT,
  model           TEXT,
  game_day        INTEGER,
  created_ms      INTEGER NOT NULL,   -- 现实时间，排序主轴
  updated_ms      INTEGER NOT NULL,
  deleted_by      INTEGER
);
CREATE INDEX IF NOT EXISTS plan_recent ON plan(created_ms DESC);
-- 一个游戏日只能有一份计划（单主体，不再按人分区）。
CREATE UNIQUE INDEX IF NOT EXISTS plan_day ON plan(planning_day_id)
  WHERE deleted_by IS NULL;

-- 计划里的一条条待办。`ord` 保留原文件里的顺序——
-- 计划的语义依赖顺序（先采料再盖房），打乱就没意义了。
CREATE TABLE IF NOT EXISTS plan_item (
  plan_id     TEXT NOT NULL,
  id          TEXT NOT NULL,
  kind        TEXT,
  title       TEXT NOT NULL,
  task_type   TEXT,
  args_json   TEXT,
  priority    INTEGER,
  reason      TEXT,
  success     TEXT,
  fallback    TEXT,
  state       TEXT NOT NULL DEFAULT 'todo',
  outcome     TEXT,
  carry_from  TEXT,
  created_ms  INTEGER,
  updated_ms  INTEGER,
  ord         INTEGER NOT NULL,
  PRIMARY KEY (plan_id, id)
);
CREATE INDEX IF NOT EXISTS plan_item_plan ON plan_item(plan_id, ord);

-- 回滚审计。回滚是「忘记」操作，必须能回答「谁在什么时候忘了什么」。
CREATE TABLE IF NOT EXISTS rollback_log (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  at_ms          INTEGER NOT NULL,
  target_ms      INTEGER NOT NULL,
  mode           TEXT NOT NULL,        -- logical / purge / undo
  affected_json  TEXT NOT NULL,
  undo_of        INTEGER,
  note           TEXT
);

-- ============================================================ 知识库
--
-- **与记忆表完全分开**，不是一个字段的差别，是四个语义维度都不同：
--   记忆会衰减 / 会过期 / 会失效 / 能回滚；
--   知识库条目是外部资料，不衰减、不随时间变淡、「忘掉它」没有语义。
-- 混进 fact 表的后果是具体的：资料会被 30 天半衰期悄悄淡掉，
-- 回滚「忘掉某时刻之后的记忆」会把资料一起抹掉 —— 两者都不是用户的意思。
-- 详见 docs/知识库接入方案.md §1。

-- 一本知识库。可以有多个（MC 设定 / 服务器规则 / 世界观 ...）。
CREATE TABLE IF NOT EXISTS kb (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL,
  description TEXT,
  created_ms  INTEGER NOT NULL,
  updated_ms  INTEGER NOT NULL
);

-- 一篇文档。content_hash 用于导入幂等：同一份内容重复导入不会产生第二篇。
CREATE TABLE IF NOT EXISTS kb_doc (
  id           TEXT PRIMARY KEY,
  kb_id        TEXT NOT NULL,
  title        TEXT,
  source       TEXT,               -- 文件名 / URL / "paste"
  content_hash TEXT NOT NULL,
  -- **原文**。块是从它切出来的派生物，不是真相本身。
  --
  -- 第一版没存这一列，结果：文档没法原样查看（只能看到切碎的块）、
  -- 没法编辑（改了不知道以什么为准）、也没法换分块参数重新切
  -- （原文已经不存在了）。这三件事都要它。
  content      TEXT,
  n_chunks     INTEGER NOT NULL DEFAULT 0,
  created_ms   INTEGER NOT NULL,
  updated_ms   INTEGER NOT NULL,
  deleted_by   INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS kb_doc_hash
  ON kb_doc(kb_id, content_hash) WHERE deleted_by IS NULL;
CREATE INDEX IF NOT EXISTS kb_doc_kb ON kb_doc(kb_id) WHERE deleted_by IS NULL;

-- 检索单元。按**语义边界**（标题/段落/句子）切出来的，不是定长切块。
--
-- 向量三件套 vec / vec_dim / vec_embedder 必须一起存：
--   不同 embedder 产生的同维向量形状完全一样，不比较就看不出问题，
--   而打分会是垃圾且**零报错**。任何一个不匹配 → 该条直接不进候选。
CREATE TABLE IF NOT EXISTS kb_chunk (
  id           TEXT PRIMARY KEY,
  doc_id       TEXT NOT NULL,
  kb_id        TEXT NOT NULL,
  ord          INTEGER NOT NULL,   -- 文档内顺序：取邻居上下文 + 稳定排序
  heading      TEXT,               -- 所在小节标题（单独存，可单独加权）
  text         TEXT NOT NULL,
  vec          BLOB,
  vec_dim      INTEGER,
  vec_embedder TEXT,
  -- 嵌入状态机。没有 failed 状态的话，一条永远嵌不进去的条目会被无限重试。
  embed_state  TEXT NOT NULL DEFAULT 'pending',  -- pending/done/failed/skipped
  embed_tries  INTEGER NOT NULL DEFAULT 0,
  embed_err    TEXT,
  created_ms   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS kb_chunk_doc ON kb_chunk(doc_id, ord);
CREATE INDEX IF NOT EXISTS kb_chunk_kb ON kb_chunk(kb_id);
CREATE INDEX IF NOT EXISTS kb_chunk_pending
  ON kb_chunk(id) WHERE embed_state = 'pending';
"#;

/// 给已存在的表加列。已存在则什么都不做（幂等）。
///
/// 为什么需要它：`CREATE TABLE IF NOT EXISTS` 对已建好的表是空操作，
/// 新增列永远不会出现在老库里。这是 SQLite 的常见坑，也是
/// 「本地能跑、线上 503」的典型原因。
/// 全库唯一的主体标识。
///
/// 「中枢是**一个人的**记忆，不是多人」——所以没有多租户维度。
/// 保留一个常量列只是为了 SQL 里能写明「这是谁的」，
/// 而不是散落在每条语句里的魔法字符串。
pub const OWNER: &str = "me";

/// 把 v6 及更早的库（带旧主体列）改成单主体结构。
///
/// 幂等：表里已经没有旧主体列时直接返回。
fn migrate_to_single_owner(tx: &rusqlite::Connection) -> Result<()> {
    /// (表名, 新表建表语句, 要搬的列, 索引重建语句)
    ///
    /// 「要搬的列」里 `owner_id` 一律写成常量 `'me'`：历史数据只有一个主体，
    /// 旧主体列的值没有保留意义（而且里面混着自检用的脏值）。
    type Step = (
        &'static str,
        &'static str,
        &'static [&'static str],
        &'static [&'static str],
    );

    const STEPS: &[Step] = &[
        (
            "event",
            "CREATE TABLE event_new (
               id           INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_id     TEXT NOT NULL DEFAULT 'me',
               person_id    TEXT,
               at_ms        INTEGER NOT NULL,
               kind         TEXT NOT NULL,
               channel      TEXT NOT NULL,
               channel_day  TEXT,
               summary      TEXT NOT NULL,
               importance   REAL NOT NULL,
               scope        TEXT NOT NULL,
               dedup_key    TEXT,
               deleted_by   INTEGER)",
            &["id", "person_id", "at_ms", "kind", "channel", "channel_day", "summary",
              "importance", "scope", "dedup_key", "deleted_by"],
            &[
                "CREATE UNIQUE INDEX IF NOT EXISTS event_dedup ON event(dedup_key) WHERE dedup_key IS NOT NULL",
                "CREATE INDEX IF NOT EXISTS event_recent ON event(at_ms DESC)",
                "CREATE INDEX IF NOT EXISTS event_live ON event(at_ms DESC) WHERE deleted_by IS NULL",
            ],
        ),
        (
            "fact",
            "CREATE TABLE fact_new (
               id            TEXT PRIMARY KEY,
               owner_id      TEXT NOT NULL DEFAULT 'me',
               person_id     TEXT,
               key           TEXT NOT NULL,
               value         TEXT NOT NULL,
               scope         TEXT NOT NULL,
               status        TEXT NOT NULL,
               tier          TEXT NOT NULL,
               confidence    REAL NOT NULL,
               importance    REAL NOT NULL,
               created_ms    INTEGER NOT NULL,
               updated_ms    INTEGER NOT NULL,
               expires_ms    INTEGER,
               superseded_by TEXT,
               recall_count  INTEGER NOT NULL DEFAULT 0,
               last_recall_ms INTEGER,
               weight_hint   REAL,
               vec           BLOB,
               vec_embedder  TEXT,
               deleted_by    INTEGER)",
            &["id", "person_id", "key", "value", "scope", "status", "tier", "confidence",
              "importance", "created_ms", "updated_ms", "expires_ms", "superseded_by",
              "recall_count", "last_recall_ms", "weight_hint", "vec", "vec_embedder", "deleted_by"],
            &[
                "CREATE INDEX IF NOT EXISTS fact_lookup ON fact(person_id, status, key)",
                "CREATE INDEX IF NOT EXISTS fact_live ON fact(updated_ms DESC) WHERE deleted_by IS NULL",
            ],
        ),
        (
            "journal",
            "CREATE TABLE journal_new (
               id           TEXT PRIMARY KEY,
               owner_id     TEXT NOT NULL DEFAULT 'me',
               at_ms        INTEGER NOT NULL,
               planning_day_id TEXT,
               type         TEXT NOT NULL,
               source       TEXT NOT NULL,
               summary      TEXT NOT NULL,
               details      TEXT,
               importance   REAL NOT NULL,
               confidence   REAL NOT NULL,
               channel      TEXT NOT NULL DEFAULT 'mc',
               dedup_key    TEXT,
               recall_count INTEGER NOT NULL DEFAULT 0,
               last_recall_ms INTEGER,
               deleted_by   INTEGER)",
            &["id", "at_ms", "planning_day_id", "type", "source", "summary", "details",
              "importance", "confidence", "channel", "dedup_key", "recall_count",
              "last_recall_ms", "deleted_by"],
            &[
                "CREATE UNIQUE INDEX IF NOT EXISTS journal_dedup ON journal(dedup_key) WHERE dedup_key IS NOT NULL",
                "CREATE INDEX IF NOT EXISTS journal_recent ON journal(at_ms DESC)",
                "CREATE INDEX IF NOT EXISTS journal_live ON journal(at_ms DESC) WHERE deleted_by IS NULL",
            ],
        ),
        (
            "diary",
            "CREATE TABLE diary_new (
               id              TEXT PRIMARY KEY,
               owner_id        TEXT NOT NULL DEFAULT 'me',
               kind            TEXT NOT NULL,
               window_start_ms INTEGER NOT NULL,
               window_end_ms   INTEGER NOT NULL,
               title           TEXT,
               body            TEXT NOT NULL,
               model           TEXT,
               created_ms      INTEGER NOT NULL,
               updated_ms      INTEGER NOT NULL,
               deleted_by      INTEGER)",
            &["id", "kind", "window_start_ms", "window_end_ms", "title", "body", "model",
              "created_ms", "updated_ms", "deleted_by"],
            &["CREATE INDEX IF NOT EXISTS diary_recent ON diary(kind, window_start_ms DESC)"],
        ),
        (
            "self_narrative",
            "CREATE TABLE self_narrative_new (
               id            INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_id      TEXT NOT NULL DEFAULT 'me',
               body          TEXT NOT NULL,
               model         TEXT,
               created_ms    INTEGER NOT NULL,
               superseded_ms INTEGER,
               deleted_by    INTEGER)",
            &["id", "body", "model", "created_ms", "superseded_ms", "deleted_by"],
            &[],
        ),
        (
            "plan",
            "CREATE TABLE plan_new (
               id              TEXT PRIMARY KEY,
               owner_id        TEXT NOT NULL DEFAULT 'me',
               planning_day_id TEXT NOT NULL,
               summary         TEXT,
               model           TEXT,
               game_day        INTEGER,
               created_ms      INTEGER NOT NULL,
               updated_ms      INTEGER NOT NULL,
               deleted_by      INTEGER)",
            &["id", "planning_day_id", "summary", "model", "game_day", "created_ms",
              "updated_ms", "deleted_by"],
            &["CREATE INDEX IF NOT EXISTS plan_recent ON plan(created_ms DESC)"],
        ),
        (
            "promise",
            "CREATE TABLE promise_new (
               id             TEXT PRIMARY KEY,
               owner_id       TEXT NOT NULL DEFAULT 'me',
               person_id      TEXT,
               description    TEXT NOT NULL,
               status         TEXT NOT NULL,
               created_ms     INTEGER NOT NULL,
               due_ms         INTEGER,
               updated_ms     INTEGER NOT NULL,
               blocked_reason TEXT,
               goal_ref       TEXT,
               job_ref        TEXT,
               deleted_by     INTEGER)",
            &["id", "person_id", "description", "status", "created_ms", "due_ms",
              "updated_ms", "blocked_reason", "goal_ref", "job_ref", "deleted_by"],
            &["CREATE INDEX IF NOT EXISTS promise_open ON promise(status)"],
        ),
        (
            "channel_message",
            "CREATE TABLE channel_message_new (
               id          INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_id    TEXT NOT NULL DEFAULT 'me',
               channel     TEXT NOT NULL,
               group_id    TEXT,
               account     TEXT,
               sender_name TEXT,
               content     TEXT NOT NULL,
               at_ms       INTEGER NOT NULL,
               message_id  TEXT,
               consumed    INTEGER NOT NULL DEFAULT 0,
               deleted_by  INTEGER)",
            &["id", "channel", "group_id", "account", "sender_name", "content", "at_ms",
              "message_id", "consumed", "deleted_by"],
            &[
                "CREATE UNIQUE INDEX IF NOT EXISTS channel_message_dedup ON channel_message(channel, message_id) WHERE message_id IS NOT NULL",
                "CREATE INDEX IF NOT EXISTS channel_message_recent ON channel_message(channel, at_ms DESC)",
            ],
        ),
        (
            "mood",
            "CREATE TABLE mood_new (
               id           INTEGER PRIMARY KEY CHECK (id = 1),
               owner_id     TEXT NOT NULL DEFAULT 'me',
               valence      REAL NOT NULL,
               arousal      REAL NOT NULL,
               dominance    REAL NOT NULL,
               label        TEXT,
               reason       TEXT,
               updated_ms   INTEGER NOT NULL)",
            &["valence", "arousal", "dominance", "label", "reason", "updated_ms"],
            &[],
        ),
        (
            "trait",
            "CREATE TABLE trait_new (
               trait        TEXT PRIMARY KEY,
               owner_id     TEXT NOT NULL DEFAULT 'me',
               value        REAL NOT NULL,
               updated_ms   INTEGER NOT NULL)",
            &["trait", "value", "updated_ms"],
            &[],
        ),
        (
            "relation",
            "CREATE TABLE relation_new (
               person_id      TEXT PRIMARY KEY,
               owner_id       TEXT NOT NULL DEFAULT 'me',
               familiarity    INTEGER NOT NULL DEFAULT 0,
               trust          INTEGER NOT NULL DEFAULT 0,
               closeness      INTEGER NOT NULL DEFAULT 0,
               first_met_ms   INTEGER NOT NULL,
               last_met_ms    INTEGER NOT NULL)",
            &["person_id", "familiarity", "trust", "closeness", "first_met_ms", "last_met_ms"],
            &[],
        ),
        (
            "recall_log",
            "CREATE TABLE recall_log_new (
               id           INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_id     TEXT NOT NULL DEFAULT 'me',
               kind         TEXT NOT NULL,
               target_id    TEXT NOT NULL,
               at_ms        INTEGER NOT NULL,
               query        TEXT,
               score        REAL,
               trace_id     TEXT,
               lane         TEXT,
               kept         INTEGER,
               reason       TEXT,
               text         TEXT)",
            &["kind", "target_id", "at_ms", "query", "score", "trace_id", "lane", "kept",
              "reason", "text"],
            &["CREATE INDEX IF NOT EXISTS recall_log_recent ON recall_log(target_id, at_ms DESC)"],
        ),
        (
            "affect_log",
            "CREATE TABLE affect_log_new (
               id        INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_id  TEXT NOT NULL DEFAULT 'me',
               person_id TEXT,
               at_ms     INTEGER NOT NULL,
               kind      TEXT NOT NULL,
               field     TEXT NOT NULL,
               delta     REAL,
               reason    TEXT,
               event_id  TEXT)",
            &["person_id", "at_ms", "kind", "field", "delta", "reason", "event_id"],
            &["CREATE INDEX IF NOT EXISTS affect_log_recent ON affect_log(at_ms DESC)"],
        ),
        (
            "injection_replay",
            "CREATE TABLE injection_replay_new (
               id           INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_id     TEXT NOT NULL DEFAULT 'me',
               at_ms        INTEGER NOT NULL,
               lane         TEXT,
               query        TEXT,
               chars        INTEGER,
               injected     INTEGER NOT NULL DEFAULT 0,
               replay       TEXT,
               dedup_key    TEXT)",
            &["at_ms", "lane", "query", "chars", "injected", "replay", "dedup_key"],
            &[
                "CREATE UNIQUE INDEX IF NOT EXISTS injection_replay_dedup ON injection_replay(dedup_key) WHERE dedup_key IS NOT NULL",
                "CREATE INDEX IF NOT EXISTS injection_replay_recent ON injection_replay(at_ms DESC)",
            ],
        ),
    ];

    for (table, create_new, cols, indexes) in STEPS {
        // 已经是新结构就跳过（幂等）。
        let has_companion: i64 = tx
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = 'companion_id'"
                ),
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if has_companion == 0 {
            continue;
        }

        tx.execute_batch("PRAGMA foreign_keys=OFF")?;
        let _ = tx.execute(&format!("DROP TABLE IF EXISTS {table}_new"), []);
        tx.execute(create_new, [])?;

        // 目标表可能缺老库里的某些列（老库版本更旧），所以按「新表里有、老表里也有」取交集。
        let old_cols: std::collections::HashSet<String> = {
            let mut stmt = tx.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
            let v = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            v.into_iter().collect()
        };
        let usable: Vec<&str> = cols
            .iter()
            .copied()
            .filter(|c| old_cols.contains(*c))
            .collect();
        let col_list = usable.join(", ");
        if usable.is_empty() {
            tx.execute(&format!("DROP TABLE {table}_new"), [])?;
            continue;
        }
        tx.execute(
            &format!(
                "INSERT INTO {table}_new(owner_id, {col_list})                  SELECT '{OWNER}', {col_list} FROM {table}"
            ),
            [],
        )?;

        tx.execute(&format!("DROP TABLE {table}"), [])?;
        tx.execute(&format!("ALTER TABLE {table}_new RENAME TO {table}"), [])?;
        for idx in *indexes {
            let _ = tx.execute(idx, []);
        }
        tx.execute_batch("PRAGMA foreign_keys=ON")?;
    }

    // 自述的「全表最多一条当前版」索引要单独建：它依赖部分索引表达式。
    let _ = tx.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS self_narrative_current
         ON self_narrative((1)) WHERE superseded_ms IS NULL AND deleted_by IS NULL",
        [],
    );
    let _ = tx.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS plan_day ON plan(planning_day_id) WHERE deleted_by IS NULL",
        [],
    );

    let _ = tx.execute("DROP INDEX IF EXISTS self_narrative_history", []);
    let _ = tx.execute(
        "CREATE INDEX IF NOT EXISTS self_narrative_history ON self_narrative(created_ms DESC)",
        [],
    );
    Ok(())
}

fn add_column_if_missing(
    tx: &rusqlite::Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> Result<()> {
    let exists: i64 = tx
        .query_row(
            &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
            rusqlite::params![column],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if exists == 0 {
        tx.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
            [],
        )?;
    }
    Ok(())
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 最小可用的互斥锁。
///
/// 不用 `std::sync::Mutex` 是因为 `rusqlite::Connection` 在 panic 后会让锁中毒，
/// 而「数据库连接因一次 panic 永久不可用」比「忽略中毒继续用」更糟。
mod parking {
    use std::cell::UnsafeCell;
    use std::ops::{Deref, DerefMut};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    pub struct Mutex<T> {
        locked: AtomicBool,
        value: UnsafeCell<T>,
    }

    // SAFETY: 所有访问都经过 `locked` 这把自旋锁，同一时刻只有一个线程能拿到 `&mut T`。
    unsafe impl<T: Send> Send for Mutex<T> {}
    unsafe impl<T: Send> Sync for Mutex<T> {}

    pub struct Guard<'a, T> {
        mutex: &'a Mutex<T>,
    }

    impl<T> Mutex<T> {
        pub fn new(v: T) -> Self {
            Self {
                locked: AtomicBool::new(false),
                value: UnsafeCell::new(v),
            }
        }

        pub fn lock(&self) -> Guard<'_, T> {
            while self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                thread::yield_now();
            }
            Guard { mutex: self }
        }
    }

    impl<T> Deref for Guard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            // SAFETY: 持有 guard 期间独占访问。
            unsafe { &*self.mutex.value.get() }
        }
    }

    impl<T> DerefMut for Guard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            // SAFETY: 持有 guard 期间独占访问。
            unsafe { &mut *self.mutex.value.get() }
        }
    }

    impl<T> Drop for Guard<'_, T> {
        fn drop(&mut self) {
            self.mutex.locked.store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 建库后所有表都存在() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        let counts = db.counts().unwrap();
        assert_eq!(counts.len(), TABLES.len());
        for (t, _) in &counts {
            assert!(TABLES.contains(&t.as_str()), "意外的表 {t}");
        }
    }

    #[test]
    fn schema_版本被记录() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        let db = Db::open(&p).unwrap();
        let v: i32 = db
            .with_read(|c| {
                Ok(c.query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn 知识库三张表被建出来且字段齐全() {
        // 为什么这条要单独立一个判据：`CREATE TABLE IF NOT EXISTS` 只在
        // SCHEMA_SQL 里写对才会建表，而写错（或忘了加进 SCHEMA_SQL）时
        // **应用照样能启动**，直到第一次访问 `/v1/kb/*` 才 503。
        // 那种失败模式是「启动正常、功能失踪」，必须在这里拦住。
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();

        db.with_read(|c| {
            for t in ["kb", "kb_doc", "kb_chunk"] {
                let n: i64 = c.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    rusqlite::params![t],
                    |r| r.get(0),
                )?;
                assert_eq!(n, 1, "表 {t} 不存在");
            }

            // kb_chunk 的向量三件套必须一起在。少任何一个，
            // 都会出现「不同 embedder 的向量被拿来比较」这种静默错误。
            let cols: Vec<String> = c
                .prepare("PRAGMA table_info(kb_chunk)")?
                .query_map([], |r| r.get::<_, String>(1))?
                .filter_map(std::result::Result::ok)
                .collect();
            for need in [
                "vec",
                "vec_dim",
                "vec_embedder",
                "embed_state",
                "heading",
                "ord",
            ] {
                assert!(
                    cols.iter().any(|c| c == need),
                    "kb_chunk 缺列 {need}，实得 {cols:?}"
                );
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 知识库表进了表清单() {
        // 不在 TABLES 里的表不会被 counts()/快照校验覆盖 —— 快照会「校验通过」
        // 但实际没检查它。这是数据库类项目里很典型的一处静默遗漏。
        for t in ["kb", "kb_doc", "kb_chunk"] {
            assert!(TABLES.contains(&t), "表 {t} 没进 TABLES 清单");
        }
    }

    #[test]
    fn 重复打开同一个库不报错() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        let _a = Db::open(&p).unwrap();
        drop(_a);
        let b = Db::open(&p).unwrap();
        assert!(b.counts().is_ok());
    }

    #[test]
    fn 打开比程序更新的库会被拒绝() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        {
            let db = Db::open(&p).unwrap();
            db.with_write(|tx| {
                tx.execute(
                    "INSERT INTO schema_version(version, applied_ms) VALUES (?1, ?2)",
                    rusqlite::params![SCHEMA_VERSION + 5, now_ms()],
                )?;
                Ok(())
            })
            .unwrap();
        }
        let err = Db::open(&p).unwrap_err();
        assert!(
            matches!(err.kind(), crate::error::ErrorKind::Config),
            "应该拒绝启动，实得 {err:?}"
        );
    }

    #[test]
    fn 快照是一致且可独立打开的() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO person(person_id, display_name, status, created_ms)
                 VALUES ('p-1','测试',?1,?2)",
                rusqlite::params!["pending", now_ms()],
            )?;
            Ok(())
        })
        .unwrap();

        let dest = dir.path().join("snap.db");
        db.snapshot_to(&dest).unwrap();
        assert!(dest.exists());

        // 独立打开快照，确认数据在。
        let c = Connection::open(&dest).unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM person", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "快照里应有那一行");

        let status: String = c.query_row("PRAGMA quick_check", [], |r| r.get(0)).unwrap();
        assert_eq!(status, "ok");
    }

    #[test]
    fn 快照会覆盖已存在的同名文件() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        let dest = dir.path().join("snap.db");
        db.snapshot_to(&dest).unwrap();
        // 第二次必须成功（VACUUM INTO 本身要求目标不存在，我们负责删）。
        db.snapshot_to(&dest).unwrap();
    }

    #[test]
    fn 写事务失败会回滚() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        let r = db.with_write(|tx| {
            tx.execute(
                "INSERT INTO person(person_id, display_name, status, created_ms)
                 VALUES ('p-x','失败',?1,?2)",
                rusqlite::params!["pending", now_ms()],
            )?;
            Err::<(), _>(Error::BadRequest("故意失败".into()))
        });
        assert!(r.is_err());
        let n: i64 = db
            .with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM person", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 0, "失败的事务不应留下数据");
    }

    #[test]
    fn wal_文件路径可读() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        // 刚建库可能还没有 wal，但不该 panic。
        let _ = db.wal_size();
    }
}
