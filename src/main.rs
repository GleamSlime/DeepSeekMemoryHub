//! 蓝鲸记忆中枢 — 启动入口。
//!
//! 子命令：
//!   serve                启动 HTTP 服务（默认）
//!   snapshot             立即拍一份快照
//!   rollback --to <ms>   回滚到指定时刻
//!   undo --id <n>        撤销某次回滚
//!   rollback-history     列出回滚历史
//!   doctor               检查数据目录与数据库健康

use std::sync::Arc;

use bluewhale_memory::config::Config;
use bluewhale_memory::db::{now_ms, Db};
use bluewhale_memory::http::AppState;
use bluewhale_memory::snapshot::SnapshotStore;
use bluewhale_memory::{rollback, APP_NAME, VERSION};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,hyper=warn".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("serve");

    let result = match cmd {
        "serve" => serve().await,
        "snapshot" => cli_snapshot(&args),
        "rollback" => cli_rollback(&args),
        "undo" => cli_undo(&args),
        "rollback-history" => cli_history(),
        "doctor" => cli_doctor(),
        "migrate" => cli_migrate(&args),
        "--version" | "-V" => {
            println!(
                "{APP_NAME} {VERSION} (schema {})",
                bluewhale_memory::db::SCHEMA_VERSION
            );
            Ok(())
        }
        "--help" | "-h" | "help" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("未知命令：{other}\n");
            print_help();
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("错误：{e}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "{APP_NAME} {VERSION}
跨渠道 AI 记忆服务

用法：
  bluewhale-memory [命令]

命令：
  serve                       启动 HTTP 服务（默认）
  snapshot [--note <说明>]    立即拍一份快照
  rollback --to <毫秒时间戳>   回滚：忘掉该时刻之后的记忆
  undo --id <回滚id>          撤销某次回滚
  rollback-history            列出最近的回滚
  doctor                      检查数据目录与数据库健康
  migrate --from <旧库路径>    从 MC 插件旧库迁移存量数据（SQLite）
          [--plans <目录>]     同时迁规划（plans/ 下的 JSON 文件）
          [--dry-run]          只看会迁多少，不写入

环境变量：
  BLUEWHALE_BIND               loopback | lan | custom（默认 lan）
  BLUEWHALE_BIND_ADDR          自定义监听 IP
  BLUEWHALE_PORT               监听端口（默认 25590）
  BLUEWHALE_DATA_DIR           数据目录
  BLUEWHALE_TOKEN              鉴权 token（留空 = 只听回环）
  BLUEWHALE_SNAPSHOT_KEEP_DAYS 快照保留天数
  BLUEWHALE_MAX_CONCURRENCY    并发上限

安全约定：
  没有 token 时强制只听回环，即使 BLUEWHALE_BIND=lan。
  不给「开着但不要密码」这个状态留缝隙。"
    );
}

fn load_config() -> bluewhale_memory::Result<Config> {
    let mut cfg = Config::from_env()?;
    cfg.normalize_paths();
    Ok(cfg)
}

async fn serve() -> bluewhale_memory::Result<()> {
    let cfg = Arc::new(load_config()?);
    let db = Db::open(cfg.db_path())?;
    let state = AppState::new(db.clone(), cfg.clone())?;

    let addr = cfg.effective_bind();
    tracing::info!(
        "{} {} 启动 | schema={} | 数据={}",
        APP_NAME,
        VERSION,
        bluewhale_memory::db::SCHEMA_VERSION,
        cfg.data_dir.display()
    );
    if cfg.api_exposed_without_token() {
        tracing::warn!(
            "⚠ 监听在 {addr} 但**没有配置 token**：局域网内只能访问 /v1/health，\
             其余 API 一律 401。要让内网调用 API，请设置 BLUEWHALE_TOKEN（至少 16 字符）。"
        );
    } else if cfg.token.is_empty() {
        tracing::info!("监听 {addr}（未配置 token，仅本机可用）");
    } else {
        tracing::info!("监听 {addr}（token 已启用）");
    }

    spawn_daily_snapshot(db.clone(), cfg.clone());

    // 知识库回填。embedder 为 None 时这个函数直接返回（不启任务）——
    // 没配嵌入服务的部署与加这个功能之前**完全一致**：不探测、不报错。
    bluewhale_memory::kb::backfill::spawn(
        db.clone(),
        state.embedder.clone(),
        cfg.kb.backfill_seconds,
    );

    // 路由组装统一走 full_router：测试与线上必须是同一份，
    // 否则会出现「测试全绿但线上接口不存在」。
    let app = bluewhale_memory::http::full_router(state.clone());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("已就绪：http://{addr}/v1/health");

    // 必须用 into_make_service_with_connect_info 才能拿到对端地址，
    // api_auth_mw 靠它判断请求是不是来自本机。
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    tracing::info!("已停止");
    Ok(())
}

/// 每天至少一份快照。每小时检查，`ensure_daily` 保证同一天只拍一份。
fn spawn_daily_snapshot(db: Arc<Db>, cfg: Arc<Config>) {
    tokio::spawn(async move {
        let store = match SnapshotStore::new(cfg.snapshot_dir()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("快照目录不可用：{e}");
                return;
            }
        };
        let interval = std::time::Duration::from_secs(cfg.snapshot_check_seconds.max(60));
        loop {
            let (db2, store2, keep) = (db.clone(), store.clone(), cfg.snapshot_keep_days);
            let r =
                tokio::task::spawn_blocking(move || -> bluewhale_memory::Result<(bool, usize)> {
                    let (_info, created) = store2.ensure_daily(&db2)?;
                    let pruned = store2.prune(&db2, keep)?;
                    Ok((created, pruned))
                })
                .await;
            match r {
                Ok(Ok((created, pruned))) => {
                    if created {
                        tracing::info!("今日快照已生成");
                    }
                    if pruned > 0 {
                        tracing::info!("清理过期快照 {pruned} 份");
                    }
                }
                Ok(Err(e)) => tracing::error!("快照任务失败：{e}"),
                Err(e) => tracing::error!("快照任务 panic：{e}"),
            }
            tokio::time::sleep(interval).await;
        }
    });
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("收到 Ctrl-C，正在停止"),
        _ = term => tracing::info!("收到 SIGTERM，正在停止"),
    }
}

// ------------------------------------------------------------------ CLI

fn open_db() -> bluewhale_memory::Result<(Arc<Db>, Config)> {
    let cfg = load_config()?;
    let db = Db::open(cfg.db_path())?;
    Ok((db, cfg))
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn cli_snapshot(args: &[String]) -> bluewhale_memory::Result<()> {
    let note = flag(args, "--note");
    let (db, cfg) = open_db()?;
    let store = SnapshotStore::new(cfg.snapshot_dir())?;
    let info = store.take(&db, note)?;
    println!("快照已生成：");
    println!("  文件   {}", info.path);
    println!("  大小   {} 字节", info.bytes);
    println!("  行数   {}", info.rows);
    println!("  sha256 {}", info.sha256);
    Ok(())
}

fn cli_rollback(args: &[String]) -> bluewhale_memory::Result<()> {
    let to = flag(args, "--to")
        .ok_or_else(|| bluewhale_memory::Error::BadRequest("需要 --to <毫秒时间戳>".into()))?;
    let target: i64 = to
        .parse()
        .map_err(|_| bluewhale_memory::Error::BadRequest(format!("--to 不是数字：{to}")))?;

    let (db, _cfg) = open_db()?;
    let out = rollback::rollback_to(&db, target, Some("命令行回滚"))?;
    println!("已回滚到 {}（{}）", fmt_ms(out.target_ms), out.target_ms);
    println!(
        "回滚 id {} —— 可用 `undo --id {}` 撤销",
        out.rollback_id, out.rollback_id
    );
    for a in &out.affected {
        println!("  {:<20} {}", a.table, a.rows);
    }
    println!("共遗忘 {} 行（逻辑删除，未物理清理）", out.total);
    Ok(())
}

fn cli_undo(args: &[String]) -> bluewhale_memory::Result<()> {
    let id = flag(args, "--id")
        .ok_or_else(|| bluewhale_memory::Error::BadRequest("需要 --id <回滚id>".into()))?;
    let id: i64 = id
        .parse()
        .map_err(|_| bluewhale_memory::Error::BadRequest(format!("--id 不是数字：{id}")))?;
    let (db, _cfg) = open_db()?;
    let out = rollback::undo(&db, id)?;
    println!("已撤销回滚 {id}，恢复 {} 行", out.total);
    Ok(())
}

fn cli_history() -> bluewhale_memory::Result<()> {
    let (db, _cfg) = open_db()?;
    let list = rollback::history(&db, 50)?;
    if list.is_empty() {
        println!("没有回滚记录");
        return Ok(());
    }
    println!(
        "{:<6} {:<22} {:<22} {:<8} 说明",
        "id", "发生时间", "回滚到", "模式"
    );
    for r in list {
        println!(
            "{:<6} {:<22} {:<22} {:<8} {}",
            r.id,
            fmt_ms(r.at_ms),
            fmt_ms(r.target_ms),
            r.mode,
            r.note.unwrap_or_default()
        );
    }
    Ok(())
}

fn cli_doctor() -> bluewhale_memory::Result<()> {
    let cfg = load_config()?;
    println!("{APP_NAME} {VERSION}");
    println!("  数据目录   {}", cfg.data_dir.display());
    println!("  数据库     {}", cfg.db_path().display());
    println!("  快照目录   {}", cfg.snapshot_dir().display());
    println!("  监听       {}", cfg.effective_bind());
    if cfg.api_exposed_without_token() {
        println!("  ⚠ 未配置 token：局域网只能访问 /v1/health，其余 API 会被拒");
    }

    let db = Db::open(cfg.db_path())?;
    println!("\n表行数：");
    for (t, n) in db.counts()? {
        println!("  {:<20} {n}", t);
    }
    println!("\nWAL 大小   {} 字节", db.wal_size());
    println!("被遗忘未清理 {} 行", rollback::forgotten_count(&db)?);

    let store = SnapshotStore::new(cfg.snapshot_dir())?;
    match store.latest(&db)? {
        Some(s) => {
            let ok = store.verify(&s).unwrap_or(false);
            println!(
                "最近快照   {}（{}，{}）",
                fmt_ms(s.taken_ms),
                if ok {
                    "校验通过"
                } else {
                    "⚠ 校验失败"
                },
                human_bytes(s.bytes)
            );
        }
        None => println!("最近快照   ⚠ 还没有任何快照"),
    }

    println!("\n现在      {}", fmt_ms(now_ms()));
    Ok(())
}

fn cli_migrate(args: &[String]) -> bluewhale_memory::Result<()> {
    let from = flag(args, "--from").ok_or_else(|| {
        bluewhale_memory::Error::BadRequest(
            "需要 --from <旧库路径>\n例如：migrate --from ~/MC_AI/shared/deepseek_mc/memory.db"
                .into(),
        )
    })?;
    let dry = args.iter().any(|a| a == "--dry-run");

    let cfg = load_config()?;
    let dst = Db::open(cfg.db_path())?;

    if dry {
        println!("== 演练模式（不写入）==");
    } else {
        println!("== 开始迁移 ==");
        // 迁移前先拍一份快照：万一迁错了能回到迁移前。
        let store = SnapshotStore::new(cfg.snapshot_dir())?;
        match store.take(&dst, Some("迁移前")) {
            Ok(i) => println!("  已建快照：{}（{} 字节）", i.path, i.bytes),
            Err(e) => {
                println!("  ⚠ 快照失败（{}），仍继续迁移", e);
            }
        }
    }

    println!("  源库  {}", from);
    println!("  目标  {}", cfg.db_path().display());
    println!();

    let src_path = std::path::PathBuf::from(expand_tilde(from));

    let mut m = bluewhale_memory::migrate::from_legacy(&src_path, &dst, dry)?;

    // 规划不在 SQLite 里，是独立 JSON 文件。之前只迁了库，
    // 结果规划一条都没过来——那是「它一天做什么」的依据，不能漏。
    if let Some(plans_dir) = flag(args, "--plans") {
        let pdir = std::path::PathBuf::from(expand_tilde(plans_dir));
        println!("  规划  {}", pdir.display());
        match bluewhale_memory::migrate::from_legacy_plans(&pdir, &dst, dry) {
            Ok(pm) => {
                m.plans = pm.plans;
                m.plan_items = pm.plan_items;
                m.skipped += pm.skipped;
            }
            Err(e) => {
                println!("  ⚠ 规划迁移失败：{e}（数据库迁移已完成）");
            }
        }
    }

    println!("{:<16} 行数", "表");
    println!("{:<16} {}", "event", m.events);
    println!("{:<16} {}", "journal", m.journals);
    println!("{:<16} {}", "diary", m.diaries);
    println!("{:<16} {}", "fact", m.facts);
    println!("{:<16} {}", "self_narrative", m.self_narratives);
    println!("{:<16} {}", "mood", m.moods);
    println!("{:<16} {}", "trait", m.traits);
    println!("{:<16} {}", "relation", m.relations);
    println!("{:<16} {}", "promise", m.promises);
    println!("{:<16} {}", "channel_message", m.messages);
    println!("{:<16} {}（{} 个条目）", "plan", m.plans, m.plan_items);
    println!("{:<16} {}", "—— 合计", m.total());
    if m.skipped > 0 {
        println!("{:<16} {}（已存在，跳过）", "skip", m.skipped);
    }

    if dry {
        println!("\n演练结束。去掉 --dry-run 才会真写。");
    } else {
        println!("\n迁移完成。可用 doctor 体检，或用 rollback 撤销到迁移前的快照。");
    }
    Ok(())
}

fn expand_tilde(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME")
            .map(|h| format!("{h}/{rest}"))
            .unwrap_or_else(|_| p.to_string()),
        None => p.to_string(),
    }
}

fn fmt_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}Z")
}

/// Howard Hinnant 的 civil_from_days 算法。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn human_bytes(n: i64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 日期换算正确() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11017), (2000, 3, 1));
        // 用 Python 的 datetime 独立核对过：2026-10-27 距 1970-01-01 是 20753 天。
        // 之前这里写死成 20743（= 2026-10-17），是测试数据错了，不是算法错了。
        assert_eq!(civil_from_days(20753), (2026, 10, 27));
        assert_eq!(civil_from_days(20743), (2026, 10, 17));
    }

    #[test]
    fn 时间格式化形如_iso() {
        assert_eq!(fmt_ms(0), "1970-01-01 00:00:00Z");
    }

    #[test]
    fn 字节数人类可读() {
        assert_eq!(human_bytes(512), "512.0 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
    }
}
