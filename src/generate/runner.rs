//! 执行器：把 tick / 日记 / 周记真正跑起来，并记账。
//!
//! # 三条硬规矩
//!
//! 1. **失败绝不冒泡到服务**：生成失败只记 `generation_log` + 面板红点，
//!    不能因为 LLM 挂了就让中枢挂掉（那会把「记忆」这个核心功能也带下去）。
//! 2. **每次都记账**：成功记 token/耗时/产出摘要，失败记原因。
//!    「日记为什么三天没生成」必须能回答。
//! 3. **状态写 `gen_state`**：定时任务靠它判断「今天跑过没有」。
//!    不能用 `diary` 表反推 —— 失败时表里没新行，会被判成「从没跑过」反复补跑烧钱。

use std::sync::Arc;

use serde::Serialize;

use crate::db::Db;
use crate::error::Result;
use crate::llm::{ChatClient, Purpose};

/// 跑完一次后的记账。
pub struct Record<'a> {
    pub purpose: Purpose,
    pub ok: bool,
    pub trigger: &'a str,
    pub model: &'a str,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub elapsed_ms: i64,
    pub result: Option<String>,
    pub error: Option<String>,
}

/// 写一条生成记录。**尽力而为**：记账失败不能影响主流程。
pub fn log(db: &Db, r: Record<'_>) {
    let now = crate::db::now_ms();
    let res = db.with_write(|tx| {
        tx.execute(
            "INSERT INTO generation_log(at_ms, purpose, ok, trigger, model,
                                        prompt_tokens, completion_tokens, elapsed_ms,
                                        result, error)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            rusqlite::params![
                now,
                r.purpose.as_str(),
                if r.ok { 1 } else { 0 },
                r.trigger,
                r.model,
                r.prompt_tokens,
                r.completion_tokens,
                r.elapsed_ms,
                r.result,
                r.error
            ],
        )?;
        Ok(())
    });
    if let Err(e) = res {
        tracing::warn!("写生成记录失败（不影响主流程）：{e}");
    }
}

/// 读 `gen_state` 里的水位线。
pub fn state_get(db: &Db, key: &str) -> Option<i64> {
    // 注意别写成  —— 外层 Option 是「查询
    // 有没有报错」，内层是「有没有这行」。多套一层会永远拿到 None，
    // 而症状是「每次重启都补跑一遍」（反复烧钱），很难往这里想。
    db.with_read(|c| {
        Ok(c.query_row(
            "SELECT last_ok_ms FROM gen_state WHERE key = ?1",
            rusqlite::params![key],
            |r| r.get::<_, i64>(0),
        )
        .ok())
    })
    .ok()
    .flatten()
}

/// 写 `gen_state`。
pub fn state_set(db: &Db, key: &str, ms: i64) {
    let now = crate::db::now_ms();
    let _ = db.with_write(|tx| {
        tx.execute(
            "INSERT INTO gen_state(key, last_ok_ms, updated_ms) VALUES (?1,?2,?3)
             ON CONFLICT(key) DO UPDATE SET last_ok_ms = ?2, updated_ms = ?3",
            rusqlite::params![key, ms, now],
        )?;
        Ok(())
    });
}

/// 最近一条成功记录的时间（面板/排错用）。
pub fn last_ok_ms(db: &Db, purpose: Purpose) -> Option<i64> {
    db.with_read(|c| {
        Ok(c.query_row(
            "SELECT MAX(at_ms) FROM generation_log WHERE purpose = ?1 AND ok = 1",
            rusqlite::params![purpose.as_str()],
            |r| r.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten())
    })
    .ok()
    .flatten()
}

/// 连续失败了几次（从最近往回数）。面板用它显示红点。
pub fn consecutive_failures(db: &Db, purpose: Purpose) -> i64 {
    db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT ok FROM generation_log WHERE purpose = ?1 ORDER BY at_ms DESC LIMIT 20",
        )?;
        let oks: Vec<i64> = stmt
            .query_map(rusqlite::params![purpose.as_str()], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(oks.into_iter().take_while(|o| *o == 0).count() as i64)
    })
    .unwrap_or(0)
}

/// 面板要显示的一行。
#[derive(Serialize)]
pub struct GenLogRow {
    pub id: i64,
    pub at_ms: i64,
    pub purpose: String,
    pub ok: bool,
    pub trigger: Option<String>,
    pub model: Option<String>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub elapsed_ms: Option<i64>,
    pub result: Option<String>,
    pub error: Option<String>,
}

pub fn recent(db: &Db, limit: usize) -> Result<Vec<GenLogRow>> {
    let limit = limit.clamp(1, 200) as i64;
    db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT id, at_ms, purpose, ok, trigger, model, prompt_tokens,
                    completion_tokens, elapsed_ms, result, error
             FROM generation_log ORDER BY at_ms DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![limit], |r| {
                Ok(GenLogRow {
                    id: r.get(0)?,
                    at_ms: r.get(1)?,
                    purpose: r.get(2)?,
                    ok: r.get::<_, i64>(3)? != 0,
                    trigger: r.get(4)?,
                    model: r.get(5)?,
                    prompt_tokens: r.get(6)?,
                    completion_tokens: r.get(7)?,
                    elapsed_ms: r.get(8)?,
                    result: r.get(9)?,
                    error: r.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

/// 一次调用的计时包装。
pub async fn timed<F, T>(f: F) -> (Result<T>, i64)
where
    F: std::future::Future<Output = Result<T>>,
{
    let t0 = std::time::Instant::now();
    let r = f.await;
    (r, t0.elapsed().as_millis() as i64)
}

/// 从配置造客户端。**不缓存**：配置随时可能在面板改，缓存就得处理失效。
pub fn client(cfg: &crate::config::Config) -> ChatClient {
    ChatClient::new(cfg.llm.clone())
}

/// 生成任务是否可用（LLM 配好了 + 功能开着）。
pub fn usable(cfg: &crate::config::Config) -> bool {
    cfg.llm.is_ready()
}

/// 共享给后台任务的句柄。
#[derive(Clone)]
pub struct GenCtx {
    pub db: Arc<Db>,
    pub cfg: Arc<crate::config::Config>,
}

// ================================================================== 定时任务

/// 现在是不是该跑「每日」任务了？
///
/// 判据：**今天的目标时刻已经过了**，且 `gen_state` 里记的最后一次成功
/// 早于「今天的目标时刻」。
///
/// 为什么这样判而不是「距离上次 >= 24h」：
/// 后者在服务停机 3 天后只会跑一次，而这里会**补跑**（每天一次）。
pub fn due_daily(db: &Db, now_ms: i64, hhmm: &str, last_ok_ms: Option<i64>) -> bool {
    let Some((h, m)) = crate::generate::parse_hhmm(hhmm) else {
        return false;
    };
    let Some((today, _)) = local_date(db, now_ms) else {
        return false;
    };
    let target = local_at(db, today, h, m);
    if now_ms < target {
        return false; // 今天还没到点
    }
    match last_ok_ms {
        None => true,
        Some(last) => last < target,
    }
}

/// 现在是不是该跑「每周」任务了？
///
/// 与 `due_daily` 同构，只是目标时刻是「本周 `weekday` 那天的 `hhmm`」。
pub fn due_weekly(db: &Db, now_ms: i64, weekday: u32, hhmm: &str, last_ok_ms: Option<i64>) -> bool {
    let Some((h, m)) = crate::generate::parse_hhmm(hhmm) else {
        return false;
    };
    let Some((today, cur_wd)) = local_date(db, now_ms) else {
        return false;
    };
    // 回退到本周的那个星期几（1=周一）
    let back = (cur_wd as i64 - weekday.clamp(1, 7) as i64).max(0);
    let days = days_from_civil(today.0, today.1, today.2) - back;
    let (y, mo, d) = civil_from_days(days);
    let target = local_at(db, (y, mo, d), h, m);
    if now_ms < target {
        return false;
    }
    match last_ok_ms {
        None => true,
        Some(last) => last < target,
    }
}

/// 本地时区相对 UTC 的偏移（毫秒），**正数表示东区**。
///
/// 为什么不引 `chrono`/`time` crate：整个项目只有一个地方需要时区
/// （就是这里），而 SQLite 已经能算 `localtime` —— 为一个函数引一个
/// 带自己时区数据库的库，收益不抵依赖成本。
///
/// 做法：让 SQLite 把「现在」同时按 UTC 和 localtime 格式化，两者相减。
fn local_offset_ms(db: &Db, now_ms: i64) -> i64 {
    let r: Option<(String, String)> = db
        .with_read(|c| {
            Ok(c.query_row(
                "SELECT datetime(?1/1000,'unixepoch'), datetime(?1/1000,'unixepoch','localtime')",
                rusqlite::params![now_ms],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok())
        })
        .ok()
        .flatten();
    let Some((utc, local)) = r else {
        return 0;
    };
    // 两个都形如 "2026-11-01 09:39:00"，直接按秒差算
    match (parse_sqlite_dt(&utc), parse_sqlite_dt(&local)) {
        (Some(u), Some(l)) => (l - u) * 1000,
        _ => 0,
    }
}

/// 把 `YYYY-MM-DD HH:MM:SS` 解析成 Unix 秒（按 UTC 解释）。
fn parse_sqlite_dt(s: &str) -> Option<i64> {
    let (d, t) = s.split_once(' ')?;
    let mut dp = d.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let da: i64 = dp.next()?.parse().ok()?;
    let mut tp = t.split(':');
    let h: i64 = tp.next()?.parse().ok()?;
    let mi: i64 = tp.next()?.parse().ok()?;
    let se: i64 = tp.next().unwrap_or("0").parse().ok()?;
    Some(days_from_civil(y, mo, da) * 86400 + h * 3600 + mi * 60 + se)
}

/// 公历年月日 → 距 1970-01-01 的天数（Howard Hinnant 的算法）。
///
/// 手写而不是用库：就这一个公式，且它是**纯算术**，可测。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// 天数的逆运算：(年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 某一刻的**本地日期**＋星期几（1=周一…7=周日）。
fn local_date(db: &Db, ms: i64) -> Option<((i64, i64, i64), u32)> {
    let local = ms + local_offset_ms(db, ms);
    let days = local.div_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    // 1970-01-01 是周四 = 4（1=周一）
    let wd = ((days + 3).rem_euclid(7) + 1) as u32;
    Some(((y, m, d), wd))
}

/// 本地某天的 `h:m` 对应的 UTC 毫秒。
fn local_at(db: &Db, date: (i64, i64, i64), h: u32, m: u32) -> i64 {
    let days = days_from_civil(date.0, date.1, date.2);
    let secs = days * 86400 + h as i64 * 3600 + m as i64 * 60;
    // offset 随日期变（夏令时），用目标日期的正午去查更稳
    let probe = (secs + 12 * 3600) * 1000;
    secs * 1000 - local_offset_ms(db, probe)
}

#[cfg(test)]
mod time_tests {
    use super::{civil_from_days, days_from_civil};

    #[test]
    fn 日期与天数互转往返() {
        for (y, m, d) in [
            (1970, 1, 1),
            (2000, 2, 29),
            (2026, 11, 1),
            (2024, 12, 31),
            (1900, 3, 1),
        ] {
            let n = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(n), (y, m, d), "{y}-{m}-{d}");
        }
    }

    #[test]
    fn 纪元起点() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn 已知星期() {
        // 2026-11-01 是周日：1970-01-01(周四) 起算
        let n = days_from_civil(2026, 11, 1);
        let wd = ((n + 3).rem_euclid(7) + 1) as u32;
        assert_eq!(wd, 7, "2026-11-01 应是周日");
    }

    #[test]
    fn 解析_sqlite_时间() {
        assert_eq!(super::parse_sqlite_dt("1970-01-01 00:00:00"), Some(0));
        assert_eq!(
            super::parse_sqlite_dt("2026-11-01 00:00:00"),
            Some(1793491200)
        );
        assert_eq!(super::parse_sqlite_dt("坏格式"), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个临时库（这些判据要真跑 SQL 的 localtime 与 gen_state）。
    fn tmp_db() -> (std::sync::Arc<Db>, tempdir::TempDirGuard) {
        let dir = tempdir::TempDirGuard::new();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        (db, dir)
    }

    #[test]
    fn 到点且没跑过就要跑() {
        let (db, _g) = tmp_db();
        let now = crate::db::now_ms();
        // 用「现在」做目标时刻 —— 一定已经到点
        let (h, m) = local_hhmm(&db, now);
        assert!(due_daily(
            &db,
            now + 60_000,
            &format!("{h:02}:{m:02}"),
            None
        ));
        // 目标在未来两小时 → 不该跑
        let (fh, fm) = ((h + 2) % 24, m);
        if fh > h {
            assert!(!due_daily(&db, now, &format!("{fh:02}:{fm:02}"), None));
        }
    }

    #[test]
    fn 今天已经跑过就不跑() {
        let (db, _g) = tmp_db();
        let now = crate::db::now_ms();
        let (h, m) = local_hhmm(&db, now);
        let hhmm = format!("{h:02}:{m:02}");
        // 上次成功 = 刚刚（在目标时刻之后）→ 今天不用再跑
        assert!(!due_daily(&db, now + 60_000, &hhmm, Some(now)));
        // 上次成功 = 一天前 → 要跑
        assert!(due_daily(&db, now + 60_000, &hhmm, Some(now - 86_400_000)));
    }

    #[test]
    fn 停机几天后会补跑一次() {
        let (db, _g) = tmp_db();
        let now = crate::db::now_ms();
        let (h, m) = local_hhmm(&db, now);
        // 服务停了两天：上次成功在两天前 → 必须补
        assert!(due_daily(
            &db,
            now,
            &format!("{h:02}:{m:02}"),
            Some(now - 2 * 86_400_000)
        ));
    }

    #[test]
    fn 时间格式坏就不跑() {
        let (db, _g) = tmp_db();
        let now = crate::db::now_ms();
        assert!(!due_daily(&db, now, "", None), "空 = 关");
        assert!(!due_daily(&db, now, "bad", None));
        assert!(!due_daily(&db, now, "25:00", None));
    }

    #[test]
    fn 周记按星期判() {
        let (db, _g) = tmp_db();
        let now = crate::db::now_ms();
        let ((y, mo, d), wd) = local_date(&db, now).unwrap();
        let (h, m) = local_hhmm(&db, now);
        // 把目标设成「今天这一天的某个已过去的时刻」→ 本周那天已过
        let hhmm = format!("{h:02}:{m:02}");
        // 今天这一刻之后 1 分钟
        let just_after = local_at(&db, (y, mo, d), h, m) + 60_000;
        assert!(due_weekly(&db, just_after, wd, &hhmm, None));
        // 已经跑过 → 不跑
        assert!(!due_weekly(
            &db,
            just_after,
            wd,
            &hhmm,
            Some(just_after - 1000)
        ));
    }

    #[test]
    fn 周记目标时刻还没到就不跑() {
        let (db, _g) = tmp_db();
        let now = crate::db::now_ms();
        let ((y, mo, d), wd) = local_date(&db, now).unwrap();
        let (h, m) = local_hhmm(&db, now);
        // 目标 = 今天 h+2（如果不会跨天），此刻还没到
        if h + 2 <= 23 {
            let future = local_at(&db, (y, mo, d), h + 2, m);
            assert!(
                !due_weekly(&db, now, wd, &format!("{:02}:{:02}", h + 2, m), None) || now >= future
            );
        }
    }

    #[test]
    fn 生成记录能写能读() {
        let (db, _g) = tmp_db();
        log(
            &db,
            Record {
                purpose: Purpose::Diary,
                ok: true,
                trigger: "schedule",
                model: "m1",
                prompt_tokens: 100,
                completion_tokens: 50,
                elapsed_ms: 1234,
                result: Some("写了 300 字".into()),
                error: None,
            },
        );
        log(
            &db,
            Record {
                purpose: Purpose::Diary,
                ok: false,
                trigger: "schedule",
                model: "m1",
                prompt_tokens: 0,
                completion_tokens: 0,
                elapsed_ms: 500,
                result: None,
                error: Some("401 鉴权失败".into()),
            },
        );
        let rows = recent(&db, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(!rows[0].ok, "最新的（失败）在前");
        assert!(rows[1].ok);
        assert_eq!(rows[0].error.as_deref(), Some("401 鉴权失败"));
    }

    #[test]
    fn 连续失败计数() {
        let (db, _g) = tmp_db();
        for _ in 0..3 {
            log(
                &db,
                Record {
                    purpose: Purpose::Diary,
                    ok: false,
                    trigger: "schedule",
                    model: "m",
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    elapsed_ms: 1,
                    result: None,
                    error: Some("x".into()),
                },
            );
        }
        assert_eq!(consecutive_failures(&db, Purpose::Diary), 3);
        // 成功一次后清零
        log(
            &db,
            Record {
                purpose: Purpose::Diary,
                ok: true,
                trigger: "schedule",
                model: "m",
                prompt_tokens: 1,
                completion_tokens: 1,
                elapsed_ms: 1,
                result: Some("ok".into()),
                error: None,
            },
        );
        assert_eq!(consecutive_failures(&db, Purpose::Diary), 0);
    }

    #[test]
    fn 状态水位线读写() {
        let (db, _g) = tmp_db();
        // 先确认表真的建出来了（迁移没跑到就会静默失败）
        let n: i64 = db
            .with_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM gen_state", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 0, "gen_state 表必须存在");
        assert!(state_get(&db, "diary").is_none());
        state_set(&db, "diary", 12345);
        assert_eq!(state_get(&db, "diary"), Some(12345));
        state_set(&db, "diary", 99999);
        assert_eq!(state_get(&db, "diary"), Some(99999), "要能覆盖");
        // 不同 key 互不影响
        state_set(&db, "weekly", 1);
        assert_eq!(state_get(&db, "diary"), Some(99999));
    }

    /// 本地时区的 `(时, 分)`。
    fn local_hhmm(db: &Db, ms: i64) -> (u32, u32) {
        let off = local_offset_ms(db, ms);
        let local = ms + off;
        let secs = local.div_euclid(1000).rem_euclid(86400);
        ((secs / 3600) as u32, ((secs % 3600) / 60) as u32)
    }
}

/// 测试用临时目录（Drop 时删）。
#[cfg(test)]
mod tempdir {
    use std::path::{Path, PathBuf};

    pub struct TempDirGuard(PathBuf);
    impl TempDirGuard {
        pub fn new() -> Self {
            // **必须带计数器**：只用 now_ms() 的话，同一毫秒里创建两个临时库
            // 会指向同一个目录 —— 并行测试互相踩，症状是「单跑过、一起跑挂」。
            // 这类 flaky 最费时间，宁可用个静态计数器。
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "hubtest-{}-{}-{}",
                std::process::id(),
                crate::db::now_ms(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
