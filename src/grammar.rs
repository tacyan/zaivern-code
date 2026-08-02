//! データだけで言語を足せる軽量シンタックス定義 (プラグインの `[[syntax]]`)。
//!
//! syntect の既定セット (`load_defaults_newlines`) は 60 構文で止まっており、
//! TypeScript / Swift / Kotlin / Dart / Zig / TOML / Dockerfile / Terraform …
//! といった現代の主要言語が**丸ごと無い**。かといって `.sublime-syntax` を
//! 何十本もベンダリングすると、ライセンス管理・起動時のリンク時間・
//! バイナリサイズのどれもが割に合わない。
//!
//! そこでこのモジュールは「**1 言語 = TOML の 1 ブロック**」で定義できる
//! 軽量トークナイザを提供する。持っている知識はコメント記号・引用符・
//! 数値の書き方・キーワード表だけで、構文木は作らない。色は自前で持たず
//! syntect のテーマスコープへ写すので、テーマを変えれば一緒に変わる。
//!
//! ```toml
//! [[syntax]]
//! name = "TypeScript"
//! extensions = ["ts", "mts"]
//! line_comment = ["//"]
//! block_comment = [["/*", "*/"]]
//! strings = ["\"", "'"]
//! keywords = ["if", "else", "return"]
//! ```
//!
//! **設計上の約束**
//!
//! * `scan_line` が返す範囲は行全体を**隙間なく順に覆う** (テストで固定)。
//!   改行文字も最後のトークンに含まれるので、呼び出し側は連結するだけで
//!   元の行に戻る。
//! * 走査は 1 行ごとで、行を跨ぐ状態 ([`ScanState`]) は Copy な小さな値。
//!   エディタが可視行だけを塗り直す実装へ移っても、そのまま使える。
//! * 言語知識はすべて [`Grammar`] のデータ。関数側に言語名の分岐は書かない。

use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;

/// トークンの種類。**色は持たない** — syntect のテーマスコープへ写して、
/// 現在のカラーテーマから色を引く (`highlight::Palette`)。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Tok {
    /// 何にも当てはまらない地の文 (空白・識別子・改行)。
    Text,
    Comment,
    /// ドキュメントコメント (`///`, `/**`, `--|` …)。
    Doc,
    Str,
    /// 文字列中のエスケープ (`\n`, `\u{1F600}` …)。
    Escape,
    /// 文字リテラル (`'a'`)。
    Char,
    Number,
    Keyword,
    /// 型・記憶域クラス (`int`, `struct`, `var` …)。
    Type,
    /// 言語組み込み定数 (`true` / `false` / `null` …)。
    Constant,
    /// 組み込み関数・標準ライブラリの識別子。
    Builtin,
    /// 呼び出し位置にある識別子 (`foo(` の `foo`)。
    Function,
    /// 注釈・デコレータ (`@Override`, `@State` …)。
    Attribute,
    /// 行頭のプリプロセッサ指令 (`#include`, `#define` …)。
    Preproc,
    Operator,
    Punct,
}

impl Tok {
    /// 配列 (パレット) の添字に使う通し番号。
    pub const COUNT: usize = 16;

    pub fn index(self) -> usize {
        match self {
            Tok::Text => 0,
            Tok::Comment => 1,
            Tok::Doc => 2,
            Tok::Str => 3,
            Tok::Escape => 4,
            Tok::Char => 5,
            Tok::Number => 6,
            Tok::Keyword => 7,
            Tok::Type => 8,
            Tok::Constant => 9,
            Tok::Builtin => 10,
            Tok::Function => 11,
            Tok::Attribute => 12,
            Tok::Preproc => 13,
            Tok::Operator => 14,
            Tok::Punct => 15,
        }
    }

    /// syntect テーマのスコープ名。テーマ側は前方一致で当たるので、
    /// `keyword.control` は `keyword` しか定義していないテーマでも色が付く。
    pub fn scope(self) -> &'static str {
        match self {
            Tok::Text => "source",
            Tok::Comment => "comment.line",
            Tok::Doc => "comment.block.documentation",
            Tok::Str => "string.quoted.double",
            Tok::Escape => "constant.character.escape",
            Tok::Char => "string.quoted.single",
            Tok::Number => "constant.numeric",
            Tok::Keyword => "keyword.control",
            Tok::Type => "storage.type",
            Tok::Constant => "constant.language",
            Tok::Builtin => "support.function",
            Tok::Function => "entity.name.function",
            Tok::Attribute => "entity.other.attribute-name",
            Tok::Preproc => "keyword.control.import",
            Tok::Operator => "keyword.operator",
            Tok::Punct => "punctuation",
        }
    }

    /// 通し番号から戻す (テストとパレット構築用)。
    pub fn from_index(i: usize) -> Tok {
        const ALL: [Tok; Tok::COUNT] = [
            Tok::Text,
            Tok::Comment,
            Tok::Doc,
            Tok::Str,
            Tok::Escape,
            Tok::Char,
            Tok::Number,
            Tok::Keyword,
            Tok::Type,
            Tok::Constant,
            Tok::Builtin,
            Tok::Function,
            Tok::Attribute,
            Tok::Preproc,
            Tok::Operator,
            Tok::Punct,
        ];
        ALL[i.min(Tok::COUNT - 1)]
    }
}

/// 1 トークンぶんの範囲 (行内のバイト位置、`start..end`)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub tok: Tok,
    pub start: usize,
    pub end: usize,
}

/// 行を跨いで持ち越す状態。Copy な小さい値に保つ。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ScanState {
    #[default]
    Normal,
    /// ブロックコメントの途中 (`block_comment[idx]`)。`doc` はドキュメント扱いか。
    Block { idx: u16, doc: bool },
    /// 複数行文字列の途中 (`strings[idx]`)。
    Str { idx: u16 },
}

/// 文字列リテラルの規則。
#[derive(Clone, Debug)]
pub struct StringRule {
    pub open: String,
    pub close: String,
    /// 行を跨げるか (三重引用符・バッククォートなど)。
    pub multiline: bool,
    /// `escape` 文字による打ち消しを見るか。
    pub escapes: bool,
}

/// 折りたたみ方式 (`highlight::FoldStrategy` へ写す文字列表現)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FoldKindSpec {
    Brackets,
    Indent,
    Markdown,
}

/// 1 言語ぶんの構文知識。**言語名の分岐をコードに書かないための唯一の置き場**。
#[derive(Clone, Debug)]
pub struct Grammar {
    /// 表示名。既存の syntect 名の慣習に合わせる ("TypeScript", "Kotlin" …)。
    /// スニペット・コメント切り替え・折りたたみがこの名前で引ける。
    pub name: String,
    /// 拡張子 (小文字、ドット無し)。
    pub extensions: Vec<String>,
    /// 拡張子を持たないファイル名 (小文字で完全一致。"dockerfile" など)。
    pub filenames: Vec<String>,
    /// Markdown のフェンス言語トークン ("ts", "typescript" など、小文字)。
    pub tokens: Vec<String>,
    /// 1 行目に含まれていればこの言語とみなす文字列 (シェバン)。
    pub first_line: Vec<String>,
    pub line_comment: Vec<String>,
    /// **行頭 (インデントの後) にあるときだけ**コメントになる記号。
    /// Vim script の `"` のように、行中では文字列の開始記号でもある言語向け。
    pub line_comment_bol: Vec<String>,
    pub doc_comment: Vec<String>,
    pub block_comment: Vec<(String, String)>,
    pub doc_block: Vec<(String, String)>,
    pub strings: Vec<StringRule>,
    /// `'a'` を文字リテラルとして扱うか (Rust のライフタイム対策で既定 false)。
    pub char_literal: bool,
    pub escape: Option<char>,
    /// 識別子に使える追加文字 (`$`, `-`, `?` など)。
    pub ident_extra: Vec<char>,
    /// 行頭のプリプロセッサ指令の開始記号 (`#`, `%`)。
    pub preproc: Vec<String>,
    /// 注釈・デコレータの開始記号 (`@` など)。
    pub attribute: Vec<char>,
    pub keywords: HashSet<String>,
    pub types: HashSet<String>,
    pub constants: HashSet<String>,
    pub builtins: HashSet<String>,
    /// 大文字小文字を区別するか (SQL / Fortran などは false)。
    pub case_sensitive: bool,
    pub fold: FoldKindSpec,
}

impl Grammar {
    /// この言語の識別子として使える先頭文字か。
    fn ident_start(&self, c: char) -> bool {
        c.is_alphabetic() || c == '_' || self.ident_extra.contains(&c)
    }

    fn ident_cont(&self, c: char) -> bool {
        c.is_alphanumeric() || c == '_' || self.ident_extra.contains(&c)
    }

    /// キーワード表を引く。`case_sensitive = false` の言語は小文字化して引く
    /// (表側も読み込み時に小文字化してある)。
    fn classify(&self, word: &str) -> Option<Tok> {
        let owned;
        let key = if self.case_sensitive {
            word
        } else {
            owned = word.to_lowercase();
            &owned
        };
        if self.keywords.contains(key) {
            Some(Tok::Keyword)
        } else if self.types.contains(key) {
            Some(Tok::Type)
        } else if self.constants.contains(key) {
            Some(Tok::Constant)
        } else if self.builtins.contains(key) {
            Some(Tok::Builtin)
        } else {
            None
        }
    }
}

// ───────────────────────── 走査 ─────────────────────────

/// トークンを溜める先。同種で隣り合うものは 1 つにまとめる
/// (LayoutJob のセクション数を減らすため)。
struct Out<'a> {
    v: &'a mut Vec<Span>,
}

impl Out<'_> {
    fn push(&mut self, tok: Tok, start: usize, end: usize) {
        if end <= start {
            return;
        }
        if let Some(last) = self.v.last_mut() {
            if last.tok == tok && last.end == start {
                last.end = end;
                return;
            }
        }
        self.v.push(Span { tok, start, end });
    }
}

/// 1 行を走査して `out` へトークンを積む。`st` は行を跨ぐ状態で、
/// 呼び出し側は先頭行で `ScanState::Normal` を渡し、以降は使い回す。
///
/// **不変条件**: 積まれた範囲は `0..line.len()` を隙間なく順に覆う。
pub fn scan_line(g: &Grammar, line: &str, st: &mut ScanState, out: &mut Vec<Span>) {
    let mut o = Out { v: out };
    let len = line.len();
    let mut i = 0usize;

    // --- 行を跨いだ継続 (ブロックコメント / 複数行文字列) ---
    match *st {
        ScanState::Block { idx, doc } => {
            let pair = pair_at(g, idx, doc);
            let close = pair.map(|p| p.1.as_str()).unwrap_or("*/");
            let kind = if doc { Tok::Doc } else { Tok::Comment };
            match find_from(line, close, 0) {
                Some(p) => {
                    let e = p + close.len();
                    o.push(kind, 0, e);
                    i = e;
                    *st = ScanState::Normal;
                }
                None => {
                    o.push(kind, 0, len);
                    return;
                }
            }
        }
        ScanState::Str { idx } => {
            let Some(rule) = g.strings.get(idx as usize) else {
                *st = ScanState::Normal;
                return scan_line(g, line, st, o.v);
            };
            match scan_string_body(g, line, 0, rule, &mut o) {
                Some(end) => {
                    i = end;
                    *st = ScanState::Normal;
                }
                None => return,
            }
        }
        ScanState::Normal => {}
    }

    // --- 通常走査 ---
    let mut line_start = i == 0; // 行頭から空白しか見ていないか
    while i < len {
        let c = match line[i..].chars().next() {
            Some(c) => c,
            None => break,
        };
        let cl = c.len_utf8();

        // 1. 行頭限定のコメント (Vim script の `"` など。行中では文字列)
        if line_start
            && g.line_comment_bol
                .iter()
                .any(|c| !c.is_empty() && line[i..].starts_with(c.as_str()))
        {
            o.push(Tok::Comment, i, len);
            break;
        }

        // 2. コメント (ドキュメント優先 / 長い記号優先)
        if let Some((tok, tlen, block)) = comment_at(g, line, i) {
            match block {
                None => {
                    o.push(tok, i, len);
                    i = len;
                    break;
                }
                Some((bi, close)) => {
                    match find_from(line, close, i + tlen) {
                        Some(p) => {
                            let e = p + close.len();
                            o.push(tok, i, e);
                            i = e;
                        }
                        None => {
                            o.push(tok, i, len);
                            *st = ScanState::Block {
                                idx: bi,
                                doc: tok == Tok::Doc,
                            };
                            return;
                        }
                    }
                    line_start = false;
                    continue;
                }
            }
        }

        // 3. 文字列 (複数行は状態を持ち越す)
        if let Some((si, rule)) = string_at(g, line, i) {
            let open_end = i + rule.open.len();
            o.push(Tok::Str, i, open_end);
            match scan_string_body(g, line, open_end, rule, &mut o) {
                Some(end) => i = end,
                None => {
                    if rule.multiline {
                        *st = ScanState::Str { idx: si };
                    }
                    return;
                }
            }
            line_start = false;
            continue;
        }

        // 4. 文字リテラル
        if g.char_literal && c == '\'' {
            let end = scan_char_literal(g, line, i);
            o.push(Tok::Char, i, end);
            i = end;
            line_start = false;
            continue;
        }

        // 5. 行頭のプリプロセッサ指令
        if line_start && !g.preproc.is_empty() {
            if let Some(d) = g.preproc.iter().find(|d| line[i..].starts_with(d.as_str())) {
                let mut e = i + d.len();
                while e < len {
                    let ch = match line[e..].chars().next() {
                        Some(ch) => ch,
                        None => break,
                    };
                    if ch == ' ' || ch == '\t' {
                        // 指令名の前の空白 (`#  include`) は飲み込む
                        if line[i + d.len()..e].trim().is_empty() {
                            e += ch.len_utf8();
                            continue;
                        }
                        break;
                    }
                    if !g.ident_cont(ch) {
                        break;
                    }
                    e += ch.len_utf8();
                }
                o.push(Tok::Preproc, i, e);
                i = e;
                line_start = false;
                continue;
            }
        }

        // 6. 注釈・デコレータ (`@Override`)
        if g.attribute.contains(&c) {
            let mut e = i + cl;
            while e < len {
                let ch = match line[e..].chars().next() {
                    Some(ch) => ch,
                    None => break,
                };
                if !g.ident_cont(ch) && ch != '.' {
                    break;
                }
                e += ch.len_utf8();
            }
            if e > i + cl {
                o.push(Tok::Attribute, i, e);
                i = e;
                line_start = false;
                continue;
            }
        }

        // 7. 数値
        if c.is_ascii_digit() {
            let e = scan_number(line, i);
            o.push(Tok::Number, i, e);
            i = e;
            line_start = false;
            continue;
        }

        // 8. 識別子 / キーワード
        if g.ident_start(c) {
            let mut e = i + cl;
            while e < len {
                let ch = match line[e..].chars().next() {
                    Some(ch) => ch,
                    None => break,
                };
                if !g.ident_cont(ch) {
                    break;
                }
                e += ch.len_utf8();
            }
            let word = &line[i..e];
            let tok = match g.classify(word) {
                Some(t) => t,
                // 直後が `(` なら呼び出し (空白は跨ぐ)
                None if next_nonspace(line, e) == Some('(') => Tok::Function,
                None => Tok::Text,
            };
            o.push(tok, i, e);
            i = e;
            line_start = false;
            continue;
        }

        // 9. 演算子 / 区切り / 空白
        let tok = if is_operator(c) {
            Tok::Operator
        } else if is_punct(c) {
            Tok::Punct
        } else {
            Tok::Text
        };
        o.push(tok, i, i + cl);
        if !c.is_whitespace() {
            line_start = false;
        }
        i += cl;
    }
}

/// ブロックコメントの (開き, 閉じ) を添字から引く。`doc` なら doc_block 側。
fn pair_at(g: &Grammar, idx: u16, doc: bool) -> Option<&(String, String)> {
    if doc {
        g.doc_block.get(idx as usize)
    } else {
        g.block_comment.get(idx as usize)
    }
}

/// 位置 `i` から始まるコメントを判定する。
/// 戻り値は (種類, 開始記号の長さ, ブロックなら (添字, 閉じ記号))。
fn comment_at<'a>(
    g: &'a Grammar,
    line: &str,
    i: usize,
) -> Option<(Tok, usize, Option<(u16, &'a str)>)> {
    let rest = &line[i..];
    // ドキュメントを先に見る (`///` は `//` より長い)
    let mut best: Option<(Tok, usize, Option<(u16, &str)>)> = None;
    let mut consider = |tok: Tok, open_len: usize, block: Option<(u16, &'a str)>| {
        if best.as_ref().map(|b| b.1).unwrap_or(0) < open_len {
            best = Some((tok, open_len, block));
        }
    };
    for (bi, (open, close)) in g.doc_block.iter().enumerate() {
        if !open.is_empty() && rest.starts_with(open.as_str()) {
            consider(Tok::Doc, open.len(), Some((bi as u16, close.as_str())));
        }
    }
    for (bi, (open, close)) in g.block_comment.iter().enumerate() {
        if !open.is_empty() && rest.starts_with(open.as_str()) {
            consider(Tok::Comment, open.len(), Some((bi as u16, close.as_str())));
        }
    }
    for d in &g.doc_comment {
        if !d.is_empty() && rest.starts_with(d.as_str()) {
            consider(Tok::Doc, d.len(), None);
        }
    }
    for l in &g.line_comment {
        if !l.is_empty() && rest.starts_with(l.as_str()) {
            consider(Tok::Comment, l.len(), None);
        }
    }
    best
}

/// 位置 `i` から始まる文字列リテラルの規則を返す (開き記号が長いものを優先)。
fn string_at<'a>(g: &'a Grammar, line: &str, i: usize) -> Option<(u16, &'a StringRule)> {
    let rest = &line[i..];
    let mut best: Option<(u16, &StringRule)> = None;
    for (si, r) in g.strings.iter().enumerate() {
        if r.open.is_empty() || !rest.starts_with(r.open.as_str()) {
            continue;
        }
        if best.map(|(_, b)| b.open.len()).unwrap_or(0) < r.open.len() {
            best = Some((si as u16, r));
        }
    }
    best
}

/// 開き記号の**後ろ**から閉じ記号までを積む。閉じたら終端位置、
/// 行内に閉じが無ければ `None` (行末まで積んである)。
fn scan_string_body(
    g: &Grammar,
    line: &str,
    from: usize,
    rule: &StringRule,
    o: &mut Out,
) -> Option<usize> {
    let len = line.len();
    let esc = if rule.escapes { g.escape } else { None };
    let mut i = from;
    let mut seg = from; // まだ積んでいない文字列部分の先頭
    while i < len {
        let c = line[i..].chars().next()?;
        let cl = c.len_utf8();
        if Some(c) == esc && i + cl < len {
            // エスケープは 1 文字ぶんだけ色を変える (`\n`, `\"` …)
            let nl = line[i + cl..].chars().next().map(|n| n.len_utf8()).unwrap_or(0);
            o.push(Tok::Str, seg, i);
            o.push(Tok::Escape, i, i + cl + nl);
            i += cl + nl;
            seg = i;
            continue;
        }
        if !rule.close.is_empty() && line[i..].starts_with(rule.close.as_str()) {
            let e = i + rule.close.len();
            o.push(Tok::Str, seg, e);
            return Some(e);
        }
        // 複数行を許さない文字列は改行で打ち切る (閉じ忘れが行を汚さない)
        if c == '\n' && !rule.multiline {
            o.push(Tok::Str, seg, i);
            o.push(Tok::Text, i, len);
            return Some(len);
        }
        i += cl;
    }
    o.push(Tok::Str, seg, len);
    None
}

/// `'a'` / `'\n'` を読む。閉じが無ければ 1 文字ぶんで諦める
/// (Rust のライフタイムのような使い方で行末まで染めないため)。
fn scan_char_literal(g: &Grammar, line: &str, i: usize) -> usize {
    let len = line.len();
    let mut j = i + 1;
    let mut steps = 0;
    while j < len && steps < 12 {
        let c = match line[j..].chars().next() {
            Some(c) => c,
            None => break,
        };
        let cl = c.len_utf8();
        if Some(c) == g.escape {
            j += cl;
            j += line[j..].chars().next().map(|n| n.len_utf8()).unwrap_or(0);
            steps += 1;
            continue;
        }
        if c == '\'' {
            return j + cl;
        }
        if c == '\n' {
            break;
        }
        j += cl;
        steps += 1;
    }
    i + 1
}

/// 数値リテラルの終端。`0xFF` `1_000` `1.5e-3` `10u8` `1.0f` を 1 トークンで読む。
fn scan_number(line: &str, i: usize) -> usize {
    let b = line.as_bytes();
    let len = b.len();
    let mut j = i;
    while j < len {
        let c = b[j];
        if c.is_ascii_alphanumeric() || c == b'_' {
            j += 1;
            continue;
        }
        // 指数の符号 (1e-3, 1E+3, 0x1p-2)
        if (c == b'+' || c == b'-') && j > i {
            let p = b[j - 1].to_ascii_lowercase();
            if p == b'e' || p == b'p' {
                j += 1;
                continue;
            }
        }
        // 小数点は後ろが数字のときだけ (`1..2` の範囲演算子と区別する)
        if c == b'.' && j + 1 < len && b[j + 1].is_ascii_digit() {
            j += 1;
            continue;
        }
        break;
    }
    j.max(i + 1)
}

/// `from` 以降で最初の非空白文字 (同じ行の中だけ。行末までなら None)。
fn next_nonspace(line: &str, from: usize) -> Option<char> {
    line[from..].chars().find(|c| !c.is_whitespace())
}

fn is_operator(c: char) -> bool {
    matches!(
        c,
        '+' | '-' | '*' | '/' | '%' | '=' | '<' | '>' | '!' | '&' | '|' | '^' | '~' | '?'
    )
}

fn is_punct(c: char) -> bool {
    matches!(c, '(' | ')' | '{' | '}' | '[' | ']' | ';' | ',' | ':' | '.' | '@' | '#' | '$')
}

/// `needle` を `from` 以降から探す (バイト位置)。
fn find_from(hay: &str, needle: &str, from: usize) -> Option<usize> {
    if from > hay.len() {
        return None;
    }
    hay[from..].find(needle).map(|p| p + from)
}

// ───────────────────── マニフェスト (TOML) ─────────────────────

/// 既存構文への別名。`.mjs` → JavaScript のように、**既にある定義を使い回す**。
/// 対象は syntect の構文名でも、このパックが定義した [`Grammar`] の名前でもよい。
#[derive(Clone, Debug)]
pub struct Alias {
    pub target: String,
    pub extensions: Vec<String>,
    pub filenames: Vec<String>,
    pub tokens: Vec<String>,
    pub first_line: Vec<String>,
}

#[derive(Deserialize)]
struct RawPack {
    #[serde(default, rename = "syntax")]
    syntaxes: Vec<RawSyntax>,
    #[serde(default, rename = "alias")]
    aliases: Vec<RawAlias>,
}

#[derive(Deserialize)]
struct RawSyntax {
    name: String,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    filenames: Vec<String>,
    #[serde(default)]
    tokens: Vec<String>,
    #[serde(default)]
    first_line: Vec<String>,
    #[serde(default)]
    line_comment: Vec<String>,
    #[serde(default)]
    line_comment_bol: Vec<String>,
    #[serde(default)]
    doc_comment: Vec<String>,
    #[serde(default)]
    block_comment: Vec<Vec<String>>,
    #[serde(default)]
    doc_block: Vec<Vec<String>>,
    #[serde(default)]
    strings: Vec<String>,
    #[serde(default)]
    multiline_strings: Vec<Vec<String>>,
    /// エスケープを見ない生文字列 (`r"..."` の中身、Fortran の '' など)。
    #[serde(default)]
    raw_strings: Vec<Vec<String>>,
    #[serde(default)]
    char_literal: bool,
    #[serde(default)]
    escape: Option<String>,
    #[serde(default)]
    ident_extra: String,
    #[serde(default)]
    preproc: Vec<String>,
    #[serde(default)]
    attribute: String,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    types: Vec<String>,
    #[serde(default)]
    constants: Vec<String>,
    #[serde(default)]
    builtins: Vec<String>,
    #[serde(default)]
    case_sensitive: Option<bool>,
    #[serde(default)]
    fold: String,
}

#[derive(Deserialize)]
struct RawAlias {
    /// 別名の行き先 (syntect の構文名、またはこのパックの `name`)。
    target: String,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    filenames: Vec<String>,
    #[serde(default)]
    tokens: Vec<String>,
    #[serde(default)]
    first_line: Vec<String>,
}

fn lower_all(v: Vec<String>) -> Vec<String> {
    v.into_iter()
        .map(|s| s.trim().trim_start_matches('.').to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn pairs(v: Vec<Vec<String>>) -> Vec<(String, String)> {
    v.into_iter()
        .filter(|p| p.len() == 2 && !p[0].is_empty() && !p[1].is_empty())
        .map(|p| (p[0].clone(), p[1].clone()))
        .collect()
}

fn set_of(v: Vec<String>, case_sensitive: bool) -> HashSet<String> {
    v.into_iter()
        .map(|s| {
            let s = s.trim().to_string();
            if case_sensitive {
                s
            } else {
                s.to_lowercase()
            }
        })
        .filter(|s| !s.is_empty())
        .collect()
}

impl RawSyntax {
    fn build(self) -> Result<Grammar, String> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err("[[syntax]] に name が必要です".into());
        }
        let case_sensitive = self.case_sensitive.unwrap_or(true);
        // `escape = ""` は「エスケープ記号を持たない言語」(VB の "" 等)。
        // 省略時だけ `\` を既定にする。
        let escape = match self.escape.as_deref() {
            Some("") => None,
            Some(s) => s.chars().next(),
            None => Some('\\'),
        };

        let mut strings: Vec<StringRule> = Vec::new();
        for q in self.strings {
            if q.is_empty() {
                continue;
            }
            strings.push(StringRule {
                open: q.clone(),
                close: q,
                multiline: false,
                escapes: true,
            });
        }
        for p in pairs(self.multiline_strings) {
            strings.push(StringRule {
                open: p.0,
                close: p.1,
                multiline: true,
                escapes: true,
            });
        }
        for p in pairs(self.raw_strings) {
            strings.push(StringRule {
                open: p.0,
                close: p.1,
                multiline: true,
                escapes: false,
            });
        }
        // 開き記号が長いものを先に見たいので、走査側の best 判定に任せつつ
        // 安定した順序にしておく (同じ長さなら定義順)。
        strings.sort_by(|a, b| b.open.len().cmp(&a.open.len()));

        let fold = match self.fold.trim() {
            "" | "brackets" => FoldKindSpec::Brackets,
            "indent" => FoldKindSpec::Indent,
            "markdown" => FoldKindSpec::Markdown,
            other => return Err(format!("{name}: fold = {other:?} は未知です")),
        };

        let mut tokens = lower_all(self.tokens);
        let lname = name.to_lowercase();
        if !tokens.contains(&lname) {
            tokens.push(lname);
        }

        Ok(Grammar {
            name,
            extensions: lower_all(self.extensions),
            filenames: lower_all(self.filenames),
            tokens,
            first_line: self.first_line,
            line_comment: self.line_comment,
            line_comment_bol: self.line_comment_bol,
            doc_comment: self.doc_comment,
            block_comment: pairs(self.block_comment),
            doc_block: pairs(self.doc_block),
            strings,
            char_literal: self.char_literal,
            escape,
            ident_extra: self.ident_extra.chars().collect(),
            preproc: self.preproc,
            attribute: self.attribute.chars().collect(),
            keywords: set_of(self.keywords, case_sensitive),
            types: set_of(self.types, case_sensitive),
            constants: set_of(self.constants, case_sensitive),
            builtins: set_of(self.builtins, case_sensitive),
            case_sensitive,
            fold,
        })
    }
}

/// 読み込んだ言語定義の集合。
#[derive(Default, Debug)]
pub struct GrammarSet {
    pub grammars: Vec<Grammar>,
    pub aliases: Vec<Alias>,
}

impl GrammarSet {
    pub fn is_empty(&self) -> bool {
        self.grammars.is_empty() && self.aliases.is_empty()
    }

    /// TOML 1 枚を読む。
    pub fn parse_toml(src: &str) -> Result<GrammarSet, String> {
        let raw: RawPack = toml::from_str(src).map_err(|e| format!("構文定義の解析に失敗: {e}"))?;
        let mut out = GrammarSet::default();
        for s in raw.syntaxes {
            out.grammars.push(s.build()?);
        }
        for a in raw.aliases {
            let target = a.target.trim().to_string();
            if target.is_empty() {
                return Err("[[alias]] に target が必要です".into());
            }
            out.aliases.push(Alias {
                target,
                extensions: lower_all(a.extensions),
                filenames: lower_all(a.filenames),
                tokens: lower_all(a.tokens),
                first_line: a.first_line,
            });
        }
        Ok(out)
    }

    /// ファイル、またはディレクトリ内の `*.toml` をまとめて読む。
    /// 読めなかったものは `errors` に理由を積んで**残りは読み続ける**
    /// (1 枚壊れただけで全言語が消えないように)。
    pub fn load_path(path: &Path, errors: &mut Vec<String>) -> GrammarSet {
        let mut out = GrammarSet::default();
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        if path.is_dir() {
            if let Ok(rd) = std::fs::read_dir(path) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|x| x.to_str()) == Some("toml") {
                        files.push(p);
                    }
                }
            }
            files.sort();
        } else {
            files.push(path.to_path_buf());
        }
        for f in files {
            match std::fs::read_to_string(&f) {
                Ok(src) => match GrammarSet::parse_toml(&src) {
                    Ok(set) => out.merge(set),
                    Err(e) => errors.push(format!("{}: {e}", f.display())),
                },
                Err(e) => errors.push(format!("{}: 読めません ({e})", f.display())),
            }
        }
        out
    }

    /// 後から来た定義を足す。**同名は先に入っていた方を残す**
    /// (ユーザーのプラグインが標準パックを上書きしたい場合は、
    ///  そちらを先に読み込む側で順序を決める)。
    pub fn merge(&mut self, other: GrammarSet) {
        for g in other.grammars {
            if !self.grammars.iter().any(|x| x.name.eq_ignore_ascii_case(&g.name)) {
                self.grammars.push(g);
            }
        }
        self.aliases.extend(other.aliases);
    }

    pub fn by_name(&self, name: &str) -> Option<&Grammar> {
        self.grammars.iter().find(|g| g.name.eq_ignore_ascii_case(name))
    }

    /// ファイル名から言語名を引く。拡張子より**ファイル名の完全一致**が優先
    /// (`Dockerfile.dev` より `Dockerfile` を先に見る、という意味ではなく、
    ///  `Makefile` のように拡張子を持たないファイルを拾うため)。
    pub fn detect_path(&self, path: &Path) -> Option<String> {
        let file = path.file_name()?.to_str()?.to_lowercase();
        for g in &self.grammars {
            if g.filenames.iter().any(|f| *f == file) {
                return Some(g.name.clone());
            }
        }
        for a in &self.aliases {
            if a.filenames.iter().any(|f| *f == file) {
                return Some(a.target.clone());
            }
        }
        // 拡張子は末尾から順に長い方を試す (`foo.tar.gz` → "tar.gz" → "gz")
        let mut cand: Vec<String> = Vec::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            cand.push(ext.to_lowercase());
        }
        if let Some((_, rest)) = file.split_once('.') {
            let rest = rest.to_string();
            if !rest.is_empty() && !cand.contains(&rest) {
                cand.insert(0, rest);
            }
        }
        for c in &cand {
            for g in &self.grammars {
                if g.extensions.iter().any(|e| e == c) {
                    return Some(g.name.clone());
                }
            }
            for a in &self.aliases {
                if a.extensions.iter().any(|e| e == c) {
                    return Some(a.target.clone());
                }
            }
        }
        None
    }

    /// 1 行目 (シェバン等) から引く。
    pub fn detect_first_line(&self, line: &str) -> Option<String> {
        for g in &self.grammars {
            if g.first_line.iter().any(|p| !p.is_empty() && line.contains(p.as_str())) {
                return Some(g.name.clone());
            }
        }
        for a in &self.aliases {
            if a.first_line.iter().any(|p| !p.is_empty() && line.contains(p.as_str())) {
                return Some(a.target.clone());
            }
        }
        None
    }

    /// Markdown のフェンス言語トークン ("ts", "kotlin" …) から引く。
    pub fn detect_token(&self, token: &str) -> Option<String> {
        let t = token.trim().to_lowercase();
        if t.is_empty() {
            return None;
        }
        for g in &self.grammars {
            if g.tokens.iter().any(|x| *x == t) || g.extensions.iter().any(|x| *x == t) {
                return Some(g.name.clone());
            }
        }
        for a in &self.aliases {
            if a.tokens.iter().any(|x| *x == t) || a.extensions.iter().any(|x| *x == t) {
                return Some(a.target.clone());
            }
        }
        None
    }

    /// 収録している言語名 (UI の一覧表示用)。
    pub fn names(&self) -> Vec<&str> {
        self.grammars.iter().map(|g| g.name.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(src: &str) -> Grammar {
        GrammarSet::parse_toml(src).expect("解析できる").grammars.remove(0)
    }

    fn ts() -> Grammar {
        g(r##"
[[syntax]]
name = "TypeScript"
extensions = ["ts", "mts"]
tokens = ["ts"]
line_comment = ["//"]
doc_comment = ["///"]
block_comment = [["/*", "*/"]]
doc_block = [["/**", "*/"]]
strings = ["\"", "'"]
multiline_strings = [["`", "`"]]
ident_extra = "$"
attribute = "@"
keywords = ["if", "else", "return", "function", "const"]
types = ["string", "number"]
constants = ["true", "false", "null"]
builtins = ["console"]
"##)
    }

    /// 走査結果を「トークン種類 + 本文」の列にする (テストの読みやすさ用)。
    fn toks(gr: &Grammar, text: &str) -> Vec<(Tok, String)> {
        let mut st = ScanState::default();
        let mut out = Vec::new();
        let mut res = Vec::new();
        for line in text.split_inclusive('\n') {
            out.clear();
            scan_line(gr, line, &mut st, &mut out);
            // 不変条件: 隙間なく行全体を覆う
            let mut at = 0;
            for s in &out {
                assert_eq!(s.start, at, "隙間がある: {out:?} / {line:?}");
                assert!(s.end <= line.len());
                at = s.end;
            }
            assert_eq!(at, line.len(), "行末まで覆っていない: {line:?}");
            for s in &out {
                res.push((s.tok, line[s.start..s.end].to_string()));
            }
        }
        res
    }

    fn kinds(gr: &Grammar, text: &str, want: &[(Tok, &str)]) {
        let got = toks(gr, text);
        for (tok, body) in want {
            assert!(
                got.iter().any(|(t, b)| t == tok && b == body),
                "{tok:?} {body:?} が無い: {got:?}"
            );
        }
    }

    #[test]
    fn 走査は行全体を隙間なく覆う() {
        let gr = ts();
        // 不変条件は toks() 内で assert している
        toks(&gr, "const x: number = 1; // ok\nlet s = `a${b}c`;\n");
        toks(&gr, "");
        toks(&gr, "\n\n");
        toks(&gr, "日本語のコメント // あり\n");
    }

    #[test]
    fn キーワードと型と定数を色分けする() {
        let gr = ts();
        kinds(
            &gr,
            "const x: number = true;\n",
            &[
                (Tok::Keyword, "const"),
                (Tok::Type, "number"),
                (Tok::Constant, "true"),
            ],
        );
        // 組み込みは別種
        kinds(&gr, "console.log(1);\n", &[(Tok::Builtin, "console")]);
    }

    #[test]
    fn 呼び出し位置の識別子は関数扱い() {
        let gr = ts();
        kinds(&gr, "foo(1);\n", &[(Tok::Function, "foo"), (Tok::Number, "1")]);
        // 空白を挟んでも呼び出し
        kinds(&gr, "bar ();\n", &[(Tok::Function, "bar")]);
        // 呼び出しでなければただの文字
        kinds(&gr, "baz;\n", &[(Tok::Text, "baz")]);
    }

    #[test]
    fn 文字列とエスケープを分ける() {
        let gr = ts();
        kinds(
            &gr,
            "let s = \"a\\nb\";\n",
            // 隣り合う同種は 1 つにまとまるので、開き引用符は本文と一体になる
            &[(Tok::Str, "\"a"), (Tok::Escape, "\\n"), (Tok::Str, "b\"")],
        );
    }

    #[test]
    fn 閉じ忘れた一行文字列は行末で切れる() {
        let gr = ts();
        let got = toks(&gr, "let s = \"abc\nconst t = 1;\n");
        // 2 行目は通常の走査へ戻っている (文字列が次の行を飲み込まない)
        assert!(
            got.iter().any(|(t, b)| *t == Tok::Keyword && b == "const"),
            "{got:?}"
        );
        assert!(
            got.iter().any(|(t, b)| *t == Tok::Number && b == "1"),
            "文字列が次の行を飲み込んでいる: {got:?}"
        );
    }

    #[test]
    fn 複数行文字列は行を跨ぐ() {
        let gr = ts();
        let got = toks(&gr, "let s = `abc\ndef`;\nlet n = 1;\n");
        assert!(got.iter().any(|(t, b)| *t == Tok::Str && b == "def`"));
        assert!(got.iter().any(|(t, b)| *t == Tok::Number && b == "1"));
    }

    #[test]
    fn ブロックコメントは行を跨ぐ() {
        let gr = ts();
        let got = toks(&gr, "/* a\n b */ const x = 1;\n");
        assert!(got.iter().any(|(t, b)| *t == Tok::Comment && b == " b */"));
        assert!(got.iter().any(|(t, b)| *t == Tok::Keyword && b == "const"));
    }

    #[test]
    fn ドキュメントコメントは別種になる() {
        let gr = ts();
        kinds(&gr, "/// doc\n", &[(Tok::Doc, "/// doc\n")]);
        let got = toks(&gr, "/** doc */\n");
        assert!(got.iter().any(|(t, b)| *t == Tok::Doc && b == "/** doc */"));
        // `//` は通常コメント
        kinds(&gr, "// plain\n", &[(Tok::Comment, "// plain\n")]);
    }

    #[test]
    fn 数値の書式をまとめて読む() {
        let gr = ts();
        for (src, want) in [
            ("0xFF;\n", "0xFF"),
            ("1_000;\n", "1_000"),
            ("1.5e-3;\n", "1.5e-3"),
            ("10n;\n", "10n"),
        ] {
            kinds(&gr, src, &[(Tok::Number, want)]);
        }
        // `1..2` の範囲は数値 2 つ
        let got = toks(&gr, "1..2;\n");
        assert!(got.iter().filter(|(t, _)| *t == Tok::Number).count() == 2, "{got:?}");
    }

    #[test]
    fn デコレータを拾う() {
        let gr = ts();
        kinds(&gr, "@Component({})\n", &[(Tok::Attribute, "@Component")]);
    }

    #[test]
    fn 大文字小文字を無視する言語() {
        let gr = g(r##"
[[syntax]]
name = "SQL2"
extensions = ["sql2"]
case_sensitive = false
line_comment = ["--"]
strings = ["'"]
keywords = ["select", "from"]
"##);
        kinds(&gr, "SELECT * FROM t;\n", &[(Tok::Keyword, "SELECT"), (Tok::Keyword, "FROM")]);
    }

    #[test]
    fn プリプロセッサ指令は行頭だけ() {
        let gr = g(r##"
[[syntax]]
name = "C2"
extensions = ["c2"]
line_comment = ["//"]
preproc = ["#"]
char_literal = true
strings = ["\""]
keywords = ["int"]
"##);
        kinds(&gr, "#include <stdio.h>\n", &[(Tok::Preproc, "#include")]);
        // 行の途中の `#` は指令ではない
        let got = toks(&gr, "int x = 1; # y\n");
        assert!(!got.iter().any(|(t, _)| *t == Tok::Preproc), "{got:?}");
    }

    #[test]
    fn 文字リテラルは有効な言語だけ() {
        let gr = g(r##"
[[syntax]]
name = "C3"
extensions = ["c3"]
char_literal = true
strings = ["\""]
"##);
        kinds(&gr, "c = 'a';\n", &[(Tok::Char, "'a'")]);
        kinds(&gr, "c = '\\n';\n", &[(Tok::Char, "'\\n'")]);
        // 閉じない `'` は 1 文字で諦める (Rust のライフタイム対策)
        let got = toks(&gr, "x: &'static str = 1;\n");
        assert!(got.iter().any(|(t, b)| *t == Tok::Number && b == "1"), "{got:?}");
    }

    #[test]
    fn 拡張子とファイル名から言語を引く() {
        let set = GrammarSet::parse_toml(
            r##"
[[syntax]]
name = "TypeScript"
extensions = ["ts", "mts"]

[[syntax]]
name = "Dockerfile"
extensions = ["dockerfile"]
filenames = ["dockerfile", "containerfile"]

[[alias]]
target = "HTML"
extensions = ["vue", "svelte"]
"##,
        )
        .unwrap();
        assert_eq!(set.detect_path(Path::new("/a/b/x.ts")).as_deref(), Some("TypeScript"));
        assert_eq!(set.detect_path(Path::new("/a/Dockerfile")).as_deref(), Some("Dockerfile"));
        assert_eq!(set.detect_path(Path::new("/a/app.vue")).as_deref(), Some("HTML"));
        assert_eq!(set.detect_path(Path::new("/a/x.unknown")), None);
        assert_eq!(set.detect_token("ts").as_deref(), Some("TypeScript"));
        assert_eq!(set.detect_token("TypeScript").as_deref(), Some("TypeScript"));
        assert_eq!(set.detect_token("svelte").as_deref(), Some("HTML"));
    }

    #[test]
    fn 二重拡張子も引ける() {
        let set = GrammarSet::parse_toml(
            r##"
[[syntax]]
name = "Terraform"
extensions = ["tf", "tfvars"]
"##,
        )
        .unwrap();
        assert_eq!(
            set.detect_path(Path::new("/a/main.tf")).as_deref(),
            Some("Terraform")
        );
    }

    #[test]
    fn 壊れた定義はエラーになるが他は生き残る() {
        assert!(GrammarSet::parse_toml("[[syntax]]\nname = \"\"\n").is_err());
        assert!(GrammarSet::parse_toml("[[syntax]]\nname = \"X\"\nfold = \"zzz\"\n").is_err());
        assert!(GrammarSet::parse_toml("これはTOMLではない").is_err());
    }

    #[test]
    fn 同名は先に読んだ方が残る() {
        let mut a = GrammarSet::parse_toml("[[syntax]]\nname = \"X\"\nkeywords = [\"a\"]\n").unwrap();
        let b = GrammarSet::parse_toml("[[syntax]]\nname = \"X\"\nkeywords = [\"b\"]\n").unwrap();
        a.merge(b);
        assert_eq!(a.grammars.len(), 1);
        assert!(a.grammars[0].keywords.contains("a"));
    }

    #[test]
    fn 巨大な行でも走査は線形時間で終わる() {
        let gr = ts();
        let line = format!("{}\n", "const x = foo(1) + \"s\"; ".repeat(4000));
        let start = std::time::Instant::now();
        let mut st = ScanState::default();
        let mut out = Vec::new();
        scan_line(&gr, &line, &mut st, &mut out);
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "1 行の走査が遅すぎる: {:?}",
            start.elapsed()
        );
        assert!(!out.is_empty());
    }
}
