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
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => " ",
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
/// (変換器は entity() を直接使う。単体でも使えるよう公開ユーティリティとして残す)
#[allow(dead_code)]
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
        }
    }

    /// 出力先 (テーブルセル内ならセルバッファ)。
    fn sink(&mut self) -> &mut String {
        if let Some(t) = &mut self.table {
            if let Some(c) = &mut t.cur_cell {
                return c;
            }
        }
        &mut self.out
    }

    /// テーブルのセル外にいる (セル間の空白などは捨てる)。
    fn in_table_gap(&self) -> bool {
        self.table.as_ref().is_some_and(|t| t.cur_cell.is_none())
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
            | "nav" | "center" | "figure" | "figcaption" | "address" | "dl" | "dt" | "dd" => {
                self.block_break();
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
                    let href = attr(&tag.attrs, "href").filter(|h| !h.is_empty());
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
                let src = attr(&tag.attrs, "src").unwrap_or_default();
                if src.is_empty() {
                    return;
                }
                let alt = attr(&tag.attrs, "alt").unwrap_or_default();
                self.push_str(&format!("![{alt}]({src})"));
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
            "table" => {
                if tag.closing {
                    self.emit_table();
                } else {
                    self.block_break();
                    self.table = Some(TableState::default());
                }
            }
            "thead" | "tbody" | "tfoot" | "caption" | "colgroup" | "col" => {}
            "tr" => self.tag_table_row(tag),
            "th" | "td" => {
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
        if let Some(c) = t.cur_cell.take() {
            t.cur_row.push(c);
        }
        if !t.cur_row.is_empty() {
            t.rows.push(std::mem::take(&mut t.cur_row));
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
    )
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
fn convert(text: &str, mode: Mode) -> String {
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
                    && !is_known_tag(&tag.name)
                    && !matches!(
                        tag.name.as_str(),
                        "script" | "style" | "head" | "svg" | "iframe" | "canvas" | "template"
                    )
                {
                    let raw: String = chars[i..tag.end].iter().collect();
                    conv.push_str(&raw);
                    at_line_start = false;
                    i = tag.end;
                    continue;
                }
                // 中身ごと捨てるタグ
                if !tag.closing
                    && matches!(
                        tag.name.as_str(),
                        "script" | "style" | "head" | "svg" | "iframe" | "canvas" | "template"
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

    // 閉じ忘れのテーブルを流す
    conv.emit_table();
    conv.out.trim_end().to_string()
}

/// Markdown 中の埋め込み HTML を Markdown 相当へ変換する。
/// HTML を含まないテキストは (コメント除去と実体参照解決を除き) そのまま通る。
pub fn preprocess_markdown(text: &str) -> String {
    // HTML の気配が無ければ何もしない (毎フレーム呼ばれても軽いように)
    if !text.contains('<') && !text.contains('&') {
        return text.to_string();
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
}
