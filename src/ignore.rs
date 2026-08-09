//! `.gitignore` の解釈 (自前実装・純粋関数中心)。
//!
//! ファイルツリーとファイル索引の両方が「git が無視するものは見せない」を
//! 満たすために使う。外部クレート (`ignore` / `globset`) を足さずに自前で
//! 持つ理由:
//!
//! * 必要なのは **判定だけ**で、並列ウォーカーやスレッドプールは要らない
//!   (ツリーは遅延展開、索引は自前のバックグラウンドスレッドを既に持つ)。
//! * 依存を 1 つ増やすとロックとビルド時間が増える。ここで使う機能は
//!   この 1 ファイルで書け、テーブルテストで完全に固定できる。
//! * 判定規則を自分で持てば「無視されたものを薄く表示する」ような
//!   *ignored かどうかを知りたいだけ* の用途に、走査と切り離して使える。
//!
//! 実装している規則 (gitignore(5) 準拠):
//!
//! * `#` で始まる行はコメント。`\#` は本文の `#`。
//! * 空行は無視。末尾の空白は `\ ` でエスケープしない限り落とす。
//! * `!` で始まる行は否定 (再包含)。
//! * 末尾の `/` はディレクトリのみに一致。
//! * 先頭または途中に `/` があるパターンは `.gitignore` の位置を基準に固定。
//!   無ければどの階層でも一致する (先頭に `**/` を補う)。
//! * `*` と `?` は `/` を跨がない。`**` はディレクトリ 0 個以上
//!   (末尾の `**` だけは 1 個以上 — `a/**` は `a` 自身に当たらない)。
//! * `[abc]` `[a-z]` `[!abc]` `[^abc]` の文字クラス。
//! * **後勝ち** — 最後に一致したパターンが勝つ。深い `.gitignore` ほど
//!   後に評価するので、親より優先される。
//!
//! パス区切りは [`split_rel_os`] で吸収する。Windows の `\` 区切りでも
//! 同じ判定になり、その両側を OS に依らずテストできる。

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

// ═══════════════════════════════════════════════════════════════════════
//  パターン 1 行ぶんの表現
// ═══════════════════════════════════════════════════════════════════════

/// 1 パス要素ぶんの glob トークン。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    Lit(char),
    /// `?` — 任意の 1 文字
    Any,
    /// `*` — 0 文字以上 (要素内なので `/` は跨がない)
    Star,
    Class {
        neg: bool,
        items: Vec<ClassItem>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ClassItem {
    Ch(char),
    Range(char, char),
}

impl ClassItem {
    fn contains(&self, c: char) -> bool {
        match self {
            ClassItem::Ch(x) => *x == c,
            ClassItem::Range(a, b) => *a <= c && c <= *b,
        }
    }
}

/// パスを `/` で切った 1 区切りぶん。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seg {
    /// `**` — ディレクトリ 0 個以上
    DoubleStar,
    Glob(Vec<Tok>),
}

/// `.gitignore` の 1 行を解析したもの。
#[derive(Clone, Debug)]
pub struct Pattern {
    /// `!` 付き = 一致したら「無視しない」。
    negate: bool,
    /// 末尾 `/` 付き = ディレクトリにだけ一致する。
    dir_only: bool,
    segs: Vec<Seg>,
}

// ═══════════════════════════════════════════════════════════════════════
//  解析
// ═══════════════════════════════════════════════════════════════════════

/// `.gitignore` の本文を 1 行ずつ解析する。コメント・空行は落ちる。
pub fn parse(text: &str) -> Vec<Pattern> {
    text.lines().filter_map(parse_line).collect()
}

/// 1 行を解析する。コメント・空行なら `None`。
fn parse_line(raw: &str) -> Option<Pattern> {
    // CRLF のチェックアウトでも同じ結果になるよう、先に `\r` を落とす。
    let line = raw.strip_suffix('\r').unwrap_or(raw);
    let line = trim_trailing_spaces(line);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut body = line;
    let mut negate = false;
    if let Some(rest) = body.strip_prefix('!') {
        negate = true;
        body = rest;
    }
    if body.is_empty() {
        return None;
    }

    // 末尾 `/` (エスケープされていないもの) → ディレクトリのみ
    let mut dir_only = false;
    if ends_with_unescaped_slash(body) {
        dir_only = true;
        body = &body[..body.len() - 1];
    }
    if body.is_empty() {
        return None;
    }

    // 先頭・途中に `/` があれば「この .gitignore の位置」を基準に固定する。
    let inner = body.strip_prefix('/').unwrap_or(body);
    let anchored = body.starts_with('/') || contains_unescaped_slash(inner);

    let mut segs: Vec<Seg> = Vec::new();
    if !anchored {
        // どの階層でも一致 = 先頭に `**/` を補う
        segs.push(Seg::DoubleStar);
    }
    for part in split_unescaped_slash(inner) {
        if part.is_empty() {
            continue; // `a//b` のような入力の空要素は捨てる
        }
        if part == "**" {
            // `**` の連続は 1 つに畳む (照合の指数爆発を避ける)
            if segs.last() != Some(&Seg::DoubleStar) {
                segs.push(Seg::DoubleStar);
            }
        } else {
            segs.push(Seg::Glob(tokenize(part)));
        }
    }
    if segs.is_empty() {
        return None;
    }
    Some(Pattern {
        negate,
        dir_only,
        segs,
    })
}

/// 末尾の空白を落とす。`\ ` (バックスラッシュでエスケープ) は残す。
/// git の `trim_trailing_spaces()` と同じで、対象は**半角スペースだけ**。
fn trim_trailing_spaces(s: &str) -> &str {
    let b = s.as_bytes();
    let mut end = b.len();
    while end > 0 && b[end - 1] == b' ' {
        // 直前のバックスラッシュが奇数個ならエスケープされた空白 → ここで止める
        let mut bs = 0usize;
        let mut j = end - 1;
        while j > 0 && b[j - 1] == b'\\' {
            bs += 1;
            j -= 1;
        }
        if bs % 2 == 1 {
            break;
        }
        end -= 1;
    }
    &s[..end]
}

fn ends_with_unescaped_slash(s: &str) -> bool {
    if !s.ends_with('/') {
        return false;
    }
    let b = s.as_bytes();
    let mut bs = 0usize;
    let mut j = b.len() - 1;
    while j > 0 && b[j - 1] == b'\\' {
        bs += 1;
        j -= 1;
    }
    bs.is_multiple_of(2)
}

fn contains_unescaped_slash(s: &str) -> bool {
    let mut esc = false;
    for c in s.chars() {
        if esc {
            esc = false;
            continue;
        }
        match c {
            '\\' => esc = true,
            '/' => return true,
            _ => {}
        }
    }
    false
}

/// エスケープされていない `/` で切る。
fn split_unescaped_slash(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut esc = false;
    for (i, c) in s.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match c {
            '\\' => esc = true,
            '/' => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// 1 パス要素ぶんの glob をトークン列にする。
fn tokenize(part: &str) -> Vec<Tok> {
    let chars: Vec<char> = part.chars().collect();
    let mut out: Vec<Tok> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() => {
                out.push(Tok::Lit(chars[i + 1]));
                i += 2;
            }
            '\\' => {
                out.push(Tok::Lit('\\'));
                i += 1;
            }
            '*' => {
                // `*` の連続は 1 つに畳む (照合のバックトラックを抑える)
                if out.last() != Some(&Tok::Star) {
                    out.push(Tok::Star);
                }
                i += 1;
            }
            '?' => {
                out.push(Tok::Any);
                i += 1;
            }
            '[' => match parse_class(&chars, i) {
                Some((tok, next)) => {
                    out.push(tok);
                    i = next;
                }
                None => {
                    // 閉じない `[` は素の文字として扱う (git / fnmatch と同じ)
                    out.push(Tok::Lit('['));
                    i += 1;
                }
            },
            c => {
                out.push(Tok::Lit(c));
                i += 1;
            }
        }
    }
    out
}

/// `[...]` を解析する。戻りは (トークン, `]` の次の位置)。
fn parse_class(chars: &[char], open: usize) -> Option<(Tok, usize)> {
    let mut i = open + 1;
    let mut neg = false;
    if matches!(chars.get(i), Some('!') | Some('^')) {
        neg = true;
        i += 1;
    }
    let mut items: Vec<ClassItem> = Vec::new();
    // 先頭の `]` は「閉じ」ではなく素の `]`
    if chars.get(i) == Some(&']') {
        items.push(ClassItem::Ch(']'));
        i += 1;
    }
    while i < chars.len() && chars[i] != ']' {
        let lo = if chars[i] == '\\' && i + 1 < chars.len() {
            i += 1;
            chars[i]
        } else {
            chars[i]
        };
        i += 1;
        if chars.get(i) == Some(&'-') && chars.get(i + 1).is_some_and(|c| *c != ']') {
            i += 1;
            let hi = if chars[i] == '\\' && i + 1 < chars.len() {
                i += 1;
                chars[i]
            } else {
                chars[i]
            };
            i += 1;
            items.push(ClassItem::Range(lo, hi));
        } else {
            items.push(ClassItem::Ch(lo));
        }
    }
    if i >= chars.len() {
        return None; // `]` が無い
    }
    Some((Tok::Class { neg, items }, i + 1))
}

// ═══════════════════════════════════════════════════════════════════════
//  照合
// ═══════════════════════════════════════════════════════════════════════

impl Pattern {
    /// `comps` (このパターンを載せた `.gitignore` からの相対パスの要素列) に一致するか。
    fn matches_comps(&self, comps: &[&str], is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        match_segs(&self.segs, comps)
    }
}

/// `/` 区切り (Windows では `\` も) の相対パスを要素へ切る。空要素と `.` は落とす。
///
/// `windows` を引数にしているのは、**どの OS のテストからも両側を確かめる**ため。
/// 実行時は [`split_rel`] が `cfg!(windows)` を渡す。
pub fn split_rel_os(rel: &str, windows: bool) -> Vec<&str> {
    rel.split(|c| c == '/' || (windows && c == '\\'))
        .filter(|s| !s.is_empty() && *s != ".")
        .collect()
}

fn match_segs(segs: &[Seg], comps: &[&str]) -> bool {
    match segs.first() {
        None => comps.is_empty(),
        Some(Seg::DoubleStar) => {
            let rest = &segs[1..];
            if rest.is_empty() {
                // 末尾の `**` は 1 要素以上を要求する (`a/**` は `a` に当たらない)
                return !comps.is_empty();
            }
            (0..=comps.len()).any(|i| match_segs(rest, &comps[i..]))
        }
        Some(Seg::Glob(toks)) => match comps.split_first() {
            Some((head, tail)) => match_glob(toks, head) && match_segs(&segs[1..], tail),
            None => false,
        },
    }
}

fn match_glob(toks: &[Tok], s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    glob_here(toks, &chars)
}

fn glob_here(toks: &[Tok], s: &[char]) -> bool {
    match toks.first() {
        None => s.is_empty(),
        Some(Tok::Star) => (0..=s.len()).any(|i| glob_here(&toks[1..], &s[i..])),
        Some(Tok::Any) => !s.is_empty() && glob_here(&toks[1..], &s[1..]),
        Some(Tok::Lit(c)) => s.first() == Some(c) && glob_here(&toks[1..], &s[1..]),
        Some(Tok::Class { neg, items }) => match s.first() {
            Some(&c) => {
                let hit = items.iter().any(|it| it.contains(c));
                (hit != *neg) && glob_here(&toks[1..], &s[1..])
            }
            None => false,
        },
    }
}

/// 複数階層の `.gitignore` をまとめた判定 (純粋関数)。**後勝ち**。
///
/// `layers` は浅い順の `(基準ディレクトリの要素列, パターン列)`。基準は
/// ワークスペースルートから、その `.gitignore` が置かれたディレクトリまで
/// (ルート直下なら空)。深いものほど後に置けば、git と同じく
/// 「子の `.gitignore` が親より優先」になる。基準の配下にないパスは
/// その層では評価されない。
///
/// 戻り値: `None` = どのパターンにも当たらない / `Some(true)` = 無視 /
/// `Some(false)` = `!` で明示的に再包含。
pub fn decide(layers: &[(&[&str], &[Pattern])], comps: &[&str], is_dir: bool) -> Option<bool> {
    let mut verdict: Option<bool> = None;
    for (base, pats) in layers {
        if comps.len() <= base.len() || &comps[..base.len()] != *base {
            continue; // この `.gitignore` の配下ではない
        }
        let sub = &comps[base.len()..];
        for p in *pats {
            if p.matches_comps(sub, is_dir) {
                verdict = Some(!p.negate);
            }
        }
    }
    verdict
}

// ═══════════════════════════════════════════════════════════════════════
//  実ファイルを読むキャッシュ付きの判定器
// ═══════════════════════════════════════════════════════════════════════

/// ディレクトリごとの `.gitignore` を遅延読み込み・キャッシュして判定する。
///
/// ファイルツリー (遅延展開) と索引 (バックグラウンド DFS) の両方が使う。
/// `enabled == false` なら常に「無視しない」を返す (設定で切れる)。
pub struct Ignorer {
    enabled: bool,
    /// ディレクトリ → そこに置かれた `.gitignore` の解析結果 (無ければ空)
    dirs: HashMap<PathBuf, Vec<Pattern>>,
    /// ルート → `.git/info/exclude` + グローバル除外の解析結果
    roots: HashMap<PathBuf, Vec<Pattern>>,
    /// グローバル除外 (`core.excludesFile`) は 1 回だけ読む
    global: Option<Vec<Pattern>>,
}

impl Ignorer {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            dirs: HashMap::new(),
            roots: HashMap::new(),
            global: None,
        }
    }

    /// 設定の切り替え。値が変わったらキャッシュを捨てる。
    pub fn set_enabled(&mut self, on: bool) {
        if self.enabled != on {
            self.enabled = on;
            self.clear();
        }
    }

    /// 読み込み済みの `.gitignore` を捨てる (ツリーの再読み込みと同じ契機)。
    pub fn clear(&mut self) {
        self.dirs.clear();
        self.roots.clear();
    }

    /// `dir` に置かれた `.gitignore` を (未読なら) 読み込む。
    fn load_dir(&mut self, dir: &Path) {
        if self.dirs.contains_key(dir) {
            return;
        }
        let text = std::fs::read_to_string(dir.join(".gitignore")).unwrap_or_default();
        self.dirs.insert(dir.to_path_buf(), parse(&text));
    }

    /// `root` に効く「リポジトリ外」の除外 (`.git/info/exclude` + グローバル)。
    fn load_root(&mut self, root: &Path) {
        if self.roots.contains_key(root) {
            return;
        }
        if self.global.is_none() {
            self.global = Some(read_global_excludes());
        }
        let mut v = self.global.clone().unwrap_or_default();
        let info = root.join(".git").join("info").join("exclude");
        if let Ok(text) = std::fs::read_to_string(info) {
            v.extend(parse(&text));
        }
        self.roots.insert(root.to_path_buf(), v);
    }

    /// `path` (`root` 配下) が git に無視されるか。
    ///
    /// 走査側は「無視されたディレクトリへは降りない」ので、ここでは
    /// `path` の**祖先**が無視されているかまでは見ない (git の
    /// 「親が除外されたファイルは再包含できない」と同じ結果になる)。
    pub fn is_ignored(&mut self, root: &Path, path: &Path, is_dir: bool) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(rel) = rel_slash(root, path) else {
            return false;
        };
        // 区切りの解釈は 1 か所 (`split_rel_os`) に集約する。Windows の `\` でも
        // 同じ要素列になり、その両側を OS に依らずテストできる。
        let comps = split_rel_os(&rel, cfg!(windows));
        if comps.is_empty() {
            return false; // ルート自身は無視しない
        }
        // 先に必要な `.gitignore` を読み込む (可変借用と参照の期間を分ける)
        self.load_root(root);
        let mut dirs: Vec<PathBuf> = Vec::with_capacity(comps.len());
        let mut d = root.to_path_buf();
        for name in comps.iter().take(comps.len() - 1) {
            dirs.push(d.clone());
            d.push(name);
        }
        dirs.push(d);
        for dir in &dirs {
            self.load_dir(dir);
        }

        let mut layers: Vec<(&[&str], &[Pattern])> = Vec::with_capacity(dirs.len() + 1);
        if let Some(p) = self.roots.get(root) {
            layers.push((&comps[..0], p.as_slice()));
        }
        for (depth, dir) in dirs.iter().enumerate() {
            if let Some(p) = self.dirs.get(dir) {
                layers.push((&comps[..depth], p.as_slice()));
            }
        }
        decide(&layers, &comps, is_dir).unwrap_or(false)
    }
}

/// `base` から `path` までの相対パスを `/` 区切りの文字列にする。
///
/// 要素の切り出しは `Path::components` に任せるので、Windows の `\` でも
/// Unix の `/` でも同じ結果になる (ファイル名に `\` を含む Unix の
/// パスを壊さない)。
pub fn rel_slash(base: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(base).ok()?;
    let mut out = String::new();
    for c in rel.components() {
        if let Component::Normal(s) = c {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(&s.to_string_lossy());
        }
    }
    Some(out)
}

/// `git config core.excludesFile` (無ければ XDG の既定) を読む。
///
/// `git` を起動せず設定ファイルを直接読む — フォルダを開くたびに
/// サブプロセスを生やすと、起動の体感が落ちるため。
fn read_global_excludes() -> Vec<Pattern> {
    let Some(path) = global_excludes_path() else {
        return Vec::new();
    };
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(_) => Vec::new(),
    }
}

fn global_excludes_path() -> Option<PathBuf> {
    for cfg in git_config_paths() {
        if let Ok(text) = std::fs::read_to_string(&cfg) {
            if let Some(v) = core_excludes_file(&text) {
                return Some(expand_home(&v));
            }
        }
    }
    // 既定 (XDG): $XDG_CONFIG_HOME/git/ignore → ~/.config/git/ignore
    let p = xdg_config_dir()?.join("git").join("ignore");
    p.is_file().then_some(p)
}

fn git_config_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("GIT_CONFIG_GLOBAL") {
        v.push(PathBuf::from(p));
    }
    if let Some(x) = xdg_config_dir() {
        v.push(x.join("git").join("config"));
    }
    if let Some(h) = dirs::home_dir() {
        v.push(h.join(".gitconfig"));
    }
    v
}

fn xdg_config_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x));
        }
    }
    dirs::home_dir().map(|h| h.join(".config"))
}

/// `~` / `~/…` をホームへ展開する (どの OS でも `dirs` から取る)。
fn expand_home(s: &str) -> PathBuf {
    let s = s.trim();
    if s == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(s));
    }
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(s)
}

/// git の設定ファイル本文から `[core] excludesfile` を拾う (最後の指定が勝つ)。
///
/// 完全な INI パーサではなく、必要な 1 キーだけを見る素朴な実装。
pub fn core_excludes_file(text: &str) -> Option<String> {
    let mut in_core = false;
    let mut found: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(sec) = line.strip_prefix('[') {
            let name = sec.split(']').next().unwrap_or("").trim();
            // `[core]` と `[core "x"]` のどちらでも先頭語を見る
            let head = name.split_whitespace().next().unwrap_or("");
            in_core = head.eq_ignore_ascii_case("core");
            continue;
        }
        if !in_core {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case("excludesfile") {
            let v = v.trim().trim_matches('"');
            if !v.is_empty() {
                found = Some(v.to_string());
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    /// テーブルテスト用: ルート直下の `.gitignore` 1 枚で判定する。
    /// `windows = true` なら受け取るパスを `\` 区切りへ変換して確かめる。
    fn ignored_os(text: &str, rel: &str, is_dir: bool, windows: bool) -> bool {
        let pats = parse(text);
        let owned = if windows {
            rel.replace('/', "\\")
        } else {
            rel.to_string()
        };
        let comps = split_rel_os(&owned, windows);
        decide(&[(&[][..], pats.as_slice())], &comps, is_dir).unwrap_or(false)
    }

    #[test]
    fn gitignore_rules_table() {
        // (説明, .gitignore 本文, 相対パス, ディレクトリか, 無視されるか)
        let cases: &[(&str, &str, &str, bool, bool)] = &[
            (
                "素の名前はどの階層でも一致",
                "foo.txt",
                "foo.txt",
                false,
                true,
            ),
            (
                "素の名前は配下でも一致",
                "foo.txt",
                "a/b/foo.txt",
                false,
                true,
            ),
            ("*.log は拡張子一致", "*.log", "debug.log", false, true),
            ("*.log は配下でも一致", "*.log", "a/debug.log", false, true),
            (
                "*.log は別拡張子に当たらない",
                "*.log",
                "debug.txt",
                false,
                false,
            ),
            (
                "! による否定 (後勝ち)",
                "*.log\n!important.log",
                "important.log",
                false,
                false,
            ),
            (
                "! の否定は他のファイルに影響しない",
                "*.log\n!important.log",
                "other.log",
                false,
                true,
            ),
            (
                "否定を先に書くと後の包含が勝つ (後勝ちの順序)",
                "!important.log\n*.log",
                "important.log",
                false,
                true,
            ),
            ("build/ はディレクトリに一致", "build/", "build", true, true),
            (
                "build/ は同名ファイルに一致しない",
                "build/",
                "build",
                false,
                false,
            ),
            (
                "build/ は配下のディレクトリにも一致",
                "build/",
                "a/build",
                true,
                true,
            ),
            (
                "/root-only.txt はルート限定",
                "/root-only.txt",
                "root-only.txt",
                false,
                true,
            ),
            (
                "/root-only.txt は配下に当たらない",
                "/root-only.txt",
                "a/root-only.txt",
                false,
                false,
            ),
            (
                "**/nested/** は nested の中身に一致",
                "**/nested/**",
                "a/nested/x.txt",
                false,
                true,
            ),
            (
                "**/nested/** は nested 自身には当たらない (末尾 ** は 1 要素以上)",
                "**/nested/**",
                "a/nested",
                true,
                false,
            ),
            (
                "**/nested/** は深い階層にも一致",
                "**/nested/**",
                "a/b/nested/c/d.txt",
                false,
                true,
            ),
            ("a[bc]d は文字クラス (b)", "a[bc]d", "abd", false, true),
            ("a[bc]d は文字クラス (c)", "a[bc]d", "acd", false, true),
            ("a[bc]d は範囲外に当たらない", "a[bc]d", "axd", false, false),
            ("[a-c]x は範囲指定", "[a-c]x", "bx", false, true),
            ("[!a-c]x は否定クラス", "[!a-c]x", "dx", false, true),
            ("[!a-c]x は該当を除外", "[!a-c]x", "bx", false, false),
            ("[^a-c]x も否定クラス", "[^a-c]x", "dx", false, true),
            ("# はコメント", "#foo.txt", "foo.txt", false, false),
            (
                "\\# はコメントではなくリテラル",
                "\\#literal",
                "#literal",
                false,
                true,
            ),
            ("空行は無視", "\n\n*.log\n\n", "x.log", false, true),
            ("末尾の空白は落とす", "foo.txt   ", "foo.txt", false, true),
            (
                "エスケープした末尾空白は残す",
                "foo\\ ",
                "foo ",
                false,
                true,
            ),
            (
                "エスケープした末尾空白は空白なしに当たらない",
                "foo\\ ",
                "foo",
                false,
                false,
            ),
            ("? は 1 文字", "a?c", "abc", false, true),
            ("? は 0 文字に当たらない", "a?c", "ac", false, false),
            ("* は / を跨がない", "a*c", "a/c", false, false),
            (
                "中間の / があると固定される",
                "doc/frotz",
                "doc/frotz",
                true,
                true,
            ),
            (
                "中間の / があると配下に当たらない",
                "doc/frotz",
                "a/doc/frotz",
                true,
                false,
            ),
            (
                "a/**/b はディレクトリ 0 個以上",
                "a/**/b",
                "a/b",
                false,
                true,
            ),
            (
                "a/**/b は途中に何段あってもよい",
                "a/**/b",
                "a/x/y/b",
                false,
                true,
            ),
            (
                "node_modules/ は定番の除外",
                "node_modules/",
                "node_modules",
                true,
                true,
            ),
            ("target/ も同様", "target/", "target", true, true),
            (
                "CRLF のチェックアウトでも同じ",
                "*.log\r\n!keep.log\r\n",
                "keep.log",
                false,
                false,
            ),
        ];
        for (name, text, rel, is_dir, want) in cases {
            for windows in [false, true] {
                assert_eq!(
                    ignored_os(text, rel, *is_dir, windows),
                    *want,
                    "{name} (windows={windows}): gitignore={text:?} path={rel:?} dir={is_dir}"
                );
            }
        }
    }

    #[test]
    fn subdirectory_gitignore_overrides_parent() {
        // 親: *.log を無視 / 子 (a/): !keep.log で戻す
        let root = parse("*.log");
        let child = parse("!keep.log");
        let layers = [(&[][..], root.as_slice()), (&["a"][..], child.as_slice())];
        for windows in [false, true] {
            let sep = if windows { "\\" } else { "/" };
            let p = |s: &str| s.replace('/', sep);
            assert_eq!(
                decide(&layers, &split_rel_os(&p("a/keep.log"), windows), false),
                Some(false),
                "子の .gitignore が親より優先 (windows={windows})"
            );
            assert_eq!(
                decide(&layers, &split_rel_os(&p("a/other.log"), windows), false),
                Some(true)
            );
            // 子の .gitignore は自分の配下にしか効かない
            assert_eq!(
                decide(&layers, &split_rel_os(&p("b/keep.log"), windows), false),
                Some(true)
            );
        }
    }

    #[test]
    fn last_match_wins_within_a_file() {
        let pats = parse("*.tmp\n!keep.tmp\nkeep.tmp");
        let layers = [(&[][..], pats.as_slice())];
        // 同じファイル内の 3 行目 (最後) が勝つ
        assert_eq!(decide(&layers, &["keep.tmp"], false), Some(true));
        // どのパターンにも当たらなければ意見なし
        assert_eq!(decide(&layers, &["keep.txt"], false), None);
    }

    #[test]
    fn split_rel_handles_both_separators() {
        assert_eq!(split_rel_os("a/b/c", false), ["a", "b", "c"]);
        assert_eq!(split_rel_os("a/b/c", true), ["a", "b", "c"]);
        // Unix ではバックスラッシュはファイル名の一部 (区切りではない)
        assert_eq!(split_rel_os("a\\b", false), ["a\\b"]);
        assert_eq!(split_rel_os("a\\b", true), ["a", "b"]);
        // 空要素と `.` は落ちる
        assert_eq!(split_rel_os("./a//b/", false), ["a", "b"]);
    }

    #[test]
    fn comment_and_blank_lines_are_dropped() {
        assert_eq!(parse("# comment\n\n  \n*.log\n\\#literal\n").len(), 2);
        assert!(parse("#\n\n   \n").is_empty());
    }

    #[test]
    fn ignorer_reads_gitignore_from_disk() {
        let root = unique_temp_dir("zaivern-ignore-test", "disk");
        std::fs::write(root.join(".gitignore"), "node_modules/\ntarget/\n*.log\n").unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("a.log"), "x").unwrap();

        let mut ig = Ignorer::new(true);
        assert!(ig.is_ignored(&root, &root.join("node_modules"), true));
        assert!(ig.is_ignored(&root, &root.join("target"), true));
        assert!(ig.is_ignored(&root, &root.join("a.log"), false));
        assert!(!ig.is_ignored(&root, &root.join("src"), true));
        assert!(!ig.is_ignored(&root, &root.join("src").join("main.rs"), false));
        // ルート自身は無視しない
        assert!(!ig.is_ignored(&root, &root, true));

        // 無効化すると常に false
        let mut off = Ignorer::new(false);
        assert!(!off.is_ignored(&root, &root.join("node_modules"), true));
        // 有効化し直すと効く (キャッシュも破棄される)
        off.set_enabled(true);
        assert!(off.is_ignored(&root, &root.join("node_modules"), true));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ignorer_respects_nested_gitignore() {
        let root = unique_temp_dir("zaivern-ignore-test", "nested");
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(root.join("pkg").join(".gitignore"), "!keep.log\n").unwrap();
        std::fs::write(root.join("pkg").join("keep.log"), "x").unwrap();
        std::fs::write(root.join("pkg").join("drop.log"), "x").unwrap();

        let mut ig = Ignorer::new(true);
        assert!(!ig.is_ignored(&root, &root.join("pkg").join("keep.log"), false));
        assert!(ig.is_ignored(&root, &root.join("pkg").join("drop.log"), false));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn info_exclude_is_read() {
        let root = unique_temp_dir("zaivern-ignore-test", "exclude");
        std::fs::create_dir_all(root.join(".git").join("info")).unwrap();
        std::fs::write(root.join(".git").join("info").join("exclude"), "scratch/\n").unwrap();
        std::fs::create_dir_all(root.join("scratch")).unwrap();

        let mut ig = Ignorer::new(true);
        assert!(ig.is_ignored(&root, &root.join("scratch"), true));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn core_excludes_file_is_parsed() {
        let text = "[user]\n\tname = x\n[core]\n\texcludesfile = ~/.gitignore_global\n";
        assert_eq!(
            core_excludes_file(text).as_deref(),
            Some("~/.gitignore_global")
        );
        // core 以外のセクションの同名キーは拾わない
        assert_eq!(core_excludes_file("[other]\nexcludesfile = x\n"), None);
        // 後の指定が勝つ
        assert_eq!(
            core_excludes_file("[core]\nexcludesfile = a\n[core]\nexcludesfile = b\n").as_deref(),
            Some("b")
        );
        // 引用符は外す
        assert_eq!(
            core_excludes_file("[core]\nexcludesfile = \"c\"\n").as_deref(),
            Some("c")
        );
        // `~` はホームへ展開される (どのユーザー名でも成り立つ形で確かめる)
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_home("~/x"), home.join("x"));
            assert_eq!(expand_home("~"), home);
        }
    }
}
