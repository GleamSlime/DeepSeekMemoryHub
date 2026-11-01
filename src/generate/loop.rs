//! 后台循环：每 60 秒看一眼该不该做事。
//!
//! # 为什么是「轮询」而不是「事件驱动」
//!
//! 事件驱动（收到事件就唤醒）在**有节流**的前提下没有优势：
//! 节流器本来就允许最多等 10 分钟，早 30 秒发现毫无意义。
//! 而轮询更好推理 —— 只有一个循环、一个周期，不会因为漏发唤醒而永远不跑。
//!
//! 60 秒的周期是「定时任务精度」与「空转成本」的折中：
//! 日记定在 04:00，最多晚 1 分钟，可以接受。
//!
//! # 失败绝不退出循环
//!
//! 每次迭代的错都吞在内部并记账。循环退出 = 生成功能永久停摆，
//! 而那正是这个模块要解决的问题（MC 不开机就停）。

use std::sync::Arc;

use crate::db::Db;
use crate::generate::exec::TickState;
use crate::llm::Purpose;

/// 循环周期（秒）。
const TICK_SECONDS: u64 = 60;

/// 启动后台循环。
pub fn spawn(
    db: Arc<Db>,
    cfg: Arc<crate::config::Config>,
    llm: Arc<std::sync::RwLock<crate::llm::LlmConfig>>,
    gen_cfg: Arc<std::sync::RwLock<crate::generate::GenConfig>>,
    state: Arc<TickState>,
) {
    tokio::spawn(async move {
        // 启动后等 30 秒再开始：让 HTTP 先起来，别和启动抢资源
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        tracing::info!("生成任务循环已启动（每 {TICK_SECONDS}s 检查）");
        let mut iv = tokio::time::interval(std::time::Duration::from_secs(TICK_SECONDS));
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            step(&db, &cfg, &llm, &gen_cfg, &state).await;
        }
    });
}

/// 一次迭代：先看定时任务，再看 tick。
async fn step(
    db: &Arc<Db>,
    cfg: &Arc<crate::config::Config>,
    llm: &Arc<std::sync::RwLock<crate::llm::LlmConfig>>,
    gen_cfg: &Arc<std::sync::RwLock<crate::generate::GenConfig>>,
    state: &Arc<TickState>,
) {
    let llm_now = match llm.read() {
        Ok(g) => g.clone(),
        Err(e) => e.into_inner().clone(),
    };
    // LLM 没配：什么都不做（也不记账，否则每分钟一行噪声）
    if !llm_now.is_ready() {
        return;
    }
    let gen = match gen_cfg.read() {
        Ok(g) => g.clone(),
        Err(e) => e.into_inner().clone(),
    };
    // 热配置合成一份 Config（不动全局）
    let mut c = (**cfg).clone();
    c.llm = llm_now;
    c.generate = gen.clone();

    // ---- 1. 被动 tick（内部自带节流判据）
    if let Err(e) = crate::generate::exec::run_tick(db, &c, state, false).await {
        tracing::warn!("tick 迭代失败：{e}");
    }

    // ---- 2. 每日/每周（内部自带「今天跑过没有」判据）
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    if let Err(e) = crate::generate::daily::maybe_run(db, &c, Purpose::Diary).await {
        tracing::warn!("日记失败：{e}");
    }
    if let Err(e) = crate::generate::daily::maybe_run(db, &c, Purpose::Weekly).await {
        tracing::warn!("周记失败：{e}");
    }
}
