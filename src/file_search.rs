//! ワークスペース横断のテキスト検索と一括置換 (VS Code の「ファイル間で検索」相当)。
//!
//! 検索本体はワーカースレッドで走り、結果は mpsc でまとめて UI へ返す。
//! ファイル一覧は app.rs が既に持つ `file_index` (⌘P 用の索引) を流用するので、
//! .gitignore などの除外規則は索引側と一致する。
//!
//! # 公開 API (UI 配線はあとから)
//!
//! | 関数 | 用途 |
//! |------|------|
//! | [`spawn`] | 従来通りの「大文字小文字を無視した部分文字列検索」。既存の呼び出し元互換 |
//! | [`spawn_with_options`] | [`SearchOptions`] 付きの非同期検索。パターン不正なら `Err` |
//! | [`search_with_options`] | 同期検索。結果はファイル順で決定的 ([`SearchOutcome`]) |
//! | [`replace_all`] | 一括置換。既定は**ドライラン** ([`ReplaceReport`]) |
//! | [`glob_match`] | glob 1 本の判定 (テスト・UI プレビュー用) |
//! | [`path_allowed`] | include/exclude glob の合成判定 (exclude が勝つ) |
//!
//! # 正規表現について
//!
//! このクレートは `regex` クレートに依存していない (Cargo.toml は他所有者の管理下)。
//! そのため **自前のバックトラッキング型エンジン** を同梱している。対応構文は
//! [`RegexError::Unsupported`] のドキュメントに列挙した部分集合で、未対応の構文は
//! 「黙って別物として解釈する」のではなく **エラーを返す**。`regex` クレートが
//! 依存に入れば [`Matcher`] の中身を差し替えるだけで全構文へ広げられる。

use crate::textenc::Encoding;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};

/// 1 ヒット = (ファイル, 0-based 行番号, 行テキスト)。
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub path: PathBuf,
    pub line: usize,
    /// 表示用スニペット (前後の空白を落とし、長すぎる行は切る)。
    pub text: String,
    /// **元の行**の中でのマッチ開始バイト位置。`text` は空白を落とすので
    /// スニペット上の位置とは一致しない (ハイライトは元行を読み直すこと)。
    pub col: usize,
    /// マッチのバイト長。
    pub len: usize,
}

pub const MAX_HITS: usize = 500;
/// これより大きいファイルは検索しない (バイナリ/生成物対策)。
pub const MAX_FILE_BYTES: u64 = 1_500_000;
/// 表示用スニペットの最大文字数。
const MAX_SNIPPET_CHARS: usize = 240;
/// 1 行 1 回のマッチ試行で許すバックトラック回数。
/// 破滅的バックトラック (`(a+)+$` 等) で固まらないための保険。
const REGEX_STEP_BUDGET: usize = 100_000;
/// 正規表現プログラムの最大命令数 (`a{1000}{1000}` 対策)。
const REGEX_PROGRAM_LIMIT: usize = 20_000;
/// `{n,m}` の上限。
const REGEX_REPEAT_LIMIT: u32 = 1_000;

// ─────────────────────────── 検索オプション ───────────────────────────

/// 検索の挙動。[`Default`] は**従来の挙動**と同じ
/// (部分一致・大文字小文字無視・glob なし・500 件 / 1.5MB 上限)。
#[derive(Clone, Debug)]
pub struct SearchOptions {
    /// 検索語。`regex` が true ならパターン。空なら常に 0 件。
    pub query: String,
    pub case_sensitive: bool,
    /// 前後が「単語文字」でないマッチだけを拾う。
    /// 単語文字は `char::is_alphanumeric() || '_'` (Unicode 準拠)。
    /// 日本語のように区切りが無い文はひとかたまりの単語として扱われる。
    pub whole_word: bool,
    /// `query` を正規表現として解釈する (自前エンジン。モジュール冒頭の注記参照)。
    pub regex: bool,
    /// 空でなければ「どれかに一致するファイルだけ」を対象にする。
    pub include_globs: Vec<String>,
    /// どれかに一致したファイルは除外する。**include より強い**。
    pub exclude_globs: Vec<String>,
    pub max_results: usize,
    pub max_file_bytes: u64,
    /// シンボリックリンクを辿るか。既定 true = 従来の挙動。
    /// 置換では false のときリンクを飛ばし、true のとき実体へ書く
    /// (リンクを実ファイルで上書きしない)。
    pub follow_symlinks: bool,
    /// glob をこのディレクトリからの相対パスに対して当てる。
    /// `None` (または配下でないパス) のときは絶対パスに対し `**/` を前置して当てる。
    pub root: Option<PathBuf>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            query: String::new(),
            case_sensitive: false,
            whole_word: false,
            regex: false,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            max_results: MAX_HITS,
            max_file_bytes: MAX_FILE_BYTES,
            follow_symlinks: true,
            root: None,
        }
    }
}

impl SearchOptions {
    /// 従来通りの部分一致検索。
    pub fn literal(query: impl Into<String>) -> Self {
        Self { query: query.into(), ..Self::default() }
    }
}

/// [`search_with_options`] の結果。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchOutcome {
    pub hits: Vec<Hit>,
    /// 実際に中身を読んで走査したファイル数 (除外・バイナリ・巨大ファイルは数えない)。
    pub files_scanned: usize,
    /// `max_results` に達して打ち切ったか。
    pub truncated: bool,
}

/// パターンのコンパイル失敗。UI 側で翻訳できるよう構造体で返す。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchError {
    Regex(RegexError),
}

/// 自前正規表現エンジンのエラー。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegexError {
    /// 構文エラー (閉じ括弧が無い等)。
    Syntax(String),
    /// このエンジンが対応していない構文。
    ///
    /// 対応済み: 文字 / `.` / `^` `$` / `[...]` (`^` 否定・範囲・クラス略記) /
    /// `\d \D \w \W \s \S \b \B \t \n \r` と記号のエスケープ /
    /// `( )` `(?: )` / `|` / `* + ?` と `{n}` `{n,}` `{n,m}` (末尾 `?` で最短一致)。
    /// 未対応: 後方参照・先読み/後読み・名前付きグループ・インラインフラグ・`\p{...}`。
    Unsupported(String),
    /// パターンが大きすぎる。
    TooLarge,
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchError::Regex(e) => write!(f, "{e}"),
        }
    }
}

impl std::fmt::Display for RegexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegexError::Syntax(m) => write!(f, "正規表現の構文エラー: {m}"),
            RegexError::Unsupported(m) => write!(f, "未対応の正規表現構文: {m}"),
            RegexError::TooLarge => write!(f, "正規表現が大きすぎます"),
        }
    }
}

impl std::error::Error for SearchError {}

// ─────────────────────────── glob ───────────────────────────

/// パス比較を大文字小文字無視で行うか。Windows のファイルシステム意味論に合わせる。
/// (macOS も既定は大文字小文字を区別しないが、区別するボリュームも作れるため
///  VS Code と同じく「区別する」側に倒す。)
const PATH_CASE_INSENSITIVE: bool = cfg!(windows);

/// 文字 1 個の畳み込み。ASCII 前提にしない (Unicode の小文字化の先頭文字を使う)。
fn fold(c: char, case_insensitive: bool) -> char {
    if case_insensitive {
        c.to_lowercase().next().unwrap_or(c)
    } else {
        c
    }
}

/// パス文字列を glob 用に正規化する。区切りは `/` に寄せ、先頭の `./` を落とす。
///
/// `\` は (Unix でも) 区切りとして扱う。Windows 形式のパスを渡した設定ファイルや
/// テストがそのまま通るようにするため。Unix のファイル名に `\` を含めるのは
/// 事実上ありえないので、この単純化を採る。
fn normalize_for_glob(s: &str) -> String {
    let mut out = s.replace('\\', "/");
    while let Some(rest) = out.strip_prefix("./") {
        out = rest.to_string();
    }
    out
}

/// glob 1 本の判定。パターン・パスとも [`normalize_for_glob`] で正規化してから当てる。
///
/// 文法 (VS Code 準拠):
/// - `*` … `/` を跨がない 0 文字以上
/// - `**` … `/` を跨ぐ 0 文字以上。`**/` は「0 段以上のディレクトリ」
/// - `?` … `/` 以外の 1 文字
/// - `[abc]` `[a-z]` `[!abc]` `[^abc]` … 文字クラス
/// - `/` を含まないパターンは `**/` を前置したものとして扱う (`*.rs` = 全階層の .rs)
/// - 末尾 `/**` はそのディレクトリ自身にも一致する (`target/**` は `target` にも当たる)
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pat = normalize_for_glob(pattern);
    let p = normalize_for_glob(path);
    let effective = if pat.contains('/') { pat } else { format!("**/{pat}") };
    let pc: Vec<char> = effective.chars().collect();
    let sc: Vec<char> = p.chars().collect();
    glob_here(&pc, &sc, PATH_CASE_INSENSITIVE)
}

fn glob_here(p: &[char], s: &[char], ci: bool) -> bool {
    let Some(&head) = p.first() else {
        return s.is_empty();
    };
    match head {
        '*' if p.get(1) == Some(&'*') => {
            let mut i = 1;
            while p.get(i) == Some(&'*') {
                i += 1;
            }
            let rest = &p[i..];
            if rest.is_empty() {
                return true; // 末尾の `**` は残り全部に一致 (空も含む)
            }
            if rest[0] == '/' && glob_here(&rest[1..], s, ci) {
                return true; // `**/` は 0 段のディレクトリにも一致
            }
            (0..=s.len()).any(|k| glob_here(rest, &s[k..], ci))
        }
        '*' => {
            let rest = &p[1..];
            let mut k = 0;
            loop {
                if glob_here(rest, &s[k..], ci) {
                    return true;
                }
                if k >= s.len() || s[k] == '/' {
                    return false;
                }
                k += 1;
            }
        }
        '?' => !s.is_empty() && s[0] != '/' && glob_here(&p[1..], &s[1..], ci),
        '[' => match glob_class(p) {
            Some((matcher, next)) => {
                !s.is_empty() && matcher(s[0], ci) && glob_here(&p[next..], &s[1..], ci)
            }
            // 閉じない `[` はただの文字
            None => !s.is_empty() && s[0] == '[' && glob_here(&p[1..], &s[1..], ci),
        },
        // `dir/**` はディレクトリ自身にも一致させる
        '/' if s.is_empty() && p.len() == 3 && p[1] == '*' && p[2] == '*' => true,
        c => !s.is_empty() && fold(s[0], ci) == fold(c, ci) && glob_here(&p[1..], &s[1..], ci),
    }
}

/// `[...]` を読んで (判定クロージャ, `]` の次の位置) を返す。
#[allow(clippy::type_complexity)]
fn glob_class(p: &[char]) -> Option<(Box<dyn Fn(char, bool) -> bool>, usize)> {
    let mut i = 1;
    let negated = matches!(p.get(i), Some('!') | Some('^'));
    if negated {
        i += 1;
    }
    let mut items: Vec<(char, char)> = Vec::new();
    let mut first = true;
    loop {
        let c = *p.get(i)?;
        if c == ']' && !first {
            i += 1;
            break;
        }
        first = false;
        if p.get(i + 1) == Some(&'-') && p.get(i + 2).is_some_and(|c| *c != ']') {
            items.push((c, p[i + 2]));
            i += 3;
        } else {
            items.push((c, c));
            i += 1;
        }
    }
    let f = move |ch: char, ci: bool| {
        let hit = items.iter().any(|(a, b)| {
            let (a, b) = (*a, *b);
            (a..=b).contains(&ch)
                || (ci && {
                    let lo = ch.to_lowercase().next().unwrap_or(ch);
                    let up = ch.to_uppercase().next().unwrap_or(ch);
                    (a..=b).contains(&lo) || (a..=b).contains(&up)
                })
        });
        hit != negated
    };
    Some((Box::new(f), i))
}

/// glob を当てる対象の文字列を作る。`root` 配下なら相対パス、そうでなければ絶対パス。
fn glob_target(path: &Path, root: Option<&Path>) -> String {
    let rel = root.and_then(|r| path.strip_prefix(r).ok());
    let target = rel.unwrap_or(path);
    normalize_for_glob(&target.to_string_lossy())
}

/// include / exclude の合成判定。**exclude が勝つ**。
/// include が空なら「全部許可」。
pub fn path_allowed(
    path: &Path,
    root: Option<&Path>,
    include: &[String],
    exclude: &[String],
) -> bool {
    if include.is_empty() && exclude.is_empty() {
        return true;
    }
    let target = glob_target(path, root);
    // root 配下でない (= 絶対パスのまま当てる) ときは、相対 glob でも当たるように
    // `**/` を前置した形も試す。glob_match 側が `/` 無しパターンを前置するので、
    // ここでは `/` を含むパターンだけ面倒を見る。
    let absolute = root.is_none() || root.is_some_and(|r| path.strip_prefix(r).is_err());
    let hits = |pats: &[String]| {
        pats.iter().any(|g| {
            glob_match(g, &target)
                || (absolute && g.contains('/') && !g.starts_with('/') && !g.starts_with("**/") && {
                    glob_match(&format!("**/{g}"), &target)
                })
        })
    };
    if hits(exclude) {
        return false;
    }
    include.is_empty() || hits(include)
}

// ─────────────────────────── マッチャ ───────────────────────────

/// 単語文字の定義。`char::is_alphanumeric` なので日本語もこちら側に入る
/// (= 区切りの無い CJK 文はひとかたまりの単語として振る舞う)。
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn word_boundary_ok(line: &str, start: usize, end: usize) -> bool {
    let before = line[..start].chars().next_back();
    let after = line[end..].chars().next();
    !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
}

/// コンパイル済みの検索パターン。スレッド間で共有できる (不変データのみ)。
#[derive(Clone, Debug)]
pub struct Matcher {
    kind: Kind,
    case_sensitive: bool,
    whole_word: bool,
}

#[derive(Clone, Debug)]
enum Kind {
    /// 空クエリ。何にも一致しない。
    Never,
    /// 部分一致。`ascii_needle` があるときはバイト走査の速い経路を使う。
    Literal { chars: Vec<char>, ascii_needle: Option<Vec<u8>> },
    Regex(Program),
}

impl Matcher {
    /// [`SearchOptions`] からパターンを組み立てる。
    pub fn compile(opts: &SearchOptions) -> Result<Self, SearchError> {
        let kind = if opts.query.is_empty() {
            Kind::Never
        } else if opts.regex {
            Kind::Regex(Program::compile(&opts.query, !opts.case_sensitive).map_err(SearchError::Regex)?)
        } else if opts.query.is_ascii() {
            let needle: Vec<u8> = if opts.case_sensitive {
                opts.query.bytes().collect()
            } else {
                opts.query.bytes().map(|b| b.to_ascii_lowercase()).collect()
            };
            Kind::Literal { chars: opts.query.chars().collect(), ascii_needle: Some(needle) }
        } else {
            let chars: Vec<char> = if opts.case_sensitive {
                opts.query.chars().collect()
            } else {
                opts.query.chars().map(|c| fold(c, true)).collect()
            };
            Kind::Literal { chars, ascii_needle: None }
        };
        Ok(Self { kind, case_sensitive: opts.case_sensitive, whole_word: opts.whole_word })
    }

    /// 1 行の中で `from` バイト以降の最初のマッチ (バイト範囲) を返す。
    pub fn find_from(&self, line: &str, from: usize) -> Option<(usize, usize)> {
        let mut at = from;
        loop {
            let (s, e) = match &self.kind {
                Kind::Never => return None,
                Kind::Literal { chars, ascii_needle } => {
                    self.literal_find(line, at, chars, ascii_needle.as_deref())?
                }
                Kind::Regex(prog) => prog.find_from(line, at)?,
            };
            if !self.whole_word || word_boundary_ok(line, s, e) {
                return Some((s, e));
            }
            // 1 文字進めて再挑戦 (境界条件で落ちただけなので探索は続ける)
            let step = line[s..].chars().next().map_or(1, char::len_utf8);
            at = s + step;
            if at > line.len() {
                return None;
            }
        }
    }

    /// 行にマッチがあるか。
    #[allow(dead_code)] // UI 配線待ち
    pub fn is_match(&self, line: &str) -> bool {
        self.find_from(line, 0).is_some()
    }

    /// 行の中の重ならないマッチを全部返す (バイト範囲)。
    #[allow(dead_code)] // UI 配線待ち
    pub fn find_all(&self, line: &str) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at <= line.len() {
            let Some((s, e)) = self.find_from(line, at) else { break };
            out.push((s, e));
            at = if e > s {
                e
            } else {
                // 空マッチ: 1 文字進めないと無限ループになる
                s + line[s..].chars().next().map_or(1, char::len_utf8)
            };
        }
        out
    }

    fn literal_find(
        &self,
        line: &str,
        from: usize,
        chars: &[char],
        ascii: Option<&[u8]>,
    ) -> Option<(usize, usize)> {
        if let Some(needle) = ascii {
            // needle が ASCII なら UTF-8 の多バイト部 (>=0x80) と衝突しないので
            // バイト走査で安全に (かつ文字境界を保って) 探せる。
            let h = line.as_bytes();
            let n = needle.len();
            if n == 0 || h.len() < n || from > h.len() - n {
                return None;
            }
            for i in from..=(h.len() - n) {
                let ok = (0..n).all(|j| {
                    if self.case_sensitive {
                        h[i + j] == needle[j]
                    } else {
                        h[i + j].to_ascii_lowercase() == needle[j]
                    }
                });
                if ok {
                    return Some((i, i + n));
                }
            }
            return None;
        }
        // 非 ASCII: 文字単位で走査しつつバイト位置を持ち回る
        let n = chars.len();
        if n == 0 {
            return None;
        }
        let ci = !self.case_sensitive;
        for (bi, _) in line.char_indices().filter(|(bi, _)| *bi >= from) {
            let mut it = line[bi..].chars();
            let mut matched = true;
            for want in chars {
                match it.next() {
                    Some(c) if fold(c, ci) == *want => {}
                    _ => {
                        matched = false;
                        break;
                    }
                }
            }
            if matched {
                let consumed = line[bi..].len() - it.as_str().len();
                return Some((bi, bi + consumed));
            }
        }
        None
    }
}

// ─────────────────── 自前の正規表現エンジン (部分集合) ───────────────────

#[derive(Clone, Debug)]
enum Node {
    Char(char),
    Any,
    Class(ClassSet),
    Start,
    End,
    Boundary(bool),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat { node: Box<Node>, min: u32, max: Option<u32>, greedy: bool },
}

#[derive(Clone, Debug, Default)]
struct ClassSet {
    negated: bool,
    ranges: Vec<(char, char)>,
    /// `\d` `\w` `\s` (bool = 否定形か)
    shorthands: Vec<(Short, bool)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Short {
    Digit,
    Word,
    Space,
}

impl ClassSet {
    fn contains(&self, c: char, ci: bool) -> bool {
        let mut cand = vec![c];
        if ci {
            if let Some(l) = c.to_lowercase().next() {
                cand.push(l);
            }
            if let Some(u) = c.to_uppercase().next() {
                cand.push(u);
            }
        }
        let raw = cand.iter().any(|ch| {
            self.ranges.iter().any(|(a, b)| (*a..=*b).contains(ch))
                || self.shorthands.iter().any(|(s, neg)| short_match(*s, *ch) != *neg)
        });
        raw != self.negated
    }
}

fn short_match(s: Short, c: char) -> bool {
    match s {
        Short::Digit => c.is_ascii_digit(),
        Short::Word => is_word_char(c),
        Short::Space => c.is_whitespace(),
    }
}

#[derive(Clone, Copy, Debug)]
enum Inst {
    Char(char),
    Any,
    Class(usize),
    Start,
    End,
    Boundary(bool),
    /// 先に `0`、失敗したら `1` を試す
    Split(usize, usize),
    Jmp(usize),
    Match,
}

/// コンパイル済みプログラム (バックトラッキング VM)。
#[derive(Clone, Debug)]
struct Program {
    insts: Vec<Inst>,
    classes: Vec<ClassSet>,
    ci: bool,
}

impl Program {
    fn compile(pattern: &str, ci: bool) -> Result<Self, RegexError> {
        let mut p = Parser { src: pattern.chars().collect(), i: 0 };
        let node = p.parse_alt()?;
        if p.i < p.src.len() {
            return Err(RegexError::Syntax(format!("余分な `{}`", p.src[p.i])));
        }
        let mut prog = Program { insts: Vec::new(), classes: Vec::new(), ci };
        prog.emit_node(&node)?;
        prog.insts.push(Inst::Match);
        Ok(prog)
    }

    fn push(&mut self, i: Inst) -> Result<usize, RegexError> {
        if self.insts.len() >= REGEX_PROGRAM_LIMIT {
            return Err(RegexError::TooLarge);
        }
        self.insts.push(i);
        Ok(self.insts.len() - 1)
    }

    fn emit_node(&mut self, n: &Node) -> Result<(), RegexError> {
        match n {
            Node::Char(c) => {
                self.push(Inst::Char(fold(*c, self.ci)))?;
            }
            Node::Any => {
                self.push(Inst::Any)?;
            }
            Node::Class(cs) => {
                self.classes.push(cs.clone());
                let idx = self.classes.len() - 1;
                self.push(Inst::Class(idx))?;
            }
            Node::Start => {
                self.push(Inst::Start)?;
            }
            Node::End => {
                self.push(Inst::End)?;
            }
            Node::Boundary(want) => {
                self.push(Inst::Boundary(*want))?;
            }
            Node::Concat(v) => {
                for x in v {
                    self.emit_node(x)?;
                }
            }
            Node::Alt(branches) => {
                let mut jmps = Vec::new();
                let last = branches.len() - 1;
                for (i, b) in branches.iter().enumerate() {
                    if i == last {
                        self.emit_node(b)?;
                    } else {
                        let sp = self.push(Inst::Split(0, 0))?;
                        self.emit_node(b)?;
                        jmps.push(self.push(Inst::Jmp(0))?);
                        let next = self.insts.len();
                        self.insts[sp] = Inst::Split(sp + 1, next);
                    }
                }
                let end = self.insts.len();
                for j in jmps {
                    self.insts[j] = Inst::Jmp(end);
                }
            }
            Node::Repeat { node, min, max, greedy } => {
                if let Some(m) = max {
                    if *m < *min {
                        return Err(RegexError::Syntax("{n,m} の n > m".into()));
                    }
                }
                if *min > REGEX_REPEAT_LIMIT || max.is_some_and(|m| m > REGEX_REPEAT_LIMIT) {
                    return Err(RegexError::TooLarge);
                }
                if max.is_none() && nullable(node) {
                    // `(a?)*` の類は空マッチで無限ループするので受け付けない
                    return Err(RegexError::Unsupported(
                        "空文字に一致し得る要素の無制限繰り返し".into(),
                    ));
                }
                for _ in 0..*min {
                    self.emit_node(node)?;
                }
                match max {
                    None => self.emit_star(node, *greedy)?,
                    Some(m) => {
                        for _ in *min..*m {
                            self.emit_opt(node, *greedy)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn emit_star(&mut self, node: &Node, greedy: bool) -> Result<(), RegexError> {
        let sp = self.push(Inst::Split(0, 0))?;
        self.emit_node(node)?;
        self.push(Inst::Jmp(sp))?;
        let after = self.insts.len();
        self.insts[sp] =
            if greedy { Inst::Split(sp + 1, after) } else { Inst::Split(after, sp + 1) };
        Ok(())
    }

    fn emit_opt(&mut self, node: &Node, greedy: bool) -> Result<(), RegexError> {
        let sp = self.push(Inst::Split(0, 0))?;
        self.emit_node(node)?;
        let after = self.insts.len();
        self.insts[sp] =
            if greedy { Inst::Split(sp + 1, after) } else { Inst::Split(after, sp + 1) };
        Ok(())
    }

    /// `from` バイト以降で最初に一致するバイト範囲を返す (leftmost, Perl 流の優先順)。
    fn find_from(&self, line: &str, from: usize) -> Option<(usize, usize)> {
        let chars: Vec<char> = line.chars().collect();
        let mut offs: Vec<usize> = line.char_indices().map(|(i, _)| i).collect();
        offs.push(line.len());
        let start_idx = offs.iter().position(|o| *o >= from)?;
        let mut budget = REGEX_STEP_BUDGET;
        for s in start_idx..offs.len() {
            if let Some(e) = self.run(&chars, s, &mut budget) {
                return Some((offs[s], offs[e]));
            }
            if budget == 0 {
                return None; // 予算切れ: これ以上粘らない (固まらないことを優先)
            }
        }
        None
    }

    fn run(&self, s: &[char], start: usize, budget: &mut usize) -> Option<usize> {
        let mut stack: Vec<(usize, usize)> = vec![(0, start)];
        while let Some((mut pc, mut pos)) = stack.pop() {
            loop {
                if *budget == 0 {
                    return None;
                }
                *budget -= 1;
                match self.insts[pc] {
                    Inst::Char(c) => {
                        if pos < s.len() && fold(s[pos], self.ci) == c {
                            pc += 1;
                            pos += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Any => {
                        if pos < s.len() && s[pos] != '\n' {
                            pc += 1;
                            pos += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Class(idx) => {
                        if pos < s.len() && self.classes[idx].contains(s[pos], self.ci) {
                            pc += 1;
                            pos += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Start => {
                        if pos == 0 {
                            pc += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::End => {
                        if pos == s.len() {
                            pc += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Boundary(want) => {
                        let before = pos > 0 && is_word_char(s[pos - 1]);
                        let after = pos < s.len() && is_word_char(s[pos]);
                        if (before != after) == want {
                            pc += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Split(a, b) => {
                        stack.push((b, pos));
                        pc = a;
                    }
                    Inst::Jmp(t) => pc = t,
                    Inst::Match => return Some(pos),
                }
            }
        }
        None
    }
}

/// 空文字に一致し得るか (無制限繰り返しの安全弁)。
fn nullable(n: &Node) -> bool {
    match n {
        Node::Char(_) | Node::Any | Node::Class(_) => false,
        Node::Start | Node::End | Node::Boundary(_) => true,
        Node::Concat(v) => v.iter().all(nullable),
        Node::Alt(v) => v.iter().any(nullable),
        Node::Repeat { node, min, .. } => *min == 0 || nullable(node),
    }
}

struct Parser {
    src: Vec<char>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.src.get(self.i).copied()
    }

    fn parse_alt(&mut self) -> Result<Node, RegexError> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some('|') {
            self.i += 1;
            branches.push(self.parse_concat()?);
        }
        Ok(if branches.len() == 1 { branches.pop().expect("1 要素") } else { Node::Alt(branches) })
    }

    fn parse_concat(&mut self) -> Result<Node, RegexError> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            items.push(self.parse_repeat()?);
        }
        Ok(match items.len() {
            1 => items.pop().expect("1 要素"),
            _ => Node::Concat(items),
        })
    }

    fn parse_repeat(&mut self) -> Result<Node, RegexError> {
        let atom = self.parse_atom()?;
        let (min, max) = match self.peek() {
            Some('*') => {
                self.i += 1;
                (0, None)
            }
            Some('+') => {
                self.i += 1;
                (1, None)
            }
            Some('?') => {
                self.i += 1;
                (0, Some(1))
            }
            Some('{') => match self.try_parse_braces()? {
                Some(mm) => mm,
                None => return Ok(atom),
            },
            _ => return Ok(atom),
        };
        let greedy = if self.peek() == Some('?') {
            self.i += 1;
            false
        } else {
            if self.peek() == Some('+') {
                return Err(RegexError::Unsupported("所有量指定子 `+`".into()));
            }
            true
        };
        Ok(Node::Repeat { node: Box::new(atom), min, max, greedy })
    }

    /// `{n}` `{n,}` `{n,m}` を読む。数値でなければ `{` はただの文字として扱う。
    fn try_parse_braces(&mut self) -> Result<Option<(u32, Option<u32>)>, RegexError> {
        let save = self.i;
        self.i += 1; // '{'
        let mut min = String::new();
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            min.push(self.src[self.i]);
            self.i += 1;
        }
        if min.is_empty() {
            self.i = save;
            return Ok(None);
        }
        let min_v: u32 = min.parse().map_err(|_| RegexError::TooLarge)?;
        match self.peek() {
            Some('}') => {
                self.i += 1;
                Ok(Some((min_v, Some(min_v))))
            }
            Some(',') => {
                self.i += 1;
                let mut max = String::new();
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    max.push(self.src[self.i]);
                    self.i += 1;
                }
                if self.peek() != Some('}') {
                    self.i = save;
                    return Ok(None);
                }
                self.i += 1;
                if max.is_empty() {
                    Ok(Some((min_v, None)))
                } else {
                    Ok(Some((min_v, Some(max.parse().map_err(|_| RegexError::TooLarge)?))))
                }
            }
            _ => {
                self.i = save;
                Ok(None)
            }
        }
    }

    fn parse_atom(&mut self) -> Result<Node, RegexError> {
        let c = self.peek().ok_or_else(|| RegexError::Syntax("パターンが途中で終わった".into()))?;
        match c {
            '(' => {
                self.i += 1;
                if self.peek() == Some('?') {
                    match self.src.get(self.i + 1) {
                        Some(':') => self.i += 2,
                        Some('=') | Some('!') => {
                            return Err(RegexError::Unsupported("先読み `(?=` `(?!`".into()))
                        }
                        Some('<') => {
                            return Err(RegexError::Unsupported(
                                "後読み / 名前付きグループ `(?<`".into(),
                            ))
                        }
                        Some('P') => {
                            return Err(RegexError::Unsupported("名前付きグループ `(?P<`".into()))
                        }
                        _ => return Err(RegexError::Unsupported("インラインフラグ `(?…)`".into())),
                    }
                }
                let inner = self.parse_alt()?;
                if self.peek() != Some(')') {
                    return Err(RegexError::Syntax("`)` が足りない".into()));
                }
                self.i += 1;
                Ok(inner)
            }
            ')' => Err(RegexError::Syntax("対応する `(` が無い `)`".into())),
            '[' => self.parse_class(),
            '.' => {
                self.i += 1;
                Ok(Node::Any)
            }
            '^' => {
                self.i += 1;
                Ok(Node::Start)
            }
            '$' => {
                self.i += 1;
                Ok(Node::End)
            }
            '*' | '+' | '?' => {
                Err(RegexError::Syntax(format!("繰り返し `{c}` の対象がない")))
            }
            '\\' => {
                self.i += 1;
                let e = self
                    .peek()
                    .ok_or_else(|| RegexError::Syntax("`\\` で終わっている".into()))?;
                self.i += 1;
                match e {
                    'd' => Ok(Node::Class(ClassSet {
                        shorthands: vec![(Short::Digit, false)],
                        ..Default::default()
                    })),
                    'D' => Ok(Node::Class(ClassSet {
                        shorthands: vec![(Short::Digit, false)],
                        negated: true,
                        ..Default::default()
                    })),
                    'w' => Ok(Node::Class(ClassSet {
                        shorthands: vec![(Short::Word, false)],
                        ..Default::default()
                    })),
                    'W' => Ok(Node::Class(ClassSet {
                        shorthands: vec![(Short::Word, false)],
                        negated: true,
                        ..Default::default()
                    })),
                    's' => Ok(Node::Class(ClassSet {
                        shorthands: vec![(Short::Space, false)],
                        ..Default::default()
                    })),
                    'S' => Ok(Node::Class(ClassSet {
                        shorthands: vec![(Short::Space, false)],
                        negated: true,
                        ..Default::default()
                    })),
                    'b' => Ok(Node::Boundary(true)),
                    'B' => Ok(Node::Boundary(false)),
                    'n' => Ok(Node::Char('\n')),
                    't' => Ok(Node::Char('\t')),
                    'r' => Ok(Node::Char('\r')),
                    '0' => Ok(Node::Char('\0')),
                    'p' | 'P' => Err(RegexError::Unsupported("Unicode 特性 `\\p{…}`".into())),
                    'k' => Err(RegexError::Unsupported("名前付き後方参照 `\\k<…>`".into())),
                    c if c.is_ascii_digit() => {
                        Err(RegexError::Unsupported("後方参照 `\\1`".into()))
                    }
                    c if c.is_alphanumeric() => {
                        Err(RegexError::Unsupported(format!("エスケープ `\\{c}`")))
                    }
                    c => Ok(Node::Char(c)),
                }
            }
            c => {
                self.i += 1;
                Ok(Node::Char(c))
            }
        }
    }

    fn parse_class(&mut self) -> Result<Node, RegexError> {
        self.i += 1; // '['
        let mut set = ClassSet::default();
        if self.peek() == Some('^') {
            set.negated = true;
            self.i += 1;
        }
        let mut first = true;
        loop {
            let c = self
                .peek()
                .ok_or_else(|| RegexError::Syntax("`]` が足りない".into()))?;
            if c == ']' && !first {
                self.i += 1;
                break;
            }
            first = false;
            let lo = if c == '\\' {
                self.i += 1;
                let e = self
                    .peek()
                    .ok_or_else(|| RegexError::Syntax("`\\` で終わっている".into()))?;
                self.i += 1;
                match e {
                    'd' => {
                        set.shorthands.push((Short::Digit, false));
                        continue;
                    }
                    'D' => {
                        set.shorthands.push((Short::Digit, true));
                        continue;
                    }
                    'w' => {
                        set.shorthands.push((Short::Word, false));
                        continue;
                    }
                    'W' => {
                        set.shorthands.push((Short::Word, true));
                        continue;
                    }
                    's' => {
                        set.shorthands.push((Short::Space, false));
                        continue;
                    }
                    'S' => {
                        set.shorthands.push((Short::Space, true));
                        continue;
                    }
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    c if c.is_alphanumeric() => {
                        return Err(RegexError::Unsupported(format!("クラス内 `\\{c}`")))
                    }
                    c => c,
                }
            } else {
                self.i += 1;
                c
            };
            if self.peek() == Some('-') && self.src.get(self.i + 1).is_some_and(|c| *c != ']') {
                self.i += 1;
                let hi = self.src[self.i];
                self.i += 1;
                if hi < lo {
                    return Err(RegexError::Syntax("文字クラスの範囲が逆順".into()));
                }
                set.ranges.push((lo, hi));
            } else {
                set.ranges.push((lo, lo));
            }
        }
        Ok(Node::Class(set))
    }
}

// ─────────────────────────── ファイル読み ───────────────────────────

#[allow(dead_code)] // 置換 API の UI 配線待ち
enum Loaded {
    Text(String, Encoding),
    /// 検索対象外 (巨大・バイナリ・glob 不一致・シンボリックリンク)。
    Skipped,
    Error(String),
}

fn load_text(path: &Path, opts: &SearchOptions) -> Loaded {
    if !path_allowed(path, opts.root.as_deref(), &opts.include_globs, &opts.exclude_globs) {
        return Loaded::Skipped;
    }
    if !opts.follow_symlinks {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => return Loaded::Skipped,
            Ok(_) => {}
            Err(e) => return Loaded::Error(e.to_string()),
        }
    }
    match std::fs::metadata(path) {
        Ok(m) if !m.is_file() => return Loaded::Skipped,
        Ok(m) if m.len() > opts.max_file_bytes => return Loaded::Skipped,
        Ok(_) => {}
        // 索引と実体がずれている (削除された等) のは日常茶飯事なのでエラーにしない
        Err(_) => return Loaded::Skipped,
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return Loaded::Error(e.to_string()),
    };
    if bytes.contains(&0) {
        return Loaded::Skipped; // バイナリ
    }
    // CP932 (Shift_JIS) のファイルも検索対象にする。lossy のままだと
    // 日本語の行が置換文字の列になり、絶対にヒットしない。
    let (text, enc) = crate::textenc::decode_bytes(&bytes);
    Loaded::Text(text, enc)
}

// ─────────────────────────── 検索本体 ───────────────────────────

/// 同期検索。結果は `files` の順序どおりで**決定的**。
///
/// 内部では [`std::thread::scope`] で分割並列に走らせつつ、
/// 各チャンクの結果を順に連結するので、並列でも並びは変わらない。
pub fn search_with_options(
    files: &[PathBuf],
    opts: &SearchOptions,
) -> Result<SearchOutcome, SearchError> {
    let matcher = Matcher::compile(opts)?;
    if files.is_empty() || matches!(matcher.kind, Kind::Never) || opts.max_results == 0 {
        return Ok(SearchOutcome::default());
    }

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(files.len())
        .max(1);
    let chunk = files.len().div_ceil(threads);
    let hit_count = AtomicUsize::new(0);
    let cancel = AtomicBool::new(false);

    let parts: Vec<(Vec<Hit>, usize)> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for t in 0..threads {
            let start = t * chunk;
            let end = (start + chunk).min(files.len());
            if start >= end {
                continue;
            }
            let (slice, m, o, hc, cx) = (&files[start..end], &matcher, opts, &hit_count, &cancel);
            handles.push(scope.spawn(move || scan_chunk(slice, m, o, hc, cx)));
        }
        // ワーカーが panic しても検索全体は落とさない (空の結果として扱う)
        handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
    });

    let mut hits = Vec::new();
    let mut files_scanned = 0;
    for (mut part, scanned) in parts {
        hits.append(&mut part);
        files_scanned += scanned;
    }
    let truncated = hits.len() >= opts.max_results;
    hits.truncate(opts.max_results);
    Ok(SearchOutcome { hits, files_scanned, truncated })
}

fn scan_chunk(
    files: &[PathBuf],
    matcher: &Matcher,
    opts: &SearchOptions,
    hit_count: &AtomicUsize,
    cancel: &AtomicBool,
) -> (Vec<Hit>, usize) {
    let mut hits = Vec::new();
    let mut scanned = 0usize;
    for p in files {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Loaded::Text(text, _) = load_text(p, opts) else { continue };
        scanned += 1;
        for (n, line) in text.lines().enumerate() {
            let Some((s, e)) = matcher.find_from(line, 0) else { continue };
            hits.push(Hit { path: p.clone(), line: n, text: snippet(line), col: s, len: e - s });
            if hit_count.fetch_add(1, Ordering::Relaxed) + 1 >= opts.max_results {
                cancel.store(true, Ordering::Relaxed);
                break;
            }
        }
    }
    (hits, scanned)
}

/// [`SearchOptions`] 付きの非同期検索。パターンが不正なら**その場で** `Err`。
pub fn spawn_with_options(
    files: Vec<PathBuf>,
    opts: SearchOptions,
) -> Result<Receiver<(Vec<Hit>, usize)>, SearchError> {
    // コンパイルだけ先にやってエラーを同期で返す (UI がすぐ赤くできる)
    let _ = Matcher::compile(&opts)?;
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let out = search_with_options(&files, &opts).unwrap_or_default();
        let _ = tx.send((out.hits, out.files_scanned));
    });
    Ok(rx)
}

/// 大文字小文字を無視した検索 (従来 API)。`files` は絶対パスの一覧。
/// CPU並列ワーカースレッドによりミリ秒単位の爆速検索を行う。
pub fn spawn(files: Vec<PathBuf>, query: String) -> Receiver<(Vec<Hit>, usize)> {
    let opts = SearchOptions::literal(query);
    match spawn_with_options(files, opts) {
        Ok(rx) => rx,
        // 文字列検索はコンパイルに失敗しないが、型のために空を返す道を用意する
        Err(_) => {
            let (tx, rx) = channel();
            let _ = tx.send((Vec::new(), 0));
            rx
        }
    }
}

/// 行テキストを表示用に短くする (先頭空白を落とし、長すぎる行を切る)。
fn snippet(line: &str) -> String {
    let t = line.trim();
    if t.chars().count() <= MAX_SNIPPET_CHARS {
        t.to_string()
    } else {
        let cut: String = t.chars().take(MAX_SNIPPET_CHARS).collect();
        format!("{cut}…")
    }
}

// ─────────────────────────── 一括置換 ───────────────────────────

/// [`replace_all`] の入力。`dry_run` は**既定 true** (UI のプレビュー優先)。
#[derive(Clone, Debug)]
#[allow(dead_code)] // UI 配線待ち
pub struct ReplaceRequest {
    pub options: SearchOptions,
    /// 置換後の文字列。`$1` などのグループ参照は**解釈しない** (そのまま入る)。
    pub replacement: String,
    pub dry_run: bool,
}

impl Default for ReplaceRequest {
    fn default() -> Self {
        Self { options: SearchOptions::default(), replacement: String::new(), dry_run: true }
    }
}

/// 1 ファイルの置換結果。
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // UI 配線待ち
pub struct FileChange {
    pub path: PathBuf,
    pub replacements: usize,
    /// 実際にディスクへ書いたか (ドライランなら false)。
    pub written: bool,
}

/// 置換できなかったファイルとその理由。
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // UI 配線待ち
pub struct ReplaceIssue {
    pub path: PathBuf,
    pub message: String,
}

/// [`replace_all`] の報告。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)] // UI 配線待ち
pub struct ReplaceReport {
    pub dry_run: bool,
    /// 中身を読んで検査したファイル数。
    pub files_scanned: usize,
    pub files_changed: usize,
    pub replacements: usize,
    pub changes: Vec<FileChange>,
    pub errors: Vec<ReplaceIssue>,
}

/// ワークスペース一括置換。
///
/// - 検索と同じ [`SearchOptions`] (glob・大文字小文字・単語単位・正規表現) で対象を絞る
/// - バイナリ / 上限超えのファイルは飛ばす
/// - 改行 (LF / CRLF / CR / 混在 / 末尾改行なし) と符号化 (BOM・UTF-16・CP932) を保つ
/// - 書き込みは**同じディレクトリに一時ファイル → rename** の原子的置換
/// - 1 ファイルが失敗しても残りは続行し、[`ReplaceReport::errors`] に積む
/// - `max_results` は**検索用の上限**であり、置換は打ち切らない (中途半端な置換を避ける)
#[allow(dead_code)] // UI 配線待ち
pub fn replace_all(files: &[PathBuf], req: &ReplaceRequest) -> Result<ReplaceReport, SearchError> {
    let matcher = Matcher::compile(&req.options)?;
    let mut report = ReplaceReport { dry_run: req.dry_run, ..Default::default() };
    if matches!(matcher.kind, Kind::Never) {
        return Ok(report);
    }

    for path in files {
        let (text, enc) = match load_text(path, &req.options) {
            Loaded::Text(t, e) => (t, e),
            Loaded::Skipped => continue,
            Loaded::Error(msg) => {
                report.errors.push(ReplaceIssue { path: path.clone(), message: msg });
                continue;
            }
        };
        report.files_scanned += 1;

        let (out, count) = replace_in_text(&text, &matcher, &req.replacement);
        if count == 0 {
            continue;
        }
        report.replacements += count;
        report.files_changed += 1;

        if req.dry_run {
            report.changes.push(FileChange {
                path: path.clone(),
                replacements: count,
                written: false,
            });
            continue;
        }

        // シンボリックリンクを辿る設定のときは実体へ書く
        // (rename でリンクそのものを実ファイルに置き換えてしまわないため)。
        let target = match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => {
                std::fs::canonicalize(path).unwrap_or_else(|_| path.clone())
            }
            _ => path.clone(),
        };
        let (bytes, _used) = crate::textenc::encode_bytes(&out, enc);
        match write_atomic(&target, &bytes) {
            Ok(()) => report.changes.push(FileChange {
                path: path.clone(),
                replacements: count,
                written: true,
            }),
            Err(e) => {
                report.replacements -= count;
                report.files_changed -= 1;
                report.errors.push(ReplaceIssue {
                    path: path.clone(),
                    message: format!("書き込みに失敗: {e}"),
                });
            }
        }
    }
    Ok(report)
}

/// 本文を行単位で置換する。行末 (LF/CRLF/CR/無し) はそのまま持ち越す。
#[allow(dead_code)]
fn replace_in_text(text: &str, matcher: &Matcher, replacement: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut count = 0usize;
    for raw in lines_with_endings(text) {
        let (content, eol) = split_eol(raw);
        let ranges = matcher.find_all(content);
        if ranges.is_empty() {
            out.push_str(raw);
            continue;
        }
        let mut at = 0;
        for (s, e) in &ranges {
            out.push_str(&content[at..*s]);
            out.push_str(replacement);
            at = *e;
        }
        out.push_str(&content[at..]);
        out.push_str(eol);
        count += ranges.len();
    }
    (out, count)
}

/// 行末込みで行に切る (`str::lines` と違い CR/LF を落とさない)。
#[allow(dead_code)]
fn lines_with_endings(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            out.push(&text[start..=i]);
            start = i + 1;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

#[allow(dead_code)]
fn split_eol(line: &str) -> (&str, &str) {
    if let Some(rest) = line.strip_suffix("\r\n") {
        (rest, "\r\n")
    } else if let Some(rest) = line.strip_suffix('\n') {
        (rest, "\n")
    } else if let Some(rest) = line.strip_suffix('\r') {
        (rest, "\r") // 古い Mac 形式
    } else {
        (line, "")
    }
}

/// 同じディレクトリに一時ファイルを作って rename する原子的な書き込み。
/// 途中で電源が落ちても「元のまま」か「新しい内容」のどちらかにしかならない。
#[allow(dead_code)]
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf);
    let dir = dir.unwrap_or_else(|| PathBuf::from("."));
    let stem = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(
        ".{stem}.zv-replace-{}-{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));

    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        // 実行ビットなどの属性を引き継ぐ (引き継げなくても本体の書き込みは続ける)
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;
    use std::path::Path;

    fn tmp(tag: &str) -> PathBuf {
        unique_temp_dir("zv-fsearch", tag)
    }

    fn write(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&p, body).expect("write");
        p
    }

    fn collect(files: Vec<PathBuf>, q: &str) -> (Vec<Hit>, usize) {
        spawn(files, q.to_string())
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("search thread result")
    }

    fn search(files: &[PathBuf], opts: &SearchOptions) -> SearchOutcome {
        search_with_options(files, opts).expect("pattern compiles")
    }

    // ───────────────── 既存挙動 (後方互換) ─────────────────

    #[test]
    fn finds_case_insensitive_matches_with_line_numbers() {
        let dir = tmp("basic");
        let a = write(&dir, "a.txt", "hello\nWorld HELLO\nnope\n");
        let (hits, scanned) = collect(vec![a.clone()], "hello");
        assert_eq!(scanned, 1);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0], Hit { path: a.clone(), line: 0, text: "hello".into(), col: 0, len: 5 });
        assert_eq!(hits[1].line, 1);
        assert_eq!(hits[1].col, 6);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skips_binary_and_missing_files() {
        let dir = tmp("bin");
        let b = dir.join("b.bin");
        std::fs::write(&b, [0u8, 1, 2, b'h', b'i']).expect("write");
        let (hits, scanned) =
            collect(vec![b, dir.join("does-not-exist.txt")], "hi");
        assert!(hits.is_empty());
        assert_eq!(scanned, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn caps_hits_at_max() {
        let dir = tmp("cap");
        let c = write(&dir, "c.txt", &"match\n".repeat(MAX_HITS + 50));
        let (hits, _) = collect(vec![c], "match");
        assert_eq!(hits.len(), MAX_HITS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn max_results_stops_early() {
        let dir = tmp("cap2");
        let c = write(&dir, "c.txt", &"match\n".repeat(1000));
        let opts = SearchOptions { max_results: 7, ..SearchOptions::literal("match") };
        let out = search(&[c], &opts);
        assert_eq!(out.hits.len(), 7);
        assert!(out.truncated);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_files_are_skipped() {
        let dir = tmp("big");
        let big = write(&dir, "big.txt", &"needle\n".repeat(200));
        let opts = SearchOptions { max_file_bytes: 10, ..SearchOptions::literal("needle") };
        let out = search(&[big.clone()], &opts);
        assert!(out.hits.is_empty() && out.files_scanned == 0);
        // 上限を上げれば見つかる = スキップ理由がサイズであることの裏取り
        let opts = SearchOptions { max_file_bytes: 1 << 20, ..SearchOptions::literal("needle") };
        assert_eq!(search(&[big], &opts).files_scanned, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snippet_trims_and_caps() {
        assert_eq!(snippet("   abc  "), "abc");
        let long = "あ".repeat(MAX_SNIPPET_CHARS + 10);
        let s = snippet(&long);
        assert!(s.chars().count() == MAX_SNIPPET_CHARS + 1 && s.ends_with('…'));
    }

    // ───────────────── glob ─────────────────

    #[test]
    fn glob_table() {
        let cases: &[(&str, &str, bool)] = &[
            // `**` はディレクトリを跨ぐ
            ("src/**/*.rs", "src/a.rs", true),
            ("src/**/*.rs", "src/deep/deeper/a.rs", true),
            ("src/**/*.rs", "other/a.rs", false),
            ("**/*.rs", "a/b/c.rs", true),
            // `*` は区切りを跨がない
            ("src/*.rs", "src/a.rs", true),
            ("src/*.rs", "src/deep/a.rs", false),
            ("*.rs", "a.rs", true),
            ("*.rs", "deep/a.rs", true), // `/` 無しパターンは全階層
            ("*.rs", "deep/a.rst", false),
            // `?`
            ("a?c.txt", "abc.txt", true),
            ("a?c.txt", "ac.txt", false),
            ("a?c", "a/c", false), // `?` は区切りに当たらない
            // 文字クラス
            ("f[oa]o.txt", "foo.txt", true),
            ("f[oa]o.txt", "fao.txt", true),
            ("f[!oa]o.txt", "foo.txt", false),
            ("f[a-z]o.txt", "fzo.txt", true),
            // 先頭 ./ の正規化
            ("./src/**", "src/a.rs", true),
            ("src/**", "./src/a.rs", true),
            // ディレクトリ自身にも当たる末尾 /**
            ("target/**", "target", true),
            ("target/**", "target/debug/x.o", true),
            ("target/**", "src/target.rs", false),
            // Windows 形式の入力パス
            ("src/**/*.rs", "src\\deep\\a.rs", true),
            ("src\\*.rs", "src/a.rs", true),
            // `**/` は 0 段のディレクトリにも一致
            ("**/foo.txt", "foo.txt", true),
            ("a/**/b.txt", "a/b.txt", true),
            ("a/**/b.txt", "a/x/y/b.txt", true),
        ];
        for (pat, path, want) in cases {
            assert_eq!(glob_match(pat, path), *want, "glob_match({pat:?}, {path:?})");
        }
    }

    #[test]
    fn glob_case_rule_follows_os() {
        // Windows はファイルシステムに合わせて大文字小文字を無視する
        assert_eq!(glob_match("*.RS", "a.rs"), cfg!(windows));
        assert!(glob_match("*.rs", "a.rs"));
    }

    #[test]
    fn include_and_exclude_interact_with_exclude_winning() {
        let root = Path::new("/w");
        let rs = Path::new("/w/src/main.rs");
        let gen = Path::new("/w/src/generated/big.rs");
        let inc = vec!["src/**/*.rs".to_string()];
        let exc = vec!["**/generated/**".to_string()];
        assert!(path_allowed(rs, Some(root), &inc, &exc));
        assert!(!path_allowed(gen, Some(root), &inc, &exc)); // exclude が勝つ
        assert!(!path_allowed(Path::new("/w/docs/a.md"), Some(root), &inc, &[]));
        assert!(path_allowed(Path::new("/w/docs/a.md"), Some(root), &[], &[]));
        // root 無し = 絶対パスに対しても相対 glob が効く
        assert!(path_allowed(rs, None, &inc, &[]));
        assert!(!path_allowed(gen, None, &[], &exc));
    }

    #[test]
    fn include_exclude_filter_actual_search() {
        let dir = tmp("globfilter");
        let a = write(&dir, "src/a.rs", "needle\n");
        let b = write(&dir, "src/generated/b.rs", "needle\n");
        let c = write(&dir, "docs/c.md", "needle\n");
        let opts = SearchOptions {
            root: Some(dir.clone()),
            include_globs: vec!["src/**/*.rs".into()],
            exclude_globs: vec!["**/generated/**".into()],
            ..SearchOptions::literal("needle")
        };
        let out = search(&[a.clone(), b, c], &opts);
        assert_eq!(out.files_scanned, 1);
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].path, a);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ───────────────── マッチャ ─────────────────

    fn matcher(opts: &SearchOptions) -> Matcher {
        Matcher::compile(opts).expect("compiles")
    }

    #[test]
    fn literal_case_sensitivity() {
        let ci = matcher(&SearchOptions::literal("Foo"));
        assert!(ci.is_match("xxfooxx") && ci.is_match("FOO"));
        let cs = matcher(&SearchOptions { case_sensitive: true, ..SearchOptions::literal("Foo") });
        assert!(!cs.is_match("xxfooxx"));
        assert_eq!(cs.find_from("a Foo b", 0), Some((2, 5)));
    }

    #[test]
    fn literal_non_ascii_case_folding() {
        let m = matcher(&SearchOptions::literal("Straße"));
        assert_eq!(m.find_from("ab straße cd", 0), Some((3, 10)));
        let jp = matcher(&SearchOptions::literal("検索"));
        assert_eq!(jp.find_from("全文検索の話", 0), Some((6, 12)));
    }

    #[test]
    fn whole_word_ascii_identifier() {
        let opts = SearchOptions { whole_word: true, ..SearchOptions::literal("foo") };
        let m = matcher(&opts);
        assert!(m.is_match("let foo = 1;"));
        assert!(m.is_match("(foo)"));
        assert!(!m.is_match("foobar"));
        assert!(!m.is_match("my_foo")); // `_` も単語文字
        assert!(!m.is_match("foo_bar"));
        assert_eq!(m.find_from("foobar foo", 0), Some((7, 10))); // 先頭で諦めない
    }

    #[test]
    fn whole_word_japanese_behaves_as_one_word() {
        let opts = SearchOptions { whole_word: true, ..SearchOptions::literal("検索") };
        let m = matcher(&opts);
        // 区切りが無いので「全文検索」の一部としては単語一致しない
        assert!(!m.is_match("全文検索する"));
        // 記号や空白で区切られていれば一致する
        assert!(m.is_match("「検索」"));
        assert!(m.is_match("全文 検索 の話"));
    }

    #[test]
    fn regex_basics() {
        let re = |q: &str| matcher(&SearchOptions { regex: true, ..SearchOptions::literal(q) });
        assert_eq!(re(r"\d+").find_from("ab 1234 cd", 0), Some((3, 7)));
        assert_eq!(re(r"^fn\s+(\w+)").find_from("fn  main() {", 0), Some((0, 8)));
        assert!(re(r"a.c").is_match("abc"));
        assert!(!re(r"a.c").is_match("ac"));
        assert!(re(r"foo|bar").is_match("xxbarxx"));
        assert!(re(r"colou?r").is_match("color") && re(r"colou?r").is_match("colour"));
        assert_eq!(re(r"a{2,3}").find_from("caaaab", 0), Some((1, 4)));
        assert_eq!(re(r"<.+?>").find_from("<a><b>", 0), Some((0, 3))); // 最短一致
        assert!(re(r"[A-Z][a-z]+").is_match("Hello"));
        assert!(re(r"\bfoo\b").is_match("a foo b"));
        assert!(!re(r"\bfoo\b").is_match("foobar"));
        assert_eq!(re(r"x$").find_from("axbx", 0), Some((3, 4)));
        assert!(re(r"\.rs$").is_match("main.rs"));
    }

    #[test]
    fn regex_is_case_insensitive_by_default_and_respects_the_flag() {
        let ci = matcher(&SearchOptions { regex: true, ..SearchOptions::literal("[a-z]+") });
        assert!(ci.is_match("ABC"));
        let cs = matcher(&SearchOptions {
            regex: true,
            case_sensitive: true,
            ..SearchOptions::literal("[a-z]+")
        });
        assert!(!cs.is_match("ABC"));
        assert!(cs.is_match("abc"));
    }

    #[test]
    fn regex_whole_word_and_japanese() {
        let m = matcher(&SearchOptions {
            regex: true,
            whole_word: true,
            ..SearchOptions::literal(r"\w+索")
        });
        assert!(m.is_match("全文検索"));
        assert!(!m.is_match("全文検索する")); // 後ろに単語文字が続く
    }

    #[test]
    fn regex_errors_are_explicit_not_silent() {
        let err = |q: &str| {
            Matcher::compile(&SearchOptions { regex: true, ..SearchOptions::literal(q) })
                .expect_err("should fail")
        };
        assert!(matches!(err(r"(a"), SearchError::Regex(RegexError::Syntax(_))));
        assert!(matches!(err(r"a)"), SearchError::Regex(RegexError::Syntax(_))));
        assert!(matches!(err(r"[a-"), SearchError::Regex(RegexError::Syntax(_))));
        assert!(matches!(err(r"(a)\1"), SearchError::Regex(RegexError::Unsupported(_))));
        assert!(matches!(err(r"(?=a)"), SearchError::Regex(RegexError::Unsupported(_))));
        assert!(matches!(err(r"(?<name>a)"), SearchError::Regex(RegexError::Unsupported(_))));
        assert!(matches!(err(r"\p{L}"), SearchError::Regex(RegexError::Unsupported(_))));
        assert!(matches!(err(r"(a?)*"), SearchError::Regex(RegexError::Unsupported(_))));
        assert!(matches!(err(r"a{2000}"), SearchError::Regex(RegexError::TooLarge)));
    }

    #[test]
    fn regex_does_not_hang_on_pathological_pattern() {
        let m = matcher(&SearchOptions { regex: true, ..SearchOptions::literal(r"(a+)+$") });
        let line = format!("{}b", "a".repeat(40));
        let t0 = std::time::Instant::now();
        let _ = m.is_match(&line); // 予算切れで諦める = 固まらないことが要件
        assert!(t0.elapsed() < std::time::Duration::from_secs(5), "予算が効いていない");
    }

    #[test]
    fn literal_mode_treats_metacharacters_literally() {
        let m = matcher(&SearchOptions::literal("a.c"));
        assert!(m.is_match("xa.cx"));
        assert!(!m.is_match("abc"));
    }

    #[test]
    fn find_all_returns_non_overlapping_ranges() {
        let m = matcher(&SearchOptions::literal("aa"));
        assert_eq!(m.find_all("aaaa"), vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn regex_search_over_files() {
        let dir = tmp("regexfiles");
        let a = write(&dir, "a.rs", "fn alpha() {}\nfn beta() {}\nstruct X;\n");
        let opts = SearchOptions { regex: true, ..SearchOptions::literal(r"^fn\s+\w+") };
        let out = search(&[a], &opts);
        assert_eq!(out.hits.len(), 2);
        assert_eq!(out.hits[0].line, 0);
        assert_eq!(out.hits[1].line, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ───────────────── 置換 ─────────────────

    fn req(query: &str, replacement: &str, dry_run: bool) -> ReplaceRequest {
        ReplaceRequest {
            options: SearchOptions::literal(query),
            replacement: replacement.into(),
            dry_run,
        }
    }

    #[test]
    fn dry_run_reports_without_writing() {
        let dir = tmp("dryrun");
        let a = write(&dir, "a.txt", "foo bar foo\nbaz\n");
        let before = std::fs::read_to_string(&a).expect("read");
        let rep = replace_all(&[a.clone()], &req("foo", "qux", true)).expect("ok");
        assert!(rep.dry_run);
        assert_eq!(rep.files_changed, 1);
        assert_eq!(rep.replacements, 2);
        assert_eq!(rep.changes, vec![FileChange { path: a.clone(), replacements: 2, written: false }]);
        assert!(rep.errors.is_empty());
        assert_eq!(std::fs::read_to_string(&a).expect("read"), before, "ドライランで書いた");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_replace_writes_and_counts() {
        let dir = tmp("replace");
        let a = write(&dir, "a.txt", "foo bar foo\nbaz\n");
        let b = write(&dir, "b.txt", "nothing here\n");
        let rep = replace_all(&[a.clone(), b.clone()], &req("foo", "qux", false)).expect("ok");
        assert_eq!((rep.files_scanned, rep.files_changed, rep.replacements), (2, 1, 2));
        assert!(rep.changes[0].written);
        assert_eq!(std::fs::read_to_string(&a).expect("read"), "qux bar qux\nbaz\n");
        assert_eq!(std::fs::read_to_string(&b).expect("read"), "nothing here\n");
        // 一時ファイルを残さない
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("zv-replace"))
            .collect();
        assert!(leftovers.is_empty(), "一時ファイルが残っている");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_preserves_crlf_and_missing_trailing_newline() {
        let dir = tmp("crlf");
        let a = write(&dir, "a.txt", "foo\r\nbar foo\r\nlast foo");
        let rep = replace_all(&[a.clone()], &req("foo", "X", false)).expect("ok");
        assert_eq!(rep.replacements, 3);
        assert_eq!(std::fs::read_to_string(&a).expect("read"), "X\r\nbar X\r\nlast X");
        // 混在した改行もそのまま
        let b = write(&dir, "b.txt", "foo\nfoo\r\nfoo\n");
        replace_all(&[b.clone()], &req("foo", "Y", false)).expect("ok");
        assert_eq!(std::fs::read_to_string(&b).expect("read"), "Y\nY\r\nY\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_preserves_utf8_bom() {
        let dir = tmp("bom");
        let a = dir.join("a.txt");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("foo\n".as_bytes());
        std::fs::write(&a, &bytes).expect("write");
        replace_all(&[a.clone()], &req("foo", "bar", false)).expect("ok");
        let after = std::fs::read(&a).expect("read");
        assert_eq!(after, [0xEF, 0xBB, 0xBF, b'b', b'a', b'r', b'\n']);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_skips_binary_and_respects_globs() {
        let dir = tmp("replskip");
        let bin = dir.join("x.bin");
        std::fs::write(&bin, [b'f', b'o', b'o', 0u8, 1]).expect("write");
        let keep = write(&dir, "src/keep.rs", "foo\n");
        let skip = write(&dir, "target/skip.rs", "foo\n");
        let mut r = req("foo", "bar", false);
        r.options.root = Some(dir.clone());
        r.options.exclude_globs = vec!["target/**".into()];
        let rep = replace_all(&[bin.clone(), keep.clone(), skip.clone()], &r).expect("ok");
        assert_eq!(rep.files_changed, 1);
        assert_eq!(std::fs::read(&bin).expect("read"), [b'f', b'o', b'o', 0u8, 1]);
        assert_eq!(std::fs::read_to_string(&keep).expect("read"), "bar\n");
        assert_eq!(std::fs::read_to_string(&skip).expect("read"), "foo\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_with_regex_and_whole_word() {
        let dir = tmp("replre");
        let a = write(&dir, "a.txt", "id=12, id=345, idx=7\n");
        let mut r = req("", "N", false);
        r.options = SearchOptions { regex: true, ..SearchOptions::literal(r"\d+") };
        let rep = replace_all(&[a.clone()], &r).expect("ok");
        assert_eq!(rep.replacements, 3);
        assert_eq!(std::fs::read_to_string(&a).expect("read"), "id=N, id=N, idx=N\n");

        let b = write(&dir, "b.txt", "foo foobar foo_x foo\n");
        let mut r2 = req("foo", "Z", false);
        r2.options.whole_word = true;
        let rep2 = replace_all(&[b.clone()], &r2).expect("ok");
        assert_eq!(rep2.replacements, 2);
        assert_eq!(std::fs::read_to_string(&b).expect("read"), "Z foobar foo_x Z\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_reports_write_failure_without_aborting_the_rest() {
        let dir = tmp("replerr");
        let locked_dir = dir.join("locked");
        std::fs::create_dir_all(&locked_dir).expect("mkdir");
        let bad = write(&dir, "locked/bad.txt", "foo\n");
        let good = write(&dir, "good.txt", "foo\n");

        // 書き込みを失敗させる: Unix はディレクトリを読み取り専用に、
        // Windows はファイルの読み取り専用属性で rename を弾く。
        #[cfg(unix)]
        let armed = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o500))
                .expect("chmod");
            // root だと読み取り専用でも書けてしまうので、その場合は検証を諦める
            std::fs::File::create(locked_dir.join(".probe")).is_err()
        };
        #[cfg(windows)]
        let armed = {
            let mut perm = std::fs::metadata(&bad).expect("meta").permissions();
            perm.set_readonly(true);
            std::fs::set_permissions(&bad, perm).expect("readonly");
            true
        };
        #[cfg(not(any(unix, windows)))]
        let armed = false;

        let rep = replace_all(&[bad.clone(), good.clone()], &req("foo", "bar", false)).expect("ok");
        if armed {
            assert_eq!(rep.errors.len(), 1, "失敗が報告されていない: {rep:?}");
            assert_eq!(rep.errors[0].path, bad);
            assert_eq!(rep.files_changed, 1, "失敗したファイルを数えている");
            assert_eq!(rep.replacements, 1);
        }
        // 後続のファイルは処理され続ける
        assert_eq!(std::fs::read_to_string(&good).expect("read"), "bar\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o700));
        }
        #[cfg(windows)]
        {
            if let Ok(meta) = std::fs::metadata(&bad) {
                let mut perm = meta.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                perm.set_readonly(false);
                let _ = std::fs::set_permissions(&bad, perm);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_query_never_matches() {
        let dir = tmp("empty");
        let a = write(&dir, "a.txt", "anything\n");
        assert!(search(&[a.clone()], &SearchOptions::default()).hits.is_empty());
        let rep = replace_all(&[a.clone()], &req("", "x", false)).expect("ok");
        assert_eq!(rep.replacements, 0);
        assert_eq!(std::fs::read_to_string(&a).expect("read"), "anything\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped_when_not_following() {
        let dir = tmp("symlink");
        let real = write(&dir, "real.txt", "needle\n");
        let link = dir.join("link.txt");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let follow = SearchOptions::literal("needle");
        assert_eq!(search(&[link.clone()], &follow).files_scanned, 1);
        let no_follow =
            SearchOptions { follow_symlinks: false, ..SearchOptions::literal("needle") };
        assert_eq!(search(&[link.clone()], &no_follow).files_scanned, 0);

        // リンクを辿る置換は実体へ書く (リンクを実ファイルに化けさせない)
        let rep = replace_all(&[link.clone()], &req("needle", "pin", false)).expect("ok");
        assert_eq!(rep.replacements, 1);
        assert!(std::fs::symlink_metadata(&link).expect("meta").file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).expect("read"), "pin\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
