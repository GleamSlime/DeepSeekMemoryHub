//! 批量检验承诺识别：拿线上真实台词跑一遍，看误报率。
//!
//! 为什么需要它：只靠几条单测挡不住「规则太松」—— 线上几百条真实回复里
//! 有多少会被认成承诺，只有真跑一遍才知道。
//!
//! 用法：`cargo run --example detect_probe -- < 台词文件`（每行一条）
fn main() {
    use std::io::BufRead;
    let now = 1_800_000_000_000i64;
    let stdin = std::io::stdin();
    let mut total = 0;
    let mut hit = 0;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { continue };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        total += 1;
        if let Some(d) = bluewhale_memory::promise_detect::detect(line, now) {
            hit += 1;
            println!("✓ {}", d.description);
        }
    }
    eprintln!(
        "\n{hit}/{total} 被判成承诺（{:.1}%）",
        hit as f64 / total.max(1) as f64 * 100.0
    );
}
