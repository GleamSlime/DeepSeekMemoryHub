//! 语音库：把发出去的语音存下来，供人工筛选后训练更好的音源。
//!
//! # 为什么中枢要管这个
//!
//! 语音是**训练素材**，不是运行数据：想做得更好，唯一的办法是攒一批「念得好」的
//! 语音（人工打分标出来），再拿去微调/训练。这些文件天然属于「关于它的数据」，
//! 和记忆放在一起、用同一个备份策略，比散在接入方机器上强。
//!
//! # 存什么格式
//!
//! 统一存 **m4a**。训练工具链普遍吃 m4a/wav，而 m4a 比 wav 小一个数量级
//! （1 分钟约 0.5 MB vs 10 MB）。接入方传上来的可能是 wav/mp3/silk，
//! 这里用 ffmpeg 转一次；**转不了就原样存**并记下真实后缀 ——
//! 宁可存个不能直接训练的格式，也不要因为环境没有 ffmpeg 就把语音丢了。
//!
//! # 为什么不在这里做评分算法
//!
//! 「这段念得好不好」是人的判断，机器打分（MOS 预测）在几秒钟的短句上
//! 噪声极大。所以只存 `rating`（1..5）和 `note`，让人标。

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// 一条语音的元数据。
#[derive(Debug, Clone, serde::Serialize)]
pub struct VoiceItem {
    pub id: String,
    pub file: String,
    pub duration_ms: Option<i64>,
    pub bytes: i64,
    pub text: Option<String>,
    pub speaker: String,
    pub voice: Option<String>,
    pub style: Option<String>,
    /// 1..5，NULL = 还没评。
    pub rating: Option<i64>,
    pub note: Option<String>,
    pub channel: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// 上传时的可选元数据（都从 multipart 的 form 字段来）。
#[derive(Debug, Default)]
pub struct Meta {
    pub text: Option<String>,
    pub speaker: Option<String>,
    pub voice: Option<String>,
    pub style: Option<String>,
    pub channel: Option<String>,
    pub rating: Option<i64>,
    pub note: Option<String>,
}

/// 按魔数判断音频类型。返回 `(扩展名, 是否已经是 m4a/mp4)`。
///
/// 不信任 `filename` 里的后缀：multipart 的文件名是客户端随便写的，
/// 拿它决定存什么扩展名，等于让调用方决定我们怎么存文件。
pub fn sniff(data: &[u8]) -> (&'static str, bool) {
    if data.len() < 12 {
        return ("bin", false);
    }
    // RIFF....WAVE
    if &data[0..4] == b"RIFF" && &data[8..12] == b"WAVE" {
        return ("wav", false);
    }
    // ID3 / 0xFFEx → mp3
    //
    // 注意长度：`sniff` 开头只要求 ≥12 字节，但 `ID3` 判断只要 3 字节 ——
    // 用 `&data[0..3]` 在恰好 12 字节的输入上没问题，可一旦有人传 4 字节的
    // 短样本就会 panic。所以这里用 `starts_with`，按实际长度取。
    if data.starts_with(b"ID3") {
        return ("mp3", false);
    }
    if data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0 {
        return ("mp3", false);
    }
    // ftyp → m4a/mp4 家族
    if data.len() > 12 && &data[4..8] == b"ftyp" {
        return ("m4a", true);
    }
    // OggS
    if &data[0..4] == b"OggS" {
        return ("ogg", false);
    }
    // SILK（QQ 语音）以 0x02 开头，长度不可靠；这里只认出「是 silk」，
    // 转不转看 ffmpeg 能不能吃。
    if data[0] == 0x02 && data.len() > 10 {
        return ("silk", false);
    }
    ("bin", false)
}

/// 找可执行文件的绝对路径。
///
/// 为什么不能只写 `Command::new("ffmpeg")`：这个服务是用 **launchd** 拉起来的，
/// 而 launchd 给的 `PATH` 只有 `/usr/bin:/bin:/usr/sbin:/sbin` ——
/// Homebrew 装的 ffmpeg 在 `/opt/homebrew/bin`，根本不在里面。
/// 实测症状：手动跑转换正常，服务里永远转不了（静默退回原格式），
/// 而日志里一个字都没有，因为「转不了」本来就被设计成正常降级。
///
/// 所以这里按固定顺序找一遍绝对路径，找不到再看 PATH。
fn tool(name: &str) -> Option<std::path::PathBuf> {
    const DIRS: &[&str] = &[
        "/opt/homebrew/bin", // Apple Silicon Homebrew
        "/usr/local/bin",    // Intel Homebrew
        "/usr/bin",
        "/bin",
        "/opt/local/bin", // MacPorts
    ];
    for d in DIRS {
        let p = Path::new(d).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    // 最后看 PATH（容器 / Linux 部署时常见）
    let path = std::env::var_os("PATH")?;
    for d in std::env::split_paths(&path) {
        let p = d.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 把任意音频转成 m4a。失败返回 `None`（调用方退回「原样存」）。
///
/// 用系统 ffmpeg。**不作为硬依赖**：没装 ffmpeg 时语音库照样能用，
/// 只是文件保持原格式、训练前需要自己转。
pub fn to_m4a(input: &Path, output: &Path) -> Option<()> {
    let status = std::process::Command::new(tool("ffmpeg")?)
        .args(["-y", "-loglevel", "error", "-i"])
        .arg(input)
        .args(["-c:a", "aac", "-b:a", "128k", "-movflags", "+faststart"])
        .arg(output)
        .status()
        .ok()?;
    if status.success() && output.exists() {
        Some(())
    } else {
        let _ = std::fs::remove_file(output);
        None
    }
}

/// 探测时长（毫秒）。拿不到返回 `None` —— 不瞎填一个数。
pub fn duration_ms(path: &Path) -> Option<i64> {
    let out = std::process::Command::new(tool("ffprobe")?)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let secs: f64 = s.trim().parse().ok()?;
    Some((secs * 1000.0).round() as i64)
}

/// 存一份语音，返回元数据（不含 id —— 由调用方生成并入库）。
///
/// 返回 `(最终文件名, 实际字节数, 时长)`。
pub fn store_file(dir: &Path, id: &str, data: &[u8]) -> Result<(String, i64, Option<i64>)> {
    std::fs::create_dir_all(dir)?;
    let (ext, already_m4a) = sniff(data);

    // 先写到临时文件（扩展名按魔数），需要转换时再产出 m4a。
    let tmp_in = dir.join(format!("{id}.in.{ext}"));
    std::fs::write(&tmp_in, data)?;

    if already_m4a || ext == "m4a" {
        let name = format!("{id}.m4a");
        std::fs::rename(&tmp_in, dir.join(&name))?;
        let dur = duration_ms(&dir.join(&name));
        return Ok((name, data.len() as i64, dur));
    }

    // 试着转 m4a
    let target = dir.join(format!("{id}.m4a"));
    if to_m4a(&tmp_in, &target).is_some() {
        let _ = std::fs::remove_file(&tmp_in);
        let bytes = std::fs::metadata(&target)
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        let dur = duration_ms(&target);
        return Ok((format!("{id}.m4a"), bytes, dur));
    }

    // 转不了（没 ffmpeg / 格式不认）：原样保留，扩展名用真实的。
    let name = format!("{id}.{ext}");
    std::fs::rename(&tmp_in, dir.join(&name))?;
    let dur = duration_ms(&dir.join(&name));
    Ok((name, data.len() as i64, dur))
}

/// 解析出可安全用于文件名的 id：只留字母数字与 `-`。
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

/// 语音文件路径（拒绝任何越界访问）。
pub fn resolve_file(dir: &Path, file: &str) -> Option<PathBuf> {
    // 文件名必须是 `<安全id>.<ext>`：不含分隔符、不含 `..`
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

/// 校验评分范围。
pub fn check_rating(r: i64) -> Result<i64> {
    if (1..=5).contains(&r) {
        Ok(r)
    } else {
        Err(Error::BadRequest("评分只能是 1–5".into()))
    }
}

// ================================================================== 查询

/// 列表。`rating` 过滤：只给打过分的（用来挑训练素材）。
pub fn list(db: &crate::db::Db, only_rated: bool, limit: usize) -> Result<Vec<VoiceItem>> {
    let limit = limit.clamp(1, 500);
    db.with_read(|c| {
        let sql = if only_rated {
            "SELECT id, file, duration_ms, bytes, text, speaker, voice, style,
                    rating, note, channel, created_ms, updated_ms
             FROM voice WHERE deleted_by IS NULL AND rating IS NOT NULL
             ORDER BY rating DESC, created_ms DESC LIMIT ?1"
        } else {
            "SELECT id, file, duration_ms, bytes, text, speaker, voice, style,
                    rating, note, channel, created_ms, updated_ms
             FROM voice WHERE deleted_by IS NULL
             ORDER BY created_ms DESC LIMIT ?1"
        };
        let mut stmt = c.prepare(sql)?;
        let items = stmt
            .query_map(rusqlite::params![limit as i64], |r| {
                Ok(VoiceItem {
                    id: r.get(0)?,
                    file: r.get(1)?,
                    duration_ms: r.get(2)?,
                    bytes: r.get(3)?,
                    text: r.get(4)?,
                    speaker: r.get(5)?,
                    voice: r.get(6)?,
                    style: r.get(7)?,
                    rating: r.get(8)?,
                    note: r.get(9)?,
                    channel: r.get(10)?,
                    created_ms: r.get(11)?,
                    updated_ms: r.get(12)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    })
}

/// 取一条（含文件名，用于下载）。
pub fn get(db: &crate::db::Db, id: &str) -> Result<Option<VoiceItem>> {
    db.with_read(|c| {
        let v = c
            .query_row(
                "SELECT id, file, duration_ms, bytes, text, speaker, voice, style,
                        rating, note, channel, created_ms, updated_ms
                 FROM voice WHERE id = ?1 AND deleted_by IS NULL",
                rusqlite::params![id],
                |r| {
                    Ok(VoiceItem {
                        id: r.get(0)?,
                        file: r.get(1)?,
                        duration_ms: r.get(2)?,
                        bytes: r.get(3)?,
                        text: r.get(4)?,
                        speaker: r.get(5)?,
                        voice: r.get(6)?,
                        style: r.get(7)?,
                        rating: r.get(8)?,
                        note: r.get(9)?,
                        channel: r.get(10)?,
                        created_ms: r.get(11)?,
                        updated_ms: r.get(12)?,
                    })
                },
            )
            .ok();
        Ok(v)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 按魔数认格式而不是文件名() {
        let wav = b"RIFF\x00\x00\x00\x00WAVEfmt ";
        assert_eq!(sniff(wav).0, "wav");
        // 注意要有 12 字节以上：`sniff` 对短输入直接当 bin
        let mp3 = b"ID3\x04\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        assert_eq!(sniff(mp3).0, "mp3");
        // 短输入不该 panic
        assert_eq!(sniff(b"ID3").0, "bin");
        let m4a = b"\x00\x00\x00\x20ftypM4A \x00\x00\x00\x00";
        assert_eq!(sniff(m4a), ("m4a", true));
        let ogg = b"OggS\x00\x00\x00\x00\x00\x00\x00\x00";
        assert_eq!(sniff(ogg).0, "ogg");
        assert_eq!(sniff(b"nope").0, "bin");
    }

    #[test]
    fn 文件名不能越界() {
        let dir = std::path::Path::new("/tmp/voices");
        assert!(resolve_file(dir, "../../etc/passwd").is_none());
        assert!(resolve_file(dir, "a/b.m4a").is_none());
        // 合法名但文件不存在 → 也是 None（不泄露存在性）
        assert!(resolve_file(dir, "abc.m4a").is_none());
    }

    #[test]
    fn id_只留安全字符() {
        assert_eq!(safe_id("abc-123_x").as_deref(), Some("abc-123_x"));
        assert_eq!(safe_id("../../x").as_deref(), Some("x"));
        assert!(safe_id("日本語").is_none());
        assert!(safe_id("").is_none());
    }

    #[test]
    fn 评分范围() {
        assert!(check_rating(1).is_ok());
        assert!(check_rating(5).is_ok());
        assert!(check_rating(0).is_err());
        assert!(check_rating(6).is_err());
    }
}
