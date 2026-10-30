//! 面板用的查询函数。
//!
//! 与 `http.rs` 里的 handler 分开：这里是**纯数据**，负责 SQL 与分页；
//! handler 只做参数校验与序列化。分开的好处是这些函数能被单元测试直接调用，
//! 不必起一个 HTTP 服务。

use serde::Serialize;

use crate::db::Db;
use crate::error::Result;

/// 分页参数。`page` 从 1 开始（人看到的页码），内部换算成 offset。
#[derive(Debug, Clone, Copy)]
pub struct Page {
    pub page: u32,
    pub size: u32,
}

impl Default for Page {
    fn default() -> Self {
        Self { page: 1, size: 50 }
    }
}

impl Page {
    /// 从查询参数构造，并把越界值夹到合理范围。
    ///
    /// `size` 上限 500：再大就不是「看一页」而是「导出」了，
    /// 导出应该走别的路径，不该把内存和响应时间放大。
    pub fn new(page: Option<u32>, size: Option<u32>) -> Self {
        Self {
            page: page.unwrap_or(1).max(1),
            size: size.unwrap_or(50).clamp(1, 500),
        }
    }

    pub fn offset(&self) -> i64 {
        ((self.page - 1) as i64) * self.size as i64
    }
}

/// 带总数的分页结果。前端要显示「第 1/20 页」，所以总数必须一起给。
#[derive(Debug, Serialize)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub page: u32,
    pub size: u32,
    pub pages: u32,
}

impl<T> Paged<T> {
    pub fn new(items: Vec<T>, total: i64, p: Page) -> Self {
        let pages = if total == 0 {
            0
        } else {
            ((total + p.size as i64 - 1) / p.size as i64) as u32
        };
        Self {
            items,
            total,
            page: p.page,
            size: p.size,
            pages,
        }
    }
}

// ------------------------------------------------------------------ 事件

#[derive(Debug, Serialize)]
pub struct EventItem {
    pub id: i64,
    pub at_ms: i64,
    pub kind: String,
    pub channel: String,
    pub summary: String,
    pub importance: f64,
    pub scope: String,
    pub person_id: Option<String>,
    pub channel_day: Option<String>,
    pub dedup_key: Option<String>,
}

/// 事件的过滤条件。所有字段都是可选的，空 = 不过滤。
#[derive(Debug, Default, Clone)]
pub struct EventFilter {
    pub channel: Option<String>,
    pub kind: Option<String>,
    pub q: Option<String>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// `true` 只看已遗忘的（回滚掉的），`false` 只看有效的。
    pub forgotten: bool,
}

pub fn events(db: &Db, f: &EventFilter, p: Page) -> Result<Paged<EventItem>> {
    let mut where_sql = String::from("WHERE 1=1");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    // 回滚掉的记录默认不出现，但要能查——运维需要「我回滚掉了什么」。
    if f.forgotten {
        where_sql.push_str(" AND deleted_by IS NOT NULL");
    } else {
        where_sql.push_str(" AND deleted_by IS NULL");
    }

    if let Some(c) = f.channel.as_ref().filter(|s| !s.is_empty()) {
        where_sql.push_str(" AND channel = ?");
        args.push(Box::new(c.clone()));
    }
    if let Some(k) = f.kind.as_ref().filter(|s| !s.is_empty()) {
        where_sql.push_str(" AND kind = ?");
        args.push(Box::new(k.clone()));
    }
    if let Some(q) = f.q.as_ref().filter(|s| !s.trim().is_empty()) {
        where_sql.push_str(" AND summary LIKE ?");
        args.push(Box::new(format!("%{}%", q.trim())));
    }
    if let Some(t) = f.from_ms {
        where_sql.push_str(" AND at_ms >= ?");
        args.push(Box::new(t));
    }
    if let Some(t) = f.to_ms {
        where_sql.push_str(" AND at_ms <= ?");
        args.push(Box::new(t));
    }

    db.with_read(|c| {
        let count_sql = format!("SELECT COUNT(*) FROM event {where_sql}");
        let total: i64 = c.query_row(
            &count_sql,
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
            |r| r.get(0),
        )?;

        let mut list_sql = format!(
            "SELECT id, at_ms, kind, channel, summary, importance, scope,
                    person_id, channel_day, dedup_key
             FROM event {where_sql} ORDER BY at_ms DESC LIMIT ? OFFSET ?"
        );
        let mut all: Vec<Box<dyn rusqlite::ToSql>> = args;
        all.push(Box::new(p.size as i64));
        all.push(Box::new(p.offset()));
        list_sql.push_str("");

        let mut stmt = c.prepare(&list_sql)?;
        let items = stmt
            .query_map(
                rusqlite::params_from_iter(all.iter().map(|b| b.as_ref())),
                |r| {
                    Ok(EventItem {
                        id: r.get(0)?,
                        at_ms: r.get(1)?,
                        kind: r.get(2)?,
                        channel: r.get(3)?,
                        summary: r.get(4)?,
                        importance: r.get(5)?,
                        scope: r.get(6)?,
                        person_id: r.get(7)?,
                        channel_day: r.get(8)?,
                        dedup_key: r.get(9)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Paged::new(items, total, p))
    })
}

// ------------------------------------------------------------------ 事实

#[derive(Debug, Serialize)]
pub struct FactItem {
    pub id: String,
    pub person_id: Option<String>,
    pub key: String,
    pub value: String,
    pub scope: String,
    pub status: String,
    pub tier: String,
    pub confidence: f64,
    pub importance: f64,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub expires_ms: Option<i64>,
    pub recall_count: i64,
    pub superseded_by: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct FactFilter {
    pub q: Option<String>,
    pub status: Option<String>,
    pub tier: Option<String>,
    /// `true` 只看被取代的历史版本。
    pub show_superseded: bool,
}

pub fn facts(db: &Db, f: &FactFilter, p: Page) -> Result<Paged<FactItem>> {
    let mut where_sql = String::from("WHERE deleted_by IS NULL");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    // 默认只列「当前有效」的事实：被取代的旧版本不占列表。
    // 但它们**没有删**——`status=superseded` 的筛选项能看到，
    // 这样「它以前是这么想的」仍然可查。
    if f.show_superseded {
        where_sql.push_str(" AND status = 'superseded'");
    } else if f.status.is_none() {
        where_sql.push_str(" AND superseded_by IS NULL AND status <> 'superseded'");
    }

    if let Some(s) = f.status.as_ref().filter(|s| !s.is_empty()) {
        where_sql.push_str(" AND status = ?");
        args.push(Box::new(s.clone()));
    }
    if let Some(t) = f.tier.as_ref().filter(|s| !s.is_empty()) {
        where_sql.push_str(" AND tier = ?");
        args.push(Box::new(t.clone()));
    }
    if let Some(q) = f.q.as_ref().filter(|s| !s.trim().is_empty()) {
        where_sql.push_str(" AND (key LIKE ? OR value LIKE ?)");
        let like = format!("%{}%", q.trim());
        args.push(Box::new(like.clone()));
        args.push(Box::new(like));
    }

    db.with_read(|c| {
        let total: i64 = c.query_row(
            &format!("SELECT COUNT(*) FROM fact {where_sql}"),
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
            |r| r.get(0),
        )?;

        let mut all: Vec<Box<dyn rusqlite::ToSql>> = args;
        all.push(Box::new(p.size as i64));
        all.push(Box::new(p.offset()));

        let mut stmt = c.prepare(&format!(
            "SELECT id, person_id, key, value, scope, status, tier,
                    confidence, importance, created_ms, updated_ms, expires_ms,
                    recall_count, superseded_by
             FROM fact {where_sql} ORDER BY updated_ms DESC LIMIT ? OFFSET ?"
        ))?;
        let items = stmt
            .query_map(
                rusqlite::params_from_iter(all.iter().map(|b| b.as_ref())),
                |r| {
                    Ok(FactItem {
                        id: r.get(0)?,
                        person_id: r.get(1)?,
                        key: r.get(2)?,
                        value: r.get(3)?,
                        scope: r.get(4)?,
                        status: r.get(5)?,
                        tier: r.get(6)?,
                        confidence: r.get(7)?,
                        importance: r.get(8)?,
                        created_ms: r.get(9)?,
                        updated_ms: r.get(10)?,
                        expires_ms: r.get(11)?,
                        recall_count: r.get(12)?,
                        superseded_by: r.get(13)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Paged::new(items, total, p))
    })
}

// ------------------------------------------------------------------ 手帐

#[derive(Debug, Serialize)]
pub struct JournalItem {
    pub id: String,
    pub at_ms: i64,
    pub type_: String,
    pub source: String,
    pub summary: String,
    pub details: Option<String>,
    pub importance: f64,
    pub confidence: f64,
    pub channel: String,
    pub planning_day_id: Option<String>,
}

pub fn journal(db: &Db, q: Option<&str>, p: Page) -> Result<Paged<JournalItem>> {
    let mut where_sql = String::from("WHERE deleted_by IS NULL");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(q) = q.filter(|s| !s.trim().is_empty()) {
        where_sql.push_str(" AND summary LIKE ?");
        args.push(Box::new(format!("%{}%", q.trim())));
    }

    db.with_read(|c| {
        let total: i64 = c.query_row(
            &format!("SELECT COUNT(*) FROM journal {where_sql}"),
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
            |r| r.get(0),
        )?;

        let mut all: Vec<Box<dyn rusqlite::ToSql>> = args;
        all.push(Box::new(p.size as i64));
        all.push(Box::new(p.offset()));

        let mut stmt = c.prepare(&format!(
            "SELECT id, at_ms, type, source, summary, details,
                    importance, confidence, channel, planning_day_id
             FROM journal {where_sql} ORDER BY at_ms DESC LIMIT ? OFFSET ?"
        ))?;
        let items = stmt
            .query_map(
                rusqlite::params_from_iter(all.iter().map(|b| b.as_ref())),
                |r| {
                    Ok(JournalItem {
                        id: r.get(0)?,
                        at_ms: r.get(1)?,
                        type_: r.get(2)?,
                        source: r.get(3)?,
                        summary: r.get(4)?,
                        details: r.get(5)?,
                        importance: r.get(6)?,
                        confidence: r.get(7)?,
                        channel: r.get(8)?,
                        planning_day_id: r.get(9)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Paged::new(items, total, p))
    })
}

// ------------------------------------------------------------------ 日记

#[derive(Debug, Serialize)]
pub struct DiaryItem {
    pub id: String,
    pub kind: String,
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    pub title: Option<String>,
    pub body: String,
    pub model: Option<String>,
    pub created_ms: i64,
}

pub fn diaries(db: &Db, kind: Option<&str>, p: Page) -> Result<Paged<DiaryItem>> {
    let mut where_sql = String::from("WHERE deleted_by IS NULL");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(k) = kind.filter(|s| !s.is_empty()) {
        where_sql.push_str(" AND kind = ?");
        args.push(Box::new(k.to_string()));
    }

    db.with_read(|c| {
        let total: i64 = c.query_row(
            &format!("SELECT COUNT(*) FROM diary {where_sql}"),
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
            |r| r.get(0),
        )?;

        let mut all: Vec<Box<dyn rusqlite::ToSql>> = args;
        all.push(Box::new(p.size as i64));
        all.push(Box::new(p.offset()));

        let mut stmt = c.prepare(&format!(
            "SELECT id, kind, window_start_ms, window_end_ms, title, body,
                    model, created_ms
             FROM diary {where_sql} ORDER BY window_start_ms DESC LIMIT ? OFFSET ?"
        ))?;
        let items = stmt
            .query_map(
                rusqlite::params_from_iter(all.iter().map(|b| b.as_ref())),
                |r| {
                    Ok(DiaryItem {
                        id: r.get(0)?,
                        kind: r.get(1)?,
                        window_start_ms: r.get(2)?,
                        window_end_ms: r.get(3)?,
                        title: r.get(4)?,
                        body: r.get(5)?,
                        model: r.get(6)?,
                        created_ms: r.get(7)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Paged::new(items, total, p))
    })
}

// ------------------------------------------------------------------ 身份

#[derive(Debug, Serialize)]
pub struct PersonItem {
    pub person_id: String,
    pub display_name: String,
    pub status: String,
    pub created_ms: i64,
    pub note: Option<String>,
    /// 这个人在这几个渠道上有账号。面板上要能看出一对多。
    pub accounts: Vec<AccountItem>,
    /// 各场景里的称呼（人工设置优先，否则按渠道账号/群消息推导）。
    /// 场景称呼的编辑入口在「身份」页——关系页只展示。
    pub scenes: Vec<SceneItem>,
}

#[derive(Debug, Serialize)]
pub struct AccountItem {
    pub channel: String,
    pub account: String,
    pub seen_name: Option<String>,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

pub fn persons(db: &Db, q: Option<&str>, p: Page) -> Result<Paged<PersonItem>> {
    let mut where_sql = String::from("WHERE 1=1");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(q) = q.filter(|s| !s.trim().is_empty()) {
        where_sql.push_str(" AND display_name LIKE ?");
        args.push(Box::new(format!("%{}%", q.trim())));
    }

    db.with_read(|c| {
        let total: i64 = c.query_row(
            &format!("SELECT COUNT(*) FROM person {where_sql}"),
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
            |r| r.get(0),
        )?;

        let mut all: Vec<Box<dyn rusqlite::ToSql>> = args;
        all.push(Box::new(p.size as i64));
        all.push(Box::new(p.offset()));

        let mut stmt = c.prepare(&format!(
            "SELECT person_id, display_name, status, created_ms, note
             FROM person {where_sql} ORDER BY created_ms DESC LIMIT ? OFFSET ?"
        ))?;
        let mut items = stmt
            .query_map(
                rusqlite::params_from_iter(all.iter().map(|b| b.as_ref())),
                |r| {
                    Ok(PersonItem {
                        person_id: r.get(0)?,
                        display_name: r.get(1)?,
                        status: r.get(2)?,
                        created_ms: r.get(3)?,
                        note: r.get(4)?,
                        accounts: Vec::new(),
                        scenes: Vec::new(),
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // 一次把这页所有人的账号查出来，避免 N+1。
        for person in items.iter_mut() {
            let mut s2 = c.prepare(
                "SELECT channel, account, seen_name, first_seen_ms, last_seen_ms
                 FROM person_account WHERE person_id = ?1 ORDER BY channel",
            )?;
            person.accounts = s2
                .query_map(rusqlite::params![person.person_id], |r| {
                    Ok(AccountItem {
                        channel: r.get(0)?,
                        account: r.get(1)?,
                        seen_name: r.get(2)?,
                        first_seen_ms: r.get(3)?,
                        last_seen_ms: r.get(4)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
        }
        // 场景称呼一次查全（人 → 场景 → 名字），不在循环里按人查。
        let scene_data = load_scene_data(c)?;
        for person in items.iter_mut() {
            person.scenes = scenes_for(&person.person_id, &scene_data);
        }
        Ok(Paged::new(items, total, p))
    })
}

// ------------------------------------------------------------------ 情绪

#[derive(Debug, Serialize)]
pub struct MoodItem {
    pub valence: f64,
    pub arousal: f64,
    pub dominance: f64,
    pub label: String,
    pub reason: String,
    pub updated_ms: i64,
}

/// 当前情绪。单主体，只有一条。
///
/// **返回前补算衰减**：库里存的是最后一次被推动时的数值，
/// 而面板要显示「现在」。少了这一步，三天没动过的情绪会原样显示。
pub fn moods(db: &Db) -> Result<Vec<MoodItem>> {
    let now = crate::db::now_ms();
    db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT valence, arousal, dominance, label, reason, updated_ms
             FROM mood ORDER BY updated_ms DESC",
        )?;
        let mut out = stmt
            .query_map([], |r| {
                Ok(MoodItem {
                    valence: r.get(0)?,
                    arousal: r.get(1)?,
                    dominance: r.get(2)?,
                    label: r.get(3)?,
                    reason: r.get(4)?,
                    updated_ms: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for m in out.iter_mut() {
            let mut mood = crate::affect::Mood {
                valence: m.valence,
                arousal: m.arousal,
                dominance: m.dominance,
                label: m.label.clone(),
                reason: m.reason.clone(),
                updated_ms: m.updated_ms,
            };
            mood.decay(now - m.updated_ms, crate::affect::MOOD_HALF_LIFE_MS);
            m.valence = mood.valence;
            m.arousal = mood.arousal;
            m.dominance = mood.dominance;
            m.label = mood.label;
        }
        Ok(out)
    })
}

// ------------------------------------------------------------------ 规划

#[derive(Debug, Serialize)]
pub struct PlanItemRow {
    pub id: String,
    pub title: String,
    pub kind: Option<String>,
    pub task_type: Option<String>,
    pub priority: Option<i64>,
    pub reason: Option<String>,
    pub success: Option<String>,
    pub fallback: Option<String>,
    pub state: String,
    pub outcome: Option<String>,
    pub carry_from: Option<String>,
    pub args_json: Option<String>,
    /// 第几步。计划的语义依赖顺序（先采料再盖房），不能按 id 排。
    pub ord: i64,
}

#[derive(Debug, Serialize)]
pub struct PlanRow {
    pub id: String,
    /// 游戏日 `g<N>`。**只是标签**，排序用 created_ms。
    pub planning_day_id: String,
    pub summary: Option<String>,
    pub model: Option<String>,
    pub game_day: Option<i64>,
    pub created_ms: i64,
    pub updated_ms: i64,
    /// 条目统计。列表页不必把所有条目都拉下来。
    pub total_items: i64,
    pub done_items: i64,
    pub deferred_items: i64,
    pub todo_items: i64,
}

impl PlanRow {
    /// 完成率，用于列表上画进度条。
    pub fn progress(&self) -> f64 {
        if self.total_items == 0 {
            return 0.0;
        }
        self.done_items as f64 / self.total_items as f64
    }
}

#[derive(Debug, Serialize)]
pub struct PlanDetail {
    #[serde(flatten)]
    pub plan: PlanRow,
    pub progress: f64,
    pub items: Vec<PlanItemRow>,
}

/// 列出规划，按现实时间倒序（不是按 `g<N>` 排序——那是游戏时钟，
/// 停服期间不走，拿它排序会让「最近一份」判断错）。
pub fn plans(db: &Db, q: Option<&str>, p: Page) -> Result<Paged<PlanRow>> {
    let mut where_sql = String::from("WHERE p.deleted_by IS NULL");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(q) = q.filter(|s| !s.trim().is_empty()) {
        where_sql.push_str(" AND (p.summary LIKE ? OR p.planning_day_id LIKE ?)");
        let like = format!("%{}%", q.trim());
        args.push(Box::new(like.clone()));
        args.push(Box::new(like));
    }

    db.with_read(|c| {
        let total: i64 = c.query_row(
            &format!("SELECT COUNT(*) FROM plan p {where_sql}"),
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
            |r| r.get(0),
        )?;

        let mut all: Vec<Box<dyn rusqlite::ToSql>> = args;
        all.push(Box::new(p.size as i64));
        all.push(Box::new(p.offset()));

        let mut stmt = c.prepare(&format!(
            "SELECT p.id, p.planning_day_id, p.summary, p.model, p.game_day,
                    p.created_ms, p.updated_ms,
                    (SELECT COUNT(*) FROM plan_item i WHERE i.plan_id = p.id),
                    (SELECT COUNT(*) FROM plan_item i WHERE i.plan_id = p.id AND i.state='done'),
                    (SELECT COUNT(*) FROM plan_item i WHERE i.plan_id = p.id AND i.state='deferred'),
                    (SELECT COUNT(*) FROM plan_item i WHERE i.plan_id = p.id AND i.state='todo')
             FROM plan p {where_sql} ORDER BY p.created_ms DESC LIMIT ? OFFSET ?"
        ))?;
        let items = stmt
            .query_map(
                rusqlite::params_from_iter(all.iter().map(|b| b.as_ref())),
                |r| {
                    Ok(PlanRow {
                        id: r.get(0)?,
                        planning_day_id: r.get(1)?,
                        summary: r.get(2)?,
                        model: r.get(3)?,
                        game_day: r.get(4)?,
                        created_ms: r.get(5)?,
                        updated_ms: r.get(6)?,
                        total_items: r.get(7)?,
                        done_items: r.get(8)?,
                        deferred_items: r.get(9)?,
                        todo_items: r.get(10)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Paged::new(items, total, p))
    })
}

/// 一份规划的完整内容（含全部条目）。
pub fn plan_detail(db: &Db, plan_id: &str) -> Result<PlanDetail> {
    db.with_read(|c| {
        let plan = c
            .query_row(
                "SELECT p.id, p.planning_day_id, p.summary, p.model, p.game_day,
                        p.created_ms, p.updated_ms,
                        (SELECT COUNT(*) FROM plan_item i WHERE i.plan_id = p.id),
                        (SELECT COUNT(*) FROM plan_item i WHERE i.plan_id = p.id AND i.state='done'),
                        (SELECT COUNT(*) FROM plan_item i WHERE i.plan_id = p.id AND i.state='deferred'),
                        (SELECT COUNT(*) FROM plan_item i WHERE i.plan_id = p.id AND i.state='todo')
                 FROM plan p WHERE p.id = ?1 AND p.deleted_by IS NULL",
                rusqlite::params![plan_id],
                |r| {
                    Ok(PlanRow {
                        id: r.get(0)?,
                        planning_day_id: r.get(1)?,
                        summary: r.get(2)?,
                        model: r.get(3)?,
                        game_day: r.get(4)?,
                        created_ms: r.get(5)?,
                        updated_ms: r.get(6)?,
                        total_items: r.get(7)?,
                        done_items: r.get(8)?,
                        deferred_items: r.get(9)?,
                        todo_items: r.get(10)?,
                    })
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    crate::error::Error::NotFound(format!("规划 {plan_id}"))
                }
                other => other.into(),
            })?;

        let mut stmt = c.prepare(
            "SELECT id, title, kind, task_type, priority, reason, success, fallback, state,
                    outcome, carry_from, args_json, ord
             FROM plan_item WHERE plan_id = ?1 ORDER BY ord",
        )?;
        let items = stmt
            .query_map(rusqlite::params![plan_id], |r| {
                Ok(PlanItemRow {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    kind: r.get(2)?,
                    task_type: r.get(3)?,
                    priority: r.get(4)?,
                    reason: r.get(5)?,
                    success: r.get(6)?,
                    fallback: r.get(7)?,
                    state: r.get(8)?,
                    outcome: r.get(9)?,
                    carry_from: r.get(10)?,
                    args_json: r.get(11)?,
                    ord: r.get(12)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let progress = plan.progress();
        Ok(PlanDetail {
            plan,
            progress,
            items,
        })
    })
}

// ------------------------------------------------------------------ 自我认知

#[derive(Debug, Serialize)]
pub struct SelfNarrativeItem {
    pub id: i64,
    pub body: String,
    pub model: Option<String>,
    pub created_ms: i64,
    pub superseded_ms: Option<i64>,
    /// true = 当前生效的那一版。
    pub current: bool,
}

/// 自我认知的全部版本，最新的在前。
///
/// 这是 Java 面板有而我们漏掉的一块：它回答「它觉得自己是谁」。
/// 与 persona 的区别：persona 是操作者写的剧本（应该成为谁），
/// 这里是它自己写的（现在觉得自己是谁）。
pub fn self_narratives(db: &Db) -> Result<Vec<SelfNarrativeItem>> {
    db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT id, body, model, created_ms, superseded_ms
             FROM self_narrative WHERE deleted_by IS NULL
             ORDER BY created_ms DESC LIMIT 200",
        )?;
        let items = stmt
            .query_map([], |r| {
                let superseded: Option<i64> = r.get(4)?;
                Ok(SelfNarrativeItem {
                    id: r.get(0)?,
                    body: r.get(1)?,
                    model: r.get(2)?,
                    created_ms: r.get(3)?,
                    superseded_ms: superseded,
                    current: superseded.is_none(),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    })
}

// ------------------------------------------------------------------ 关系

#[derive(Debug, Serialize)]
pub struct RelationItem {
    pub person_id: String,
    pub display_name: Option<String>,
    pub familiarity: i64,
    pub trust: i64,
    pub closeness: i64,
    pub note: Option<String>,
    pub first_met_ms: i64,
    pub last_met_ms: i64,
    /// 亲密度阶段的中文说法，与 Java 侧 `CompanionPrompt.stageFor` 同义。
    pub stage: String,
    /// 这个人各「场景」里的称呼（MC / QQ 私聊 / QQ 群 / 直播间）。
    pub scenes: Vec<SceneItem>,
}

/// 一个场景里的称呼。
///
/// 两种来源：`manual` 是人在面板上填的，`derived` 是从已有的渠道账号
/// 或群消息里推出来的。面板要把它们分开显示——推导出来的值会随渠道数据
/// 变，人工填的不会；混在一起就没法判断「这个称呼还准不准」。
#[derive(Debug, Serialize)]
pub struct SceneItem {
    pub scene: String,
    pub label: String,
    pub name: Option<String>,
    /// manual / derived / none
    pub source: String,
    /// 推导依据（渠道账号），面板上作为提示。
    pub hint: Option<String>,
}

/// 场景键 → 面板上的中文名。顺序就是展示顺序。
pub const PERSON_SCENES: &[(&str, &str)] = &[
    ("mc", "MC 里"),
    ("qq", "QQ 私聊"),
    ("qq_group", "QQ 群"),
    ("live", "直播间"),
];

/// 场景键是否合法。
pub fn is_person_scene(scene: &str) -> bool {
    PERSON_SCENES.iter().any(|(k, _)| *k == scene)
}

/// 场景称呼的原料：一次查全，避免按人 N+1。
struct SceneData {
    /// (person_id, scene) → 人工设置的名字
    manual: std::collections::HashMap<(String, String), String>,
    /// 人 → [(渠道, 账号, 该渠道名字)]
    accounts: std::collections::HashMap<String, Vec<(String, String, Option<String>)>>,
    /// 人 → QQ 群名片（取最近一条群消息）
    group_names: std::collections::HashMap<String, String>,
}

fn load_scene_data(c: &rusqlite::Connection) -> Result<SceneData> {
    let mut manual = std::collections::HashMap::new();
    {
        let mut s = c.prepare("SELECT person_id, scene, name FROM person_scene")?;
        let rows = s.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (pid, scene, name) = row?;
            manual.insert((pid, scene), name);
        }
    }

    let mut accounts: std::collections::HashMap<String, Vec<(String, String, Option<String>)>> =
        std::collections::HashMap::new();
    {
        let mut s = c.prepare(
            "SELECT person_id, channel, account, seen_name
             FROM person_account ORDER BY channel, last_seen_ms DESC",
        )?;
        let rows = s.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (pid, channel, account, seen) = row?;
            accounts
                .entry(pid)
                .or_default()
                .push((channel, account, seen));
        }
    }

    // QQ 群名片：同一个 QQ 号在不同群可能不同，取最近一句的发送者名。
    let mut group_names = std::collections::HashMap::new();
    {
        let mut s = c.prepare(
            "SELECT person_id, sender_name FROM (
               SELECT pa.person_id AS person_id, cm.sender_name AS sender_name,
                      ROW_NUMBER() OVER (
                        PARTITION BY pa.person_id ORDER BY cm.at_ms DESC
                      ) AS rn
               FROM person_account pa
               JOIN channel_message cm
                 ON cm.channel = 'qq' AND cm.account = pa.account
               WHERE pa.channel = 'qq'
                 AND cm.group_id IS NOT NULL
                 AND cm.sender_name IS NOT NULL AND cm.sender_name <> ''
                 AND cm.deleted_by IS NULL
             ) WHERE rn = 1",
        )?;
        let rows = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (pid, name) = row?;
            group_names.insert(pid, name);
        }
    }

    Ok(SceneData {
        manual,
        accounts,
        group_names,
    })
}

/// 组装一个人的四个场景称呼：人工设置 > 渠道账号/群名片 > 不知道。
fn scenes_for(person_id: &str, data: &SceneData) -> Vec<SceneItem> {
    let mut out = Vec::with_capacity(PERSON_SCENES.len());
    for (key, label) in PERSON_SCENES {
        let scene = (*key).to_string();
        let label = (*label).to_string();
        if let Some(name) = data.manual.get(&(person_id.to_string(), scene.clone())) {
            out.push(SceneItem {
                scene,
                label,
                name: Some(name.clone()),
                source: "manual".into(),
                hint: None,
            });
            continue;
        }
        let derived = match *key {
            // QQ 群名片没有账号可当提示，hint 留空。
            "qq_group" => data
                .group_names
                .get(person_id)
                .cloned()
                .map(|n| (n, String::new())),
            ch => data.accounts.get(person_id).and_then(|list| {
                list.iter()
                    .find(|(channel, _, _)| channel == ch)
                    .and_then(|(_, account, seen)| seen.clone().map(|n| (n, account.clone())))
            }),
        };
        let (name, hint) = match derived {
            // 空串转 None：群名片那条没有账号可给，面板上「提示」那一栏
            // 不该显示成一个空的 Some("")。
            Some((n, a)) => (Some(n), if a.is_empty() { None } else { Some(a) }),
            None => (None, None),
        };
        out.push(SceneItem {
            scene,
            label,
            name: name.clone(),
            source: if name.is_some() { "derived" } else { "none" }.into(),
            hint,
        });
    }
    out
}

pub fn relations(db: &Db) -> Result<Vec<RelationItem>> {
    db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT r.person_id, p.display_name, r.familiarity, r.trust, r.closeness,
                    r.note, r.first_met_ms, r.last_met_ms
             FROM relation r LEFT JOIN person p ON p.person_id = r.person_id
             ORDER BY r.closeness DESC LIMIT 200",
        )?;
        let mut items = stmt
            .query_map([], |r| {
                let closeness: i64 = r.get(4)?;
                Ok(RelationItem {
                    person_id: r.get(0)?,
                    display_name: r.get(1)?,
                    familiarity: r.get(2)?,
                    trust: r.get(3)?,
                    closeness,
                    note: r.get(5)?,
                    first_met_ms: r.get(6)?,
                    last_met_ms: r.get(7)?,
                    stage: closeness_stage(closeness),
                    scenes: Vec::new(),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        if items.is_empty() {
            return Ok(items);
        }
        let data = load_scene_data(c)?;
        for item in items.iter_mut() {
            item.scenes = scenes_for(&item.person_id, &data);
        }
        Ok(items)
    })
}

/// 亲密度 → 中文阶段。阈值与 Java 侧一致（0/20/40/70/90）。
fn closeness_stage(c: i64) -> String {
    match c {
        c if c >= 90 => "交心",
        c if c >= 70 => "亲近",
        c if c >= 40 => "熟识",
        c if c >= 20 => "认识",
        _ => "陌生",
    }
    .to_string()
}

// ------------------------------------------------------------------ 特质

#[derive(Debug, Serialize)]
pub struct TraitItem {
    pub trait_: String,
    pub value: f64,
    pub updated_ms: i64,
    /// 数值的中文说法，避免面板上只看到 0.62 不知道什么意思。
    pub word: String,
}

pub fn traits(db: &Db) -> Result<Vec<TraitItem>> {
    db.with_read(|c| {
        let mut stmt =
            c.prepare("SELECT trait, value, updated_ms FROM trait ORDER BY value DESC")?;
        let items = stmt
            .query_map([], |r| {
                let v: f64 = r.get(1)?;
                Ok(TraitItem {
                    trait_: r.get(0)?,
                    value: v,
                    updated_ms: r.get(2)?,
                    word: trait_word(v),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    })
}

fn trait_word(v: f64) -> String {
    match v {
        v if v >= 0.75 => "很强",
        v if v >= 0.55 => "偏强",
        v if v >= 0.45 => "中等",
        v if v >= 0.25 => "偏弱",
        _ => "很弱",
    }
    .to_string()
}

// ------------------------------------------------------------------ 承诺

#[derive(Debug, Serialize)]
pub struct PromiseItem {
    pub id: String,
    pub person_id: Option<String>,
    pub description: String,
    pub status: String,
    pub created_ms: i64,
    pub due_ms: Option<i64>,
    pub blocked_reason: Option<String>,
}

pub fn promises(db: &Db, status: Option<&str>) -> Result<Vec<PromiseItem>> {
    db.with_read(|c| {
        let mut where_sql = String::from("WHERE deleted_by IS NULL");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(st) = status.filter(|s| !s.is_empty()) {
            where_sql.push_str(" AND status = ?");
            args.push(Box::new(st.to_string()));
        }
        let mut stmt = c.prepare(&format!(
            "SELECT id, person_id, description, status, created_ms, due_ms,
                    blocked_reason
             FROM promise {where_sql} ORDER BY created_ms DESC LIMIT 200"
        ))?;
        let items = stmt
            .query_map(
                rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
                |r| {
                    Ok(PromiseItem {
                        id: r.get(0)?,
                        person_id: r.get(1)?,
                        description: r.get(2)?,
                        status: r.get(3)?,
                        created_ms: r.get(4)?,
                        due_ms: r.get(5)?,
                        blocked_reason: r.get(6)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    })
}

// ------------------------------------------------------------------ 概览

/// 首页概览。比 `/v1/health` 多的是「按渠道/重要性分布」这类能直接画图的东西。
#[derive(Debug, Serialize)]
pub struct Overview {
    pub total_rows: i64,
    pub tables: Vec<(String, i64)>,
    pub writes: u64,
    pub dedup_hits: u64,
    pub wal_bytes: u64,
    pub forgotten_rows: i64,
    /// 每个渠道各有多少条事件。跨渠道系统的第一眼就该看到这个。
    pub by_channel: Vec<(String, i64)>,
    /// 最近 7 天每天的事件数，用于画趋势。
    pub daily: Vec<(String, i64)>,

    pub latest_snapshot_ms: Option<i64>,
    pub snapshot_count: i64,
    pub rollback_count: i64,
}

pub fn overview(db: &Db) -> Result<Overview> {
    let tables = db.counts()?;
    let total_rows: i64 = tables.iter().map(|(_, n)| n).sum();

    db.with_read(|c| {
        let by_channel = {
            let mut s = c.prepare(
                "SELECT channel, COUNT(*) FROM event WHERE deleted_by IS NULL
                 GROUP BY channel ORDER BY COUNT(*) DESC",
            )?;
            let v = s
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            v
        };

        let daily = {
            // 按本地日聚合：at_ms 是 UTC 毫秒，这里用整秒偏移近似本地日。
            // 精确的时区处理要引 chrono，为了一个趋势图不值得。
            let mut s = c.prepare(
                "SELECT date(at_ms/1000, 'unixepoch', 'localtime') AS d, COUNT(*)
                 FROM event WHERE deleted_by IS NULL
                 GROUP BY d ORDER BY d DESC LIMIT 7",
            )?;
            let mut v = s
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            v.reverse(); // 时间正序，图从左到右
            v
        };

        let latest_snapshot: Option<i64> = c
            .query_row("SELECT MAX(taken_ms) FROM snapshot", [], |r| r.get(0))
            .unwrap_or(None);
        let snapshot_count: i64 = c
            .query_row("SELECT COUNT(*) FROM snapshot", [], |r| r.get(0))
            .unwrap_or(0);
        let rollback_count: i64 = c
            .query_row("SELECT COUNT(*) FROM rollback_log", [], |r| r.get(0))
            .unwrap_or(0);

        Ok(Overview {
            total_rows,
            tables,
            writes: db.write_count(),
            dedup_hits: db.dedup_hits(),
            wal_bytes: db.wal_size(),
            forgotten_rows: crate::rollback::forgotten_count(db).unwrap_or(0),
            by_channel,
            daily,
            latest_snapshot_ms: latest_snapshot,
            snapshot_count,
            rollback_count,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn db() -> (tempfile::TempDir, Arc<Db>) {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(d.path().join("t.db")).unwrap();
        (d, db)
    }

    fn seed(db: &Db, n: usize, channel: &str) {
        db.with_write(|tx| {
            for i in 0..n {
                tx.execute(
                    "INSERT INTO event(at_ms, kind, channel, summary, importance, scope)
                     VALUES (?1, 'test', ?2, ?3, 0.5, 'companion')",
                    rusqlite::params![crate::db::now_ms() + i as i64, channel, format!("事件{i}")],
                )?;
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn 分页参数被夹到合理范围() {
        assert_eq!(Page::new(None, None).page, 1);
        assert_eq!(Page::new(Some(0), None).page, 1, "页码 0 应被夹成 1");
        assert_eq!(Page::new(None, Some(0)).size, 1);
        assert_eq!(Page::new(None, Some(99999)).size, 500, "上限 500");
        assert_eq!(Page::new(Some(3), Some(10)).offset(), 20);
    }

    #[test]
    fn 事件分页与总数() {
        let (_d, db) = db();
        seed(&db, 25, "mc");
        let p = events(&db, &EventFilter::default(), Page::new(Some(1), Some(10))).unwrap();
        assert_eq!(p.items.len(), 10);
        assert_eq!(p.total, 25);
        assert_eq!(p.pages, 3);

        let p3 = events(&db, &EventFilter::default(), Page::new(Some(3), Some(10))).unwrap();
        assert_eq!(p3.items.len(), 5, "最后一页只有 5 条");
    }

    #[test]
    fn 事件按渠道过滤() {
        let (_d, db) = db();
        seed(&db, 5, "mc");
        seed(&db, 3, "qq");
        let f = EventFilter {
            channel: Some("qq".into()),
            ..Default::default()
        };
        let p = events(&db, &f, Page::default()).unwrap();
        assert_eq!(p.total, 3);
        assert!(p.items.iter().all(|e| e.channel == "qq"));
    }

    #[test]
    fn 事件按关键词过滤() {
        let (_d, db) = db();
        seed(&db, 10, "mc");
        let f = EventFilter {
            q: Some("事件3".into()),
            ..Default::default()
        };
        let p = events(&db, &f, Page::default()).unwrap();
        assert!(p.total >= 1);
        assert!(p.items.iter().all(|e| e.summary.contains("事件3")));
    }

    #[test]
    fn 被回滚的事件默认不出现但可查() {
        let (_d, db) = db();
        seed(&db, 5, "mc");
        crate::rollback::rollback_to(&db, crate::db::now_ms() - 1000, None).unwrap();

        let live = events(&db, &EventFilter::default(), Page::default()).unwrap();
        assert_eq!(live.total, 0, "回滚后默认查不到");

        let gone = events(
            &db,
            &EventFilter {
                forgotten: true,
                ..Default::default()
            },
            Page::default(),
        )
        .unwrap();
        assert_eq!(gone.total, 5, "但运维要能查到被忘了什么");
    }

    #[test]
    fn 空结果的分页不会崩() {
        let (_d, db) = db();
        let p = events(&db, &EventFilter::default(), Page::default()).unwrap();
        assert_eq!(p.total, 0);
        assert_eq!(p.pages, 0, "0 条时页数应为 0，不是 1");
        assert!(p.items.is_empty());
    }

    #[test]
    fn 事实可按键值搜索() {
        let (_d, db) = db();
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO fact(id, key, value, scope, status, tier,
                                  confidence, importance, created_ms, updated_ms)
                 VALUES ('f1','主人爱吃的','鱼','player','confirmed','curated',0.9,0.8,1,1)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let f = FactFilter {
            q: Some("鱼".into()),
            ..Default::default()
        };
        assert_eq!(facts(&db, &f, Page::default()).unwrap().total, 1);

        let f = FactFilter {
            q: Some("不吃的东西".into()),
            ..Default::default()
        };
        assert_eq!(facts(&db, &f, Page::default()).unwrap().total, 0);
    }

    #[test]
    fn 身份带出其所有渠道账号() {
        let (_d, db) = db();
        db.with_write(|tx| {
            tx.execute(
                "INSERT INTO person(person_id, display_name, status, created_ms)
                 VALUES ('p1','小明','confirmed',1)",
                [],
            )?;
            for ch in ["mc", "qq"] {
                tx.execute(
                    "INSERT INTO person_account(channel, account, person_id, first_seen_ms, last_seen_ms)
                     VALUES (?1, ?2, 'p1', 1, 1)",
                    rusqlite::params![ch, format!("{ch}-acct")],
                )?;
            }
            Ok(())
        })
        .unwrap();

        let p = persons(&db, None, Page::default()).unwrap();
        assert_eq!(p.total, 1);
        assert_eq!(p.items[0].accounts.len(), 2, "一个人应有 2 个渠道账号");
    }

    #[test]
    fn 概览给出渠道与日趋势() {
        let (_d, db) = db();
        seed(&db, 3, "mc");
        seed(&db, 2, "qq");
        let o = overview(&db).unwrap();
        assert!(o.total_rows >= 5);
        assert_eq!(o.by_channel.len(), 2);
        assert!(!o.daily.is_empty());
    }
}

// ------------------------------------------------------------------ 时间线

/// 一个规划周期里发生的事。
#[derive(Debug, Serialize)]
pub struct TimelineEntry {
    pub id: String,
    pub at_ms: i64,
    pub kind: String,
    pub summary: String,
    pub details: Option<String>,
    pub importance: f64,
    /// true = 这条来自手帐（经过筛选），false = 原始事件。
    pub from_journal: bool,
}

/// 一个周期：计划 + 它实际做了的事。
///
/// # 为什么要把这两样合在一起
///
/// 分开看时，「计划写了什么」和「实际发生了什么」在两张表里，
/// 想知道「它今天到底干了没」得来回对照。
///
/// 合到一条脊线上的价值不在于好看：**有计划但下面什么都没有的周期会立刻显形**。
/// 空转的日子从「列表里数字是 0」变成一个明显空掉的段落。
#[derive(Debug, Serialize)]
pub struct TimelineDay {
    /// 游戏日标签 `g<N>`，没有计划的日子用 `(未计划)`。
    pub cycle: String,
    pub plan_id: Option<String>,
    pub plan_summary: Option<String>,
    pub total_items: i64,
    pub done_items: i64,
    pub deferred_items: i64,
    /// 这一天的锚点时间（有计划的 created_ms，否则手帐里最早一条的 at_ms）。
    pub anchor_ms: i64,
    /// 它实际做了什么，时间正序。
    pub entries: Vec<TimelineEntry>,
    /// 这一天有手帐但没计划，或者有计划但没执行记录 —— 空转的信号。
    pub has_plan: bool,
    pub has_activity: bool,
}

/// 最近 N 个周期的时间线，**最新的在前**。
pub fn timeline(db: &Db, days: usize) -> Result<Vec<TimelineDay>> {
    let days = days.clamp(1, 60);

    db.with_read(|c| {
        // 1. 最近的计划（按 created_ms，因为 g<N> 是游戏时钟，停服不走）
        // 这元组有 7 个字段，逐个写出来比命名一个只用一次的结构体更清楚，
        // 但 clippy 认为该抽出来 —— 抽成别名，两边的意图都保住了。
        type PlanRow7 = (String, String, Option<String>, i64, i64, i64, i64);
        let mut plans: Vec<PlanRow7> = Vec::new();
        {
            let mut stmt = c.prepare(
                "SELECT id, planning_day_id, summary, created_ms,
                        (SELECT COUNT(*) FROM plan_item i WHERE i.plan_id = p.id),
                        (SELECT COUNT(*) FROM plan_item i WHERE i.plan_id = p.id AND i.state='done'),
                        (SELECT COUNT(*) FROM plan_item i WHERE i.plan_id = p.id AND i.state='deferred')
                 FROM plan p WHERE deleted_by IS NULL
                 ORDER BY created_ms DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(rusqlite::params![days as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })?;
            for row in rows {
                plans.push(row?);
            }
        }

        // 2. 手帐按规划日分组
        let mut by_day: std::collections::HashMap<String, Vec<TimelineEntry>> =
            std::collections::HashMap::new();
        {
            let mut stmt = c.prepare(
                "SELECT id, at_ms, type, summary, details, importance, planning_day_id
                 FROM journal WHERE deleted_by IS NULL
                 ORDER BY at_ms DESC LIMIT 600",
            )?;
            let rows = stmt.query_map([], |r| {
                let day: Option<String> = r.get(6)?;
                Ok((
                    day.unwrap_or_else(|| "(未计划)".into()),
                    TimelineEntry {
                        id: r.get(0)?,
                        at_ms: r.get(1)?,
                        kind: r.get(2)?,
                        summary: r.get(3)?,
                        details: r.get(4)?,
                        importance: r.get(5)?,
                        from_journal: true,
                    },
                ))
            })?;
            for row in rows {
                let (day, e) = row?;
                by_day.entry(day).or_default().push(e);
            }
        }

        // 3. 合并：计划主导顺序，没计划的日期补在后面
        let mut out: Vec<TimelineDay> = Vec::new();
        let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (id, cycle, summary, created, total, done, deferred) in plans {
            used.insert(cycle.clone());
            let mut entries = by_day.remove(&cycle).unwrap_or_default();
            entries.sort_by_key(|e| e.at_ms);
            let has_activity = !entries.is_empty();
            out.push(TimelineDay {
                cycle,
                plan_id: Some(id),
                plan_summary: summary,
                total_items: total,
                done_items: done,
                deferred_items: deferred,
                anchor_ms: created,
                entries,
                has_plan: true,
                has_activity,
            });
        }

        // 有手帐但没计划的日期（例如那天的计划文件被轮转掉了）
        let mut orphans: Vec<(String, Vec<TimelineEntry>)> = by_day
            .into_iter()
            .filter(|(k, _)| !used.contains(k))
            .collect();
        orphans.sort_by(|a, b| {
            let ta = a.1.first().map(|e| e.at_ms).unwrap_or(0);
            let tb = b.1.first().map(|e| e.at_ms).unwrap_or(0);
            tb.cmp(&ta)
        });
        for (cycle, mut entries) in orphans.into_iter().take(days) {
            entries.sort_by_key(|e| e.at_ms);
            let anchor = entries.first().map(|e| e.at_ms).unwrap_or(0);
            out.push(TimelineDay {
                cycle,
                plan_id: None,
                plan_summary: None,
                total_items: 0,
                done_items: 0,
                deferred_items: 0,
                anchor_ms: anchor,
                has_activity: true,
                entries,
                has_plan: false,
            });
        }

        // 按锚点时间倒序，取前 days 个
        out.sort_by_key(|d| std::cmp::Reverse(d.anchor_ms));
        out.truncate(days);
        Ok(out)
    })
}

// ------------------------------------------------------------------ 群消息

#[derive(Debug, Serialize)]
pub struct MessageRow {
    pub id: i64,
    pub channel: String,
    pub group_id: Option<String>,
    pub account: Option<String>,
    pub sender_name: Option<String>,
    pub content: String,
    pub at_ms: i64,
    pub message_id: Option<String>,
    pub consumed: i64,
}

/// 渠道原始消息。
///
/// 单独一页的理由：在跨渠道系统里，「QQ 群里到底说了什么」是最常被追问的一条。
/// 它和 `journal` 不是一回事——journal 是筛选后的经历，这里是原话。
pub fn messages(
    db: &Db,
    channel: Option<&str>,
    q: Option<&str>,
    p: Page,
) -> Result<Paged<MessageRow>> {
    let mut where_sql = String::from("WHERE deleted_by IS NULL");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(c) = channel.filter(|s| !s.is_empty()) {
        where_sql.push_str(" AND channel = ?");
        args.push(Box::new(c.to_string()));
    }
    if let Some(q) = q.filter(|s| !s.trim().is_empty()) {
        where_sql.push_str(" AND (content LIKE ? OR sender_name LIKE ?)");
        let like = format!("%{}%", q.trim());
        args.push(Box::new(like.clone()));
        args.push(Box::new(like));
    }

    db.with_read(|c| {
        let total: i64 = c.query_row(
            &format!("SELECT COUNT(*) FROM channel_message {where_sql}"),
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
            |r| r.get(0),
        )?;

        let mut all: Vec<Box<dyn rusqlite::ToSql>> = args;
        all.push(Box::new(p.size as i64));
        all.push(Box::new(p.offset()));

        let mut stmt = c.prepare(&format!(
            "SELECT id, channel, group_id, account, sender_name, content, at_ms,
                    message_id, consumed
             FROM channel_message {where_sql} ORDER BY at_ms DESC LIMIT ? OFFSET ?"
        ))?;
        let items = stmt
            .query_map(
                rusqlite::params_from_iter(all.iter().map(|b| b.as_ref())),
                |r| {
                    Ok(MessageRow {
                        id: r.get(0)?,
                        channel: r.get(1)?,
                        group_id: r.get(2)?,
                        account: r.get(3)?,
                        sender_name: r.get(4)?,
                        content: r.get(5)?,
                        at_ms: r.get(6)?,
                        message_id: r.get(7)?,
                        consumed: r.get(8)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Paged::new(items, total, p))
    })
}

// ------------------------------------------------------------------ 注入回放

#[derive(Debug, Serialize)]
pub struct RecallTraceItem {
    pub kind: String,
    pub target_id: String,
    pub text: Option<String>,
    pub score: Option<f64>,
    pub kept: bool,
    pub reason: Option<String>,
}

/// 一次检索 = 一个 trace。
#[derive(Debug, Serialize)]
pub struct RecallTrace {
    pub trace_id: String,
    pub at_ms: i64,
    pub query: Option<String>,
    pub lane: Option<String>,
    pub kept: usize,
    pub dropped: usize,
    pub items: Vec<RecallTraceItem>,
}

/// 最近的注入回放，新的在前。
///
/// 回答的问题是「它为什么这么说 / 为什么没提那件事」：
/// 一次检索选了哪几条、丢了哪几条、各自什么分数。
pub fn recall_traces(db: &Db, limit: usize) -> Result<Vec<RecallTrace>> {
    let limit = limit.clamp(1, 100);

    db.with_read(|c| {
        // 先取最近的 trace id（新的在前）
        let mut ids: Vec<String> = Vec::new();
        {
            let mut stmt = c.prepare(
                "SELECT trace_id, MAX(at_ms) AS t FROM recall_log
                 WHERE trace_id IS NOT NULL GROUP BY trace_id ORDER BY t DESC LIMIT ?1",
            )?;
            let rows =
                stmt.query_map(rusqlite::params![limit as i64], |r| r.get::<_, String>(0))?;
            for row in rows {
                ids.push(row?);
            }
        }

        let mut out = Vec::new();
        for tid in ids {
            let mut stmt = c.prepare(
                "SELECT kind, target_id, text, score, kept, reason, at_ms, query, lane
                 FROM recall_log WHERE trace_id = ?1 ORDER BY score DESC",
            )?;
            let mut items = Vec::new();
            let mut at = 0i64;
            let mut query = None;
            let mut lane = None;
            let mut kept = 0usize;
            let mut dropped = 0usize;
            let rows = stmt.query_map(rusqlite::params![tid], |r| {
                let k: i64 = r.get(4)?;
                Ok((
                    RecallTraceItem {
                        kind: r.get(0)?,
                        target_id: r.get(1)?,
                        text: r.get(2)?,
                        score: r.get(3)?,
                        kept: k != 0,
                        reason: r.get(5)?,
                    },
                    r.get::<_, i64>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            })?;
            for row in rows {
                let (item, t, qy, ln) = row?;
                at = at.max(t);
                if query.is_none() {
                    query = qy;
                }
                if lane.is_none() {
                    lane = ln;
                }
                if item.kept {
                    kept += 1;
                } else {
                    dropped += 1;
                }
                items.push(item);
            }
            out.push(RecallTrace {
                trace_id: tid,
                at_ms: at,
                query,
                lane,
                kept,
                dropped,
                items,
            });
        }
        Ok(out)
    })
}

// ------------------------------------------------------------------ 注入回放（插件上报）

#[derive(Debug, Serialize)]
pub struct InjectionRow {
    pub id: i64,
    pub at_ms: i64,
    pub lane: Option<String>,
    pub query: Option<String>,
    pub chars: i64,
    pub injected: bool,
    pub replay: Option<String>,
}

/// 插件推上来的真实注入回放，新的在前。
pub fn injection_replays(db: &Db, limit: usize) -> Result<Vec<InjectionRow>> {
    let limit = limit.clamp(1, 200);
    db.with_read(|c| {
        let mut stmt = c.prepare(
            "SELECT id, at_ms, lane, query, COALESCE(chars,0), injected, replay
             FROM injection_replay ORDER BY at_ms DESC LIMIT ?1",
        )?;
        let items = stmt
            .query_map(rusqlite::params![limit as i64], |r| {
                Ok(InjectionRow {
                    id: r.get(0)?,
                    at_ms: r.get(1)?,
                    lane: r.get(2)?,
                    query: r.get(3)?,
                    chars: r.get(4)?,
                    injected: r.get::<_, i64>(5)? != 0,
                    replay: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    })
}
