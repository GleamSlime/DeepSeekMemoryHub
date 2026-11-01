//! 记忆画面：图片也是记忆。
//!
//! # 为什么要有它
//!
//! 「那天群里发的图」「某人在 MC 里拍的那张」—— 这些是纯文本记不住的东西。
//! 和事件一样，它该在**提到相关词**时被想起来，也该能被人手动整理。
//!
//! # 和语音库的区别
//!
//! 语音是**训练素材**（攒起来给人打分），图片是**记忆**（进上下文、被检索）。
//! 所以这里有 `recall_count` / `keywords` / `person_ids`，语音那边没有。
//!
//! # 存原图 + 缩略图
//!
//! 列表页一次要显示几十张，拉原图会几 MB 起步。所以存两份：
//! 原图给「点开看」，缩略图给列表。缩略图生成失败就退回原图 ——
//! 慢一点，但至少能看见。

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// 一条记忆画面。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageItem {
    pub id: String,
    pub file: String,
    pub thumb: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub bytes: i64,
    /// 一句话描述 —— 检索主要靠它。
    pub caption: Option<String>,
    /// 逗号分隔的额外关键词。
    pub keywords: Option<String>,
    pub sender: Option<String>,
    pub channel: Option<String>,
    /// 逗号分隔的 person_id。
    pub person_ids: Option<String>,
    pub recall_count: i64,
    pub last_recall_ms: Option<i64>,
    /// 这张图**是干什么用的**：`emoji`（表情包）/ `photo`（普通图/回忆）。
    ///
    /// 为什么单独一个字段而不是靠 keywords 约定：自动发图时「挑一张表情包活跃气氛」
    /// 和「把那次湖边的照片翻出来」是**两件完全不同的事**，混在一个池子里挑，
    /// 就会在闲聊里发出别人的生活照。靠关键词里塞 `[表情包]` 也能凑，
    /// 但那等于让「检索词」兼职「类型」，一旦有人手工改关键词就悄悄失效。
    ///
    /// 空值按 `photo` 处理（老数据没有这一列时的合理默认）。
    pub kind: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// 内容摘要（sha256）。用于去重：同一张图被反复转发时只存一份。
///
/// 用内容哈希而不是文件大小/文件名：群里转发同一张图，
/// 名字每次都不同、大小偶尔相同 —— 只有内容是稳定的。
pub fn digest(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 按魔数判断图片类型。返回扩展名。
///
/// 不信文件名：multipart 的 filename 是客户端写的，拿它定扩展名
/// 等于让调用方决定我们存什么。
pub fn sniff(data: &[u8]) -> Option<&'static str> {
    if data.len() < 12 {
        return None;
    }
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpg");
    }
    if data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("png");
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return Some("gif");
    }
    if data.len() > 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return Some("webp");
    }
    if data.starts_with(b"BM") {
        return Some("bmp");
    }
    None
}

/// MIME。
pub fn mime(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "application/octet-stream",
    }
}

/// 找可执行文件（同 `voice::tool` 的理由：launchd 的 PATH 里没有 Homebrew）。
fn tool(name: &str) -> Option<PathBuf> {
    const DIRS: &[&str] = &[
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/opt/local/bin",
    ];
    for d in DIRS {
        let p = Path::new(d).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    for d in std::env::split_paths(&path) {
        let p = d.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 用 `sips`（macOS 自带）或 `ffmpeg` 生成缩略图。失败返回 `None`。
///
/// 为什么两个都试：`sips` 只存在于 macOS，但**不需要装任何东西**；
/// ffmpeg 跨平台但可能没装。两个都没有时调用方退回原图。
pub fn make_thumb(src: &Path, dst: &Path, max_px: u32) -> Option<()> {
    if let Some(sips) = tool("sips") {
        let ok = std::process::Command::new(sips)
            .args(["-Z", &max_px.to_string(), "-s", "format", "jpeg"])
            .arg(src)
            .arg("--out")
            .arg(dst)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok && dst.exists() {
            return Some(());
        }
        let _ = std::fs::remove_file(dst);
    }
    if let Some(ff) = tool("ffmpeg") {
        let ok = std::process::Command::new(ff)
            .args(["-y", "-loglevel", "error", "-i"])
            .arg(src)
            .args(["-vf", &format!("scale='min({max_px},iw)':'min({max_px},ih)':force_original_aspect_ratio=decrease")])
            .arg(dst)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok && dst.exists() {
            return Some(());
        }
        let _ = std::fs::remove_file(dst);
    }
    None
}

/// 读尺寸。`sips` 输出「  pixelWidth: 800」这种，解析出来。
///
/// 拿不到就 `None` —— 面板上显示「—」比显示一个假数字好。
pub fn dimensions(path: &Path) -> (Option<i64>, Option<i64>) {
    let Some(sips) = tool("sips") else {
        return (None, None);
    };
    let Ok(out) = std::process::Command::new(sips)
        .args(["-g", "pixelWidth", "-g", "pixelHeight"])
        .arg(path)
        .output()
    else {
        return (None, None);
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut w = None;
    let mut h = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("pixelWidth:") {
            w = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("pixelHeight:") {
            h = v.trim().parse().ok();
        }
    }
    (w, h)
}

/// 存图的结果：(文件名, 缩略图, 宽, 高, 字节)。
pub struct Stored {
    pub file: String,
    pub thumb: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub bytes: i64,
}

/// 存一张图。
pub fn store_image(dir: &Path, id: &str, data: &[u8]) -> Result<Stored> {
    let ext = sniff(data).ok_or_else(|| {
        Error::BadRequest("认不出图片格式（支持 jpg / png / gif / webp / bmp）".into())
    })?;
    std::fs::create_dir_all(dir)?;
    let name = format!("{id}.{ext}");
    let path = dir.join(&name);
    std::fs::write(&path, data)?;

    let thumb_name = format!("{id}.thumb.jpg");
    let thumb_path = dir.join(&thumb_name);
    let thumb = make_thumb(&path, &thumb_path, 320).map(|_| thumb_name);
    let (w, h) = dimensions(&path);
    Ok(Stored {
        file: name,
        thumb,
        width: w,
        height: h,
        bytes: data.len() as i64,
    })
}

/// 解析出可安全用于文件名的 id。
pub fn safe_id(raw: &str) -> Option<String> {
    let t: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if t.is_empty() || t.len() > 64 {
        None
    } else {
        Some(t)
    }
}

/// 解析文件名到路径（拒绝越界）。
pub fn resolve_file(dir: &Path, file: &str) -> Option<PathBuf> {
    if file.contains('/') || file.contains('\\') || file.contains("..") {
        return None;
    }
    let stem = file.split('.').next()?;
    safe_id(stem)?;
    let p = dir.join(file);
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

// ================================================================== 查询

const COLS: &str = "id, file, thumb, width, height, bytes, caption, keywords,
                    sender, channel, person_ids, recall_count, last_recall_ms,
                    kind, created_ms, updated_ms";

fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<ImageItem> {
    Ok(ImageItem {
        id: r.get(0)?,
        file: r.get(1)?,
        thumb: r.get(2)?,
        width: r.get(3)?,
        height: r.get(4)?,
        bytes: r.get(5)?,
        caption: r.get(6)?,
        keywords: r.get(7)?,
        sender: r.get(8)?,
        channel: r.get(9)?,
        person_ids: r.get(10)?,
        recall_count: r.get(11)?,
        last_recall_ms: r.get(12)?,
        kind: r.get(13)?,
        created_ms: r.get(14)?,
        updated_ms: r.get(15)?,
    })
}

pub fn list(db: &crate::db::Db, q: Option<&str>, limit: usize) -> Result<Vec<ImageItem>> {
    let limit = limit.clamp(1, 300);
    db.with_read(|c| {
        let (sql, args): (String, Vec<Box<dyn rusqlite::ToSql>>) = match q.filter(|s| !s.trim().is_empty()) {
            Some(q) => (
                format!(
                    "SELECT {COLS} FROM image
                     WHERE deleted_by IS NULL
                       AND (caption LIKE ?1 OR keywords LIKE ?1 OR sender LIKE ?1)
                     ORDER BY created_ms DESC LIMIT ?2"
                ),
                vec![
                    Box::new(format!("%{}%", q.trim())),
                    Box::new(limit as i64),
                ],
            ),
            None => (
                format!("SELECT {COLS} FROM image WHERE deleted_by IS NULL ORDER BY created_ms DESC LIMIT ?1"),
                vec![Box::new(limit as i64)],
            ),
        };
        let mut stmt = c.prepare(&sql)?;
        let items = stmt
            .query_map(rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())), row_to_item)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    })
}

/// 最近 `n` 条消息（群 + 私聊都算）拼成一段文本，供关键词匹配。
///
/// 为什么看「最近几条」而不是只看着这句：群里的对话常常是
/// 「图 → 哈哈这啥 → 你说呢」这种接力，只看当前这句会漏掉图。
/// 用户 2026-11-01 明确要求看最近三条。
pub fn recent_text(db: &crate::db::Db, scene: crate::context::Scene, n: usize) -> Result<String> {
    let n = n.clamp(1, 20) as i64;
    db.with_read(|c| {
        let sql = match scene {
            crate::context::Scene::Group => {
                "SELECT content FROM channel_message
                 WHERE deleted_by IS NULL AND channel = 'qq' AND group_id IS NOT NULL
                   AND group_id <> ''
                 ORDER BY at_ms DESC, id DESC LIMIT ?1"
            }
            crate::context::Scene::Private => {
                "SELECT content FROM channel_message
                 WHERE deleted_by IS NULL AND channel = 'qq'
                   AND (group_id IS NULL OR group_id = '')
                 ORDER BY at_ms DESC, id DESC LIMIT ?1"
            }
        };
        let mut stmt = c.prepare(sql)?;
        let rows = stmt
            .query_map(rusqlite::params![n], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.join("\n"))
    })
}

/// 读原图并编码成 base64（data URL 形式）。读不到返回 `None`。
///
/// 为什么要塞 base64 进上下文：模型（多模态）真能看到图，
/// 给 id 让它「假装看到」是错的；用户要的是「匹配到就直接给图」。
/// 代价是 token —— 所以调用方必须限制张数与尺寸（见
/// `[context] image_inline_max`）。
pub fn to_data_url(dir: &std::path::Path, item: &ImageItem) -> Option<String> {
    let path = resolve_file(dir, &item.file)?;
    let bytes = std::fs::read(&path).ok()?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    Some(format!("data:{};base64,{}", mime(ext), b64(&bytes)))
}

/// 标准 base64 编码（带 `=` 补齐）。手写：只为两个调用点，不值得引 crate。
pub fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 这个摘要是不是已经有一条有效记录了？是就返回它的 id。
///
/// 去重是**入库前**的判断（而不是靠唯一索引报错再兜）：
/// 命中时调用方要返回「已存在」而不是 500，
/// 而且**不该先把文件落盘再回滚** —— 那会留下孤儿文件。
pub fn find_by_digest(db: &crate::db::Db, sha: &str) -> Result<Option<String>> {
    db.with_read(|c| {
        Ok(c.query_row(
            "SELECT id FROM image WHERE sha256 = ?1 AND deleted_by IS NULL",
            rusqlite::params![sha],
            |r| r.get::<_, String>(0),
        )
        .ok())
    })
}

pub fn get(db: &crate::db::Db, id: &str) -> Result<Option<ImageItem>> {
    db.with_read(|c| {
        Ok(c.query_row(
            &format!("SELECT {COLS} FROM image WHERE id = ?1 AND deleted_by IS NULL"),
            rusqlite::params![id],
            row_to_item,
        )
        .ok())
    })
}

/// 找出「这段话里提到的」图片：caption 或 keywords 里出现了句子中的词。
///
/// 与称呼对照同一套思路：**只在被提到时才想起来**。
/// 把最近几十张图全塞进上下文既费 token，也会让它开始胡说「我看到你发的图」。
pub fn mentioned(db: &crate::db::Db, text: &str, limit: usize) -> Result<Vec<ImageItem>> {
    let t = text.trim();
    if t.chars().count() < 2 || limit == 0 {
        return Ok(Vec::new());
    }
    // 先挑候选：最近 200 张，在内存里比对（图数量级小，不值得建索引）
    let all = list(db, None, 200)?;
    let mut out = Vec::new();
    for img in all {
        let hay = format!(
            "{} {}",
            img.caption.as_deref().unwrap_or(""),
            img.keywords.as_deref().unwrap_or("")
        );
        if hay.trim().is_empty() {
            continue;
        }
        // ---------------------------------------------------------- 命中规则
        //
        // **只有关键词命中才算**（keywords 字段，人工填的）。
        //
        // 为什么不再拿 caption 做 2 字滑窗匹配：实测误命中率极高 ——
        // caption「今天我们一起去湖边钓鱼」会被
        // 「今天天气不错」「我们走吧」「一起吃饭吗」全部命中，
        // 因为它们共享 `今天`/`我们`/`一起` 这种毫无信息量的两字片段。
        // 结果是几乎每句闲聊都会把这张图塞进上下文，既费 token
        // 又会让模型开始编「我看到你说的那张图」。
        //
        // 关键词是**人明确指定的触发词**，语义清楚、可控；
        // caption 只用来「让人/模型知道这张图是什么」，不参与匹配。
        // 想让它能被某句话触发，就把那个词加进 keywords。
        let mut hit = false;
        if let Some(kw) = img.keywords.as_deref() {
            for k in kw.split([',', '，', ' ', '、', ';', '；']).map(str::trim) {
                // 单字关键词太容易误伤（「鱼」「湖」），要求 ≥2 字
                if k.chars().count() >= 2 && t.contains(k) {
                    hit = true;
                    break;
                }
            }
        }
        if hit {
            out.push(img);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 按魔数认图片() {
        assert_eq!(
            sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Some("jpg")
        );
        assert_eq!(
            sniff(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0]),
            Some("png")
        );
        assert_eq!(sniff(b"GIF89a000000"), Some("gif"));
        assert_eq!(sniff(b"notimage0000"), None);
        // 短输入不该 panic
        assert_eq!(sniff(b"\xFF"), None);
    }

    #[test]
    fn 文件名不能越界() {
        let dir = Path::new("/tmp/images");
        assert!(resolve_file(dir, "../../etc/passwd").is_none());
        assert!(resolve_file(dir, "a/b.png").is_none());
        assert!(resolve_file(dir, "abc.png").is_none());
    }

    #[test]
    fn mime_覆盖常见格式() {
        assert_eq!(mime("jpg"), "image/jpeg");
        assert_eq!(mime("png"), "image/png");
        assert_eq!(mime("webp"), "image/webp");
        assert_eq!(mime("unknown"), "application/octet-stream");
    }
}
