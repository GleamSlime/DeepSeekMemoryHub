//! AI 调用统计：调了哪些模型、1/7/30 天烧了多少 token。
//!
//! # 数据来源
//!
//! `generation_log` —— 每次调 LLM 都有一行（成功也留），带
//! `model` / `prompt_tokens` / `completion_tokens` / `elapsed_ms` / `ok`。
//!
//! # 与 DSH `local-usage` 的对应
//!
//! DSH 的采样点是「每一轮请求」，字段是
//! `route`（模型）/ `tokens{uncachedInput, output, cacheRead, cacheWrite}` /
//! `durationMs` / `outputTps`。这里只保留**能真实拿到**的部分：
//!
//! | DSH 字段 | 这里 | 说明 |
//! | --- | --- | --- |
//! | `route` | `model` | 调的是哪个模型 |
//! | `uncachedInputTokens` | `prompt_tokens` | OpenAI 兼容接口只给总输入 |
//! | `outputTokens` | `completion_tokens` | |
//! | `cacheRead/WriteTokens` | **没有** | 兼容网关普遍不返回缓存明细，编不出来 |
//! | `durationMs` | `elapsed_ms` | |
//! | `outputTps` | 现算 | `completion_tokens / (elapsed_ms/1000)` |
//!
//! 不硬凑成 DSH 的形状是有意为之：**给出不存在的缓存字段比不给更糟** ——
//! 面板上出现一列永远为 0 的数字，人会以为「缓存没生效」。

use serde::Serialize;

use crate::db::Db;
use crate::error::Result;

/// 一个时间窗口的汇总。
#[derive(Debug, Default, Serialize)]
pub struct Window {
    pub calls: i64,
    pub ok: i64,
    pub failed: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    /// 平均耗时（毫秒）。0 表示没有成功调用。
    pub avg_ms: i64,
    /// 输出速度（token/秒），只有成功调用才有意义。
    pub output_tps: f64,
}

/// 按模型分组的统计。
#[derive(Debug, Default, Serialize)]
pub struct ByModel {
    pub model: String,
    pub w1: Window,
    pub w7: Window,
    pub w30: Window,
    /// 最近一次调用是什么时候（判断这个模型还在用没有）。
    pub last_ms: Option<i64>,
}

/// 全量统计。
#[derive(Debug, Default, Serialize)]
pub struct Stats {
    pub overall: ByModel,
    pub by_model: Vec<ByModel>,
    /// 按用途分（tick / diary / weekly）的 30 天汇总。
    pub by_purpose: Vec<PurposeRow>,
    /// 最近若干条调用（面板上给个明细表）。
    pub recent: Vec<RecentRow>,
    /// 数据从什么时候开始有（面板提示「统计只覆盖 X 之后」）。
    pub first_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct PurposeRow {
    pub purpose: String,
    pub w30: Window,
}

#[derive(Debug, Serialize)]
pub struct RecentRow {
    pub at_ms: i64,
    pub purpose: String,
    pub model: String,
    pub ok: bool,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub elapsed_ms: i64,
    pub trigger: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
}

const DAY: i64 = 86_400_000;

/// 查三个窗口的统计。
pub fn load(db: &Db, now_ms: i64, recent: usize) -> Result<Stats> {
    let w1 = now_ms - DAY;
    let w7 = now_ms - 7 * DAY;
    let w30 = now_ms - 30 * DAY;

    db.with_read(|c| {
        // ---- 按模型聚合：先取出最近 30 天出现过哪些模型，
        //      再逐个算三个窗口（一次性 GROUP BY 写不出「三个窗口」）
        let models: Vec<String> = {
            let mut s2 = c.prepare(
                "SELECT DISTINCT COALESCE(model,'(未知)') FROM generation_log
                 WHERE at_ms > ?1 ORDER BY 1",
            )?;
            let v = s2
                .query_map(rusqlite::params![w30], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            v
        };

        let mut by_model = Vec::new();
        for m in models {
            let row = ByModel {
                model: m.clone(),
                w1: window(c, &m, w1, now_ms)?,
                w7: window(c, &m, w7, now_ms)?,
                w30: window(c, &m, w30, now_ms)?,
                last_ms: c
                    .query_row(
                        "SELECT MAX(at_ms) FROM generation_log WHERE COALESCE(model,'(未知)') = ?1",
                        rusqlite::params![&m],
                        |r| r.get::<_, Option<i64>>(0),
                    )
                    .ok()
                    .flatten(),
            };
            by_model.push(row);
        }
        // 总用量大的排前面（面板默认顺序就是「谁最费钱」）
        by_model.sort_by_key(|m| std::cmp::Reverse(m.w30.total_tokens));

        let mut overall = ByModel {
            model: "（全部）".into(),
            ..Default::default()
        };
        for m in &by_model {
            acc(&mut overall.w1, &m.w1);
            acc(&mut overall.w7, &m.w7);
            acc(&mut overall.w30, &m.w30);
            overall.last_ms = overall.last_ms.max(m.last_ms);
        }

        // ---- 按用途（30 天）
        let mut by_purpose = Vec::new();
        for p in ["tick", "diary", "weekly"] {
            let mut s3 = c.prepare(
                "SELECT ok, prompt_tokens, completion_tokens, elapsed_ms
                 FROM generation_log
                 WHERE purpose = ?1 AND at_ms > ?2 AND prompt_tokens > 0",
            )?;
            let rows = s3
                .query_map(rusqlite::params![p, w30], |r| {
                    Ok((
                        r.get::<_, i64>(0)? != 0,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut w = Window::default();
            for (ok, pt, ct, ms) in rows {
                w.calls += 1;
                if ok {
                    w.ok += 1;
                    w.prompt_tokens += pt;
                    w.completion_tokens += ct;
                    w.total_tokens += pt + ct;
                    w.avg_ms += ms;
                } else {
                    w.failed += 1;
                }
            }
            if w.ok > 0 {
                w.avg_ms /= w.ok;
                if w.avg_ms > 0 {
                    w.output_tps = w.completion_tokens as f64 / (w.avg_ms as f64 / 1000.0);
                }
            }
            by_purpose.push(PurposeRow {
                purpose: p.to_string(),
                w30: w,
            });
        }

        // ---- 明细
        let mut s4 = c.prepare(
            "SELECT at_ms, purpose, COALESCE(model,''), ok, prompt_tokens,
                    completion_tokens, elapsed_ms, trigger, result, error
             FROM generation_log ORDER BY id DESC LIMIT ?1",
        )?;
        let recent_rows = s4
            .query_map(rusqlite::params![recent.clamp(1, 200) as i64], |r| {
                Ok(RecentRow {
                    at_ms: r.get(0)?,
                    purpose: r.get(1)?,
                    model: r.get(2)?,
                    ok: r.get::<_, i64>(3)? != 0,
                    prompt_tokens: r.get(4)?,
                    completion_tokens: r.get(5)?,
                    elapsed_ms: r.get(6)?,
                    trigger: r.get(7)?,
                    result: r.get(8)?,
                    error: r.get(9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let first_ms: Option<i64> = c
            .query_row("SELECT MIN(at_ms) FROM generation_log", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .ok()
            .flatten();

        Ok(Stats {
            overall,
            by_model,
            by_purpose,
            recent: recent_rows,
            first_ms,
        })
    })
}

/// 单个模型在某个窗口里的用量。**只统计成功调用**：
/// 失败的那次没有产出，把它的 token 算进消耗会让人以为「花钱买了空气」。
fn window(c: &rusqlite::Connection, model: &str, from_ms: i64, to_ms: i64) -> Result<Window> {
    let mut s = c.prepare(
        "SELECT ok, prompt_tokens, completion_tokens, elapsed_ms
         FROM generation_log
         WHERE COALESCE(model,'(未知)') = ?1 AND at_ms > ?2 AND at_ms <= ?3",
    )?;
    let rows = s
        .query_map(rusqlite::params![model, from_ms, to_ms], |r| {
            Ok((
                r.get::<_, i64>(0)? != 0,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut w = Window::default();
    for (ok, pt, ct, ms) in rows {
        w.calls += 1;
        if ok {
            w.ok += 1;
            w.prompt_tokens += pt;
            w.completion_tokens += ct;
            w.total_tokens += pt + ct;
            w.avg_ms += ms;
        } else {
            w.failed += 1;
        }
    }
    if w.ok > 0 {
        w.avg_ms /= w.ok;
        if w.avg_ms > 0 {
            w.output_tps = w.completion_tokens as f64 / (w.avg_ms as f64 / 1000.0);
        }
    }
    Ok(w)
}

fn acc(into: &mut Window, from: &Window) {
    // ⚠️ `avg_ms` 是**平均值**，不能直接相加。
    // 先按 (均值 × 次数) 还原成总耗时累加，全部加完再除 ——
    // 实测踩过：直接 `into.avg_ms += from.avg_ms * from.ok` 然后每步除一次，
    // 三个模型合并后得到 4239ms（实际均值才 1000 出头）。
    let sum_before = into.avg_ms * into.ok;
    into.calls += from.calls;
    into.ok += from.ok;
    into.failed += from.failed;
    into.prompt_tokens += from.prompt_tokens;
    into.completion_tokens += from.completion_tokens;
    into.total_tokens += from.total_tokens;

    let sum_after = sum_before + from.avg_ms * from.ok;
    into.avg_ms = if into.ok > 0 { sum_after / into.ok } else { 0 };
    if into.avg_ms > 0 {
        into.output_tps = into.completion_tokens as f64 / (into.avg_ms as f64 / 1000.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::runner::tests_support::fresh_db;

    /// 一次测试用调用的描述（参数太多，打包成结构更易读）。
    struct L<'a> {
        at: i64,
        model: &'a str,
        ok: bool,
        pt: i64,
        ct: i64,
        ms: i64,
        purpose: &'a str,
    }

    fn add_log(db: &Db, l: L<'_>) {
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO generation_log(at_ms, purpose, ok, trigger, model,
                                            prompt_tokens, completion_tokens, elapsed_ms,
                                            result, error)
                 VALUES (?1,?2,?3,'t',?4,?5,?6,?7,NULL,NULL)",
                rusqlite::params![
                    l.at,
                    l.purpose,
                    if l.ok { 1 } else { 0 },
                    l.model,
                    l.pt,
                    l.ct,
                    l.ms
                ],
            )?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 三个窗口各算各的() {
        let (db, _g) = fresh_db();
        let now = 1_800_000_000_000i64;
        // 今天：100/50
        add_log(
            &db,
            L {
                at: now - 3_600_000,
                model: "m1",
                ok: true,
                pt: 100,
                ct: 50,
                ms: 1000,
                purpose: "tick",
            },
        );
        // 3 天前：200/100（在 7 天内，不在 1 天内）
        add_log(
            &db,
            L {
                at: now - 3 * DAY,
                model: "m1",
                ok: true,
                pt: 200,
                ct: 100,
                ms: 2000,
                purpose: "diary",
            },
        );
        // 20 天前：400/200（在 30 天内，不在 7 天内）
        add_log(
            &db,
            L {
                at: now - 20 * DAY,
                model: "m1",
                ok: true,
                pt: 400,
                ct: 200,
                ms: 4000,
                purpose: "weekly",
            },
        );

        let st = load(&db, now, 10).unwrap();
        let m = st.by_model.iter().find(|x| x.model == "m1").unwrap();
        assert_eq!(m.w1.total_tokens, 150, "1 天只有第一次");
        assert_eq!(m.w7.total_tokens, 450, "7 天含前两次");
        assert_eq!(m.w30.total_tokens, 1050, "30 天全含");
        assert_eq!(m.w30.calls, 3);
        assert_eq!(m.w30.ok, 3);
    }

    #[test]
    fn 失败调用不计入_token_消耗() {
        // 失败那次没有产出，把它的 token 算进消耗会让人以为「花钱买了空气」
        let (db, _g) = fresh_db();
        let now = 1_800_000_000_000i64;
        add_log(
            &db,
            L {
                at: now - 1000,
                model: "m1",
                ok: true,
                pt: 100,
                ct: 50,
                ms: 1000,
                purpose: "tick",
            },
        );
        add_log(
            &db,
            L {
                at: now - 500,
                model: "m1",
                ok: false,
                pt: 999,
                ct: 999,
                ms: 300,
                purpose: "tick",
            },
        );
        let st = load(&db, now, 10).unwrap();
        let m = st.by_model.iter().find(|x| x.model == "m1").unwrap();
        assert_eq!(m.w1.total_tokens, 150, "失败那次不加油耗");
        assert_eq!(m.w1.calls, 2, "但调用次数要算");
        assert_eq!(m.w1.failed, 1);
    }

    #[test]
    fn 按模型分开统计() {
        let (db, _g) = fresh_db();
        let now = 1_800_000_000_000i64;
        add_log(
            &db,
            L {
                at: now - 1000,
                model: "便宜模型",
                ok: true,
                pt: 100,
                ct: 50,
                ms: 500,
                purpose: "tick",
            },
        );
        add_log(
            &db,
            L {
                at: now - 900,
                model: "贵模型",
                ok: true,
                pt: 5000,
                ct: 900,
                ms: 9000,
                purpose: "diary",
            },
        );
        let st = load(&db, now, 10).unwrap();
        assert_eq!(st.by_model.len(), 2);
        // 用量大的排前面
        assert_eq!(st.by_model[0].model, "贵模型");
        assert_eq!(st.overall.w1.total_tokens, 6050, "汇总要合并所有模型");
    }

    #[test]
    fn 按用途分开统计() {
        let (db, _g) = fresh_db();
        let now = 1_800_000_000_000i64;
        add_log(
            &db,
            L {
                at: now - 1000,
                model: "m",
                ok: true,
                pt: 100,
                ct: 50,
                ms: 1000,
                purpose: "tick",
            },
        );
        add_log(
            &db,
            L {
                at: now - 900,
                model: "m",
                ok: true,
                pt: 200,
                ct: 100,
                ms: 2000,
                purpose: "tick",
            },
        );
        add_log(
            &db,
            L {
                at: now - 800,
                model: "m",
                ok: true,
                pt: 1000,
                ct: 500,
                ms: 8000,
                purpose: "diary",
            },
        );
        let st = load(&db, now, 10).unwrap();
        let tick = st.by_purpose.iter().find(|p| p.purpose == "tick").unwrap();
        assert_eq!(tick.w30.calls, 2);
        assert_eq!(tick.w30.total_tokens, 450);
        let diary = st.by_purpose.iter().find(|p| p.purpose == "diary").unwrap();
        assert_eq!(diary.w30.total_tokens, 1500);
    }

    #[test]
    fn 空库不炸() {
        let (db, _g) = fresh_db();
        let st = load(&db, 1_800_000_000_000, 10).unwrap();
        assert!(st.by_model.is_empty());
        assert_eq!(st.overall.w1.calls, 0);
        assert!(st.first_ms.is_none());
    }

    #[test]
    fn 输出速度算得对() {
        let (db, _g) = fresh_db();
        let now = 1_800_000_000_000i64;
        // 100 token 用了 2 秒 → 50 tok/s
        add_log(
            &db,
            L {
                at: now - 1000,
                model: "m",
                ok: true,
                pt: 10,
                ct: 100,
                ms: 2000,
                purpose: "tick",
            },
        );
        let st = load(&db, now, 10).unwrap();
        let m = st.by_model.iter().find(|x| x.model == "m").unwrap();
        assert!(
            (m.w1.output_tps - 50.0).abs() < 0.01,
            "实际 {}",
            m.w1.output_tps
        );
    }

    #[test]
    fn 合并多模型的均值是对的() {
        // 回归：`avg_ms` 是平均值不能相加。旧写法每合并一次就除一次，
        // 三个模型 5 次调用会得到 4239ms（真实均值 1396）。
        // 注意要用**次数不同**的数据才抓得到：等权重时新旧写法碰巧结果一样。
        let (db, _g) = fresh_db();
        let now = 1_800_000_000_000i64;
        // A 模型 1 次 1000ms；B 模型 3 次各 2000ms → 真均值 (1000+6000)/4 = 1750
        add_log(
            &db,
            L {
                at: now - 1000,
                model: "A",
                ok: true,
                pt: 10,
                ct: 10,
                ms: 1000,
                purpose: "tick",
            },
        );
        for i in 0..3 {
            add_log(
                &db,
                L {
                    at: now - 900 + i,
                    model: "B",
                    ok: true,
                    pt: 10,
                    ct: 10,
                    ms: 2000,
                    purpose: "tick",
                },
            );
        }
        let st = load(&db, now, 10).unwrap();
        assert_eq!(st.overall.w1.ok, 4);
        assert_eq!(st.overall.w1.avg_ms, 1750, "加权平均，不是简单相加");
    }

    #[test]
    fn 明细按新到旧() {
        let (db, _g) = fresh_db();
        let now = 1_800_000_000_000i64;
        add_log(
            &db,
            L {
                at: now - 5000,
                model: "m",
                ok: true,
                pt: 1,
                ct: 1,
                ms: 1,
                purpose: "tick",
            },
        );
        add_log(
            &db,
            L {
                at: now - 1000,
                model: "m",
                ok: true,
                pt: 2,
                ct: 2,
                ms: 1,
                purpose: "diary",
            },
        );
        let st = load(&db, now, 10).unwrap();
        assert_eq!(st.recent.len(), 2);
        assert_eq!(st.recent[0].purpose, "diary", "最新的在前");
    }
}
