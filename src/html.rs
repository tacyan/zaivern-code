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
    lang == "HTML" || t.ends_with(".html") || t.ends_with(".htm") || t.ends_with(".xhtml")
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
        // match に無いものは表を引く (分音符付きラテン文字・ASCII 記号名など)
        _ => {
            return NAMED_ENTITIES
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, c)| c.to_string());
        }
    };
    Some(s.to_string())
}

/// 名前付き実体参照の続き。match 腕に並べると 150 行を超えるのでデータで持つ。
///
/// ここが欠けると `&eacute;` `&lpar;` がそのまま本文へ出る (= 生のマークアップの
/// 漏れと同じ見え方になる) ので、実際の文書で出る範囲を厚く埋めている。
const NAMED_ENTITIES: &[(&str, char)] = &[
    // ── ASCII 記号 (HTML5 が名前を付けているもの) ──
    ("excl", '!'),
    ("quest", '?'),
    ("num", '#'),
    ("dollar", '$'),
    ("percnt", '%'),
    ("ast", '*'),
    ("midast", '*'),
    ("plus", '+'),
    ("comma", ','),
    ("period", '.'),
    ("sol", '/'),
    ("bsol", '\\'),
    ("colon", ':'),
    ("semi", ';'),
    ("equals", '='),
    ("commat", '@'),
    ("lsqb", '['),
    ("lbrack", '['),
    ("rsqb", ']'),
    ("rbrack", ']'),
    ("lcub", '{'),
    ("lbrace", '{'),
    ("rcub", '}'),
    ("rbrace", '}'),
    ("lpar", '('),
    ("rpar", ')'),
    ("Hat", '^'),
    ("grave", '`'),
    ("lowbar", '_'),
    ("verbar", '|'),
    ("vert", '|'),
    ("tilde", '~'),
    ("Tab", '\t'),
    ("NewLine", '\n'),
    // ── 空白 ──
    ("NonBreakingSpace", '\u{00A0}'),
    ("numsp", '\u{2007}'),
    ("puncsp", '\u{2008}'),
    ("hairsp", '\u{200A}'),
    ("emsp13", '\u{2004}'),
    ("emsp14", '\u{2005}'),
    ("zwsp", '\u{200B}'),
    // ── ラテン文字 (Latin-1 補助 + 拡張 A の頻出分) ──
    ("Agrave", 'À'),
    ("Aacute", 'Á'),
    ("Acirc", 'Â'),
    ("Atilde", 'Ã'),
    ("Auml", 'Ä'),
    ("Aring", 'Å'),
    ("AElig", 'Æ'),
    ("Ccedil", 'Ç'),
    ("Egrave", 'È'),
    ("Eacute", 'É'),
    ("Ecirc", 'Ê'),
    ("Euml", 'Ë'),
    ("Igrave", 'Ì'),
    ("Iacute", 'Í'),
    ("Icirc", 'Î'),
    ("Iuml", 'Ï'),
    ("ETH", 'Ð'),
    ("Ntilde", 'Ñ'),
    ("Ograve", 'Ò'),
    ("Oacute", 'Ó'),
    ("Ocirc", 'Ô'),
    ("Otilde", 'Õ'),
    ("Ouml", 'Ö'),
    ("Oslash", 'Ø'),
    ("Ugrave", 'Ù'),
    ("Uacute", 'Ú'),
    ("Ucirc", 'Û'),
    ("Uuml", 'Ü'),
    ("Yacute", 'Ý'),
    ("THORN", 'Þ'),
    ("szlig", 'ß'),
    ("agrave", 'à'),
    ("aacute", 'á'),
    ("acirc", 'â'),
    ("atilde", 'ã'),
    ("auml", 'ä'),
    ("aring", 'å'),
    ("aelig", 'æ'),
    ("ccedil", 'ç'),
    ("egrave", 'è'),
    ("eacute", 'é'),
    ("ecirc", 'ê'),
    ("euml", 'ë'),
    ("igrave", 'ì'),
    ("iacute", 'í'),
    ("icirc", 'î'),
    ("iuml", 'ï'),
    ("eth", 'ð'),
    ("ntilde", 'ñ'),
    ("ograve", 'ò'),
    ("oacute", 'ó'),
    ("ocirc", 'ô'),
    ("otilde", 'õ'),
    ("ouml", 'ö'),
    ("oslash", 'ø'),
    ("ugrave", 'ù'),
    ("uacute", 'ú'),
    ("ucirc", 'û'),
    ("uuml", 'ü'),
    ("yacute", 'ý'),
    ("thorn", 'þ'),
    ("yuml", 'ÿ'),
    ("OElig", 'Œ'),
    ("oelig", 'œ'),
    ("Scaron", 'Š'),
    ("scaron", 'š'),
    ("Yuml", 'Ÿ'),
    ("fnof", 'ƒ'),
    ("circ", 'ˆ'),
    // ── ギリシャ文字の残り ──
    ("Epsilon", 'Ε'),
    ("Zeta", 'Ζ'),
    ("Eta", 'Η'),
    ("Theta", 'Θ'),
    ("Iota", 'Ι'),
    ("Kappa", 'Κ'),
    ("Mu", 'Μ'),
    ("Nu", 'Ν'),
    ("Xi", 'Ξ'),
    ("Omicron", 'Ο'),
    ("Rho", 'Ρ'),
    ("Tau", 'Τ'),
    ("Upsilon", 'Υ'),
    ("Chi", 'Χ'),
    ("Psi", 'Ψ'),
    ("zeta", 'ζ'),
    ("eta", 'η'),
    ("iota", 'ι'),
    ("kappa", 'κ'),
    ("nu", 'ν'),
    ("xi", 'ξ'),
    ("omicron", 'ο'),
    ("rho", 'ρ'),
    ("sigmaf", 'ς'),
    ("upsilon", 'υ'),
    ("chi", 'χ'),
    ("psi", 'ψ'),
    ("thetasym", 'ϑ'),
    ("upsih", 'ϒ'),
    ("piv", 'ϖ'),
    // ── 記号・矢印の残り ──
    ("oplus", '⊕'),
    ("otimes", '⊗'),
    ("sdot", '⋅'),
    ("lceil", '⌈'),
    ("rceil", '⌉'),
    ("lfloor", '⌊'),
    ("rfloor", '⌋'),
    ("lang", '⟨'),
    ("rang", '⟩'),
    ("sube", '⊆'),
    ("supe", '⊇'),
    ("nsub", '⊄'),
    ("ni", '∋'),
    ("weierp", '℘'),
    ("real", 'ℜ'),
    ("image", 'ℑ'),
    ("alefsym", 'ℵ'),
    ("uArr", '⇑'),
    ("dArr", '⇓'),
    ("nwarr", '↖'),
    ("nearr", '↗'),
    ("searr", '↘'),
    ("swarr", '↙'),
    ("hearts", '♥'),
    ("diamonds", '♦'),
    ("horbar", '―'),
    ("dash", '‐'),
    ("mldr", '…'),
    ("nldr", '‥'),
    ("checkmark", '✓'),
    ("sung", '♪'),
    ("flat", '♭'),
    ("natur", '♮'),
    ("sharp", '♯'),
    ("numero", '№'),
    ("phone", '☎'),
    ("female", '♀'),
    ("male", '♂'),
];

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
    let name: String = chars[name_start..k]
        .iter()
        .collect::<String>()
        .to_lowercase();
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
            || !lower.as_bytes()[at - 1].is_ascii_alphanumeric()
                && lower.as_bytes()[at - 1] != b'-';
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
                    val.split(|c: char| c.is_whitespace() || c == '>')
                        .next()
                        .unwrap_or("")
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

/// `colspan="2"` のような桁数属性を読む。壊れた値・0 は 1、上限は `MAX_SPAN`。
fn attr_span(attrs: &str, name: &str) -> usize {
    attr(attrs, name)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n >= 1)
        .map_or(1, |n| n.min(MAX_SPAN))
}

/// `src` / `srcset="a.png 1x, b.png 2x"` から先頭の URL を取り出す。
fn first_src(attrs: &str) -> Option<String> {
    attr_text(attrs, "src")
        .filter(|s| !s.trim().is_empty())
        .or_else(|| attr_text(attrs, "data-src"))
        .or_else(|| {
            attr_text(attrs, "srcset").and_then(|s| {
                s.split(',')
                    .next()
                    .and_then(|p| p.split_whitespace().next())
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
            })
        })
        .filter(|s| !s.trim().is_empty())
}

/// Markdown のリンク先として安全な形へ整える。
///
/// `markdown::read_link` は `(` の次から**最初の `)`** までを URL と読むので、
/// 丸括弧を含む URL (Wikipedia の `Foo_(bar)` 等) や空白入りの相対パスは
/// そのまま書くとリンクが途中で切れる。パーセント符号化すれば同じ URL のまま
/// 記法を壊さない。
fn safe_url(u: &str) -> String {
    let mut out = String::with_capacity(u.len());
    for c in u.trim().chars() {
        match c {
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            ' ' => out.push_str("%20"),
            // 改行はリンク記法を必ず壊すので落とす
            '\n' | '\r' | '\t' => {}
            _ => out.push(c),
        }
    }
    out
}

/// 画像の代替文字列を Markdown の `![...]` に収まる形へ整える。
fn safe_alt(alt: &str) -> String {
    alt.replace('[', "(")
        .replace(']', ")")
        .replace(['\n', '\r'], " ")
}

/// 描けない図版 (svg / canvas) に添える説明。取れないときは None (何も出さない)。
///
/// `aria-label` / `title` 属性か、中の `<title>` を使う。README のインライン
/// アイコンには説明が無いことが多く、そこへ一律にプレースホルダを置くと
/// 本文が「🖼」だらけになるので、**名前が分かるときだけ**残す。
fn media_label(region: &[char], attrs: &str) -> Option<String> {
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    for key in ["aria-label", "title"] {
        if let Some(l) = attr_text(attrs, key) {
            let l = squash(&l);
            if !l.is_empty() {
                return Some(l);
            }
        }
    }
    let text: String = region.iter().collect();
    let lower = text.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let gt = text[open..].find('>')? + open + 1;
    let close = lower.get(gt..)?.find("</title")? + gt;
    let inner = squash(&decode_entities(text.get(gt..close)?));
    (!inner.is_empty()).then_some(inner)
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

/// `colspan` / `rowspan` として受け付ける上限。
/// `colspan="99999"` のような壊れた/悪意ある値で行が膨らまないように。
const MAX_SPAN: usize = 64;

/// 1 つのテーブルで作る桁数の上限。
/// **中身のあるセルは必ず出す**ので、この上限が削るのは span 由来の空欄だけ。
const MAX_TABLE_COLS: usize = 1024;

/// 溜めているテーブル 1 つ。
///
/// `<th>` かどうかは持たない — Markdown のテーブルはヘッダ行が必須なので
/// `emit_table` は **`<th>` の有無に関わらず 1 行目をヘッダとして扱う**。
/// 覚えても出力が変わらない値は持たない (書くだけの状態は嘘の情報源になる)。
#[derive(Default)]
struct TableState {
    rows: Vec<Vec<String>>,
    cur_row: Vec<String>,
    cur_cell: Option<String>,
    /// いま開いているセルの (colspan, rowspan)
    cur_span: (usize, usize),
    /// いま開いているセルの左端の桁
    cur_col: usize,
    /// 桁ごとの rowspan の残り (この行を含む数)
    rowspan_left: Vec<usize>,
    /// `<caption>` の本文 (テーブルの直前に見出しとして出す)
    caption: String,
    in_caption: bool,
}

impl TableState {
    /// 上の行から rowspan で伸びてきている桁を空欄で飛ばす。
    /// これをしないと、結合セルのある表で以降のセルが**左へ詰まって**
    /// 見出しと中身の対応が 1 桁ずつずれる。
    fn skip_spanned(&mut self) {
        while self.rowspan_left.get(self.cur_col).copied().unwrap_or(0) > 0 {
            if self.cur_row.len() >= MAX_TABLE_COLS {
                return;
            }
            self.cur_row.push(String::new());
            self.cur_col += 1;
        }
    }

    /// 開いているセルを確定して行へ置く (colspan の分だけ桁を埋める)。
    fn finish_cell(&mut self) {
        let Some(text) = self.cur_cell.take() else {
            return;
        };
        let (cs, rs) = self.cur_span;
        while self.cur_row.len() < self.cur_col.min(MAX_TABLE_COLS) {
            self.cur_row.push(String::new());
        }
        self.cur_row.push(text);
        for _ in 1..cs {
            if self.cur_row.len() >= MAX_TABLE_COLS {
                break;
            }
            self.cur_row.push(String::new());
        }
        if rs > 1 {
            let end = (self.cur_col + cs).min(MAX_TABLE_COLS);
            if self.rowspan_left.len() < end {
                self.rowspan_left.resize(end, 0);
            }
            for c in self.cur_col.min(end)..end {
                self.rowspan_left[c] = rs;
            }
        }
        self.cur_col = (self.cur_col + cs).min(MAX_TABLE_COLS);
        self.skip_spanned();
    }

    /// 行の開始。上から伸びている結合セルの分だけ最初から桁を空ける。
    fn begin_row(&mut self) {
        self.cur_row.clear();
        self.cur_cell = None;
        self.cur_col = 0;
        self.skip_spanned();
    }

    /// 行の終了。rowspan の残りを 1 行分減らす。
    fn end_row(&mut self) {
        self.finish_cell();
        if !self.cur_row.is_empty() {
            self.rows.push(std::mem::take(&mut self.cur_row));
        }
        for v in &mut self.rowspan_left {
            *v = v.saturating_sub(1);
        }
    }
}

/// 開いている装飾 1 つ。
///
/// 開き記号を出した長さと位置を控えておくと「中身が 1 文字も無かった」が
/// 判定できる。`<b></b>` が `****` という literal になるのを防ぐため。
struct OpenSpan {
    /// 同じ種類の閉じタグを探すための鍵
    key: &'static str,
    /// 出力済みの開き記号の長さ (記号を出していなければ 0)
    open_len: usize,
    /// 閉じたときに出す文字列
    close: String,
    /// 開き記号を出した直後の sink 長
    at: usize,
}

struct Conv {
    mode: Mode,
    out: String,
    lists: Vec<ListKind>,
    quote_depth: usize,
    links: Vec<Option<String>>,
    pre: bool,
    pre_lang_pending: bool,
    /// 開いている `<pre>` のフェンス開始位置 (sink 上のバイト長)
    pre_start: usize,
    table: Option<TableState>,
    /// `<table>` の入れ子の深さ (内側のテーブルは外側のセルへ平坦化する)
    table_depth: usize,
    /// 開いた装飾のスタック
    spans: Vec<OpenSpan>,
    /// 開いているインラインコードの開始位置 (sink 上のバイト長)
    code_open: Vec<usize>,
    /// `<picture>` の深さと、その中で最初に見た `<source>` の URL
    picture_depth: usize,
    picture_src: Option<String>,
    picture_img: bool,
    /// `<video>`/`<audio>` が src 無しで開いていて、子の `<source>` を待っている
    media_pending: Option<&'static str>,
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
            pre_start: 0,
            table: None,
            table_depth: 0,
            spans: Vec::new(),
            code_open: Vec::new(),
            picture_depth: 0,
            picture_src: None,
            picture_img: false,
            media_pending: None,
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

    /// 本文テキストを 1 文字書く (タグが出す記法とは別経路)。
    ///
    /// リンクの中では `[` `]` を丸括弧へ落とす。`markdown::read_link` は
    /// **最初の `]`** でリンク文字列を切るので、本文に `]` があると
    /// リンクがそこで壊れて残りが生の記法として出てしまう。
    /// タグ側が出す `![alt](src)` を巻き込まないよう、この経路だけで直す。
    fn push_text(&mut self, c: char) {
        if self.links.last().is_some_and(Option::is_some) {
            match c {
                '[' => return self.push_char('('),
                ']' => return self.push_char(')'),
                _ => {}
            }
        }
        self.push_char(c);
    }

    fn push_text_str(&mut self, t: &str) {
        for c in t.chars() {
            self.push_text(c);
        }
    }

    /// テーブルのセルの中にいる。
    fn in_table_cell(&self) -> bool {
        self.table.as_ref().is_some_and(|t| t.cur_cell.is_some())
    }

    /// 装飾を開く。閉じるときに「中身が空だったか」を判定できるよう長さを控える。
    fn open_span(&mut self, key: &'static str, open: &str, close: &str) {
        let before = self.sink().len();
        self.push_str(open);
        let at = self.sink().len();
        self.spans.push(OpenSpan {
            key,
            open_len: at.saturating_sub(before),
            close: close.to_string(),
            at,
        });
    }

    /// 装飾を閉じる。同じ鍵で開いた最も内側の要素まで巻き戻す。
    ///
    /// `<b><i>x</b></i>` のような交差した閉じタグでも、間に挟まった装飾を
    /// 先に閉じることで記号の対応が崩れない。開いていない閉じタグは
    /// 記号だけ出す (従来どおり)。
    fn close_span(&mut self, key: &'static str, fallback: &str) {
        let Some(k) = self.spans.iter().rposition(|s| s.key == key) else {
            self.push_str(fallback);
            return;
        };
        while self.spans.len() > k {
            let Some(sp) = self.spans.pop() else { return };
            self.finish_span(sp);
        }
    }

    /// 開いた装飾 1 つを閉じる。中身が空白だけなら**開き記号だけ**取り消す。
    ///
    /// `<b></b>` を素直に閉じると `****` という literal がプレビューに出る。
    /// 中身の空白は消さない (`a<b> </b>c` を `ac` に詰めないため)。
    fn finish_span(&mut self, sp: OpenSpan) {
        let has_body = {
            let s = self.sink();
            sp.at > s.len() || !s[sp.at..].trim().is_empty()
        };
        if has_body {
            self.push_str(&sp.close);
            return;
        }
        let from = sp.at - sp.open_len;
        self.sink().replace_range(from..sp.at, "");
    }

    /// 開閉どちらかの装飾タグを処理する。
    fn emphasis(&mut self, tag: &Tag, key: &'static str, mark: &str) {
        if tag.closing {
            self.close_span(key, mark);
        } else {
            self.open_span(key, mark, mark);
        }
    }

    /// ハード改行 (行末スペース 2 個)。段落は変えずに行だけ分ける。
    fn hard_break(&mut self) {
        let s = self.sink();
        if s.is_empty() || s.ends_with('\n') {
            return;
        }
        while s.ends_with(' ') || s.ends_with('\t') {
            s.pop();
        }
        s.push_str("  \n");
    }

    /// インラインコードを開く。
    fn open_code(&mut self) {
        let at = self.sink().len();
        self.push_str("`");
        self.code_open.push(at);
    }

    /// インラインコードを閉じる。
    ///
    /// 中身に `` ` `` があれば区切りを 1 つ長くする (markdown 側は連長を
    /// 数えて対応する)。中身の改行は空白へ潰す — Markdown のインラインコードは
    /// 行をまたげないので、放置すると `` ` `` が本文へ生のまま残る。
    fn close_code(&mut self) {
        let Some(at) = self.code_open.pop() else {
            self.push_str("`");
            return;
        };
        let s = self.sink();
        let Some(tick) = s.get(at..).and_then(|r| r.find('`')) else {
            s.push('`');
            return;
        };
        let open_at = at + tick;
        let body = s[open_at + 1..].replace(['\n', '\r'], " ");
        if body.trim().is_empty() {
            s.truncate(open_at);
            return;
        }
        let (mut run, mut max_run) = (0usize, 0usize);
        for c in body.chars() {
            run = if c == '`' { run + 1 } else { 0 };
            max_run = max_run.max(run);
        }
        let delim = "`".repeat(max_run + 1);
        // 先頭/末尾が ` のときは空白で挟む (CommonMark は 1 個だけ剥がす)
        let pad = if body.starts_with('`') || body.ends_with('`') {
            " "
        } else {
            ""
        };
        s.truncate(open_at);
        s.push_str(&delim);
        s.push_str(pad);
        s.push_str(&body);
        s.push_str(pad);
        s.push_str(&delim);
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

    /// ブロック要素の境界。**リスト項目の中では空行を作らない。**
    ///
    /// `<li><p>本文</p></li>` は実世界の HTML でごく普通に出るが、そこで
    /// 空行を入れるとリストが分断され「マーカーだけの空項目」が生まれる。
    /// 項目の中では代わりにハード改行 + インデントで続きにする。
    fn block_boundary(&mut self) {
        if self.lists.is_empty() || self.in_table_cell() {
            self.block_break();
            return;
        }
        let indent = "  ".repeat(self.lists.len());
        let quote = self.quote_depth;
        let s = self.sink();
        // マーカー直後なら足すものは無い
        if s.is_empty() || s.ends_with("- ") || s.ends_with(". ") || s.ends_with("• ") {
            return;
        }
        // 前のブロックが残した行末のインデントを落としてから判断する。
        // 落とさずに改行を足すと「空白だけの行」= 空行になってリストが切れる
        while s.ends_with(' ') || s.ends_with('\t') {
            s.pop();
        }
        if !s.ends_with('\n') {
            s.push_str("  \n");
        }
        for _ in 0..quote {
            s.push_str("> ");
        }
        s.push_str(&indent);
    }

    /// タグ 1 個を処理する。
    fn tag(&mut self, tag: &Tag) {
        let name = tag.name.as_str();
        match name {
            // 強調系は開閉をスタックで対応させる (空要素・交差した閉じタグの修復)
            "b" | "strong" => self.emphasis(tag, "strong", "**"),
            "i" | "em" | "cite" | "var" | "dfn" => self.emphasis(tag, "em", "*"),
            "s" | "del" | "strike" => self.emphasis(tag, "del", "~~"),
            // Markdown に強調表示の記法は無いので太字で代用する
            "mark" => self.emphasis(tag, "strong", "**"),
            "code" if self.pre => {
                // <pre><code class="language-x"> の言語はフェンス開始時に処理済み
            }
            "code" | "kbd" | "samp" | "tt" => {
                if tag.closing {
                    self.close_code();
                } else {
                    self.open_code();
                }
            }
            "br" => {
                if self.in_table_cell() {
                    // GFM の表のセルは改行を持てない (markdown 側もセル内は 1 行)。
                    // 生の <br> を残すと本文にマークアップが出るので空白へ落とす
                    self.push_char(' ');
                } else if self.table.is_some() {
                    // セルの外 (行間) の <br> は捨てる
                } else if !tag.closing {
                    // Markdown のハード改行 (行末スペース 2 個)
                    self.sink().push_str("  ");
                    self.newline();
                }
            }
            "hr" => {
                if tag.closing || self.in_table_cell() {
                    // セル内の `---` は区切り行と読み違えられるので出さない
                    return;
                }
                self.block_break();
                self.push_str("---");
                self.block_break();
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.tag_heading(tag, name),
            "p" | "div" | "section" | "article" | "main" | "header" | "footer" | "aside"
            | "nav" | "center" | "figure" | "address" | "dl" | "form" | "fieldset" | "hgroup"
            | "dialog" | "search" | "noscript" | "html" | "body" => {
                self.block_boundary();
            }
            // 図版のキャプションは本文と区別が付くよう斜体にする
            "figcaption" => {
                if tag.closing {
                    self.close_span("figcaption", "*");
                    self.block_boundary();
                } else {
                    self.block_boundary();
                    self.open_span("figcaption", "*", "*");
                }
            }
            // <select> の選択肢は 1 行ずつに割る (地の文へ繋がると読めない)
            "option" => {
                if !tag.closing {
                    self.newline();
                }
            }
            // 定義リスト: 用語は太字、説明は 1 段下げ。
            // 段落を分けずに行だけ分けたいのでハード改行 (行末スペース 2 個) を使う
            "dt" => {
                if tag.closing {
                    self.close_span("dt", "**");
                    self.hard_break();
                } else {
                    self.block_boundary();
                    self.open_span("dt", "**", "**");
                }
            }
            "dd" => {
                if tag.closing {
                    self.hard_break();
                } else {
                    self.hard_break();
                    let quote = self.quote_depth;
                    let s = self.sink();
                    for _ in 0..quote {
                        s.push_str("> ");
                    }
                    s.push_str("  ");
                }
            }
            // 引用符付きの短い引用
            "q" => {
                if tag.closing {
                    self.close_span("q", "\u{201D}");
                } else {
                    self.open_span("q", "\u{201C}", "\u{201D}");
                }
            }
            // 略語は展開を丸括弧で添える (title を落とすと情報が消える)
            "abbr" | "acronym" => {
                if tag.closing {
                    self.close_span("abbr", "");
                } else {
                    let title = attr_text(&tag.attrs, "title")
                        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
                        .filter(|t| !t.is_empty());
                    let close = title.map_or(String::new(), |t| format!(" ({t})"));
                    self.open_span("abbr", "", &close);
                }
            }
            // インライン装飾のうち Markdown に無いものは素通し (中身は残る)。
            // style 属性に太字/斜体/打消しが書かれていれば拾う
            "span" | "font" | "small" | "u" | "ins" | "bdi" | "bdo" | "time" | "data"
            | "output" | "ruby" | "rt" | "rp" | "big" | "label" | "legend" | "button" => {
                if tag.closing {
                    self.close_span("style", "");
                } else {
                    let (open, close) = style_marks(&tag.attrs);
                    self.open_span("style", &open, &close);
                }
            }
            "sup" => self.push_str(if tag.closing { ")" } else { "^(" }),
            "sub" => self.push_str(if tag.closing { ")" } else { "_(" }),
            // チェックボックスはタスクリスト記法へ (li 直後なら `- [x]` になる)
            "input" => {
                if tag.closing {
                    return;
                }
                let ty = attr(&tag.attrs, "type")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if ty == "checkbox" || ty == "radio" {
                    let done = attr(&tag.attrs, "checked").is_some()
                        || tag
                            .attrs
                            .to_ascii_lowercase()
                            .split_whitespace()
                            .any(|w| w == "checked");
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
                let icon = match name {
                    "audio" => "🔊",
                    "video" => "🎬",
                    _ => "🔗",
                };
                if tag.closing {
                    self.media_pending = None;
                    return;
                }
                match attr_text(&tag.attrs, "src")
                    .or_else(|| attr_text(&tag.attrs, "data"))
                    .filter(|s| !s.trim().is_empty())
                {
                    Some(src) => self.push_media(icon, &src),
                    // <video><source src=…> 形式。子の <source> を待つ
                    None if matches!(name, "video" | "audio") => {
                        self.media_pending = Some(icon);
                    }
                    None => {}
                }
            }
            // <picture> の中では <img> が主役。<img> が無い (= <source> だけの)
            // 構成でも画像が消えないよう、最初の srcset を控えておく
            "picture" => {
                if tag.closing {
                    self.picture_depth = self.picture_depth.saturating_sub(1);
                    if self.picture_depth == 0 {
                        if !self.picture_img {
                            if let Some(src) = self.picture_src.take() {
                                self.push_str(&format!("![]({})", safe_url(&src)));
                            }
                        }
                        self.picture_src = None;
                        self.picture_img = false;
                    }
                } else {
                    self.picture_depth += 1;
                    self.picture_src = None;
                    self.picture_img = false;
                }
            }
            "source" => {
                if tag.closing {
                    return;
                }
                let Some(src) = first_src(&tag.attrs) else {
                    return;
                };
                if self.picture_depth > 0 {
                    if self.picture_src.is_none() {
                        self.picture_src = Some(src);
                    }
                } else if let Some(icon) = self.media_pending {
                    self.push_media(icon, &src);
                }
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
                    // `javascript:` 等の実行系スキームはリンクにしない (中身は残る)
                    let href = attr_text(&tag.attrs, "href")
                        .map(|h| h.trim().to_string())
                        .filter(|h| !h.is_empty() && !is_script_url(h))
                        .map(|h| safe_url(&h));
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
                let Some(src) = first_src(&tag.attrs) else {
                    return;
                };
                if self.picture_depth > 0 {
                    self.picture_img = true;
                }
                let alt = attr_text(&tag.attrs, "alt")
                    .or_else(|| attr_text(&tag.attrs, "title"))
                    .unwrap_or_default();
                // alt の `[` `]` は画像記法を壊すので丸括弧へ置き換える
                self.push_str(&format!("![{}]({})", safe_alt(&alt), safe_url(&src)));
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
                let span = (
                    attr_span(&tag.attrs, "colspan"),
                    attr_span(&tag.attrs, "rowspan"),
                );
                if let Some(t) = &mut self.table {
                    t.finish_cell();
                    if !tag.closing {
                        t.cur_cell = Some(String::new());
                        t.cur_span = span;
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
        // 表のセルの中は 1 行しか持てないので、箇条書きは中黒で区切る
        // (改行を空白へ潰すと項目の切れ目が消えて読めなくなる)
        if self.in_table_cell() {
            let s = self.sink();
            if !s.is_empty() && !s.ends_with(' ') {
                s.push(' ');
            }
            s.push_str("• ");
            return;
        }
        // 直前の項目内ブロックが残したインデントを落とす。残したまま改行すると
        // 空白だけの行 (= 空行) が入ってリストが 2 つに割れる。
        //
        // **落とすのは「空白だけの行」全体だけ。** 行末の空白を無条件に削ると
        // 中身が空の項目のマーカー (`1. `) の空白まで食う
        {
            let s = self.sink();
            let line_start = s.rfind('\n').map_or(0, |p| p + 1);
            if s[line_start..].trim().is_empty() {
                s.truncate(line_start);
            }
            if !s.is_empty() && !s.ends_with('\n') {
                s.push('\n');
            }
        }
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
                t.end_row();
            } else {
                t.begin_row();
            }
        }
    }

    /// `tag()` から抽出: `<pre>` の開閉 (コードフェンスの開始/終了) を処理する。
    fn tag_pre(&mut self, tag: &Tag) {
        if tag.closing {
            self.pre = false;
            self.pre_lang_pending = false;
            let start = self.pre_start;
            let s = self.sink();
            let marker = pick_fence(s, start);
            if marker != "```" && s.len() >= start + 3 {
                s.replace_range(start..start + 3, marker);
            }
            if !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str(marker);
            self.block_break();
        } else {
            self.block_break();
            self.pre = true;
            // 言語は直後の <code class="language-x"> から拾う
            self.pre_lang_pending = true;
            self.pre_start = self.sink().len();
            self.sink().push_str("```");
            // <pre class="language-x"> にも対応
            if let Some(lang) = fence_lang(&tag.attrs) {
                self.sink().push_str(&lang);
                self.pre_lang_pending = false;
            }
            self.sink().push('\n');
        }
    }

    /// 再生できないメディアを「何があるか」の分かるリンクにする。
    fn push_media(&mut self, icon: &str, src: &str) {
        self.media_pending = None;
        let src = src.trim();
        if src.is_empty() || is_script_url(src) {
            return;
        }
        // 表示側の文字列も記法を壊さない形へ (`]` があるとリンクが切れる)
        self.push_str(&format!("[{icon} {}]({})", safe_alt(src), safe_url(src)));
    }

    /// 溜めたテーブルを Markdown テーブルとして書き出す。
    fn emit_table(&mut self) {
        let Some(mut t) = self.table.take() else {
            return;
        };
        t.in_caption = false;
        // 閉じ忘れの行/セルも流す (壊れた HTML でも表を落とさない)
        t.finish_cell();
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
        let clean = |s: &str| s.replace('\n', " ").replace('|', "\\|").trim().to_string();
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

/// `<pre>` の中身に合わせてフェンス記号を選ぶ。
///
/// markdown 側のフェンスは「同じ記号で始まる行」で閉じるので、中身に
/// ` ``` ` で始まる行があると**そこでコードブロックが切れ**、残りが本文として
/// 描かれてしまう (Markdown の説明ページで必ず起きる)。その場合だけ `~~~` を
/// 使う。両方入っているときは打つ手が無いので ` ``` ` のまま (壊れるが落ちない)。
///
/// `s[start..]` は `<pre>` が書き出したフェンス行から末尾まで。
fn pick_fence(s: &str, start: usize) -> &'static str {
    let Some(region) = s.get(start..) else {
        return "```";
    };
    if !region.starts_with("```") {
        return "```";
    }
    // 1 行目は自分が書いた開きフェンスなので飛ばす
    let body_lines = || region.split('\n').skip(1);
    let has_back = body_lines().any(|l| l.trim_start().starts_with("```"));
    let has_tilde = body_lines().any(|l| l.trim_start().starts_with("~~~"));
    if has_back && !has_tilde {
        "~~~"
    } else {
        "```"
    }
}

/// リンク先として開いてはいけない (実行系の) スキームか。
fn is_script_url(u: &str) -> bool {
    let head: String = u
        .trim_start()
        .chars()
        .take_while(|c| *c != ':' && !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(head.as_str(), "javascript" | "vbscript")
}

/// class 属性から `language-x` / `lang-x` を取り出す。
fn fence_lang(attrs: &str) -> Option<String> {
    let class = attr(attrs, "class")?;
    class
        .split_whitespace()
        .find_map(|c| {
            c.strip_prefix("language-")
                .or_else(|| c.strip_prefix("lang-"))
        })
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
            // フォーム/旧世代の要素。ここに無いと Markdown モードで
            // `<option>` などが生のまま本文へ出る。
            //
            // **ここへ足す名前は「ジェネリクスの型名として書かれうるか」で選ぶ。**
            // `Map` / `Element` / `Content` / `Command` / `Frame` / `Dir` は
            // `Array<Map>` のような書き方が普通にあり、タグ扱いにすると
            // **本文からその語が消える**ので、あえて入れていない
            // (`Option` は Rust では必ず `Option<T>` と書かれ、属性部に `<` が
            //  入るので `looks_like_markup` 側で弾かれる)
            | "option" | "xmp" | "menuitem" | "frameset" | "applet" | "keygen"
            | "listing" | "plaintext" | "spacer" | "bgsound" | "isindex" | "multicol"
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
            // 数式区切りの中も原文コピー。`$a < b$` の `<` を勝手にタグと読んだり
            // `a & b` を実体参照として潰したりしないため。
            // 誤検出で本文を丸ごと飲み込まないよう、走査は行内に限る。
            if c == '$' || c == '\\' {
                let eol = chars[i..]
                    .iter()
                    .position(|&x| x == '\n')
                    .map_or(chars.len(), |k| i + k);
                if let Some((_, _, end)) = crate::markdown::math::read_at(&chars[..eol], i) {
                    conv.push_char(c);
                    for &cc in &chars[i + 1..end] {
                        conv.sink().push(cc);
                    }
                    at_line_start = false;
                    i = end;
                    continue;
                }
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
                // `<!DOCTYPE …>` / `<![CDATA[…]]>` などの宣言。
                // Markdown モードでも捨てる — `<!` の次が英字か `[` である形は
                // ジェネリクスにはならないので誤爆せず、残すと宣言行が
                // そのまま本文に出る (README の先頭で実際に起きる)
                let decl = chars
                    .get(i + 2)
                    .is_some_and(|c| c.is_ascii_alphabetic() || *c == '[');
                if mode == Mode::Html || decl {
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
                    let end = skip_until_close(&chars, tag.end, &tag.name);
                    // 描けない図版は、名前が分かるときだけ説明を残す。
                    // README のインラインアイコンには説明が無いことが多く、
                    // 一律にプレースホルダを置くと本文が記号だらけになる
                    if matches!(tag.name.as_str(), "svg" | "canvas") {
                        if let Some(label) = media_label(&chars[i..end], &tag.attrs) {
                            conv.push_str(&format!("🖼 {label}"));
                        }
                    }
                    i = end;
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
                    if conv.pre {
                        conv.sink().push_str(&rep);
                    } else {
                        conv.push_text_str(&rep);
                    }
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
            conv.push_text(c);
        }
        at_line_start = c == '\n';
        i += 1;
    }

    // 閉じ忘れを全部流す (壊れた HTML でも出力が途切れないように)
    while let Some(sp) = conv.spans.pop() {
        conv.finish_span(sp);
    }
    while !conv.code_open.is_empty() {
        conv.close_code();
    }
    while let Some(href) = conv.links.pop() {
        if let Some(h) = href {
            conv.push_str(&format!("]({h})"));
        }
    }
    if conv.pre {
        let fake = Tag {
            name: "pre".to_string(),
            closing: true,
            attrs: String::new(),
            end: 0,
        };
        conv.tag_pre(&fake);
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

    /// 数式と mermaid フェンスは HTML 前処理を素通りしなければならない。
    /// ここが壊れると、プレビュー側がどれだけ頑張っても式が読めなくなる。
    #[test]
    fn math_and_mermaid_survive_preprocess() {
        for src in [
            "行内の $x^2 + y^2 = z^2$ です",
            "不等式 $a < b$ と $x > y$",
            "行列 $\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}$",
            "別行立て\n\n$$\\frac{1}{2}$$\n",
            "括弧形式 \\(x < y\\) と \\[a & b\\]",
            "```mermaid\ngraph TD\n  A[<b>太字</b>] --> B & C\n```",
            "```mermaid\nsequenceDiagram\n  A->>B: a & b\n```",
        ] {
            assert_eq!(preprocess_markdown(src), src, "前処理で書き換わった: {src}");
        }
    }

    /// 数式の保護が本文の HTML 変換を止めてしまわないこと。
    #[test]
    fn math_protection_does_not_swallow_the_document() {
        // 閉じない `$` の後ろでも HTML はきちんと変換される
        let out = preprocess_markdown("値段は $5 です\n<b>太字</b>\n");
        assert!(out.contains("**太字**"), "{out}");
        // 通貨表記は数式にならないのでそのまま
        assert!(out.contains("$5"), "{out}");
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
        let md = preprocess_markdown(
            r#"<p align="center"><img src="logo.png" alt="Logo" width="200"></p>"#,
        );
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
            (
                "<blockquote>引用</blockquote>",
                &["> 引用"],
                &["<blockquote>"],
            ),
            (
                "<ul><li>一</li><li>二</li></ul>",
                &["- 一", "- 二"],
                &["<li>"],
            ),
            (
                "<ol><li>甲</li><li>乙</li></ol>",
                &["1. 甲", "2. 乙"],
                &["<ol>"],
            ),
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
            (
                "<dl><dt>語</dt><dd>説明</dd></dl>",
                &["**語**", "説明"],
                &["<dl>"],
            ),
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
            (
                "<form><label>名前</label></form>",
                &["名前"],
                &["<form>", "<label>"],
            ),
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
        assert!(
            md.lines().filter(|l| l.contains("---")).count() <= 1,
            "{md:?}"
        );
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
            "<p",
            "</p>",
            "<a href",
            "<img",
            "<details>",
            "<summary>",
            "<table>",
            "<tr>",
            "<th>",
            "<td>",
            "&nbsp;",
            "&amp;",
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

    // ─── 生のマークアップが漏れていないことの検査 ───────────────────

    /// 変換結果に生の HTML が残っていないか。
    ///
    /// 「タグが 1 つ残る」= プレビューが崩れて見える、なので入口をまとめて
    /// 1 箇所で検査する。Markdown モードは `Vec<String>` のようなジェネリクスを
    /// 原文で残すのが仕様なので、閉じタグと**既知のタグ名で始まる `<`** だけを見る。
    fn assert_no_raw_markup(out: &str, ctx: &str) {
        let lower = out.to_ascii_lowercase();
        // 閉じタグは 1 つも残ってはいけない (ジェネリクスと紛れる形が無い)
        for name in [
            "div",
            "p",
            "span",
            "a",
            "table",
            "tr",
            "td",
            "th",
            "ul",
            "ol",
            "li",
            "pre",
            "code",
            "dl",
            "dt",
            "dd",
            "figure",
            "figcaption",
            "picture",
            "source",
            "video",
            "audio",
            "iframe",
            "svg",
            "script",
            "style",
            "template",
            "noscript",
            "select",
            "option",
            "button",
            "form",
            "label",
            "abbr",
            "mark",
            "q",
            "kbd",
            "blockquote",
            "strong",
            "em",
            "details",
            "summary",
            "header",
            "footer",
            "nav",
        ] {
            let close = format!("</{name}>");
            assert!(
                !lower.contains(&close),
                "{ctx}: {close:?} が残っている:\n{out}"
            );
            // 開始タグ (`<td>` / `<td ` の両方)
            for open in [format!("<{name}>"), format!("<{name} ")] {
                assert!(
                    !lower.contains(&open),
                    "{ctx}: {open:?} が残っている:\n{out}"
                );
            }
        }
        assert!(
            !lower.contains("<!--"),
            "{ctx}: コメントが残っている:\n{out}"
        );
        assert!(
            !lower.contains("<!doctype"),
            "{ctx}: DOCTYPE が残っている:\n{out}"
        );
        // 解決できるはずの実体参照が生で残っていないか
        let chars: Vec<char> = out.chars().collect();
        for (i, c) in chars.iter().enumerate() {
            if *c != '&' {
                continue;
            }
            let Some(end) = (i + 1..chars.len().min(i + 33)).find(|&k| chars[k] == ';') else {
                continue;
            };
            let name: String = chars[i + 1..end].iter().collect();
            // 実体参照の形をしているものだけ見る (地の文の `&` … `;` を拾わない)
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '#') {
                continue;
            }
            assert!(
                entity(&name).is_none(),
                "{ctx}: 実体参照 &{name}; が解決されずに残っている:\n{out}"
            );
        }
    }

    /// 実世界にある形の HTML 文書を通しで変換し、生のマークアップが
    /// 1 つも本文へ出ないことを確認する (`real_world_readme_…` の系統)。
    #[test]
    fn real_world_documents_leave_no_raw_markup() {
        let docs: &[(&str, &str)] = &[
            (
                "ブログ記事 (nav/header/footer/script/style つき)",
                r#"<!DOCTYPE html>
<html lang="ja"><head><meta charset="utf-8"><title>記事</title>
<style>body{color:#333}</style></head>
<body>
<nav><ul><li><a href="/">ホーム</a></li><li><a href="/blog">ブログ</a></li></ul></nav>
<header><h1>HTML &amp; Markdown</h1></header>
<article>
  <p>本文です。<abbr title="HyperText Markup Language">HTML</abbr> の話。</p>
  <figure><img src="fig.png" alt="図1"><figcaption>図 1: 構成</figcaption></figure>
  <blockquote><p>引用の 1 段目</p><blockquote><p>2 段目</p></blockquote></blockquote>
  <p>キーは <kbd>Ctrl</kbd>+<kbd>C</kbd>、<mark>重要</mark>です。</p>
</article>
<footer><p>&copy; 2026 &mdash; example</p></footer>
<script>console.log("<p>これは出てはいけない</p>")</script>
</body></html>"#,
            ),
            (
                "仕様表 (colspan / rowspan / thead / tfoot)",
                r#"<table>
  <caption>対応表</caption>
  <thead><tr><th>機能</th><th>Linux</th><th>macOS</th><th>Windows</th></tr></thead>
  <tbody>
    <tr><td>PTY</td><td colspan="2">対応</td><td>一部</td></tr>
    <tr><td rowspan="2">GPU</td><td>OK</td><td>OK</td><td>OK</td></tr>
    <tr><td>NG</td><td>OK</td><td>NG</td></tr>
  </tbody>
  <tfoot><tr><td colspan="4">以上</td></tr></tfoot>
</table>"#,
            ),
            (
                "定義リストと入れ子リスト",
                r#"<dl>
  <dt>ワークツリー</dt><dd>独立した作業ディレクトリ。</dd>
  <dt>リース</dt><dd>行域の<strong>所有権</strong>。</dd>
</dl>
<ul>
  <li><p>親の段落</p>
    <ul><li>子 1</li><li>子 2<ul><li>孫</li></ul></li></ul>
  </li>
  <li>単純な項目</li>
</ul>"#,
            ),
            (
                "メディア (picture / video / iframe / svg)",
                r##"<picture>
  <source media="(prefers-color-scheme: dark)" srcset="dark.png">
  <source srcset="light.png">
  <img src="fallback.png" alt="ロゴ">
</picture>
<picture><source srcset="only-source.avif 1x, only-source@2x.avif 2x"></picture>
<video controls poster="p.jpg"><source src="movie.mp4" type="video/mp4">対応してません</video>
<audio><source src="track.ogg"></audio>
<iframe src="https://example.com/embed" width="560"></iframe>
<svg viewBox="0 0 10 10"><title>状態遷移図</title><circle cx="5" cy="5" r="4"/></svg>
<svg class="icon"><use href="#i"/></svg>"##,
            ),
            (
                "フォームと選択肢",
                r#"<form action="/s"><fieldset><legend>絞り込み</legend>
<label for="k">種別</label>
<select id="k"><option>すべて</option><option selected>本</option><option>雑誌</option></select>
<textarea rows="3">初期値</textarea>
<button type="submit">送信</button>
</fieldset></form>"#,
            ),
            (
                "コード例を含む解説 (pre の中に ``` がある)",
                r#"<h2>書き方</h2>
<pre><code class="language-markdown">見出し

```rust
let x = 1;
```
</code></pre>
<p>インラインは <code>`code`</code> のように書きます。</p>"#,
            ),
            (
                "メール風の入れ子テーブルとインライン style",
                r#"<table width="100%"><tr><td align="center">
  <table><tr><td><span style="font-weight:bold">見出し</span></td></tr>
  <tr><td><span style="font-style:italic;line-through">古い値</span></td></tr></table>
</td></tr></table>"#,
            ),
            (
                "壊れた文書 (閉じ忘れ・交差・属性の引用符なし)",
                r#"<div class=box><p>閉じない段落
<b>太字<i>交差</b>している</i>
<ul><li>一<li>二
<table><tr><td>セル<td>もう 1 つ
<a href=/rel/path?a=1&amp;b=2>リンク
<img src=x.png alt=図>
<pre><code>閉じない"#,
            ),
        ];
        for (name, src) in docs {
            for (mode, out) in [
                ("html_to_md", html_to_md(src)),
                ("preprocess_markdown", preprocess_markdown(src)),
            ] {
                assert_no_raw_markup(&out, &format!("{name} / {mode}"));
            }
        }
    }

    // ─── 表 (結合セル・セル内の構造) ───────────────────────────────

    /// `| a | b |` 行をセルの列へ分解する (空白の詰め方に依らず桁を見るため)。
    fn cells(line: &str) -> Vec<&str> {
        line.trim()
            .trim_start_matches('|')
            .trim_end_matches('|')
            .split('|')
            .map(str::trim)
            .collect()
    }

    #[test]
    fn table_spans_keep_columns_aligned() {
        // colspan: 埋めないと後続のセルが左へ詰まり、見出しと中身がずれる
        let md = html_to_md(
            "<table><tr><th>A</th><th>B</th><th>C</th></tr>\
             <tr><td colspan=\"2\">結合</td><td>右端</td></tr></table>",
        );
        let rows: Vec<&str> = md.lines().filter(|l| l.starts_with('|')).collect();
        assert_eq!(cells(rows[0]), ["A", "B", "C"], "{md}");
        assert_eq!(cells(rows[2]), ["結合", "", "右端"], "{md}");

        // rowspan: 次の行でも桁を予約する
        let md = html_to_md(
            "<table><tr><th>K</th><th>V1</th><th>V2</th></tr>\
             <tr><td rowspan=\"2\">共通</td><td>a</td><td>b</td></tr>\
             <tr><td>c</td><td>d</td></tr></table>",
        );
        let rows: Vec<&str> = md.lines().filter(|l| l.starts_with('|')).collect();
        assert_eq!(cells(rows[2]), ["共通", "a", "b"], "{md}");
        assert_eq!(cells(rows[3]), ["", "c", "d"], "{md}");

        // 壊れた span 値でも桁が飛ばない
        for bad in ["0", "-3", "abc", "", "999999"] {
            let md = html_to_md(&format!(
                "<table><tr><td colspan=\"{bad}\">x</td><td>y</td></tr></table>"
            ));
            let cols = md
                .lines()
                .find(|l| l.starts_with('|'))
                .unwrap_or_default()
                .matches('|')
                .count();
            assert!(
                (2..=MAX_SPAN + 2).contains(&cols),
                "colspan={bad:?} で桁数が壊れた: {md}"
            );
        }
    }

    #[test]
    fn table_cells_stay_on_one_line() {
        // GFM の表は 1 セル 1 行しか持てない。<br>・段落・箇条書きが
        // セルの外へ漏れると表そのものが崩れる
        let md = html_to_md(
            "<table><tr><td>1 行目<br>2 行目</td>\
             <td><p>段落 A</p><p>段落 B</p></td>\
             <td><ul><li>甲</li><li>乙</li></ul></td>\
             <td><hr></td></tr></table>",
        );
        // 1 行 (= ヘッダ扱いの行) と区切り行だけ。セルの中身が行を割っていない
        let rows: Vec<&str> = md.lines().filter(|l| l.starts_with('|')).collect();
        assert_eq!(rows.len(), 2, "セルの中身が行を割った:\n{md}");
        let row = rows[0];
        assert!(row.contains("1 行目 2 行目"), "{md}");
        assert!(row.contains("段落 A") && row.contains("段落 B"), "{md}");
        // 箇条書きは中黒で区切って項目の切れ目を残す
        assert!(row.contains("• 甲") && row.contains("• 乙"), "{md}");
        // セル内の <hr> は区切り行と読み違えられるので出さない
        assert!(!row.contains("---"), "{md}");
    }

    #[test]
    fn table_cell_pipes_and_code_are_escaped() {
        let md = html_to_md(
            "<table><tr><td>a|b</td><td><code>x || y</code></td>\
             <td>&verbar;</td></tr></table>",
        );
        let row = md.lines().find(|l| l.starts_with("| ")).unwrap_or_default();
        // 生の | が残ると桁が増えて表がずれる
        assert_eq!(row.matches("\\|").count(), 4, "{md}");
    }

    // ─── インライン記法を壊さない ─────────────────────────────────

    #[test]
    fn inline_code_survives_backticks_and_newlines() {
        // Markdown モードは「インラインコード内は触らない」が先に立つので
        // (裸の ` が保護をトグルする)、ここは HTML 文書として通す
        let cases: &[(&str, &str)] = &[
            ("<code>plain</code>", "`plain`"),
            // 中身に ` があるときは区切りを伸ばして前後に空白を入れる
            ("<code>a`b</code>", "``a`b``"),
            ("<code>``x``</code>", "``` ``x`` ```"),
            // 改行を残すと Markdown のインラインコードが閉じない
            ("<code>a\nb</code>", "`a b`"),
        ];
        for (src, want) in cases {
            assert_eq!(html_to_md(src).trim(), *want, "{src:?}");
        }
        // 空の要素は記法ごと消す (`` が literal で残らない)
        assert_eq!(html_to_md("<code></code>").trim(), "");
        assert_eq!(preprocess_markdown("<code>plain</code>").trim(), "`plain`");
    }

    #[test]
    fn pre_containing_a_fence_switches_marker() {
        let md = html_to_md("<pre><code>前\n```\n中\n```\n後</code></pre>");
        assert!(md.starts_with("~~~"), "{md:?}");
        assert!(md.trim_end().ends_with("~~~"), "{md:?}");
        // 中身は 1 文字も落とさない
        for w in ["前", "```", "中", "後"] {
            assert!(md.contains(w), "{w:?} が無い: {md:?}");
        }
        // 普通の pre は ``` のまま
        let md = html_to_md("<pre><code class=\"language-rust\">let x;</code></pre>");
        assert!(md.starts_with("```rust"), "{md:?}");
        // 両方入っていたら ``` のまま (落ちないことだけ保証する)
        let md = html_to_md("<pre>```\n~~~\n</pre>");
        assert!(md.starts_with("```"), "{md:?}");
    }

    #[test]
    fn empty_inline_elements_emit_no_stray_marks() {
        // `****` `~~~~` のような literal がプレビューに出ないこと
        for src in [
            "<b></b>",
            "<strong> </strong>",
            "<em></em>",
            "<i></i>",
            "<del></del>",
            "<mark></mark>",
            "<code></code>",
            "<q></q>",
            "<abbr title=\"x\"></abbr>",
            "<figcaption></figcaption>",
        ] {
            let md = preprocess_markdown(src);
            for bad in ["**", "~~", "``", "\u{201C}\u{201D}"] {
                assert!(!md.contains(bad), "{src:?} → {md:?} に {bad:?} が出た");
            }
        }
        // 交差した閉じタグでも本文が消えず、生のタグも残らない
        let md = preprocess_markdown("<b>太<i>交</b>差</i>");
        for w in ['太', '交', '差'] {
            assert!(md.contains(w), "{w:?} が消えた: {md:?}");
        }
        assert!(!md.contains('<'), "{md:?}");
    }

    #[test]
    fn link_and_image_urls_survive_parens_and_spaces() {
        let cases: &[(&str, &str)] = &[
            // 丸括弧をそのまま書くと markdown 側が最初の ) で URL を切る
            (
                r#"<a href="https://ja.wikipedia.org/wiki/Rust_(プログラミング言語)">Rust</a>"#,
                "[Rust](https://ja.wikipedia.org/wiki/Rust_%28プログラミング言語%29)",
            ),
            (
                r#"<img src="my image.png" alt="図">"#,
                "![図](my%20image.png)",
            ),
            // リンク文字列の中の ] はリンクを途中で切る
            (r#"<a href="/x">a[1]b</a>"#, "[a(1)b](/x)"),
            // 実体参照経由の ] も同じ経路で直す
            (r#"<a href="/x">a&rsqb;b</a>"#, "[a)b](/x)"),
        ];
        for (src, want) in cases {
            let md = preprocess_markdown(src);
            assert_eq!(md.trim(), *want, "{src:?}");
        }
        // 実行系スキームはリンクにしない (中身のテキストは残す)
        let md = preprocess_markdown(r#"<a href="javascript:alert(1)">押す</a>"#);
        assert_eq!(md.trim(), "押す", "{md:?}");
    }

    // ─── ブロック構造 ─────────────────────────────────────────────

    #[test]
    fn block_elements_inside_list_items_do_not_split_the_list() {
        let md = html_to_md("<ul><li><p>一つ目</p><p>続き</p></li><li>二つ目</li></ul>");
        // 空行が入るとリストが切れて「マーカーだけの項目」が生まれる
        assert!(!md.contains("-\n"), "空の項目ができた:\n{md}");
        assert!(!md.contains("- \n"), "空の項目ができた:\n{md}");
        let items: Vec<&str> = md
            .lines()
            .filter(|l| l.trim_start().starts_with("- "))
            .collect();
        assert_eq!(items.len(), 2, "{md}");
        assert!(
            md.contains("一つ目") && md.contains("続き") && md.contains("二つ目"),
            "{md}"
        );
    }

    #[test]
    fn definition_list_keeps_terms_and_descriptions_apart() {
        let md = html_to_md("<dl><dt>語</dt><dd>説明</dd><dt>語 2</dt><dd>説明 2</dd></dl>");
        let lines: Vec<&str> = md.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 4, "用語と説明が同じ行に潰れた:\n{md}");
        assert!(lines[0].contains("**語**"), "{md}");
        assert!(lines[1].contains("説明"), "{md}");
        // ハード改行 (行末スペース 2 個) で行が分かれていること
        assert!(lines[0].ends_with("  "), "{:?}", lines[0]);
    }

    #[test]
    fn nested_blockquotes_are_prefixed_by_depth() {
        let md = html_to_md("<blockquote>外<blockquote>内</blockquote></blockquote>");
        assert!(md.contains("> 外"), "{md}");
        assert!(md.contains("> > 内"), "{md}");
    }

    // ─── メディア ─────────────────────────────────────────────────

    #[test]
    fn media_without_src_falls_back_to_source_children() {
        let cases: &[(&str, &[&str], &[&str])] = &[
            // <picture> は <img> が主役
            (
                r#"<picture><source srcset="d.png"><img src="l.png" alt="ロゴ"></picture>"#,
                &["![ロゴ](l.png)"],
                &["d.png"],
            ),
            // <img> が無ければ最初の <source> を使う (画像が消えない)
            (
                r#"<picture><source srcset="a.avif 1x, a@2x.avif 2x"></picture>"#,
                &["![](a.avif)"],
                &["2x"],
            ),
            // <video src> 無し + 子の <source>
            (
                r#"<video controls><source src="m.mp4"></video>"#,
                &["🎬", "(m.mp4)"],
                &["<source"],
            ),
            (
                r#"<audio><source src="t.ogg"></audio>"#,
                &["🔊", "(t.ogg)"],
                &["<audio"],
            ),
            // src があれば従来どおり。子の <source> で二重に出さない
            (
                r#"<video src="a.mp4"><source src="b.mp4"></video>"#,
                &["(a.mp4)"],
                &["b.mp4"],
            ),
        ];
        for (src, want, unwanted) in cases {
            let md = preprocess_markdown(src);
            for w in *want {
                assert!(md.contains(w), "{src:?} → {md:?} に {w:?} が無い");
            }
            for u in *unwanted {
                assert!(!md.contains(u), "{src:?} → {md:?} に {u:?} が残った");
            }
        }
    }

    #[test]
    fn undrawable_graphics_leave_a_label_only_when_named() {
        // 名前が取れるときだけ説明を残す (アイコンだらけにしない)
        let md = html_to_md("<p><svg><title>構成図</title><rect/></svg></p>");
        assert!(md.contains("🖼 構成図"), "{md:?}");
        let md = html_to_md(r#"<p><svg aria-label="警告アイコン"><path/></svg></p>"#);
        assert!(md.contains("🖼 警告アイコン"), "{md:?}");
        // 名前が無いインライン SVG は何も残さない (中身も漏らさない)
        let md = html_to_md(r#"<p>前<svg><path d="M0 0 L1 1"/></svg>後</p>"#);
        assert!(!md.contains("path") && !md.contains("M0 0"), "{md:?}");
        assert!(md.contains('前') && md.contains('後'), "{md:?}");
    }

    // ─── インライン要素の細部 ─────────────────────────────────────

    #[test]
    fn inline_semantics_render_readably() {
        let cases: &[(&str, &[&str], &[&str])] = &[
            (
                r#"<abbr title="HyperText Markup Language">HTML</abbr>"#,
                &["HTML (HyperText Markup Language)"],
                &["<abbr", "title="],
            ),
            // title が無ければ本文だけ
            ("<abbr>ID</abbr>", &["ID"], &["("]),
            ("<q>引用</q>", &["\u{201C}引用\u{201D}"], &["<q>"]),
            ("<mark>目印</mark>", &["**目印**"], &["<mark>"]),
            ("<kbd>Ctrl</kbd>", &["`Ctrl`"], &["<kbd>"]),
            (
                "<time datetime=\"2026-08-12\">今日</time>",
                &["今日"],
                &["datetime"],
            ),
            ("<ins>追加</ins>", &["追加"], &["<ins>"]),
            (
                "<figure><img src=\"a.png\" alt=\"図\"><figcaption>説明文</figcaption></figure>",
                &["![図](a.png)", "*説明文*"],
                &["<figcaption>"],
            ),
            // <select> の選択肢が地の文へ繋がらない
            (
                "<select><option>甲</option><option>乙</option></select>",
                &["甲", "乙"],
                &["甲乙", "<option>"],
            ),
        ];
        for (src, want, unwanted) in cases {
            let md = html_to_md(src);
            for w in *want {
                assert!(md.contains(w), "{src:?} → {md:?} に {w:?} が無い");
            }
            for u in *unwanted {
                assert!(!md.contains(u), "{src:?} → {md:?} に {u:?} が残った");
            }
        }
    }

    #[test]
    fn extended_entity_table() {
        let cases: &[(&str, &str)] = &[
            // ラテン文字 — 欠けると本文に &eacute; がそのまま出る
            ("caf&eacute;", "café"),
            ("&Uuml;ber &ntilde;", "Über ñ"),
            ("&aring;&oslash;&aelig;", "åøæ"),
            ("&szlig;&ccedil;&OElig;", "ßçŒ"),
            // ASCII 記号名
            ("a&lpar;b&rpar;", "a(b)"),
            ("&lbrack;x&rbrack;", "[x]"),
            ("&num;1 &percnt; &commat;", "#1 % @"),
            ("&colon;&semi;&excl;&quest;", ":;!?"),
            // 記号・矢印
            ("&mdash;&hellip;&rarr;&copy;", "—…→©"),
            ("&oplus;&sdot;&lceil;&rceil;", "⊕⋅⌈⌉"),
            ("&hearts;&numero;", "♥№"),
            // 数値参照は従来どおり
            ("&#65;&#x1F600;", "A\u{1F600}"),
            // 未知の名前はそのまま残す (誤変換しない)
            ("&notarealentity;", "&notarealentity;"),
        ];
        for (src, want) in cases {
            assert_eq!(decode_entities(src), *want, "{src:?}");
            assert_eq!(preprocess_markdown(src).trim(), *want, "{src:?}");
        }
        // 表の名前が重複していない (先に書いたほうが勝って気付けなくなる)
        for (i, (n, _)) in NAMED_ENTITIES.iter().enumerate() {
            assert!(
                !NAMED_ENTITIES[i + 1..].iter().any(|(m, _)| m == n),
                "実体参照 {n:?} が表に 2 回ある"
            );
        }
    }

    /// 対応タグを増やすと、その名前のジェネリクスが本文から消える。
    /// `is_known_tag` へ足すたびにここで踏み止まること。
    #[test]
    fn generic_type_names_are_not_eaten_by_new_tags() {
        for src in [
            "Array<Map>",
            "Vec<Element>",
            "Box<Content>",
            "Handler<Command>",
            "Rc<Frame>",
            "PathBuf<Dir>",
            "Vec<Option<T>>",
            "HashMap<String, Vec<u8>>",
            "fn f() -> Result<(), Box<dyn Error>>",
        ] {
            assert_eq!(
                preprocess_markdown(src),
                src,
                "{src:?} は原文のまま残すべき"
            );
        }
    }

    // ─── 壊れた入力 ───────────────────────────────────────────────

    #[test]
    fn hostile_html_degrades_without_panic() {
        let cases = [
            "<table><td colspan=\"999999\">x",
            "<table><tr><td rowspan=\"64\">a</td></tr><tr><td>b</td></tr>",
            &"<ul><li>".repeat(200),
            &"<blockquote>".repeat(200),
            &"<b>".repeat(500),
            &"<table><tr><td>".repeat(100),
            "<picture><source srcset=\"\">",
            "<video><source>",
            "<svg><title>閉じない",
            "<code>``",
            "<pre><code class=\"language-",
            "<a href=\"(\">x</a>",
            "<abbr title=\"",
            "<dl><dd>説明だけ",
            "<figcaption>孤立",
            "&#xFFFFFFFF; &#99999999999;",
            "<td colspan=\"1e9\">",
        ];
        for src in cases {
            let md = preprocess_markdown(src);
            let html = html_to_md(src);
            // 出力が青天井にならない (span の増幅などが無い)
            assert!(
                md.len() < src.len() * 64 + 4096,
                "{src:?} → {} bytes",
                md.len()
            );
            assert!(
                html.len() < src.len() * 64 + 4096,
                "{src:?} → {} bytes",
                html.len()
            );
        }
    }

    // ─── 巨大入力 ───────────────────────────────────────────────────

    #[test]
    fn huge_input_is_capped_with_notice() {
        let big = format!("<p>{}</p>", "あ".repeat(4_000_000)); // 12MB 超
        assert!(big.len() > 10 * 1024 * 1024);
        let md = html_to_md(&big);
        assert!(
            md.len() < crate::markdown::MAX_PREVIEW_BYTES + 4096,
            "len={}",
            md.len()
        );
        assert!(
            md.contains("⚠") || md.contains("KB"),
            "{}",
            &md[md.len() - 200..]
        );
        // HTML を含まない巨大 Markdown も同じ経路で切り詰められる
        let plain = "行\n".repeat(400_000);
        let out = preprocess_markdown(&plain);
        assert!(out.len() < crate::markdown::MAX_PREVIEW_BYTES + 4096);
    }
}
