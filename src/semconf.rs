//! 意味的衝突 (semantic conflict) の検出 — **ファイルが違うのに噛み合わない変更**を、
//! マージが通ってビルドが壊れるより前に見せる。
//!
//! ## なぜ要るのか
//!
//! ファイル所有リース ([`crate::lease`]) は「同じファイルを 2 人が同時に編集する」
//! ことを構造的に防ぐ (実測で 64 体・1500 書込でもテキスト衝突 0)。しかし
//! **意味的衝突は原理的に防げない**:
//!
//! > A が `api.rs` を確保して関数シグネチャを変え、B が `caller.rs` を確保して
//! > 古い呼び方のまま書く。ファイルが違うのでリースも git も両方通し、
//! > **マージは成功し、ビルドが壊れる**。
//!
//! 防げないことは受け入れ、[`crate::conflict`] (衝突レーダー) と同じ思想 —
//! **起きたことを早く見せる** — へ倒す。
//!
//! ## 何を見ているのか (ここが誤検出しない理由の芯)
//!
//! 入力は「担当者 → その担当者が加えた差分」だけで、**リポジトリ全体は読まない**。
//! これは手抜きではなく、**検出したい事象の定義そのもの**である:
//!
//! * A の変更で既存の呼び出し側が壊れるだけなら、**A 自身のビルドが落ちる** —
//!   A のワークツリーには呼び出し側も入っているので、A が気付く。
//! * 本当に誰も気付けないのは「B が **これから書いた** コードが、A の変更を
//!   知らないまま古い前提に立っている」場合だけ。B のワークツリーに A の変更は
//!   入っていないので、B のビルドも通ってしまう。
//!
//! よってこのモジュールは **B の「追加行」だけを参照側として見る**。文脈行や
//! 既存コードは参照と数えない。これだけで誤検出の母数が桁で落ちる。
//!
//! ## 対応言語は Rust だけ
//!
//! **他言語は意図的にやらない。** 中途半端に当てると誤検出でユーザーが機能ごと
//! 切ってしまい、Rust の分まで失う。拡張子が `.rs` でないファイルは入口で捨てる。
//!
//! ## 誤検出を潰すために置いている番人 (すべてテストがある)
//!
//! 1. コメント・文字列・生文字列・文字リテラルの中身は [`sanitize_line`] で
//!    空白へ潰してから照合する (ライフタイム `'a` を文字リテラルと誤らない)。
//! 2. 照合は必ず**単語境界**。`foo` は `foo_bar` に当たらない。
//! 3. **非 `pub` の定義は候補にしない。** 別ファイルから参照できないものを
//!    「壊した」と言うのは定義上ありえない。
//! 4. **素の識別子一致は参照と見なさない。** 呼び出し `f(` ・経路 `m::f` ・
//!    型位置 `-> T` ・構造体リテラル `T {` ・`use` のいずれかの形を要求する
//!    (局所変数と区別できないため)。
//! 5. A が別ファイルへ**移しただけ**なら消滅と数えない (担当者単位で相殺する)。
//! 6. B が**自分でも同じ名前を定義している**なら、それは B 自身のものなので出さない。
//! 7. 標準トレイトのメソッド名 (`new` / `from` / `fmt` …) は [`COMMON_METHODS`]
//!    で丸ごと除外する。型が違えば同名でも無関係だから。
//! 8. B の呼び出しが**新しいシグネチャに合っている**なら出さない (B は既に追随済み)。
//! 9. `match` に `_ =>` があるなら variant 追加は網羅性を壊さないので出さない。
//! 10. 確信度が [`Limits::min_confidence`] 未満のものは**作っても出さない**。
//!
//! ## 検出できないもの (正直に)
//!
//! マクロ経由の定義・呼び出し、トレイト実装越しの動的ディスパッチ、`cfg` で
//! 切り替わるコード、型推論に依存する変更 (引数の数も名前も同じで型だけ変わる等)、
//! `use` の別名 (`use a::b as c`) を跨いだ参照。**当てられないものは黙って
//! 当てない**のがこのモジュールの方針。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;

use crate::i18n::{tr, trf};
use crate::panels::space;

// ═══════════════════════════════════════════════════════════════════════
//  1. 定数 — 上限と除外表
// ═══════════════════════════════════════════════════════════════════════

/// 1 回の解析で出す警告の上限 (既定)。超えたぶんは [`Report::omitted`] に載せ、
/// **画面へ必ず出す** (無音で切らない)。
pub const MAX_WARNINGS: usize = 40;

/// A 側の候補シンボル数の上限。名前ごとに正規表現を組むので歯止めを置く。
pub const MAX_SYMBOLS: usize = 200;

/// 短すぎる名前は照合しない。`id` / `n` のような語は別物に当たりすぎる。
pub const MIN_NAME_LEN: usize = 3;

/// 画面に出す根拠行の最大文字数。
const EVIDENCE_CHARS: usize = 120;

/// 走査の基準間隔。実測に応じて [`crate::git::scan_interval`] が伸ばす。
const SCAN_BASE: Duration = Duration::from_secs(20);

/// 標準トレイト / 標準ライブラリのメソッド名。**同名でも型が違えば無関係**なので、
/// これらは検出対象から丸ごと外す。ここを緩めると `Foo::new` を消しただけで
/// 世界中の `Bar::new(` が警告になる。
const COMMON_METHODS: &[&str] = &[
    "new",
    "default",
    "from",
    "into",
    "try_from",
    "try_into",
    "clone",
    "clone_from",
    "drop",
    "fmt",
    "len",
    "is_empty",
    "next",
    "next_back",
    "iter",
    "iter_mut",
    "into_iter",
    "to_string",
    "as_str",
    "as_ref",
    "as_mut",
    "deref",
    "deref_mut",
    "eq",
    "ne",
    "cmp",
    "partial_cmp",
    "hash",
    "add",
    "sub",
    "mul",
    "div",
    "rem",
    "neg",
    "not",
    "index",
    "index_mut",
    "borrow",
    "borrow_mut",
    "to_owned",
    "from_str",
    "from_iter",
    "extend",
    "size_hint",
    "call",
    "source",
    "type_id",
];

/// モジュール名として扱わないファイル名 (クレート根)。
const CRATE_ROOTS: &[&str] = &["lib", "main"];

// ═══════════════════════════════════════════════════════════════════════
//  2. 純粋ロジック — Rust ソース 1 行の無害化
// ═══════════════════════════════════════════════════════════════════════

/// [`sanitize_line`] が行を跨いで持ち越す状態。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SanMode {
    /// 地のコード。
    #[default]
    Code,
    /// ブロックコメントの中 (Rust は入れ子になるので深さを持つ)。
    Block(u32),
    /// 通常文字列の中 (Rust の `"` は行を跨げる)。
    Str,
    /// 生文字列の中 (`r#"…"#` の `#` の個数)。
    Raw(usize),
}

/// Rust の 1 行から**コメント・文字列・文字リテラルの中身を空白へ潰す**。
///
/// 返り値は元と同じ長さのバイト列にはならないが、**バイト位置は保つ** —
/// 潰した箇所を同じ長さの空白へ置き換えるので、`find` の位置がそのまま
/// 元の行の位置として使える。
///
/// ライフタイム `'a` を文字リテラルと誤らないのがこの関数の要点で、誤ると
/// `&'a str` 以降が丸ごと文字列扱いになり、その行の識別子を全部見失う。
pub fn sanitize_line(line: &str, mode: &mut SanMode) -> String {
    let b = line.as_bytes();
    let mut out: Vec<u8> = vec![b' '; b.len()];
    let mut i = 0usize;
    while i < b.len() {
        match *mode {
            SanMode::Code => {
                if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
                    // 行コメント: 行末まで捨てる
                    break;
                }
                if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                    *mode = SanMode::Block(1);
                    i += 2;
                    continue;
                }
                if b[i] == b'"' {
                    *mode = SanMode::Str;
                    i += 1;
                    continue;
                }
                if b[i] == b'r' && !prev_is_ident(b, i) {
                    if let Some((hashes, after)) = raw_string_open(b, i) {
                        *mode = SanMode::Raw(hashes);
                        i = after;
                        continue;
                    }
                }
                if b[i] == b'\'' {
                    match char_literal_end(b, i) {
                        // 文字リテラル: 丸ごと空白へ
                        Some(end) => {
                            i = end;
                            continue;
                        }
                        // ライフタイム: `'` だけ空白にして中身は識別子として残す
                        None => {
                            i += 1;
                            continue;
                        }
                    }
                }
                out[i] = b[i];
                i += 1;
            }
            SanMode::Block(depth) => {
                if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                    *mode = SanMode::Block(depth + 1);
                    i += 2;
                } else if b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                    *mode = if depth <= 1 {
                        SanMode::Code
                    } else {
                        SanMode::Block(depth - 1)
                    };
                    i += 2;
                } else {
                    i += 1;
                }
            }
            SanMode::Str => {
                if b[i] == b'\\' {
                    i += 2;
                } else if b[i] == b'"' {
                    *mode = SanMode::Code;
                    i += 1;
                } else {
                    i += 1;
                }
            }
            SanMode::Raw(n) => {
                if b[i] == b'"' && b[i + 1..].iter().take(n).filter(|c| **c == b'#').count() == n {
                    *mode = SanMode::Code;
                    i += 1 + n;
                } else {
                    i += 1;
                }
            }
        }
    }
    // 上のループはマルチバイト文字を分割しない (地のコードでは 1 バイトずつ
    // そのまま写し、潰す側は ASCII の境界でしか切らない)。念のため lossy。
    String::from_utf8_lossy(&out).into_owned()
}

/// `i` の直前が識別子の一部か (`for` の `r` を生文字列の開始と誤らないため)。
fn prev_is_ident(b: &[u8], i: usize) -> bool {
    i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')
}

/// `r"` / `r#"` … の開始か。`(# の個数, 開き引用符の次の位置)`。
fn raw_string_open(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let mut j = i + 1;
    let mut hashes = 0usize;
    while j < b.len() && b[j] == b'#' {
        hashes += 1;
        j += 1;
    }
    if j < b.len() && b[j] == b'"' {
        Some((hashes, j + 1))
    } else {
        None
    }
}

/// `'` から始まる文字リテラルの終端 (閉じ `'` の次)。**ライフタイムなら `None`**。
fn char_literal_end(b: &[u8], i: usize) -> Option<usize> {
    if i + 1 >= b.len() {
        return None;
    }
    if b[i + 1] == b'\\' {
        // `'\n'` `'\u{1F600}'` — 12 バイト以内に閉じがあれば文字リテラル
        let mut j = i + 2;
        while j < b.len() && j <= i + 13 {
            if b[j] == b'\'' {
                return Some(j + 1);
            }
            j += 1;
        }
        return None;
    }
    // 1 文字 (マルチバイトもあり得る) の直後が `'` なら文字リテラル。
    let s = std::str::from_utf8(&b[i + 1..]).ok()?;
    let c = s.chars().next()?;
    let end = i + 1 + c.len_utf8();
    if b.get(end) == Some(&b'\'') {
        Some(end + 1)
    } else {
        None
    }
}

/// 一連の行が**ブロックコメントの途中から始まっている**か。
///
/// ハンクの先頭は必ずしも構文の切れ目ではない。`/*` より先に `*/` が来るなら、
/// その範囲はコメントの続きなので、そう仮定して無害化を始める。これが無いと
/// doc コメントの中の識別子を「コード」として拾う (= 誤検出) 。
fn starts_inside_block_comment(rows: &[Row]) -> bool {
    let mut joined = String::new();
    for r in rows {
        joined.push_str(&r.raw);
        joined.push('\n');
    }
    match (joined.find("/*"), joined.find("*/")) {
        (Some(o), Some(c)) => c < o,
        (None, Some(_)) => true,
        _ => false,
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  3. 純粋ロジック — 差分を「照合できる形」へ畳む
// ═══════════════════════════════════════════════════════════════════════

/// 畳む前の 1 行。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    /// 元ファイルの行番号 (1 始まり)。
    line: usize,
    /// この面で「変更された」行か (after なら追加行、before なら削除行)。
    changed: bool,
    raw: String,
}

/// 行ごとの目印。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mark {
    /// [`Joined::text`] 上の行頭バイト位置。
    off: usize,
    line: usize,
    changed: bool,
    raw: String,
}

/// ハンク 1 つを「片面ぶんの連続したテキスト」へ畳んだもの。
///
/// 複数行に跨るシグネチャや呼び出しをそのまま照合できるようにするのが目的。
/// 無害化はこの順序で行うので、複数行のブロックコメント / 文字列も追える。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Joined {
    text: String,
    marks: Vec<Mark>,
}

impl Joined {
    fn build(rows: &[Row]) -> Joined {
        let mut mode = if starts_inside_block_comment(rows) {
            SanMode::Block(1)
        } else {
            SanMode::Code
        };
        let mut text = String::new();
        let mut marks = Vec::with_capacity(rows.len());
        for r in rows {
            let off = text.len();
            let clean = sanitize_line(&r.raw, &mut mode);
            text.push_str(&clean);
            text.push('\n');
            marks.push(Mark {
                off,
                line: r.line,
                changed: r.changed,
                raw: r.raw.clone(),
            });
        }
        Joined { text, marks }
    }

    /// バイト位置 `off` を含む行の目印。
    fn mark_at(&self, off: usize) -> Option<&Mark> {
        if self.marks.is_empty() {
            return None;
        }
        let idx = match self.marks.binary_search_by_key(&off, |m| m.off) {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        };
        self.marks.get(idx)
    }
}

/// 変更の種類。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Modified,
    Created,
    Deleted,
    /// 改名 (`from` は変更前のパス)。
    Renamed {
        from: String,
    },
}

/// 1 ハンクぶん。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct HunkCode {
    /// 変更後の姿 (文脈行 + 追加行)。`changed` が立つのが追加行。
    after: Joined,
    /// 変更前の姿 (文脈行 + 削除行)。`changed` が立つのが削除行。
    before: Joined,
}

/// 1 ファイルぶんの変更。
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileCode {
    path: String,
    kind: ChangeKind,
    hunks: Vec<HunkCode>,
}

/// 1 人の担当者 (エージェント / ワークツリー) が加えた変更のまとまり。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub label: String,
    files: Vec<FileCode>,
}

impl Owner {
    /// 触ったファイル数 (Rust 以外は入口で捨てているので、解析対象の数)。
    pub fn touched(&self) -> usize {
        self.files.len()
    }
}

/// パスを照合用に正規化する (`\` → `/`、先頭 `./` を落とす)。
///
/// 大文字小文字は畳まない — Rust の識別子は大小を区別するので、パスだけ
/// 畳んでもモジュール名の照合には効かない。
fn norm_path(p: &str) -> String {
    let s = p.replace('\\', "/");
    s.strip_prefix("./").unwrap_or(&s).to_string()
}

/// このパスを意味解析の対象にするか。**Rust だけ**。
fn is_rust(path: &str) -> bool {
    path.ends_with(".rs")
}

/// `git diff`（**文脈行つき**）1 本から担当者 1 人ぶんを起こす。
///
/// 文脈行が要るのは `enum` の本体や `match` の網羅性を見るため。
/// `--unified=0` で取ると規則 3 が丸ごと効かなくなる。
pub fn owner_from_diff(label: &str, diff_text: &str) -> Owner {
    let mut files = Vec::new();
    for f in crate::diff::parse_unified(diff_text) {
        if f.is_binary {
            continue;
        }
        let kind = if f.is_rename && f.old_path != f.new_path {
            ChangeKind::Renamed {
                from: norm_path(&f.old_path),
            }
        } else if f.is_deleted_file() {
            ChangeKind::Deleted
        } else if f.is_new_file() {
            ChangeKind::Created
        } else {
            ChangeKind::Modified
        };
        let path = match &kind {
            ChangeKind::Deleted => norm_path(&f.old_path),
            _ => norm_path(&f.new_path),
        };
        // 削除・改名は「元が Rust なら」対象 (規則 4 で使う)。
        let rust = is_rust(&path) || matches!(&kind, ChangeKind::Renamed { from } if is_rust(from));
        if path.is_empty() || !rust {
            continue;
        }
        let mut hunks = Vec::new();
        for h in &f.hunks {
            let mut after_rows = Vec::new();
            let mut before_rows = Vec::new();
            for l in &h.lines {
                match l.kind {
                    crate::diff::LineKind::Context => {
                        after_rows.push(Row {
                            line: l.new_no.unwrap_or(0),
                            changed: false,
                            raw: l.text.clone(),
                        });
                        before_rows.push(Row {
                            line: l.old_no.unwrap_or(0),
                            changed: false,
                            raw: l.text.clone(),
                        });
                    }
                    crate::diff::LineKind::Added => after_rows.push(Row {
                        line: l.new_no.unwrap_or(0),
                        changed: true,
                        raw: l.text.clone(),
                    }),
                    crate::diff::LineKind::Removed => before_rows.push(Row {
                        line: l.old_no.unwrap_or(0),
                        changed: true,
                        raw: l.text.clone(),
                    }),
                }
            }
            hunks.push(HunkCode {
                after: Joined::build(&after_rows),
                before: Joined::build(&before_rows),
            });
        }
        files.push(FileCode { path, kind, hunks });
    }
    Owner {
        label: label.to_string(),
        files,
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  4. 純粋ロジック — 定義の抽出
// ═══════════════════════════════════════════════════════════════════════

/// 定義の種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DefKind {
    /// 関数・メソッド。
    Fn,
    /// `struct` / `enum` / `trait` / `union` / `type`。
    Type,
    /// `const` / `static`。
    Const,
}

/// 見つけた定義 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Def {
    kind: DefKind,
    name: String,
    /// `pub` 系の可視性が付いているか。**付いていないものは扱わない**。
    is_pub: bool,
    line: usize,
    /// [`Joined::text`] 上の名前の終端 (シグネチャを読む起点)。
    name_end: usize,
    /// 変更された行 (追加 / 削除) にあるか。
    changed: bool,
}

fn re_fn() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(concat!(
            r"(?m)^[ \t]*(?P<vis>pub(?:[ \t]*\([^)\n]*\))?[ \t]+)?",
            r"(?:default[ \t]+)?(?:const[ \t]+)?(?:async[ \t]+)?(?:unsafe[ \t]+)?",
            r"(?:extern[ \t]+(?:[^ \t\n]+[ \t]+)?)?",
            r"fn[ \t]+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
        ))
        .expect("fn 定義の正規表現")
    })
}

fn re_type() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(concat!(
            r"(?m)^[ \t]*(?P<vis>pub(?:[ \t]*\([^)\n]*\))?[ \t]+)?",
            r"(?:default[ \t]+)?(?:unsafe[ \t]+)?",
            r"(?P<kw>struct|enum|trait|union|type)[ \t]+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
        ))
        .expect("型定義の正規表現")
    })
}

fn re_const() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(concat!(
            r"(?m)^[ \t]*(?P<vis>pub(?:[ \t]*\([^)\n]*\))?[ \t]+)?",
            r"(?:const|static)[ \t]+(?:mut[ \t]+)?",
            r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)[ \t]*:",
        ))
        .expect("定数定義の正規表現")
    })
}

/// 畳んだテキストから定義を拾う。
fn defs_in(j: &Joined) -> Vec<Def> {
    let mut out = Vec::new();
    let mut push = |kind: DefKind, m: &regex::Captures<'_>| {
        let Some(name) = m.name("name") else { return };
        let Some(mark) = j.mark_at(name.start()) else {
            return;
        };
        out.push(Def {
            kind,
            name: name.as_str().to_string(),
            is_pub: m.name("vis").is_some(),
            line: mark.line,
            name_end: name.end(),
            changed: mark.changed,
        });
    };
    for c in re_fn().captures_iter(&j.text) {
        push(DefKind::Fn, &c);
    }
    for c in re_type().captures_iter(&j.text) {
        push(DefKind::Type, &c);
    }
    for c in re_const().captures_iter(&j.text) {
        push(DefKind::Const, &c);
    }
    out.sort_by(|a, b| (a.kind, &a.name, a.line).cmp(&(b.kind, &b.name, b.line)));
    out
}

// ═══════════════════════════════════════════════════════════════════════
//  5. 純粋ロジック — 参照の形
// ═══════════════════════════════════════════════════════════════════════

/// 参照の形。**素の識別子一致は参照と見なさない** (局所変数と区別できない)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefShape {
    /// `name(` — 呼び出し。
    Call,
    /// `mod::name` / `name::…` — 経路。
    Path,
    /// `Name {` — 構造体リテラル。
    Literal,
    /// `-> Name` / `: Name` / `<Name` / `impl Name` — 型の位置。
    TypePos,
    /// 大文字だけの定数名の素の出現 (`SCREAMING_CASE` は局所変数と衝突しない)。
    Bare,
}

impl RefShape {
    fn label(self) -> &'static str {
        match self {
            RefShape::Call => "呼び出し",
            RefShape::Path => "経路",
            RefShape::Literal => "構造体リテラル",
            RefShape::TypePos => "型の位置",
            RefShape::Bare => "定数の参照",
        }
    }
}

/// 名前 1 つぶんの参照検出器。種類ごとに**当てる形を変える**のが要点。
fn ref_regex(kind: DefKind, name: &str) -> Option<Regex> {
    let n = regex::escape(name);
    let pat = match kind {
        // 関数: 呼び出しか経路か `use`。素の識別子には当てない。
        DefKind::Fn => format!(
            concat!(
                r"(?P<call>\b{n}[ \t]*\()",
                r"|(?P<path>::[ \t]*{n}\b|\b{n}[ \t]*::)",
                r"|(?P<ty>\b(?:use|as)[ \t]+(?:[A-Za-z_][A-Za-z0-9_]*[ \t]*::[ \t]*)*{n}\b)",
            ),
            n = n
        ),
        // 型: 経路・リテラル・型位置・タプル構造体の呼び出し。
        DefKind::Type => format!(
            concat!(
                r"(?P<call>\b{n}[ \t]*\()",
                r"|(?P<path>\b{n}[ \t]*::|::[ \t]*{n}\b)",
                r"|(?P<lit>\b{n}[ \t]*\{{)",
                r"|(?P<ty>(?:->|:|<|&)[ \t]*{n}\b",
                r"|\b(?:impl|for|dyn|use|as|enum|struct|trait|type)[ \t]+",
                r"(?:[A-Za-z_][A-Za-z0-9_]*[ \t]*::[ \t]*)*{n}\b)",
            ),
            n = n
        ),
        // 定数: 経路つきか、SCREAMING_CASE の素の出現。
        DefKind::Const => {
            if is_screaming(name) {
                format!(r"(?P<bare>\b{n}\b)", n = n)
            } else {
                format!(r"(?P<path>::[ \t]*{n}\b|\b{n}[ \t]*::)", n = n)
            }
        }
    };
    Regex::new(&pat).ok()
}

/// `SCREAMING_SNAKE_CASE` か。定数名がこれなら素の出現でも局所変数と衝突しない。
fn is_screaming(name: &str) -> bool {
    name.chars().any(|c| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// 名前が「見分けが付く」か。付かない名前は確信度を 1 段下げる。
fn distinctive(name: &str) -> bool {
    name.contains('_')
        || name.len() >= 6
        || name
            .chars()
            .skip(1)
            .any(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// この名前をそもそも候補にしてよいか。
fn usable_name(name: &str) -> bool {
    name.len() >= MIN_NAME_LEN && !COMMON_METHODS.contains(&name)
}

/// 見つけた参照 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
struct RefHit {
    shape: RefShape,
    path: String,
    line: usize,
    evidence: String,
    /// [`Joined::text`] 上の位置 (呼び出しの引数を数えるのに使う)。
    at: usize,
    /// 見つけたハンクの添字 (同じハンク内の情報だけを見るため)。
    hunk: usize,
    file: usize,
}

/// 担当者 `o` の**追加行だけ**から、名前 `name` への参照を集める。
fn refs_of(o: &Owner, kind: DefKind, name: &str, limit: usize) -> Vec<RefHit> {
    let Some(re) = ref_regex(kind, name) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (fi, f) in o.files.iter().enumerate() {
        for (hi, h) in f.hunks.iter().enumerate() {
            for c in re.captures_iter(&h.after.text) {
                let Some(m) = c.get(0) else { continue };
                let Some(mark) = h.after.mark_at(m.start()) else {
                    continue;
                };
                // **追加行にある参照だけ**を数える。文脈行は「B が今書いた」
                // ものではないので、意味的衝突の定義から外れる。
                if !mark.changed {
                    continue;
                }
                let shape = if c.name("call").is_some() {
                    RefShape::Call
                } else if c.name("lit").is_some() {
                    RefShape::Literal
                } else if c.name("path").is_some() {
                    RefShape::Path
                } else if c.name("bare").is_some() {
                    RefShape::Bare
                } else {
                    RefShape::TypePos
                };
                out.push(RefHit {
                    shape,
                    path: f.path.clone(),
                    line: mark.line,
                    evidence: clip(mark.raw.trim(), EVIDENCE_CHARS),
                    at: m.start(),
                    hunk: hi,
                    file: fi,
                });
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }
    out
}

/// 長い行を省略する (画面のどの幅でも見切れないよう、先に文字数で切る)。
fn clip(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════
//  6. 純粋ロジック — シグネチャ
// ═══════════════════════════════════════════════════════════════════════

/// 関数シグネチャ 1 つ。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Sig {
    /// `名前: 型` の並び (空白を畳んだもの)。レシーバ (`self`) も含む。
    params: Vec<String>,
    /// 先頭が `self` 系か。
    has_self: bool,
}

impl Sig {
    /// 呼び出し側から見た引数の数。メソッド呼び出しならレシーバを除く。
    fn arity(&self, method_call: bool) -> usize {
        let n = self.params.len();
        if method_call && self.has_self {
            n - 1
        } else {
            n
        }
    }
}

/// `from` 以降で最初の `(` を探し、対応する `)` までの中身を返す。
/// ジェネリクス `<…>` は読み飛ばす。閉じが見つからなければ `None`。
fn params_after(text: &str, from: usize) -> Option<(String, usize)> {
    let b = text.as_bytes();
    let mut i = from;
    // 空白とジェネリクスを飛ばす
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'<' => {
                let mut depth = 0i32;
                while i < b.len() {
                    if b[i] == b'<' {
                        depth += 1;
                    } else if b[i] == b'>' {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    i += 1;
                }
            }
            _ => break,
        }
    }
    if i >= b.len() || b[i] != b'(' {
        return None;
    }
    let start = i + 1;
    let mut depth = 0i32;
    while i < b.len() {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((text[start..i].to_string(), i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// 括弧の外側にあるカンマで分ける。
fn split_top_level(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '(' | '[' | '{' | '<' => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' | '}' | '>' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur = String::new();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out.retain(|p| !p.is_empty());
    out
}

/// 空白を 1 つへ畳む (`a :  u8` と `a: u8` を同じと見なす)。
fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_receiver(p: &str) -> bool {
    let t = squeeze(p)
        .replace('&', " ")
        .replace("mut", " ")
        .replace("'_", " ");
    let t: String = t
        .split_whitespace()
        .filter(|w| !w.starts_with('\''))
        .collect::<Vec<_>>()
        .join(" ");
    t == "self" || t.starts_with("self :")
}

/// 定義位置からシグネチャを読む。
fn sig_at(j: &Joined, d: &Def) -> Option<Sig> {
    let (body, _) = params_after(&j.text, d.name_end)?;
    let params: Vec<String> = split_top_level(&body).iter().map(|p| squeeze(p)).collect();
    let has_self = params.first().is_some_and(|p| is_receiver(p));
    Some(Sig { params, has_self })
}

/// 呼び出し位置から実引数の数を読む。読めなければ `None`。
fn call_arity(text: &str, at: usize) -> Option<usize> {
    // `at` は名前の先頭。名前の直後の `(` から数える。
    let b = text.as_bytes();
    let mut i = at;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
        i += 1;
    }
    let (body, _) = params_after(text, i)?;
    if body.trim().is_empty() {
        return Some(0);
    }
    Some(split_top_level(&body).len())
}

/// 呼び出しがメソッド呼び出し (`x.name(`) か。
fn is_method_call(text: &str, at: usize) -> bool {
    let b = text.as_bytes();
    let mut i = at;
    while i > 0 {
        i -= 1;
        match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' => continue,
            b'.' => return i == 0 || b[i - 1] != b'.',
            _ => return false,
        }
    }
    false
}

// ═══════════════════════════════════════════════════════════════════════
//  7. 純粋ロジック — 警告の型
// ═══════════════════════════════════════════════════════════════════════

/// どの規則で当たったか。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    /// 1: 公開定義が消えた / 改名された。
    DefinitionGone,
    /// 2: `pub fn` のシグネチャが変わった。
    SignatureChanged,
    /// 3: `pub enum` に variant が増えた (網羅性が壊れる)。
    VariantAdded,
    /// 4: ファイルが消えた / 改名された (`mod` / `use` が外れる)。
    ModuleGone,
}

impl Rule {
    pub fn icon(self) -> &'static str {
        match self {
            Rule::DefinitionGone => "🗑",
            Rule::SignatureChanged => "✍",
            Rule::VariantAdded => "➕",
            Rule::ModuleGone => "📂",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Rule::DefinitionGone => "定義が消えた",
            Rule::SignatureChanged => "シグネチャが変わった",
            Rule::VariantAdded => "variant が増えた",
            Rule::ModuleGone => "モジュールが消えた",
        }
    }
}

/// どれくらい確からしいか。**`Low` は作るが出さない** (既定の下限が `Medium`)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn label(self) -> &'static str {
        match self {
            Confidence::Low => "低",
            Confidence::Medium => "中",
            Confidence::High => "高",
        }
    }
}

/// 警告 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    pub rule: Rule,
    pub confidence: Confidence,
    /// 定義を動かした側。
    pub from: String,
    pub from_path: String,
    pub from_line: usize,
    /// 古い前提のまま書いた側。
    pub to: String,
    pub to_path: String,
    pub to_line: usize,
    /// 問題になっているシンボル。
    pub symbol: String,
    /// 規則ごとの補足 (引数の数の変化など)。空でよい。
    pub note: String,
    /// `to` 側の該当行。
    pub evidence: String,
}

impl Warning {
    /// 並べ替えの鍵。**決定的にするための唯一の順序**。
    fn key(
        &self,
    ) -> (
        std::cmp::Reverse<Confidence>,
        Rule,
        &str,
        &str,
        &str,
        &str,
        usize,
    ) {
        (
            std::cmp::Reverse(self.confidence),
            self.rule,
            &self.symbol,
            &self.from,
            &self.to,
            &self.to_path,
            self.to_line,
        )
    }

    /// 重複判定の鍵 (同じ場所・同じ規則は 1 件)。
    fn dedup_key(&self) -> (Rule, String, String, String, String, usize) {
        (
            self.rule,
            self.from.clone(),
            self.to.clone(),
            self.symbol.clone(),
            self.to_path.clone(),
            self.to_line,
        )
    }

    /// 画面に出す 1 行。**表示時に翻訳する** (解析は純粋なまま)。
    pub fn message(&self) -> String {
        let body = match self.rule {
            Rule::DefinitionGone => trf(
                "{from} が {sym} を {fp} から消した／改名したのに、{to} は {tp} で今それを書いています",
                &[
                    ("from", self.from.clone()),
                    ("sym", self.symbol.clone()),
                    ("fp", self.from_path.clone()),
                    ("to", self.to.clone()),
                    ("tp", self.to_path.clone()),
                ],
            ),
            Rule::SignatureChanged => trf(
                "{from} が {sym} のシグネチャを {fp} で変えたのに、{to} は {tp} で古い呼び方をしています",
                &[
                    ("from", self.from.clone()),
                    ("sym", self.symbol.clone()),
                    ("fp", self.from_path.clone()),
                    ("to", self.to.clone()),
                    ("tp", self.to_path.clone()),
                ],
            ),
            Rule::VariantAdded => trf(
                "{from} が {sym} に variant を足したのに、{to} は {tp} でワイルドカードの無い match を書いています",
                &[
                    ("from", self.from.clone()),
                    ("sym", self.symbol.clone()),
                    ("to", self.to.clone()),
                    ("tp", self.to_path.clone()),
                ],
            ),
            Rule::ModuleGone => trf(
                "{from} が {fp} を消した／改名したのに、{to} は {tp} でモジュール {sym} を参照しています",
                &[
                    ("from", self.from.clone()),
                    ("fp", self.from_path.clone()),
                    ("to", self.to.clone()),
                    ("tp", self.to_path.clone()),
                    ("sym", self.symbol.clone()),
                ],
            ),
        };
        if self.note.is_empty() {
            body
        } else {
            format!("{body} ({})", tr(&self.note))
        }
    }
}

/// 出す量の上限。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_warnings: usize,
    /// これ未満の確信度は**作っても出さない**。
    pub min_confidence: Confidence,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_warnings: MAX_WARNINGS,
            min_confidence: Confidence::Medium,
        }
    }
}

/// 1 回の解析の結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub warnings: Vec<Warning>,
    /// 上限で落とした件数。**0 でなければ必ず画面へ出す** (無音で切らない)。
    pub omitted: usize,
    /// 確信度が下限に届かず落とした件数。
    pub low: usize,
    /// 解析した担当者の数。
    pub owners: usize,
}

// ═══════════════════════════════════════════════════════════════════════
//  8. 純粋ロジック — 解析本体
// ═══════════════════════════════════════════════════════════════════════

/// 担当者 1 人ぶんの「他人を壊しうる事実」。
struct Facts {
    /// 消えた公開定義 (担当者全体で相殺済み)。
    gone: Vec<(DefKind, String, String, usize)>,
    /// シグネチャが変わった `pub fn`。`(名前, パス, 行, 旧, 新)`。
    resigned: Vec<(String, String, usize, Sig, Sig)>,
    /// variant が増えた `pub enum`。`(enum 名, パス, 行, 増えた variant)`。
    variants: Vec<(String, String, usize, Vec<String>)>,
    /// 消えた / 改名されたモジュール。`(モジュール名, 旧パス)`。
    modules: Vec<(String, String)>,
}

fn facts_of(o: &Owner) -> Facts {
    let mut removed: BTreeMap<(DefKind, String), (String, usize)> = BTreeMap::new();
    let mut added: BTreeSet<(DefKind, String)> = BTreeSet::new();
    let mut resigned = Vec::new();
    let mut variants = Vec::new();
    let mut modules = Vec::new();
    let mut created_mods: BTreeSet<String> = BTreeSet::new();

    for f in &o.files {
        match &f.kind {
            ChangeKind::Created => {
                if let Some(m) = module_name(&f.path) {
                    created_mods.insert(m);
                }
            }
            ChangeKind::Deleted => {
                if let Some(m) = module_name(&f.path) {
                    modules.push((m, f.path.clone()));
                }
            }
            ChangeKind::Renamed { from } => {
                if let Some(m) = module_name(from) {
                    modules.push((m, from.clone()));
                }
                if let Some(m) = module_name(&f.path) {
                    created_mods.insert(m);
                }
            }
            ChangeKind::Modified => {}
        }
        // ファイルごとに、消えた定義 / 増えた定義 / シグネチャ変更を拾う。
        let mut old_sigs: BTreeMap<String, (Sig, usize)> = BTreeMap::new();
        let mut new_sigs: BTreeMap<String, (Sig, usize)> = BTreeMap::new();
        for h in &f.hunks {
            for d in defs_in(&h.before) {
                if !d.changed || !d.is_pub {
                    continue;
                }
                removed
                    .entry((d.kind, d.name.clone()))
                    .or_insert((f.path.clone(), d.line));
                if d.kind == DefKind::Fn {
                    if let Some(s) = sig_at(&h.before, &d) {
                        old_sigs.insert(d.name.clone(), (s, d.line));
                    }
                }
            }
            for d in defs_in(&h.after) {
                if !d.changed || !d.is_pub {
                    continue;
                }
                added.insert((d.kind, d.name.clone()));
                if d.kind == DefKind::Fn {
                    if let Some(s) = sig_at(&h.after, &d) {
                        new_sigs.insert(d.name.clone(), (s, d.line));
                    }
                }
            }
            variants.extend(added_variants(&h.after, &f.path));
        }
        for (name, (old, _)) in &old_sigs {
            let Some((new, line)) = new_sigs.get(name) else {
                continue;
            };
            if old.params != new.params {
                resigned.push((
                    name.clone(),
                    f.path.clone(),
                    *line,
                    old.clone(),
                    new.clone(),
                ));
            }
        }
    }

    // 担当者の中で作り直した名前は「消えていない」(別ファイルへ移しただけ)。
    let gone: Vec<(DefKind, String, String, usize)> = removed
        .into_iter()
        .filter(|((k, n), _)| !added.contains(&(*k, n.clone())))
        .map(|((k, n), (p, l))| (k, n, p, l))
        .take(MAX_SYMBOLS)
        .collect();
    modules.retain(|(m, _)| !created_mods.contains(m));

    Facts {
        gone,
        resigned,
        variants,
        modules,
    }
}

/// `.rs` のパスからモジュール名を起こす。クレート根は `None`。
fn module_name(path: &str) -> Option<String> {
    let p = path.trim_end_matches('/');
    let file = p.rsplit('/').next()?;
    let stem = file.strip_suffix(".rs")?;
    if stem == "mod" {
        // `a/b/mod.rs` → `b`
        let parent = p.rsplit('/').nth(1)?;
        return (!parent.is_empty()).then(|| parent.to_string());
    }
    if CRATE_ROOTS.contains(&stem) {
        return None;
    }
    (!stem.is_empty()).then(|| stem.to_string())
}

fn re_enum_head() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(concat!(
            r"(?m)^[ \t]*pub(?:[ \t]*\([^)\n]*\))?[ \t]+",
            r"enum[ \t]+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
        ))
        .expect("pub enum の正規表現")
    })
}

fn re_variant() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^[ \t]*(?P<v>[A-Z][A-Za-z0-9_]*)[ \t]*(?:[,({=]|$)")
            .expect("variant の正規表現")
    })
}

fn re_wildcard_arm() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?:^|[^A-Za-z0-9_.])_[ \t]*(?:if[^=\n]*)?=>")
            .expect("ワイルドカードの正規表現")
    })
}

/// `pub enum` に**追加行として**増えた variant を拾う。
///
/// `enum` の本体は文脈行から辿るので、`--unified=0` の差分では 1 件も出ない
/// (それは正しい — 本体が見えないなら網羅性の判断はできない)。
fn added_variants(j: &Joined, path: &str) -> Vec<(String, String, usize, Vec<String>)> {
    let mut out = Vec::new();
    for c in re_enum_head().captures_iter(&j.text) {
        let Some(name) = c.name("name") else { continue };
        let Some(all) = c.get(0) else { continue };
        let Some((body, _)) = brace_body(&j.text, all.end()) else {
            continue;
        };
        let mut found: Vec<String> = Vec::new();
        for (off, depth, line_text) in lines_with_depth(&j.text, body.0, body.1) {
            if depth != 1 {
                continue;
            }
            let Some(mark) = j.mark_at(off) else { continue };
            if !mark.changed {
                continue;
            }
            if let Some(v) = re_variant().captures(line_text).and_then(|m| m.name("v")) {
                found.push(v.as_str().to_string());
            }
        }
        if found.is_empty() {
            continue;
        }
        found.sort();
        found.dedup();
        let line = j.mark_at(name.start()).map(|m| m.line).unwrap_or(0);
        out.push((name.as_str().to_string(), path.to_string(), line, found));
    }
    out
}

/// `from` 以降で最初の `{` を探し、`(本体の開始, 本体の終了)` を返す。
fn brace_body(text: &str, from: usize) -> Option<((usize, usize), usize)> {
    let b = text.as_bytes();
    let mut i = from;
    while i < b.len() && b[i] != b'{' {
        // 宣言が終わっている (`;` は unit / tuple 形) なら本体は無い
        if b[i] == b';' {
            return None;
        }
        i += 1;
    }
    if i >= b.len() {
        return None;
    }
    let start = i + 1;
    let mut depth = 0i32;
    while i < b.len() {
        match b[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(((start, i), i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `start..end` の各行を `(行頭のバイト位置, 行頭時点の波括弧の深さ, 行本文)` で返す。
///
/// 深さは `start` を 1 として数える (= enum 本体の直下が 1)。
fn lines_with_depth(text: &str, start: usize, end: usize) -> Vec<(usize, i32, &str)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut depth = 1i32;
    let mut line_start = start;
    let mut i = start;
    let mut depth_at_line = depth;
    while i < end && i < b.len() {
        match b[i] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'\n' => {
                out.push((line_start, depth_at_line, &text[line_start..i]));
                line_start = i + 1;
                depth_at_line = depth;
            }
            _ => {}
        }
        i += 1;
    }
    if line_start < end.min(b.len()) {
        out.push((
            line_start,
            depth_at_line,
            &text[line_start..end.min(b.len())],
        ));
    }
    out
}

/// 意味的衝突を解析する。**純粋関数** — 同じ入力なら必ず同じ出力。
pub fn analyze(owners: &[Owner], limits: Limits) -> Report {
    let mut all: Vec<Warning> = Vec::new();
    let mut low = 0usize;
    if owners.len() < 2 {
        return Report {
            owners: owners.len(),
            ..Report::default()
        };
    }
    for a in owners {
        let facts = facts_of(a);
        for b in owners {
            if b.label == a.label {
                continue;
            }
            emit_definition_gone(a, &facts, b, &mut all);
            emit_signature_changed(a, &facts, b, &mut all);
            emit_variant_added(a, &facts, b, &mut all);
            emit_module_gone(a, &facts, b, &mut all);
        }
    }
    // 確信度の下限で落とす。
    all.retain(|w| {
        let keep = w.confidence >= limits.min_confidence;
        if !keep {
            low += 1;
        }
        keep
    });
    // 決定的な並べ替え → 重複除去 → 上限。
    all.sort_by(|x, y| x.key().cmp(&y.key()));
    let mut seen: BTreeSet<(Rule, String, String, String, String, usize)> = BTreeSet::new();
    all.retain(|w| seen.insert(w.dedup_key()));
    let omitted = all.len().saturating_sub(limits.max_warnings);
    all.truncate(limits.max_warnings);
    Report {
        warnings: all,
        omitted,
        low,
        owners: owners.len(),
    }
}

/// 規則 1: 消えた公開定義を、相手が今書いている。
fn emit_definition_gone(a: &Owner, facts: &Facts, b: &Owner, out: &mut Vec<Warning>) {
    let bdef = facts_defined(b);
    for (kind, name, path, line) in &facts.gone {
        if !usable_name(name) {
            continue;
        }
        // B が自分でも同じ名前を定義しているなら、それは B のもの。
        if bdef.contains(name) {
            continue;
        }
        for r in refs_of(b, *kind, name, limit_per_symbol()) {
            let conf = if distinctive(name)
                && matches!(
                    r.shape,
                    RefShape::Call | RefShape::Path | RefShape::Literal | RefShape::Bare
                ) {
                Confidence::High
            } else if distinctive(name) {
                Confidence::Medium
            } else {
                Confidence::Low
            };
            out.push(Warning {
                rule: Rule::DefinitionGone,
                confidence: conf,
                from: a.label.clone(),
                from_path: path.clone(),
                from_line: *line,
                to: b.label.clone(),
                to_path: r.path.clone(),
                to_line: r.line,
                symbol: name.clone(),
                note: r.shape.label().to_string(),
                evidence: r.evidence,
            });
        }
    }
}

/// 規則 2: シグネチャが変わったのに、相手が古い呼び方で書いている。
fn emit_signature_changed(a: &Owner, facts: &Facts, b: &Owner, out: &mut Vec<Warning>) {
    for (name, path, line, old, new) in &facts.resigned {
        if !usable_name(name) {
            continue;
        }
        let arity_changed = old.params.len() != new.params.len();
        for r in refs_of(b, DefKind::Fn, name, limit_per_symbol()) {
            if r.shape != RefShape::Call {
                continue;
            }
            let Some(file) = b.files.get(r.file) else {
                continue;
            };
            let Some(hunk) = file.hunks.get(r.hunk) else {
                continue;
            };
            let method = is_method_call(&hunk.after.text, r.at);
            let got = call_arity(&hunk.after.text, r.at);
            let want = new.arity(method);
            let had = old.arity(method);
            let (conf, note) = match got {
                // B は既に新しい形に合っている → **警告しない**
                Some(n) if n == want && arity_changed => continue,
                Some(n) if arity_changed && n == had => (
                    Confidence::High,
                    format!("引数 {had} → {want} なのに {n} 個で呼んでいます"),
                ),
                // 数がどちらとも違う = 同名の別物の可能性が高い → 出さない
                Some(n) if arity_changed && n != had => {
                    let _ = n;
                    continue;
                }
                // 数は変わっていないが名前 / 型が変わった
                _ if !arity_changed => (
                    Confidence::Medium,
                    trf(
                        "引数が {old} → {new} に変わりました",
                        &[
                            ("old", old.params.join(", ")),
                            ("new", new.params.join(", ")),
                        ],
                    ),
                ),
                // 引数の数が読めなかった (行を跨いだ呼び出し等)
                _ => (
                    Confidence::Medium,
                    format!("引数 {had} → {want} に変わりました"),
                ),
            };
            out.push(Warning {
                rule: Rule::SignatureChanged,
                confidence: conf,
                from: a.label.clone(),
                from_path: path.clone(),
                from_line: *line,
                to: b.label.clone(),
                to_path: r.path.clone(),
                to_line: r.line,
                symbol: name.clone(),
                note,
                evidence: r.evidence,
            });
        }
    }
}

/// 規則 3: `pub enum` に variant が増えたのに、相手がワイルドカード無しで match している。
fn emit_variant_added(a: &Owner, facts: &Facts, b: &Owner, out: &mut Vec<Warning>) {
    for (name, path, line, added) in &facts.variants {
        if !usable_name(name) {
            continue;
        }
        let Ok(re) = Regex::new(&format!(
            r"\b{n}[ \t]*::[ \t]*[A-Za-z_][A-Za-z0-9_]*[ \t]*(?:\([^)\n]*\)|\{{[^}}\n]*\}})?[ \t]*=>",
            n = regex::escape(name)
        )) else {
            continue;
        };
        for (fi, f) in b.files.iter().enumerate() {
            let _ = fi;
            for h in &f.hunks {
                // 同じハンクにワイルドカードがあるなら網羅性は壊れない。
                if re_wildcard_arm().is_match(&h.after.text) {
                    continue;
                }
                let Some(m) = re.find(&h.after.text) else {
                    continue;
                };
                let Some(mark) = h.after.mark_at(m.start()) else {
                    continue;
                };
                if !mark.changed {
                    continue;
                }
                out.push(Warning {
                    rule: Rule::VariantAdded,
                    confidence: Confidence::High,
                    from: a.label.clone(),
                    from_path: path.clone(),
                    from_line: *line,
                    to: b.label.clone(),
                    to_path: f.path.clone(),
                    to_line: mark.line,
                    symbol: name.clone(),
                    note: trf("増えた variant: {v}", &[("v", added.join(", "))]),
                    evidence: clip(mark.raw.trim(), EVIDENCE_CHARS),
                });
            }
        }
    }
}

/// 規則 4: ファイルが消えた / 改名されたのに、相手が `mod` / `use` を書いている。
fn emit_module_gone(a: &Owner, facts: &Facts, b: &Owner, out: &mut Vec<Warning>) {
    for (name, path) in &facts.modules {
        if name.len() < MIN_NAME_LEN {
            continue;
        }
        let n = regex::escape(name);
        let Ok(decl) = Regex::new(&format!(
            r"(?m)^[ \t]*(?:pub(?:[ \t]*\([^)\n]*\))?[ \t]+)?mod[ \t]+{n}[ \t]*;"
        )) else {
            continue;
        };
        let Ok(qualified) =
            Regex::new(&format!(r"\b(?:crate|super|self)[ \t]*::[ \t]*{n}[ \t]*::"))
        else {
            continue;
        };
        let Ok(imported) = Regex::new(&format!(r"(?m)^[ \t]*use[ \t]+[^;\n]*\b{n}[ \t]*::")) else {
            continue;
        };
        for f in &b.files {
            for h in &f.hunks {
                for (re, conf) in [
                    (&decl, Confidence::High),
                    (&qualified, Confidence::High),
                    (&imported, Confidence::Medium),
                ] {
                    let Some(m) = re.find(&h.after.text) else {
                        continue;
                    };
                    let Some(mark) = h.after.mark_at(m.start()) else {
                        continue;
                    };
                    if !mark.changed {
                        continue;
                    }
                    out.push(Warning {
                        rule: Rule::ModuleGone,
                        confidence: conf,
                        from: a.label.clone(),
                        from_path: path.clone(),
                        from_line: 0,
                        to: b.label.clone(),
                        to_path: f.path.clone(),
                        to_line: mark.line,
                        symbol: name.clone(),
                        note: String::new(),
                        evidence: clip(mark.raw.trim(), EVIDENCE_CHARS),
                    });
                }
            }
        }
    }
}

/// B 自身が定義している名前。
fn facts_defined(b: &Owner) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for f in &b.files {
        for h in &f.hunks {
            for d in defs_in(&h.after) {
                out.insert(d.name);
            }
        }
    }
    out
}

/// 1 シンボルあたりに拾う参照の上限 (1 つの名前で画面を埋めない)。
fn limit_per_symbol() -> usize {
    8
}

// ═══════════════════════════════════════════════════════════════════════
//  9. レイアウト (純粋関数)
// ═══════════════════════════════════════════════════════════════════════

/// 空状態カードの最大幅。
const EMPTY_CARD_MAX_W: f32 = 460.0;
/// 空状態カードの高さ。
const EMPTY_CARD_H: f32 = 150.0;

/// 空状態カードの矩形。**常に `avail` の中央 1 枚で、どの寸法でも必ず収まる。**
pub fn empty_card(avail: egui::Rect) -> egui::Rect {
    let aw = avail.width().max(0.0);
    let ah = avail.height().max(0.0);
    let w = (aw - space::LG * 2.0).clamp(0.0, EMPTY_CARD_MAX_W).min(aw);
    let h = EMPTY_CARD_H.min(ah);
    let x = avail.left() + (aw - w) * 0.5;
    let y = avail.top() + (ah - h) * 0.5;
    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))
}

/// 1 行の列幅。**どの幅でも合計が可用幅を超えない。**
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowLayout {
    pub conf_w: f32,
    pub who_w: f32,
    pub sym_w: f32,
    pub detail_w: f32,
    /// 列の間隔。
    pub gap: f32,
    /// 狭すぎて詳細を畳んだか (畳んだら 1 行に収める代わりにホバーで出す)。
    pub compact: bool,
}

const CONF_W: f32 = 34.0;
const WHO_MIN: f32 = 90.0;
const WHO_MAX: f32 = 150.0;
const SYM_MIN: f32 = 80.0;
const SYM_MAX: f32 = 200.0;
const DETAIL_MIN: f32 = 120.0;

/// 可用幅から列幅を決める (純粋関数)。
pub fn row_layout(avail_w: f32) -> RowLayout {
    let gap = space::SM;
    let w = avail_w.max(0.0);
    // 3 つの隙間 + 確信度の札。ここが取れないなら全部を比率で割る。
    let fixed = CONF_W + gap * 3.0;
    if w < fixed + WHO_MIN + SYM_MIN + DETAIL_MIN {
        // 狭い: 確信度 + 名前 + 相手 の 3 列へ縮退する。
        let inner = (w - gap * 2.0).max(0.0);
        let conf_w = CONF_W.min(inner);
        let rest = (inner - conf_w).max(0.0);
        return RowLayout {
            conf_w,
            who_w: rest * 0.45,
            sym_w: rest * 0.55,
            detail_w: 0.0,
            gap,
            compact: true,
        };
    }
    let rest = w - fixed;
    let who_w = (rest * 0.22).clamp(WHO_MIN, WHO_MAX);
    let sym_w = ((rest - who_w) * 0.28).clamp(SYM_MIN, SYM_MAX);
    let detail_w = (rest - who_w - sym_w).max(DETAIL_MIN);
    RowLayout {
        conf_w: CONF_W,
        who_w,
        sym_w,
        detail_w,
        gap,
        compact: false,
    }
}

/// 行の中の各セルの矩形 (純粋関数)。**必ず `row` の中に収まり、重ならない。**
pub fn row_rects(row: egui::Rect, lay: &RowLayout) -> Vec<egui::Rect> {
    let mut out = Vec::new();
    let mut x = row.left();
    let widths: Vec<f32> = if lay.compact {
        vec![lay.conf_w, lay.who_w, lay.sym_w]
    } else {
        vec![lay.conf_w, lay.who_w, lay.sym_w, lay.detail_w]
    };
    for (i, wdt) in widths.iter().enumerate() {
        let left = x.min(row.right());
        let right = (left + wdt.max(0.0)).min(row.right());
        out.push(egui::Rect::from_min_max(
            egui::pos2(left, row.top()),
            egui::pos2(right, row.bottom()),
        ));
        x = right + if i + 1 < widths.len() { lay.gap } else { 0.0 };
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════
//  10. 収集 (裏のスレッドで走る) — UI スレッドは絶対に待たない
// ═══════════════════════════════════════════════════════════════════════

/// 走査 1 回ぶんの結果。
#[derive(Clone, Debug, Default)]
struct Snapshot {
    report: Report,
    /// 担当者の表示名 (画面のヘッダに出す)。
    labels: Vec<String>,
    /// 使った共通ベース (短縮 OID)。取れなければ `None`。
    base: Option<String>,
    /// 降格・不能の理由。**そのまま画面へ出す**。
    note: Option<String>,
    cost: Duration,
}

/// GUI が開いているワークスペースのルート。
///
/// `app.rs` を触らずに済ませるため、**自分自身のインスタンス登録**から引く
/// ([`crate::lease`] と同じ手)。登録が無ければカレントディレクトリへ落ちる。
fn gui_workspace_root() -> PathBuf {
    let me = std::process::id();
    crate::instances::scan_and_prune(&crate::instances::instances_dir())
        .into_iter()
        .find(|e| e.pid == me)
        .and_then(|e| e.workspace_roots.first().map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// 走査対象 1 本。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Tree {
    label: String,
    dir: PathBuf,
}

/// 本体 + linked worktree を集める。**ハードコードしたパスは 1 つも無い。**
fn trees_of(root: &Path) -> Vec<Tree> {
    let Ok(top) = crate::worktree::repo_root(root) else {
        return Vec::new();
    };
    let mut out = vec![Tree {
        label: leaf_name(&top),
        dir: top.clone(),
    }];
    if let Ok(porcelain) = crate::worktree::git_out(&top, &["worktree", "list", "--porcelain"]) {
        for (branch, dir) in crate::git::worktree_holders(&porcelain, &top) {
            out.push(Tree {
                label: if branch.is_empty() {
                    leaf_name(&dir)
                } else {
                    branch
                },
                dir,
            });
        }
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out.dedup_by(|a, b| a.dir == b.dir);
    out
}

fn leaf_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}

/// 走査 1 回。**裏のスレッドから呼ぶこと** (git を 2N 回起動する)。
fn scan(root: &Path, limits: Limits) -> Snapshot {
    let t0 = Instant::now();
    let trees = trees_of(root);
    if trees.len() < 2 {
        return Snapshot {
            note: Some(tr(
                "並列ワークツリーが 1 本しかありません。意味的衝突は 2 人以上から起きます",
            )),
            labels: trees.into_iter().map(|t| t.label).collect(),
            cost: t0.elapsed(),
            ..Snapshot::default()
        };
    }
    let heads: Vec<Option<String>> = trees
        .iter()
        .map(|t| {
            crate::worktree::git_out(&t.dir, &["rev-parse", "HEAD"])
                .ok()
                .map(|s| s.trim().to_string())
        })
        .collect();
    let live: Vec<&str> = heads.iter().filter_map(|h| h.as_deref()).collect();
    let mut base = None;
    if live.len() == trees.len() {
        let mut args: Vec<&str> = vec!["merge-base", "--octopus"];
        args.extend_from_slice(&live);
        base = crate::worktree::git_out(&trees[0].dir, &args)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }
    let Some(base) = base else {
        return Snapshot {
            note: Some(tr(
                "共通ベースが取れないので意味解析はできません (履歴が繋がっていない可能性)",
            )),
            labels: trees.into_iter().map(|t| t.label).collect(),
            cost: t0.elapsed(),
            ..Snapshot::default()
        };
    };
    let mut owners = Vec::new();
    for t in &trees {
        // 文脈行つきで取る (enum 本体と match の網羅性を見るのに要る)。
        let diff = crate::worktree::git_out(
            &t.dir,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--find-renames",
                "--unified=3",
                &base,
            ],
        )
        .unwrap_or_default();
        owners.push(owner_from_diff(&t.label, &diff));
    }
    let labels: Vec<String> = owners
        .iter()
        .map(|o| format!("{} ({})", o.label, o.touched()))
        .collect();
    let report = analyze(&owners, limits);
    Snapshot {
        report,
        labels,
        base: Some(base.chars().take(8).collect()),
        note: None,
        cost: t0.elapsed(),
    }
}

/// 設定を読む。`app.rs` を触らずに済ませるため、**設定ファイルから直に**引く
/// (`ZaivernApp` のフィールドは非公開で、`Config` を出すメソッドが無いため)。
fn limits_from_config(root: &Path) -> Limits {
    let cfg = crate::config::load(std::slice::from_ref(&root.to_path_buf()), false);
    let max = cfg.feature_i64("semconf.max_warnings");
    let only_high = cfg.feature_bool("semconf.only_high");
    Limits {
        max_warnings: max.clamp(1, 500) as usize,
        min_confidence: if only_high {
            Confidence::High
        } else {
            Confidence::Medium
        },
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  11. パネル — `app.rs` を 1 バイトも触らずにウィンドウを出す
// ═══════════════════════════════════════════════════════════════════════

/// パネルの状態。**ウィンドウより長生きさせる** (設計原則 1) ため、
/// `ZaivernApp` のフィールドではなくモジュール側に置く。
#[derive(Default)]
struct PanelState {
    open: bool,
    root: PathBuf,
    snap: Snapshot,
    pending: Option<Receiver<Snapshot>>,
    last_scan: Option<Instant>,
    last_cost: Option<Duration>,
}

fn state() -> &'static Mutex<PanelState> {
    static S: OnceLock<Mutex<PanelState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(PanelState::default()))
}

/// パレットの項目から呼ぶ入口。
pub fn open_panel() {
    let root = gui_workspace_root();
    if let Ok(mut st) = state().lock() {
        st.open = true;
        st.root = root;
        st.last_scan = None; // 開いた回は必ず取り直す
    }
}

/// パネルの開閉を切り替える。
pub fn toggle_panel() {
    let opened = state().lock().map(|s| s.open).unwrap_or(false);
    if opened {
        if let Ok(mut st) = state().lock() {
            st.open = false;
        }
    } else {
        open_panel();
    }
}

fn spawn_scan(root: PathBuf) -> Receiver<Snapshot> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let limits = limits_from_config(&root);
        let _ = tx.send(scan(&root, limits));
    });
    rx
}

/// 非同期の結果を拾い、必要なら次の走査を出す。**待たない。**
fn poll(st: &mut PanelState, ctx: &egui::Context) {
    if let Some(rx) = &st.pending {
        match rx.try_recv() {
            Ok(s) => {
                st.last_cost = Some(s.cost);
                st.snap = s;
                st.last_scan = Some(Instant::now());
                st.pending = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => st.pending = None,
        }
    }
    if st.pending.is_none() {
        let due = st
            .last_scan
            .is_none_or(|t| t.elapsed() >= crate::git::scan_interval(SCAN_BASE, st.last_cost));
        if due {
            st.pending = Some(spawn_scan(st.root.clone()));
        }
    }
    // 開いている間だけ、結果を拾うために軽く回す。
    ctx.request_repaint_after(Duration::from_millis(400));
}

/// 毎フレーム呼ばれる描画。**閉じているフレームは 1 ピクセルも触らない**
/// (設計原則 3: アイドル時のコストはゼロ)。
pub fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {
    let _ = app; // 状態はモジュール側に持つので app の中身へは触らない
    let Ok(mut st) = state().lock() else { return };
    if !st.open {
        return;
    }
    poll(&mut st, ctx);
    let mut open = true;
    let mut refresh = false;
    egui::Window::new(tr("🧭 意味的衝突 — ファイルは違うのに噛み合わない変更"))
        .collapsible(false)
        .resizable(true)
        .default_width(720.0)
        .default_height(460.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            refresh = body(ui, &st);
        });
    if !open {
        st.open = false;
    }
    if refresh {
        st.last_scan = None;
    }
}

/// 本体。押されたら `true` (再走査の要求)。
fn body(ui: &mut egui::Ui, st: &PanelState) -> bool {
    let mut refresh = false;
    let vis = ui.visuals().clone();
    let dim = vis.weak_text_color();
    ui.horizontal_wrapped(|ui| {
        let rep = &st.snap.report;
        ui.label(
            egui::RichText::new(trf(
                "担当 {n} 人 / 警告 {w} 件",
                &[
                    ("n", rep.owners.to_string()),
                    ("w", rep.warnings.len().to_string()),
                ],
            ))
            .strong(),
        );
        if let Some(b) = &st.snap.base {
            ui.label(egui::RichText::new(trf("ベース {b}", &[("b", b.clone())])).color(dim));
        }
        if let Some(c) = st.last_cost {
            ui.label(
                egui::RichText::new(format!("{} ms", c.as_millis()))
                    .color(dim)
                    .small(),
            )
            .on_hover_text(tr("走査は裏のスレッドで行うので、UI は止まりません"));
        }
        if st.pending.is_some() {
            ui.spinner();
        }
        if ui.button(tr("再走査")).clicked() {
            refresh = true;
        }
    });
    // 担当者の一覧は 1 行に収め、狭ければ省略してホバーで全文。
    if !st.snap.labels.is_empty() {
        let all = st.snap.labels.join(" · ");
        let w = ui.available_width();
        ui.label(
            egui::RichText::new(clip(&all, ((w / 7.0) as usize).max(12)))
                .color(dim)
                .small(),
        )
        .on_hover_text(all);
    }
    if let Some(note) = &st.snap.note {
        ui.colored_label(vis.warn_fg_color, note);
    }
    ui.separator();

    if st.snap.report.warnings.is_empty() {
        empty_state(ui, &st.snap);
        return refresh;
    }
    // 打ち切りは**無音にしない**。
    if st.snap.report.omitted > 0 {
        ui.colored_label(
            vis.warn_fg_color,
            trf(
                "⚠ 上限 {n} 件で打ち切りました (残り {m} 件)",
                &[
                    ("n", st.snap.report.warnings.len().to_string()),
                    ("m", st.snap.report.omitted.to_string()),
                ],
            ),
        );
    }
    if st.snap.report.low > 0 {
        ui.label(
            egui::RichText::new(trf(
                "確信度が下限に届かなかった {n} 件は出していません",
                &[("n", st.snap.report.low.to_string())],
            ))
            .color(dim)
            .small(),
        );
    }
    egui::ScrollArea::vertical()
        .id_salt("zv-semconf-rows")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let lay = row_layout(ui.available_width());
            for w in &st.snap.report.warnings {
                warning_row(ui, w, &lay, &vis);
            }
        });
    refresh
}

/// 空状態。**利用可能領域の中央に 1 枚のカード**で出す (下に取り残さない)。
fn empty_state(ui: &mut egui::Ui, snap: &Snapshot) {
    let vis = ui.visuals().clone();
    let avail = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    let card = empty_card(avail);
    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(card), |ui| {
        egui::Frame::none()
            .fill(vis.faint_bg_color)
            .stroke(egui::Stroke::new(1.0_f32, vis.widgets.noninteractive.bg_stroke.color))
            .rounding(egui::Rounding::same(10.0))
            .inner_margin(egui::Margin::same(space::MD))
            .show(ui, |ui| {
                ui.set_width((card.width() - space::MD * 2.0).max(0.0));
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new(tr("噛み合わない変更は見つかりません")).size(14.0));
                    ui.add_space(space::XS);
                    ui.label(
                        egui::RichText::new(tr(
                            "Rust の公開定義・シグネチャ・enum の variant・モジュールの消失だけを見ています",
                        ))
                        .small()
                        .color(vis.weak_text_color()),
                    );
                    if snap.report.low > 0 {
                        ui.label(
                            egui::RichText::new(trf(
                                "確信度が足りない候補が {n} 件ありました",
                                &[("n", snap.report.low.to_string())],
                            ))
                            .small()
                            .color(vis.weak_text_color()),
                        );
                    }
                });
            });
    });
}

fn conf_color(c: Confidence, vis: &egui::Visuals) -> egui::Color32 {
    match c {
        Confidence::High => vis.error_fg_color,
        Confidence::Medium => vis.warn_fg_color,
        Confidence::Low => vis.weak_text_color(),
    }
}

/// 警告 1 行。**どの幅でも見切れない。**
///
/// セルの矩形は [`row_rects`] が決める — つまり画面に出る位置と、
/// 「収まり・重ならない」を検査するテストが**同じ関数**を通る。
/// 描画側だけ別の計算をしていると、テストは緑のまま画面が壊れる。
fn warning_row(ui: &mut egui::Ui, w: &Warning, lay: &RowLayout, vis: &egui::Visuals) {
    let msg = w.message();
    let hint = format!("{msg}\n{}:{}\n{}", w.to_path, w.to_line, w.evidence);
    let h = ui.text_style_height(&egui::TextStyle::Body);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
    let cells = row_rects(rect, lay);
    let put = |ui: &mut egui::Ui, i: usize, text: egui::RichText| -> Option<egui::Response> {
        let cell = *cells.get(i)?;
        // 幅ゼロのセルには何も描かない (空白を作らない)。
        (cell.width() > 1.0).then(|| ui.put(cell, egui::Label::new(text).truncate()))
    };
    if let Some(r) = put(
        ui,
        0,
        egui::RichText::new(tr(w.confidence.label()))
            .color(conf_color(w.confidence, vis))
            .strong(),
    ) {
        r.on_hover_text(tr("確信度"));
    }
    if let Some(r) = put(ui, 1, egui::RichText::new(&w.to)) {
        r.on_hover_text(trf(
            "{from} → {to}",
            &[("from", w.from.clone()), ("to", w.to.clone())],
        ));
    }
    if let Some(r) = put(
        ui,
        2,
        egui::RichText::new(format!("{} {}", w.rule.icon(), w.symbol)).strong(),
    ) {
        r.on_hover_text(tr(w.rule.label()));
    }
    if let Some(r) = put(ui, 3, egui::RichText::new(&msg)) {
        r.on_hover_text(&hint);
    }
    if lay.compact {
        // 狭いときは詳細を 1 段下げる (横に伸ばして見切れさせない)。
        ui.add(
            egui::Label::new(
                egui::RichText::new(&msg)
                    .small()
                    .color(vis.weak_text_color()),
            )
            .truncate(),
        )
        .on_hover_text(&hint);
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  12. 登録
// ═══════════════════════════════════════════════════════════════════════

/// パレットへの登録。**共有ファイルを 1 バイトも触らずに機能が繋がる**入口。
///
/// 打鍵は割り当てていない — `keybinds::BindAction` は固定長配列 + 件数検査を
/// 持つ最も硬い共有面で、機能ブランチ側から増やすと直列マージが必ず衝突する。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "semconf",
    entries: &[crate::feature::Entry {
        icon: "🧭",
        label: "意味的衝突 — ファイルは違うのに噛み合わない変更を見る",
        id: "semconf.open",
    }],
    dispatch: |_app, _ctx, id| match id {
        "semconf.open" => {
            toggle_panel();
            true
        }
        _ => false,
    },
    // 窓は中央ビューに属さないオーバーレイなので、毎フレームここから描く。
    // **閉じているときは 1 命令も走らない** ので、アイドル時のコストはゼロ。
    draw: Some(draw),
    settings: &[
        crate::feature::Setting {
            key: "semconf.only_high",
            label: "意味的衝突は確信度が高いものだけ出す",
            help: "誤検出をさらに減らしたいときに入れます。既定では中確信度まで出します。",
            default: crate::feature::SettingValue::Bool(false),
        },
        crate::feature::Setting {
            key: "semconf.max_warnings",
            label: "意味的衝突を一度に出す上限 (件)",
            help: "超えたぶんは件数として画面に出します (黙って切りません)。",
            default: crate::feature::SettingValue::Int(MAX_WARNINGS as i64),
        },
    ],
    binds: &[],
};

// ═══════════════════════════════════════════════════════════════════════
//  13. テスト
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の diff を組む小さなヘルパ。
    fn diff(path: &str, before: &str, after: &str) -> String {
        let old: Vec<&str> = before.lines().collect();
        let new: Vec<&str> = after.lines().collect();
        let mut s = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n");
        s.push_str(&format!(
            "@@ -1,{} +1,{} @@\n",
            old.len().max(1),
            new.len().max(1)
        ));
        for l in &old {
            s.push_str(&format!("-{l}\n"));
        }
        for l in &new {
            s.push_str(&format!("+{l}\n"));
        }
        s
    }

    /// 文脈行を明示して組む (`enum` 本体や `match` の網羅性を見るテスト用)。
    /// 各行は先頭 1 文字が ` ` / `+` / `-`。
    fn raw_diff(path: &str, lines: &[&str]) -> String {
        let old = lines.iter().filter(|l| !l.starts_with('+')).count().max(1);
        let new = lines.iter().filter(|l| !l.starts_with('-')).count().max(1);
        let mut s = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n");
        s.push_str(&format!("@@ -1,{old} +1,{new} @@\n"));
        for l in lines {
            s.push_str(l);
            s.push('\n');
        }
        s
    }

    fn owner(label: &str, diffs: &[String]) -> Owner {
        owner_from_diff(label, &diffs.join(""))
    }

    fn run(a: Owner, b: Owner) -> Report {
        analyze(&[a, b], Limits::default())
    }

    fn symbols(r: &Report) -> Vec<&str> {
        r.warnings.iter().map(|w| w.symbol.as_str()).collect()
    }

    // ── 無害化 (誤検出の一次防壁) ─────────────────────────────────

    #[test]
    fn 行コメントと文字列の中身は照合対象から消える() {
        let mut m = SanMode::default();
        let got = sanitize_line(r#"let s = "run_agent(x)"; // call run_agent"#, &mut m);
        assert!(
            !got.contains("run_agent"),
            "コメント/文字列が残った: {got:?}"
        );
        assert!(got.contains("let s ="), "地のコードまで消えた: {got:?}");
        assert_eq!(m, SanMode::Code);
    }

    #[test]
    fn 生文字列とブロックコメントも消える() {
        let mut m = SanMode::default();
        let a = sanitize_line(r##"let s = r#"run_agent(1)"#; let t = 1;"##, &mut m);
        assert!(!a.contains("run_agent"), "生文字列が残った: {a:?}");
        assert!(a.contains("let t"), "閉じた後のコードが消えた: {a:?}");

        let mut m = SanMode::default();
        let b = sanitize_line("/* run_agent(1)", &mut m);
        assert!(!b.contains("run_agent"));
        assert_eq!(m, SanMode::Block(1), "ブロックが行を跨いでいない");
        let c = sanitize_line("still_comment(); */ real_code();", &mut m);
        assert!(
            !c.contains("still_comment"),
            "継続中のコメントが残った: {c:?}"
        );
        assert!(c.contains("real_code"), "閉じた後が消えた: {c:?}");
        assert_eq!(m, SanMode::Code);
    }

    #[test]
    fn ライフタイムを文字リテラルと誤らない() {
        let mut m = SanMode::default();
        let got = sanitize_line("fn f<'a>(x: &'a str) -> Widget { }", &mut m);
        assert!(
            got.contains("Widget"),
            "ライフタイム以降を飲み込んだ: {got:?}"
        );
        assert_eq!(m, SanMode::Code, "文字列の途中だと誤認した");

        let mut m = SanMode::default();
        let got = sanitize_line(r"let c = 'x'; let d = Widget;", &mut m);
        assert!(got.contains("Widget"));
        assert_eq!(m, SanMode::Code);
    }

    #[test]
    fn ハンクがブロックコメントの途中から始まっても地のコードと誤らない() {
        let rows = vec![
            Row {
                line: 1,
                changed: false,
                raw: " * old_api(1) を使う".into(),
            },
            Row {
                line: 2,
                changed: false,
                raw: " */".into(),
            },
            Row {
                line: 3,
                changed: true,
                raw: "let x = 1;".into(),
            },
        ];
        assert!(starts_inside_block_comment(&rows));
        let j = Joined::build(&rows);
        assert!(
            !j.text.contains("old_api"),
            "コメントの中身が残った: {}",
            j.text
        );
        assert!(j.text.contains("let x"));
    }

    // ── 規則 1: 定義の消滅 ────────────────────────────────────────

    #[test]
    fn 規則1_消えた公開関数を相手が新しく呼んでいると当たる() {
        let a = owner(
            "A",
            &[diff("src/api.rs", "pub fn dispatch_agent(n: u8) {}", "")],
        );
        let b = owner("B", &[diff("src/caller.rs", "", "    dispatch_agent(3);")]);
        let r = run(a, b);
        assert_eq!(symbols(&r), vec!["dispatch_agent"], "{:#?}", r.warnings);
        assert_eq!(r.warnings[0].rule, Rule::DefinitionGone);
        assert_eq!(r.warnings[0].confidence, Confidence::High);
        assert_eq!(r.warnings[0].to_path, "src/caller.rs");
        println!("── 規則1 ── {}", r.warnings[0].message());
    }

    #[test]
    fn 規則1_消えた公開型を相手が構造体リテラルで書いていると当たる() {
        let a = owner("A", &[diff("src/api.rs", "pub struct AgentSpec {}", "")]);
        let b = owner(
            "B",
            &[diff(
                "src/caller.rs",
                "",
                "    let s = AgentSpec { id: 1 };",
            )],
        );
        let r = run(a, b);
        assert_eq!(symbols(&r), vec!["AgentSpec"]);
        assert_eq!(r.warnings[0].confidence, Confidence::High);
    }

    #[test]
    fn 規則1_コメントと文字列の中の同名は当たらない() {
        let a = owner(
            "A",
            &[diff("src/api.rs", "pub fn dispatch_agent(n: u8) {}", "")],
        );
        let b = owner(
            "B",
            &[diff(
                "src/caller.rs",
                "",
                "    // dispatch_agent(3) を呼ぶ予定\n    let s = \"dispatch_agent(3)\";",
            )],
        );
        let r = run(a, b);
        assert!(
            r.warnings.is_empty(),
            "コメント/文字列で当たった: {:#?}",
            r.warnings
        );
    }

    #[test]
    fn 規則1_単語の一部には当たらない() {
        let a = owner("A", &[diff("src/api.rs", "pub fn dispatch(n: u8) {}", "")]);
        let b = owner(
            "B",
            &[diff(
                "src/caller.rs",
                "",
                "    dispatch_all(3); let x = predispatch(1);",
            )],
        );
        let r = run(a, b);
        assert!(
            r.warnings.is_empty(),
            "部分一致で当たった: {:#?}",
            r.warnings
        );
    }

    #[test]
    fn 規則1_非公開の定義は候補にしない() {
        let a = owner(
            "A",
            &[diff("src/api.rs", "fn dispatch_agent(n: u8) {}", "")],
        );
        let b = owner("B", &[diff("src/caller.rs", "", "    dispatch_agent(3);")]);
        let r = run(a, b);
        assert!(r.warnings.is_empty(), "非公開で当たった: {:#?}", r.warnings);
    }

    #[test]
    fn 規則1_相手が自分でも同じ名前を定義しているなら当たらない() {
        let a = owner(
            "A",
            &[diff("src/api.rs", "pub fn dispatch_agent(n: u8) {}", "")],
        );
        let b = owner(
            "B",
            &[diff(
                "src/mine.rs",
                "",
                "pub fn dispatch_agent(n: u8) {}\nfn go() { dispatch_agent(1); }",
            )],
        );
        let r = run(a, b);
        assert!(
            r.warnings.is_empty(),
            "自前の定義で当たった: {:#?}",
            r.warnings
        );
    }

    #[test]
    fn 規則1_別ファイルへ移しただけなら消滅と数えない() {
        let a = owner(
            "A",
            &[
                diff("src/api.rs", "pub fn dispatch_agent(n: u8) {}", ""),
                diff("src/api2.rs", "", "pub fn dispatch_agent(n: u8) {}"),
            ],
        );
        let b = owner("B", &[diff("src/caller.rs", "", "    dispatch_agent(3);")]);
        let r = run(a, b);
        assert!(r.warnings.is_empty(), "移動で当たった: {:#?}", r.warnings);
    }

    #[test]
    fn 規則1_標準トレイトのメソッド名は除外する() {
        let a = owner("A", &[diff("src/api.rs", "    pub fn new(n: u8) {}", "")]);
        let b = owner(
            "B",
            &[diff("src/caller.rs", "", "    let x = Other::new(3);")],
        );
        let r = run(a, b);
        assert!(r.warnings.is_empty(), "new で当たった: {:#?}", r.warnings);
    }

    #[test]
    fn 規則1_文脈行にしかない参照は当たらない() {
        // B が「今書いた」わけではないものは意味的衝突ではない。
        let a = owner(
            "A",
            &[diff("src/api.rs", "pub fn dispatch_agent(n: u8) {}", "")],
        );
        let b = owner(
            "B",
            &[raw_diff(
                "src/caller.rs",
                &[
                    " fn go() {",
                    "     dispatch_agent(3);",
                    "+    let z = 1;",
                    " }",
                ],
            )],
        );
        let r = run(a, b);
        assert!(r.warnings.is_empty(), "文脈行で当たった: {:#?}", r.warnings);
    }

    // ── 規則 2: シグネチャ変更 ────────────────────────────────────

    #[test]
    fn 規則2_引数の数が変わったのに古い呼び方だと当たる() {
        let a = owner(
            "A",
            &[diff(
                "src/api.rs",
                "pub fn spawn_worker(a: u8, b: u8) {}",
                "pub fn spawn_worker(a: u8) {}",
            )],
        );
        let b = owner("B", &[diff("src/caller.rs", "", "    spawn_worker(1, 2);")]);
        let r = run(a, b);
        let sig: Vec<_> = r
            .warnings
            .iter()
            .filter(|w| w.rule == Rule::SignatureChanged)
            .collect();
        assert_eq!(sig.len(), 1, "{:#?}", r.warnings);
        assert_eq!(sig[0].confidence, Confidence::High);
        println!("── 規則2 ── {}", sig[0].message());
    }

    #[test]
    fn 規則2_相手が既に新しい形で呼んでいるなら当たらない() {
        let a = owner(
            "A",
            &[diff(
                "src/api.rs",
                "pub fn spawn_worker(a: u8, b: u8) {}",
                "pub fn spawn_worker(a: u8) {}",
            )],
        );
        let b = owner("B", &[diff("src/caller.rs", "", "    spawn_worker(1);")]);
        let r = run(a, b);
        assert!(
            r.warnings.iter().all(|w| w.rule != Rule::SignatureChanged),
            "追随済みなのに当たった: {:#?}",
            r.warnings
        );
    }

    #[test]
    fn 規則2_引数の型だけが変わったら中確信度で出す() {
        let a = owner(
            "A",
            &[diff(
                "src/api.rs",
                "pub fn spawn_worker(a: u8) {}",
                "pub fn spawn_worker(a: String) {}",
            )],
        );
        let b = owner("B", &[diff("src/caller.rs", "", "    spawn_worker(1);")]);
        let r = run(a, b);
        let sig: Vec<_> = r
            .warnings
            .iter()
            .filter(|w| w.rule == Rule::SignatureChanged)
            .collect();
        assert_eq!(sig.len(), 1, "{:#?}", r.warnings);
        assert_eq!(sig[0].confidence, Confidence::Medium);
    }

    #[test]
    fn 規則2_引数の数がどちらとも違うなら別物として出さない() {
        let a = owner(
            "A",
            &[diff(
                "src/api.rs",
                "pub fn spawn_worker(a: u8, b: u8) {}",
                "pub fn spawn_worker(a: u8) {}",
            )],
        );
        // 5 引数は旧 (2) とも新 (1) とも違う = 同名の別関数の可能性が高い
        let b = owner(
            "B",
            &[diff(
                "src/caller.rs",
                "",
                "    spawn_worker(1, 2, 3, 4, 5);",
            )],
        );
        let r = run(a, b);
        assert!(
            r.warnings.iter().all(|w| w.rule != Rule::SignatureChanged),
            "別物に当たった: {:#?}",
            r.warnings
        );
    }

    // ── 規則 3: enum の variant 追加 ──────────────────────────────

    #[test]
    fn 規則3_variantが増えてワイルドカードの無いmatchがあると当たる() {
        let a = owner(
            "A",
            &[raw_diff(
                "src/api.rs",
                &[
                    " pub enum StageKind {",
                    "     Plan,",
                    "+    Review,",
                    "     Done,",
                    " }",
                ],
            )],
        );
        let b = owner(
            "B",
            &[raw_diff(
                "src/caller.rs",
                &[
                    " fn go(k: StageKind) {",
                    "+    match k {",
                    "+        StageKind::Plan => 1,",
                    "+        StageKind::Done => 2,",
                    "+    };",
                    " }",
                ],
            )],
        );
        let r = run(a, b);
        let v: Vec<_> = r
            .warnings
            .iter()
            .filter(|w| w.rule == Rule::VariantAdded)
            .collect();
        assert_eq!(v.len(), 1, "{:#?}", r.warnings);
        assert_eq!(v[0].symbol, "StageKind");
        println!("── 規則3 ── {}", v[0].message());
    }

    #[test]
    fn 規則3_ワイルドカードのあるmatchには当たらない() {
        let a = owner(
            "A",
            &[raw_diff(
                "src/api.rs",
                &[" pub enum StageKind {", "     Plan,", "+    Review,", " }"],
            )],
        );
        let b = owner(
            "B",
            &[raw_diff(
                "src/caller.rs",
                &[
                    " fn go(k: StageKind) {",
                    "+    match k {",
                    "+        StageKind::Plan => 1,",
                    "+        _ => 0,",
                    "+    };",
                    " }",
                ],
            )],
        );
        let r = run(a, b);
        assert!(
            r.warnings.iter().all(|w| w.rule != Rule::VariantAdded),
            "ワイルドカードがあるのに当たった: {:#?}",
            r.warnings
        );
    }

    #[test]
    fn 規則3_非公開enumには当たらない() {
        let a = owner(
            "A",
            &[raw_diff(
                "src/api.rs",
                &[" enum StageKind {", "     Plan,", "+    Review,", " }"],
            )],
        );
        let b = owner(
            "B",
            &[raw_diff(
                "src/caller.rs",
                &["+    match k { StageKind::Plan => 1, StageKind::Done => 2 };"],
            )],
        );
        let r = run(a, b);
        assert!(
            r.warnings.iter().all(|w| w.rule != Rule::VariantAdded),
            "非公開 enum で当たった: {:#?}",
            r.warnings
        );
    }

    // ── 規則 4: ファイルの消滅 / 改名 ─────────────────────────────

    #[test]
    fn 規則4_消えたファイルをmodで参照していると当たる() {
        let a = Owner {
            label: "A".into(),
            files: vec![FileCode {
                path: "src/telemetry.rs".into(),
                kind: ChangeKind::Deleted,
                hunks: Vec::new(),
            }],
        };
        let b = owner("B", &[diff("src/main.rs", "", "mod telemetry;")]);
        let r = run(a, b);
        let m: Vec<_> = r
            .warnings
            .iter()
            .filter(|w| w.rule == Rule::ModuleGone)
            .collect();
        assert_eq!(m.len(), 1, "{:#?}", r.warnings);
        assert_eq!(m[0].symbol, "telemetry");
        assert_eq!(m[0].confidence, Confidence::High);
        println!("── 規則4 ── {}", m[0].message());
    }

    #[test]
    fn 規則4_同じ名前で作り直しているなら当たらない() {
        let a = Owner {
            label: "A".into(),
            files: vec![
                FileCode {
                    path: "src/telemetry.rs".into(),
                    kind: ChangeKind::Deleted,
                    hunks: Vec::new(),
                },
                FileCode {
                    path: "src/core/telemetry.rs".into(),
                    kind: ChangeKind::Created,
                    hunks: Vec::new(),
                },
            ],
        };
        let b = owner("B", &[diff("src/main.rs", "", "mod telemetry;")]);
        let r = run(a, b);
        assert!(
            r.warnings.iter().all(|w| w.rule != Rule::ModuleGone),
            "作り直しで当たった: {:#?}",
            r.warnings
        );
    }

    #[test]
    fn 規則4_モジュール名はmod_rsなら親フォルダから起こす() {
        assert_eq!(module_name("src/foo.rs").as_deref(), Some("foo"));
        assert_eq!(module_name("src/net/mod.rs").as_deref(), Some("net"));
        assert_eq!(module_name("src/main.rs"), None);
        assert_eq!(module_name("src/lib.rs"), None);
        assert_eq!(module_name("README.md"), None);
    }

    // ── 誤検出の総合ガード ────────────────────────────────────────

    #[test]
    fn 同名の別スコープと単語の一部では当たらない() {
        // A は `spawn_worker` を消した。B は別モジュールの `pool::spawn_worker_v2`
        // を呼び、変数 `spawn_worker_count` も使う。どちらも当たってはいけない。
        let a = owner(
            "A",
            &[diff("src/api.rs", "pub fn spawn_worker(a: u8) {}", "")],
        );
        let b = owner(
            "B",
            &[diff(
                "src/caller.rs",
                "",
                "    let spawn_worker_count = 1;\n    pool::spawn_worker_v2(1);",
            )],
        );
        let r = run(a, b);
        assert!(r.warnings.is_empty(), "{:#?}", r.warnings);
    }

    #[test]
    fn rust以外のファイルは最初から見ない() {
        let a = owner(
            "A",
            &[diff("src/api.py", "def dispatch_agent(n):\n    pass", "")],
        );
        let b = owner("B", &[diff("src/caller.py", "", "    dispatch_agent(3)")]);
        let r = run(a, b);
        assert!(r.warnings.is_empty(), "Rust 以外を見た: {:#?}", r.warnings);
    }

    #[test]
    fn 担当者が一人なら何も出さない() {
        let a = owner("A", &[diff("src/api.rs", "pub fn dispatch_agent() {}", "")]);
        let r = analyze(&[a], Limits::default());
        assert!(r.warnings.is_empty());
        assert_eq!(r.owners, 1);
    }

    // ── 決定性 ────────────────────────────────────────────────────

    #[test]
    fn 同じ入力なら何度解析しても同じ結果になる() {
        let mk = || {
            (
                owner(
                    "A",
                    &[
                        diff("src/api.rs", "pub fn dispatch_agent(n: u8) {}", ""),
                        diff("src/two.rs", "pub struct AgentSpec {}", ""),
                        diff(
                            "src/three.rs",
                            "pub fn spawn_worker(a: u8, b: u8) {}",
                            "pub fn spawn_worker(a: u8) {}",
                        ),
                    ],
                ),
                owner(
                    "B",
                    &[diff(
                        "src/caller.rs",
                        "",
                        "    dispatch_agent(3);\n    let s = AgentSpec { id: 1 };\n    spawn_worker(1, 2);",
                    )],
                ),
            )
        };
        let (a1, b1) = mk();
        let (a2, b2) = mk();
        let r1 = analyze(&[a1, b1], Limits::default());
        let r2 = analyze(&[a2, b2], Limits::default());
        assert_eq!(r1, r2, "同じ入力で結果が揺れた");
        assert!(r1.warnings.len() >= 3, "{:#?}", r1.warnings);
        // 並びも確信度の降順で安定していること
        for w in r1.warnings.windows(2) {
            assert!(w[0].confidence >= w[1].confidence, "並びが崩れた: {w:#?}");
        }
        println!("── 決定性 ── {} 件", r1.warnings.len());
    }

    #[test]
    fn 担当者の並び順を入れ替えても同じ集合になる() {
        let a = owner(
            "A",
            &[diff("src/api.rs", "pub fn dispatch_agent(n: u8) {}", "")],
        );
        let b = owner("B", &[diff("src/caller.rs", "", "    dispatch_agent(3);")]);
        let fwd = analyze(&[a.clone(), b.clone()], Limits::default());
        let rev = analyze(&[b, a], Limits::default());
        assert_eq!(fwd.warnings, rev.warnings, "順序で結果が変わった");
    }

    // ── 上限と確信度の下限 ────────────────────────────────────────

    #[test]
    fn 上限で打ち切ったら件数を必ず残す() {
        let mut a_diffs = Vec::new();
        let mut b_lines = String::new();
        for i in 0..12 {
            a_diffs.push(diff(
                &format!("src/api{i}.rs"),
                &format!("pub fn dispatch_agent_{i}(n: u8) {{}}"),
                "",
            ));
            b_lines.push_str(&format!("    dispatch_agent_{i}(1);\n"));
        }
        let a = owner("A", &a_diffs);
        let b = owner("B", &[diff("src/caller.rs", "", &b_lines)]);
        let limits = Limits {
            max_warnings: 5,
            min_confidence: Confidence::Medium,
        };
        let r = analyze(&[a, b], limits);
        assert_eq!(r.warnings.len(), 5, "上限を守っていない");
        assert_eq!(r.omitted, 7, "打ち切りが無音になっている: {r:#?}");
        println!(
            "── 上限 ── 表示 {} / 打ち切り {}",
            r.warnings.len(),
            r.omitted
        );
    }

    #[test]
    fn 確信度の下限に届かないものは出さないが件数は残す() {
        // `abc` は見分けが付かない名前 (`_` 無し・6 文字未満・全部小文字) なので Low。
        let a = owner("A", &[diff("src/api.rs", "pub fn abc(n: u8) {}", "")]);
        let b = owner("B", &[diff("src/caller.rs", "", "    abc(3);")]);
        let r = run(a, b);
        assert!(r.warnings.is_empty(), "低確信度を出した: {:#?}", r.warnings);
        assert_eq!(r.low, 1, "落とした件数を数えていない");
    }

    #[test]
    fn 高確信度だけに絞ると中確信度は消える() {
        let a = owner(
            "A",
            &[diff(
                "src/api.rs",
                "pub fn spawn_worker(a: u8) {}",
                "pub fn spawn_worker(a: String) {}",
            )],
        );
        let b = owner("B", &[diff("src/caller.rs", "", "    spawn_worker(1);")]);
        let only_high = Limits {
            max_warnings: MAX_WARNINGS,
            min_confidence: Confidence::High,
        };
        let r = analyze(&[a, b], only_high);
        assert!(r.warnings.is_empty(), "{:#?}", r.warnings);
        assert!(r.low >= 1);
    }

    // ── レイアウト (純粋関数) ─────────────────────────────────────

    #[test]
    fn 空状態カードは可用領域の中央に必ず収まる() {
        for (w, h) in [
            (900.0_f32, 700.0_f32),
            (1200.0, 300.0),
            (320.0, 200.0),
            (100.0, 60.0),
            (0.0, 0.0),
        ] {
            let avail = egui::Rect::from_min_size(egui::pos2(12.0, 34.0), egui::vec2(w, h));
            let card = empty_card(avail);
            assert!(
                avail.contains_rect(card),
                "{w}x{h} でカードがはみ出した: {card:?}"
            );
            if w > 0.0 && h > 0.0 {
                assert!((card.center().x - avail.center().x).abs() < 0.01);
                assert!((card.center().y - avail.center().y).abs() < 0.01);
            }
        }
    }

    #[test]
    fn 行のセルはどの幅でも可用領域に収まり重ならない() {
        for w in [900.0_f32, 1200.0, 640.0, 420.0, 300.0, 160.0, 40.0, 0.0] {
            let lay = row_layout(w);
            let row = egui::Rect::from_min_size(egui::pos2(5.0, 5.0), egui::vec2(w, 20.0));
            let cells = row_rects(row, &lay);
            assert!(!cells.is_empty(), "幅 {w} でセルが 0 個");
            for c in &cells {
                assert!(row.contains_rect(*c), "幅 {w} ではみ出した: {c:?}");
                assert!(c.width() >= 0.0);
            }
            for pair in cells.windows(2) {
                assert!(
                    pair[0].right() <= pair[1].left() + 0.01,
                    "幅 {w} で列が重なった: {:?} / {:?}",
                    pair[0],
                    pair[1]
                );
            }
        }
    }

    #[test]
    fn 狭い幅では詳細列を畳む() {
        assert!(!row_layout(900.0).compact);
        assert!(row_layout(240.0).compact);
        assert_eq!(row_layout(240.0).detail_w, 0.0);
    }

    // ── 部品の単体テスト ──────────────────────────────────────────

    #[test]
    fn 引数の数は入れ子の括弧に惑わされない() {
        assert_eq!(call_arity("f(1, g(2, 3), [4, 5])", 0), Some(3));
        assert_eq!(call_arity("f()", 0), Some(0));
        // 閉じが無ければ「読めない」と正直に返す
        assert_eq!(call_arity("f(1, 2", 0), None);
    }

    #[test]
    fn メソッド呼び出しを見分ける() {
        let t = "    x.run(1);";
        let at = t.find("run").unwrap();
        assert!(is_method_call(t, at));
        let t2 = "    run(1);";
        let at2 = t2.find("run").unwrap();
        assert!(!is_method_call(t2, at2));
        // 範囲演算子 `..` は「メソッド呼び出し」ではない
        let t3 = "    a..run(1)";
        let at3 = t3.find("run").unwrap();
        assert!(!is_method_call(t3, at3));
    }

    #[test]
    fn 見分けの付く名前かを判定する() {
        assert!(distinctive("dispatch_agent"));
        assert!(distinctive("AgentSpec"));
        assert!(distinctive("worker")); // 6 文字
        assert!(!distinctive("abc"));
        assert!(!distinctive("run"));
    }

    #[test]
    fn 定数名がscreaming_caseかを判定する() {
        assert!(is_screaming("MAX_WARNINGS"));
        assert!(is_screaming("PI2"));
        assert!(!is_screaming("MaxWarnings"));
        assert!(!is_screaming("max_warnings"));
        assert!(!is_screaming("_"));
    }

    // ── 登録面 ────────────────────────────────────────────────────

    #[test]
    fn 登録の_id_はモジュール接頭辞で設定キーも同じ接頭辞になっている() {
        assert_eq!(FEATURE.module, "semconf");
        for e in FEATURE.entries {
            assert!(e.id.starts_with("semconf."), "ID がずれている: {}", e.id);
        }
        for s in FEATURE.settings {
            assert!(
                s.key.starts_with("semconf."),
                "設定キーがずれている: {}",
                s.key
            );
        }
        // draw が繋がっていないと画面に出ない (= 未完成)
        assert!(FEATURE.draw.is_some());
    }

    #[test]
    fn 設定から上限と下限を組み立てても実ホームには触らない() {
        // 実 `~/.zaivern` を汚さないため、隔離した一時ディレクトリで読む。
        let dir = crate::test_util::unique_temp_dir("zaivern-semconf", "cfg");
        std::fs::create_dir_all(&dir).expect("一時ディレクトリ");
        let lim = limits_from_config(&dir);
        assert!(lim.max_warnings >= 1 && lim.max_warnings <= 500);
        assert!(lim.min_confidence >= Confidence::Medium);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 走査対象が一本しか無ければ理由を出して黙らない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-semconf", "scan");
        std::fs::create_dir_all(&dir).expect("一時ディレクトリ");
        let snap = scan(&dir, Limits::default());
        assert!(snap.note.is_some(), "理由が出ていない");
        assert!(snap.report.warnings.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// このリポジトリ自身を走査しても落ちず、上限を守る (実データの煙試験)。
    ///
    /// 走査対象のパスは `CARGO_MANIFEST_DIR` から導く — **ユーザー名も
    /// ドライブ文字も書かない**ので、どの環境でも同じように動く
    /// (`panels::egui_id_guard` と同じ手)。linked worktree が 1 本も無い
    /// 環境では [`Snapshot::note`] が出るだけで、どちらでも合格する。
    #[test]
    fn 自分自身のリポジトリを走査しても落ちず上限を守る() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let snap = scan(root, Limits::default());
        assert!(
            snap.report.warnings.len() <= MAX_WARNINGS,
            "上限を超えた: {}",
            snap.report.warnings.len()
        );
        assert!(
            snap.note.is_some() || snap.base.is_some(),
            "走れなかった理由も結果も無い (無音になっている)"
        );
        println!(
            "── 実リポジトリ ── 担当 (触った Rust ファイル数): {}",
            snap.labels.join(" · ")
        );
        println!(
            "── 実リポジトリ ── 担当 {} 人 / 警告 {} 件 / 打ち切り {} / 低確信度 {} / {} ms",
            snap.report.owners,
            snap.report.warnings.len(),
            snap.report.omitted,
            snap.report.low,
            snap.cost.as_millis()
        );
        if let Some(n) = &snap.note {
            println!("── 実リポジトリ ── {n}");
        }
        for w in snap.report.warnings.iter().take(8) {
            println!(
                "── 実リポジトリ ── [{}] {}",
                w.confidence.label(),
                w.message()
            );
        }
    }
}
