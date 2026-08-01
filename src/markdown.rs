//! Markdown プレビュー描画 — 依存追加なしの軽量レンダラ。
//!
//! エディタで開いている .md をレンダリングして表示するためのモジュール。
//! CommonMark 完全準拠は狙わず、README / メモ用途で実用になる範囲を自前実装する:
//! 見出し・段落・箇条書き(ネスト/番号/タスク)・引用・水平線・テーブル・
//! フェンスコード(syntect ハイライト)・インライン装飾(強調/斜体/打消/コード/リンク)。
//!
//! 描画は egui の `horizontal_wrapped` + スパン単位の Label で行い、
//! リンクは `Hyperlink` としてクリックでブラウザが開く。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, FontId, RichText};

use crate::highlight::Highlighter;
use crate::i18n::{tr, trf};
use crate::theme::Theme;

/// このバッファを Markdown としてプレビュー可能か。
pub fn is_markdown(title: &str, lang: &str) -> bool {
    let t = title.to_lowercase();
    lang == "Markdown" || t.ends_with(".md") || t.ends_with(".markdown") || t.ends_with(".mdx")
}

// ─── 入力サイズの上限 ───────────────────────────────────────────────
//
// プレビューは 1 行ずつ egui のウィジェットを積むため、文書の大きさが
// そのままフレーム時間になる。10MB のログのようなものを開いても UI が
// 固まらないよう、描画対象を先頭側だけに切り詰めて「切り詰めた」と伝える。

/// プレビューで描画する最大バイト数。
pub const MAX_PREVIEW_BYTES: usize = 512 * 1024;
/// プレビューで描画する最大行数。
pub const MAX_PREVIEW_LINES: usize = 20_000;

/// 描画対象を上限まで切り詰める。返り値は (切り詰め後, 切り詰めたか)。
/// 切る位置は必ず文字境界で、可能なら直前の改行に合わせる。
pub fn cap_input(text: &str) -> (&str, bool) {
    let mut end = text.len();
    let mut cut = false;
    if end > MAX_PREVIEW_BYTES {
        end = MAX_PREVIEW_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        // 行の途中で切らない (直前の改行まで戻す。近くに無ければそのまま)
        if let Some(nl) = text[..end].rfind('\n') {
            if end - nl < 4096 {
                end = nl;
            }
        }
        cut = true;
    }
    let head = &text[..end];
    // 行数の上限 (1 行が極端に短い文書対策)
    if head.lines().count() > MAX_PREVIEW_LINES {
        let mut n = 0;
        let mut off = 0;
        for (idx, ch) in head.char_indices() {
            if ch == '\n' {
                n += 1;
                if n >= MAX_PREVIEW_LINES {
                    off = idx;
                    break;
                }
            }
        }
        if off > 0 {
            return (&head[..off], true);
        }
    }
    (head, cut)
}

/// 切り詰めをユーザーへ伝える一文。
pub fn truncation_note(shown: usize, total: usize) -> String {
    trf(
        "⚠ 文書が大きいため先頭 {shown} KB のみ表示しています (全体 {total} KB)。編集タブでは全文を扱えます。",
        &[
            ("shown", (shown / 1024).max(1).to_string()),
            ("total", (total / 1024).max(1).to_string()),
        ],
    )
}

// ─── フロントマター ─────────────────────────────────────────────────

/// 先頭の YAML (`---`) / TOML (`+++`) フロントマターを本文から切り離す。
/// 返り値は (フロントマター本体, 残りの本文)。
///
/// 区切りが閉じていない場合はフロントマターとみなさない (`---` だけの
/// 水平線で始まる文書を丸ごと飲み込まないため)。
pub fn split_front_matter(text: &str) -> (Option<&str>, &str) {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    for fence in ["---", "+++"] {
        let Some(rest) = body.strip_prefix(fence) else {
            continue;
        };
        // 区切り行は fence だけ (末尾空白は許容)
        let rest = match rest.split_once('\n') {
            Some((head, r)) if head.trim().is_empty() => r,
            _ => continue,
        };
        let mut off = 0;
        for line in rest.split_inclusive('\n') {
            let t = line.trim_end_matches(['\n', '\r']).trim_end();
            if t == fence || (fence == "---" && t == "...") {
                let fm = &rest[..off];
                let after = &rest[off + line.len()..];
                return (Some(fm), after);
            }
            off += line.len();
        }
    }
    (None, text)
}

/// 脚注定義行 `[^label]: 本文` を (ラベル, 本文) に分解する。
pub fn footnote_def(line: &str) -> Option<(String, String)> {
    let rest = line.trim_start().strip_prefix("[^")?;
    let close = rest.find("]:")?;
    let label = rest[..close].trim();
    if label.is_empty() {
        return None;
    }
    Some((label.to_string(), rest[close + 2..].trim().to_string()))
}

/// ATX 見出し行 (`## 題`) なら (レベル, 本文) を返す。
/// 末尾の閉じ `###` と、アンカー指定 `{#id}` は本文から取り除く。
pub fn atx_heading(t: &str) -> Option<(usize, String)> {
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &t[hashes..];
    // `#` の直後は空白が必要 (`#hashtag` を見出しにしない)。行全体が `#` だけなら空見出し
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let mut body = rest.trim();
    // 閉じの `###`
    body = body.trim_end_matches('#').trim_end();
    // `{#anchor}` (VS Code / kramdown 系のアンカー指定)
    if let Some(open) = body.rfind("{#") {
        if body.ends_with('}') {
            body = body[..open].trim_end();
        }
    }
    Some((hashes, body.to_string()))
}

/// フェンスコードの開始行なら (フェンス記号, 言語トークン) を返す。
pub fn fence_open(t: &str) -> Option<(&'static str, String)> {
    for f in ["```", "~~~"] {
        if let Some(rest) = t.trim_start().strip_prefix(f) {
            // 情報文字列の先頭語だけを言語として使う (```rust,no_run 等に対応)
            let info = rest.trim().trim_start_matches(['`', '~']).trim();
            let lang = info
                .split([' ', '\t', ',', '{'])
                .next()
                .unwrap_or("")
                .to_string();
            return Some((f, lang));
        }
    }
    None
}

/// 行末のハード改行指定 (`  ` 2 個 / `\`) を検出し、(改行するか, 本文) を返す。
pub fn hard_break(line: &str) -> (bool, &str) {
    let t = line.trim_start();
    let slashes = t.chars().rev().take_while(|&c| c == '\\').count();
    // 奇数個なら最後の 1 個がハード改行指定 (`\\` は文字としてのバックスラッシュ)
    if slashes % 2 == 1 {
        return (true, t[..t.len() - 1].trim_end());
    }
    if line.ends_with("  ") && !t.trim().is_empty() {
        return (true, t.trim_end());
    }
    (false, t.trim_end())
}

/// Setext 見出しの下線 (`===` / `---`) なら見出しレベルを返す。
pub fn setext_level(line: &str) -> Option<usize> {
    let t = line.trim();
    if t.len() < 2 {
        return None;
    }
    if t.chars().all(|c| c == '=') {
        return Some(1);
    }
    if t.chars().all(|c| c == '-') {
        return Some(2);
    }
    None
}

// ─── インライン構文 ─────────────────────────────────────────────────

/// インライン装飾を適用した最小単位。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Span {
    pub text: String,
    pub code: bool,
    pub strong: bool,
    pub em: bool,
    pub strike: bool,
    /// Some(url) ならリンク (画像は "🖼 alt" テキストのリンクに落とす)
    pub link: Option<String>,
    /// `![alt](url)` 由来。ローカルファイルなら実画像として描画する
    pub image: bool,
    /// 脚注参照 `[^1]` 由来 (小さめ・アクセント色で描く)
    pub fnote: bool,
    /// `$…$` / `\(…\)` 由来の数式 (text は生 TeX)
    pub math: bool,
    /// `$$…$$` / `\[…\]` 由来 (行内でも大きめに組む)
    pub math_display: bool,
}

/// `<https://…>` / `<mailto:…>` 形式の自動リンクを chars[i] の `<` から読む。
fn read_autolink(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    debug_assert_eq!(chars.get(i), Some(&'<'));
    // URL に空白は入らない。長すぎるものは自動リンクとみなさない
    let close = (i + 1..chars.len().min(i + 2048)).find(|&k| chars[k] == '>')?;
    if (i + 1..close).any(|k| chars[k].is_whitespace() || chars[k] == '<') {
        return None;
    }
    let body: String = chars[i + 1..close].iter().collect();
    if body.is_empty() {
        return None;
    }
    if body.contains("://") {
        return Some((body.clone(), body, close + 1));
    }
    if let Some(mail) = body.strip_prefix("mailto:") {
        return Some((mail.to_string(), body.clone(), close + 1));
    }
    // 素のメールアドレス <a@b.c>
    let at = body.find('@')?;
    if at > 0 && body[at + 1..].contains('.') && !body.contains(['/', '"', '=']) {
        return Some((body.clone(), format!("mailto:{body}"), close + 1));
    }
    None
}

/// 素の `http(s)://…` を chars[i] から読む (GFM の自動リンク)。
/// 直前が英数字なら誤検出として扱わない。返り値は (URL, 次位置)。
fn read_bare_url(chars: &[char], i: usize) -> Option<(String, usize)> {
    if i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '/') {
        return None;
    }
    // 毎フレーム走るので文字列を作らずに突き合わせる
    let starts = |s: &str| {
        s.chars()
            .enumerate()
            .all(|(k, c)| chars.get(i + k) == Some(&c))
    };
    let scheme_len = if starts("https://") {
        8
    } else if starts("http://") {
        7
    } else {
        return None;
    };
    let mut k = i + scheme_len;
    if chars.get(k).is_none_or(|c| c.is_whitespace()) {
        return None;
    }
    while k < chars.len()
        && !chars[k].is_whitespace()
        && !matches!(chars[k], '<' | '>' | '"' | '`' | '|' | '\\')
    {
        k += 1;
    }
    // 文末の句読点・閉じ括弧は URL に含めない
    while k > i
        && matches!(
            chars[k - 1],
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '、' | '。' | '」'
        )
    {
        k -= 1;
    }
    if k <= i + scheme_len {
        return None;
    }
    Some((chars[i..k].iter().collect(), k))
}

fn flush(out: &mut Vec<Span>, cur: &mut String, strong: bool, em: bool, strike: bool) {
    if cur.is_empty() {
        return;
    }
    out.push(Span {
        text: std::mem::take(cur),
        strong,
        em,
        strike,
        ..Default::default()
    });
}

/// `[text](url)` を chars[i] の `[` から読む。成功したら (text, url, 次位置)。
fn read_link(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    debug_assert_eq!(chars.get(i), Some(&'['));
    let close = (i + 1..chars.len()).find(|&k| chars[k] == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = (close + 2..chars.len()).find(|&k| chars[k] == ')')?;
    let text: String = chars[i + 1..close].iter().collect();
    let url: String = chars[close + 2..end].iter().collect();
    Some((text, url, end + 1))
}

/// 1行分のインライン構文をスパン列へ分解する。
pub fn parse_inline(s: &str) -> Vec<Span> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut strong, mut em, mut strike) = (false, false, false);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\\' if next.is_some() => {
                // `\(…\)` / `\[…\]` は数式。エスケープより先に見る
                if let Some((tex, kind, ni)) = math::read_at(&chars, i) {
                    flush(&mut out, &mut cur, strong, em, strike);
                    out.push(Span {
                        text: tex,
                        math: true,
                        math_display: kind == math::Delim::Display,
                        ..Default::default()
                    });
                    i = ni;
                    continue;
                }
                // エスケープ: 次の1文字をそのまま出す (`\$` は数式を開かない)
                cur.push(next.unwrap());
                i += 2;
            }
            // 行内数式 `$…$` / `$$…$$`。通貨表記を拾わない規則は math::read_at 参照
            '$' => match math::read_at(&chars, i) {
                Some((tex, kind, ni)) => {
                    flush(&mut out, &mut cur, strong, em, strike);
                    out.push(Span {
                        text: tex,
                        math: true,
                        math_display: kind == math::Delim::Display,
                        ..Default::default()
                    });
                    i = ni;
                }
                None => {
                    cur.push('$');
                    i += 1;
                }
            },
            '`' => {
                // インラインコード: 同じ長さのバッククォート列で閉じる
                // (``a ` b`` のようにコード内にバッククォートを含められる)
                let n = chars[i..].iter().take_while(|&&c| c == '`').count();
                let mut k = i + n;
                let mut close = None;
                while k < chars.len() {
                    if chars[k] == '`' {
                        let m = chars[k..].iter().take_while(|&&c| c == '`').count();
                        if m == n {
                            close = Some(k);
                            break;
                        }
                        k += m;
                    } else {
                        k += 1;
                    }
                }
                match close {
                    Some(cl) => {
                        flush(&mut out, &mut cur, strong, em, strike);
                        let mut t: String = chars[i + n..cl].iter().collect();
                        // CommonMark: 前後に空白が1つずつあるときだけ剥がす
                        if t.len() > 2
                            && t.starts_with(' ')
                            && t.ends_with(' ')
                            && !t.trim().is_empty()
                        {
                            t = t[1..t.len() - 1].to_string();
                        }
                        out.push(Span {
                            text: t,
                            code: true,
                            ..Default::default()
                        });
                        i = cl + n;
                    }
                    None => {
                        for _ in 0..n {
                            cur.push('`');
                        }
                        i += n;
                    }
                }
            }
            '*' | '_' if next == Some(c) => {
                // ** / __ = 強調
                flush(&mut out, &mut cur, strong, em, strike);
                strong = !strong;
                i += 2;
            }
            '*' => {
                flush(&mut out, &mut cur, strong, em, strike);
                em = !em;
                i += 1;
            }
            '_' => {
                // snake_case を斜体にしないよう、単語境界でのみ効かせる
                let prev = if i == 0 {
                    None
                } else {
                    chars.get(i - 1).copied()
                };
                let boundary = if em {
                    next.is_none_or(|n| !n.is_alphanumeric())
                } else {
                    prev.is_none_or(|p| !p.is_alphanumeric())
                };
                if boundary {
                    flush(&mut out, &mut cur, strong, em, strike);
                    em = !em;
                } else {
                    cur.push('_');
                }
                i += 1;
            }
            '~' if next == Some('~') => {
                flush(&mut out, &mut cur, strong, em, strike);
                strike = !strike;
                i += 2;
            }
            '!' if next == Some('[') => match read_link(&chars, i + 1) {
                Some((alt, url, ni)) => {
                    flush(&mut out, &mut cur, strong, em, strike);
                    out.push(Span {
                        text: format!("🖼 {}", if alt.is_empty() { &url } else { &alt }),
                        link: Some(url.clone()),
                        image: true,
                        ..Default::default()
                    });
                    i = ni;
                }
                None => {
                    cur.push('!');
                    i += 1;
                }
            },
            // 脚注参照 `[^1]` (定義行 `[^1]:` はブロック側で処理する)
            '[' if next == Some('^') => match (i + 2..chars.len()).find(|&k| chars[k] == ']') {
                Some(cl) if cl > i + 2 => {
                    flush(&mut out, &mut cur, strong, em, strike);
                    let label: String = chars[i + 2..cl].iter().collect();
                    out.push(Span {
                        text: format!("[{label}]"),
                        fnote: true,
                        ..Default::default()
                    });
                    i = cl + 1;
                }
                _ => {
                    cur.push('[');
                    i += 1;
                }
            },
            '[' => match read_link(&chars, i) {
                Some((text, url, ni)) => {
                    flush(&mut out, &mut cur, strong, em, strike);
                    out.push(Span {
                        text,
                        link: Some(url),
                        ..Default::default()
                    });
                    i = ni;
                }
                None => {
                    cur.push('[');
                    i += 1;
                }
            },
            // 自動リンク <https://…> / <a@b.c>
            '<' => match read_autolink(&chars, i) {
                Some((text, url, ni)) => {
                    flush(&mut out, &mut cur, strong, em, strike);
                    out.push(Span {
                        text,
                        link: Some(url),
                        ..Default::default()
                    });
                    i = ni;
                }
                None => {
                    cur.push('<');
                    i += 1;
                }
            },
            _ => {
                // 素の URL も自動リンクにする
                if c == 'h' {
                    if let Some((url, ni)) = read_bare_url(&chars, i) {
                        flush(&mut out, &mut cur, strong, em, strike);
                        out.push(Span {
                            text: url.clone(),
                            link: Some(url),
                            ..Default::default()
                        });
                        i = ni;
                        continue;
                    }
                }
                cur.push(c);
                i += 1;
            }
        }
    }
    flush(&mut out, &mut cur, strong, em, strike);
    out
}

// ─── ブロック構文の判定ヘルパ ───────────────────────────────────────

/// 水平線 (`---` / `***` / `___`、3文字以上、空白許容)。
pub fn is_hr(t: &str) -> bool {
    let t = t.trim();
    if t.len() < 3 {
        return false;
    }
    for mark in ['-', '*', '_'] {
        if t.chars().all(|c| c == mark || c == ' ')
            && t.chars().filter(|&c| c == mark).count() >= 3
            && !t.contains(|c: char| c != mark && c != ' ')
        {
            return true;
        }
    }
    false
}

/// テーブルの区切り行 (`|---|:--:|` 形式) か。
pub fn is_table_sep(t: &str) -> bool {
    let t = t.trim();
    t.starts_with('|') && t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// `| a | b |` をセル列へ分解する。`\|` はエスケープされたパイプとして本文に残す。
pub fn split_row(t: &str) -> Vec<String> {
    let mut cells: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = t.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '|' => {
                cells.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    // 行頭・行末のパイプが作る空要素は1つずつだけ取り除く (途中の空セルは保持)
    if cells.first().is_some_and(|c| c.is_empty()) {
        cells.remove(0);
    }
    if cells.len() > 1 && cells.last().is_some_and(|c| c.is_empty()) {
        cells.pop();
    }
    cells
}

/// テーブル列の揃え。区切り行の `:--` / `:-:` / `--:` に対応する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAlign {
    Left,
    Center,
    Right,
}

/// 区切り行 `|:--|:-:|--:|` から列ごとの揃えを得る。
pub fn table_aligns(sep: &str) -> Vec<TableAlign> {
    split_row(sep)
        .iter()
        .map(|c| match (c.starts_with(':'), c.ends_with(':')) {
            (true, true) => TableAlign::Center,
            (false, true) => TableAlign::Right,
            _ => TableAlign::Left,
        })
        .collect()
}

/// リスト行なら (本文開始オフセット, 行頭記号) を返す。
pub fn list_marker(t: &str) -> Option<(usize, String)> {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(m) {
            // タスクリスト `- [ ]` / `- [x]` (GFM)
            if let Some(after) = rest.strip_prefix("[ ] ") {
                let _ = after;
                return Some((m.len() + 4, "☐".into()));
            }
            for done in ["[x] ", "[X] "] {
                if rest.starts_with(done) {
                    return Some((m.len() + 4, "☑".into()));
                }
            }
            // 本文が空の `- ` 単体も箇条書きとして扱う
            return Some((m.len(), "•".into()));
        }
        if t.trim_end() == m.trim_end() {
            return Some((t.len(), "•".into()));
        }
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..=9).contains(&digits) {
        for sep in [". ", ") "] {
            if t[digits..].starts_with(sep) {
                return Some((digits + 2, format!("{}.", &t[..digits])));
            }
        }
    }
    None
}

/// 引用行の `>` の深さと本文を返す (`>> x` → (2, "x"))。
pub fn quote_depth(line: &str) -> (usize, &str) {
    let mut rest = line.trim_start();
    let mut depth = 0;
    while let Some(r) = rest.strip_prefix('>') {
        depth += 1;
        rest = r.strip_prefix(' ').unwrap_or(r).trim_start_matches('\t');
        if depth > 16 {
            break;
        }
    }
    (depth, rest)
}

/// CJK 文字か (段落の行連結で空白を挟まない判定に使う)。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x30FF | 0x3400..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF)
}

/// 段落バッファへ1行を連結する。日本語同士なら空白を挟まない。
pub fn append_para(para: &mut String, line: &str) {
    if para.is_empty() {
        para.push_str(line);
        return;
    }
    let last = para.chars().last().unwrap_or(' ');
    let first = line.chars().next().unwrap_or(' ');
    if !(is_cjk(last) && is_cjk(first)) {
        para.push(' ');
    }
    para.push_str(line);
}

// ─── 画像 ───────────────────────────────────────────────────────────

/// プレビューで実際にデコードする画像 1 枚の上限バイト数。
pub const MAX_IMAGE_BYTES: usize = 24 * 1024 * 1024;
/// プレビュー内での表示用に縮小する長辺 (GPU 上限とは別の、紙面上の上限)。
const PREVIEW_MAX_SIDE: u32 = 1600;

/// 画像 URL の解決結果。
#[derive(Debug, Clone, PartialEq)]
pub enum ImageSrc {
    /// ローカルに実在するファイル
    Local(PathBuf),
    /// `data:` URI から復号した実体 (MIME, バイト列)
    Data { mime: String, bytes: Vec<u8> },
    /// http(s) — 描画時にネットワークへ出ないのでプレースホルダにする
    Remote(String),
    /// 解決できなかった (欠損・不正・ディレクトリ等)
    Missing(String),
}

/// `%E3%81%82` のようなパーセントエンコードを復号する。
/// 不正なシーケンスはそのまま残す (`100%_done.png` を壊さない)。
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| -> Option<u8> {
                match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// base64 を復号する (標準/URL セーフ両対応、空白・改行は無視)。
/// 依存を増やさないための最小実装。
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut nbits = 0u32;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        if c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c)?;
        acc = (acc << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push(((acc >> nbits) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// `data:image/png;base64,….` を (MIME, バイト列) へ復号する。
/// base64 でない `data:` (パーセントエンコードされた SVG 等) にも対応。
pub fn parse_data_uri(url: &str) -> Option<(String, Vec<u8>)> {
    let rest = url
        .strip_prefix("data:")
        .or_else(|| url.strip_prefix("DATA:"))?;
    let (meta, payload) = rest.split_once(',')?;
    let meta_l = meta.to_ascii_lowercase();
    let mime = meta_l
        .split(';')
        .next()
        .filter(|m| !m.is_empty())
        .unwrap_or("text/plain")
        .to_string();
    let bytes = if meta_l.split(';').any(|p| p.trim() == "base64") {
        base64_decode(payload)?
    } else {
        percent_decode(payload).into_bytes()
    };
    Some((mime, bytes))
}

/// `file://` URL をローカルパスへ変換する。
/// `file:///C:/x` のようなドライブ表記でも先頭の `/` を落として解釈する。
pub fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let lower = url.to_ascii_lowercase();
    let rest = if lower.starts_with("file://") {
        &url[7..]
    } else {
        return None;
    };
    // Windows のアプリは file://C:\dir\a.png のように区切りが `\` の URL を
    // 吐くことがある。RFC 的には不正だが実在するので受け付ける。
    let normalized;
    let rest: &str = if rest.contains('\\') {
        normalized = rest.replace('\\', "/");
        &normalized
    } else {
        rest
    };
    // ドライブ文字直結 (file://C:/…) はホスト部ではなくパスとして扱う
    let b0 = rest.as_bytes();
    if b0.len() >= 2 && b0[0].is_ascii_alphabetic() && b0[1] == b':' {
        return Some(PathBuf::from(percent_decode(rest)));
    }
    // file://host/path のホスト部は捨てる (localhost / 空のみ想定)
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(p) => &rest[p..],
        None if rest.is_empty() => return None,
        None => return None,
    };
    let decoded = percent_decode(path);
    // `/C:/…` は Windows のドライブ表記。先頭の `/` を落とす
    let b = decoded.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        return Some(PathBuf::from(&decoded[1..]));
    }
    Some(PathBuf::from(decoded))
}

/// 画像 URL を「どう描くか」へ分類する。ネットワークアクセスは一切しない。
/// 相対パスの基準は必ず `dir` (文書のあるディレクトリ) で、プロセスの
/// カレントディレクトリには決して依存しない。
pub fn classify_image(dir: Option<&Path>, url: &str) -> ImageSrc {
    let u = url.trim();
    if u.is_empty() {
        return ImageSrc::Missing(url.to_string());
    }
    let lower = u.to_ascii_lowercase();
    if lower.starts_with("data:") {
        return match parse_data_uri(u) {
            Some((mime, bytes)) if !bytes.is_empty() => ImageSrc::Data { mime, bytes },
            _ => ImageSrc::Missing(u.to_string()),
        };
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return ImageSrc::Remote(u.to_string());
    }
    if lower.starts_with("file://") {
        return match file_url_to_path(u) {
            Some(p) if p.is_file() => ImageSrc::Local(p),
            _ => ImageSrc::Missing(u.to_string()),
        };
    }
    // その他のスキーム (ftp: / mailto: 等) はローカル解決の対象外
    if let Some(colon) = u.find(':') {
        let scheme = &lower[..colon];
        if colon > 1
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-')
            && u[colon..].starts_with("://")
        {
            return ImageSrc::Remote(u.to_string());
        }
    }
    // クエリ/フラグメントを落としてからパーセント復号する
    let clean = u.split(['?', '#']).next().unwrap_or(u);
    if clean.is_empty() {
        return ImageSrc::Missing(u.to_string());
    }
    let decoded = percent_decode(clean);
    let p = Path::new(&decoded);
    let full = if p.is_absolute() {
        p.to_path_buf()
    } else {
        match dir {
            Some(d) => d.join(p),
            None => return ImageSrc::Missing(u.to_string()),
        }
    };
    if full.is_file() {
        ImageSrc::Local(full)
    } else {
        ImageSrc::Missing(u.to_string())
    }
}

/// プレビュー内で参照された画像のテクスチャキャッシュ。
/// ローカルファイルは mtime をキーに含めるため、外部で差し替わると再読込される。
/// `data:` URI は内容そのものが鍵なので一度載せたら使い回す。
#[derive(Default)]
pub struct ImageCache {
    map: HashMap<String, (Option<std::time::SystemTime>, Option<egui::TextureHandle>)>,
}

impl ImageCache {
    fn get(&mut self, ctx: &egui::Context, path: &Path) -> Option<egui::TextureHandle> {
        let key = path.to_string_lossy().to_string();
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if let Some((cached, tex)) = self.map.get(&key) {
            if *cached == mtime {
                return tex.clone();
            }
        }
        let bytes = std::fs::read(path).ok();
        let tex = bytes
            .filter(|b| b.len() <= MAX_IMAGE_BYTES)
            .and_then(|b| decode_texture(ctx, &key, &b));
        self.map.insert(key, (mtime, tex.clone()));
        tex
    }

    /// 鍵で引くだけ (デコードはしない)。外側 = 登録済みか、
    /// 内側 = テクスチャを作れたか。
    fn cached(&self, key: &str) -> Option<Option<egui::TextureHandle>> {
        self.map.get(key).map(|(_, tex)| tex.clone())
    }

    /// `data:` URI 等、メモリ上のバイト列からテクスチャを得る。
    fn get_bytes(
        &mut self,
        ctx: &egui::Context,
        key: &str,
        bytes: &[u8],
    ) -> Option<egui::TextureHandle> {
        if let Some((_, tex)) = self.map.get(key) {
            return tex.clone();
        }
        let tex = if bytes.len() <= MAX_IMAGE_BYTES {
            decode_texture(ctx, key, bytes)
        } else {
            None
        };
        self.map.insert(key.to_string(), (None, tex.clone()));
        tex
    }
}

/// バイト列を GPU テクスチャへ載せる。デコード失敗でも panic しない。
/// 巨大画像は「GPU 上限 (editor::MAX_TEXTURE_SIDE)」と「紙面上の上限」の
/// 小さい方まで縮小してから載せる。
fn decode_texture(ctx: &egui::Context, key: &str, bytes: &[u8]) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?;
    let mut rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let cap = PREVIEW_MAX_SIDE.min(crate::editor::MAX_TEXTURE_SIDE);
    if let Some((nw, nh)) = crate::editor::image_downscale(w, h, cap) {
        rgba = image::imageops::resize(&rgba, nw, nh, image::imageops::FilterType::Triangle);
    }
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
    Some(ctx.load_texture(
        format!("zv-md-img:{key}"),
        color,
        egui::TextureOptions::LINEAR,
    ))
}

/// 画像を描けないときの一行プレースホルダ文言。
fn image_placeholder_text(src: &ImageSrc, alt: &str) -> String {
    let alt = alt.trim();
    match src {
        ImageSrc::Remote(_) => {
            if alt.is_empty() {
                tr("🌐 リモート画像 (未取得)")
            } else {
                trf(
                    "🌐 リモート画像 (未取得): {alt}",
                    &[("alt", alt.to_string())],
                )
            }
        }
        _ => {
            if alt.is_empty() {
                tr("🖼 画像を表示できません")
            } else {
                trf("🖼 画像を表示できません: {alt}", &[("alt", alt.to_string())])
            }
        }
    }
}

// ─── Mermaid 図 ─────────────────────────────────────────────────────
//
// ```mermaid フェンスを、依存追加なしで「描ける範囲だけ」図として描く。
// 対応するのは実文書での出現頻度が圧倒的に高い 2 種類だけ:
//   * graph / flowchart (TD TB LR RL BT)
//   * sequenceDiagram
// それ以外の図種・壊れた入力は必ず「ソースコードブロック + 一行の注記」へ
// 落とす。空白にも panic にもしない、が唯一の絶対条件。
//
// 決定性について: 解析も配置も HashMap を一切使わない (ID 引きは Vec の
// 線形探索、集合は Vec<bool>)。同じ入力からは必ず同じ座標が出る。

pub mod mermaid {
    use std::cell::RefCell;
    use std::cmp::Ordering;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::rc::Rc;

    use eframe::egui::{pos2, vec2, Pos2, Rect, Vec2};

    use crate::i18n::trf;

    /// 描画量の上限。超えた分は捨てて注記を出す (巨大図で UI を止めない)。
    pub const MAX_NODES: usize = 200;
    pub const MAX_EDGES: usize = 400;
    /// ラベル 1 行の桁数 (全角 = 2 桁で数える)。
    pub const LABEL_COLS: usize = 22;
    /// レイアウトキャッシュの保持数。
    const CACHE_CAP: usize = 24;

    // ── 構文木 ──────────────────────────────────────────────────────

    /// ノード形状 (mermaid の括弧記法に 1 対 1 で対応)。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Shape {
        /// `A[Box]`
        Rect,
        /// `A(Round)`
        Round,
        /// `A([Stadium])`
        Stadium,
        /// `A[[Subroutine]]`
        Subroutine,
        /// `A[(Cylinder)]`
        Cylinder,
        /// `A((Circle))`
        Circle,
        /// `A{Diamond}`
        Diamond,
        /// `A{{Hexagon}}`
        Hexagon,
        /// `A>Flag]`
        Asymmetric,
    }

    /// 流れの向き。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Dir {
        /// TD / TB
        Down,
        /// BT
        Up,
        /// LR
        Right,
        /// RL
        Left,
    }

    impl Dir {
        /// 主軸が横向きか。
        pub fn horizontal(self) -> bool {
            matches!(self, Dir::Right | Dir::Left)
        }

        /// `graph` / `flowchart` に続く向きトークンを読む (未知は TD 扱い)。
        pub fn parse(tok: &str) -> Dir {
            match tok.trim().to_ascii_uppercase().as_str() {
                "BT" => Dir::Up,
                "LR" => Dir::Right,
                "RL" => Dir::Left,
                _ => Dir::Down,
            }
        }
    }

    impl Default for Dir {
        fn default() -> Self {
            Dir::Down
        }
    }

    /// 線種。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Line {
        /// `-->` `---`
        Solid,
        /// `-.->` `-.-`
        Dotted,
        /// `==>` `===`
        Thick,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct Node {
        pub id: String,
        pub label: String,
        pub shape: Shape,
        /// 所属サブグラフ (`subgraph` … `end`)
        pub group: Option<usize>,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct Edge {
        pub from: usize,
        pub to: usize,
        pub label: String,
        pub line: Line,
        /// 終点側の矢印 (`-->` の `>`)
        pub arrow_to: bool,
        /// 始点側の矢印 (`<-->` の `<`)
        pub arrow_from: bool,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct Group {
        pub id: String,
        pub title: String,
        pub nodes: Vec<usize>,
    }

    /// フローチャート 1 枚。
    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct Flow {
        pub dir: Dir,
        pub nodes: Vec<Node>,
        pub edges: Vec<Edge>,
        pub groups: Vec<Group>,
        /// 上限超過など、利用者に伝えるべきこと
        pub notice: Option<String>,
    }

    impl Flow {
        /// ID からノード添字を引く (無ければ作る)。上限超過時は None。
        fn node_of(
            &mut self,
            id: &str,
            label: &str,
            shape: Shape,
            group: Option<usize>,
        ) -> Option<usize> {
            if let Some(k) = self.nodes.iter().position(|n| n.id == id) {
                // 後から形やラベルが指定されたら上書きする (mermaid と同じ)
                if shape != Shape::Rect || label != id {
                    if self.nodes[k].label == self.nodes[k].id || label != id {
                        self.nodes[k].label = label.to_string();
                    }
                    self.nodes[k].shape = shape;
                }
                if self.nodes[k].group.is_none() {
                    self.nodes[k].group = group;
                }
                if let (Some(g), Some(gi)) = (group, self.nodes[k].group) {
                    if gi == g && !self.groups[g].nodes.contains(&k) {
                        self.groups[g].nodes.push(k);
                    }
                }
                return Some(k);
            }
            if self.nodes.len() >= MAX_NODES {
                self.notice = Some(cap_notice(self.nodes.len(), self.edges.len()));
                return None;
            }
            self.nodes.push(Node {
                id: id.to_string(),
                label: label.to_string(),
                shape,
                group,
            });
            let k = self.nodes.len() - 1;
            if let Some(g) = group {
                self.groups[g].nodes.push(k);
            }
            Some(k)
        }
    }

    /// メッセージ矢印の終端形。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SeqArrow {
        /// `->` `-->` (線のみ)
        Open,
        /// `->>` `-->>` (実線矢印)
        Solid,
        /// `-x` `--x` (×印)
        Cross,
        /// `-)` `--)` (非同期)
        Async,
    }

    /// ノートの位置指定。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum NotePos {
        Over,
        LeftOf,
        RightOf,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub enum SeqEvent {
        Msg {
            from: usize,
            to: usize,
            text: String,
            /// `-->` `-->>` は破線
            dashed: bool,
            arrow: SeqArrow,
        },
        Note {
            from: usize,
            to: usize,
            text: String,
            pos: NotePos,
        },
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct Participant {
        pub id: String,
        pub label: String,
    }

    /// シーケンス図 1 枚。
    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct Seq {
        pub title: Option<String>,
        pub participants: Vec<Participant>,
        pub events: Vec<SeqEvent>,
        /// `activate` / `deactivate` から起こした活性化区間 (参加者, 開始, 終了)
        pub activations: Vec<(usize, usize, usize)>,
        pub notice: Option<String>,
    }

    impl Seq {
        fn participant_of(&mut self, id: &str) -> Option<usize> {
            let id = id.trim();
            if id.is_empty() {
                return None;
            }
            if let Some(k) = self.participants.iter().position(|p| p.id == id) {
                return Some(k);
            }
            if self.participants.len() >= MAX_NODES {
                return None;
            }
            self.participants.push(Participant {
                id: id.to_string(),
                label: id.to_string(),
            });
            Some(self.participants.len() - 1)
        }
    }

    /// 解析結果。描けない入力は必ず Unsupported / Invalid に落ちる。
    #[derive(Debug, Clone, PartialEq)]
    pub enum Diagram {
        Flow(Flow),
        Seq(Seq),
        /// 未対応の図種 (classDiagram / gantt / pie …)
        Unsupported(String),
        /// 図として読めなかった (空・壊れている)
        Invalid,
    }

    // ── 字句の下ごしらえ ────────────────────────────────────────────

    /// `%%` 以降を落とす (引用符と角括弧の中は守る)。
    pub fn strip_comment(line: &str) -> &str {
        let b = line.as_bytes();
        let mut quoted = false;
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'"' => quoted = !quoted,
                b'%' if !quoted && b.get(i + 1) == Some(&b'%') => return &line[..i],
                _ => {}
            }
            i += 1;
        }
        line
    }

    /// ラベルの後始末: `<br>` を改行に、囲みの引用符を剥がす。
    pub fn clean_label(raw: &str) -> String {
        let mut s = raw.trim().to_string();
        for br in ["<br/>", "<br />", "<br>", "<BR/>", "<BR>"] {
            if s.contains(br) {
                s = s.replace(br, "\n");
            }
        }
        s = s.replace("#quot;", "\"").replace("&quot;", "\"");
        let t = s.trim();
        let unq = if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
            &t[1..t.len() - 1]
        } else if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
            &t[1..t.len() - 1]
        } else {
            t
        };
        unq.trim().to_string()
    }

    /// ノード記法 `id[ラベル]` を (ID, ラベル, 形) へ。
    /// 括弧が閉じていない等の壊れた記法は ID だけ拾って四角に落とす。
    pub fn node_spec(seg: &str) -> Option<(String, String, Shape)> {
        let s = seg.trim();
        if s.is_empty() {
            return None;
        }
        let cut = s.find(['[', '(', '{', '>']);
        let (id, rest) = match cut {
            Some(k) => (s[..k].trim(), &s[k..]),
            None => (s, ""),
        };
        // ID に空白は入らない。`A B[x]` のような入力は最後の語を ID とみなす
        let id = id.split_whitespace().last().unwrap_or("");
        if id.is_empty() {
            return None;
        }
        if rest.is_empty() {
            return Some((id.to_string(), id.to_string(), Shape::Rect));
        }
        const FORMS: &[(&str, &str, Shape)] = &[
            ("([", "])", Shape::Stadium),
            ("[[", "]]", Shape::Subroutine),
            ("[(", ")]", Shape::Cylinder),
            ("((", "))", Shape::Circle),
            ("{{", "}}", Shape::Hexagon),
            ("[", "]", Shape::Rect),
            ("(", ")", Shape::Round),
            ("{", "}", Shape::Diamond),
            (">", "]", Shape::Asymmetric),
        ];
        for (open, close, shape) in FORMS {
            if rest.len() >= open.len() + close.len()
                && rest.starts_with(open)
                && rest.ends_with(close)
            {
                let inner = clean_label(&rest[open.len()..rest.len() - close.len()]);
                let label = if inner.is_empty() {
                    id.to_string()
                } else {
                    inner
                };
                return Some((id.to_string(), label, *shape));
            }
        }
        Some((id.to_string(), id.to_string(), Shape::Rect))
    }

    /// 連結子 1 個ぶんの情報。
    #[derive(Debug, Clone, PartialEq)]
    pub struct Link {
        pub line: Line,
        pub arrow_to: bool,
        pub arrow_from: bool,
        pub label: String,
    }

    fn is_link_char(c: char) -> bool {
        matches!(c, '-' | '=' | '.')
    }

    fn is_arrow_head(c: Option<&char>) -> bool {
        matches!(c, Some('>') | Some('x') | Some('o'))
    }

    /// chars[i] から連結子を読む。読めたら (Link, 次位置)。
    /// `-->` `---` `-.->` `==>` `<-->` `-->|ラベル|` `-- ラベル -->` に対応。
    pub fn read_link(c: &[char], i: usize) -> Option<(Link, usize)> {
        let mut k = i;
        let mut arrow_from = false;
        if c.get(k) == Some(&'<') {
            if !matches!(c.get(k + 1), Some('-') | Some('=')) {
                return None;
            }
            arrow_from = true;
            k += 1;
        }
        let start = k;
        while k < c.len() && is_link_char(c[k]) {
            k += 1;
        }
        if k == start {
            return None;
        }
        let pre: String = c[start..k].iter().collect();
        // ID に紛れた単独の `-` `.` は連結子にしない
        if pre.chars().count() < 2 && !arrow_from {
            return None;
        }
        let style = |s: &str| {
            if s.contains('=') {
                Line::Thick
            } else if s.contains('.') {
                Line::Dotted
            } else {
                Line::Solid
            }
        };
        if is_arrow_head(c.get(k)) {
            let mut k2 = k + 1;
            let mut label = String::new();
            if c.get(k2) == Some(&'|') {
                if let Some(close) = (k2 + 1..c.len()).find(|&z| c[z] == '|') {
                    label = c[k2 + 1..close].iter().collect();
                    k2 = close + 1;
                }
            }
            return Some((
                Link {
                    line: style(&pre),
                    arrow_to: true,
                    arrow_from,
                    label: clean_label(&label),
                },
                k2,
            ));
        }
        // 矢印なしでも `|ラベル|` は付く (`---|text|`)
        if c.get(k) == Some(&'|') {
            if let Some(close) = (k + 1..c.len()).find(|&z| c[z] == '|') {
                let label: String = c[k + 1..close].iter().collect();
                return Some((
                    Link {
                        line: style(&pre),
                        arrow_to: false,
                        arrow_from,
                        label: clean_label(&label),
                    },
                    close + 1,
                ));
            }
        }
        // `-- ラベル -->` 形式。開始側が 2 文字 (`--` `==`) か `.` 終わりのときだけ
        // 中置ラベルを探す (`---` は「ラベル無しの実線」なので探さない)。
        if pre.chars().count() <= 2 || pre.ends_with('.') {
            let mut z = k;
            while z < c.len() && !is_link_char(c[z]) {
                z += 1;
            }
            if z < c.len() && z > k {
                let text: String = c[k..z].iter().collect();
                let s2 = z;
                while z < c.len() && is_link_char(c[z]) {
                    z += 1;
                }
                let post: String = c[s2..z].iter().collect();
                let arrow = is_arrow_head(c.get(z));
                let end = if arrow { z + 1 } else { z };
                if !text.trim().is_empty() {
                    let both = format!("{pre}{post}");
                    return Some((
                        Link {
                            line: style(&both),
                            arrow_to: arrow,
                            arrow_from,
                            label: clean_label(&text),
                        },
                        end,
                    ));
                }
            }
        }
        Some((
            Link {
                line: style(&pre),
                arrow_to: false,
                arrow_from,
                label: String::new(),
            },
            k,
        ))
    }

    // ── フローチャートの解析 ────────────────────────────────────────

    /// 1 文 (`;` / 改行区切り) を「ノード → 連結子 → ノード …」の鎖として取り込む。
    fn take_chain(stmt: &str, g: &mut Flow, group: Option<usize>) {
        let c: Vec<char> = stmt.chars().collect();
        let mut depth = 0i32;
        let mut quoted = false;
        let mut seg = String::new();
        let mut prev: Option<usize> = None;
        let mut pending: Option<Link> = None;
        let mut i = 0;
        while i < c.len() {
            let ch = c[i];
            if ch == '"' {
                quoted = !quoted;
                seg.push(ch);
                i += 1;
                continue;
            }
            if !quoted {
                match ch {
                    '[' | '(' | '{' => depth += 1,
                    ']' | ')' | '}' => depth = (depth - 1).max(0),
                    _ => {}
                }
                if depth == 0 && (is_link_char(ch) || ch == '<') {
                    if let Some((link, ni)) = read_link(&c, i) {
                        let cur =
                            node_spec(&seg).and_then(|(id, lb, sh)| g.node_of(&id, &lb, sh, group));
                        connect(g, &mut prev, &mut pending, cur);
                        pending = Some(link);
                        seg.clear();
                        i = ni;
                        continue;
                    }
                }
            }
            seg.push(ch);
            i += 1;
        }
        let cur = node_spec(&seg).and_then(|(id, lb, sh)| g.node_of(&id, &lb, sh, group));
        connect(g, &mut prev, &mut pending, cur);
    }

    fn connect(
        g: &mut Flow,
        prev: &mut Option<usize>,
        pending: &mut Option<Link>,
        cur: Option<usize>,
    ) {
        if let (Some(p), Some(l), Some(n)) = (*prev, pending.take(), cur) {
            if g.edges.len() < MAX_EDGES {
                g.edges.push(Edge {
                    from: p,
                    to: n,
                    label: l.label,
                    line: l.line,
                    arrow_to: l.arrow_to,
                    arrow_from: l.arrow_from,
                });
            } else {
                g.notice = Some(cap_notice(g.nodes.len(), g.edges.len()));
            }
        }
        if cur.is_some() {
            *prev = cur;
        }
    }

    fn cap_notice(nodes: usize, edges: usize) -> String {
        trf(
            "図が大きすぎるため一部だけ描画しています (ノード {n} / 辺 {e} が上限)",
            &[("n", nodes.to_string()), ("e", edges.to_string())],
        )
    }

    fn parse_flow(src: &str, dir_tok: &str) -> Flow {
        let mut g = Flow {
            dir: Dir::parse(dir_tok),
            ..Default::default()
        };
        let mut groups: Vec<usize> = Vec::new();
        let mut first = true;
        for raw in src.lines() {
            let line = strip_comment(raw);
            for stmt in line.split(';') {
                let t = stmt.trim();
                if t.is_empty() {
                    continue;
                }
                if first {
                    // 図種の宣言行そのものは読み飛ばす
                    first = false;
                    let head = t
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if head == "graph" || head == "flowchart" {
                        continue;
                    }
                }
                let low = t.to_ascii_lowercase();
                if low == "end" {
                    groups.pop();
                    continue;
                }
                if let Some(rest) = strip_kw(t, "subgraph") {
                    if g.groups.len() < MAX_NODES {
                        let (id, title) = subgraph_head(rest);
                        g.groups.push(Group {
                            id,
                            title,
                            nodes: Vec::new(),
                        });
                        groups.push(g.groups.len() - 1);
                    }
                    continue;
                }
                // 見た目指定・対話系は図の骨格に関係しないので読み飛ばす
                if [
                    "style",
                    "classdef",
                    "class",
                    "click",
                    "linkstyle",
                    "direction",
                    "%%",
                ]
                .iter()
                .any(|k| low.starts_with(k))
                {
                    continue;
                }
                take_chain(t, &mut g, groups.last().copied());
            }
        }
        g
    }

    /// `subgraph one[タイトル]` / `subgraph タイトル` を (ID, 表示名) へ。
    fn subgraph_head(rest: &str) -> (String, String) {
        let r = rest.trim();
        if r.is_empty() {
            return (String::new(), String::new());
        }
        if let Some(k) = r.find('[') {
            if r.ends_with(']') {
                let id = r[..k].trim().to_string();
                let title = clean_label(&r[k + 1..r.len() - 1]);
                return (id, title);
            }
        }
        let label = clean_label(r);
        (label.clone(), label)
    }

    /// 行頭キーワードを (大小無視で) 剥がす。語境界を要求する。
    fn strip_kw<'a>(line: &'a str, kw: &str) -> Option<&'a str> {
        // 日本語で始まる行を途中で切らないよう、文字境界を確かめてから比べる
        if line.len() < kw.len()
            || !line.is_char_boundary(kw.len())
            || !line[..kw.len()].eq_ignore_ascii_case(kw)
        {
            return None;
        }
        let rest = &line[kw.len()..];
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            Some(rest)
        } else {
            None
        }
    }

    // ── シーケンス図の解析 ──────────────────────────────────────────

    /// `A->>B: msg` の矢印部を読む。返り値は (from, arrow, dashed, to, text)。
    fn seq_msg(line: &str) -> Option<(String, SeqArrow, bool, String, String)> {
        // 長い記法から順に見る (`-->>` を `-->` と誤読しない)
        const FORMS: &[(&str, SeqArrow, bool)] = &[
            ("-->>", SeqArrow::Solid, true),
            ("--x", SeqArrow::Cross, true),
            ("--)", SeqArrow::Async, true),
            ("-->", SeqArrow::Open, true),
            ("->>", SeqArrow::Solid, false),
            ("-x", SeqArrow::Cross, false),
            ("-)", SeqArrow::Async, false),
            ("->", SeqArrow::Open, false),
        ];
        let mut best: Option<(usize, &str, SeqArrow, bool)> = None;
        for (tok, arrow, dashed) in FORMS {
            if let Some(at) = line.find(tok) {
                let better = match best {
                    None => true,
                    Some((b, bt, _, _)) => at < b || (at == b && tok.len() > bt.len()),
                };
                if better {
                    best = Some((at, tok, *arrow, *dashed));
                }
            }
        }
        let (at, tok, arrow, dashed) = best?;
        let from = line[..at].trim().to_string();
        let rest = &line[at + tok.len()..];
        let (to, text) = match rest.find(':') {
            Some(k) => (
                rest[..k].trim().to_string(),
                rest[k + 1..].trim().to_string(),
            ),
            None => (rest.trim().to_string(), String::new()),
        };
        if from.is_empty() || to.is_empty() {
            return None;
        }
        Some((from, arrow, dashed, to, text))
    }

    fn parse_seq(src: &str) -> Seq {
        let mut d = Seq::default();
        let mut open: Vec<(usize, usize)> = Vec::new();
        let mut first = true;
        for raw in src.lines() {
            let t = strip_comment(raw).trim();
            if t.is_empty() {
                continue;
            }
            if first {
                first = false;
                if t.eq_ignore_ascii_case("sequenceDiagram") {
                    continue;
                }
            }
            if let Some(rest) = strip_kw(t, "title") {
                d.title = Some(clean_label(rest));
                continue;
            }
            if let Some(rest) = strip_kw(t, "participant").or_else(|| strip_kw(t, "actor")) {
                let rest = rest.trim();
                let (id, label) = match find_kw(rest, "as") {
                    Some(k) => (rest[..k].trim(), clean_label(&rest[k + 2..])),
                    None => (rest, clean_label(rest)),
                };
                if let Some(p) = d.participant_of(id) {
                    d.participants[p].label = if label.is_empty() {
                        id.to_string()
                    } else {
                        label
                    };
                }
                continue;
            }
            if let Some(rest) = strip_kw(t, "activate") {
                if let Some(p) = d.participant_of(rest.trim()) {
                    open.push((p, d.events.len()));
                }
                continue;
            }
            if let Some(rest) = strip_kw(t, "deactivate") {
                if let Some(p) = d.participant_of(rest.trim()) {
                    if let Some(k) = open.iter().rposition(|&(q, _)| q == p) {
                        let (_, at) = open.remove(k);
                        d.activations.push((p, at, d.events.len()));
                    }
                }
                continue;
            }
            if let Some(rest) = strip_kw(t, "note").or_else(|| strip_kw(t, "Note")) {
                if let Some(ev) = seq_note(rest, &mut d) {
                    d.events.push(ev);
                }
                continue;
            }
            // ブロック構文 (loop/alt/opt/par/else/end/rect/critical/break) は
            // 骨格に効かないので読み飛ばす。autonumber も同じ。
            let low = t.to_ascii_lowercase();
            if [
                "loop",
                "alt",
                "opt",
                "par",
                "else",
                "end",
                "rect",
                "critical",
                "break",
                "autonumber",
                "box",
                "link",
                "links",
            ]
            .iter()
            .any(|k| low == *k || low.starts_with(&format!("{k} ")))
            {
                continue;
            }
            if let Some((from, arrow, dashed, to, text)) = seq_msg(t) {
                let (f, o) = (d.participant_of(&from), d.participant_of(&to));
                if let (Some(f), Some(o)) = (f, o) {
                    if d.events.len() < MAX_EDGES {
                        d.events.push(SeqEvent::Msg {
                            from: f,
                            to: o,
                            text: clean_label(&text),
                            dashed,
                            arrow,
                        });
                    } else {
                        d.notice = Some(cap_notice(d.participants.len(), d.events.len()));
                    }
                }
            }
        }
        // 閉じ忘れた activate は最後まで伸ばす
        for (p, at) in open {
            d.activations.push((p, at, d.events.len()));
        }
        d
    }

    /// `over A,B: text` / `left of A: text` / `right of A: text`
    fn seq_note(rest: &str, d: &mut Seq) -> Option<SeqEvent> {
        let r = rest.trim();
        let low = r.to_ascii_lowercase();
        let (pos, body) = if let Some(b) = low.strip_prefix("over") {
            (NotePos::Over, &r[r.len() - b.len()..])
        } else if let Some(b) = low.strip_prefix("left of") {
            (NotePos::LeftOf, &r[r.len() - b.len()..])
        } else if let Some(b) = low.strip_prefix("right of") {
            (NotePos::RightOf, &r[r.len() - b.len()..])
        } else {
            return None;
        };
        let (who, text) = match body.find(':') {
            Some(k) => (body[..k].trim(), clean_label(&body[k + 1..])),
            None => (body.trim(), String::new()),
        };
        let mut ids = who.split(',').map(str::trim).filter(|s| !s.is_empty());
        let a = d.participant_of(ids.next()?)?;
        let b = ids.next().and_then(|s| d.participant_of(s)).unwrap_or(a);
        Some(SeqEvent::Note {
            from: a.min(b),
            to: a.max(b),
            text,
            pos,
        })
    }

    /// 語として現れる `as` の位置 (バイト添字)。
    fn find_kw(s: &str, kw: &str) -> Option<usize> {
        let b = s.as_bytes();
        let mut i = 0;
        while i + kw.len() <= b.len() {
            if s[i..i + kw.len()].eq_ignore_ascii_case(kw) {
                let before_ok = i == 0 || b[i - 1].is_ascii_whitespace();
                let after = b.get(i + kw.len());
                let after_ok = after.is_none_or(|c| c.is_ascii_whitespace());
                if before_ok && after_ok {
                    return Some(i);
                }
            }
            i += 1;
        }
        None
    }

    // ── 入口 ────────────────────────────────────────────────────────

    /// mermaid ソースを図へ。読めない・未対応は必ず Unsupported / Invalid。
    pub fn parse(src: &str) -> Diagram {
        let head_line = src
            .lines()
            .map(strip_comment)
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        if head_line.is_empty() {
            return Diagram::Invalid;
        }
        let mut it = head_line.split_whitespace();
        let head = it.next().unwrap_or("");
        let arg = it.next().unwrap_or("");
        // `graph TD;` のように区切りが付くことがある
        let head = head.trim_end_matches([';', ':']);
        let low = head.to_ascii_lowercase();
        if low == "graph" || low == "flowchart" {
            let g = parse_flow(src, arg.trim_end_matches(';'));
            if g.nodes.is_empty() {
                return Diagram::Invalid;
            }
            return Diagram::Flow(g);
        }
        if low == "sequencediagram" {
            let d = parse_seq(src);
            if d.participants.is_empty() {
                return Diagram::Invalid;
            }
            return Diagram::Seq(d);
        }
        Diagram::Unsupported(head.to_string())
    }

    /// 未対応図種の一行注記。
    pub fn unsupported_notice(kind: &str) -> String {
        trf(
            "この図の種類はまだ描画できません (mermaid: {kind})",
            &[("kind", kind.to_string())],
        )
    }

    /// 図として読めなかったときの一行注記。
    pub fn invalid_notice() -> String {
        crate::i18n::tr("この mermaid 図を解釈できませんでした").to_string()
    }

    // ── ラベルの折返し ──────────────────────────────────────────────

    /// 表示桁数 (全角 = 2)。
    pub fn disp_cols(s: &str) -> usize {
        s.chars()
            .map(|c| if super::is_cjk(c) { 2 } else { 1 })
            .sum()
    }

    /// ラベルを max 桁で折り返す。明示改行を尊重し、単語境界を優先する。
    pub fn wrap_label(label: &str, max: usize) -> Vec<String> {
        let max = max.max(4);
        let mut out = Vec::new();
        for para in label.split('\n') {
            let mut cur = String::new();
            let mut cur_w = 0usize;
            let mut word = String::new();
            let mut word_w = 0usize;
            let flush_word = |cur: &mut String,
                              cur_w: &mut usize,
                              word: &mut String,
                              word_w: &mut usize,
                              out: &mut Vec<String>| {
                if word.is_empty() {
                    return;
                }
                if *cur_w > 0 && *cur_w + *word_w > max {
                    out.push(std::mem::take(cur));
                    *cur_w = 0;
                }
                cur.push_str(word);
                *cur_w += *word_w;
                word.clear();
                *word_w = 0;
            };
            for c in para.chars() {
                let w = if super::is_cjk(c) { 2 } else { 1 };
                if c == ' ' {
                    flush_word(&mut cur, &mut cur_w, &mut word, &mut word_w, &mut out);
                    if cur_w > 0 && cur_w + 1 <= max {
                        cur.push(' ');
                        cur_w += 1;
                    }
                    continue;
                }
                if super::is_cjk(c) {
                    // CJK は語境界が無いので 1 文字ずつ置く
                    flush_word(&mut cur, &mut cur_w, &mut word, &mut word_w, &mut out);
                    if cur_w + w > max && cur_w > 0 {
                        out.push(std::mem::take(&mut cur));
                        cur_w = 0;
                    }
                    cur.push(c);
                    cur_w += w;
                    continue;
                }
                if word_w + w > max {
                    // max より長い単語は強制分割
                    flush_word(&mut cur, &mut cur_w, &mut word, &mut word_w, &mut out);
                    if cur_w > 0 {
                        out.push(std::mem::take(&mut cur));
                        cur_w = 0;
                    }
                }
                word.push(c);
                word_w += w;
            }
            flush_word(&mut cur, &mut cur_w, &mut word, &mut word_w, &mut out);
            out.push(cur);
        }
        while out.len() > 1 && out.last().is_some_and(|l| l.trim().is_empty()) {
            out.pop();
        }
        if out.is_empty() {
            out.push(String::new());
        }
        // ノードが縦に伸びすぎないよう行数も抑える
        if out.len() > 8 {
            out.truncate(8);
            if let Some(last) = out.last_mut() {
                last.push('…');
            }
        }
        out
    }

    // ── 配置 ────────────────────────────────────────────────────────

    /// 配置に使う寸法。フォント計測結果を渡すことで layout を純関数に保つ。
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Metrics {
        /// 半角 1 文字の幅
        pub char_w: f32,
        /// 1 行の高さ
        pub line_h: f32,
        pub pad_x: f32,
        pub pad_y: f32,
        /// ランク間の間隔 (主軸)
        pub gap_rank: f32,
        /// 同一ランク内の間隔 (交差軸)
        pub gap_cross: f32,
        /// 図全体の余白
        pub margin: f32,
    }

    impl Default for Metrics {
        fn default() -> Self {
            Metrics {
                char_w: 7.0,
                line_h: 16.0,
                pad_x: 10.0,
                pad_y: 6.0,
                gap_rank: 46.0,
                gap_cross: 22.0,
                margin: 12.0,
            }
        }
    }

    impl Metrics {
        fn hash_into(&self, h: &mut impl Hasher) {
            for v in [
                self.char_w,
                self.line_h,
                self.pad_x,
                self.pad_y,
                self.gap_rank,
                self.gap_cross,
                self.margin,
            ] {
                v.to_bits().hash(h);
            }
        }
    }

    /// 配置済みノード。
    #[derive(Debug, Clone, PartialEq)]
    pub struct Placed {
        pub node: usize,
        pub rect: Rect,
        pub lines: Vec<String>,
        pub shape: Shape,
    }

    /// 配置済みの辺 (points は始点→終点の折れ線)。
    #[derive(Debug, Clone, PartialEq)]
    pub struct EdgeGeom {
        pub points: Vec<Pos2>,
        pub label: String,
        pub label_at: Pos2,
        pub line: Line,
        pub arrow_to: bool,
        pub arrow_from: bool,
    }

    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct FlowLayout {
        pub boxes: Vec<Placed>,
        pub edges: Vec<EdgeGeom>,
        /// サブグラフの枠 (タイトル, 矩形)
        pub groups: Vec<(String, Rect)>,
        pub size: Vec2,
        pub notice: Option<String>,
    }

    /// 形ごとの外寸倍率 (菱形と円はラベルを収めるために膨らませる)。
    fn shape_pad(shape: Shape) -> (f32, f32) {
        match shape {
            Shape::Diamond => (1.45, 1.5),
            Shape::Circle => (1.3, 1.3),
            Shape::Hexagon => (1.25, 1.0),
            Shape::Asymmetric => (1.15, 1.0),
            _ => (1.0, 1.0),
        }
    }

    /// 有向辺の後退辺 (循環を作る辺) を DFS で洗い出す。
    /// ノード添字順に走るので結果は入力だけで決まる。
    fn back_edges(n: usize, edges: &[Edge]) -> Vec<bool> {
        let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, e) in edges.iter().enumerate() {
            if e.from != e.to && e.from < n && e.to < n {
                out[e.from].push(i);
            }
        }
        let mut state = vec![0u8; n];
        let mut back = vec![false; edges.len()];
        for s in 0..n {
            if state[s] != 0 {
                continue;
            }
            state[s] = 1;
            let mut stack = vec![(s, 0usize)];
            while let Some((v, ei)) = stack.pop() {
                if ei < out[v].len() {
                    stack.push((v, ei + 1));
                    let e = out[v][ei];
                    let w = edges[e].to;
                    match state[w] {
                        0 => {
                            state[w] = 1;
                            stack.push((w, 0));
                        }
                        1 => back[e] = true,
                        _ => {}
                    }
                } else {
                    state[v] = 2;
                }
            }
        }
        back
    }

    /// ランク割当 (発生源からの最長路)。後退辺は無視するので必ず収束する。
    pub fn ranks(n: usize, edges: &[Edge]) -> Vec<usize> {
        let back = back_edges(n, edges);
        let mut rank = vec![0usize; n];
        for _ in 0..=n {
            let mut changed = false;
            for (i, e) in edges.iter().enumerate() {
                if back[i] || e.from == e.to || e.from >= n || e.to >= n {
                    continue;
                }
                if rank[e.to] < rank[e.from] + 1 {
                    rank[e.to] = rank[e.from] + 1;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        rank
    }

    /// フローチャートを配置する。同じ (図, 寸法) からは必ず同じ座標が出る。
    pub fn layout_flow(g: &Flow, m: Metrics) -> FlowLayout {
        let n = g.nodes.len();
        if n == 0 {
            return FlowLayout {
                size: vec2(0.0, 0.0),
                notice: g.notice.clone(),
                ..Default::default()
            };
        }
        // ラベル折返しと素の寸法
        let mut lines: Vec<Vec<String>> = Vec::with_capacity(n);
        let mut size: Vec<Vec2> = Vec::with_capacity(n);
        for node in &g.nodes {
            let ls = wrap_label(&node.label, LABEL_COLS);
            let cols = ls.iter().map(|l| disp_cols(l)).max().unwrap_or(1).max(1);
            let (fx, fy) = shape_pad(node.shape);
            let mut w = (cols as f32 * m.char_w + m.pad_x * 2.0) * fx;
            let mut h = (ls.len() as f32 * m.line_h + m.pad_y * 2.0) * fy;
            w = w.max(40.0);
            h = h.max(26.0);
            if node.shape == Shape::Circle {
                let d = w.max(h);
                w = d;
                h = d;
            }
            lines.push(ls);
            size.push(vec2(w, h));
        }

        let rank = ranks(n, &g.edges);
        let maxr = rank.iter().copied().max().unwrap_or(0);
        let mut layers: Vec<Vec<usize>> = vec![Vec::new(); maxr + 1];
        for v in 0..n {
            layers[rank[v]].push(v);
        }

        // 交差を減らす重心法 (前後 2 往復。順序は元の添字で必ず破られる)
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut succs: Vec<Vec<usize>> = vec![Vec::new(); n];
        for e in &g.edges {
            if e.from == e.to || e.from >= n || e.to >= n || rank[e.from] == rank[e.to] {
                continue;
            }
            let (a, b) = if rank[e.from] < rank[e.to] {
                (e.from, e.to)
            } else {
                (e.to, e.from)
            };
            succs[a].push(b);
            preds[b].push(a);
        }
        let mut pos_in = vec![0usize; n];
        let refresh = |layers: &Vec<Vec<usize>>, pos_in: &mut Vec<usize>| {
            for l in layers {
                for (p, &v) in l.iter().enumerate() {
                    pos_in[v] = p;
                }
            }
        };
        refresh(&layers, &mut pos_in);
        for _ in 0..2 {
            for r in 1..=maxr {
                sort_layer(
                    &mut layers[r],
                    &preds,
                    &pos_in,
                    rank.as_slice(),
                    r.wrapping_sub(1),
                );
                refresh(&layers, &mut pos_in);
            }
            for r in (0..maxr).rev() {
                sort_layer(&mut layers[r], &succs, &pos_in, rank.as_slice(), r + 1);
                refresh(&layers, &mut pos_in);
            }
        }

        // 主軸 (ランク方向) と交差軸の座標
        let horiz = g.dir.horizontal();
        let main_of = |s: Vec2| if horiz { s.x } else { s.y };
        let cross_of = |s: Vec2| if horiz { s.y } else { s.x };
        let mut main_at = vec![0.0f32; maxr + 1];
        let mut acc = 0.0f32;
        for (r, layer) in layers.iter().enumerate() {
            main_at[r] = acc;
            let ext = layer
                .iter()
                .map(|&v| main_of(size[v]))
                .fold(0.0f32, f32::max);
            acc += ext + m.gap_rank;
        }
        let total_main = (acc - m.gap_rank).max(0.0);
        let mut cross_len = vec![0.0f32; maxr + 1];
        for (r, layer) in layers.iter().enumerate() {
            let sum: f32 = layer.iter().map(|&v| cross_of(size[v])).sum();
            let gaps = m.gap_cross * layer.len().saturating_sub(1) as f32;
            cross_len[r] = sum + gaps;
        }
        let total_cross = cross_len.iter().copied().fold(0.0f32, f32::max);

        let mut rects = vec![Rect::NOTHING; n];
        for (r, layer) in layers.iter().enumerate() {
            let mut c = (total_cross - cross_len[r]) * 0.5;
            let ext = layer
                .iter()
                .map(|&v| main_of(size[v]))
                .fold(0.0f32, f32::max);
            for &v in layer {
                let s = size[v];
                // 主軸方向はランク帯の中央へ寄せる
                let main = main_at[r] + (ext - main_of(s)) * 0.5;
                let (x, y) = match g.dir {
                    Dir::Down => (c, main),
                    Dir::Up => (c, total_main - main - s.y),
                    Dir::Right => (main, c),
                    Dir::Left => (total_main - main - s.x, c),
                };
                rects[v] = Rect::from_min_size(pos2(x + m.margin, y + m.margin), s);
                c += cross_of(s) + m.gap_cross;
            }
        }

        let size_all = if horiz {
            vec2(total_main + m.margin * 2.0, total_cross + m.margin * 2.0)
        } else {
            vec2(total_cross + m.margin * 2.0, total_main + m.margin * 2.0)
        };

        // サブグラフの枠 (所属ノードの外接矩形 + 余白)
        let mut groups = Vec::new();
        for grp in &g.groups {
            let mut r = Rect::NOTHING;
            for &v in &grp.nodes {
                if v < n {
                    r = r.union(rects[v]);
                }
            }
            if r.is_finite() && !grp.nodes.is_empty() {
                let title = if grp.title.is_empty() {
                    grp.id.clone()
                } else {
                    grp.title.clone()
                };
                groups.push((title, r.expand2(vec2(10.0, 14.0))));
            }
        }

        let mut boxes = Vec::with_capacity(n);
        for v in 0..n {
            boxes.push(Placed {
                node: v,
                rect: rects[v],
                lines: std::mem::take(&mut lines[v]),
                shape: g.nodes[v].shape,
            });
        }

        let mut geoms = Vec::with_capacity(g.edges.len());
        for e in &g.edges {
            if e.from >= n || e.to >= n {
                continue;
            }
            let (a, b) = (rects[e.from], rects[e.to]);
            let points = if e.from == e.to {
                self_loop(a)
            } else {
                let p0 = border_point(a, b.center());
                let p1 = border_point(b, a.center());
                vec![p0, p1]
            };
            let label_at = mid_point(&points);
            geoms.push(EdgeGeom {
                points,
                label: e.label.clone(),
                label_at,
                line: e.line,
                arrow_to: e.arrow_to,
                arrow_from: e.arrow_from,
            });
        }

        FlowLayout {
            boxes,
            edges: geoms,
            groups,
            size: size_all,
            notice: g.notice.clone(),
        }
    }

    fn sort_layer(
        layer: &mut Vec<usize>,
        nbr: &[Vec<usize>],
        pos_in: &[usize],
        rank: &[usize],
        want_rank: usize,
    ) {
        if layer.len() < 2 {
            return;
        }
        let mut keyed: Vec<(f32, usize, usize)> = layer
            .iter()
            .enumerate()
            .map(|(p, &v)| {
                let mut sum = 0.0f32;
                let mut cnt = 0u32;
                for &u in &nbr[v] {
                    if rank[u] == want_rank {
                        sum += pos_in[u] as f32;
                        cnt += 1;
                    }
                }
                let b = if cnt > 0 { sum / cnt as f32 } else { p as f32 };
                (b, p, v)
            })
            .collect();
        keyed.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        *layer = keyed.into_iter().map(|k| k.2).collect();
    }

    /// 矩形の中心から `toward` へ向かう線が枠を横切る点。
    pub fn border_point(r: Rect, toward: Pos2) -> Pos2 {
        let c = r.center();
        let d = toward - c;
        let (hw, hh) = (r.width() * 0.5, r.height() * 0.5);
        if d.x.abs() < 1e-6 && d.y.abs() < 1e-6 {
            return c;
        }
        let tx = if d.x.abs() > 1e-6 {
            hw / d.x.abs()
        } else {
            f32::INFINITY
        };
        let ty = if d.y.abs() > 1e-6 {
            hh / d.y.abs()
        } else {
            f32::INFINITY
        };
        let t = tx.min(ty);
        c + d * t
    }

    fn self_loop(r: Rect) -> Vec<Pos2> {
        let d = 16.0;
        vec![
            pos2(r.right(), r.center().y - r.height() * 0.2),
            pos2(r.right() + d, r.center().y - r.height() * 0.2),
            pos2(r.right() + d, r.center().y + r.height() * 0.2),
            pos2(r.right(), r.center().y + r.height() * 0.2),
        ]
    }

    fn mid_point(p: &[Pos2]) -> Pos2 {
        match p.len() {
            0 => pos2(0.0, 0.0),
            1 => p[0],
            _ => {
                let a = p[p.len() / 2 - 1];
                let b = p[p.len() / 2];
                pos2((a.x + b.x) * 0.5, (a.y + b.y) * 0.5)
            }
        }
    }

    // ── シーケンス図の配置 ──────────────────────────────────────────

    #[derive(Debug, Clone, PartialEq)]
    pub struct SeqCol {
        pub label: Vec<String>,
        pub head: Rect,
        pub x: f32,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct SeqArrowGeom {
        pub from: Pos2,
        pub to: Pos2,
        pub text: String,
        pub dashed: bool,
        pub arrow: SeqArrow,
        /// 自分自身へのメッセージは折返しで描く
        pub loopback: bool,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct SeqNoteGeom {
        pub rect: Rect,
        pub lines: Vec<String>,
    }

    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct SeqLayout {
        pub title: Option<String>,
        pub cols: Vec<SeqCol>,
        pub lifelines: Vec<(Pos2, Pos2)>,
        pub arrows: Vec<SeqArrowGeom>,
        pub notes: Vec<SeqNoteGeom>,
        pub activations: Vec<Rect>,
        pub size: Vec2,
        pub notice: Option<String>,
    }

    /// シーケンス図を「参加者の列 × 時間の行」で配置する。
    pub fn layout_seq(d: &Seq, m: Metrics) -> SeqLayout {
        let np = d.participants.len();
        if np == 0 {
            return SeqLayout {
                size: vec2(0.0, 0.0),
                notice: d.notice.clone(),
                ..Default::default()
            };
        }
        let head_h = m.line_h * 2.0 + m.pad_y * 2.0;
        let row_h = m.line_h * 2.2;
        let title_h = if d.title.is_some() {
            m.line_h * 1.8
        } else {
            0.0
        };

        let labels: Vec<Vec<String>> = d
            .participants
            .iter()
            .map(|p| wrap_label(&p.label, LABEL_COLS.min(16)))
            .collect();
        // 列幅は「見出し」と「その列に触るメッセージ文」の両方から決める
        let mut col_w = vec![0.0f32; np];
        for (i, ls) in labels.iter().enumerate() {
            let cols = ls.iter().map(|l| disp_cols(l)).max().unwrap_or(1);
            col_w[i] = cols as f32 * m.char_w + m.pad_x * 2.0;
        }
        let mut gap_need = vec![0.0f32; np.saturating_sub(1).max(1)];
        for ev in &d.events {
            if let SeqEvent::Msg { from, to, text, .. } = ev {
                if from == to || text.is_empty() {
                    continue;
                }
                let (a, b) = (*from.min(to), *from.max(to));
                let need = disp_cols(text) as f32 * m.char_w + 24.0;
                let span = (b - a).max(1);
                let per = need / span as f32;
                for k in a..b {
                    if k < gap_need.len() {
                        gap_need[k] = gap_need[k].max(per);
                    }
                }
            }
        }
        let mut xs = vec![0.0f32; np];
        let mut x = m.margin;
        for i in 0..np {
            xs[i] = x + col_w[i] * 0.5;
            let gap = if i < np - 1 {
                let a = col_w[i] * 0.5 + col_w[i + 1] * 0.5;
                (a + m.gap_cross).max(gap_need.get(i).copied().unwrap_or(0.0))
            } else {
                // 最後の列は「見出しの右端まで」進める (図幅の計算根拠)
                col_w[i]
            };
            x += gap;
        }
        let mut width = x + m.margin;

        let mut cols = Vec::with_capacity(np);
        for i in 0..np {
            let head = Rect::from_center_size(
                pos2(xs[i], m.margin + title_h + head_h * 0.5),
                vec2(col_w[i], head_h),
            );
            cols.push(SeqCol {
                label: labels[i].clone(),
                head,
                x: xs[i],
            });
        }

        let top = m.margin + title_h + head_h;
        let mut arrows = Vec::new();
        let mut notes = Vec::new();
        let mut y = top + row_h * 0.6;
        let mut row_y = Vec::with_capacity(d.events.len() + 1);
        for ev in &d.events {
            row_y.push(y);
            match ev {
                SeqEvent::Msg {
                    from,
                    to,
                    text,
                    dashed,
                    arrow,
                } => {
                    let loopback = from == to;
                    let (a, b) = (xs[*from], xs[*to]);
                    if loopback {
                        width = width.max(a + 34.0 + m.margin);
                    }
                    arrows.push(SeqArrowGeom {
                        from: pos2(a, y),
                        to: pos2(
                            if loopback { a + 34.0 } else { b },
                            if loopback { y + row_h * 0.7 } else { y },
                        ),
                        text: text.clone(),
                        dashed: *dashed,
                        arrow: *arrow,
                        loopback,
                    });
                    y += if loopback { row_h * 1.6 } else { row_h };
                }
                SeqEvent::Note {
                    from,
                    to,
                    text,
                    pos,
                } => {
                    let ls = wrap_label(text, LABEL_COLS);
                    let w = (ls.iter().map(|l| disp_cols(l)).max().unwrap_or(1) as f32) * m.char_w
                        + m.pad_x * 2.0;
                    let h = ls.len() as f32 * m.line_h + m.pad_y * 2.0;
                    // 左寄せのノートが図の外へはみ出さないよう内側へ寄せる
                    let cx = match pos {
                        NotePos::Over => (xs[*from] + xs[*to]) * 0.5,
                        NotePos::LeftOf => xs[*from] - w * 0.5 - 12.0,
                        NotePos::RightOf => xs[*from] + w * 0.5 + 12.0,
                    }
                    .max(w * 0.5 + m.margin);
                    width = width.max(cx + w * 0.5 + m.margin);
                    notes.push(SeqNoteGeom {
                        rect: Rect::from_center_size(
                            pos2(cx, y + h * 0.5 - m.line_h * 0.3),
                            vec2(w, h),
                        ),
                        lines: ls,
                    });
                    y += h + m.pad_y * 2.0;
                }
            }
        }
        row_y.push(y);
        let bottom = y + row_h * 0.4;

        let lifelines = (0..np)
            .map(|i| (pos2(xs[i], top), pos2(xs[i], bottom)))
            .collect();

        let activations = d
            .activations
            .iter()
            .filter_map(|&(p, a, b)| {
                let ya = *row_y.get(a)?;
                let yb = *row_y.get(b.min(row_y.len() - 1))?;
                Some(Rect::from_min_max(
                    pos2(xs[p] - 4.0, ya - m.line_h * 0.4),
                    pos2(xs[p] + 4.0, (yb + m.line_h * 0.4).max(ya + m.line_h)),
                ))
            })
            .collect();

        SeqLayout {
            title: d.title.clone(),
            cols,
            lifelines,
            arrows,
            notes,
            activations,
            size: vec2(width.max(80.0), bottom + m.margin),
            notice: d.notice.clone(),
        }
    }

    // ── キャッシュ ──────────────────────────────────────────────────

    /// 描画に使える形まで畳んだ結果。
    #[derive(Debug, Clone, PartialEq)]
    pub enum Prepared {
        Flow(FlowLayout),
        Seq(SeqLayout),
        /// ソースコードブロックへ落とす (一行注記つき)
        Fallback(String),
    }

    /// 解析 + 配置。フレームごとに呼ばれても計算は 1 回きり。
    pub fn build(src: &str, m: Metrics) -> Prepared {
        match parse(src) {
            Diagram::Flow(g) => Prepared::Flow(layout_flow(&g, m)),
            Diagram::Seq(d) => Prepared::Seq(layout_seq(&d, m)),
            Diagram::Unsupported(kind) => Prepared::Fallback(unsupported_notice(&kind)),
            Diagram::Invalid => Prepared::Fallback(invalid_notice()),
        }
    }

    thread_local! {
        /// (鍵, 結果) の小さな LRU。UI スレッド専有なのでロック不要。
        static CACHE: RefCell<Vec<(u64, Rc<Prepared>)>> = const { RefCell::new(Vec::new()) };
    }

    fn cache_key(src: &str, m: Metrics) -> u64 {
        // DefaultHasher は固定鍵なので同じ入力からは同じ値になる
        let mut h = DefaultHasher::new();
        src.hash(&mut h);
        m.hash_into(&mut h);
        h.finish()
    }

    /// フェンス内容と寸法で引くレイアウトキャッシュ。
    pub fn cached(src: &str, m: Metrics) -> Rc<Prepared> {
        let key = cache_key(src, m);
        if let Some(hit) = CACHE.with(|c| {
            let mut c = c.borrow_mut();
            c.iter().position(|(k, _)| *k == key).map(|i| {
                let e = c.remove(i);
                let v = e.1.clone();
                c.push(e);
                v
            })
        }) {
            return hit;
        }
        let built = Rc::new(build(src, m));
        CACHE.with(|c| {
            let mut c = c.borrow_mut();
            c.push((key, built.clone()));
            while c.len() > CACHE_CAP {
                c.remove(0);
            }
        });
        built
    }

    /// テスト用: キャッシュを空にする。
    #[cfg(test)]
    pub fn clear_cache() {
        CACHE.with(|c| c.borrow_mut().clear());
    }

    // ─── テスト ─────────────────────────────────────────────────────
    //
    // 描画は egui のウィジェットなので、ここでは「解析」と「配置」だけを
    // 純関数として突く。図が壊れないことの保証はこの層で取り切る。

    #[cfg(test)]
    mod tests {
        use super::*;

        fn flow(src: &str) -> Flow {
            match parse(src) {
                Diagram::Flow(g) => g,
                other => panic!("フローチャートとして読めない: {other:?}"),
            }
        }

        fn seq(src: &str) -> Seq {
            match parse(src) {
                Diagram::Seq(d) => d,
                other => panic!("シーケンス図として読めない: {other:?}"),
            }
        }

        fn ids(g: &Flow) -> Vec<&str> {
            g.nodes.iter().map(|n| n.id.as_str()).collect()
        }

        /// 辺を「始点ID 線種 矢印 ラベル 終点ID」の見やすい形へ。
        fn edge_str(g: &Flow, e: &Edge) -> String {
            let line = match e.line {
                Line::Solid => "-",
                Line::Dotted => ".",
                Line::Thick => "=",
            };
            format!(
                "{}{}{}{}{}{}",
                g.nodes[e.from].id,
                if e.arrow_from { "<" } else { "" },
                line,
                if e.arrow_to { ">" } else { "" },
                if e.label.is_empty() {
                    String::new()
                } else {
                    format!("|{}|", e.label)
                },
                g.nodes[e.to].id,
            )
        }

        fn edges_str(g: &Flow) -> Vec<String> {
            g.edges.iter().map(|e| edge_str(g, e)).collect()
        }

        fn overlaps(a: Rect, b: Rect) -> bool {
            a.min.x < b.max.x - 0.01
                && b.min.x < a.max.x - 0.01
                && a.min.y < b.max.y - 0.01
                && b.min.y < a.max.y - 0.01
        }

        // ---- 解析: ノード形状 -------------------------------------------

        #[test]
        fn node_shape_table() {
            let cases: &[(&str, Shape, &str)] = &[
                ("A[Box]", Shape::Rect, "Box"),
                ("A(Round)", Shape::Round, "Round"),
                ("A([Stadium])", Shape::Stadium, "Stadium"),
                ("A[[Sub]]", Shape::Subroutine, "Sub"),
                ("A[(DB)]", Shape::Cylinder, "DB"),
                ("A((Circle))", Shape::Circle, "Circle"),
                ("A{Diamond}", Shape::Diamond, "Diamond"),
                ("A{{Hex}}", Shape::Hexagon, "Hex"),
                ("A>Flag]", Shape::Asymmetric, "Flag"),
                ("A", Shape::Rect, "A"),
                ("A[\"引用 付き\"]", Shape::Rect, "引用 付き"),
                ("A[1行目<br/>2行目]", Shape::Rect, "1行目\n2行目"),
            ];
            for (src, shape, label) in cases {
                let (id, got_label, got_shape) = node_spec(src).expect(src);
                assert_eq!(id, "A", "{src} の ID");
                assert_eq!(got_shape, *shape, "{src} の形");
                assert_eq!(got_label, *label, "{src} のラベル");
            }
        }

        #[test]
        fn node_shape_survives_broken_brackets() {
            // 閉じ括弧が無くても ID だけは拾って図にする
            let (id, label, shape) = node_spec("A[未完").expect("壊れていても読む");
            assert_eq!((id.as_str(), shape), ("A", Shape::Rect));
            assert_eq!(label, "A");
            assert_eq!(node_spec("   "), None);
            assert_eq!(node_spec("[label]"), None);
        }

        // ---- 解析: 辺 ---------------------------------------------------

        #[test]
        fn edge_style_table() {
            let cases: &[(&str, &str)] = &[
                ("graph TD\nA-->B", "A->B"),
                ("graph TD\nA --- B", "A-B"),
                ("graph TD\nA-.->B", "A.>B"),
                ("graph TD\nA -.- B", "A.B"),
                ("graph TD\nA==>B", "A=>B"),
                ("graph TD\nA === B", "A=B"),
                ("graph TD\nA<-->B", "A<->B"),
                ("graph TD\nA --x B", "A->B"),
                ("graph TD\nA --o B", "A->B"),
                ("graph TD\nA-->|はい|B", "A->|はい|B"),
                ("graph TD\nA -- ラベル --> B", "A->|ラベル|B"),
                ("graph TD\nA -. 点線 .-> B", "A.>|点線|B"),
                ("graph TD\nA == 太線 ==> B", "A=>|太線|B"),
                ("graph TD\nA ---|開いた線| B", "A-|開いた線|B"),
            ];
            for (src, want) in cases {
                let g = flow(src);
                assert_eq!(edges_str(&g), vec![want.to_string()], "入力: {src}");
            }
        }

        #[test]
        fn chains_create_every_hop() {
            let g = flow("graph LR\nA-->B-->C-.->D");
            assert_eq!(ids(&g), ["A", "B", "C", "D"]);
            assert_eq!(edges_str(&g), ["A->B", "B->C", "C.>D"]);
        }

        #[test]
        fn statements_split_on_semicolon_and_newline() {
            let g = flow("graph TD; A-->B; B-->C;\nC-->A");
            assert_eq!(ids(&g), ["A", "B", "C"]);
            assert_eq!(g.edges.len(), 3);
        }

        // ---- 解析: 向き・サブグラフ・注釈 --------------------------------

        #[test]
        fn direction_variants() {
            for (tok, want) in [
                ("TD", Dir::Down),
                ("TB", Dir::Down),
                ("BT", Dir::Up),
                ("LR", Dir::Right),
                ("RL", Dir::Left),
                ("lr", Dir::Right),
                ("", Dir::Down),
                ("XX", Dir::Down),
            ] {
                let g = flow(&format!("graph {tok}\nA-->B"));
                assert_eq!(g.dir, want, "graph {tok}");
            }
            assert_eq!(flow("flowchart LR\nA-->B").dir, Dir::Right);
            assert_eq!(flow("graph TD;\nA-->B").dir, Dir::Down);
        }

        #[test]
        fn subgraphs_collect_their_nodes() {
            let g = flow(
                "graph TD\n\
                 subgraph front[フロント]\n\
                 A-->B\n\
                 end\n\
                 subgraph back\n\
                 C\n\
                 end\n\
                 B-->C",
            );
            assert_eq!(g.groups.len(), 2);
            assert_eq!(g.groups[0].title, "フロント");
            assert_eq!(g.groups[0].nodes, vec![0, 1]);
            assert_eq!(g.groups[1].title, "back");
            assert_eq!(g.groups[1].nodes, vec![2]);
            assert_eq!(g.edges.len(), 2);
        }

        #[test]
        fn comments_and_styling_are_ignored() {
            let g = flow(
                "%%{init: {'theme':'dark'}}%%\n\
                 graph TD\n\
                 %% これは注釈\n\
                 A-->B %% 行末の注釈\n\
                 style A fill:#f00\n\
                 classDef big font-size:20px\n\
                 click A callback\n\
                 linkStyle 0 stroke:#333",
            );
            assert_eq!(ids(&g), ["A", "B"]);
            assert_eq!(g.edges.len(), 1);
        }

        #[test]
        fn percent_inside_quotes_is_not_a_comment() {
            let g = flow("graph TD\nA[\"50%% 完了\"]-->B");
            assert_eq!(g.nodes[0].label, "50%% 完了");
        }

        // ---- 解析: 退避先 ------------------------------------------------

        #[test]
        fn unknown_diagram_types_degrade() {
            for kind in [
                "classDiagram",
                "gantt",
                "stateDiagram-v2",
                "pie",
                "erDiagram",
                "journey",
            ] {
                let src = format!("{kind}\n  なにか\n");
                match parse(&src) {
                    Diagram::Unsupported(got) => {
                        assert_eq!(got, kind, "{kind}");
                        let note = unsupported_notice(&got);
                        assert!(note.contains(kind), "注記に図種が入っていない: {note}");
                    }
                    other => panic!("{kind} は未対応に落ちるべき: {other:?}"),
                }
            }
        }

        #[test]
        fn empty_and_bodyless_input_is_invalid() {
            assert_eq!(parse(""), Diagram::Invalid);
            assert_eq!(parse("   \n\n  "), Diagram::Invalid);
            assert_eq!(parse("%% 注釈だけ"), Diagram::Invalid);
            assert_eq!(parse("graph TD"), Diagram::Invalid);
            assert_eq!(parse("sequenceDiagram"), Diagram::Invalid);
            assert!(!invalid_notice().is_empty());
        }

        #[test]
        fn malformed_lines_never_panic() {
            let junk = [
                "graph TD\nA-->",
                "graph TD\n-->B",
                "graph TD\nA[[[[[[",
                "graph TD\nA-->|閉じない B",
                "graph TD\n|||||",
                "graph TD\nA{{{{}}}}-->B",
                "graph TD\nsubgraph\nend\nend\nend",
                "graph TD\nA -- -- -- B",
                "graph TD\n\u{0}\u{1}",
                "graph LR\nA((()))-->B",
                "flowchart\nA==>>B",
                "graph TD\nA-.-.-.-.->B",
            ];
            for src in junk {
                // 落ちない・固まらないことだけを見る (中身は問わない)
                let d = parse(src);
                if let Diagram::Flow(g) = d {
                    let _ = layout_flow(&g, Metrics::default());
                }
            }
        }

        #[test]
        fn caps_are_enforced_with_a_notice() {
            let mut src = String::from("graph TD\n");
            for i in 0..(MAX_NODES + 40) {
                src.push_str(&format!("N{i}-->N{}\n", i + 1));
            }
            let g = flow(&src);
            assert_eq!(g.nodes.len(), MAX_NODES, "ノード数の上限");
            assert!(g.edges.len() <= MAX_EDGES, "辺数の上限");
            let note = g.notice.expect("上限に当たったら必ず注記を出す");
            assert!(note.contains(&MAX_NODES.to_string()), "注記: {note}");
        }

        #[test]
        fn edge_cap_is_enforced() {
            let mut src = String::from("graph TD\n");
            // 10 ノードの完全グラフ (90 辺) を 5 回書いて 400 辺を超えさせる
            for _ in 0..5 {
                for a in 0..10 {
                    for b in 0..10 {
                        if a != b {
                            src.push_str(&format!("N{a}-->N{b}\n"));
                        }
                    }
                }
            }
            let g = flow(&src);
            assert_eq!(g.edges.len(), MAX_EDGES);
            assert!(g.notice.is_some(), "辺の上限でも注記を出す");
        }

        // ---- 配置の不変条件 ----------------------------------------------

        const DAG: &str = "graph TD\n\
                           A[開始]-->B{分岐}\n\
                           B-->|はい|C(処理)\n\
                           B-->|いいえ|D((終了))\n\
                           C-->E[[まとめ]]\n\
                           D-->E\n\
                           A-->E";

        #[test]
        fn ranks_are_monotone_along_edges() {
            let g = flow(DAG);
            let r = ranks(g.nodes.len(), &g.edges);
            for e in &g.edges {
                assert!(
                    r[e.to] > r[e.from],
                    "{} → {} でランクが進んでいない ({} → {})",
                    g.nodes[e.from].id,
                    g.nodes[e.to].id,
                    r[e.from],
                    r[e.to]
                );
            }
        }

        #[test]
        fn ranks_terminate_on_cycles() {
            let g = flow("graph LR\nA-->B\nB-->C\nC-->A\nC-->D");
            let r = ranks(g.nodes.len(), &g.edges);
            assert!(
                r.iter().all(|&x| x < g.nodes.len()),
                "循環でランクが発散した: {r:?}"
            );
            let lay = layout_flow(&g, Metrics::default());
            assert_eq!(lay.boxes.len(), 4);
        }

        #[test]
        fn nodes_never_overlap_and_stay_inside_bounds() {
            for src in [
                DAG,
                "graph LR\nA-->B-->C\nA-->C",
                "graph BT\nA-->B\nA-->C\nA-->D\nB-->E",
            ] {
                let g = flow(src);
                let lay = layout_flow(&g, Metrics::default());
                let bounds = Rect::from_min_size(pos2(0.0, 0.0), lay.size);
                for b in &lay.boxes {
                    assert!(
                        bounds.contains_rect(b.rect),
                        "{} が図の外に出た: {:?} / 図 {:?}",
                        g.nodes[b.node].id,
                        b.rect,
                        lay.size
                    );
                }
                for (i, a) in lay.boxes.iter().enumerate() {
                    for b in &lay.boxes[i + 1..] {
                        assert!(
                            !overlaps(a.rect, b.rect),
                            "{} と {} が重なった",
                            g.nodes[a.node].id,
                            g.nodes[b.node].id
                        );
                    }
                }
            }
        }

        #[test]
        fn edge_endpoints_sit_on_node_borders() {
            let g = flow(DAG);
            let lay = layout_flow(&g, Metrics::default());
            for (e, geom) in g.edges.iter().zip(&lay.edges) {
                let (a, b) = (lay.boxes[e.from].rect, lay.boxes[e.to].rect);
                let p0 = geom.points[0];
                let p1 = *geom.points.last().unwrap();
                assert!(
                    a.expand(1.0).contains(p0),
                    "始点が枠から離れた: {p0:?} / {a:?}"
                );
                assert!(
                    b.expand(1.0).contains(p1),
                    "終点が枠から離れた: {p1:?} / {b:?}"
                );
            }
        }

        #[test]
        fn horizontal_and_vertical_flow_swap_the_axes() {
            let td = layout_flow(&flow("graph TD\nA-->B"), Metrics::default());
            let lr = layout_flow(&flow("graph LR\nA-->B"), Metrics::default());
            assert!(
                td.boxes[0].rect.center().y < td.boxes[1].rect.center().y,
                "TD は上から下"
            );
            assert!(
                lr.boxes[0].rect.center().x < lr.boxes[1].rect.center().x,
                "LR は左から右"
            );
            let bt = layout_flow(&flow("graph BT\nA-->B"), Metrics::default());
            assert!(
                bt.boxes[0].rect.center().y > bt.boxes[1].rect.center().y,
                "BT は下から上"
            );
            let rl = layout_flow(&flow("graph RL\nA-->B"), Metrics::default());
            assert!(
                rl.boxes[0].rect.center().x > rl.boxes[1].rect.center().x,
                "RL は右から左"
            );
        }

        /// 同じ入力からは必ず同じ座標が出る。
        /// (HashMap の反復順が座標に漏れると、同一プロセス内でも別インスタンスは
        ///  異なる乱数鍵を持つため、この 2 回比較で必ず露見する)
        #[test]
        fn layout_is_deterministic() {
            let src = "graph TD\n\
                       A-->B\nA-->C\nA-->D\nB-->E\nC-->E\nD-->F\nE-->G\nF-->G\n\
                       G-->H\nH-->A\nsubgraph s1[群]\nB\nC\nend";
            let one = layout_flow(&flow(src), Metrics::default());
            let two = layout_flow(&flow(src), Metrics::default());
            assert_eq!(one, two, "同じ入力から違う配置が出た (反復順が漏れている)");
            let three = layout_flow(&flow(src), Metrics::default());
            assert_eq!(one, three);
        }

        #[test]
        fn subgraph_frames_wrap_their_nodes() {
            let g = flow("graph TD\nsubgraph s[群]\nA-->B\nend\nB-->C");
            let lay = layout_flow(&g, Metrics::default());
            assert_eq!(lay.groups.len(), 1);
            let (title, frame) = &lay.groups[0];
            assert_eq!(title, "群");
            assert!(frame.contains_rect(lay.boxes[0].rect));
            assert!(frame.contains_rect(lay.boxes[1].rect));
        }

        #[test]
        fn single_node_graph_has_positive_size() {
            let lay = layout_flow(&flow("graph TD\nA[ひとつだけ]"), Metrics::default());
            assert_eq!(lay.boxes.len(), 1);
            assert!(lay.size.x > 0.0 && lay.size.y > 0.0);
            assert!(Rect::from_min_size(pos2(0.0, 0.0), lay.size).contains_rect(lay.boxes[0].rect));
        }

        // ---- ラベルの折返し ----------------------------------------------

        #[test]
        fn wrap_label_respects_width_and_breaks() {
            assert_eq!(wrap_label("短い", 22), vec!["短い"]);
            assert_eq!(wrap_label("上\n下", 22), vec!["上", "下"]);
            let w = wrap_label("the quick brown fox jumps over the lazy dog", 12);
            assert!(w.len() > 1, "長い文は折り返す: {w:?}");
            assert!(
                w.iter().all(|l| disp_cols(l) <= 12),
                "はみ出した行がある: {w:?}"
            );
            // CJK は全角 2 桁で数える
            let j = wrap_label("あいうえおかきくけこさしすせそ", 10);
            assert!(j.iter().all(|l| disp_cols(l) <= 10), "{j:?}");
            // max より長い単語も必ず分割される
            let long = wrap_label("supercalifragilisticexpialidocious", 8);
            assert!(long.iter().all(|l| disp_cols(l) <= 8), "{long:?}");
            assert_eq!(wrap_label("", 10), vec![""]);
        }

        #[test]
        fn disp_cols_counts_wide_characters() {
            assert_eq!(disp_cols("abc"), 3);
            assert_eq!(disp_cols("あい"), 4);
            assert_eq!(disp_cols("あa"), 3);
        }

        // ---- シーケンス図 --------------------------------------------------

        const SEQ: &str = "sequenceDiagram\n\
                           participant A as 利用者\n\
                           participant B as サーバ\n\
                           A->>B: 要求\n\
                           activate B\n\
                           B-->>A: 応答\n\
                           deactivate B\n\
                           Note over A,B: 往復ここまで\n\
                           A-)B: 非同期\n\
                           B-xA: 失敗";

        #[test]
        fn sequence_parse_table() {
            let d = seq(SEQ);
            assert_eq!(
                d.participants
                    .iter()
                    .map(|p| (p.id.as_str(), p.label.as_str()))
                    .collect::<Vec<_>>(),
                [("A", "利用者"), ("B", "サーバ")]
            );
            let kinds: Vec<String> = d
                .events
                .iter()
                .map(|e| match e {
                    SeqEvent::Msg {
                        from,
                        to,
                        text,
                        dashed,
                        arrow,
                    } => {
                        format!("{from}->{to} {arrow:?} dashed={dashed} {text}")
                    }
                    SeqEvent::Note {
                        from,
                        to,
                        text,
                        pos,
                    } => format!("note {pos:?} {from}..{to} {text}"),
                })
                .collect();
            assert_eq!(
                kinds,
                [
                    "0->1 Solid dashed=false 要求",
                    "1->0 Solid dashed=true 応答",
                    "note Over 0..1 往復ここまで",
                    "0->1 Async dashed=false 非同期",
                    "1->0 Cross dashed=false 失敗",
                ]
            );
            assert_eq!(d.activations, vec![(1, 1, 2)]);
        }

        #[test]
        fn sequence_arrow_forms() {
            for (src, want) in [
                ("A->B: x", SeqArrow::Open),
                ("A->>B: x", SeqArrow::Solid),
                ("A-->B: x", SeqArrow::Open),
                ("A-->>B: x", SeqArrow::Solid),
                ("A-xB: x", SeqArrow::Cross),
                ("A--xB: x", SeqArrow::Cross),
                ("A-)B: x", SeqArrow::Async),
                ("A--)B: x", SeqArrow::Async),
            ] {
                let (_, arrow, _, _, text) = seq_msg(src).expect(src);
                assert_eq!(arrow, want, "{src}");
                assert_eq!(text, "x", "{src}");
            }
            assert!(seq_msg("ただの文").is_none());
            assert!(seq_msg("->B: 始点が無い").is_none());
        }

        #[test]
        fn sequence_notes_and_blocks() {
            let d = seq("sequenceDiagram\n\
                 participant A\n\
                 participant B\n\
                 Note left of A: 左\n\
                 Note right of B: 右\n\
                 loop 毎日\n\
                 A->>B: 呼ぶ\n\
                 end\n\
                 autonumber");
            let notes: Vec<_> = d
                .events
                .iter()
                .filter_map(|e| match e {
                    SeqEvent::Note { pos, text, .. } => Some((*pos, text.clone())),
                    _ => None,
                })
                .collect();
            assert_eq!(
                notes,
                [
                    (NotePos::LeftOf, "左".into()),
                    (NotePos::RightOf, "右".into())
                ]
            );
            // loop / end / autonumber は骨格に効かず、中のメッセージは残る
            assert_eq!(d.events.len(), 3);
        }

        #[test]
        fn sequence_implicit_participants_keep_first_seen_order() {
            let d = seq("sequenceDiagram\nC->>A: 1\nB->>C: 2");
            assert_eq!(
                d.participants
                    .iter()
                    .map(|p| p.id.as_str())
                    .collect::<Vec<_>>(),
                ["C", "A", "B"]
            );
        }

        #[test]
        fn sequence_layout_assigns_columns_and_rows() {
            let d = seq(SEQ);
            let lay = layout_seq(&d, Metrics::default());
            assert_eq!(lay.cols.len(), 2);
            assert!(lay.cols[0].x < lay.cols[1].x, "参加者は左から順に列を持つ");
            // 見出しは同じ高さに並ぶ
            assert_eq!(lay.cols[0].head.top(), lay.cols[1].head.top());
            // 事象は時間順に下がる
            let ys: Vec<f32> = lay.arrows.iter().map(|a| a.from.y).collect();
            assert!(
                ys.windows(2).all(|w| w[0] < w[1]),
                "行が時間順に下がっていない: {ys:?}"
            );
            // 生存線は見出しの下から図の下端まで
            for (i, (a, b)) in lay.lifelines.iter().enumerate() {
                assert_eq!(a.x, lay.cols[i].x);
                assert!(a.y >= lay.cols[i].head.bottom() - 0.01);
                assert!(b.y > a.y);
            }
            assert_eq!(lay.activations.len(), 1);
            assert!(lay.notes.len() == 1);
            let bounds = Rect::from_min_size(pos2(0.0, 0.0), lay.size);
            for c in &lay.cols {
                assert!(bounds.contains_rect(c.head), "見出しが図の外: {:?}", c.head);
            }
        }

        #[test]
        fn sequence_layout_is_deterministic() {
            let a = layout_seq(&seq(SEQ), Metrics::default());
            let b = layout_seq(&seq(SEQ), Metrics::default());
            assert_eq!(a, b);
        }

        #[test]
        fn sequence_self_message_is_a_loopback() {
            let lay = layout_seq(&seq("sequenceDiagram\nA->>A: 自分へ"), Metrics::default());
            assert_eq!(lay.arrows.len(), 1);
            assert!(lay.arrows[0].loopback);
            assert!(lay.arrows[0].to.y > lay.arrows[0].from.y);
        }

        // ---- キャッシュ ----------------------------------------------------

        #[test]
        fn cache_returns_the_same_layout_for_the_same_source() {
            clear_cache();
            let src = "graph TD\nA-->B-->C";
            let m = Metrics::default();
            let a = cached(src, m);
            let b = cached(src, m);
            assert!(Rc::ptr_eq(&a, &b), "同じ入力で作り直している");
            assert_eq!(&*a, &build(src, m));
            // 寸法が変われば作り直す
            let m2 = Metrics {
                char_w: m.char_w + 1.0,
                ..m
            };
            let c = cached(src, m2);
            assert!(!Rc::ptr_eq(&a, &c));
            clear_cache();
        }

        #[test]
        fn build_never_leaves_a_blank_result() {
            for src in [
                "",
                "gantt\n title x",
                "graph TD\nA-->B",
                SEQ,
                "!!!壊れた!!!",
            ] {
                match build(src, Metrics::default()) {
                    Prepared::Fallback(note) => assert!(!note.is_empty(), "注記が空: {src}"),
                    Prepared::Flow(l) => assert!(!l.boxes.is_empty()),
                    Prepared::Seq(l) => assert!(!l.cols.is_empty()),
                }
            }
        }
    }
}

// ─── 数式 (TeX サブセット) ──────────────────────────────────────────
//
// `$…$` `\(…\)` (行内) と `$$…$$` `\[…\]` (別行立て) を、依存追加なしで
// 読める形に描く。対応するのは実文書に出る範囲だけ:
//   上下付き (入れ子可) / \frac / \sqrt[n]{} / ギリシャ文字と常用記号 /
//   \left…\right の大きさ合わせ (近似) / \text / 行列 (pmatrix 等) /
//   \sin \cos \log \lim などの立体関数名 / \vec \bar \hat \overline。
// 未対応の命令は「生 TeX を等幅で、控えめな印つきで」出す。空白にはしない。
//
// 記号は「同梱フォントに字があるか」を描画時に確かめ、無ければ ASCII 代替へ
// 落とす (豆腐 □ にしない)。SYMBOLS の ascii 列がその代替。

pub mod math {
    /// 1 つの数式で扱う TeX の最大長 (これを超えたら生表示に落とす)。
    pub const MAX_LEN: usize = 4000;
    /// 再帰の深さ上限 (`{{{{…` でスタックを溶かさない)。
    pub const MAX_DEPTH: usize = 24;

    // ── 記号表 ──────────────────────────────────────────────────────

    /// 記号の役割 (前後の空きの決定に使う)。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Class {
        /// 普通の記号・変数
        Ord,
        /// 二項演算子 (+ − × …)
        Bin,
        /// 関係子 (= ≤ → …)
        Rel,
        /// 大型演算子 (∑ ∏ ∫)
        BigOp,
        /// 句読点
        Punct,
        /// 開き括弧
        Open,
        /// 閉じ括弧
        Close,
    }

    /// TeX 名 → 表示字 → フォントに無いときの ASCII 代替。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Sym {
        pub tex: &'static str,
        pub glyph: &'static str,
        /// 同梱フォントに glyph が無いときに使う。必ず ASCII だけで書く
        pub ascii: &'static str,
        pub class: Class,
    }

    const fn s(tex: &'static str, glyph: &'static str, ascii: &'static str, class: Class) -> Sym {
        Sym {
            tex,
            glyph,
            ascii,
            class,
        }
    }

    /// 対応記号の全表。ここに無い `\macro` は生 TeX として出す。
    pub const SYMBOLS: &[Sym] = &[
        // ギリシャ小文字
        s("alpha", "α", "alpha", Class::Ord),
        s("beta", "β", "beta", Class::Ord),
        s("gamma", "γ", "gamma", Class::Ord),
        s("delta", "δ", "delta", Class::Ord),
        s("epsilon", "ε", "eps", Class::Ord),
        s("varepsilon", "ε", "eps", Class::Ord),
        s("zeta", "ζ", "zeta", Class::Ord),
        s("eta", "η", "eta", Class::Ord),
        s("theta", "θ", "theta", Class::Ord),
        s("vartheta", "ϑ", "theta", Class::Ord),
        s("iota", "ι", "iota", Class::Ord),
        s("kappa", "κ", "kappa", Class::Ord),
        s("lambda", "λ", "lambda", Class::Ord),
        s("mu", "μ", "mu", Class::Ord),
        s("nu", "ν", "nu", Class::Ord),
        s("xi", "ξ", "xi", Class::Ord),
        s("omicron", "ο", "o", Class::Ord),
        s("pi", "π", "pi", Class::Ord),
        s("varpi", "ϖ", "pi", Class::Ord),
        s("rho", "ρ", "rho", Class::Ord),
        s("varrho", "ϱ", "rho", Class::Ord),
        s("sigma", "σ", "sigma", Class::Ord),
        s("varsigma", "ς", "sigma", Class::Ord),
        s("tau", "τ", "tau", Class::Ord),
        s("upsilon", "υ", "upsilon", Class::Ord),
        s("phi", "φ", "phi", Class::Ord),
        s("varphi", "ϕ", "phi", Class::Ord),
        s("chi", "χ", "chi", Class::Ord),
        s("psi", "ψ", "psi", Class::Ord),
        s("omega", "ω", "omega", Class::Ord),
        // ギリシャ大文字
        s("Gamma", "Γ", "Gamma", Class::Ord),
        s("Delta", "Δ", "Delta", Class::Ord),
        s("Theta", "Θ", "Theta", Class::Ord),
        s("Lambda", "Λ", "Lambda", Class::Ord),
        s("Xi", "Ξ", "Xi", Class::Ord),
        s("Pi", "Π", "Pi", Class::Ord),
        s("Sigma", "Σ", "Sigma", Class::Ord),
        s("Upsilon", "Υ", "Upsilon", Class::Ord),
        s("Phi", "Φ", "Phi", Class::Ord),
        s("Psi", "Ψ", "Psi", Class::Ord),
        s("Omega", "Ω", "Omega", Class::Ord),
        // 大型演算子
        s("sum", "∑", "SUM", Class::BigOp),
        s("prod", "∏", "PROD", Class::BigOp),
        s("coprod", "∐", "COPROD", Class::BigOp),
        s("int", "∫", "INT", Class::BigOp),
        s("iint", "∬", "INT2", Class::BigOp),
        s("oint", "∮", "OINT", Class::BigOp),
        s("bigcup", "∪", "UNION", Class::BigOp),
        s("bigcap", "∩", "INTER", Class::BigOp),
        // 二項演算子
        s("pm", "±", "+/-", Class::Bin),
        s("mp", "∓", "-/+", Class::Bin),
        s("times", "×", "x", Class::Bin),
        s("div", "÷", "/", Class::Bin),
        s("cdot", "⋅", "*", Class::Bin),
        s("ast", "∗", "*", Class::Bin),
        s("star", "⋆", "*", Class::Bin),
        s("circ", "∘", "o", Class::Bin),
        s("bullet", "∙", ".", Class::Bin),
        s("oplus", "⊕", "(+)", Class::Bin),
        s("ominus", "⊖", "(-)", Class::Bin),
        s("otimes", "⊗", "(x)", Class::Bin),
        s("cup", "∪", "U", Class::Bin),
        s("cap", "∩", "^", Class::Bin),
        s("setminus", "∖", "\\", Class::Bin),
        s("wedge", "∧", "and", Class::Bin),
        s("land", "∧", "and", Class::Bin),
        s("vee", "∨", "or", Class::Bin),
        s("lor", "∨", "or", Class::Bin),
        s("neg", "¬", "not", Class::Ord),
        s("lnot", "¬", "not", Class::Ord),
        // 関係子
        s("le", "≤", "<=", Class::Rel),
        s("leq", "≤", "<=", Class::Rel),
        s("ge", "≥", ">=", Class::Rel),
        s("geq", "≥", ">=", Class::Rel),
        s("ne", "≠", "!=", Class::Rel),
        s("neq", "≠", "!=", Class::Rel),
        s("approx", "≈", "~=", Class::Rel),
        s("sim", "∼", "~", Class::Rel),
        s("simeq", "≃", "~=", Class::Rel),
        s("cong", "≅", "~=", Class::Rel),
        s("equiv", "≡", "==", Class::Rel),
        s("propto", "∝", "prop", Class::Rel),
        s("ll", "≪", "<<", Class::Rel),
        s("gg", "≫", ">>", Class::Rel),
        s("in", "∈", "in", Class::Rel),
        s("notin", "∉", "not in", Class::Rel),
        s("ni", "∋", "owns", Class::Rel),
        s("subset", "⊂", "sub", Class::Rel),
        s("subseteq", "⊆", "sub=", Class::Rel),
        s("supset", "⊃", "sup", Class::Rel),
        s("supseteq", "⊇", "sup=", Class::Rel),
        s("perp", "⊥", "perp", Class::Rel),
        s("parallel", "∥", "||", Class::Rel),
        s("mid", "∣", "|", Class::Rel),
        // 矢印
        s("to", "→", "->", Class::Rel),
        s("rightarrow", "→", "->", Class::Rel),
        s("longrightarrow", "⟶", "-->", Class::Rel),
        s("gets", "←", "<-", Class::Rel),
        s("leftarrow", "←", "<-", Class::Rel),
        s("leftrightarrow", "↔", "<->", Class::Rel),
        s("Rightarrow", "⇒", "=>", Class::Rel),
        s("implies", "⇒", "=>", Class::Rel),
        s("Leftarrow", "⇐", "<=", Class::Rel),
        s("Leftrightarrow", "⇔", "<=>", Class::Rel),
        s("iff", "⇔", "<=>", Class::Rel),
        s("mapsto", "↦", "|->", Class::Rel),
        s("uparrow", "↑", "^", Class::Rel),
        s("downarrow", "↓", "v", Class::Rel),
        // その他の記号
        s("infty", "∞", "inf", Class::Ord),
        s("partial", "∂", "d", Class::Ord),
        s("nabla", "∇", "grad", Class::Ord),
        s("forall", "∀", "for all", Class::Ord),
        s("exists", "∃", "exists", Class::Ord),
        s("nexists", "∄", "no", Class::Ord),
        s("emptyset", "∅", "{}", Class::Ord),
        s("varnothing", "∅", "{}", Class::Ord),
        s("therefore", "∴", "therefore", Class::Ord),
        s("because", "∵", "because", Class::Ord),
        s("angle", "∠", "angle", Class::Ord),
        s("degree", "°", "deg", Class::Ord),
        s("prime", "′", "'", Class::Ord),
        s("hbar", "ℏ", "h-", Class::Ord),
        s("ell", "ℓ", "l", Class::Ord),
        s("aleph", "ℵ", "aleph", Class::Ord),
        s("Re", "ℜ", "Re", Class::Ord),
        s("Im", "ℑ", "Im", Class::Ord),
        s("ldots", "…", "...", Class::Punct),
        s("dots", "…", "...", Class::Punct),
        s("cdots", "⋯", "...", Class::Punct),
        s("vdots", "⋮", ":", Class::Punct),
        s("ddots", "⋱", "...", Class::Punct),
        s("checkmark", "✓", "ok", Class::Ord),
        // 括弧
        s("langle", "⟨", "<", Class::Open),
        s("rangle", "⟩", ">", Class::Close),
        s("lceil", "⌈", "|~", Class::Open),
        s("rceil", "⌉", "~|", Class::Close),
        s("lfloor", "⌊", "|_", Class::Open),
        s("rfloor", "⌋", "_|", Class::Close),
        // 直接書ける記号のエスケープ
        s("%", "%", "%", Class::Ord),
        s("&", "&", "&", Class::Ord),
        s("#", "#", "#", Class::Ord),
        s("_", "_", "_", Class::Ord),
        s("$", "$", "$", Class::Ord),
        s("{", "{", "{", Class::Open),
        s("}", "}", "}", Class::Close),
        s("|", "∥", "||", Class::Ord),
    ];

    /// 立体で組む関数名 (`\sin` など)。
    pub const FUNCS: &[&str] = &[
        "sin",
        "cos",
        "tan",
        "sec",
        "csc",
        "cot",
        "arcsin",
        "arccos",
        "arctan",
        "sinh",
        "cosh",
        "tanh",
        "log",
        "ln",
        "lg",
        "exp",
        "lim",
        "limsup",
        "liminf",
        "max",
        "min",
        "sup",
        "inf",
        "det",
        "dim",
        "ker",
        "deg",
        "gcd",
        "arg",
        "Pr",
        "hom",
        "mod",
        "bmod",
        "operatorname",
    ];

    /// 空きだけを作る命令 (幅は em 比)。
    const SPACES: &[(&str, f32)] = &[
        (",", 0.17),
        (":", 0.22),
        (";", 0.28),
        ("!", -0.17),
        ("quad", 1.0),
        ("qquad", 2.0),
        (" ", 0.25),
    ];

    /// 記号表を引く。
    pub fn symbol(name: &str) -> Option<&'static Sym> {
        SYMBOLS.iter().find(|s| s.tex == name)
    }

    // ── 構文木 ──────────────────────────────────────────────────────

    /// 上に付ける印。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Accent {
        /// \bar \overline — 横線
        Bar,
        /// \vec — 矢印
        Vec,
        /// \hat — ^
        Hat,
        /// \tilde — ~
        Tilde,
        /// \dot — ・
        Dot,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub enum Node {
        /// 変数 (斜体で組む)
        Ident(String),
        /// 数字
        Num(String),
        /// 表引きできた記号
        Sym(&'static Sym),
        /// 生の演算子・句読点 (+ - = , など)
        Op(String, Class),
        /// \text{…} \mathrm{…} — 立体
        Text(String),
        /// \sin など立体の関数名
        Fun(String),
        Row(Vec<Node>),
        /// \frac{分子}{分母}
        Frac(Box<Node>, Box<Node>),
        Sqrt {
            index: Option<Box<Node>>,
            body: Box<Node>,
        },
        Script {
            base: Box<Node>,
            sup: Option<Box<Node>>,
            sub: Option<Box<Node>>,
        },
        /// \left( … \right)
        Delim {
            left: String,
            right: String,
            body: Box<Node>,
        },
        Accent {
            mark: Accent,
            body: Box<Node>,
        },
        /// pmatrix / bmatrix / cases など
        Matrix {
            left: String,
            right: String,
            rows: Vec<Vec<Node>>,
        },
        /// 幅だけの空き (em 比)
        Space(f32),
        /// 未対応 → 生 TeX をそのまま
        Raw(String),
    }

    impl Node {
        /// 空の行か。
        pub fn is_empty(&self) -> bool {
            matches!(self, Node::Row(r) if r.is_empty())
        }

        /// 前後の空きを決めるための役割。
        pub fn class(&self) -> Class {
            match self {
                Node::Sym(s) => s.class,
                Node::Op(_, c) => *c,
                Node::Row(r) => r.first().map(|n| n.class()).unwrap_or(Class::Ord),
                Node::Script { base, .. } => base.class(),
                _ => Class::Ord,
            }
        }
    }

    // ── 字句 ────────────────────────────────────────────────────────

    #[derive(Debug, Clone, PartialEq)]
    pub enum Tok {
        Chr(char),
        /// `\name` (英字列) か `\x` (記号 1 文字)
        Cmd(String),
        Open,
        Close,
        Sup,
        Sub,
        /// 行列の列区切り `&`
        Amp,
        /// 行列の行区切り `\\`
        NewRow,
    }

    /// TeX を字句へ分ける。壊れていても必ず返る。
    pub fn tokenize(src: &str) -> Vec<Tok> {
        let c: Vec<char> = src.chars().take(MAX_LEN).collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < c.len() {
            match c[i] {
                '\\' => {
                    let mut k = i + 1;
                    if k < c.len() && c[k] == '\\' {
                        out.push(Tok::NewRow);
                        i = k + 1;
                        continue;
                    }
                    if k < c.len() && c[k].is_ascii_alphabetic() {
                        while k < c.len() && c[k].is_ascii_alphabetic() {
                            k += 1;
                        }
                        out.push(Tok::Cmd(c[i + 1..k].iter().collect()));
                        i = k;
                    } else if k < c.len() {
                        out.push(Tok::Cmd(c[k].to_string()));
                        i = k + 1;
                    } else {
                        out.push(Tok::Chr('\\'));
                        i += 1;
                    }
                }
                '{' => {
                    out.push(Tok::Open);
                    i += 1;
                }
                '}' => {
                    out.push(Tok::Close);
                    i += 1;
                }
                '^' => {
                    out.push(Tok::Sup);
                    i += 1;
                }
                '_' => {
                    out.push(Tok::Sub);
                    i += 1;
                }
                '&' => {
                    out.push(Tok::Amp);
                    i += 1;
                }
                ch => {
                    out.push(Tok::Chr(ch));
                    i += 1;
                }
            }
        }
        out
    }

    struct Parser {
        t: Vec<Tok>,
        i: usize,
        depth: usize,
    }

    /// 括弧として使える 1 文字 (`\left` `\right` の引数)。
    fn delim_str(tok: &Tok) -> Option<String> {
        match tok {
            Tok::Chr(c) if matches!(c, '(' | ')' | '[' | ']' | '/' | '|' | '.') => {
                Some(c.to_string())
            }
            Tok::Open => Some("{".into()),
            Tok::Close => Some("}".into()),
            Tok::Cmd(name) => match name.as_str() {
                "{" => Some("{".into()),
                "}" => Some("}".into()),
                "|" => Some("‖".into()),
                "langle" => Some("⟨".into()),
                "rangle" => Some("⟩".into()),
                "lceil" => Some("⌈".into()),
                "rceil" => Some("⌉".into()),
                "lfloor" => Some("⌊".into()),
                "rfloor" => Some("⌋".into()),
                _ => None,
            },
            _ => None,
        }
    }

    /// 行列環境 → (左括弧, 右括弧)。未知の環境は None。
    pub fn matrix_delims(env: &str) -> Option<(&'static str, &'static str)> {
        match env {
            "matrix" => Some(("", "")),
            "pmatrix" => Some(("(", ")")),
            "bmatrix" => Some(("[", "]")),
            "Bmatrix" => Some(("{", "}")),
            "vmatrix" => Some(("|", "|")),
            "Vmatrix" => Some(("‖", "‖")),
            "smallmatrix" => Some(("", "")),
            "cases" => Some(("{", "")),
            "array" => Some(("", "")),
            "aligned" | "align" | "align*" | "gathered" => Some(("", "")),
            _ => None,
        }
    }

    impl Parser {
        fn peek(&self) -> Option<&Tok> {
            self.t.get(self.i)
        }

        /// `{abc}` か 1 つの原子を読む。
        fn group(&mut self) -> Node {
            if self.depth >= MAX_DEPTH {
                self.i = self.t.len();
                return Node::Row(Vec::new());
            }
            match self.peek() {
                Some(Tok::Open) => {
                    self.i += 1;
                    self.depth += 1;
                    let body = self.row(true);
                    self.depth -= 1;
                    if matches!(self.peek(), Some(Tok::Close)) {
                        self.i += 1;
                    }
                    body
                }
                Some(_) => {
                    self.depth += 1;
                    let n = self.atom();
                    self.depth -= 1;
                    n.unwrap_or_else(|| Node::Row(Vec::new()))
                }
                None => Node::Row(Vec::new()),
            }
        }

        /// `{}` の中身 (in_group) か全体を、上下付きまで含めて読む。
        fn row(&mut self, in_group: bool) -> Node {
            let mut out: Vec<Node> = Vec::new();
            while let Some(tok) = self.peek() {
                match tok {
                    Tok::Close if in_group => break,
                    Tok::Close => {
                        self.i += 1;
                        continue;
                    }
                    Tok::Amp | Tok::NewRow => break,
                    // 環境の終わりはセルの中身ではない
                    Tok::Cmd(c) if c == "end" => break,
                    Tok::Sup | Tok::Sub => {
                        // 底の無い上下付きは空の底に付ける
                        let base = out.pop().unwrap_or_else(|| Node::Row(Vec::new()));
                        out.push(self.scripts(base));
                    }
                    _ => {
                        if let Some(n) = self.atom() {
                            let n = if matches!(self.peek(), Some(Tok::Sup) | Some(Tok::Sub)) {
                                self.scripts(n)
                            } else {
                                n
                            };
                            out.push(n);
                        }
                    }
                }
            }
            if out.len() == 1 {
                out.pop().unwrap()
            } else {
                Node::Row(out)
            }
        }

        /// 上下付きを (順不同・重複可で) まとめて読む。
        fn scripts(&mut self, base: Node) -> Node {
            let mut sup = None;
            let mut sub = None;
            loop {
                match self.peek() {
                    Some(Tok::Sup) => {
                        self.i += 1;
                        let g = self.group();
                        sup = Some(Box::new(g));
                    }
                    Some(Tok::Sub) => {
                        self.i += 1;
                        let g = self.group();
                        sub = Some(Box::new(g));
                    }
                    _ => break,
                }
            }
            Node::Script {
                base: Box::new(base),
                sup,
                sub,
            }
        }

        /// 原子 1 個。読めないトークンは None (読み進めるだけ)。
        fn atom(&mut self) -> Option<Node> {
            let tok = self.peek()?.clone();
            match tok {
                Tok::Open => Some(self.group()),
                Tok::Close | Tok::Amp | Tok::NewRow => {
                    self.i += 1;
                    None
                }
                Tok::Sup | Tok::Sub => {
                    let base = Node::Row(Vec::new());
                    Some(self.scripts(base))
                }
                Tok::Chr(c) => {
                    self.i += 1;
                    if c == ' ' || c == '\t' || c == '\n' {
                        return None;
                    }
                    if c.is_ascii_digit() {
                        // 続く数字と小数点はまとめる
                        let mut num = c.to_string();
                        while let Some(Tok::Chr(d)) = self.peek() {
                            if d.is_ascii_digit() || (*d == '.' && !num.ends_with('.')) {
                                num.push(*d);
                                self.i += 1;
                            } else {
                                break;
                            }
                        }
                        return Some(Node::Num(num));
                    }
                    if c.is_alphabetic() {
                        return Some(Node::Ident(c.to_string()));
                    }
                    let class = match c {
                        '+' | '-' | '*' | '/' => Class::Bin,
                        '=' | '<' | '>' => Class::Rel,
                        ',' | ';' | ':' => Class::Punct,
                        '(' | '[' => Class::Open,
                        ')' | ']' => Class::Close,
                        _ => Class::Ord,
                    };
                    // 見た目の良い字へ寄せる (ASCII 代替は描画側で担保)
                    let text = match c {
                        '-' => "−".to_string(),
                        '\'' => "′".to_string(),
                        _ => c.to_string(),
                    };
                    Some(Node::Op(text, class))
                }
                Tok::Cmd(name) => {
                    self.i += 1;
                    Some(self.command(&name))
                }
            }
        }

        fn command(&mut self, name: &str) -> Node {
            if let Some((_, w)) = SPACES.iter().find(|(k, _)| *k == name) {
                return Node::Space(*w);
            }
            match name {
                "frac" | "dfrac" | "tfrac" | "binom" => {
                    let a = self.group();
                    let b = self.group();
                    if name == "binom" {
                        return Node::Delim {
                            left: "(".into(),
                            right: ")".into(),
                            body: Box::new(Node::Frac(Box::new(a), Box::new(b))),
                        };
                    }
                    Node::Frac(Box::new(a), Box::new(b))
                }
                "sqrt" => {
                    let mut index = None;
                    // `\sqrt[3]{x}` の [3]
                    if matches!(self.peek(), Some(Tok::Chr('['))) {
                        self.i += 1;
                        let mut inner = Vec::new();
                        while let Some(t) = self.peek() {
                            if matches!(t, Tok::Chr(']')) {
                                self.i += 1;
                                break;
                            }
                            match self.atom() {
                                Some(n) => inner.push(n),
                                None => {}
                            }
                        }
                        index = Some(Box::new(if inner.len() == 1 {
                            inner.pop().unwrap()
                        } else {
                            Node::Row(inner)
                        }));
                    }
                    Node::Sqrt {
                        index,
                        body: Box::new(self.group()),
                    }
                }
                "text" | "textrm" | "textbf" | "textit" | "mathrm" | "mathbf" | "mathit"
                | "mathsf" | "mathtt" | "mathbb" | "mathcal" | "mathfrak" | "bm" | "boldsymbol" => {
                    let g = self.group();
                    Node::Text(plain_text(&g))
                }
                "left" => {
                    let left = self
                        .peek()
                        .and_then(delim_str)
                        .map(|d| {
                            self.i += 1;
                            d
                        })
                        .unwrap_or_else(|| "(".into());
                    let body = self.row_until_right();
                    let right = if matches!(self.peek(), Some(Tok::Cmd(c)) if c == "right") {
                        self.i += 1;
                        self.peek()
                            .and_then(delim_str)
                            .map(|d| {
                                self.i += 1;
                                d
                            })
                            .unwrap_or_else(|| ".".into())
                    } else {
                        ".".into()
                    };
                    Node::Delim {
                        left: if left == "." { String::new() } else { left },
                        right: if right == "." { String::new() } else { right },
                        body: Box::new(body),
                    }
                }
                "right" => Node::Row(Vec::new()),
                "begin" => self.environment(),
                "end" => {
                    let _ = self.env_name();
                    Node::Row(Vec::new())
                }
                "overline" | "bar" => Node::Accent {
                    mark: Accent::Bar,
                    body: Box::new(self.group()),
                },
                "vec" => Node::Accent {
                    mark: Accent::Vec,
                    body: Box::new(self.group()),
                },
                "hat" | "widehat" => Node::Accent {
                    mark: Accent::Hat,
                    body: Box::new(self.group()),
                },
                "tilde" | "widetilde" => Node::Accent {
                    mark: Accent::Tilde,
                    body: Box::new(self.group()),
                },
                "dot" => Node::Accent {
                    mark: Accent::Dot,
                    body: Box::new(self.group()),
                },
                "operatorname" => Node::Fun(plain_text(&self.group())),
                _ => {
                    if FUNCS.contains(&name) {
                        return Node::Fun(name.to_string());
                    }
                    if let Some(sym) = symbol(name) {
                        return Node::Sym(sym);
                    }
                    Node::Raw(format!("\\{name}"))
                }
            }
        }

        /// `\right` の手前まで読む。
        fn row_until_right(&mut self) -> Node {
            let mut out = Vec::new();
            while let Some(tok) = self.peek() {
                if matches!(tok, Tok::Cmd(c) if c == "right") {
                    break;
                }
                if matches!(tok, Tok::Close) {
                    break;
                }
                if let Some(n) = self.atom() {
                    let n = if matches!(self.peek(), Some(Tok::Sup) | Some(Tok::Sub)) {
                        self.scripts(n)
                    } else {
                        n
                    };
                    out.push(n);
                }
            }
            if out.len() == 1 {
                out.pop().unwrap()
            } else {
                Node::Row(out)
            }
        }

        /// `{pmatrix}` のような環境名を読む。
        fn env_name(&mut self) -> String {
            if !matches!(self.peek(), Some(Tok::Open)) {
                return String::new();
            }
            self.i += 1;
            let mut name = String::new();
            while let Some(t) = self.peek() {
                match t {
                    Tok::Close => {
                        self.i += 1;
                        break;
                    }
                    Tok::Chr(c) => {
                        name.push(*c);
                        self.i += 1;
                    }
                    Tok::Cmd(c) => {
                        name.push_str(c);
                        self.i += 1;
                    }
                    _ => {
                        self.i += 1;
                    }
                }
            }
            name
        }

        fn environment(&mut self) -> Node {
            let env = self.env_name();
            let Some((left, right)) = matrix_delims(&env) else {
                return Node::Raw(format!("\\begin{{{env}}}"));
            };
            // array は列指定 {cc} が続く
            if env == "array" && matches!(self.peek(), Some(Tok::Open)) {
                let _ = self.env_name();
            }
            let mut rows: Vec<Vec<Node>> = vec![Vec::new()];
            let mut guard = 0usize;
            loop {
                guard += 1;
                if guard > 4096 {
                    break;
                }
                match self.peek() {
                    None => break,
                    Some(Tok::Cmd(c)) if c == "end" => {
                        self.i += 1;
                        let _ = self.env_name();
                        break;
                    }
                    Some(Tok::Amp) => {
                        self.i += 1;
                        rows.last_mut().unwrap().push(Node::Row(Vec::new()));
                    }
                    Some(Tok::NewRow) => {
                        self.i += 1;
                        rows.push(Vec::new());
                    }
                    _ => {
                        let cell = self.row(false);
                        let last = rows.last_mut().unwrap();
                        // row() は & / \\ の手前で止まる。空セルの場所を保つ
                        if last.last().map(|n| n.is_empty()).unwrap_or(false) && !cell.is_empty() {
                            *last.last_mut().unwrap() = cell;
                        } else {
                            last.push(cell);
                        }
                        if matches!(self.peek(), None) {
                            break;
                        }
                    }
                }
            }
            while rows
                .last()
                .map(|r| r.iter().all(Node::is_empty))
                .unwrap_or(false)
                && rows.len() > 1
            {
                rows.pop();
            }
            Node::Matrix {
                left: left.to_string(),
                right: right.to_string(),
                rows,
            }
        }
    }

    /// `\text{…}` の中身をただの文字列へ潰す。
    fn plain_text(n: &Node) -> String {
        match n {
            Node::Ident(s) | Node::Num(s) | Node::Text(s) | Node::Fun(s) | Node::Raw(s) => {
                s.clone()
            }
            Node::Op(s, _) => s.clone(),
            Node::Sym(s) => s.glyph.to_string(),
            Node::Row(r) => r.iter().map(plain_text).collect(),
            Node::Space(_) => " ".to_string(),
            _ => String::new(),
        }
    }

    /// TeX を構文木へ。壊れていても panic せず、未対応は Raw に落ちる。
    pub fn parse(src: &str) -> Node {
        if src.len() > MAX_LEN {
            return Node::Raw(src.chars().take(MAX_LEN).collect());
        }
        let mut p = Parser {
            t: tokenize(src),
            i: 0,
            depth: 0,
        };
        let n = p.row(false);
        // 余りが出たら (壊れた入力) 読み飛ばして続きも拾う
        if p.i < p.t.len() {
            let mut all = vec![n];
            let mut guard = 0;
            while p.i < p.t.len() && guard < 4096 {
                guard += 1;
                let before = p.i;
                let more = p.row(false);
                if !more.is_empty() {
                    all.push(more);
                }
                if p.i == before {
                    p.i += 1;
                }
            }
            return Node::Row(all);
        }
        n
    }

    // ── 版面 (配置) ─────────────────────────────────────────────────

    /// フォント計測の抽象。テストは偽の実装を挿して純関数として検査する。
    pub trait TextMetrics {
        /// 文字列の幅
        fn width(&self, text: &str, size: f32, mono: bool) -> f32;
        /// 行の高さ
        fn line_h(&self, size: f32, mono: bool) -> f32;
        /// この字がフォントに在るか (無ければ ASCII 代替へ落とす)
        fn has_glyph(&self, text: &str, mono: bool) -> bool;
    }

    /// 字面の組み方。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Style {
        /// 変数 — 斜体
        Var,
        /// 立体
        Norm,
        /// 未対応 TeX — 等幅 + 印
        Raw,
    }

    /// ベースライン上の高さ比 (egui は行の上端しか教えてくれないので比で扱う)。
    pub const ASCENT_RATIO: f32 = 0.78;

    #[derive(Debug, Clone, PartialEq)]
    pub enum Kind {
        Glyph {
            text: String,
            size: f32,
            style: Style,
        },
        /// ベースライン揃えの横並び (dx, 箱)
        Row(Vec<(f32, MBox)>),
        /// 分数 (rule_y はベースラインからの線の高さ、gap は線と分子分母の空き)
        Frac {
            num: Box<MBox>,
            den: Box<MBox>,
            rule_y: f32,
            thick: f32,
            gap: f32,
        },
        /// 根号 (lead は √ 記号の幅)
        Sqrt {
            body: Box<MBox>,
            index: Option<Box<MBox>>,
            lead: f32,
            size: f32,
        },
        /// 上下付き (dy は上へ正)
        Script {
            base: Box<MBox>,
            sup: Option<(f32, Box<MBox>)>,
            sub: Option<(f32, Box<MBox>)>,
        },
        /// 大きさを合わせた括弧
        Delim {
            left: String,
            right: String,
            body: Box<MBox>,
            size: f32,
            lw: f32,
            rw: f32,
        },
        Accent {
            body: Box<MBox>,
            mark: Accent,
            size: f32,
        },
        /// 行列 (セルは (dx, dy, 箱)。dy はベースラインからの下向き)
        Grid {
            cells: Vec<Vec<(f32, f32, MBox)>>,
            left: String,
            right: String,
            size: f32,
            lw: f32,
            rw: f32,
        },
        Space,
    }

    /// 版面上の 1 要素。w = 幅、asc/desc = ベースラインからの上下。
    #[derive(Debug, Clone, PartialEq)]
    pub struct MBox {
        pub w: f32,
        pub asc: f32,
        pub desc: f32,
        pub kind: Kind,
    }

    impl MBox {
        pub fn height(&self) -> f32 {
            self.asc + self.desc
        }
    }

    /// 上下付きの縮小率。
    fn script_size(size: f32) -> f32 {
        (size * 0.72).max(7.0)
    }

    /// 記号を「在る字」か「ASCII 代替」で返す。
    fn pick(sym: &Sym, m: &dyn TextMetrics) -> String {
        if m.has_glyph(sym.glyph, false) {
            sym.glyph.to_string()
        } else {
            sym.ascii.to_string()
        }
    }

    fn glyph_box(text: &str, size: f32, style: Style, m: &dyn TextMetrics) -> MBox {
        let mono = style == Style::Raw;
        let h = m.line_h(size, mono);
        MBox {
            w: m.width(text, size, mono),
            asc: h * ASCENT_RATIO,
            desc: h * (1.0 - ASCENT_RATIO),
            kind: Kind::Glyph {
                text: text.to_string(),
                size,
                style,
            },
        }
    }

    /// 役割ごとの前空き (em 比)。
    fn lead_gap(prev: Option<Class>, cur: Class) -> f32 {
        let Some(p) = prev else { return 0.0 };
        match (p, cur) {
            (_, Class::Rel) | (Class::Rel, _) => 0.22,
            (_, Class::Bin) | (Class::Bin, _) => 0.16,
            (Class::Punct, _) => 0.12,
            (Class::BigOp, _) => 0.1,
            _ => 0.0,
        }
    }

    /// 構文木を版面へ。座標計算はここで閉じるので純関数として検査できる。
    pub fn layout(n: &Node, size: f32, m: &dyn TextMetrics) -> MBox {
        match n {
            Node::Ident(s) => glyph_box(s, size, Style::Var, m),
            Node::Num(s) => glyph_box(s, size, Style::Norm, m),
            Node::Text(s) => glyph_box(s, size, Style::Norm, m),
            Node::Fun(s) => glyph_box(s, size, Style::Norm, m),
            Node::Sym(sym) => {
                let big = sym.class == Class::BigOp;
                let sz = if big { size * 1.35 } else { size };
                let mut b = glyph_box(&pick(sym, m), sz, Style::Norm, m);
                if big {
                    // 大型演算子はベースラインをまたいで置く
                    b.asc = b.height() * 0.72;
                    b.desc = b.height() * 0.28;
                }
                b
            }
            Node::Op(s, _) => {
                let text = if m.has_glyph(s, false) {
                    s.clone()
                } else if s == "−" {
                    "-".to_string()
                } else if s == "′" {
                    "'".to_string()
                } else {
                    s.clone()
                };
                glyph_box(&text, size, Style::Norm, m)
            }
            Node::Raw(s) => {
                let mut b = glyph_box(s, size * 0.92, Style::Raw, m);
                b.w += size * 0.2;
                b
            }
            Node::Space(w) => MBox {
                w: size * w,
                asc: 0.0,
                desc: 0.0,
                kind: Kind::Space,
            },
            Node::Row(items) => {
                let mut placed: Vec<(f32, MBox)> = Vec::with_capacity(items.len());
                let mut x = 0.0f32;
                let mut asc = 0.0f32;
                let mut desc = 0.0f32;
                let mut prev: Option<Class> = None;
                for it in items {
                    let b = layout(it, size, m);
                    let cls = it.class();
                    if !matches!(it, Node::Space(_)) {
                        x += size * lead_gap(prev, cls);
                    }
                    asc = asc.max(b.asc);
                    desc = desc.max(b.desc);
                    let w = b.w;
                    placed.push((x, b));
                    x += w;
                    prev = Some(cls);
                }
                if placed.is_empty() {
                    let h = m.line_h(size, false);
                    return MBox {
                        w: 0.0,
                        asc: h * ASCENT_RATIO,
                        desc: h * (1.0 - ASCENT_RATIO),
                        kind: Kind::Row(placed),
                    };
                }
                MBox {
                    w: x,
                    asc,
                    desc,
                    kind: Kind::Row(placed),
                }
            }
            Node::Frac(a, b) => {
                let sz = size * 0.96;
                let num = layout(a, sz, m);
                let den = layout(b, sz, m);
                let w = num.w.max(den.w) + size * 0.3;
                let rule_y = size * 0.28;
                let gap = size * 0.1;
                let thick = (size * 0.055).max(1.0);
                let asc = rule_y + num.height() + gap;
                let desc = (den.height() + gap - rule_y).max(size * 0.2);
                MBox {
                    w,
                    asc,
                    desc,
                    kind: Kind::Frac {
                        num: Box::new(num),
                        den: Box::new(den),
                        rule_y,
                        thick,
                        gap,
                    },
                }
            }
            Node::Sqrt { index, body } => {
                let inner = layout(body, size, m);
                let idx = index
                    .as_ref()
                    .map(|i| Box::new(layout(i, script_size(size) * 0.85, m)));
                let lead = size * 0.62 + idx.as_ref().map(|b| b.w * 0.8).unwrap_or(0.0);
                let asc = inner.asc + size * 0.24;
                MBox {
                    w: lead + inner.w + size * 0.14,
                    asc: asc.max(idx.as_ref().map(|b| b.height() + size * 0.3).unwrap_or(0.0)),
                    desc: inner.desc,
                    kind: Kind::Sqrt {
                        body: Box::new(inner),
                        index: idx,
                        lead,
                        size,
                    },
                }
            }
            Node::Script { base, sup, sub } => {
                let b = layout(base, size, m);
                let ss = script_size(size);
                let sup_b = sup.as_ref().map(|s| layout(s, ss, m));
                let sub_b = sub.as_ref().map(|s| layout(s, ss, m));
                let up = size * 0.44;
                let down = size * 0.24;
                let mut w = b.w;
                let mut asc = b.asc;
                let mut desc = b.desc;
                let sup_p = sup_b.map(|s| {
                    w = w.max(b.w + s.w + size * 0.06);
                    asc = asc.max(up + s.asc);
                    (up, Box::new(s))
                });
                let sub_p = sub_b.map(|s| {
                    w = w.max(b.w + s.w + size * 0.06);
                    desc = desc.max(down + s.desc);
                    (-down, Box::new(s))
                });
                MBox {
                    w,
                    asc,
                    desc,
                    kind: Kind::Script {
                        base: Box::new(b),
                        sup: sup_p,
                        sub: sub_p,
                    },
                }
            }
            Node::Delim { left, right, body } => {
                let inner = layout(body, size, m);
                // 中身の高さに合わせて括弧を伸ばす (近似: 文字サイズを拡大)
                let base_h = m.line_h(size, false).max(1.0);
                let scale = (inner.height() / base_h).clamp(1.0, 3.0);
                let dsz = size * scale;
                let lw = if left.is_empty() {
                    0.0
                } else {
                    m.width(left, dsz, false)
                };
                let rw = if right.is_empty() {
                    0.0
                } else {
                    m.width(right, dsz, false)
                };
                MBox {
                    w: lw + inner.w + rw + size * 0.1,
                    asc: inner.asc + size * 0.06,
                    desc: inner.desc + size * 0.06,
                    kind: Kind::Delim {
                        left: left.clone(),
                        right: right.clone(),
                        body: Box::new(inner),
                        size: dsz,
                        lw,
                        rw,
                    },
                }
            }
            Node::Accent { mark, body } => {
                let inner = layout(body, size, m);
                MBox {
                    w: inner.w,
                    asc: inner.asc + size * 0.22,
                    desc: inner.desc,
                    kind: Kind::Accent {
                        body: Box::new(inner),
                        mark: *mark,
                        size,
                    },
                }
            }
            Node::Matrix { left, right, rows } => {
                let sz = size * 0.95;
                let laid: Vec<Vec<MBox>> = rows
                    .iter()
                    .map(|r| r.iter().map(|c| layout(c, sz, m)).collect())
                    .collect();
                let ncol = laid.iter().map(|r| r.len()).max().unwrap_or(0);
                let mut colw = vec![0.0f32; ncol];
                for r in &laid {
                    for (c, b) in r.iter().enumerate() {
                        colw[c] = colw[c].max(b.w);
                    }
                }
                let gap_x = size * 0.7;
                let gap_y = size * 0.35;
                let total_w: f32 = colw.iter().sum::<f32>() + gap_x * ncol.saturating_sub(1) as f32;
                let row_h: Vec<f32> = laid
                    .iter()
                    .map(|r| r.iter().map(|b| b.height()).fold(size, f32::max))
                    .collect();
                let total_h: f32 =
                    row_h.iter().sum::<f32>() + gap_y * laid.len().saturating_sub(1) as f32;
                let mut cells: Vec<Vec<(f32, f32, MBox)>> = Vec::with_capacity(laid.len());
                let mut y = -total_h * 0.5;
                for (ri, r) in laid.into_iter().enumerate() {
                    let mut line = Vec::with_capacity(r.len());
                    let mut x = 0.0f32;
                    for (ci, b) in r.into_iter().enumerate() {
                        // セルは列内で中央寄せ
                        let cw = colw.get(ci).copied().unwrap_or(b.w);
                        let dx = x + (cw - b.w) * 0.5;
                        let dy = y + (row_h[ri] - b.height()) * 0.5 + b.asc;
                        line.push((dx, dy, b));
                        x += cw + gap_x;
                    }
                    y += row_h[ri] + gap_y;
                    cells.push(line);
                }
                let dsz = size * (total_h / m.line_h(size, false).max(1.0)).clamp(1.0, 4.0);
                let lw = if left.is_empty() {
                    0.0
                } else {
                    m.width(left, dsz, false)
                };
                let rw = if right.is_empty() {
                    0.0
                } else {
                    m.width(right, dsz, false)
                };
                MBox {
                    w: total_w + lw + rw + size * 0.2,
                    asc: total_h * 0.5 + size * 0.3,
                    desc: total_h * 0.5 - size * 0.1,
                    kind: Kind::Grid {
                        cells,
                        left: left.clone(),
                        right: right.clone(),
                        size: dsz,
                        lw,
                        rw,
                    },
                }
            }
        }
    }

    // ── 本文中の区切り検出 ──────────────────────────────────────────
    //
    // 通貨表記を数式にしないための規則 (README に書ける形で明文化):
    //   1. 閉じ記号は同じ行に無ければならない
    //   2. 開き `$` の直後は空白でも `$` でもない
    //   3. 閉じ `$` の直前は空白ではない
    //   4. 閉じ `$` の直後は数字ではない  ← `$5 and $10` はこれで弾かれる
    //   5. 中身が空なら数式にしない
    //   6. `\$` はエスケープなので数式を開かない (呼び出し側が先に処理する)
    //   7. コードスパン `` `…` ` / フェンス内では走査自体をしない

    /// 本文中の数式区切り。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Delim {
        /// `$…$` / `\(…\)`
        Inline,
        /// `$$…$$` / `\[…\]`
        Display,
    }

    /// 閉じ記号を探す距離の上限。毎フレーム走るので、閉じない `$` が並んだ
    /// 行で O(n^2) にならないよう頭打ちにする。
    pub const MAX_SCAN: usize = 2048;

    /// chars[i] から数式を読む。返り値は (TeX, 区切り種別, 次位置)。
    pub fn read_at(c: &[char], i: usize) -> Option<(String, Delim, usize)> {
        match c.get(i)? {
            '$' => {
                if c.get(i + 1) == Some(&'$') {
                    let close = find_seq(c, i + 2, '$', 2)?;
                    let body: String = c[i + 2..close].iter().collect();
                    if body.trim().is_empty() {
                        return None;
                    }
                    return Some((body, Delim::Display, close + 2));
                }
                let after = *c.get(i + 1)?;
                if after.is_whitespace() || after == '$' {
                    return None;
                }
                let limit = c.len().min(i + MAX_SCAN);
                let mut k = i + 1;
                while k < limit && c[k] != '$' {
                    // `\$` は数式の中でもエスケープ扱いで飛ばす
                    if c[k] == '\\' && k + 1 < limit {
                        k += 2;
                        continue;
                    }
                    k += 1;
                }
                if k >= limit || c[k] != '$' {
                    return None;
                }
                if c[k - 1].is_whitespace() {
                    return None;
                }
                if c.get(k + 1).is_some_and(|n| n.is_ascii_digit()) {
                    return None;
                }
                let body: String = c[i + 1..k].iter().collect();
                if body.trim().is_empty() {
                    return None;
                }
                Some((body, Delim::Inline, k + 1))
            }
            '\\' => match c.get(i + 1) {
                Some('(') => {
                    let close = find_pair(c, i + 2, ')')?;
                    let body: String = c[i + 2..close].iter().collect();
                    Some((body, Delim::Inline, close + 2))
                }
                Some('[') => {
                    let close = find_pair(c, i + 2, ']')?;
                    let body: String = c[i + 2..close].iter().collect();
                    Some((body, Delim::Display, close + 2))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// `ch` が n 個続く位置を探す。
    fn find_seq(c: &[char], from: usize, ch: char, n: usize) -> Option<usize> {
        let limit = c.len().min(from + MAX_SCAN);
        let mut k = from;
        while k + n <= limit {
            if c[k..k + n].iter().all(|&x| x == ch) {
                return Some(k);
            }
            k += 1;
        }
        None
    }

    /// `\)` `\]` の位置を探す。
    fn find_pair(c: &[char], from: usize, close: char) -> Option<usize> {
        let limit = c.len().min(from + MAX_SCAN);
        let mut k = from;
        while k + 1 < limit {
            if c[k] == '\\' && c[k + 1] == close {
                return Some(k);
            }
            k += 1;
        }
        None
    }

    // ── 版面キャッシュ ──────────────────────────────────────────────
    //
    // フォント構成は起動時に固定されるので、鍵は (TeX, 文字サイズ) で足りる。

    use std::cell::RefCell;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::rc::Rc;

    const CACHE_CAP: usize = 64;

    thread_local! {
        static CACHE: RefCell<Vec<(u64, Rc<MBox>)>> = const { RefCell::new(Vec::new()) };
    }

    /// 解析 + 版面をキャッシュ越しに得る (毎フレームの再計算を避ける)。
    pub fn cached_layout(tex: &str, size: f32, m: &dyn TextMetrics) -> Rc<MBox> {
        // DefaultHasher は固定鍵なので同じ入力からは同じ値になる
        let mut h = DefaultHasher::new();
        tex.hash(&mut h);
        size.to_bits().hash(&mut h);
        let key = h.finish();
        if let Some(hit) = CACHE.with(|c| {
            let mut c = c.borrow_mut();
            c.iter().position(|(k, _)| *k == key).map(|i| {
                let e = c.remove(i);
                let v = e.1.clone();
                c.push(e);
                v
            })
        }) {
            return hit;
        }
        let built = Rc::new(layout(&parse(tex), size, m));
        CACHE.with(|c| {
            let mut c = c.borrow_mut();
            c.push((key, built.clone()));
            while c.len() > CACHE_CAP {
                c.remove(0);
            }
        });
        built
    }

    // ─── テスト ─────────────────────────────────────────────────────
    //
    // 描画は painter 任せなので、字句 → 構文木 → 版面の 3 層を純関数として突く。

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 幅 = 文字数 × 0.5em の偽フォント (座標計算だけを見るため)。
        struct Fake {
            glyphs: bool,
        }

        impl TextMetrics for Fake {
            fn width(&self, text: &str, size: f32, _mono: bool) -> f32 {
                text.chars().count() as f32 * size * 0.5
            }
            fn line_h(&self, size: f32, _mono: bool) -> f32 {
                size * 1.25
            }
            fn has_glyph(&self, _text: &str, _mono: bool) -> bool {
                self.glyphs
            }
        }

        fn ok() -> Fake {
            Fake { glyphs: true }
        }

        /// 構文木を短い文字列へ潰して表で比較できるようにする。
        fn sexp(n: &Node) -> String {
            match n {
                Node::Ident(s) => format!("i:{s}"),
                Node::Num(s) => format!("n:{s}"),
                Node::Sym(s) => format!("s:{}", s.tex),
                Node::Op(s, _) => format!("o:{s}"),
                Node::Text(s) => format!("t:{s}"),
                Node::Fun(s) => format!("f:{s}"),
                Node::Raw(s) => format!("raw:{s}"),
                Node::Space(_) => "sp".into(),
                Node::Row(r) => format!("({})", r.iter().map(sexp).collect::<Vec<_>>().join(" ")),
                Node::Frac(a, b) => format!("frac[{} {}]", sexp(a), sexp(b)),
                Node::Sqrt { index, body } => match index {
                    Some(i) => format!("root[{} {}]", sexp(i), sexp(body)),
                    None => format!("sqrt[{}]", sexp(body)),
                },
                Node::Script { base, sup, sub } => {
                    let mut s = format!("scr[{}", sexp(base));
                    if let Some(u) = sup {
                        s.push_str(&format!(" ^{}", sexp(u)));
                    }
                    if let Some(d) = sub {
                        s.push_str(&format!(" _{}", sexp(d)));
                    }
                    s.push(']');
                    s
                }
                Node::Delim { left, right, body } => format!("delim[{left}{}{right}]", sexp(body)),
                Node::Accent { mark, body } => format!("acc:{mark:?}[{}]", sexp(body)),
                Node::Matrix { left, right, rows } => format!(
                    "mat[{left}{right} {}]",
                    rows.iter()
                        .map(|r| r.iter().map(sexp).collect::<Vec<_>>().join(","))
                        .collect::<Vec<_>>()
                        .join(" / ")
                ),
            }
        }

        /// 版面に現れる字面をすべて拾う (代替への差し替えを確かめる)。
        fn glyphs(b: &MBox, out: &mut Vec<String>) {
            match &b.kind {
                Kind::Glyph { text, .. } => out.push(text.clone()),
                Kind::Row(items) => items.iter().for_each(|(_, c)| glyphs(c, out)),
                Kind::Frac { num, den, .. } => {
                    glyphs(num, out);
                    glyphs(den, out);
                }
                Kind::Sqrt { body, index, .. } => {
                    if let Some(i) = index {
                        glyphs(i, out);
                    }
                    glyphs(body, out);
                }
                Kind::Script { base, sup, sub } => {
                    glyphs(base, out);
                    if let Some((_, s)) = sup {
                        glyphs(s, out);
                    }
                    if let Some((_, s)) = sub {
                        glyphs(s, out);
                    }
                }
                Kind::Delim { body, .. } => glyphs(body, out),
                Kind::Accent { body, .. } => glyphs(body, out),
                Kind::Grid { cells, .. } => {
                    for r in cells {
                        for (_, _, c) in r {
                            glyphs(c, out);
                        }
                    }
                }
                Kind::Space => {}
            }
        }

        // ---- 字句 --------------------------------------------------------

        #[test]
        fn tokenize_table() {
            assert_eq!(
                tokenize("x^2"),
                vec![Tok::Chr('x'), Tok::Sup, Tok::Chr('2')]
            );
            assert_eq!(
                tokenize("\\frac{a}{b}"),
                vec![
                    Tok::Cmd("frac".into()),
                    Tok::Open,
                    Tok::Chr('a'),
                    Tok::Close,
                    Tok::Open,
                    Tok::Chr('b'),
                    Tok::Close
                ]
            );
            // `\,` のような 1 文字命令、行列の区切り
            assert_eq!(tokenize("\\,"), vec![Tok::Cmd(",".into())]);
            assert_eq!(
                tokenize("a&b\\\\c"),
                vec![
                    Tok::Chr('a'),
                    Tok::Amp,
                    Tok::Chr('b'),
                    Tok::NewRow,
                    Tok::Chr('c')
                ]
            );
            // 末尾の裸のバックスラッシュでも落ちない
            assert_eq!(tokenize("\\"), vec![Tok::Chr('\\')]);
        }

        // ---- 構文木 ------------------------------------------------------

        #[test]
        fn parse_table() {
            let cases: &[(&str, &str)] = &[
                ("x", "i:x"),
                ("12.5", "n:12.5"),
                ("x+y", "(i:x o:+ i:y)"),
                ("x^2", "scr[i:x ^n:2]"),
                ("a_i", "scr[i:a _i:i]"),
                ("x^2_i", "scr[i:x ^n:2 _i:i]"),
                ("x^{2n}", "scr[i:x ^(n:2 i:n)]"),
                // 上付きの中の入れ子
                ("e^{x^2}", "scr[i:e ^scr[i:x ^n:2]]"),
                ("\\frac{a}{b}", "frac[i:a i:b]"),
                // \frac が ^ の中に入る
                ("e^{\\frac{1}{2}}", "scr[i:e ^frac[n:1 n:2]]"),
                ("\\frac{\\frac{a}{b}}{c}", "frac[frac[i:a i:b] i:c]"),
                ("\\sqrt{x}", "sqrt[i:x]"),
                ("\\sqrt[3]{x}", "root[n:3 i:x]"),
                ("\\sqrt[n+1]{x}", "root[(i:n o:+ n:1) i:x]"),
                ("\\alpha", "s:alpha"),
                ("\\sum_{i=1}^{n}", "scr[s:sum ^i:n _(i:i o:= n:1)]"),
                ("\\text{と書く}", "t:と書く"),
                ("\\sin x", "(f:sin i:x)"),
                ("\\lim_{x \\to 0}", "scr[f:lim _(i:x s:to n:0)]"),
                ("\\left(x\\right)", "delim[(i:x)]"),
                ("\\left[\\frac{a}{b}\\right]", "delim[[frac[i:a i:b]]]"),
                ("\\vec{v}", "acc:Vec[i:v]"),
                ("\\overline{AB}", "acc:Bar[(i:A i:B)]"),
                ("\\hat{y}", "acc:Hat[i:y]"),
                // 未知の命令は生 TeX へ
                ("\\foobar", "raw:\\foobar"),
                ("\\unknown{x}", "(raw:\\unknown i:x)"),
                ("\\begin{gantt}", "raw:\\begin{gantt}"),
            ];
            for (src, want) in cases {
                assert_eq!(sexp(&parse(src)), *want, "入力: {src}");
            }
        }

        #[test]
        fn parse_matrix_environments() {
            assert_eq!(
                sexp(&parse("\\begin{pmatrix}a&b\\\\c&d\\end{pmatrix}")),
                "mat[() i:a,i:b / i:c,i:d]"
            );
            assert_eq!(
                sexp(&parse("\\begin{bmatrix}1\\\\2\\end{bmatrix}")),
                "mat[[] n:1 / n:2]"
            );
            // 未知の環境は生 TeX に落ちる (空白にはならない)
            assert!(sexp(&parse("\\begin{tikzpicture}x\\end{tikzpicture}")).contains("raw:"));
        }

        #[test]
        fn parse_is_total_on_broken_input() {
            let junk = [
                "\\frac{a",
                "}}}}",
                "^^^^",
                "x_{_{_{_{",
                "\\left(",
                "\\right)",
                "\\sqrt[",
                "\\begin{pmatrix}",
                "\\end{pmatrix}",
                "$",
                "\\\\\\\\\\\\",
                "{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{a}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}",
            ];
            for src in junk {
                let n = parse(src);
                let b = layout(&n, 14.0, &ok());
                assert!(
                    b.w.is_finite() && b.height().is_finite(),
                    "壊れた寸法: {src}"
                );
            }
        }

        #[test]
        fn deep_nesting_is_capped_not_crashed() {
            let src = "{".repeat(200) + "x" + &"}".repeat(200);
            let b = layout(&parse(&src), 14.0, &ok());
            assert!(b.w.is_finite());
        }

        #[test]
        fn long_input_is_truncated_not_dropped() {
            let src = "x+".repeat(MAX_LEN);
            let n = parse(&src);
            assert!(matches!(n, Node::Raw(_)), "上限超過は生表示へ落とす");
        }

        // ---- 記号表 ------------------------------------------------------

        #[test]
        fn symbol_table_is_well_formed() {
            for s in SYMBOLS {
                assert!(!s.tex.is_empty(), "TeX 名が空");
                assert!(!s.glyph.is_empty(), "{} の字が空", s.tex);
                assert!(!s.ascii.is_empty(), "{} の代替が空", s.tex);
                assert!(
                    s.ascii.is_ascii(),
                    "{} の代替は ASCII で書く (代替まで豆腐になる): {}",
                    s.tex,
                    s.ascii
                );
            }
            let mut names: Vec<&str> = SYMBOLS.iter().map(|s| s.tex).collect();
            let total = names.len();
            names.sort_unstable();
            names.dedup();
            assert_eq!(
                total,
                names.len(),
                "SYMBOLS に重複した TeX 名がある (後ろが死ぬ)"
            );
        }

        #[test]
        fn every_symbol_parses_back_to_itself() {
            for s in SYMBOLS {
                let src = format!("\\{}", s.tex);
                assert_eq!(sexp(&parse(&src)), format!("s:{}", s.tex), "{src}");
            }
        }

        #[test]
        fn missing_glyphs_fall_back_to_ascii() {
            let src: String = SYMBOLS.iter().map(|s| format!("\\{} ", s.tex)).collect();
            let mut with = Vec::new();
            glyphs(
                &layout(&parse(&src), 14.0, &Fake { glyphs: true }),
                &mut with,
            );
            let mut without = Vec::new();
            glyphs(
                &layout(&parse(&src), 14.0, &Fake { glyphs: false }),
                &mut without,
            );
            assert_eq!(with.len(), SYMBOLS.len());
            assert_eq!(without.len(), SYMBOLS.len());
            for (i, s) in SYMBOLS.iter().enumerate() {
                assert_eq!(with[i], s.glyph, "{} の字", s.tex);
                assert_eq!(without[i], s.ascii, "{} の代替", s.tex);
            }
        }

        #[test]
        fn function_names_are_upright() {
            for f in FUNCS {
                if *f == "operatorname" {
                    continue;
                }
                assert_eq!(sexp(&parse(&format!("\\{f}"))), format!("f:{f}"));
            }
        }

        // ---- 版面 --------------------------------------------------------

        #[test]
        fn layout_sizes_are_sane() {
            let m = ok();
            let x = layout(&parse("x"), 14.0, &m);
            assert!(x.w > 0.0 && x.asc > 0.0 && x.desc > 0.0);

            // 分数は分子・分母より広く、高さは両方ぶん
            let f = layout(&parse("\\frac{abc}{d}"), 14.0, &m);
            let num = layout(&parse("abc"), 14.0 * 0.96, &m);
            assert!(f.w > num.w, "分数の幅が分子以下: {} <= {}", f.w, num.w);
            assert!(f.height() > num.height() * 2.0 * 0.9);

            // 上付きは箱を上へ伸ばす
            let plain = layout(&parse("x"), 14.0, &m);
            let sup = layout(&parse("x^2"), 14.0, &m);
            assert!(sup.asc > plain.asc, "上付きで上に伸びていない");
            assert!(sup.w > plain.w);
            let sub = layout(&parse("x_2"), 14.0, &m);
            assert!(sub.desc > plain.desc, "下付きで下に伸びていない");

            // 根号は中身より広い (根の記号ぶん)
            let s = layout(&parse("\\sqrt{x}"), 14.0, &m);
            assert!(s.w > plain.w);
            let s3 = layout(&parse("\\sqrt[3]{x}"), 14.0, &m);
            assert!(s3.w > s.w, "指数付きの根号がもっと広くならない");

            // \left…\right は中身の高さで括弧が伸びる
            let d1 = layout(&parse("\\left(x\\right)"), 14.0, &m);
            let d2 = layout(&parse("\\left(\\frac{a}{b}\\right)"), 14.0, &m);
            match (&d1.kind, &d2.kind) {
                (Kind::Delim { size: a, .. }, Kind::Delim { size: b, .. }) => {
                    assert!(b > a, "中身が高いのに括弧が伸びていない: {a} → {b}")
                }
                other => panic!("Delim にならない: {other:?}"),
            }
        }

        #[test]
        fn script_size_shrinks_but_stays_readable() {
            assert!(script_size(14.0) < 14.0);
            assert!(script_size(4.0) >= 7.0, "小さすぎる字は作らない");
        }

        #[test]
        fn row_inserts_spacing_around_relations() {
            let m = ok();
            let tight = layout(&parse("xy"), 14.0, &m);
            let rel = layout(&parse("x=y"), 14.0, &m);
            let eq = layout(&parse("="), 14.0, &m);
            assert!(rel.w > tight.w + eq.w, "関係子の前後に空きが入っていない");
        }

        #[test]
        fn layout_is_deterministic() {
            let m = ok();
            let src = "\\sum_{i=1}^{n} \\frac{\\sqrt[3]{x_i^2}}{\\alpha + \\beta} \\to \\infty";
            assert_eq!(layout(&parse(src), 14.0, &m), layout(&parse(src), 14.0, &m));
        }

        #[test]
        fn matrix_cells_are_laid_out_in_a_grid() {
            let b = layout(
                &parse("\\begin{pmatrix}a&b\\\\c&d\\end{pmatrix}"),
                14.0,
                &ok(),
            );
            let Kind::Grid {
                cells, left, right, ..
            } = &b.kind
            else {
                panic!("Grid にならない: {:?}", b.kind);
            };
            assert_eq!((left.as_str(), right.as_str()), ("(", ")"));
            assert_eq!(cells.len(), 2);
            assert_eq!(cells[0].len(), 2);
            // 同じ行は同じ高さ、同じ列は同じ位置
            assert_eq!(cells[0][0].1, cells[0][1].1);
            assert_eq!(cells[0][0].0, cells[1][0].0);
            assert!(cells[0][1].0 > cells[0][0].0, "2 列目が右に無い");
            assert!(cells[1][0].1 > cells[0][0].1, "2 行目が下に無い");
        }

        // ---- 本文中の区切り ------------------------------------------------

        fn read(src: &str) -> Option<(String, Delim, usize)> {
            let c: Vec<char> = src.chars().collect();
            read_at(&c, 0)
        }

        #[test]
        fn delimiter_table() {
            assert_eq!(read("$x^2$"), Some(("x^2".into(), Delim::Inline, 5)));
            assert_eq!(read("$$a$$"), Some(("a".into(), Delim::Display, 5)));
            assert_eq!(read("\\(x\\)"), Some(("x".into(), Delim::Inline, 5)));
            assert_eq!(read("\\[x\\]"), Some(("x".into(), Delim::Display, 5)));
            // 後ろに本文が続いても位置が正しい
            let c: Vec<char> = "$a$ と $b$".chars().collect();
            assert_eq!(read_at(&c, 0), Some(("a".into(), Delim::Inline, 3)));
            assert_eq!(read_at(&c, 6), Some(("b".into(), Delim::Inline, 9)));
        }

        /// 通貨表記を数式にしない規則を固定する。
        #[test]
        fn currency_and_prose_do_not_become_math() {
            // 閉じの直後が数字 → 数式にしない ($5 and $10)
            assert_eq!(read("$5 and $10"), None);
            // 開きの直後が空白
            assert_eq!(read("$ x$"), None);
            // 閉じの直前が空白
            assert_eq!(read("$x $"), None);
            // 同じ行に閉じが無い
            assert_eq!(read("$x"), None);
            // 中身が空
            assert_eq!(read("$$"), None);
            assert_eq!(read("$$$$"), None);
            // 単独の $
            assert_eq!(read("$"), None);
            // 金額の並びも素通し
            assert_eq!(read("$100"), None);
        }

        #[test]
        fn escaped_dollar_inside_math_does_not_close_it() {
            assert_eq!(read("$a\\$b$"), Some(("a\\$b".into(), Delim::Inline, 6)));
        }

        #[test]
        fn unclosed_paren_forms_are_not_math() {
            assert_eq!(read("\\(x"), None);
            assert_eq!(read("\\[x"), None);
            assert_eq!(read("\\alpha"), None);
        }
    }
}

// ─── 描画 ───────────────────────────────────────────────────────────

/// スパン描画に必要な文脈 (画像の基準ディレクトリとテクスチャキャッシュ)。
pub struct RenderCtx<'a> {
    /// 相対パス画像の基準 (通常はバッファのあるディレクトリ)
    pub dir: Option<&'a Path>,
    pub images: &'a mut ImageCache,
}

/// インラインスパン列をその場に描く (呼び出し側が wrap コンテナを用意する)。
fn spans_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    text: &str,
    size: f32,
    strong_all: bool,
    color: Color32,
    rctx: &mut RenderCtx,
) {
    for sp in parse_inline(text) {
        if sp.math {
            let sz = if sp.math_display { size * 1.1 } else { size };
            math_ui(ui, theme, &sp.text, sz, color);
            continue;
        }
        if sp.fnote {
            // 脚注参照は本文より小さく、アクセント色で
            ui.label(
                RichText::new(&sp.text)
                    .size(size * 0.78)
                    .color(theme.accent),
            );
            continue;
        }
        if let Some(url) = &sp.link {
            // 画像は「実体を描く / 説明付きプレースホルダ」のどちらかに必ず落とす
            if sp.image {
                image_ui(ui, theme, size, url, &sp.text, rctx);
                continue;
            }
            ui.hyperlink_to(RichText::new(&sp.text).size(size), url)
                .on_hover_text(url);
            continue;
        }
        let mut rt = if sp.code {
            RichText::new(&sp.text)
                .font(FontId::monospace(size * 0.92))
                .color(theme.accent)
                .background_color(theme.panel_alt)
        } else {
            RichText::new(&sp.text).size(size).color(color)
        };
        if sp.strong || strong_all {
            rt = rt.strong();
        }
        if sp.em {
            rt = rt.italics();
        }
        if sp.strike {
            rt = rt.strikethrough();
        }
        ui.label(rt);
    }
}

/// 画像スパン 1 個を描く。実体を出せないときは必ず説明付きの
/// プレースホルダに落とし、生の URL やマークアップを晒さない。
fn image_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    size: f32,
    url: &str,
    label: &str,
    rctx: &mut RenderCtx,
) {
    let alt = label.trim_start_matches("🖼 ");
    let draw = |ui: &mut egui::Ui, tex: &egui::TextureHandle| {
        let avail = ui.available_width().max(60.0);
        ui.add(egui::Image::new(tex).max_width(avail.min(tex.size_vec2().x)))
            .on_hover_text(if alt.is_empty() { url } else { alt });
    };
    // data: URI は毎フレーム base64 を解き直さないよう、URL そのものを鍵に引く
    let is_data = url.len() > 5 && url[..5].eq_ignore_ascii_case("data:");
    if is_data {
        if let Some(hit) = rctx.images.cached(url) {
            match hit {
                Some(tex) => draw(ui, &tex),
                None => {
                    let src = ImageSrc::Missing(url.to_string());
                    ui.label(
                        RichText::new(image_placeholder_text(&src, alt))
                            .size(size * 0.95)
                            .color(theme.warn),
                    );
                }
            }
            return;
        }
    }
    let src = classify_image(rctx.dir, url);
    let tex = match &src {
        ImageSrc::Local(p) => rctx.images.get(ui.ctx(), p),
        ImageSrc::Data { bytes, .. } => rctx.images.get_bytes(ui.ctx(), url, bytes),
        _ => None,
    };
    if let Some(tex) = tex {
        draw(ui, &tex);
        return;
    }
    // デコードできなかった data: URI も「試した」ことを記録して再挑戦を防ぐ
    if is_data && rctx.images.cached(url).is_none() {
        rctx.images.get_bytes(ui.ctx(), url, &[]);
    }
    let text = image_placeholder_text(&src, alt);
    let color = if matches!(src, ImageSrc::Remote(_)) {
        theme.text_dim
    } else {
        theme.warn
    };
    let resp = ui.label(RichText::new(text).size(size * 0.95).color(color));
    // リモート/欠損とも、元の URL は hover とクリックで辿れるようにする
    if matches!(src, ImageSrc::Remote(_)) {
        resp.on_hover_text(url);
        ui.label(RichText::new(" ").size(size));
        ui.hyperlink_to(RichText::new(tr("開く")).size(size * 0.9), url);
    } else {
        resp.on_hover_text(url);
    }
}

/// 1行を折り返しつきで描く。
fn line_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    text: &str,
    size: f32,
    strong_all: bool,
    color: Color32,
    rctx: &mut RenderCtx,
) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        spans_ui(ui, theme, text, size, strong_all, color, rctx);
    });
}

/// セル内容を折り返しなしで描いたときの幅を見積もる (中央/右揃えの余白計算用)。
/// spans_ui は item_spacing.x = 0 で描くためスパン幅の総和と一致する。
fn spans_width(ui: &egui::Ui, text: &str, size: f32) -> f32 {
    ui.fonts(|f| {
        parse_inline(text)
            .iter()
            .map(|sp| {
                let font = if sp.code {
                    FontId::monospace(size * 0.92)
                } else {
                    FontId::proportional(size)
                };
                f.layout_no_wrap(sp.text.clone(), font, Color32::WHITE)
                    .size()
                    .x
            })
            .sum()
    })
}

/// テーブルセルの書式ひとまとめ (table_cell_ui の引数構造化用。値の器のみで計算はしない)。
#[derive(Clone, Copy)]
struct CellStyle {
    size: f32,
    strong: bool,
    color: Color32,
    align: TableAlign,
}

/// テーブルの1セルを揃え付きで描く。
/// 中央/右揃えの列はセル幅いっぱいを確保して余白で寄せる
/// (egui::Grid のセルは常に左詰めのため、揃えはセル内で自前で行う)。
fn table_cell_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    text: &str,
    style: CellStyle,
    rctx: &mut RenderCtx,
) {
    let CellStyle {
        size,
        strong: strong_all,
        color,
        align,
    } = style;
    if align == TableAlign::Left {
        line_ui(ui, theme, text, size, strong_all, color, rctx);
        return;
    }
    let w = ui.available_width();
    let pad = (w - spans_width(ui, text, size)).max(0.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.set_min_width(w);
        ui.add_space(if align == TableAlign::Right {
            pad
        } else {
            pad * 0.5
        });
        spans_ui(ui, theme, text, size, strong_all, color, rctx);
    });
}

/// フェンスコードブロック (syntect でハイライト、横スクロール)。
fn code_block_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    hl: &Highlighter,
    base: f32,
    idx: usize,
    lang_tok: &str,
    code: &str,
) {
    let lang = hl.lang_for_fence(lang_tok);
    let job = hl.layout_job(
        code.trim_end_matches('\n'),
        &lang,
        &theme.syntect_theme,
        FontId::monospace(base * 0.92),
        theme.term_fg,
    );
    egui::Frame::none()
        .fill(theme.term_bg)
        .rounding(egui::Rounding::same(6.0))
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            if !lang_tok.is_empty() {
                ui.label(RichText::new(lang_tok).size(10.5).color(theme.text_dim));
            }
            egui::ScrollArea::horizontal()
                .id_salt(("md-code", idx))
                .show(ui, |ui| {
                    ui.label(job);
                });
        });
}

// ─── 図と数式の描画 ─────────────────────────────────────────────────
//
// 解析と配置は mermaid / math モジュールの純関数に閉じている。ここは
// 「出来上がった座標を egui の painter に流すだけ」に徹する。

/// egui のフォントを使った計測 (math::TextMetrics の実装)。
/// `ui.fonts(|f| …)` の中でだけ生きる借用なので、レイアウトもその中で行う。
struct FontMetrics<'a> {
    fonts: &'a egui::epaint::Fonts,
}

impl math::TextMetrics for FontMetrics<'_> {
    fn width(&self, text: &str, size: f32, mono: bool) -> f32 {
        let fid = if mono {
            FontId::monospace(size)
        } else {
            FontId::proportional(size)
        };
        self.fonts
            .layout_no_wrap(text.to_string(), fid, Color32::WHITE)
            .size()
            .x
    }

    fn line_h(&self, size: f32, mono: bool) -> f32 {
        let fid = if mono {
            FontId::monospace(size)
        } else {
            FontId::proportional(size)
        };
        self.fonts.row_height(&fid)
    }

    fn has_glyph(&self, text: &str, mono: bool) -> bool {
        let fid = if mono {
            FontId::monospace(12.0)
        } else {
            FontId::proportional(12.0)
        };
        self.fonts.has_glyphs(&fid, text)
    }
}

/// 数式の 1 文字塊をガレーにする (変数は斜体で組む)。
fn math_galley(
    ui: &egui::Ui,
    text: &str,
    size: f32,
    style: math::Style,
    color: Color32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::default();
    let font = if style == math::Style::Raw {
        FontId::monospace(size)
    } else {
        FontId::proportional(size)
    };
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: font,
            color,
            italics: style == math::Style::Var,
            ..Default::default()
        },
    );
    ui.fonts(|f| f.layout_job(job))
}

/// 版面を描く。`base` はベースラインの左端。
fn draw_math(ui: &egui::Ui, theme: &Theme, b: &math::MBox, base: egui::Pos2, color: Color32) {
    use math::Kind;
    let p = ui.painter();
    match &b.kind {
        Kind::Space => {}
        Kind::Glyph { text, size, style } => {
            let col = if *style == math::Style::Raw {
                theme.warn
            } else {
                color
            };
            let g = math_galley(ui, text, *size, *style, col);
            p.galley(egui::pos2(base.x, base.y - b.asc), g, col);
            if *style == math::Style::Raw {
                // 未対応 TeX は下線で「そのまま出している」ことを示す
                let y = base.y + b.desc * 0.4;
                p.line_segment(
                    [egui::pos2(base.x, y), egui::pos2(base.x + b.w, y)],
                    egui::Stroke::new(1.0_f32, theme.warn.linear_multiply(0.6)),
                );
            }
        }
        Kind::Row(items) => {
            for (dx, child) in items {
                draw_math(ui, theme, child, egui::pos2(base.x + dx, base.y), color);
            }
        }
        Kind::Frac {
            num,
            den,
            rule_y,
            thick,
            gap,
        } => {
            let cx = base.x + b.w * 0.5;
            let y = base.y - rule_y;
            p.line_segment(
                [
                    egui::pos2(base.x + b.w * 0.06, y),
                    egui::pos2(base.x + b.w * 0.94, y),
                ],
                egui::Stroke::new(*thick, color),
            );
            draw_math(
                ui,
                theme,
                num,
                egui::pos2(cx - num.w * 0.5, y - gap - num.desc),
                color,
            );
            draw_math(
                ui,
                theme,
                den,
                egui::pos2(cx - den.w * 0.5, y + gap + den.asc),
                color,
            );
        }
        Kind::Sqrt {
            body,
            index,
            lead,
            size,
        } => {
            let idx_w = index.as_ref().map(|i| i.w * 0.8).unwrap_or(0.0);
            let top = base.y - b.asc + size * 0.06;
            let bot = base.y + body.desc;
            let x0 = base.x + idx_w;
            let hook = (lead - idx_w).max(size * 0.3);
            let stroke = egui::Stroke::new((size * 0.06).max(1.0), color);
            p.line_segment(
                [
                    egui::pos2(x0 + hook * 0.05, base.y - body.asc * 0.35),
                    egui::pos2(x0 + hook * 0.35, bot),
                ],
                stroke,
            );
            p.line_segment(
                [
                    egui::pos2(x0 + hook * 0.35, bot),
                    egui::pos2(x0 + hook * 0.8, top),
                ],
                stroke,
            );
            p.line_segment(
                [
                    egui::pos2(x0 + hook * 0.8, top),
                    egui::pos2(base.x + b.w, top),
                ],
                stroke,
            );
            if let Some(idx) = index {
                draw_math(
                    ui,
                    theme,
                    idx,
                    egui::pos2(base.x, top + idx.asc + size * 0.1),
                    color,
                );
            }
            draw_math(ui, theme, body, egui::pos2(base.x + lead, base.y), color);
        }
        Kind::Script { base: bb, sup, sub } => {
            draw_math(ui, theme, bb, base, color);
            let x = base.x + bb.w;
            if let Some((dy, s)) = sup {
                draw_math(ui, theme, s, egui::pos2(x, base.y - dy), color);
            }
            if let Some((dy, s)) = sub {
                draw_math(ui, theme, s, egui::pos2(x, base.y - dy), color);
            }
        }
        Kind::Delim {
            left,
            right,
            body,
            size,
            lw,
            rw,
        } => {
            let cy = base.y - b.asc + b.height() * 0.5;
            if !left.is_empty() {
                let g = math_galley(ui, left, *size, math::Style::Norm, color);
                let h = g.size().y;
                p.galley(egui::pos2(base.x, cy - h * 0.5), g, color);
            }
            draw_math(ui, theme, body, egui::pos2(base.x + lw, base.y), color);
            if !right.is_empty() {
                let g = math_galley(ui, right, *size, math::Style::Norm, color);
                let h = g.size().y;
                p.galley(egui::pos2(base.x + b.w - rw, cy - h * 0.5), g, color);
            }
        }
        Kind::Accent { body, mark, size } => {
            draw_math(ui, theme, body, base, color);
            let y = base.y - body.asc - size * 0.06;
            let (x0, x1) = (base.x + b.w * 0.1, base.x + b.w * 0.9);
            let stroke = egui::Stroke::new((size * 0.06).max(1.0), color);
            match mark {
                math::Accent::Bar => {
                    p.line_segment([egui::pos2(x0, y), egui::pos2(x1, y)], stroke);
                }
                math::Accent::Vec => {
                    p.line_segment([egui::pos2(x0, y), egui::pos2(x1, y)], stroke);
                    let h = size * 0.12;
                    p.line_segment([egui::pos2(x1 - h, y - h), egui::pos2(x1, y)], stroke);
                    p.line_segment([egui::pos2(x1 - h, y + h), egui::pos2(x1, y)], stroke);
                }
                math::Accent::Hat => {
                    let cx = (x0 + x1) * 0.5;
                    p.line_segment([egui::pos2(x0, y), egui::pos2(cx, y - size * 0.16)], stroke);
                    p.line_segment([egui::pos2(cx, y - size * 0.16), egui::pos2(x1, y)], stroke);
                }
                math::Accent::Tilde => {
                    let g = math_galley(ui, "~", *size * 0.9, math::Style::Norm, color);
                    let w = g.size().x;
                    p.galley(
                        egui::pos2(base.x + (b.w - w) * 0.5, y - size * 0.62),
                        g,
                        color,
                    );
                }
                math::Accent::Dot => {
                    p.circle_filled(
                        egui::pos2((x0 + x1) * 0.5, y - size * 0.05),
                        (size * 0.06).max(1.0),
                        color,
                    );
                }
            }
        }
        Kind::Grid {
            cells,
            left,
            right,
            size,
            lw,
            rw,
        } => {
            let cy = base.y - b.asc + b.height() * 0.5;
            if !left.is_empty() {
                let g = math_galley(ui, left, *size, math::Style::Norm, color);
                let h = g.size().y;
                p.galley(egui::pos2(base.x, cy - h * 0.5), g, color);
            }
            for row in cells {
                for (dx, dy, cell) in row {
                    draw_math(
                        ui,
                        theme,
                        cell,
                        egui::pos2(base.x + lw + dx, base.y + dy),
                        color,
                    );
                }
            }
            if !right.is_empty() {
                let g = math_galley(ui, right, *size, math::Style::Norm, color);
                let h = g.size().y;
                p.galley(egui::pos2(base.x + b.w - rw, cy - h * 0.5), g, color);
            }
        }
    }
}

/// 数式 1 個をその場に描く (行内・別行立ての両方)。
/// 生 TeX は hover で読めるようにしておく。
fn math_ui(ui: &mut egui::Ui, theme: &Theme, tex: &str, size: f32, color: Color32) {
    let mb = ui.fonts(|f| math::cached_layout(tex, size, &FontMetrics { fonts: f }));
    let w = mb.w.max(2.0);
    let h = mb.height().max(size);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let base = egui::pos2(rect.left(), rect.top() + mb.asc);
        draw_math(ui, theme, &mb, base, color);
    }
    resp.on_hover_text(tex);
}

/// 別行立ての数式ブロック (中央寄せ、本文より少し大きく)。
fn display_math_ui(ui: &mut egui::Ui, theme: &Theme, base: f32, tex: &str) {
    let size = base * 1.15;
    let mb = ui.fonts(|f| math::cached_layout(tex, size, &FontMetrics { fonts: f }));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let pad = ((ui.available_width() - mb.w) * 0.5).max(0.0);
        ui.add_space(pad);
        math_ui(ui, theme, tex, size, theme.text);
    });
    ui.add_space(4.0);
}

/// 行頭から始まる別行立て数式を読む。読めたら (TeX, 消費行数)。
/// `$$ … $$` と `\[ … \]` の両方、1 行形と複数行形に対応する。
fn display_math_block(lines: &[&str], i: usize) -> Option<(String, usize)> {
    let t = lines.get(i)?.trim();
    for (open, close) in [("$$", "$$"), ("\\[", "\\]")] {
        let Some(rest) = t.strip_prefix(open) else {
            continue;
        };
        // 1 行で閉じている形
        if let Some(body) = rest.strip_suffix(close) {
            if !body.trim().is_empty() {
                return Some((body.trim().to_string(), 1));
            }
        }
        if !rest.trim().is_empty() && rest.contains(close) {
            continue;
        }
        // 複数行形: 閉じ記号のある行まで
        let mut body = if rest.trim().is_empty() {
            String::new()
        } else {
            format!("{}\n", rest.trim())
        };
        let mut k = i + 1;
        while k < lines.len() {
            let l = lines[k];
            if let Some(head) = l.trim().strip_suffix(close) {
                body.push_str(head);
                let tex = body.trim().to_string();
                if tex.is_empty() {
                    return None;
                }
                return Some((tex, k - i + 1));
            }
            body.push_str(l);
            body.push('\n');
            k += 1;
            // 閉じないまま延々と食べない
            if k - i > 60 {
                return None;
            }
        }
        return None;
    }
    None
}

// ─── mermaid 図の描画 ───────────────────────────────────────────────

/// フォント計測から図の寸法を決める (ズームやテーマ変更に追従する)。
fn mermaid_metrics(ui: &egui::Ui, base: f32) -> mermaid::Metrics {
    let size = base * 0.9;
    let fid = FontId::proportional(size);
    let (char_w, line_h) = ui.fonts(|f| (f.glyph_width(&fid, 'M'), f.row_height(&fid)));
    mermaid::Metrics {
        char_w: char_w.max(4.0),
        line_h: line_h.max(10.0),
        pad_x: size * 0.7,
        pad_y: size * 0.42,
        gap_rank: size * 3.2,
        gap_cross: size * 1.5,
        margin: size * 0.9,
    }
}

/// 矢印の先端を塗る。
fn arrow_head(p: &egui::Painter, tip: egui::Pos2, from: egui::Pos2, color: Color32, w: f32) {
    let d = tip - from;
    if d.length_sq() < 1e-6 {
        return;
    }
    let dir = d.normalized();
    let n = egui::vec2(-dir.y, dir.x);
    let back = tip - dir * (w * 2.2);
    p.add(egui::Shape::convex_polygon(
        vec![tip, back + n * w, back - n * w],
        color,
        egui::Stroke::NONE,
    ));
}

/// 線種に応じて折れ線を引く。
fn poly_line(p: &egui::Painter, pts: &[egui::Pos2], line: mermaid::Line, color: Color32) {
    let width: f32 = match line {
        mermaid::Line::Thick => 2.6,
        _ => 1.4,
    };
    let stroke = egui::Stroke::new(width, color);
    for w in pts.windows(2) {
        if line == mermaid::Line::Dotted {
            p.extend(egui::Shape::dashed_line(&[w[0], w[1]], stroke, 5.0, 4.0));
        } else {
            p.line_segment([w[0], w[1]], stroke);
        }
    }
}

/// ノード 1 個の枠を形どおりに描く。
fn node_shape(
    p: &egui::Painter,
    r: egui::Rect,
    shape: mermaid::Shape,
    fill: Color32,
    stroke: egui::Stroke,
) {
    use mermaid::Shape as S;
    let (cx, cy) = (r.center().x, r.center().y);
    match shape {
        S::Rect | S::Subroutine | S::Cylinder => {
            p.rect_filled(r, egui::Rounding::same(3.0), fill);
            p.rect_stroke(r, egui::Rounding::same(3.0), stroke);
            if shape == S::Subroutine {
                let d = 7.0;
                for x in [r.left() + d, r.right() - d] {
                    p.line_segment([egui::pos2(x, r.top()), egui::pos2(x, r.bottom())], stroke);
                }
            }
            if shape == S::Cylinder {
                let d = 6.0;
                p.line_segment(
                    [
                        egui::pos2(r.left(), r.top() + d),
                        egui::pos2(r.right(), r.top() + d),
                    ],
                    stroke,
                );
            }
        }
        S::Round => {
            p.rect_filled(r, egui::Rounding::same(10.0), fill);
            p.rect_stroke(r, egui::Rounding::same(10.0), stroke);
        }
        S::Stadium => {
            let rad = r.height() * 0.5;
            p.rect_filled(r, egui::Rounding::same(rad), fill);
            p.rect_stroke(r, egui::Rounding::same(rad), stroke);
        }
        S::Circle => {
            let rad = r.width().min(r.height()) * 0.5;
            p.circle_filled(r.center(), rad, fill);
            p.circle_stroke(r.center(), rad, stroke);
        }
        S::Diamond => {
            let pts = vec![
                egui::pos2(cx, r.top()),
                egui::pos2(r.right(), cy),
                egui::pos2(cx, r.bottom()),
                egui::pos2(r.left(), cy),
            ];
            p.add(egui::Shape::convex_polygon(pts, fill, stroke));
        }
        S::Hexagon => {
            let d = r.width() * 0.16;
            let pts = vec![
                egui::pos2(r.left() + d, r.top()),
                egui::pos2(r.right() - d, r.top()),
                egui::pos2(r.right(), cy),
                egui::pos2(r.right() - d, r.bottom()),
                egui::pos2(r.left() + d, r.bottom()),
                egui::pos2(r.left(), cy),
            ];
            p.add(egui::Shape::convex_polygon(pts, fill, stroke));
        }
        S::Asymmetric => {
            let d = r.width() * 0.16;
            let pts = vec![
                egui::pos2(r.left(), r.top()),
                egui::pos2(r.right(), r.top()),
                egui::pos2(r.right(), r.bottom()),
                egui::pos2(r.left(), r.bottom()),
                egui::pos2(r.left() + d, cy),
            ];
            p.add(egui::Shape::convex_polygon(pts, fill, stroke));
        }
    }
}

/// 折返し済みラベルを矩形の中央へ描く。
fn centered_lines(
    p: &egui::Painter,
    r: egui::Rect,
    lines: &[String],
    size: f32,
    color: Color32,
    line_h: f32,
) {
    let total = lines.len() as f32 * line_h;
    let mut y = r.center().y - total * 0.5 + line_h * 0.5;
    for l in lines {
        p.text(
            egui::pos2(r.center().x, y),
            egui::Align2::CENTER_CENTER,
            l,
            FontId::proportional(size),
            color,
        );
        y += line_h;
    }
}

/// フローチャートを描く。
fn flow_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    lay: &mermaid::FlowLayout,
    size: f32,
    m: mermaid::Metrics,
) {
    let (resp, p) = ui.allocate_painter(lay.size, egui::Sense::hover());
    let o = resp.rect.min.to_vec2();
    let shift = |r: egui::Rect| r.translate(o);
    // サブグラフの枠を先に (背面へ)
    for (title, r) in &lay.groups {
        let rr = shift(*r);
        p.rect_filled(
            rr,
            egui::Rounding::same(6.0),
            theme.panel.linear_multiply(0.6),
        );
        p.rect_stroke(
            rr,
            egui::Rounding::same(6.0),
            egui::Stroke::new(1.0_f32, theme.border),
        );
        if !title.is_empty() {
            p.text(
                egui::pos2(rr.left() + 6.0, rr.top() + 2.0),
                egui::Align2::LEFT_TOP,
                title,
                FontId::proportional(size * 0.85),
                theme.text_dim,
            );
        }
    }
    // 辺
    for e in &lay.edges {
        let pts: Vec<egui::Pos2> = e.points.iter().map(|q| *q + o).collect();
        poly_line(&p, &pts, e.line, theme.text_dim);
        if e.arrow_to && pts.len() >= 2 {
            arrow_head(
                &p,
                pts[pts.len() - 1],
                pts[pts.len() - 2],
                theme.text_dim,
                4.5,
            );
        }
        if e.arrow_from && pts.len() >= 2 {
            arrow_head(&p, pts[0], pts[1], theme.text_dim, 4.5);
        }
        if !e.label.is_empty() {
            let at = e.label_at + o;
            let g = ui.fonts(|f| {
                f.layout_no_wrap(
                    e.label.clone(),
                    FontId::proportional(size * 0.82),
                    theme.text,
                )
            });
            let r = egui::Rect::from_center_size(at, g.size() + egui::vec2(6.0, 2.0));
            p.rect_filled(r, egui::Rounding::same(3.0), theme.bg);
            p.galley(r.center() - g.size() * 0.5, g, theme.text);
        }
    }
    // ノード
    for b in &lay.boxes {
        let r = shift(b.rect);
        node_shape(
            &p,
            r,
            b.shape,
            theme.panel_alt,
            egui::Stroke::new(1.4_f32, theme.accent),
        );
        centered_lines(&p, r, &b.lines, size, theme.text, m.line_h);
    }
}

/// シーケンス図を描く。
fn seq_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    lay: &mermaid::SeqLayout,
    size: f32,
    m: mermaid::Metrics,
) {
    let (resp, p) = ui.allocate_painter(lay.size, egui::Sense::hover());
    let o = resp.rect.min.to_vec2();
    if let Some(t) = &lay.title {
        p.text(
            egui::pos2(resp.rect.center().x, resp.rect.top() + m.margin * 0.5),
            egui::Align2::CENTER_TOP,
            t,
            FontId::proportional(size * 1.1),
            theme.text,
        );
    }
    // 生存線
    for (a, b) in &lay.lifelines {
        p.extend(egui::Shape::dashed_line(
            &[*a + o, *b + o],
            egui::Stroke::new(1.0_f32, theme.border),
            4.0,
            4.0,
        ));
    }
    // 活性化バー
    for r in &lay.activations {
        let rr = r.translate(o);
        p.rect_filled(rr, egui::Rounding::same(2.0), theme.accent_soft);
        p.rect_stroke(
            rr,
            egui::Rounding::same(2.0),
            egui::Stroke::new(1.0_f32, theme.accent),
        );
    }
    // 参加者
    for c in &lay.cols {
        let r = c.head.translate(o);
        p.rect_filled(r, egui::Rounding::same(4.0), theme.panel_alt);
        p.rect_stroke(
            r,
            egui::Rounding::same(4.0),
            egui::Stroke::new(1.4_f32, theme.accent),
        );
        centered_lines(&p, r, &c.label, size, theme.text, m.line_h);
    }
    // ノート
    for n in &lay.notes {
        let r = n.rect.translate(o);
        p.rect_filled(r, egui::Rounding::same(3.0), theme.panel);
        p.rect_stroke(
            r,
            egui::Rounding::same(3.0),
            egui::Stroke::new(1.0_f32, theme.warn),
        );
        centered_lines(&p, r, &n.lines, size * 0.9, theme.text_dim, m.line_h);
    }
    // メッセージ
    for a in &lay.arrows {
        let (from, to) = (a.from + o, a.to + o);
        let line = if a.dashed {
            mermaid::Line::Dotted
        } else {
            mermaid::Line::Solid
        };
        let pts = if a.loopback {
            vec![from, egui::pos2(to.x, from.y), to, egui::pos2(from.x, to.y)]
        } else {
            vec![from, to]
        };
        poly_line(&p, &pts, line, theme.text_dim);
        let tip = *pts.last().unwrap();
        let prev = pts[pts.len() - 2];
        match a.arrow {
            mermaid::SeqArrow::Open => {}
            mermaid::SeqArrow::Cross => {
                let d = 4.0;
                let st = egui::Stroke::new(1.6_f32, theme.err);
                p.line_segment([tip + egui::vec2(-d, -d), tip + egui::vec2(d, d)], st);
                p.line_segment([tip + egui::vec2(-d, d), tip + egui::vec2(d, -d)], st);
            }
            mermaid::SeqArrow::Async => {
                let st = egui::Stroke::new(1.4_f32, theme.text_dim);
                let d = (tip - prev).normalized();
                let n = egui::vec2(-d.y, d.x);
                p.line_segment([tip, tip - d * 6.0 + n * 5.0], st);
                p.line_segment([tip, tip - d * 6.0 - n * 5.0], st);
            }
            mermaid::SeqArrow::Solid => arrow_head(&p, tip, prev, theme.text_dim, 4.5),
        }
        if !a.text.is_empty() {
            let mid = if a.loopback {
                egui::pos2(from.x.max(to.x) + 8.0, (from.y + to.y) * 0.5)
            } else {
                egui::pos2((from.x + to.x) * 0.5, from.y - m.line_h * 0.62)
            };
            let anchor = if a.loopback {
                egui::Align2::LEFT_CENTER
            } else {
                egui::Align2::CENTER_CENTER
            };
            p.text(
                mid,
                anchor,
                &a.text,
                FontId::proportional(size * 0.85),
                theme.text,
            );
        }
    }
}

/// ```mermaid フェンス 1 個を描く。
/// 描けない図種・壊れた入力は「注記 + ソースコードブロック」へ必ず落ちる。
fn mermaid_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    hl: &Highlighter,
    base: f32,
    idx: usize,
    src: &str,
) {
    let m = mermaid_metrics(ui, base);
    let prep = mermaid::cached(src, m);
    let size = base * 0.9;
    let notice = |ui: &mut egui::Ui, text: &str| {
        ui.label(RichText::new(text).size(base * 0.88).color(theme.warn));
    };
    match &*prep {
        mermaid::Prepared::Fallback(note) => {
            notice(ui, note);
            code_block_ui(ui, theme, hl, base, idx, "mermaid", src);
        }
        mermaid::Prepared::Flow(lay) => {
            if let Some(n) = &lay.notice {
                notice(ui, n);
            }
            egui::ScrollArea::horizontal()
                .id_salt(("md-mermaid", idx))
                .show(ui, |ui| flow_ui(ui, theme, lay, size, m));
        }
        mermaid::Prepared::Seq(lay) => {
            if let Some(n) = &lay.notice {
                notice(ui, n);
            }
            egui::ScrollArea::horizontal()
                .id_salt(("md-mermaid", idx))
                .show(ui, |ui| seq_ui(ui, theme, lay, size, m));
        }
    }
}

/// Markdown 全文を ui へ描画する。
/// `rctx` は画像解決用の文脈 (基準ディレクトリ + テクスチャキャッシュ)。
pub fn render(
    ui: &mut egui::Ui,
    theme: &Theme,
    hl: &Highlighter,
    base: f32,
    text: &str,
    rctx: &mut RenderCtx,
) {
    ui.spacing_mut().item_spacing.y = 6.0;
    // 巨大文書でも UI を止めないよう、描く量に上限を設ける
    let total = text.len();
    let (text, truncated) = cap_input(text);
    if truncated {
        ui.label(
            RichText::new(truncation_note(text.len(), total))
                .size(base * 0.9)
                .color(theme.warn),
        );
        ui.separator();
    }
    // 先頭の YAML / TOML フロントマターは水平線+ゴミにせず畳んで見せる
    let fm_lang = if text.trim_start_matches('\u{feff}').starts_with("+++") {
        "toml"
    } else {
        "yaml"
    };
    let (front, text) = split_front_matter(text);
    if let Some(fm) = front {
        front_matter_ui(ui, theme, hl, base, fm, fm_lang);
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut para = String::new();
    let flush_para = |ui: &mut egui::Ui, para: &mut String, theme: &Theme, rctx: &mut RenderCtx| {
        if !para.trim_end().is_empty() {
            line_ui(ui, theme, para.trim_end(), base, false, theme.text, rctx);
        }
        para.clear();
    };

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();

        // フェンスコード (``` と ~~~ の両方)
        if let Some((f, lang_tok)) = fence_open(trimmed) {
            flush_para(ui, &mut para, theme, rctx);
            let start = i + 1;
            let mut end = start;
            while end < lines.len() && !lines[end].trim_start().starts_with(f) {
                end += 1;
            }
            let code: String = lines[start..end].iter().flat_map(|l| [*l, "\n"]).collect();
            if lang_tok.eq_ignore_ascii_case("mermaid") {
                mermaid_ui(ui, theme, hl, base, i, &code);
            } else {
                code_block_ui(ui, theme, hl, base, i, &lang_tok, &code);
            }
            i = (end + 1).min(lines.len());
            continue;
        }

        // 別行立ての数式 (`$$ … $$` / `\[ … \]`)
        if trimmed.starts_with("$$") || trimmed.starts_with("\\[") {
            if let Some((tex, used)) = display_math_block(&lines, i) {
                flush_para(ui, &mut para, theme, rctx);
                display_math_ui(ui, theme, base, &tex);
                i += used;
                continue;
            }
        }

        // 見出し
        if let Some((level, body)) = atx_heading(trimmed) {
            flush_para(ui, &mut para, theme, rctx);
            let scale = [1.85f32, 1.5, 1.28, 1.12, 1.02, 0.95][level - 1];
            ui.add_space(if level <= 2 { 8.0 } else { 4.0 });
            line_ui(ui, theme, &body, base * scale, true, theme.text, rctx);
            if level <= 2 {
                ui.separator();
            }
            i += 1;
            continue;
        }

        // 水平線
        if is_hr(trimmed) {
            flush_para(ui, &mut para, theme, rctx);
            ui.separator();
            i += 1;
            continue;
        }

        // 引用 (連続する > 行をまとめる。`>>` の入れ子は深さぶん帯を重ねる)
        if trimmed.starts_with('>') {
            flush_para(ui, &mut para, theme, rctx);
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                let (depth, body) = quote_depth(lines[i].trim_start());
                let bar: String = std::iter::repeat_n("▍", depth.max(1)).collect();
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.label(
                        RichText::new(format!("{bar} "))
                            .color(theme.accent)
                            .size(base),
                    );
                    spans_ui(ui, theme, body, base, false, theme.text_dim, rctx);
                });
                i += 1;
            }
            continue;
        }

        // 脚注定義 `[^1]: 本文`
        if let Some((label, body)) = footnote_def(trimmed) {
            flush_para(ui, &mut para, theme, rctx);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.label(
                    RichText::new(format!("[{label}] "))
                        .size(base * 0.78)
                        .color(theme.accent),
                );
                spans_ui(ui, theme, &body, base * 0.92, false, theme.text_dim, rctx);
            });
            i += 1;
            continue;
        }

        // テーブル
        if trimmed.starts_with('|') && lines.get(i + 1).map(|l| is_table_sep(l)).unwrap_or(false) {
            flush_para(ui, &mut para, theme, rctx);
            let header = split_row(trimmed);
            let aligns = table_aligns(lines[i + 1]);
            let ncols = header.len().max(1);
            let table_id = i;
            let mut r = i + 2;
            let mut rows: Vec<Vec<String>> = Vec::new();
            while r < lines.len() && lines[r].trim_start().starts_with('|') {
                let lt = lines[r].trim_start();
                // 迷い込んだ区切り行 (`|---|` 等) はセルとして描画しない
                if !is_table_sep(lt) {
                    let mut row = split_row(lt);
                    // GFM 準拠: ヘッダより多いセルは切り捨て、足りない分は空セルで埋める
                    row.resize(ncols, String::new());
                    rows.push(row);
                }
                r += 1;
            }
            // 列幅の上限は全列均等割り (最低 80px)。egui::Grid は上限が有限のときだけ
            // セル内折り返しが有効になる。収まらない分は横スクロールで逃がす。
            let cap = ((ui.available_width() - 34.0 - 16.0 * (ncols - 1) as f32) / ncols as f32)
                .max(80.0);
            let col_align = |c: usize| aligns.get(c).copied().unwrap_or(TableAlign::Left);
            egui::ScrollArea::horizontal()
                .id_salt(("md-table-scroll", table_id))
                .show(ui, |ui| {
                    egui::Frame::none()
                        .stroke(egui::Stroke::new(1.0_f32, theme.border))
                        .rounding(egui::Rounding::same(4.0))
                        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                        .show(ui, |ui| {
                            egui::Grid::new(("md-table", table_id))
                                .num_columns(ncols)
                                .max_col_width(cap)
                                .striped(true)
                                .spacing([16.0, 5.0])
                                .show(ui, |ui| {
                                    for (c, cell) in header.iter().enumerate() {
                                        let style = CellStyle {
                                            size: base,
                                            strong: true,
                                            color: theme.text,
                                            align: col_align(c),
                                        };
                                        table_cell_ui(ui, theme, cell, style, rctx);
                                    }
                                    ui.end_row();
                                    for row in &rows {
                                        for (c, cell) in row.iter().enumerate() {
                                            let style = CellStyle {
                                                size: base,
                                                strong: false,
                                                color: theme.text,
                                                align: col_align(c),
                                            };
                                            table_cell_ui(ui, theme, cell, style, rctx);
                                        }
                                        ui.end_row();
                                    }
                                });
                        });
                });
            i = r;
            continue;
        }

        // リスト
        if let Some((off, bullet)) = list_marker(trimmed) {
            flush_para(ui, &mut para, theme, rctx);
            let done = bullet == "☑";
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add_space(6.0 + indent as f32 * base * 0.55);
                let bcol = if done { theme.ok } else { theme.accent };
                ui.label(RichText::new(format!("{bullet} ")).color(bcol).size(base));
                let tcol = if done { theme.text_dim } else { theme.text };
                spans_ui(ui, theme, &trimmed[off..], base, false, tcol, rctx);
            });
            i += 1;
            continue;
        }

        // 空行 = 段落の区切り
        if trimmed.is_empty() {
            flush_para(ui, &mut para, theme, rctx);
            i += 1;
            continue;
        }

        // Setext 見出し (次行が === / --- のとき、この行が見出しになる)
        if para.is_empty() {
            if let Some(level) = lines.get(i + 1).and_then(|l| setext_level(l)) {
                let scale = if level == 1 { 1.85 } else { 1.5 };
                ui.add_space(8.0);
                line_ui(ui, theme, trimmed, base * scale, true, theme.text, rctx);
                ui.separator();
                i += 2;
                continue;
            }
        }

        // 通常テキスト → 段落として連結。
        // 行末スペース2つ / 行末バックスラッシュ (Markdown のハード改行、
        // <br> 由来も同じ) は段落を確定する
        let (brk, body) = hard_break(line);
        append_para(&mut para, body);
        if brk {
            flush_para(ui, &mut para, theme, rctx);
        }
        i += 1;
    }
    flush_para(ui, &mut para, theme, rctx);
}

/// フロントマターを畳んだブロックとして描く (既定は閉じた状態)。
fn front_matter_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    hl: &Highlighter,
    base: f32,
    fm: &str,
    lang: &str,
) {
    egui::CollapsingHeader::new(
        RichText::new(tr("📄 フロントマター"))
            .size(base * 0.9)
            .color(theme.text_dim),
    )
    .id_salt("md-front-matter")
    .default_open(false)
    .show(ui, |ui| {
        code_block_ui(ui, theme, hl, base, usize::MAX, lang, fm);
    });
}

// ─── テスト ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧 `resolve_image` 相当 (ローカル実ファイルに解決できたときだけ Some)。
    fn resolve_image(dir: Option<&Path>, url: &str) -> Option<PathBuf> {
        match classify_image(dir, url) {
            ImageSrc::Local(p) => Some(p),
            _ => None,
        }
    }

    /// スパン列を「(種別, 本文)」の並びへ落として比較しやすくする。
    fn kinds(text: &str) -> Vec<(&'static str, String)> {
        parse_inline(text)
            .into_iter()
            .map(|s| {
                let kind = if s.math {
                    if s.math_display {
                        "math$$"
                    } else {
                        "math"
                    }
                } else if s.image {
                    "image"
                } else if s.fnote {
                    "footnote"
                } else if s.link.is_some() {
                    "link"
                } else if s.code {
                    "code"
                } else if s.strong {
                    "strong"
                } else if s.em {
                    "em"
                } else if s.strike {
                    "strike"
                } else {
                    "text"
                };
                (kind, s.text)
            })
            .collect()
    }

    /// 図と数式を含む文書を実際に描いてみて、描画経路が落ちないことを見る。
    /// (解析と配置は純関数側で検査済み。ここは painter 呼び出しの通し確認)
    #[test]
    fn render_smoke_covers_diagrams_and_math() {
        let doc = "\
# 見出し

流れ図:

```mermaid
graph TD
  A[開始] --> B{分岐}
  B -->|はい| C(処理)
  B -.->|いいえ| D((終了))
  subgraph g[まとめ]
  C --> E[[結果]]
  end
  D --> E
```

順序図:

```mermaid
sequenceDiagram
  participant A as 利用者
  participant B as サーバ
  A->>B: 要求
  activate B
  B-->>A: 応答
  deactivate B
  Note over A,B: ここまで
  A->>A: 自分へ
```

未対応の図:

```mermaid
gantt
  title 予定
```

行内の $x^2 + \\sqrt[3]{y}$ と \\(\\alpha \\to \\beta\\)、通貨の $5 and $10。

$$
\\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a} = \\sum_{i=1}^{n} \\begin{pmatrix}1 & 0\\\\0 & 1\\end{pmatrix}
$$

\\[ \\lim_{x \\to \\infty} \\frac{1}{x} = 0 \\]

未対応の命令 $\\thisisnotreal{x}$ も本文を壊さない。
";
        let ctx = egui::Context::default();
        let hl = Highlighter::new();
        let theme = crate::theme::by_name("zaivern-dark");
        let mut images = ImageCache::default();
        // 2 フレーム回してキャッシュ経路 (2 回目は再計算しない) も通す
        for _ in 0..2 {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let mut rctx = RenderCtx {
                        dir: None,
                        images: &mut images,
                    };
                    render(ui, &theme, &hl, 14.0, doc, &mut rctx);
                });
            });
        }
    }

    // ─── 数式と mermaid の本文側 (区切り検出・退避先・字の有無) ────────

    /// `$` を数式にする条件を本文の側から固定する。
    #[test]
    fn inline_math_delimiters() {
        assert_eq!(
            kinds("式は $x^2$ です"),
            [
                ("text", "式は ".into()),
                ("math", "x^2".into()),
                ("text", " です".into()),
            ]
        );
        assert_eq!(kinds("$$a+b$$"), [("math$$", "a+b".into())]);
        assert_eq!(kinds("\\(x\\)"), [("math", "x".into())]);
        assert_eq!(kinds("\\[x\\]"), [("math$$", "x".into())]);
        // 1 行に 2 つ
        assert_eq!(
            kinds("$a$ と $b$"),
            [
                ("math", "a".into()),
                ("text", " と ".into()),
                ("math", "b".into()),
            ]
        );
    }

    /// 通貨・エスケープ・コードスパンは数式にしない (誤爆すると本文が消える)。
    #[test]
    fn dollars_that_must_not_become_math() {
        assert_eq!(
            kinds("$5 and $10 です"),
            [("text", "$5 and $10 です".into())]
        );
        assert_eq!(kinds("値段は $100"), [("text", "値段は $100".into())]);
        // `\$` はエスケープ (前の `\` が消えてただの $ になる)
        assert_eq!(kinds("\\$x\\$"), [("text", "$x$".into())]);
        // コードスパンの中の $ は素通し
        assert_eq!(
            kinds("`$x$` は式ではない"),
            [("code", "$x$".into()), ("text", " は式ではない".into()),]
        );
        // 開きの直後が空白 / 閉じの直前が空白
        assert_eq!(kinds("$ x$"), [("text", "$ x$".into())]);
        assert_eq!(kinds("$x $"), [("text", "$x $".into())]);
        // 閉じが無い
        assert_eq!(kinds("$x のまま"), [("text", "$x のまま".into())]);
    }

    /// 別行立ての数式ブロック (1 行形と複数行形の両方)。
    #[test]
    fn display_math_block_forms() {
        let one = ["$$E = mc^2$$"];
        assert_eq!(display_math_block(&one, 0), Some(("E = mc^2".into(), 1)));

        let multi = ["$$", "\\frac{a}{b}", "+ c", "$$", "続き"];
        assert_eq!(
            display_math_block(&multi, 0),
            Some(("\\frac{a}{b}\n+ c".into(), 4))
        );

        let bracket = ["\\[", "x^2", "\\]"];
        assert_eq!(display_math_block(&bracket, 0), Some(("x^2".into(), 3)));
        assert_eq!(display_math_block(&["\\[x\\]"], 0), Some(("x".into(), 1)));

        // 開いたまま閉じない / 空の式は本文へ戻す
        assert_eq!(display_math_block(&["$$", "x"], 0), None);
        assert_eq!(display_math_block(&["$$", "$$"], 0), None);
        assert_eq!(display_math_block(&["ふつうの行"], 0), None);
        // 60 行を超えても暴走しない
        let mut long: Vec<&str> = vec!["$$"];
        long.extend(std::iter::repeat_n("x", 200));
        assert_eq!(display_math_block(&long, 0), None);
    }

    #[test]
    fn mermaid_fence_is_recognised_by_token() {
        for open in ["```mermaid", "``` mermaid", "~~~mermaid", "```Mermaid"] {
            let (_, tok) = fence_open(open).expect(open);
            assert!(tok.eq_ignore_ascii_case("mermaid"), "{open} → {tok}");
        }
        // 他の言語は従来どおりコードブロック
        assert_eq!(fence_open("```rust").map(|(_, t)| t), Some("rust".into()));
    }

    /// 数式で使う記号が、同梱フォントだけの環境でも豆腐 (□) にならないこと。
    ///
    /// egui 同梱の Ubuntu-Light / Hack には数学記号がまばらにしか無く、
    /// `→` や `⊕` はそのままでは描けない。描画側は「字が在るか」を毎回確かめ、
    /// 無ければ SYMBOLS の ascii 列へ落とす。ここではその落とし先まで含めて
    /// 全記号が必ず描けることを見る (アプリが積む追加フォントに頼らない)。
    #[test]
    fn math_symbols_have_glyphs_or_ascii_fallback() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        let missing: Vec<String> = ctx.fonts(|f| {
            use math::TextMetrics as _;
            let m = FontMetrics { fonts: f };
            math::SYMBOLS
                .iter()
                .filter_map(|s| {
                    let shown = if m.has_glyph(s.glyph, false) {
                        s.glyph
                    } else {
                        s.ascii
                    };
                    (!m.has_glyph(shown, false)).then(|| format!("\\{} → {shown}", s.tex))
                })
                .collect()
        });
        assert!(
            missing.is_empty(),
            "同梱フォントで豆腐になる数式記号がある: {missing:?}"
        );
    }

    /// 実フォント計測でも版面が壊れないこと (幅・高さが有限で正)。
    #[test]
    fn math_layout_with_real_fonts_is_finite() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        let cases = [
            "x^2 + y^2 = z^2",
            "\\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}",
            "\\sum_{i=1}^{n} i = \\frac{n(n+1)}{2}",
            "\\begin{pmatrix}1 & 0\\\\0 & 1\\end{pmatrix}",
            "\\lim_{x \\to \\infty} \\frac{1}{x} = 0",
            "\\text{日本語のテキスト}",
            "\\undefinedmacro{x}",
        ];
        ctx.fonts(|f| {
            let m = FontMetrics { fonts: f };
            for tex in cases {
                let b = math::layout(&math::parse(tex), 14.0, &m);
                assert!(b.w.is_finite() && b.w > 0.0, "幅が壊れた: {tex} → {}", b.w);
                assert!(
                    b.height().is_finite() && b.height() > 0.0,
                    "高さが壊れた: {tex}"
                );
            }
        });
    }

    #[test]
    fn detects_markdown_files() {
        assert!(is_markdown("README.md", "Plain Text"));
        assert!(is_markdown("Notes.MD", "Plain Text"));
        assert!(is_markdown("x.markdown", "Plain Text"));
        assert!(is_markdown("untitled-1", "Markdown"));
        assert!(!is_markdown("main.rs", "Rust"));
    }

    #[test]
    fn inline_bold_and_code() {
        let sp = parse_inline("a **b** `c`");
        assert_eq!(sp.len(), 4);
        assert_eq!(sp[0].text, "a ");
        assert!(sp[1].strong && sp[1].text == "b");
        assert_eq!(sp[2].text, " ");
        assert!(sp[3].code && sp[3].text == "c");
    }

    #[test]
    fn inline_link_and_image() {
        let sp = parse_inline("see [doc](https://a.b) ![alt](img.png)");
        assert_eq!(sp[1].link.as_deref(), Some("https://a.b"));
        assert_eq!(sp[1].text, "doc");
        assert_eq!(sp[3].link.as_deref(), Some("img.png"));
        assert!(sp[3].text.contains("alt"));
    }

    #[test]
    fn inline_snake_case_is_not_emphasis() {
        let sp = parse_inline("use snake_case_name here");
        assert_eq!(sp.len(), 1);
        assert!(sp[0].text.contains("snake_case_name"));
        assert!(!sp[0].em);
    }

    #[test]
    fn inline_escape_keeps_literal() {
        let sp = parse_inline(r"\*not em\*");
        assert_eq!(sp.len(), 1);
        assert_eq!(sp[0].text, "*not em*");
    }

    #[test]
    fn block_helpers() {
        assert!(is_hr("---"));
        assert!(is_hr("* * *"));
        assert!(!is_hr("--"));
        assert!(!is_hr("a---"));
        assert!(is_table_sep("| --- | :--: |"));
        assert!(!is_table_sep("| a | b |"));
        assert_eq!(split_row("| a | b |"), vec!["a", "b"]);
    }

    #[test]
    fn table_row_split_edge_cases() {
        // エスケープされたパイプはセルを割らず本文の `|` になる
        assert_eq!(split_row(r"| a \| b | c |"), vec!["a | b", "c"]);
        // 空セルは保持される (先頭・途中・末尾)
        assert_eq!(split_row("|| b |"), vec!["", "b"]);
        assert_eq!(split_row("| a || c |"), vec!["a", "", "c"]);
        assert_eq!(split_row("| a | |"), vec!["a", ""]);
        // 閉じパイプなしでも同じ結果
        assert_eq!(split_row("| a | b"), vec!["a", "b"]);
        assert_eq!(split_row("a | b"), vec!["a", "b"]);
    }

    #[test]
    fn table_alignment_parse() {
        use TableAlign::*;
        assert_eq!(
            table_aligns("|:--|:-:|--:|---|"),
            vec![Left, Center, Right, Left]
        );
        assert_eq!(table_aligns("| :--: | --- |"), vec![Center, Left]);
        assert_eq!(list_marker("- x"), Some((2, "•".into())));
        assert_eq!(list_marker("3. x"), Some((3, "3.".into())));
        assert_eq!(list_marker("- [x] done"), Some((6, "☑".into())));
        assert_eq!(list_marker("普通の行"), None);
    }

    #[test]
    fn paragraph_join_is_cjk_aware() {
        let mut p = String::new();
        append_para(&mut p, "hello");
        append_para(&mut p, "world");
        assert_eq!(p, "hello world");
        let mut q = String::new();
        append_para(&mut q, "こんにちは");
        append_para(&mut q, "世界");
        assert_eq!(q, "こんにちは世界");
    }

    #[test]
    fn resolve_image_remote_and_data_urls_are_none() {
        let dir = crate::test_util::unique_temp_dir("zaivern-markdown-test", "remote");
        // dir があってもリモート/データ URL はローカル解決しない
        assert_eq!(resolve_image(Some(&dir), "http://a.b/img.png"), None);
        assert_eq!(resolve_image(Some(&dir), "https://a.b/img.png"), None);
        assert_eq!(
            resolve_image(Some(&dir), "data:image/png;base64,AAAA"),
            None
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_image_empty_url_is_none() {
        let dir = crate::test_util::unique_temp_dir("zaivern-markdown-test", "empty");
        assert_eq!(resolve_image(Some(&dir), ""), None);
        // クエリ/フラグメントだけの URL も除去後に空になり None
        assert_eq!(resolve_image(Some(&dir), "?q=1"), None);
        assert_eq!(resolve_image(Some(&dir), "#frag"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_image_strips_query_and_fragment() {
        let dir = crate::test_util::unique_temp_dir("zaivern-markdown-test", "query");
        let img = dir.join("img.png");
        std::fs::write(&img, b"png").expect("write test image");
        assert_eq!(resolve_image(Some(&dir), "img.png?v=1"), Some(img.clone()));
        assert_eq!(resolve_image(Some(&dir), "img.png#sec"), Some(img.clone()));
        assert_eq!(resolve_image(Some(&dir), "img.png?v=1#sec"), Some(img));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_image_absolute_path_ignores_dir() {
        let dir = crate::test_util::unique_temp_dir("zaivern-markdown-test", "abs");
        let img = dir.join("abs.png");
        std::fs::write(&img, b"png").expect("write test image");
        let url = img.to_str().expect("utf-8 temp path");
        // 絶対パスは dir と無関係に解決される (dir が別でも None でも同じ)
        let other = crate::test_util::unique_temp_dir("zaivern-markdown-test", "abs-other");
        assert_eq!(resolve_image(Some(&other), url), Some(img.clone()));
        assert_eq!(resolve_image(None, url), Some(img));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn resolve_image_relative_needs_dir_and_existing_file() {
        let dir = crate::test_util::unique_temp_dir("zaivern-markdown-test", "rel");
        let img = dir.join("rel.png");
        std::fs::write(&img, b"png").expect("write test image");
        // dir がなければ相対パスは解決できない
        assert_eq!(resolve_image(None, "rel.png"), None);
        // dir 起点で実在すれば Some、しなければ None
        assert_eq!(resolve_image(Some(&dir), "rel.png"), Some(img));
        assert_eq!(resolve_image(Some(&dir), "missing.png"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_image_directory_is_not_a_file() {
        let dir = crate::test_util::unique_temp_dir("zaivern-markdown-test", "dirpath");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("create sub dir");
        // is_file 判定なのでディレクトリは None
        assert_eq!(resolve_image(Some(&dir), "sub"), None);
        let abs = sub.to_str().expect("utf-8 temp path");
        assert_eq!(resolve_image(None, abs), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ─── 画像ソースの分類 ───────────────────────────────────────────

    #[test]
    fn classify_image_table() {
        let dir = crate::test_util::unique_temp_dir("zaivern-markdown-test", "classify");
        std::fs::write(dir.join("a.png"), b"png").expect("write");
        // 日本語 + 空白入りのファイル名 (パーセントエンコード有無の両方)
        std::fs::write(dir.join("図 1.png"), b"png").expect("write");
        let abs = dir.join("a.png");
        let abs_s = abs.to_str().expect("utf-8");
        let file_url = format!("file://{}", abs.display());

        let cases: Vec<(&str, ImageSrc)> = vec![
            ("a.png", ImageSrc::Local(abs.clone())),
            ("./a.png", ImageSrc::Local(dir.join("./a.png"))),
            (abs_s, ImageSrc::Local(abs.clone())),
            (&file_url, ImageSrc::Local(abs.clone())),
            ("図 1.png", ImageSrc::Local(dir.join("図 1.png"))),
            ("%E5%9B%B3%201.png", ImageSrc::Local(dir.join("図 1.png"))),
            (
                "https://example.com/x.png",
                ImageSrc::Remote("https://example.com/x.png".into()),
            ),
            (
                "http://example.com/x.png",
                ImageSrc::Remote("http://example.com/x.png".into()),
            ),
            ("ftp://h/x.png", ImageSrc::Remote("ftp://h/x.png".into())),
            ("missing.png", ImageSrc::Missing("missing.png".into())),
            ("", ImageSrc::Missing("".into())),
        ];
        for (url, want) in cases {
            assert_eq!(classify_image(Some(&dir), url), want, "url={url:?}");
        }
        // 相対パスは基準ディレクトリが無ければ解決しない (プロセス cwd に依存しない)
        assert_eq!(
            classify_image(None, "a.png"),
            ImageSrc::Missing("a.png".into())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn classify_image_data_uri() {
        // 1x1 GIF (base64)
        let gif = "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
        match classify_image(None, gif) {
            ImageSrc::Data { mime, bytes } => {
                assert_eq!(mime, "image/gif");
                assert_eq!(&bytes[..3], b"GIF");
            }
            other => panic!("expected Data, got {other:?}"),
        }
        // base64 でない data: URI (パーセントエンコード)
        match classify_image(None, "data:image/svg+xml,%3Csvg%3E%3C/svg%3E") {
            ImageSrc::Data { mime, bytes } => {
                assert_eq!(mime, "image/svg+xml");
                assert_eq!(String::from_utf8_lossy(&bytes), "<svg></svg>");
            }
            other => panic!("expected Data, got {other:?}"),
        }
        // 壊れた data: URI は Missing (panic しない)
        assert!(matches!(
            classify_image(None, "data:image/png;base64,"),
            ImageSrc::Missing(_)
        ));
        assert!(matches!(
            classify_image(None, "data:nocomma"),
            ImageSrc::Missing(_)
        ));
        assert!(matches!(
            classify_image(None, "data:image/png;base64,***"),
            ImageSrc::Missing(_)
        ));
    }

    #[test]
    fn base64_and_percent_decoding() {
        assert_eq!(base64_decode("aGVsbG8="), Some(b"hello".to_vec()));
        // 改行入り (メールや HTML で折り返された data URI)
        assert_eq!(base64_decode("aGVs\nbG8="), Some(b"hello".to_vec()));
        // URL セーフ表現
        assert_eq!(base64_decode("-_8="), Some(vec![0xfb, 0xff]));
        assert_eq!(base64_decode("!!!"), None);
        assert_eq!(percent_decode("%E3%81%82%20b"), "あ b");
        // 不正なシーケンスは温存する
        assert_eq!(percent_decode("100%_done.png"), "100%_done.png");
        assert_eq!(percent_decode("no-escape"), "no-escape");
    }

    #[test]
    fn file_url_forms() {
        assert_eq!(
            file_url_to_path("file:///tmp/a%20b.png"),
            Some(PathBuf::from("/tmp/a b.png"))
        );
        assert_eq!(
            file_url_to_path("file://localhost/tmp/x.png"),
            Some(PathBuf::from("/tmp/x.png"))
        );
        // Windows のドライブ表記は先頭の / を落とす
        assert_eq!(
            file_url_to_path("file:///C:/tmp/x.png"),
            Some(PathBuf::from("C:/tmp/x.png"))
        );
        assert_eq!(file_url_to_path("https://a.b/x.png"), None);
        assert_eq!(file_url_to_path("file://"), None);
    }

    #[test]
    fn image_placeholder_wording() {
        let remote = ImageSrc::Remote("https://a.b/x.png".into());
        assert!(image_placeholder_text(&remote, "").contains("リモート画像"));
        assert!(image_placeholder_text(&remote, "図").contains("図"));
        let missing = ImageSrc::Missing("x.png".into());
        assert!(image_placeholder_text(&missing, "").contains("表示できません"));
    }

    // ─── インライン構文 ─────────────────────────────────────────────

    #[test]
    fn inline_code_can_contain_backticks() {
        let sp = kinds("``a ` b``");
        assert_eq!(sp, vec![("code", "a ` b".to_string())]);
        // 前後 1 個の空白だけ剥がす
        assert_eq!(kinds("`` `x` ``"), vec![("code", "`x`".to_string())]);
        // 閉じないバッククォートは文字として残す
        assert_eq!(kinds("`open"), vec![("text", "`open".to_string())]);
    }

    #[test]
    fn autolinks_are_linked() {
        let sp = parse_inline("見て <https://a.b/c> ね");
        assert_eq!(sp[1].link.as_deref(), Some("https://a.b/c"));
        let sp = parse_inline("<a@b.co>");
        assert_eq!(sp[0].link.as_deref(), Some("mailto:a@b.co"));
        assert_eq!(sp[0].text, "a@b.co");
        // 素の URL も拾う。文末の句読点は含めない
        let sp = parse_inline("詳細は https://a.b/c。");
        assert_eq!(sp[1].link.as_deref(), Some("https://a.b/c"));
        assert_eq!(sp[2].text, "。");
        let sp = parse_inline("(https://a.b/c)");
        assert_eq!(sp[1].link.as_deref(), Some("https://a.b/c"));
        // タグでない `<` はそのまま文字
        assert_eq!(kinds("a < b"), vec![("text", "a < b".to_string())]);
    }

    #[test]
    fn footnote_reference_and_definition() {
        let sp = kinds("本文[^1]です");
        assert_eq!(
            sp,
            vec![
                ("text", "本文".to_string()),
                ("footnote", "[1]".to_string()),
                ("text", "です".to_string()),
            ]
        );
        assert_eq!(
            footnote_def("[^1]: 説明文"),
            Some(("1".to_string(), "説明文".to_string()))
        );
        assert_eq!(footnote_def("[^]: x"), None);
        assert_eq!(footnote_def("ただの行"), None);
    }

    #[test]
    fn strikethrough_and_mixed_japanese_emoji() {
        let sp = parse_inline("~~消し~~ 🎉 **太字** です");
        assert!(sp[0].strike && sp[0].text == "消し");
        assert!(sp.iter().any(|s| s.strong && s.text == "太字"));
        assert!(sp.iter().any(|s| s.text.contains('🎉')));
    }

    // ─── ブロック構文 ───────────────────────────────────────────────

    #[test]
    fn task_list_variants() {
        assert_eq!(list_marker("- [ ] todo"), Some((6, "☐".into())));
        assert_eq!(list_marker("* [x] done"), Some((6, "☑".into())));
        assert_eq!(list_marker("+ [X] done"), Some((6, "☑".into())));
        assert_eq!(list_marker("1) 番号"), Some((3, "1.".into())));
        assert_eq!(list_marker("12. 番号"), Some((4, "12.".into())));
        assert_eq!(list_marker("-"), Some((1, "•".into())));
        assert_eq!(list_marker("-notalist"), None);
    }

    /// 入れ子リストは「行頭の空白量 = 段差」で表す。
    /// 描画は indent をそのまま字下げに使うので、ここでは各行が
    /// リストとして認識され、段差が取れることを確かめる。
    #[test]
    fn nested_list_indentation() {
        let src = "- 親\n  - 子\n    1. 孫\n- 親2";
        let got: Vec<(usize, String)> = src
            .lines()
            .map(|l| {
                let t = l.trim_start();
                let indent = l.len() - t.len();
                let (_, bullet) = list_marker(t).expect("リスト行として認識される");
                (indent, bullet)
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (0, "•".to_string()),
                (2, "•".to_string()),
                (4, "1.".to_string()),
                (0, "•".to_string()),
            ]
        );
    }

    #[test]
    fn quote_nesting_depth() {
        assert_eq!(quote_depth("> a"), (1, "a"));
        assert_eq!(quote_depth(">> b"), (2, "b"));
        assert_eq!(quote_depth("> > c"), (2, "c"));
        assert_eq!(quote_depth("no quote"), (0, "no quote"));
    }

    #[test]
    fn atx_headings_and_anchors() {
        assert_eq!(atx_heading("# 題"), Some((1, "題".to_string())));
        assert_eq!(atx_heading("###### 小"), Some((6, "小".to_string())));
        // 閉じの ### とアンカー指定は本文から外す
        assert_eq!(atx_heading("## 題 ##"), Some((2, "題".to_string())));
        assert_eq!(
            atx_heading("## インストール {#install}"),
            Some((2, "インストール".to_string()))
        );
        // `#` の直後に空白が無いものは見出しではない
        assert_eq!(atx_heading("#hashtag"), None);
        assert_eq!(atx_heading("####### 7つ"), None);
        assert_eq!(atx_heading("普通の行"), None);
    }

    #[test]
    fn fence_open_variants() {
        assert_eq!(fence_open("```"), Some(("```", String::new())));
        assert_eq!(fence_open("```rust"), Some(("```", "rust".to_string())));
        assert_eq!(fence_open("~~~python"), Some(("~~~", "python".to_string())));
        // 情報文字列は先頭語だけ言語として使う
        assert_eq!(
            fence_open("```rust,no_run"),
            Some(("```", "rust".to_string()))
        );
        assert_eq!(
            fence_open("````js title=x"),
            Some(("```", "js".to_string()))
        );
        assert_eq!(fence_open("  ```sh"), Some(("```", "sh".to_string())));
        assert_eq!(fence_open("普通の行"), None);
    }

    #[test]
    fn hard_break_forms() {
        assert_eq!(hard_break("行末に空白2つ  "), (true, "行末に空白2つ"));
        assert_eq!(
            hard_break("行末にバックスラッシュ\\"),
            (true, "行末にバックスラッシュ")
        );
        // `\\` はバックスラッシュそのもの (改行しない)
        assert_eq!(hard_break("path\\\\"), (false, "path\\\\"));
        assert_eq!(hard_break("普通の行"), (false, "普通の行"));
        assert_eq!(hard_break("  "), (false, ""));
    }

    #[test]
    fn setext_underlines() {
        assert_eq!(setext_level("==="), Some(1));
        assert_eq!(setext_level("---"), Some(2));
        assert_eq!(setext_level("--"), Some(2));
        assert_eq!(setext_level("-"), None);
        assert_eq!(setext_level("=-="), None);
        assert_eq!(setext_level("text"), None);
    }

    #[test]
    fn front_matter_is_split_off() {
        let (fm, body) = split_front_matter("---\ntitle: x\ntags: [a]\n---\n# 本文\n");
        assert_eq!(fm, Some("title: x\ntags: [a]\n"));
        assert_eq!(body, "# 本文\n");
        // TOML 形式
        let (fm, body) = split_front_matter("+++\na = 1\n+++\nbody");
        assert_eq!(fm, Some("a = 1\n"));
        assert_eq!(body, "body");
        // 閉じていないものはフロントマターにしない (水平線として残す)
        let src = "---\nnot closed\n";
        assert_eq!(split_front_matter(src), (None, src));
        // 冒頭が水平線なだけの文書も飲み込まない
        let hr = "---\n\n本文\n";
        assert_eq!(split_front_matter(hr), (None, hr));
        // BOM 付きでも認識する
        let (fm, _) = split_front_matter("\u{feff}---\nk: v\n---\nx");
        assert_eq!(fm, Some("k: v\n"));
        // フロントマターの無い文書は素通し
        let plain = "# 見出し\n";
        assert_eq!(split_front_matter(plain), (None, plain));
    }

    #[test]
    fn crlf_input_is_handled() {
        let (fm, body) = split_front_matter("---\r\nk: v\r\n---\r\n# H\r\n");
        assert_eq!(fm, Some("k: v\r\n"));
        assert!(body.starts_with("# H"));
        // lines() が \r を落とすので、ハード改行判定も CRLF で壊れない
        assert_eq!("a  \r\nb".lines().next(), Some("a  "));
    }

    // ─── 巨大入力の上限 ─────────────────────────────────────────────

    #[test]
    fn cap_input_truncates_large_documents() {
        let small = "小さい文書\n";
        let (t, cut) = cap_input(small);
        assert_eq!(t, small);
        assert!(!cut);

        // 10MB 相当 (日本語混じり) でも上限内へ切り詰められ、文字境界で切れる
        let big = "あいうえお かきくけこ\n".repeat(500_000);
        assert!(big.len() > 10 * 1024 * 1024);
        let (t, cut) = cap_input(&big);
        assert!(cut);
        assert!(t.len() <= MAX_PREVIEW_BYTES);
        assert!(big.starts_with(t), "先頭からの部分文字列であること");
        // 文字境界で切れている (ここで panic しないことが本質)
        assert!(t.chars().count() > 0);

        // 1 行あたりが極端に短い文書は行数でも止まる
        let many = "x\n".repeat(MAX_PREVIEW_LINES * 2);
        let (t, cut) = cap_input(&many);
        assert!(cut);
        assert!(t.lines().count() <= MAX_PREVIEW_LINES);

        let note = truncation_note(1024, 2048);
        assert!(note.contains('1') && note.contains('2'));
    }

    #[test]
    fn extremely_long_single_line_does_not_panic() {
        let line = "あ".repeat(400_000); // 1.2MB、改行なし
        let (t, cut) = cap_input(&line);
        assert!(cut);
        assert!(t.len() <= MAX_PREVIEW_BYTES);
        // 途中で切れても UTF-8 として妥当
        assert!(t.chars().all(|c| c == 'あ'));
        // インライン解析も落ちない
        assert!(!parse_inline(t).is_empty());
    }

    #[test]
    fn malformed_inline_markup_never_panics() {
        for src in [
            "**未閉じ",
            "`code",
            "[link](",
            "![img](",
            "<b>unclosed",
            "<<<>>>",
            "~~~~~~",
            "[^",
            "***",
            "a<b<c<d",
            "https://",
            "|||",
        ] {
            let _ = parse_inline(src);
            let _ = list_marker(src);
            let _ = split_row(src);
            let _ = table_aligns(src);
            let _ = footnote_def(src);
            let _ = split_front_matter(src);
        }
    }
}
