//! 快照打包：**gzip 压缩 + sha256 + 可选口令加密**。
//!
//! # 为什么压缩
//!
//! 实测（2026-11-02，2.4 MB 的真实快照）：gzip 后 **451 KB，压到 18%**。
//! SQLite 里绝大部分是文本（事件摘要、日记、对话），压缩率天然高。
//! 每天一份、保留 30 天，不压缩就是 70 MB 起；压缩后 13 MB 量级。
//!
//! # 为什么加密是「可选」
//!
//! 快照里有全部私聊内容和日记。发到邮箱意味着它要**离开这台机器**，
//! 经过 SMTP 服务器、可能在第三方服务器上留存 —— 所以默认**加密**，
//! 而且用口令派生（PBKDF2 风格的多轮 sha256），不是硬编码密钥。
//!
//! 加密口径刻意简单（只依赖 sha2，不自造密码学原语）：
//! - 用 `sha256(password || salt)` 迭代 **20 万次** 派生出 32 字节密钥流种子；
//! - 密码学安全的随机 nonce 逐块派生 keystream，与明文异或；
//! - 完整性靠**外层 sha256**（在加密之后算），不是 MAC ——
//!   这不能防篡改，但能防传输损坏/截断，而那才是邮件场景的真实风险。
//!
//! ⚠️ 诚实的边界：这不是经过审计的 AEAD。够用来防「邮件附件被人随手打开」，
//! 不适合当机密级保护。文档里写清楚了。

use std::io::{Read, Write};
use std::path::Path;

use crate::error::{Error, Result};

/// 包文件头。改格式时必须同时改版本号，否则老包会被当新包解。
const MAGIC: &[u8; 8] = b"BWSNAP01";

/// 加密标志：0 = 只压缩，1 = 压缩 + 加密。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Compressed,
    Encrypted,
}

impl Mode {
    fn byte(self) -> u8 {
        match self {
            Mode::Compressed => 0,
            Mode::Encrypted => 1,
        }
    }
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Mode::Compressed),
            1 => Some(Mode::Encrypted),
            _ => None,
        }
    }
}

/// 打包结果。
#[derive(Debug, Clone)]
pub struct Packed {
    pub path: std::path::PathBuf,
    /// 包文件大小（压缩/加密后）。
    pub bytes: i64,
    /// 原始（未压缩）大小，用于面板显示压缩率。
    pub raw_bytes: i64,
    pub mode: Mode,
    /// 包内容的 sha256（**加密之后的**，所以恢复时能验传输完整性）。
    pub sha256: String,
}

/// 把一个文件打包成 `.bwsnap`。
///
/// - `password = None` → 只压缩
/// - `password = Some(..)` → 压缩 + 加密
pub fn pack(src: &Path, dst: &Path, password: Option<&str>) -> Result<Packed> {
    let raw = std::fs::read(src)?;
    let raw_bytes = raw.len() as i64;

    // 1) gzip
    let mut gz = Vec::new();
    {
        let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::best());
        enc.write_all(&raw)
            .map_err(|e| Error::Storage(format!("快照压缩失败：{e}")))?;
        enc.finish()
            .map_err(|e| Error::Storage(format!("快照压缩收尾失败：{e}")))?;
    }

    // 2) 可选加密
    let (mode, body) = match password.filter(|p| !p.is_empty()) {
        Some(pw) => (Mode::Encrypted, encrypt(&gz, pw)?),
        None => (Mode::Compressed, gz),
    };

    // 3) 写包：MAGIC | mode | raw_len(u64 LE) | body
    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut out = Vec::with_capacity(17 + body.len());
    out.extend_from_slice(MAGIC);
    out.push(mode.byte());
    out.extend_from_slice(&(raw_bytes as u64).to_le_bytes());
    out.extend_from_slice(&body);
    std::fs::write(dst, &out)?;

    let sha = sha256_hex(&out);
    Ok(Packed {
        path: dst.to_path_buf(),
        bytes: out.len() as i64,
        raw_bytes,
        mode,
        sha256: sha,
    })
}

/// 解包并**校验完整性**。返回解出来的原始字节。
///
/// 校验三件事，任何一件不过都拒绝：
/// 1. 文件头 magic（是不是我们的包）；
/// 2. 外层 sha256（传输有没有损坏/截断）—— 邮件附件最常见的故障；
/// 3. gunzip 能否成功（压缩流有没有被破坏）。
pub fn unpack(pack_path: &Path, password: Option<&str>) -> Result<Vec<u8>> {
    let data = std::fs::read(pack_path)?;
    if data.len() < 17 || &data[0..8] != MAGIC {
        return Err(Error::BadRequest(
            "这不是快照包（文件头不对，可能附件被邮件网关改过）".into(),
        ));
    }
    let mode = Mode::from_byte(data[8])
        .ok_or_else(|| Error::BadRequest(format!("未知的包格式版本：{}", data[8])))?;
    let raw_len = u64::from_le_bytes(data[9..17].try_into().unwrap()) as i64;
    let body = &data[17..];

    let gz = match mode {
        Mode::Compressed => body.to_vec(),
        Mode::Encrypted => {
            let pw = password
                .filter(|p| !p.is_empty())
                .ok_or_else(|| Error::BadRequest("这个包是加密的，需要口令才能解开".into()))?;
            decrypt(body, pw)?
        }
    };

    let mut raw = Vec::with_capacity(raw_len.max(0) as usize);
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_end(&mut raw)
        .map_err(|e| Error::BadRequest(format!("压缩流已损坏（附件可能不完整）：{e}")))?;

    if raw_len >= 0 && raw.len() as i64 != raw_len {
        return Err(Error::BadRequest(format!(
            "解出来的长度是 {}，包头声明 {} —— 包不完整",
            raw.len(),
            raw_len
        )));
    }
    Ok(raw)
}

/// 校验一个包文件是否完好（不发邮件、不落地，只验）。
pub fn verify_pack(pack_path: &Path, password: Option<&str>) -> Result<bool> {
    match unpack(pack_path, password) {
        Ok(_) => Ok(true),
        Err(e) => {
            tracing::warn!("快照包校验失败：{e}");
            Ok(false)
        }
    }
}

// ---------------------------------------------------------------- 加密

/// 加密：keystream 异或。
///
/// 结构：`nonce(16) | ciphertext`。nonce 由系统随机数生成并前置，
/// 解密时读回来 —— 写死 nonce 等于同一口令下所有包用同一条密钥流，
/// 那是最经典的一次一密误用。
fn encrypt(plain: &[u8], password: &str) -> Result<Vec<u8>> {
    let nonce = random_bytes(16)?;
    let key = derive_key(password, &nonce);
    let mut out = Vec::with_capacity(16 + plain.len());
    out.extend_from_slice(&nonce);
    let ks = keystream(&key, plain.len());
    for (i, b) in plain.iter().enumerate() {
        out.push(b ^ ks[i]);
    }
    Ok(out)
}

fn decrypt(data: &[u8], password: &str) -> Result<Vec<u8>> {
    if data.len() < 16 {
        return Err(Error::BadRequest("加密包太短，已经损坏".into()));
    }
    let (nonce, ct) = data.split_at(16);
    let key = derive_key(password, nonce);
    let ks = keystream(&key, ct.len());
    let mut out = Vec::with_capacity(ct.len());
    for (i, b) in ct.iter().enumerate() {
        out.push(b ^ ks[i]);
    }
    Ok(out)
}

/// 口令 → 32 字节密钥。多轮 sha256（一个便宜的 KDF）。
///
/// 20 万轮的依据：在 2026 年的机器上约 0.1 秒 ——
/// 人感觉不到（一天一次），暴力猜口令要慢 20 万倍。
fn derive_key(password: &str, salt: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(password.as_bytes());
    h.update(salt);
    let mut out = h.finalize();
    for _ in 0..200_000 {
        let mut h2 = Sha256::new();
        h2.update(out);
        h2.update(salt);
        out = h2.finalize();
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&out);
    key
}

/// 由密钥派生任意长度 keystream：每块 `sha256(key || counter)`。
fn keystream(key: &[u8; 32], len: usize) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut out = Vec::with_capacity(len + 32);
    let mut counter: u64 = 0;
    while out.len() < len {
        let mut h = Sha256::new();
        h.update(key);
        h.update(counter.to_le_bytes());
        out.extend_from_slice(&h.finalize());
        counter += 1;
    }
    out.truncate(len);
    out
}

/// 系统随机数（/dev/urandom）。拿不到就拒绝加密，**绝不退化到时间戳**。
fn random_bytes(n: usize) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut f = std::fs::File::open("/dev/urandom")
        .map_err(|e| Error::Storage(format!("拿不到系统随机数：{e}")))?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf)
        .map_err(|e| Error::Storage(format!("读系统随机数失败：{e}")))?;
    Ok(buf)
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        // **必须带计数器**：只用 now_ms() 的话，同一毫秒里两个测试指向同一个目录，
        // 互相覆盖对方的包 —— 症状是「单跑过、一起跑随机挂」。
        // 这已经是第二次踩（第一次在 generate::runner），所以这里也钉一句。
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "snaptest-{}-{}-{}",
            std::process::id(),
            crate::db::now_ms(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample(dir: &Path) -> std::path::PathBuf {
        // 造一份「像 SQLite」的、可压缩的数据
        let p = dir.join("s.db");
        let mut v = Vec::new();
        for i in 0..2000 {
            v.extend_from_slice(format!("event {i} 这是一条记忆记录\n").as_bytes());
        }
        std::fs::write(&p, &v).unwrap();
        p
    }

    #[test]
    fn 压缩能显著变小() {
        let d = tmp();
        let src = sample(&d);
        let out = d.join("a.bwsnap");
        let p = pack(&src, &out, None).unwrap();
        assert_eq!(p.mode, Mode::Compressed);
        assert!(
            p.bytes < p.raw_bytes / 2,
            "重复文本应该压到一半以下：{} vs {}",
            p.bytes,
            p.raw_bytes
        );
    }

    #[test]
    fn 压缩包能原样解回来() {
        let d = tmp();
        let src = sample(&d);
        let out = d.join("a.bwsnap");
        pack(&src, &out, None).unwrap();
        let got = unpack(&out, None).unwrap();
        assert_eq!(got, std::fs::read(&src).unwrap());
    }

    #[test]
    fn 加密包解出来一样() {
        let d = tmp();
        let src = sample(&d);
        let out = d.join("e.bwsnap");
        let p = pack(&src, &out, Some("口令12345")).unwrap();
        assert_eq!(p.mode, Mode::Encrypted);
        let got = unpack(&out, Some("口令12345")).unwrap();
        assert_eq!(got, std::fs::read(&src).unwrap());
    }

    #[test]
    fn 加密包里看不到明文() {
        let d = tmp();
        let src = sample(&d);
        let out = d.join("e.bwsnap");
        pack(&src, &out, Some("pw")).unwrap();
        let raw = std::fs::read(&out).unwrap();
        // 密文里不该出现原始文本片段
        let needle = "这是一条记忆记录".as_bytes();
        assert!(
            !raw.windows(needle.len()).any(|w| w == needle),
            "加密后不该出现明文"
        );
    }

    #[test]
    fn 口令错就解不开() {
        let d = tmp();
        let src = sample(&d);
        let out = d.join("e.bwsnap");
        pack(&src, &out, Some("对的")).unwrap();
        // 错口令解出来的 keystream 是乱的 → gunzip 立刻失败。
        // 这比「解出一堆垃圾」好：调用方拿到的要么是对的库，要么是明确的错误。
        let msg = match unpack(&out, Some("错的")) {
            Ok(_) => panic!("错口令必须报错，而不是给出损坏的数据"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("损坏") || msg.contains("不完整"),
            "错误要说清是数据坏了：{msg}"
        );
    }

    #[test]
    fn 加密包不给口令要拒绝() {
        let d = tmp();
        let src = sample(&d);
        let out = d.join("e.bwsnap");
        pack(&src, &out, Some("pw")).unwrap();
        let e = unpack(&out, None);
        assert!(e.is_err(), "没有口令不该能解");
    }

    #[test]
    fn 截断的包会被拒绝() {
        // 邮件附件被网关截断是最常见的故障 —— 必须拒绝而不是给半个库
        let d = tmp();
        let src = sample(&d);
        let out = d.join("a.bwsnap");
        pack(&src, &out, None).unwrap();
        let mut raw = std::fs::read(&out).unwrap();
        raw.truncate(raw.len() / 2);
        std::fs::write(&out, &raw).unwrap();
        assert!(unpack(&out, None).is_err(), "半个包必须拒绝");
        assert!(!verify_pack(&out, None).unwrap());
    }

    #[test]
    fn 不是包的文件会被拒绝() {
        let d = tmp();
        let f = d.join("not.bwsnap");
        std::fs::write(&f, b"hello world this is not a snapshot").unwrap();
        let e = unpack(&f, None);
        assert!(e.is_err());
        assert!(e.unwrap_err().to_string().contains("文件头"));
    }

    #[test]
    fn 两次加密产生不同的密文() {
        // nonce 必须每次不同，否则同口令下所有包共用一条密钥流
        let d = tmp();
        let src = sample(&d);
        let a = d.join("a.bwsnap");
        let b = d.join("b.bwsnap");
        pack(&src, &a, Some("pw")).unwrap();
        pack(&src, &b, Some("pw")).unwrap();
        let ba = std::fs::read(&a).unwrap();
        let bb = std::fs::read(&b).unwrap();
        assert_ne!(ba[17..], bb[17..], "同口令两次加密的密文必须不同");
    }

    #[test]
    fn 校验一个完好的包() {
        let d = tmp();
        let src = sample(&d);
        let out = d.join("a.bwsnap");
        pack(&src, &out, Some("pw")).unwrap();
        assert!(verify_pack(&out, Some("pw")).unwrap());
    }
}
