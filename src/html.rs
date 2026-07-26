//! HTML → Markdown 変換 — プレビューの HTML 対応。
//!
//! 2つの入口を持つ:
//! - [`preprocess_markdown`]: Markdown 中に埋め込まれた HTML (README によくある
//!   `<img>` / `<div align>` / `<br>` / `<table>` / `<details>` 等) を Markdown
//!   相当へ変換する。フェンスコードとインラインコード内は一切触らない。
//! - [`html_to_md`]: .html ファイル全体を Markdown へ変換する。head/script/style
//!   は捨て、本文の構造 (見出し・リスト・テーブル・pre 等) を写し取る。
//!
//! どちらも出力は markdown::render がそのまま描画できる Markdown テキスト。
//! ブラウザエンジン並みの再現は狙わず「読める形に完全に落とす」ことを目的とする。

/// このバッファを HTML としてプレビュー可能か。
pub fn is_html(title: &str, lang: &str) -> bool {
    let t = title.to_lowercase();
    lang == "HTML"
        || t.ends_with(".html")
        || t.ends_with(".htm")
        || t.ends_with(".xhtml")
}

// ─── 文字実体参照 ───────────────────────────────────────────────────

/// `&amp;` 等の文字実体参照 1 つを解決する (`&` `;` は含まない名前部分)。
fn entity(name: &str) -> Option<String> {
    // 数値参照 &#123; / &#x1F600;
    if let Some(num) = name.strip_prefix('#') {
        let cp = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num.parse::<u32>().ok()?
        };
        return char::from_u32(cp).map(|c| c.to_string());
    }
    let s = match name {
        "amp" | "AMP" => "&",
        "lt" | "LT" => "<",
        "gt" | "GT" => ">",
        "quot" | "QUOT" => "\"",
        "apos" => "'",
        // 実体としての nbsp は「折り返さない空白」。通常の空白に潰さない
        "nbsp" => "\u{00A0}",
        "ensp" => "\u{2002}",
        "emsp" => "\u{2003}",
        "thinsp" => "\u{2009}",
        "zwnj" => "\u{200C}",
        "zwj" => "\u{200D}",
        "shy" => "\u{00AD}",
        "lsaquo" => "‹",
        "rsaquo" => "›",
        "sbquo" => "‚",
        "bdquo" => "„",
        "dagger" => "†",
        "Dagger" => "‡",
        "permil" => "‰",
        "prime" => "′",
        "Prime" => "″",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "cent" => "¢",
        "curren" => "¤",
        "frac12" => "½",
        "frac14" => "¼",
        "frac34" => "¾",
        "sup1" => "¹",
        "sup2" => "²",
        "sup3" => "³",
        "micro" => "µ",
        "not" => "¬",
        "iexcl" => "¡",
        "iquest" => "¿",
        "brvbar" => "¦",
        "uml" => "¨",
        "ordf" => "ª",
        "ordm" => "º",
        "acute" => "´",
        "cedil" => "¸",
        "macr" => "¯",
        "sup" => "⊃",
        "sub" => "⊂",
        "ne" => "≠",
        "le" => "≤",
        "ge" => "≥",
        "asymp" => "≈",
        "equiv" => "≡",
        "infin" => "∞",
        "radic" => "√",
        "sum" => "∑",
        "prod" => "∏",
        "int" => "∫",
        "part" => "∂",
        "nabla" => "∇",
        "isin" => "∈",
        "notin" => "∉",
        "cap" => "∩",
        "cup" => "∪",
        "and" => "∧",
        "or" => "∨",
        "forall" => "∀",
        "exist" => "∃",
        "empty" => "∅",
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ε",
        "theta" => "θ",
        "lambda" => "λ",
        "mu" => "μ",
        "pi" => "π",
        "sigma" => "σ",
        "tau" => "τ",
        "phi" => "φ",
        "omega" => "ω",
        "Delta" => "Δ",
        "Sigma" => "Σ",
        "Omega" => "Ω",
        "Alpha" => "Α",
        "Beta" => "Β",
        "Gamma" => "Γ",
        "Lambda" => "Λ",
        "Pi" => "Π",
        "Phi" => "Φ",
        "lArr" => "⇐",
        "rArr" => "⇒",
        "hArr" => "⇔",
        "crarr" => "↵",
        "spades" => "♠",
        "clubs" => "♣",
        "diams" => "♦",
        "loz" => "◊",
        "oline" => "‾",
        "frasl" => "⁄",
        "minus" => "−",
        "lowast" => "∗",
        "there4" => "∴",
        "ang" => "∠",
        "perp" => "⊥",
        "sim" => "∼",
        "cong" => "≅",
        "prop" => "∝",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "mdash" => "—",
        "ndash" => "–",
        "hellip" => "…",
        "laquo" => "«",
        "raquo" => "»",
        "ldquo" => "\u{201C}",
        "rdquo" => "\u{201D}",
        "lsquo" => "\u{2018}",
        "rsquo" => "\u{2019}",
        "times" => "×",
        "divide" => "÷",
        "middot" => "·",
        "bull" => "•",
        "sect" => "§",
        "para" => "¶",
        "deg" => "°",
        "plusmn" => "±",
        "larr" => "←",
        "rarr" => "→",
        "uarr" => "↑",
        "darr" => "↓",
        "harr" => "↔",
        "star" => "☆",
        "starf" => "★",
        "check" => "✓",
        "cross" => "✗",
        "heart" => "♥",
        _ => return None,
    };
    Some(s.to_string())
}

/// テキスト中の文字実体参照をすべて解決する。未知の参照はそのまま残す。
/// (変換ループは entity() を直接使う。属性値の復号にはこちらを使う)
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '&' {
            // `;` は 32 文字以内に現れるはず (それ以上は実体参照とみなさない)
            let end = (i + 1..chars.len().min(i + 33)).find(|&k| chars[k] == ';');
            if let Some(end) = end {
                let name: String = chars[i + 1..end].iter().collect();
                if let Some(rep) = entity(&name) {
                    out.push_str(&rep);
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ─── タグの読み取り ─────────────────────────────────────────────────

/// 読み取ったタグ。
struct Tag {
    name: String,
    closing: bool,
    /// 属性部分の生文字列 (小文字化済み、値は元のまま)
    attrs: String,
    /// タグ全体の終端位置 (`>` の次)
    end: usize,
}

/// chars[i] == '<' からタグを読む。タグとして成立しなければ None。
fn read_tag(chars: &[char], i: usize) -> Option<Tag> {
    let mut k = i + 1;
    let closing = if chars.get(k) == Some(&'/') {
        k += 1;
        true
    } else {
        false
    };
    // タグ名はアルファベット始まり (英数字と - を許容)
    if !chars.get(k).is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let name_start = k;
    while chars
        .get(k)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '-')
    {
        k += 1;
    }
    let name: String = chars[name_start..k].iter().collect::<String>().to_lowercase();
    // `>` まで読む (引用符内の > は無視)
    let mut quote: Option<char> = None;
    let attr_start = k;
    while k < chars.len() {
        let c = chars[k];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => {
                let attrs: String = chars[attr_start..k].iter().collect();
                return Some(Tag {
                    name,
                    closing,
                    attrs: attrs.trim().trim_end_matches('/').to_string(),
                    end: k + 1,
                });
            }
            None => {}
        }
        k += 1;
    }
    None
}

/// 属性文字列から `name="value"` / `name='value'` / `name=value` を取り出す。
fn attr(attrs: &str, name: &str) -> Option<String> {
    // ASCII のみ小文字化 (バイト長を変えず、attrs とオフセットを共有するため)
    let lower: String = attrs.chars().map(|c| c.to_ascii_lowercase()).collect();
    let mut search = 0;
    while let Some(p) = lower[search..].find(name) {
        let at = search + p;
        // 前が単語境界か
        let ok_before = at == 0
            || !lower.as_bytes()[at - 1].is_ascii_alphanumeric() && lower.as_bytes()[at - 1] != b'-';
        let after = at + name.len();
        if ok_before {
            let rest = lower[after..].trim_start();
            if let Some(eq_rest) = rest.strip_prefix('=') {
                let val_off = attrs.len() - eq_rest.len();
                let val = attrs[val_off..].trim_start();
                let val = if let Some(v) = val.strip_prefix('"') {
                    v.split('"').next().unwrap_or("")
                } else if let Some(v) = val.strip_prefix('\'') {
                    v.split('\'').next().unwrap_or("")
                } else {
                    val.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or("")
                };
                return Some(val.to_string());
            }
        }
        search = at + name.len();
    }
    None
}

/// 属性値を取り出して実体参照も解決する (`src="a&amp;b.png"` 対策)。
fn attr_text(attrs: &str, name: &str) -> Option<String> {
    attr(attrs, name).map(|v| decode_entities(&v))
}

/// `style="font-weight:bold"` 等から Markdown の装飾記号を作る。
/// 返り値は (開き, 閉じ)。装飾なしなら両方空。
fn style_marks(attrs: &str) -> (String, String) {
    let Some(style) = attr(attrs, "style") else {
        return (String::new(), String::new());
    };
    let s = style.to_ascii_lowercase().replace(' ', "");
    let mut open = String::new();
    if s.contains("font-weight:bold")
        || s.contains("font-weight:600")
        || s.contains("font-weight:700")
        || s.contains("font-weight:800")
        || s.contains("font-weight:900")
    {
        open.push_str("**");
    }
    if s.contains("font-style:italic") || s.contains("font-style:oblique") {
        open.push('*');
    }
    if s.contains("line-through") {
        open.push_str("~~");
    }
    let close: String = {
        // 閉じ記号は開いた順の逆順
        let mut parts: Vec<&str> = Vec::new();
        if open.starts_with("**") {
            parts.push("**");
        }
        if open.contains('*') && open.trim_start_matches("**").starts_with('*') {
            parts.push("*");
        }
        if open.ends_with("~~") {
            parts.push("~~");
        }
        parts.reverse();
        parts.concat()
    };
    (open, close)
}

// ─── 変換器 ─────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    /// Markdown 埋め込み HTML: 未知タグ・非タグの `<` は原文のまま残す
    Markdown,
    /// HTML 文書全体: 未知タグは捨て、テキストの連続空白は 1 個に潰す
    Html,
}

enum ListKind {
    Ul,
    Ol(usize),
}

#[derive(Default)]
struct TableState {
    rows: Vec<Vec<String>>,
    header: bool,
    cur_row: Vec<String>,
    cur_cell: Option<String>,
    row_is_th: bool,
    /// `<caption>` の本文 (テーブルの直前に見出しとして出す)
    caption: String,
    in_caption: bool,
}

struct Conv {
    mode: Mode,
    out: String,
    lists: Vec<ListKind>,
    quote_depth: usize,
    links: Vec<Option<String>>,
    pre: bool,
    pre_lang_pending: bool,
    table: Option<TableState>,
    /// `<table>` の入れ子の深さ (内側のテーブルは外側のセルへ平坦化する)
    table_depth: usize,
    /// `<span style>` 等で開いた装飾の閉じ記号スタック
    spans: Vec<String>,
}

impl Conv {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            out: String::new(),
            lists: Vec::new(),
            quote_depth: 0,
            links: Vec::new(),
            pre: false,
            pre_lang_pending: false,
            table: None,
            table_depth: 0,
            spans: Vec::new(),
        }
    }

    /// 出力先 (テーブルのキャプション内 → セル内 → 本文 の順)。
    fn sink(&mut self) -> &mut String {
        if let Some(t) = &mut self.table {
            if t.in_caption {
                return &mut t.caption;
            }
            if let Some(c) = &mut t.cur_cell {
                return c;
            }
        }
        &mut self.out
    }

    /// テーブルのセル外にいる (セル間の空白などは捨てる)。
    fn in_table_gap(&self) -> bool {
        self.table
            .as_ref()
            .is_some_and(|t| t.cur_cell.is_none() && !t.in_caption)
    }

    /// テキストを 1 文字書く。行頭なら引用プレフィックスを付ける。
    fn push_char(&mut self, c: char) {
        if self.in_table_gap() {
            return; // <tr> と <td> の間のテキストは捨てる
        }
        if self.mode == Mode::Html && !self.pre && c.is_whitespace() {
            // 連続空白は 1 個へ。行頭 (ブロック境界直後) には置かない
            let s = self.sink();
            if s.is_empty() || s.ends_with(char::is_whitespace) {
                return;
            }
            s.push(' ');
            return;
        }
        if c == '\n' {
            self.newline();
            return;
        }
        let quote = self.quote_depth;
        let s = self.sink();
        if quote > 0 && (s.is_empty() || s.ends_with('\n')) {
            for _ in 0..quote {
                s.push_str("> ");
            }
        }
        s.push(c);
    }

    fn push_str(&mut self, t: &str) {
        for c in t.chars() {
            self.push_char(c);
        }
    }

    fn newline(&mut self) {
        let s = self.sink();
        if !s.is_empty() {
            s.push('\n');
        }
    }

    /// ブロック境界 = 空行を 1 つ挟む。
    fn block_break(&mut self) {
        let quote = self.quote_depth;
        let s = self.sink();
        if s.is_empty() {
            return;
        }
        while s.ends_with(' ') || s.ends_with('\t') {
            s.pop();
        }
        if !s.ends_with('\n') {
            s.push('\n');
        }
        if quote == 0 && !s.ends_with("\n\n") {
            s.push('\n');
        }
    }

    /// タグ 1 個を処理する。
    fn tag(&mut self, tag: &Tag) {
        let name = tag.name.as_str();
        match name {
            "b" | "strong" => self.push_str("**"),
            "i" | "em" | "cite" | "var" | "dfn" => self.push_str("*"),
            "s" | "del" | "strike" => self.push_str("~~"),
            "code" if self.pre => {
                // <pre><code class="language-x"> の言語はフェンス開始時に処理済み
            }
            "code" | "kbd" | "samp" | "tt" => self.push_str("`"),
            "br" => {
                if self.table.is_some() {
                    self.push_char(' ');
                } else if !tag.closing {
                    // Markdown のハード改行 (行末スペース 2 個)
                    self.sink().push_str("  ");
                    self.newline();
                }
            }
            "hr" => {
                if !tag.closing {
                    self.block_break();
                    self.push_str("---");
                    self.block_break();
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.tag_heading(tag, name),
            "p" | "div" | "section" | "article" | "main" | "header" | "footer" | "aside"
            | "nav" | "center" | "figure" | "figcaption" | "address" | "dl" | "form"
            | "fieldset" | "hgroup" | "dialog" | "search" | "noscript" | "html" | "body" => {
                self.block_break();
            }
            // 定義リスト: 用語は太字、説明は 1 段下げ
            "dt" => {
                if tag.closing {
                    self.push_str("**");
                    self.newline();
                } else {
                    self.block_break();
                    self.push_str("**");
                }
            }
            "dd" => {
                if tag.closing {
                    self.newline();
                } else {
                    self.newline();
                    self.push_str("  ");
                }
            }
            // インライン装飾のうち Markdown に無いものは素通し (中身は残る)。
            // style 属性に太字/斜体/打消しが書かれていれば拾う
            "span" | "font" | "small" | "u" | "ins" | "mark" | "abbr" | "bdi" | "bdo" | "q"
            | "time" | "data" | "output" | "ruby" | "rt" | "rp" | "big" => {
                if tag.closing {
                    if let Some(close) = self.spans.pop() {
                        self.push_str(&close);
                    }
                } else {
                    let (open, close) = style_marks(&tag.attrs);
                    self.push_str(&open);
                    self.spans.push(close);
                }
            }
            "sup" => self.push_str(if tag.closing { ")" } else { "^(" }),
            "sub" => self.push_str(if tag.closing { ")" } else { "_(" }),
            // チェックボックスはタスクリスト記法へ (li 直後なら `- [x]` になる)
            "input" => {
                if tag.closing {
                    return;
                }
                let ty = attr(&tag.attrs, "type").unwrap_or_default().to_ascii_lowercase();
                if ty == "checkbox" || ty == "radio" {
                    let done = attr(&tag.attrs, "checked").is_some()
                        || tag.attrs.to_ascii_lowercase().split_whitespace().any(|w| w == "checked");
                    let in_list = {
                        let s = self.sink();
                        s.ends_with("- ") || s.ends_with("* ") || s.ends_with("+ ")
                    };
                    if in_list {
                        self.push_str(if done { "[x] " } else { "[ ] " });
                    } else {
                        self.push_str(if done { "☑ " } else { "☐ " });
                    }
                }
            }
            // 再生できないメディアは「ここに何があるか」をリンクで残す
            "video" | "audio" | "embed" | "object" | "iframe" => {
                if tag.closing {
                    return;
                }
                let src = attr_text(&tag.attrs, "src")
                    .or_else(|| attr_text(&tag.attrs, "data"))
                    .unwrap_or_default();
                if src.is_empty() {
                    return;
                }
                let icon = match name {
                    "audio" => "🔊",
                    "video" => "🎬",
                    _ => "🔗",
                };
                self.push_str(&format!("[{icon} {src}]({src})"));
            }
            "blockquote" => {
                self.block_break();
                if tag.closing {
                    self.quote_depth = self.quote_depth.saturating_sub(1);
                } else {
                    self.quote_depth += 1;
                }
            }
            "ul" => {
                if tag.closing {
                    self.lists.pop();
                    if self.lists.is_empty() {
                        self.block_break();
                    }
                } else {
                    if self.lists.is_empty() {
                        self.block_break();
                    }
                    self.lists.push(ListKind::Ul);
                }
            }
            "ol" => {
                if tag.closing {
                    self.lists.pop();
                    if self.lists.is_empty() {
                        self.block_break();
                    }
                } else {
                    if self.lists.is_empty() {
                        self.block_break();
                    }
                    self.lists.push(ListKind::Ol(0));
                }
            }
            "li" => self.tag_list_item(tag),
            "a" => {
                if tag.closing {
                    if let Some(href) = self.links.pop().flatten() {
                        self.push_str(&format!("]({href})"));
                    }
                } else {
                    let href = attr_text(&tag.attrs, "href").filter(|h| !h.is_empty());
                    if href.is_some() {
                        self.push_str("[");
                    }
                    self.links.push(href);
                }
            }
            "img" => {
                if tag.closing {
                    return;
                }
                // src が空なら data-src / srcset の先頭を代わりに使う (遅延読込対策)
                let src = attr_text(&tag.attrs, "src")
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| attr_text(&tag.attrs, "data-src"))
                    .or_else(|| {
                        attr_text(&tag.attrs, "srcset").and_then(|s| {
                            s.split(',')
                                .next()
                                .and_then(|p| p.split_whitespace().next())
                                .map(|p| p.to_string())
                        })
                    })
                    .unwrap_or_default();
                if src.trim().is_empty() {
                    return;
                }
                let alt = attr_text(&tag.attrs, "alt")
                    .or_else(|| attr_text(&tag.attrs, "title"))
                    .unwrap_or_default();
                // alt の `[` `]` は画像記法を壊すので丸括弧へ置き換える
                let alt = alt.replace('[', "(").replace(']', ")").replace('\n', " ");
                self.push_str(&format!("![{alt}]({})", src.trim()));
            }
            "details" => self.block_break(),
            "summary" => {
                if !tag.closing {
                    self.block_break();
                    self.push_str("▶ **");
                } else {
                    self.push_str("**");
                    self.block_break();
                }
            }
            // 入れ子テーブルは外側の表を壊さないよう「内側は無視」して平坦化する
            "table" => {
                if tag.closing {
                    self.table_depth = self.table_depth.saturating_sub(1);
                    if self.table_depth == 0 {
                        self.emit_table();
                    }
                } else {
                    if self.table_depth == 0 {
                        self.block_break();
                        self.table = Some(TableState::default());
                    }
                    self.table_depth += 1;
                }
            }
            "caption" => {
                if let Some(t) = &mut self.table {
                    t.in_caption = !tag.closing;
                }
            }
            "thead" | "tbody" | "tfoot" | "colgroup" | "col" => {}
            "tr" if self.table_depth <= 1 => self.tag_table_row(tag),
            "tr" => {}
            "th" | "td" if self.table_depth <= 1 => {
                if let Some(t) = &mut self.table {
                    if let Some(c) = t.cur_cell.take() {
                        t.cur_row.push(c);
                    }
                    if !tag.closing {
                        t.cur_cell = Some(String::new());
                        if name == "td" {
                            t.row_is_th = false;
                        }
                    }
                }
            }
            "th" | "td" => {
                // 内側テーブルのセル区切りは空白 1 個に落とす
                self.push_char(' ');
            }
            "pre" => self.tag_pre(tag),
            _ => {
                // 未知タグ: HTML モードでは捨てる (中身のテキストは流れてくる)。
                // Markdown モードでは呼び出し側 (convert) が原文のまま出力する
            }
        }
    }

    /// `tag()` から抽出: h1〜h6 の見出しタグを処理する。
    fn tag_heading(&mut self, tag: &Tag, name: &str) {
        self.block_break();
        if !tag.closing {
            let n = name[1..].parse::<usize>().unwrap_or(1);
            let quote = self.quote_depth;
            let s = self.sink();
            if quote > 0 && (s.is_empty() || s.ends_with('\n')) {
                for _ in 0..quote {
                    s.push_str("> ");
                }
            }
            for _ in 0..n {
                s.push('#');
            }
            s.push(' ');
        }
    }

    /// `tag()` から抽出: `<li>` を処理する (マーカーとインデントの出力)。
    fn tag_list_item(&mut self, tag: &Tag) {
        if tag.closing {
            return;
        }
        self.newline();
        let depth = self.lists.len().saturating_sub(1);
        let marker = match self.lists.last_mut() {
            Some(ListKind::Ol(n)) => {
                *n += 1;
                format!("{n}. ")
            }
            _ => "- ".to_string(),
        };
        let quote = self.quote_depth;
        let s = self.sink();
        if quote > 0 && (s.is_empty() || s.ends_with('\n')) {
            for _ in 0..quote {
                s.push_str("> ");
            }
        }
        for _ in 0..depth {
            s.push_str("  ");
        }
        s.push_str(&marker);
    }

    /// `tag()` から抽出: `<tr>` の開閉 (行バッファの確定/初期化) を処理する。
    fn tag_table_row(&mut self, tag: &Tag) {
        if let Some(t) = &mut self.table {
            if tag.closing {
                if let Some(c) = t.cur_cell.take() {
                    t.cur_row.push(c);
                }
                if !t.cur_row.is_empty() {
                    if t.rows.is_empty() && t.row_is_th {
                        t.header = true;
                    }
                    t.rows.push(std::mem::take(&mut t.cur_row));
                }
                t.row_is_th = false;
            } else {
                t.cur_row.clear();
                t.cur_cell = None;
                t.row_is_th = true;
            }
        }
    }

    /// `tag()` から抽出: `<pre>` の開閉 (コードフェンスの開始/終了) を処理する。
    fn tag_pre(&mut self, tag: &Tag) {
        if tag.closing {
            self.pre = false;
            self.pre_lang_pending = false;
            let s = self.sink();
            if !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str("```");
            self.block_break();
        } else {
            self.block_break();
            self.pre = true;
            // 言語は直後の <code class="language-x"> から拾う
            self.pre_lang_pending = true;
            self.sink().push_str("```");
            // <pre class="language-x"> にも対応
            if let Some(lang) = fence_lang(&tag.attrs) {
                self.sink().push_str(&lang);
                self.pre_lang_pending = false;
            }
            self.sink().push('\n');
        }
    }

    /// 溜めたテーブルを Markdown テーブルとして書き出す。
    fn emit_table(&mut self) {
        let Some(mut t) = self.table.take() else { return };
        t.in_caption = false;
        if let Some(c) = t.cur_cell.take() {
            t.cur_row.push(c);
        }
        if !t.cur_row.is_empty() {
            t.rows.push(std::mem::take(&mut t.cur_row));
        }
        let caption = t.caption.trim().replace('\n', " ");
        if !caption.is_empty() {
            self.block_break();
            self.push_str(&format!("**{caption}**"));
            self.newline();
        }
        if t.rows.is_empty() {
            return;
        }
        let ncols = t.rows.iter().map(|r| r.len()).max().unwrap_or(1);
        let clean = |s: &str| {
            s.replace('\n', " ")
                .replace('|', "\\|")
                .trim()
                .to_string()
        };
        self.block_break();
        for (ri, row) in t.rows.iter().enumerate() {
            let mut line = String::from("|");
            for c in 0..ncols {
                line.push(' ');
                line.push_str(&clean(row.get(c).map(|s| s.as_str()).unwrap_or("")));
                line.push_str(" |");
            }
            self.push_str(&line);
            self.newline();
            if ri == 0 {
                // Markdown テーブルにはヘッダ行が必須なので、th が無くても
                // 最初の行をヘッダとして扱い区切り行を入れる
                let mut sep = String::from("|");
                for _ in 0..ncols {
                    sep.push_str(" --- |");
                }
                self.push_str(&sep);
                self.newline();
            }
        }
        self.block_break();
    }
}

/// class 属性から `language-x` / `lang-x` を取り出す。
fn fence_lang(attrs: &str) -> Option<String> {
    let class = attr(attrs, "class")?;
    class
        .split_whitespace()
        .find_map(|c| c.strip_prefix("language-").or_else(|| c.strip_prefix("lang-")))
        .map(|s| s.to_string())
}

/// 変換対象として扱うタグか。Markdown モードでは未知タグを原文のまま残すため、
/// この判定に漏れたもの (`Vec<String>` の型パラメータ等) は変換されない。
///
/// 標準 HTML の要素名はできるだけ網羅する。tag() に処理が無いものは
/// 「何も出さない = 中身のテキストだけ残る」に落ちるので、生のマークアップが
/// ユーザーの目に触れることはない。
fn is_known_tag(name: &str) -> bool {
    matches!(
        name,
        "b" | "strong" | "i" | "em" | "cite" | "var" | "dfn" | "s" | "del" | "strike"
            | "code" | "kbd" | "samp" | "tt" | "br" | "hr"
            | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            | "p" | "div" | "section" | "article" | "main" | "header" | "footer" | "aside"
            | "nav" | "center" | "figure" | "figcaption" | "address" | "dl" | "dt" | "dd"
            | "blockquote" | "ul" | "ol" | "li" | "a" | "img" | "details" | "summary"
            | "table" | "thead" | "tbody" | "tfoot" | "caption" | "colgroup" | "col"
            | "tr" | "th" | "td" | "pre"
            | "span" | "small" | "sub" | "sup" | "u" | "ins" | "mark" | "abbr" | "font"
            | "picture" | "source" | "video" | "audio" | "input" | "label" | "button"
            // 構造・フォーム・その他の標準要素 (処理は無く、中身だけ残す)
            | "html" | "body" | "form" | "fieldset" | "legend" | "textarea" | "select"
            | "optgroup" | "datalist" | "hgroup" | "menu" | "dialog" | "search"
            | "progress" | "meter" | "output" | "time" | "data" | "ruby" | "rt" | "rp"
            | "rb" | "rtc" | "bdi" | "bdo" | "wbr" | "area" | "track" | "embed" | "object"
            | "param" | "noscript" | "slot" | "base" | "link" | "meta" | "title" | "q"
            | "big" | "marquee" | "nobr" | "blink" | "acronym" | "basefont" | "noframes"
            | "iframe"
    )
}

/// Markdown モードで `<…>` をタグとして扱ってよいか。
///
/// `Vec<Option<T>>` のようなジェネリクスは「タグ名らしき語 + 属性部に `<`」
/// という形になる。属性部に `<` を含むものはタグとみなさないことで、
/// コード片を壊さずに本物の HTML だけを変換できる。
fn looks_like_markup(tag: &Tag) -> bool {
    !tag.attrs.contains('<') && is_known_tag(&tag.name)
}

/// script / style / head 等、中身ごと捨てるタグの終了位置を探す。
/// 見つからなければ末尾まで捨てる。
fn skip_until_close(chars: &[char], from: usize, name: &str) -> usize {
    let close: Vec<char> = format!("</{name}").chars().collect();
    let lower: Vec<char> = chars[from..]
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let mut k = 0;
    while k + close.len() <= lower.len() {
        if lower[k..k + close.len()] == close[..] {
            // `>` まで飛ばす
            let mut e = from + k + close.len();
            while e < chars.len() && chars[e] != '>' {
                e += 1;
            }
            return (e + 1).min(chars.len());
        }
        k += 1;
    }
    chars.len()
}

/// 共通の変換ループ。
///
/// 入力は描画側と同じ上限で切り詰める。10MB の HTML を毎回丸ごと
/// `Vec<char>` に展開すると UI スレッドが止まるため。
fn convert(text: &str, mode: Mode) -> String {
    let orig_len = text.len();
    let (text, truncated) = crate::markdown::cap_input(text);
    let chars: Vec<char> = text.chars().collect();
    let mut conv = Conv::new(mode);
    let mut i = 0;
    // Markdown モード用: フェンスコード/インラインコードの保護
    let mut in_fence = false;
    let mut at_line_start = true;
    let mut in_code = false;

    while i < chars.len() {
        let c = chars[i];

        if mode == Mode::Markdown {
            // 行頭の ``` でフェンスをトグルし、フェンス内は原文コピー
            if at_line_start {
                let mut k = i;
                while chars.get(k).is_some_and(|c| *c == ' ' || *c == '\t') {
                    k += 1;
                }
                if chars.get(k) == Some(&'`')
                    && chars.get(k + 1) == Some(&'`')
                    && chars.get(k + 2) == Some(&'`')
                {
                    in_fence = !in_fence;
                    in_code = false;
                    // フェンス行を行末まで原文コピー
                    while i < chars.len() {
                        let cc = chars[i];
                        conv.sink().push(cc);
                        i += 1;
                        if cc == '\n' {
                            break;
                        }
                    }
                    at_line_start = true;
                    continue;
                }
            }
            if in_fence {
                conv.sink().push(c);
                at_line_start = c == '\n';
                i += 1;
                continue;
            }
            // インラインコード (同一行内のみ)
            if c == '`' {
                in_code = !in_code;
                conv.push_char('`');
                i += 1;
                at_line_start = false;
                continue;
            }
            if c == '\n' {
                in_code = false;
            }
            if in_code {
                conv.sink().push(c);
                at_line_start = false;
                i += 1;
                continue;
            }
        }

        if c == '<' {
            // コメント <!-- ... -->
            if chars.get(i + 1) == Some(&'!') {
                if chars.get(i + 2) == Some(&'-') && chars.get(i + 3) == Some(&'-') {
                    let mut k = i + 4;
                    while k + 2 < chars.len() {
                        if chars[k] == '-' && chars[k + 1] == '-' && chars[k + 2] == '>' {
                            break;
                        }
                        k += 1;
                    }
                    i = (k + 3).min(chars.len());
                    continue;
                }
                // <!DOCTYPE ...> 等
                if mode == Mode::Html {
                    while i < chars.len() && chars[i] != '>' {
                        i += 1;
                    }
                    i = (i + 1).min(chars.len());
                    continue;
                }
            }
            if let Some(tag) = read_tag(&chars, i) {
                // Markdown モードの未知タグは原文のまま残す (Vec<String> 等の誤爆防止)
                if mode == Mode::Markdown
                    && !looks_like_markup(&tag)
                    && !matches!(
                        tag.name.as_str(),
                        "script" | "style" | "head" | "svg" | "canvas" | "template"
                    )
                {
                    let raw: String = chars[i..tag.end].iter().collect();
                    conv.push_str(&raw);
                    at_line_start = false;
                    i = tag.end;
                    continue;
                }
                // 中身ごと捨てるタグ (描画できない/意味のないものだけ)
                if !tag.closing
                    && matches!(
                        tag.name.as_str(),
                        "script" | "style" | "head" | "svg" | "canvas" | "template"
                    )
                {
                    i = skip_until_close(&chars, tag.end, &tag.name);
                    at_line_start = true;
                    continue;
                }
                // pre 内では code 以外のタグを無視 (テキストだけ拾う)
                if conv.pre && !matches!(tag.name.as_str(), "pre" | "code") {
                    i = tag.end;
                    continue;
                }
                if conv.pre && tag.name == "code" && !tag.closing && conv.pre_lang_pending {
                    // ```<lang> を後付けする: 直前の "```\n" を差し替え
                    if let Some(lang) = fence_lang(&tag.attrs) {
                        let s = conv.sink();
                        if s.ends_with("```\n") {
                            s.truncate(s.len() - 1);
                            s.push_str(&lang);
                            s.push('\n');
                        }
                    }
                    conv.pre_lang_pending = false;
                    i = tag.end;
                    continue;
                }
                conv.tag(&tag);
                at_line_start = false;
                i = tag.end;
                continue;
            }
            // タグとして成立しない `<` は文字として扱う
            conv.push_char('<');
            at_line_start = false;
            i += 1;
            continue;
        }

        if c == '&' {
            // 実体参照 (pre 内でも解決する)
            let end = (i + 1..chars.len().min(i + 33)).find(|&k| chars[k] == ';');
            if let Some(end) = end {
                let name: String = chars[i + 1..end].iter().collect();
                if let Some(rep) = entity(&name) {
                    conv.push_str(&rep);
                    at_line_start = false;
                    i = end + 1;
                    continue;
                }
            }
        }

        if conv.pre {
            // <pre> と <code> の間の空白は捨てる (フェンス先頭の空行防止)
            if conv.pre_lang_pending && c.is_whitespace() {
                i += 1;
                continue;
            }
            conv.pre_lang_pending = false;
            conv.sink().push(c);
        } else {
            conv.push_char(c);
        }
        at_line_start = c == '\n';
        i += 1;
    }

    // 閉じ忘れの装飾/テーブルを流す (壊れた HTML でも出力が途切れないように)
    while let Some(close) = conv.spans.pop() {
        conv.push_str(&close);
    }
    conv.table_depth = 0;
    conv.emit_table();
    let mut out = conv.out.trim_end().to_string();
    if truncated {
        out.push_str("\n\n");
        out.push_str(&crate::markdown::truncation_note(text.len(), orig_len));
    }
    out
}

/// Markdown 中の埋め込み HTML を Markdown 相当へ変換する。
/// HTML を含まないテキストは (コメント除去と実体参照解決を除き) そのまま通る。
pub fn preprocess_markdown(text: &str) -> String {
    let (capped, truncated) = crate::markdown::cap_input(text);
    // HTML の気配が無ければ何もしない (毎フレーム呼ばれても軽いように)
    if !capped.contains('<') && !capped.contains('&') {
        let mut out = capped.to_string();
        if truncated {
            out.push_str("\n\n");
            out.push_str(&crate::markdown::truncation_note(capped.len(), text.len()));
        }
        return out;
    }
    convert(text, Mode::Markdown)
}

/// HTML 文書全体を Markdown へ変換する。
pub fn html_to_md(html: &str) -> String {
    convert(html, Mode::Html)
}

// ─── テスト ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_html_files() {
        assert!(is_html("index.html", "Plain Text"));
        assert!(is_html("Page.HTM", "Plain Text"));
        assert!(is_html("untitled", "HTML"));
        assert!(!is_html("README.md", "Markdown"));
    }

    #[test]
    fn entities_decode() {
        assert_eq!(decode_entities("a &amp; b &lt;c&gt;"), "a & b <c>");
        assert_eq!(decode_entities("&#65;&#x42;"), "AB");
        assert_eq!(decode_entities("&unknown; stays"), "&unknown; stays");
    }

    #[test]
    fn md_inline_tags_convert() {
        let md = preprocess_markdown("a <b>bold</b> and <i>it</i> <br> next");
        assert!(md.contains("**bold**"));
        assert!(md.contains("*it*"));
        // <br> はハード改行 (行末スペース2つ + 改行)
        assert!(md.contains("  \n"));
    }

    #[test]
    fn md_img_and_link_convert() {
        let md = preprocess_markdown(r#"<p align="center"><img src="logo.png" alt="Logo" width="200"></p>"#);
        assert!(md.contains("![Logo](logo.png)"));
        let md = preprocess_markdown(r#"<a href="https://x.y">site</a>"#);
        assert!(md.contains("[site](https://x.y)"));
    }

    #[test]
    fn md_fenced_code_is_untouched() {
        let src = "```html\n<b>not converted</b>\n```\n<b>converted</b>";
        let md = preprocess_markdown(src);
        assert!(md.contains("<b>not converted</b>"));
        assert!(md.contains("**converted**"));
    }

    #[test]
    fn md_inline_code_is_untouched() {
        let md = preprocess_markdown("use `<br>` tag and <br> here");
        assert!(md.contains("`<br>`"));
    }

    #[test]
    fn md_unknown_tag_stays_literal() {
        let md = preprocess_markdown("a Vec<String> and Result<T, E>");
        assert!(md.contains("Vec<String>"));
        assert!(md.contains("Result<T, E>"));
    }

    #[test]
    fn md_comment_is_stripped() {
        let md = preprocess_markdown("keep <!-- gone\nacross lines --> this");
        assert!(!md.contains("gone"));
        assert!(md.contains("keep"));
        assert!(md.contains("this"));
    }

    #[test]
    fn md_details_summary() {
        let md = preprocess_markdown("<details><summary>Open me</summary>\nbody\n</details>");
        assert!(md.contains("▶ **Open me**"));
        assert!(md.contains("body"));
    }

    #[test]
    fn html_table_converts() {
        let html = "<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>";
        let md = html_to_md(html);
        assert!(md.contains("| A | B |"));
        assert!(md.contains("| --- | --- |"));
        assert!(md.contains("| 1 | 2 |"));
    }

    #[test]
    fn html_doc_converts() {
        let html = r#"<!DOCTYPE html><html><head><title>T</title>
<style>body { color: red; }</style><script>var x = "<b>no</b>";</script></head>
<body><h1>Hello</h1><p>World &amp; you</p>
<ul><li>one</li><li>two</li></ul>
<pre><code class="language-rust">fn main() {}</code></pre>
</body></html>"#;
        let md = html_to_md(html);
        assert!(md.contains("# Hello"));
        assert!(md.contains("World & you"));
        assert!(md.contains("- one"));
        assert!(md.contains("- two"));
        assert!(md.contains("```rust"));
        assert!(md.contains("fn main() {}"));
        assert!(!md.contains("color: red"));
        assert!(!md.contains("var x"));
    }

    #[test]
    fn html_nested_list_and_quote() {
        let md = html_to_md("<blockquote><p>quoted</p></blockquote><ol><li>a</li><li>b</li></ol>");
        assert!(md.contains("> quoted"));
        assert!(md.contains("1. a"));
        assert!(md.contains("2. b"));
    }

    #[test]
    fn plain_markdown_passes_through() {
        let src = "# Title\n\n- item **bold**\n";
        assert_eq!(preprocess_markdown(src), src);
    }

    /// 抽出ヘルパー用の Tag を組み立てる (end は変換ループでのみ意味を持つ)。
    fn test_tag(name: &str, closing: bool, attrs: &str) -> Tag {
        Tag {
            name: name.to_string(),
            closing,
            attrs: attrs.to_string(),
            end: 0,
        }
    }

    #[test]
    fn extracted_heading_helper_emits_hashes() {
        let mut c = Conv::new(Mode::Html);
        c.tag_heading(&test_tag("h3", false, ""), "h3");
        assert_eq!(c.out, "### ");
    }

    #[test]
    fn extracted_list_item_helper_numbers_ordered() {
        let mut c = Conv::new(Mode::Html);
        c.lists.push(ListKind::Ol(0));
        let li = test_tag("li", false, "");
        c.tag_list_item(&li);
        c.tag_list_item(&li);
        assert_eq!(c.out, "1. \n2. ");
    }

    #[test]
    fn extracted_pre_helper_opens_fence_with_lang() {
        let mut c = Conv::new(Mode::Html);
        c.tag_pre(&test_tag("pre", false, r#"class="language-rust""#));
        assert!(c.pre);
        assert!(!c.pre_lang_pending);
        assert_eq!(c.out, "```rust\n");
    }

    // ─── タグ別の変換表 ─────────────────────────────────────────────

    /// Markdown 中に埋め込まれた HTML タグを 1 つずつ確認する。
    /// (入力, 出力に含まれるべき文字列, 出力に含まれてはいけない文字列)
    #[test]
    fn embedded_html_tag_table() {
        let cases: &[(&str, &[&str], &[&str])] = &[
            ("<p>段落</p>", &["段落"], &["<p>", "</p>"]),
            ("<div>ブロック</div>", &["ブロック"], &["<div>"]),
            ("<b>太字</b>", &["**太字**"], &["<b>"]),
            ("<strong>強</strong>", &["**強**"], &["<strong>"]),
            ("<i>斜</i>", &["*斜*"], &["<i>"]),
            ("<em>強調</em>", &["*強調*"], &["<em>"]),
            ("<code>x=1</code>", &["`x=1`"], &["<code>"]),
            ("<del>消</del>", &["~~消~~"], &["<del>"]),
            ("行1<br>行2", &["行1", "行2"], &["<br>"]),
            ("<hr>", &["---"], &["<hr>"]),
            ("<h1>見出し</h1>", &["# 見出し"], &["<h1>"]),
            ("<h6>小</h6>", &["###### 小"], &["<h6>"]),
            ("<blockquote>引用</blockquote>", &["> 引用"], &["<blockquote>"]),
            ("<ul><li>一</li><li>二</li></ul>", &["- 一", "- 二"], &["<li>"]),
            ("<ol><li>甲</li><li>乙</li></ol>", &["1. 甲", "2. 乙"], &["<ol>"]),
            (
                r#"<a href="https://a.b">サイト</a>"#,
                &["[サイト](https://a.b)"],
                &["<a href"],
            ),
            (
                r#"<img src="a.png" alt="図">"#,
                &["![図](a.png)"],
                &["<img"],
            ),
            (
                "<details><summary>開く</summary>中身</details>",
                &["▶ **開く**", "中身"],
                &["<details>", "<summary>"],
            ),
            (
                "<table><tr><th>A</th></tr><tr><td>1</td></tr></table>",
                &["| A |", "| --- |", "| 1 |"],
                &["<table>", "<td>"],
            ),
            (
                "<pre><code>let x;</code></pre>",
                &["```", "let x;"],
                &["<pre>", "<code>"],
            ),
            (
                r#"<span style="font-weight:bold">濃い</span>"#,
                &["**濃い**"],
                &["<span"],
            ),
            (
                r#"<span style="font-style:italic">斜め</span>"#,
                &["*斜め*"],
                &["<span"],
            ),
            (r#"<span class="x">素</span>"#, &["素"], &["<span", "class"]),
            ("<dl><dt>語</dt><dd>説明</dd></dl>", &["**語**", "説明"], &["<dl>"]),
            ("<sup>2</sup>", &["^(2)"], &["<sup>"]),
            ("<sub>i</sub>", &["_(i)"], &["<sub>"]),
            (
                r#"<video src="v.mp4"></video>"#,
                &["🎬", "(v.mp4)"],
                &["<video"],
            ),
            (
                r#"<iframe src="https://a.b/e"></iframe>"#,
                &["https://a.b/e"],
                &["<iframe"],
            ),
            // 未知の (=対応していない) タグは中身のテキストだけ残す
            ("<marquee>流れる</marquee>", &["流れる"], &["<marquee>"]),
            ("<form><label>名前</label></form>", &["名前"], &["<form>", "<label>"]),
            ("<body><p>本文</p></body>", &["本文"], &["<body>"]),
        ];
        for (src, want, unwanted) in cases {
            let md = preprocess_markdown(src);
            for w in *want {
                assert!(md.contains(w), "{src:?} → {md:?} に {w:?} が無い");
            }
            for u in *unwanted {
                assert!(!md.contains(u), "{src:?} → {md:?} に {u:?} が残っている");
            }
        }
    }

    #[test]
    fn inline_html_inside_paragraph() {
        // 段落の途中の HTML が周囲の Markdown を壊さない
        let md = preprocess_markdown("**強調** と <b>太字</b> と *斜体*");
        assert!(md.contains("**強調**") && md.contains("**太字**") && md.contains("*斜体*"));
        let md = preprocess_markdown("テキスト<br>続き");
        assert!(md.starts_with("テキスト  \n続き"), "{md:?}");
        let md = preprocess_markdown(r#"文中に <img src="x.png" width="200"> がある"#);
        assert!(md.contains("文中に ![](x.png) がある"), "{md:?}");
        // 見出し行の中のインライン HTML
        let md = preprocess_markdown("## <code>fn</code> の話");
        assert!(md.contains("## `fn` の話"), "{md:?}");
        // リスト項目の中のインライン HTML
        let md = preprocess_markdown("- <b>項目</b> の説明");
        assert!(md.contains("- **項目** の説明"), "{md:?}");
    }

    #[test]
    fn checkbox_becomes_task_list() {
        let md = html_to_md(
            "<ul><li><input type=\"checkbox\" checked> 済</li>\
             <li><input type=\"checkbox\"> 未</li></ul>",
        );
        assert!(md.contains("- [x] 済"), "{md:?}");
        assert!(md.contains("- [ ] 未"), "{md:?}");
        // リスト外のチェックボックスは記号で表す
        let md = html_to_md("<p><input type=\"checkbox\" checked> 単体</p>");
        assert!(md.contains("☑"), "{md:?}");
    }

    // ─── 文字実体参照 ───────────────────────────────────────────────

    #[test]
    fn entity_decode_table() {
        let cases: &[(&str, &str)] = &[
            ("&amp;", "&"),
            ("&lt;", "<"),
            ("&gt;", ">"),
            ("&quot;", "\""),
            ("&#39;", "'"),
            ("&apos;", "'"),
            ("&nbsp;", "\u{00A0}"),
            ("&#x3042;", "あ"),
            ("&#X3042;", "あ"),
            ("&#12354;", "あ"),
            ("&#x1F600;", "😀"),
            ("&copy;", "©"),
            ("&mdash;", "—"),
            ("&yen;", "¥"),
            ("&alpha;", "α"),
            // 未知・不正はそのまま残す (壊さない)
            ("&notanentity;", "&notanentity;"),
            ("&#xZZZZ;", "&#xZZZZ;"),
            ("&#99999999;", "&#99999999;"),
            ("&", "&"),
            ("&;", "&;"),
            ("a & b", "a & b"),
        ];
        for (src, want) in cases {
            assert_eq!(&decode_entities(src), want, "src={src:?}");
        }
        // Markdown / HTML どちらの経路でも解決される
        assert!(preprocess_markdown("A &amp; B &#x3042;").contains("A & B あ"));
        assert!(html_to_md("<p>A &amp; B &#x3042;</p>").contains("A & B あ"));
        // インラインコード/フェンス内の実体参照は触らない
        assert!(preprocess_markdown("`&amp;`").contains("`&amp;`"));
        assert!(preprocess_markdown("```\n&amp;\n```").contains("&amp;"));
    }

    #[test]
    fn entities_in_attributes_are_decoded() {
        let md = preprocess_markdown(r#"<img src="a&amp;b.png" alt="X &amp; Y">"#);
        assert!(md.contains("![X & Y](a&b.png)"), "{md:?}");
        let md = preprocess_markdown(r#"<a href="?x=1&amp;y=2">L</a>"#);
        assert!(md.contains("[L](?x=1&y=2)"), "{md:?}");
    }

    // ─── 壊れた HTML への耐性 ───────────────────────────────────────

    #[test]
    fn malformed_html_degrades_without_panic() {
        let cases = [
            "<b>閉じ忘れ",
            "</b>いきなり閉じ",
            "<div><p>入れ子違い</div></p>",
            "<table><tr><td>閉じ忘れ",
            "<ul><li>a<li>b",
            "<a href=\"x\">リンク",
            "<span style=\"font-weight:bold\">濃い",
            "<img src=",
            "<<<>>>",
            "<b<i>x</i>",
            "< b >空白",
            "<!-- 閉じないコメント",
            "<pre>閉じない",
        ];
        for src in cases {
            let md = preprocess_markdown(src);
            let html = html_to_md(src);
            // 生のタグを丸ごと吐き出さないこと (`<b>` が残らない)
            assert!(!md.contains("<b>"), "{src:?} → {md:?}");
            assert!(!html.contains("<b>"), "{src:?} → {html:?}");
        }
        // 未閉じの装飾は変換の最後に閉じられる
        let md = preprocess_markdown(r#"<span style="font-style:italic">斜め"#);
        assert!(md.starts_with('*') && md.ends_with('*'), "{md:?}");
    }

    #[test]
    fn generics_are_not_mistaken_for_tags() {
        for src in [
            "Vec<String>",
            "Result<T, E>",
            "Vec<Option<T>>",
            "HashMap<K, V>",
            "Box<dyn Error>",
            "a < b && c > d",
        ] {
            let md = preprocess_markdown(src);
            assert_eq!(md, src, "{src:?} は原文のまま残すべき");
        }
    }

    #[test]
    fn nested_table_flattens_into_outer_cell() {
        let md = html_to_md(
            "<table><tr><td>外<table><tr><td>内</td></tr></table></td><td>右</td></tr></table>",
        );
        // 外側の表が壊れず、内側はセル内のテキストとして残る
        assert!(md.contains("外"), "{md:?}");
        assert!(md.contains("内"), "{md:?}");
        assert!(md.contains("右"), "{md:?}");
        assert!(md.lines().filter(|l| l.contains("---")).count() <= 1, "{md:?}");
    }

    #[test]
    fn table_caption_becomes_heading_line() {
        let md = html_to_md("<table><caption>売上</caption><tr><td>1</td></tr></table>");
        assert!(md.contains("**売上**"), "{md:?}");
        assert!(md.contains("| 1 |"), "{md:?}");
    }

    #[test]
    fn crlf_html_is_handled() {
        let md = html_to_md("<h1>題</h1>\r\n<p>本文</p>\r\n");
        assert!(md.contains("# 題"), "{md:?}");
        assert!(md.contains("本文"), "{md:?}");
    }

    /// 実際の README にありがちな混在文書を通しで変換し、
    /// 生のマークアップが 1 つも残らないことを確認する。
    #[test]
    fn real_world_readme_leaves_no_raw_markup() {
        let src = r#"---
title: サンプル
---

# プロジェクト <img src="logo.png" alt="ロゴ" width="20">

<p align="center">
  <a href="https://ci.example/badge"><img src="badge.svg" alt="CI"></a>
</p>

説明文です。<br>2 行目&nbsp;です。

<details>
<summary>詳しく</summary>

| 名前 | 値 |
|:-----|---:|
| a    |  1 |

- [x] 済んだ
- [ ] これから
  - ネスト

</details>

<table>
  <tr><th>キー</th><th>説明</th></tr>
  <tr><td><code>--fast</code></td><td>速くする &amp; 省メモリ</td></tr>
</table>

```rust
let v: Vec<Option<String>> = vec![];
```

普通の `Vec<String>` は壊さない。
"#;
        let md = preprocess_markdown(src);
        for raw in [
            "<p", "</p>", "<a href", "<img", "<details>", "<summary>", "<table>", "<tr>",
            "<th>", "<td>", "&nbsp;", "&amp;",
        ] {
            assert!(!md.contains(raw), "{raw:?} が残っている:\n{md}");
        }
        // 中身は保たれている
        for want in [
            "# プロジェクト",
            "![ロゴ](logo.png)",
            "[![CI](badge.svg)](https://ci.example/badge)",
            "▶ **詳しく**",
            "| 名前 | 値 |",
            "- [x] 済んだ",
            "| `--fast` |",
            "省メモリ",
        ] {
            assert!(md.contains(want), "{want:?} が無い:\n{md}");
        }
        // フェンス内と本文のジェネリクスは無傷
        assert!(md.contains("Vec<Option<String>>"), "{md}");
        assert!(md.contains("`Vec<String>`"), "{md}");
        // フロントマターはそのまま残り、描画側で畳まれる
        let (fm, _) = crate::markdown::split_front_matter(&md);
        assert_eq!(fm.map(|s| s.trim()), Some("title: サンプル"));
    }

    // ─── 巨大入力 ───────────────────────────────────────────────────

    #[test]
    fn huge_input_is_capped_with_notice() {
        let big = format!("<p>{}</p>", "あ".repeat(4_000_000)); // 12MB 超
        assert!(big.len() > 10 * 1024 * 1024);
        let md = html_to_md(&big);
        assert!(md.len() < crate::markdown::MAX_PREVIEW_BYTES + 4096, "len={}", md.len());
        assert!(md.contains("⚠") || md.contains("KB"), "{}", &md[md.len() - 200..]);
        // HTML を含まない巨大 Markdown も同じ経路で切り詰められる
        let plain = "行\n".repeat(400_000);
        let out = preprocess_markdown(&plain);
        assert!(out.len() < crate::markdown::MAX_PREVIEW_BYTES + 4096);
    }
}
