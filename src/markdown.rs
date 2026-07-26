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
    lang == "Markdown"
        || t.ends_with(".md")
        || t.ends_with(".markdown")
        || t.ends_with(".mdx")
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
    let starts = |s: &str| s.chars().enumerate().all(|(k, c)| chars.get(i + k) == Some(&c));
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
                // エスケープ: 次の1文字をそのまま出す
                cur.push(next.unwrap());
                i += 2;
            }
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
                        if t.len() > 2 && t.starts_with(' ') && t.ends_with(' ')
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
                let prev = if i == 0 { None } else { chars.get(i - 1).copied() };
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
            '[' if next == Some('^') => {
                match (i + 2..chars.len()).find(|&k| chars[k] == ']') {
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
                }
            }
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
    t.starts_with('|')
        && t.contains('-')
        && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
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
    let rest = url.strip_prefix("data:").or_else(|| url.strip_prefix("DATA:"))?;
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
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-')
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
    Some(ctx.load_texture(format!("zv-md-img:{key}"), color, egui::TextureOptions::LINEAR))
}

/// 画像を描けないときの一行プレースホルダ文言。
fn image_placeholder_text(src: &ImageSrc, alt: &str) -> String {
    let alt = alt.trim();
    match src {
        ImageSrc::Remote(_) => {
            if alt.is_empty() {
                tr("🌐 リモート画像 (未取得)")
            } else {
                trf("🌐 リモート画像 (未取得): {alt}", &[("alt", alt.to_string())])
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
                f.layout_no_wrap(sp.text.clone(), font, Color32::WHITE).size().x
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
    let CellStyle { size, strong: strong_all, color, align } = style;
    if align == TableAlign::Left {
        line_ui(ui, theme, text, size, strong_all, color, rctx);
        return;
    }
    let w = ui.available_width();
    let pad = (w - spans_width(ui, text, size)).max(0.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.set_min_width(w);
        ui.add_space(if align == TableAlign::Right { pad } else { pad * 0.5 });
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
    let flush_para =
        |ui: &mut egui::Ui, para: &mut String, theme: &Theme, rctx: &mut RenderCtx| {
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
            let code: String = lines[start..end]
                .iter()
                .flat_map(|l| [*l, "\n"])
                .collect();
            code_block_ui(ui, theme, hl, base, i, &lang_tok, &code);
            i = (end + 1).min(lines.len());
            continue;
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
                    ui.label(RichText::new(format!("{bar} ")).color(theme.accent).size(base));
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
        if trimmed.starts_with('|')
            && lines.get(i + 1).map(|l| is_table_sep(l)).unwrap_or(false)
        {
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
            let cap = ((ui.available_width() - 34.0 - 16.0 * (ncols - 1) as f32)
                / ncols as f32)
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
                let kind = if s.image {
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
        assert_eq!(table_aligns("|:--|:-:|--:|---|"), vec![Left, Center, Right, Left]);
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
        assert_eq!(resolve_image(Some(&dir), "data:image/png;base64,AAAA"), None);
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
        assert_eq!(hard_break("行末にバックスラッシュ\\"), (true, "行末にバックスラッシュ"));
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
