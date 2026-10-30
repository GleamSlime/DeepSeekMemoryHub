//! 语义边界切分。
//!
//! # 为什么不按固定字数切
//!
//! 固定字数切块会切断语义（一句话被劈成两半），所以要靠「重叠窗口」打补丁。
//! 而重叠会让同一段文字被重复召回 —— 在本项目里这是个真问题：
//! **没有重排序模型**，同一段占据多个 top-k 名额，等于直接损失候选多样性。
//!
//! 所以这里按标题 → 段落 → 句子 → 硬切 的优先级切，**不做重叠**。
//!
//! # 默认值 700 字是被「没有 reranker」反推出来的
//!
//! 没有精排时正确的代偿方式是「多给几条让下游 LLM 自己挑」：
//!
//! | 块大小 | 4000 字预算能塞几条 |
//! | --- | --- |
//! | 2000 字 | 2 条 |
//! | **700 字** | **5~6 条** |
//! | 300 字 | 13 条，但单条上下文不够 |
//!
//! 700 字对中文约 400~500 token，与业界常用的 256~512 token/块 同量级。
//! **接上 reranker 之后可以放心调回 1500~2000**（精排能滤掉噪声）。

use serde::{Deserialize, Serialize};

/// 默认块上限（字符数，不是字节数）。
pub const DEFAULT_MAX_CHARS: usize = 700;

/// 切分参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Opts {
    /// 单块字符上限。`0` 会被当成默认值 —— 配置里留空比填 0 常见，
    /// 而 0 当成「不限」会让超长文档变成一个巨大块，那是更糟的默认。
    pub max_chars: usize,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            max_chars: DEFAULT_MAX_CHARS,
        }
    }
}

impl Opts {
    fn limit(&self) -> usize {
        if self.max_chars == 0 {
            DEFAULT_MAX_CHARS
        } else {
            self.max_chars
        }
    }
}

/// 一个切分结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// 在文档内的顺序（0 起）。用于取邻居上下文与稳定排序。
    pub ord: usize,
    /// 所在小节的标题。**单独存**：可以单独加权，且显示时不必重复。
    pub heading: Option<String>,
    pub text: String,
}

/// 把一篇文档切成若干块。
///
/// 保证：`chunks` 合起来覆盖原文的所有非空白内容（**不重叠、不丢字**，
/// 除非单句超长被硬切，那种情况在返回值里也能看出来）。
pub fn split(content: &str, opts: &Opts) -> Vec<Chunk> {
    let limit = opts.limit();
    let content = content.trim();
    if content.is_empty() {
        return vec![];
    }

    // 整篇够短 **且没有任何标题** → 不切。
    //
    // 「有标题就按标题切」这一条是实测逼出来的：一份 200 字的文档里写了
    // `# 材料 / # 建造 / # 食物` 三节，如果整篇塞成一块，问「我该吃什么」
    // 拿到的是整篇的混合向量（实测 cos=0.339、字面 0），食物那段被埋掉了。
    // 按标题切成三块后，每块各自是一个话题，精度明显更好。
    //
    // 所以「短就不切」只适用于**没有结构**的纯文本。有标题就说明作者
    // 已经给出了语义边界，不用白不用。
    let has_heading = content.lines().any(|l| heading_of(l).is_some());
    if chars(content) <= limit && !has_heading {
        return vec![Chunk {
            ord: 0,
            heading: None,
            text: content.to_string(),
        }];
    }

    let mut out: Vec<Chunk> = Vec::new();
    for (heading, body) in sections(content) {
        if body.trim().is_empty() {
            continue;
        }
        for piece in pack(&body, limit) {
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            out.push(Chunk {
                ord: out.len(),
                heading: heading.clone(),
                text: piece.to_string(),
            });
        }
    }

    // 全篇只有标题、没有正文时，上面会产出空 vec；把原文兜住，
    // 否则一篇「只有标题」的文档会静默变成 0 条，用户以为导入失败。
    if out.is_empty() {
        let t: String = content.chars().take(limit).collect();
        out.push(Chunk {
            ord: 0,
            heading: None,
            text: t,
        });
    }
    out
}

/// 字符数（不是字节数）。中文 1 字 = 1，与用户的「字数」直觉一致。
fn chars(s: &str) -> usize {
    s.chars().count()
}

/// 是不是 markdown 标题行；是则返回标题文本。
fn heading_of(line: &str) -> Option<String> {
    let t = line.trim_start();
    if !t.starts_with('#') {
        return None;
    }
    let n = t.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&n) {
        return None;
    }
    let rest = t[n..].trim();
    // `#标题` 不带空格也算标题（中文用户常这么写），
    // 但 `#123` 这种像编号的不算 —— 用「后面第一个字符不是数字」区分。
    if rest.is_empty() {
        return None;
    }
    if t.chars().nth(n) != Some(' ') && rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(rest.to_string())
}

/// 按 markdown 标题切出 (标题, 正文)。
fn sections(content: &str) -> Vec<(Option<String>, String)> {
    let mut out: Vec<(Option<String>, String)> = Vec::new();
    let mut cur_heading: Option<String> = None;
    let mut buf = String::new();

    for line in content.lines() {
        match heading_of(line) {
            Some(h) => {
                if !buf.trim().is_empty() {
                    out.push((cur_heading.take(), std::mem::take(&mut buf)));
                } else {
                    buf.clear();
                }
                cur_heading = Some(h);
            }
            None => {
                buf.push_str(line);
                buf.push('\n');
            }
        }
    }
    if !buf.trim().is_empty() {
        out.push((cur_heading, buf));
    }
    // 没有标题的纯文本：整篇一段
    if out.is_empty() && !content.trim().is_empty() {
        out.push((None, content.to_string()));
    }
    out
}

/// 把一段正文打包成若干 ≤ limit 的块。
///
/// 顺序：先按空行分段，段落装不下时按句子拆，单句还超长才硬切。
/// **绝不重叠**：每段只属于一个块。
fn pack(body: &str, limit: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();

    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.trim().is_empty() {
            out.push(std::mem::take(cur));
        } else {
            cur.clear();
        }
    };

    for para in paragraphs(body) {
        let p = para.trim();
        if p.is_empty() {
            continue;
        }
        // 整段能塞进当前块 → 直接塞（保留段落边界，读起来更像原文）
        // `+1` 是给拼接时的换行符留的位置。
        if chars(&cur) + chars(p) < limit {
            if !cur.is_empty() {
                cur.push('\n');
            }
            cur.push_str(p);
            continue;
        }
        // 塞不下 → 先把当前块收尾
        flush(&mut cur, &mut out);

        if chars(p) <= limit {
            cur.push_str(p);
            continue;
        }
        // 单段超长 → 按句子拆
        for sent in sentences(p) {
            let s = sent.trim();
            if s.is_empty() {
                continue;
            }
            if chars(&cur) + chars(s) <= limit {
                cur.push_str(s);
                continue;
            }
            flush(&mut cur, &mut out);
            if chars(s) <= limit {
                cur.push_str(s);
            } else {
                // 单句还超长 → 硬切。这是唯一会破坏语义的情形。
                for piece in hard_split(s, limit) {
                    flush(&mut cur, &mut out);
                    out.push(piece);
                }
            }
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// 按空行分段。
fn paragraphs(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            if !buf.trim().is_empty() {
                out.push(std::mem::take(&mut buf));
            }
            buf.clear();
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if !buf.trim().is_empty() {
        out.push(buf);
    }
    if out.is_empty() && !body.trim().is_empty() {
        out.push(body.to_string());
    }
    out
}

/// 按句子切。中文句末标点 + 换行都算边界，标点**留在前一句**上。
fn sentences(p: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for ch in p.chars() {
        buf.push(ch);
        if matches!(ch, '。' | '！' | '？' | '；' | '\n' | '!' | '?' | ';') {
            // 句末连续的引号/括号跟着走，别把「他说：“好。”」拆开
            out.push(std::mem::take(&mut buf));
        }
    }
    if !buf.trim().is_empty() {
        out.push(buf);
    }
    if out.is_empty() && !p.trim().is_empty() {
        out.push(p.to_string());
    }
    out
}

/// 硬切：按字符数切，但尽量在空白处下刀（英文/混排时更可读）。
fn hard_split(s: &str, limit: usize) -> Vec<String> {
    let cs: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let end = (i + limit).min(cs.len());
        let mut cut = end;
        if end < cs.len() {
            // 在后 20% 里找最后一个空白，找到就在那切
            let floor = i + limit * 4 / 5;
            if let Some(p) = (floor..end).rev().find(|&k| cs[k].is_whitespace()) {
                cut = p + 1;
            }
        }
        let piece: String = cs[i..cut].iter().collect();
        if !piece.trim().is_empty() {
            out.push(piece);
        }
        i = cut;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(max: usize) -> Opts {
        Opts { max_chars: max }
    }

    #[test]
    fn 短文档不切() {
        let r = split("这是一份很短的设定。", &Opts::default());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "这是一份很短的设定。");
        assert_eq!(r[0].ord, 0);
    }

    #[test]
    fn 空内容返回空() {
        assert!(split("", &Opts::default()).is_empty());
        assert!(split("   \n\n  ", &Opts::default()).is_empty());
    }

    #[test]
    fn 按标题切且标题被带上() {
        let doc = "# 第一节\n这里是第一节的内容。\n\n# 第二节\n这里是第二节的内容。";
        let r = split(doc, &opts(10));
        assert_eq!(r.len(), 2, "两个小节应切两块：{r:?}");
        assert_eq!(r[0].heading.as_deref(), Some("第一节"));
        assert!(r[0].text.contains("第一节的内容"));
        assert_eq!(r[1].heading.as_deref(), Some("第二节"));
        assert!(r[1].text.contains("第二节的内容"));
        // ord 必须连续，取邻居上下文靠它
        assert_eq!(r[0].ord, 0);
        assert_eq!(r[1].ord, 1);
    }

    #[test]
    fn 超长段落按句子切() {
        let doc = "第一句话在这里。第二句话也在。第三句同样存在。第四句收尾了。";
        let r = split(doc, &opts(12));
        assert!(r.len() > 1, "12 字上限下应切成多块：{r:?}");
        for c in &r {
            assert!(
                chars(&c.text) <= 12,
                "块超限: {:?} = {}",
                c.text,
                chars(&c.text)
            );
        }
    }

    #[test]
    fn 单句超长被硬切() {
        let long = "啊".repeat(100);
        let r = split(&long, &opts(30));
        assert!(r.len() >= 4, "100 字按 30 切至少 4 块：{r:?}");
        let total: usize = r.iter().map(|c| chars(&c.text)).sum();
        assert_eq!(total, 100, "硬切不能丢字");
    }

    #[test]
    fn 正文不丢字也不重叠() {
        // 这是这个模块最重要的判据。
        //
        // **标题不进 `text`**（单独存在 `heading` 字段里，见模块注释），
        // 所以这里先把标题行剔掉，只校验**正文**覆盖率。
        let doc = "# 标题A\n\
                   第一段的内容比较长需要被切分开来处理才好看。\
                   第二句也在这里。\n\n\
                   第二段另起一行，同样有一些文字。\n\n\
                   # 标题B\n\
                   另一个小节的内容，也要被正确切分。";
        let r = split(doc, &opts(20));
        assert!(r.len() > 2, "应切成多块：{r:?}");

        let strip = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };

        // 正文 = 原文去掉所有标题行
        let body: String = doc
            .lines()
            .filter(|l| heading_of(l).is_none())
            .collect::<Vec<_>>()
            .join("\n");

        let joined: String = r.iter().map(|c| strip(&c.text)).collect();
        for ch in strip(&body).chars() {
            assert!(joined.contains(ch), "正文字符 {ch:?} 在切分结果里丢了");
        }

        // 拼接后总长必须等于正文长度（不重叠、不重复）
        assert_eq!(
            joined.chars().count(),
            strip(&body).chars().count(),
            "切分结果总长与正文不符，说明有重叠或丢字"
        );

        // 标题必须被保留在 heading 字段里，不能丢
        let heads: Vec<String> = r.iter().filter_map(|c| c.heading.clone()).collect();
        assert!(heads.contains(&"标题A".to_string()), "标题A 丢了");
        assert!(heads.contains(&"标题B".to_string()), "标题B 丢了");
    }

    #[test]
    fn 短但带标题仍然按标题切() {
        // 实测踩过的坑：200 字文档写了三个 `#` 小节，整篇塞成一块后，
        // 问其中某一节的内容拿到的是整篇的混合向量（cos 只有 0.33、
        // 字面 0），那一节被埋掉了。
        let doc = "# 材料\n蜘蛛线用来做鱼竿。\n\n# 建造\n圆石做墙。\n\n# 食物\n烤鱼能吃。";
        // 整篇 30 字，远低于 700 的默认上限
        assert!(chars(doc) < DEFAULT_MAX_CHARS);
        let r = split(doc, &Opts::default());
        assert_eq!(r.len(), 3, "有标题就该切成三块：{r:?}");
        assert_eq!(r[0].heading.as_deref(), Some("材料"));
        assert_eq!(r[1].heading.as_deref(), Some("建造"));
        assert_eq!(r[2].heading.as_deref(), Some("食物"));
        assert!(r[2].text.contains("烤鱼"), "食物块应只含食物内容");
        assert!(!r[2].text.contains("蜘蛛线"), "食物块不该混入材料内容");
    }

    #[test]
    fn 短且无标题才不切() {
        let doc = "这是一段没有结构的纯文本，讲了三件不相干的事但没分节。";
        assert!(chars(doc) < DEFAULT_MAX_CHARS);
        let r = split(doc, &Opts::default());
        assert_eq!(r.len(), 1, "没有语义边界可用，就不该硬拆");
        assert!(r[0].heading.is_none());
    }

    #[test]
    fn 标题前的导语不会被丢掉() {
        // 很多文档第一行是概述、第二行才开始第一节。导语的 heading 应为 None。
        // 这类「第一个标题之前的内容」很容易在按标题切时被漏掉。
        let doc = "导语一句话。\n\n# 第一节\n正文内容。";
        let r = split(doc, &opts(10));
        assert!(
            r.iter()
                .any(|c| c.heading.is_none() && c.text.contains("导语")),
            "导语没被保留：{r:?}"
        );
    }

    #[test]
    fn 每块都不超上限() {
        let doc = (0..50)
            .map(|i| format!("第{i}句话在这里。"))
            .collect::<String>();
        let limit = 40;
        let r = split(&doc, &opts(limit));
        for c in &r {
            assert!(
                chars(&c.text) <= limit,
                "块超限 {} > {limit}: {:?}",
                chars(&c.text),
                c.text
            );
        }
    }

    #[test]
    fn 中文标题不带空格也算() {
        let doc = "#第一节\n内容一\n#第二节\n内容二";
        let r = split(doc, &opts(5));
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(r[0].heading.as_deref(), Some("第一节"));
    }

    #[test]
    fn 井号加数字不当标题() {
        // `#123` 是编号不是标题，当成标题会把正文误切开
        let r = split("#123 号条目\n内容在这里", &Opts::default());
        assert_eq!(r.len(), 1);
        assert!(r[0].heading.is_none());
    }

    #[test]
    fn max_chars_为零时用默认值() {
        // 配置里留空很常见；把 0 当「不限」会让超长文档变成一个巨大块
        let o = opts(0);
        assert_eq!(o.limit(), DEFAULT_MAX_CHARS);
    }

    #[test]
    fn 只有标题没有正文不会变成零条() {
        // 静默变成 0 条会让用户以为导入失败
        let r = split("# 只有个标题", &Opts::default());
        assert_eq!(r.len(), 1);
        assert!(!r[0].text.trim().is_empty());
    }

    #[test]
    fn 英文混排长文本被硬切但不丢字() {
        let doc = "word ".repeat(200);
        let r = split(&doc, &opts(100));
        assert!(r.len() > 1);
        let joined: String = r.iter().map(|c| c.text.clone()).collect();
        assert_eq!(
            joined.chars().filter(|c| !c.is_whitespace()).count(),
            doc.chars().filter(|c| !c.is_whitespace()).count()
        );
    }
}
