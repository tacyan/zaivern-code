//! unified diff のパースと、追加/削除行を色分けするインライン diff ビュー。
//!
//! `git diff` / `gh pr diff` が吐く unified 形式をそのまま受け取り、
//! ファイル単位 → ハンク単位 → 行単位に分解して描画する。
//!
//! パース部 (`parse_unified`) は純関数で、GUI に依存しない。
//! ハンクヘッダの解釈は `git::parse_range` / `git::parse_hunk_marks` と同じ流儀
//! (カウント省略 = 1、行番号は diff 上 1-based) に揃えてある。


use std::collections::HashMap;

use eframe::egui::{self, Color32, FontId, RichText};

use crate::i18n::{tr, trf};
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// データモデル
// ---------------------------------------------------------------------------

/// diff 行の種別。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// 変更なしの文脈行 (先頭 ' ')
    Context,
    /// 追加行 (先頭 '+')
    Added,
    /// 削除行 (先頭 '-')
    Removed,
}

/// diff の 1 行。`old_no` / `new_no` は 1-based の行番号 (存在しない側は None)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    /// 先頭のマーカー (' ' / '+' / '-') を除いた本文。
    pub text: String,
}

/// `@@ -a,b +c,d @@ ...` で区切られる 1 ハンク。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// `@@` 行そのもの (末尾の文脈テキストを含む)。
    pub header: String,
    pub old_start: usize,
    pub new_start: usize,
    pub lines: Vec<DiffLine>,
}

/// 1 ファイル分の diff。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub hunks: Vec<Hunk>,
    pub is_binary: bool,
    pub is_rename: bool,
    pub additions: usize,
    pub deletions: usize,
}

const DEV_NULL: &str = "/dev/null";

impl FileDiff {
    fn new() -> Self {
        FileDiff {
            old_path: String::new(),
            new_path: String::new(),
            hunks: Vec::new(),
            is_binary: false,
            is_rename: false,
            additions: 0,
            deletions: 0,
        }
    }

    /// 表示用のパス。リネームなら `old → new`、それ以外は存在する方。
    pub fn display_path(&self) -> String {
        if self.is_rename && self.old_path != self.new_path {
            format!("{} → {}", self.old_path, self.new_path)
        } else if !self.new_path.is_empty() && self.new_path != DEV_NULL {
            self.new_path.clone()
        } else {
            self.old_path.clone()
        }
    }
}

// ---------------------------------------------------------------------------
// インラインレビューコメント (レビューのループを閉じる)
// ---------------------------------------------------------------------------

/// コメントが指す diff の側。
///
/// 追加行・文脈行は「これから直す側」= 新側、削除行は旧側にしか行番号が無い。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CommentSide {
    /// 変更後 (`+++` 側) の行番号。
    New,
    /// 変更前 (`---` 側) の行番号 — 削除行に付いたコメント。
    Old,
}

/// コメントの取り付け先。
///
/// **行インデックスではなく (パス, 側, 1-based 行番号)** を鍵にするので、
/// 同じ diff を再パースしても・スクロールで仮想化が働いても位置がずれない。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommentAnchor {
    /// エージェントに渡すパス (リネームなら新パス)。
    pub path: String,
    pub side: CommentSide,
    /// 1-based 行番号。
    pub line: usize,
}

impl CommentAnchor {
    pub fn new(path: impl Into<String>, side: CommentSide, line: usize) -> Self {
        CommentAnchor {
            path: path.into(),
            side,
            line,
        }
    }
}

/// diff の 1 行に紐づくレビューコメント。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffComment {
    /// ストア内で一意。編集・削除・解決のキー。
    pub id: u64,
    pub anchor: CommentAnchor,
    /// アンカー行の内容。プロンプトの引用 (`> ...`) に使う。
    pub quote: String,
    pub body: String,
    /// 解決済みはプロンプトから除外される。
    pub resolved: bool,
}

/// エージェントに渡すファイルパス。リネームでも「これから編集する側」= 新パス。
/// 表示用の `display_path()` (`old → new`) とは別物なので混ぜないこと。
pub fn anchor_path(file: &FileDiff) -> String {
    if !file.new_path.is_empty() && file.new_path != DEV_NULL {
        file.new_path.clone()
    } else {
        file.old_path.clone()
    }
}

/// diff 行のクリック先 (側, 1-based 行番号) を決める純関数。
///
/// 追加行・文脈行は**新側**の行番号、削除行は**旧側**の行番号を選ぶ。
/// 行番号を持たない側にしか座標が無い行 (壊れた diff) は `None`。
pub fn line_target(line: &DiffLine) -> Option<(CommentSide, usize)> {
    match line.kind {
        LineKind::Added | LineKind::Context => line.new_no.map(|n| (CommentSide::New, n)),
        LineKind::Removed => line.old_no.map(|n| (CommentSide::Old, n)),
    }
}

/// `line_target` にファイルパスを載せてアンカーにする。
pub fn line_anchor(path: &str, line: &DiffLine) -> Option<CommentAnchor> {
    line_target(line).map(|(side, no)| CommentAnchor::new(path, side, no))
}

/// プロンプトに載せる引用行の最大文字数 (超過分は `…`)。
const MAX_QUOTE_CHARS: usize = 200;
/// コメント本文 1 件の最大文字数 (超過分は `…`)。
const MAX_BODY_CHARS: usize = 2000;
/// 組み立てるプロンプトの先頭行。
const REVIEW_PROMPT_HEADER: &str = "以下のレビューコメントに対応してください:";

/// 改行・タブを空白へ潰して 1 行にし、長すぎれば文字単位で省略する。
///
/// 文字単位なので日本語でも UTF-8 境界を割らない。インデントは情報なので
/// 連続空白は潰さない (行頭のズレがコメントの手掛かりになる)。
fn flatten_one_line(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut truncated = false;
    for (i, ch) in s.chars().enumerate() {
        if i >= max {
            truncated = true;
            break;
        }
        out.push(match ch {
            '\n' | '\r' | '\t' => ' ',
            c => c,
        });
    }
    while out.ends_with(' ') {
        out.pop();
    }
    if truncated {
        out.push('…');
    }
    out
}

/// 文字単位で丸める (改行は保つ)。
fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// レビューコメントをエージェント向けの追いプロンプトへ組み立てる**純関数**。
///
/// - 解決済み / 本文が空のコメントは対象外。対象が 0 件なら空文字列を返す。
/// - ファイルは初出順 (= diff の並び)、ファイル内は行番号 → 側 → 追加順にそろえる。
/// - 各コメントは `@path:line` + 引用 1 行 + 本文。削除行は `(削除行)` を添える。
///
/// ```text
/// 以下のレビューコメントに対応してください:
///
/// @src/foo.rs:120
/// > 該当行の内容
/// コメント本文
/// ```
pub fn build_review_prompt(comments: &[DiffComment]) -> String {
    let mut targets: Vec<&DiffComment> = comments
        .iter()
        .filter(|c| !c.resolved && !c.body.trim().is_empty())
        .collect();
    if targets.is_empty() {
        return String::new();
    }

    // ファイルの初出順を控える (アルファベット順ではなく diff の並びを保つ)。
    let mut order: Vec<&str> = Vec::new();
    for c in targets.iter().copied() {
        let p = c.anchor.path.as_str();
        if !order.iter().any(|q| *q == p) {
            order.push(p);
        }
    }
    let idx = |p: &str| order.iter().position(|q| *q == p).unwrap_or(usize::MAX);
    targets.sort_by(|a, b| {
        idx(&a.anchor.path)
            .cmp(&idx(&b.anchor.path))
            .then(a.anchor.line.cmp(&b.anchor.line))
            .then(a.anchor.side.cmp(&b.anchor.side))
            .then(a.id.cmp(&b.id))
    });

    let mut out = String::from(REVIEW_PROMPT_HEADER);
    for c in targets {
        out.push_str("\n\n@");
        out.push_str(&c.anchor.path);
        out.push(':');
        out.push_str(&c.anchor.line.to_string());
        if c.anchor.side == CommentSide::Old {
            out.push_str(" (削除行)");
        }
        let quote = flatten_one_line(&c.quote, MAX_QUOTE_CHARS);
        if !quote.is_empty() {
            out.push_str("\n> ");
            out.push_str(&quote);
        }
        out.push('\n');
        out.push_str(&clamp_chars(c.body.trim(), MAX_BODY_CHARS));
    }
    out
}

/// インラインコメントの保管庫。
///
/// diff 本体 (`Vec<FileDiff>`) とは独立に持ち、鍵は `CommentAnchor` なので
/// 再パースやスクロールでコメントが別の行にずれることが無い。
#[derive(Clone, Debug, Default)]
pub struct DiffCommentStore {
    /// 追加順。表示順・プロンプト順の安定性はこの順序が土台。
    comments: Vec<DiffComment>,
    /// 次に払い出す id (削除しても再利用しない)。
    next_id: u64,
    /// 未確定の下書き (行アンカー → 本文)。
    drafts: HashMap<CommentAnchor, String>,
    /// 編集中の既存コメント (id → 編集中の本文)。
    editing: HashMap<u64, String>,
}

impl DiffCommentStore {
    /// コメントを追加し、払い出した id を返す。
    pub fn add(
        &mut self,
        anchor: CommentAnchor,
        quote: impl Into<String>,
        body: impl Into<String>,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.comments.push(DiffComment {
            id,
            anchor,
            quote: quote.into(),
            body: body.into(),
            resolved: false,
        });
        id
    }

    /// 本文を差し替える。存在しない id なら false。
    pub fn edit(&mut self, id: u64, body: impl Into<String>) -> bool {
        match self.comments.iter_mut().find(|c| c.id == id) {
            Some(c) => {
                c.body = body.into();
                true
            }
            None => false,
        }
    }

    /// 削除する。編集中の状態も一緒に片付ける。
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.comments.len();
        self.comments.retain(|c| c.id != id);
        self.editing.remove(&id);
        self.comments.len() != before
    }

    /// 解決状態を明示的に設定する。
    /// (UI はトグルしか使わないが、呼び出し側の配線用に公開しておく)
    #[allow(dead_code)]
    pub fn set_resolved(&mut self, id: u64, resolved: bool) -> bool {
        match self.comments.iter_mut().find(|c| c.id == id) {
            Some(c) => {
                c.resolved = resolved;
                true
            }
            None => false,
        }
    }

    /// 解決状態を反転する。存在しない id なら false。
    pub fn toggle_resolved(&mut self, id: u64) -> bool {
        match self.comments.iter_mut().find(|c| c.id == id) {
            Some(c) => {
                c.resolved = !c.resolved;
                true
            }
            None => false,
        }
    }

    /// 追加順のまま全件。
    #[allow(dead_code)]
    pub fn all(&self) -> &[DiffComment] {
        &self.comments
    }

    /// 指定行のコメント (追加順)。
    #[allow(dead_code)]
    pub fn at(&self, anchor: &CommentAnchor) -> Vec<&DiffComment> {
        self.comments.iter().filter(|c| &c.anchor == anchor).collect()
    }

    /// 指定行のコメント件数と「全部解決済みか」。0 件なら None。
    pub fn badge(&self, anchor: &CommentAnchor) -> Option<(usize, bool)> {
        let mut n = 0usize;
        let mut all_resolved = true;
        for c in self.comments.iter().filter(|c| &c.anchor == anchor) {
            n += 1;
            all_resolved &= c.resolved;
        }
        (n > 0).then_some((n, all_resolved))
    }

    /// その行に何か描くもの (コメント or 下書き) があるか。仮想化の判定に使う。
    pub fn has_ui_at(&self, anchor: &CommentAnchor) -> bool {
        self.drafts.contains_key(anchor) || self.comments.iter().any(|c| &c.anchor == anchor)
    }

    /// 下書きを開く/閉じるを反転する (行クリックの動作)。
    pub fn toggle_draft(&mut self, anchor: CommentAnchor) {
        if self.drafts.remove(&anchor).is_none() {
            self.drafts.insert(anchor, String::new());
        }
    }

    pub fn close_draft(&mut self, anchor: &CommentAnchor) {
        self.drafts.remove(anchor);
    }

    pub fn len(&self) -> usize {
        self.comments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.comments.is_empty()
    }

    /// 未解決かつ本文のあるもの = プロンプトに載る件数。
    pub fn actionable_len(&self) -> usize {
        self.comments
            .iter()
            .filter(|c| !c.resolved && !c.body.trim().is_empty())
            .count()
    }

    pub fn clear(&mut self) {
        self.comments.clear();
        self.drafts.clear();
        self.editing.clear();
    }

    /// 送信用プロンプト (`build_review_prompt` の薄い包み)。
    pub fn prompt(&self) -> String {
        build_review_prompt(&self.comments)
    }
}

// ---------------------------------------------------------------------------
// パース
// ---------------------------------------------------------------------------

/// `-a,b` / `+c,d` / `+c` (カウント省略 = 1) を (start, count) にパースする。
/// git.rs の `parse_range` と同じ規約。
fn parse_range(token: &str) -> Option<(usize, usize)> {
    let body = token
        .strip_prefix('+')
        .or_else(|| token.strip_prefix('-'))?;
    let mut parts = body.splitn(2, ',');
    let start: usize = parts.next()?.trim().parse().ok()?;
    let count: usize = match parts.next() {
        Some(cnt) => cnt.trim().parse().ok()?,
        None => 1,
    };
    Some((start, count))
}

/// `@@ -a,b +c,d @@ trailing` から ((old_start, old_count), (new_start, new_count)) を取り出す。
fn parse_hunk_header(line: &str) -> Option<((usize, usize), (usize, usize))> {
    if !line.starts_with("@@") {
        return None;
    }
    let mut tokens = line.split_whitespace();
    let _at = tokens.next()?; // "@@"
    let (old_tok, new_tok) = match (tokens.next(), tokens.next()) {
        (Some(o), Some(n)) if o.starts_with('-') && n.starts_with('+') => (o, n),
        _ => return None,
    };
    Some((parse_range(old_tok)?, parse_range(new_tok)?))
}

/// `--- a/foo` / `+++ b/foo` / `--- /dev/null` からパスを取り出す。
/// 末尾のタイムスタンプ (タブ区切り) は落とす。
fn strip_side_prefix(rest: &str) -> String {
    let rest = rest.split('\t').next().unwrap_or(rest).trim_end();
    let rest = unquote_git_path(rest);
    if rest == DEV_NULL {
        return DEV_NULL.to_string();
    }
    rest.strip_prefix("a/")
        .or_else(|| rest.strip_prefix("b/"))
        .unwrap_or(&rest)
        .to_string()
}

/// git がクォートしたパス (`"a/\346\227\245..."`) を復号する。
/// core.quotePath 既定では非 ASCII が 8 進エスケープになるため、
/// 復号しないと日本語ファイル名の見出しが `\346...` のまま表示される。
fn unquote_git_path(s: &str) -> String {
    if !(s.len() >= 2 && s.starts_with('"') && s.ends_with('"')) {
        return s.to_string();
    }
    let inner = &s[1..s.len() - 1];
    let mut out: Vec<u8> = Vec::with_capacity(inner.len());
    let mut it = inner.bytes().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match it.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b't') => out.push(b'\t'),
            Some(b'r') => out.push(b'\r'),
            Some(b'\\') => out.push(b'\\'),
            Some(b'"') => out.push(b'"'),
            Some(d @ b'0'..=b'7') => {
                // 最大 3 桁の 8 進エスケープ (バイト値)
                let mut v = u32::from(d - b'0');
                for _ in 0..2 {
                    match it.peek() {
                        Some(&n @ b'0'..=b'7') => {
                            v = v * 8 + u32::from(n - b'0');
                            it.next();
                        }
                        _ => break,
                    }
                }
                out.push(v as u8);
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `diff --git a/x b/y` の残り部分から (old, new) を取り出す。
/// スペースを含むパスに備え、まず ` b/` を境界として探す。
/// パス自体が ` b/` を含む場合 (`a b/c.rs` 等) は rfind だけだと誤分割
/// するため、「両側が同じパスになる分割」を優先して選ぶ (リネーム以外の
/// diff は old == new なのでこれで正しく直る)。
fn split_git_header(rest: &str) -> Option<(String, String)> {
    let candidates: Vec<usize> = rest
        .match_indices(" b/")
        .map(|(i, _)| i)
        .collect();
    // 両側が一致する分割があればそれが正解 (非リネームの通常ケース)
    for &pos in &candidates {
        let (a, b) = rest.split_at(pos);
        let (a, b) = (strip_side_prefix(a), strip_side_prefix(&b[1..]));
        if a == b {
            return Some((a, b));
        }
    }
    if let Some(&pos) = candidates.last() {
        let (a, b) = rest.split_at(pos);
        return Some((strip_side_prefix(a), strip_side_prefix(&b[1..])));
    }
    // フォールバック: 空白 2 分割。
    let (a, b) = rest.split_once(' ')?;
    Some((strip_side_prefix(a), strip_side_prefix(b)))
}

/// unified diff 全体を FileDiff の並びへ分解する。
///
/// 対応: 複数ファイル / `diff --git` ヘッダ / `--- +++` / `@@` (カウント省略含む) /
/// new file・deleted file mode / バイナリ / リネーム (ハンク無しも可) /
/// `\ No newline at end of file` (本文行として数えない)。
pub fn parse_unified(input: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut cur: Option<FileDiff> = None;
    // ハンク進行中の状態。
    let mut rem_old = 0usize;
    let mut rem_new = 0usize;
    let mut old_no = 0usize;
    let mut new_no = 0usize;
    let mut in_hunk = false;

    for line in input.lines() {
        // --- ハンク本体 (宣言された行数を消化しきるまでを最優先で処理) ---
        if in_hunk && (rem_old > 0 || rem_new > 0) {
            if line.starts_with('\\') {
                // "\ No newline at end of file" — 本文行ではない。
                continue;
            }
            let parsed = match line.as_bytes().first() {
                Some(b'+') => Some((LineKind::Added, &line[1..])),
                Some(b'-') => Some((LineKind::Removed, &line[1..])),
                Some(b' ') => Some((LineKind::Context, &line[1..])),
                // 空行は「空の文脈行」として出力されることがある。
                None => Some((LineKind::Context, "")),
                _ => None,
            };
            let Some((kind, body)) = parsed else {
                // ハンク内で想定外の行 → ハンク終了とみなして読み直す。
                in_hunk = false;
                rem_old = 0;
                rem_new = 0;
                process_file_level(line, &mut cur, &mut files);
                continue;
            };

            let file = cur.as_mut().expect("in_hunk implies a current file");
            let (o, n) = match kind {
                LineKind::Context => {
                    let (o, n) = (old_no, new_no);
                    old_no += 1;
                    new_no += 1;
                    rem_old = rem_old.saturating_sub(1);
                    rem_new = rem_new.saturating_sub(1);
                    (Some(o), Some(n))
                }
                LineKind::Added => {
                    let n = new_no;
                    new_no += 1;
                    rem_new = rem_new.saturating_sub(1);
                    file.additions += 1;
                    (None, Some(n))
                }
                LineKind::Removed => {
                    let o = old_no;
                    old_no += 1;
                    rem_old = rem_old.saturating_sub(1);
                    file.deletions += 1;
                    (Some(o), None)
                }
            };
            file.hunks
                .last_mut()
                .expect("in_hunk implies at least one hunk")
                .lines
                .push(DiffLine {
                    kind,
                    old_no: o,
                    new_no: n,
                    text: body.to_string(),
                });
            if rem_old == 0 && rem_new == 0 {
                in_hunk = false;
            }
            continue;
        }
        in_hunk = false;

        // --- ハンクヘッダ ---
        if let Some(((os, oc), (ns, nc))) = parse_hunk_header(line) {
            let file = cur.get_or_insert_with(FileDiff::new);
            file.hunks.push(Hunk {
                header: line.to_string(),
                old_start: os,
                new_start: ns,
                lines: Vec::new(),
            });
            rem_old = oc;
            rem_new = nc;
            old_no = os;
            new_no = ns;
            in_hunk = rem_old > 0 || rem_new > 0;
            continue;
        }

        // 宣言行数を消化しきった直後の "\ No newline" はここに落ちてくる。
        if line.starts_with('\\') {
            continue;
        }

        process_file_level(line, &mut cur, &mut files);
    }

    if let Some(f) = cur.take() {
        files.push(f);
    }
    files
}

/// ハンク外のメタ行を処理する。
fn process_file_level(line: &str, cur: &mut Option<FileDiff>, files: &mut Vec<FileDiff>) {
    if let Some(rest) = line.strip_prefix("diff --git ") {
        if let Some(f) = cur.take() {
            files.push(f);
        }
        let mut f = FileDiff::new();
        if let Some((a, b)) = split_git_header(rest) {
            f.old_path = a;
            f.new_path = b;
        }
        *cur = Some(f);
        return;
    }

    if let Some(rest) = line.strip_prefix("--- ") {
        // `diff --git` を伴わない素の unified diff では、ここが次ファイルの開始。
        if cur.as_ref().map(|f| !f.hunks.is_empty()).unwrap_or(false) {
            if let Some(f) = cur.take() {
                files.push(f);
            }
        }
        let f = cur.get_or_insert_with(FileDiff::new);
        f.old_path = strip_side_prefix(rest);
        return;
    }
    if let Some(rest) = line.strip_prefix("+++ ") {
        let f = cur.get_or_insert_with(FileDiff::new);
        f.new_path = strip_side_prefix(rest);
        return;
    }

    if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
        let f = cur.get_or_insert_with(FileDiff::new);
        f.is_binary = true;
        // "Binary files a/x and b/y differ" からパスを補う。
        if f.new_path.is_empty() {
            if let Some(body) = line
                .strip_prefix("Binary files ")
                .and_then(|b| b.strip_suffix(" differ"))
            {
                if let Some((a, b)) = body.split_once(" and ") {
                    f.old_path = strip_side_prefix(a);
                    f.new_path = strip_side_prefix(b);
                }
            }
        }
        return;
    }

    if let Some(rest) = line.strip_prefix("rename from ") {
        let f = cur.get_or_insert_with(FileDiff::new);
        f.is_rename = true;
        f.old_path = strip_side_prefix(rest);
        return;
    }
    if let Some(rest) = line.strip_prefix("rename to ") {
        let f = cur.get_or_insert_with(FileDiff::new);
        f.is_rename = true;
        f.new_path = strip_side_prefix(rest);
    }

    // new file mode / deleted file mode / index / similarity index / mode 変更などは
    // 追加情報を持たないので読み飛ばす。
}

// ---------------------------------------------------------------------------
// 描画
// ---------------------------------------------------------------------------

/// `a` と `b` を混ぜる。t=0 で a、t=1 で b。
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

/// テーマ由来の diff 配色。ハードコードせず bg と ok/err/accent を混ぜて作る。
struct DiffPalette {
    add_bg: Color32,
    del_bg: Color32,
    gutter_bg: Color32,
    hunk_bg: Color32,
    add_fg: Color32,
    del_fg: Color32,
}

impl DiffPalette {
    fn from_theme(t: &Theme) -> Self {
        // ライトテーマは地の明度が高く、同じ比率では色が沈むので濃いめに混ぜる。
        let tint = if t.dark { 0.18 } else { 0.26 };
        DiffPalette {
            add_bg: mix(t.bg, t.ok, tint),
            del_bg: mix(t.bg, t.err, tint),
            gutter_bg: mix(t.bg, t.panel, 0.7),
            hunk_bg: mix(t.bg, t.accent_soft, 0.9),
            // 記号 (+/-) は本文より強調するが、テーマ色を保つ。
            add_fg: mix(t.text, t.ok, if t.dark { 0.65 } else { 0.55 }),
            del_fg: mix(t.text, t.err, if t.dark { 0.65 } else { 0.55 }),
        }
    }
}

const GUTTER_COL_W: f32 = 34.0;
const SIGN_W: f32 = 12.0;
/// コメントマーカー列の幅 (常に確保して行のズレを防ぐ)。
const MARK_COL_W: f32 = 16.0;

/// diff ビューが呼び出し側へ返すアクション。
///
/// 既定は `None` なので、返り値を無視する既存の呼び出し元は今まで通り動く。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DiffAction {
    /// 何も起きていない。
    #[default]
    None,
    /// 「エージェントに送る」が押された。中身は組み立て済みプロンプト。
    SendToAgent(String),
}

/// `diff_ui` が生成したプロンプトの一時置き場 (型で衝突を防ぐための newtype)。
#[derive(Clone, Debug, Default)]
struct PendingReview(String);

fn pending_review_id() -> egui::Id {
    egui::Id::new("zv-diff-pending-review")
}

/// `diff_ui` 経由で組み立てられたレビュープロンプトを **1 回だけ**取り出す。
///
/// シグネチャを変えられない既存の呼び出し元 (`panels::pr_diff_ui` /
/// `race::race_diff_ui`) の配線ポイント。app.rs 側の毎フレーム処理で
/// `if let Some(p) = diff::take_pending_review_prompt(ctx) { ... }` と拾い、
/// `agent_input::AgentInputBuffer::append_prompt` などへ流せばよい。
// app.rs 側の配線待ち。配線されるまで未使用でも警告にしない。
#[allow(dead_code)]
pub fn take_pending_review_prompt(ctx: &egui::Context) -> Option<String> {
    ctx.data_mut(|d| d.remove_temp::<PendingReview>(pending_review_id()))
        .map(|p| p.0)
        .filter(|p| !p.is_empty())
}

/// diff をインライン表示する。スクロールは呼び出し側の責務。
///
/// コメントの状態は egui の一時データ (この `Ui` の id 由来) に持たせるので、
/// 呼び出し元がストアを抱えなくてもインラインコメントが使える。生成された
/// プロンプトは `take_pending_review_prompt` で受け取る。
pub fn diff_ui(ui: &mut egui::Ui, theme: &Theme, files: &[FileDiff]) {
    let store_id = ui.id().with("zv-diff-comments");
    let mut store: DiffCommentStore = ui.data_mut(|d| d.get_temp(store_id)).unwrap_or_default();
    let action = diff_ui_with_actions(ui, theme, files, &mut store);
    ui.data_mut(|d| d.insert_temp(store_id, store));
    if let DiffAction::SendToAgent(prompt) = action {
        ui.ctx()
            .data_mut(|d| d.insert_temp(pending_review_id(), PendingReview(prompt)));
    }
}

/// コメントストアを外から与える版。返り値で「エージェントに送る」を受け取れる。
pub fn diff_ui_with_actions(
    ui: &mut egui::Ui,
    theme: &Theme,
    files: &[FileDiff],
    comments: &mut DiffCommentStore,
) -> DiffAction {
    let pal = DiffPalette::from_theme(theme);
    let size = 12.5;

    if files.is_empty() {
        ui.label(
            RichText::new(tr("差分はありません"))
                .color(theme.text_dim)
                .size(size),
        );
        return DiffAction::None;
    }

    let action = review_toolbar_ui(ui, theme, comments, size);

    for (fi, file) in files.iter().enumerate() {
        let header = file_header_job(file, theme, size);
        let path = anchor_path(file);
        egui::CollapsingHeader::new(header)
            .id_salt(("zv-diff-file", fi))
            .default_open(true)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                if file.is_binary {
                    ui.label(
                        RichText::new(tr("バイナリファイル (差分表示なし)"))
                            .color(theme.text_dim)
                            .size(size),
                    );
                    return;
                }
                if file.hunks.is_empty() {
                    let msg = if file.is_rename {
                        tr("リネームのみ (内容の変更なし)")
                    } else {
                        tr("変更行なし")
                    };
                    ui.label(RichText::new(msg).color(theme.text_dim).size(size));
                    return;
                }
                // 仮想化: 画面外の行はウィジェットを組み立てず、同じ高さの
                // 空白だけ確保する。数千行の PR でも毎フレームのコストは
                // 可視行ぶんだけになる (行高は最初に描いた 1 行から実測)。
                // ただしコメント/下書きが付いた行は高さが違うので必ず実描画し、
                // 以降の行の座標が狂わないようにする。
                let clip = ui.clip_rect();
                let mut row_h: Option<f32> = None;
                for (hi, hunk) in file.hunks.iter().enumerate() {
                    hunk_header_ui(ui, theme, &pal, hunk, size);
                    for (li, line) in hunk.lines.iter().enumerate() {
                        let anchor = line_anchor(&path, line);
                        let expanded = anchor.as_ref().is_some_and(|a| comments.has_ui_at(a));
                        if let (Some(h), false) = (row_h, expanded) {
                            let top = ui.cursor().top();
                            if top + h < clip.top() || top > clip.bottom() {
                                ui.allocate_space(egui::vec2(ui.available_width(), h));
                                continue;
                            }
                        }
                        let badge = anchor.as_ref().and_then(|a| comments.badge(a));
                        let (h, resp) =
                            diff_line_ui(ui, theme, &pal, line, size, badge, (fi, hi, li));
                        // 高さの基準は「素の行」からだけ拾う。
                        if row_h.is_none() && !expanded {
                            row_h = Some(h);
                        }
                        if let Some(a) = anchor {
                            if resp.clicked() {
                                comments.toggle_draft(a.clone());
                            }
                            comment_thread_ui(ui, theme, comments, &a, line, size);
                        }
                    }
                }
            });
        ui.add_space(6.0);
    }
    action
}

/// コメントが 1 件でもあるときだけ出す操作バー。
fn review_toolbar_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    store: &mut DiffCommentStore,
    size: f32,
) -> DiffAction {
    if store.is_empty() {
        return DiffAction::None;
    }
    let mut action = DiffAction::None;
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(trf(
                "レビューコメント {n} 件 (未解決 {a} 件)",
                &[
                    ("n", store.len().to_string()),
                    ("a", store.actionable_len().to_string()),
                ],
            ))
            .color(theme.text_dim)
            .size(size),
        );
        let ready = store.actionable_len() > 0;
        if ui
            .add_enabled(ready, egui::Button::new(tr("エージェントに送る")))
            .on_hover_text(tr("未解決コメントをまとめて追いプロンプトにする"))
            .clicked()
        {
            let prompt = store.prompt();
            if !prompt.is_empty() {
                action = DiffAction::SendToAgent(prompt);
            }
        }
        if ui
            .add_enabled(ready, egui::Button::new(tr("コピー")))
            .clicked()
        {
            let prompt = store.prompt();
            ui.output_mut(|o| o.copied_text = prompt);
        }
        if ui.button(tr("すべて削除")).clicked() {
            store.clear();
        }
    });
    ui.add_space(4.0);
    action
}

/// ファイル見出し: パス + `+追加 -削除`。
fn file_header_job(file: &FileDiff, theme: &Theme, size: f32) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let mut job = LayoutJob::default();
    let fmt = |color: Color32| TextFormat {
        font_id: FontId::monospace(size),
        color,
        ..Default::default()
    };
    job.append(&file.display_path(), 0.0, fmt(theme.text));
    if file.is_rename {
        job.append("  [renamed]", 0.0, fmt(theme.text_dim));
    }
    if file.is_binary {
        job.append("  [binary]", 0.0, fmt(theme.text_dim));
    } else {
        job.append(&format!("  +{}", file.additions), 0.0, fmt(theme.ok));
        job.append(&format!(" -{}", file.deletions), 0.0, fmt(theme.err));
    }
    job
}

/// ハンク見出し (`@@ ... @@`) — アクセント色の帯で本文と区別する。
fn hunk_header_ui(ui: &mut egui::Ui, theme: &Theme, pal: &DiffPalette, hunk: &Hunk, size: f32) {
    let w = ui.available_width();
    egui::Frame::none()
        .fill(pal.hunk_bg)
        .inner_margin(egui::Margin::symmetric(4.0, 2.0))
        .show(ui, |ui| {
            ui.set_min_width(w);
            ui.add(
                egui::Label::new(
                    RichText::new(&hunk.header)
                        .monospace()
                        .size(size)
                        .color(theme.accent),
                )
                .wrap_mode(egui::TextWrapMode::Extend),
            );
        });
}

/// 1 行分: [旧行番号][新行番号][コメント印] +/- 本文。
///
/// 1 行を描き、(占めた高さ, クリック判定用の Response) を返す。
/// 高さは仮想化のプレースホルダ用、Response はコメント追加のトリガ。
#[allow(clippy::too_many_arguments)]
fn diff_line_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    pal: &DiffPalette,
    line: &DiffLine,
    size: f32,
    badge: Option<(usize, bool)>,
    row_key: (usize, usize, usize),
) -> (f32, egui::Response) {
    let (bg, sign_fg, sign) = match line.kind {
        LineKind::Added => (pal.add_bg, pal.add_fg, "+"),
        LineKind::Removed => (pal.del_bg, pal.del_fg, "-"),
        LineKind::Context => (theme.bg, theme.text_dim, " "),
    };
    let w = ui.available_width();
    let resp = egui::Frame::none().fill(bg).show(ui, |ui| {
        ui.set_min_width(w);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            gutter_cell(ui, theme, pal, line.old_no, size);
            gutter_cell(ui, theme, pal, line.new_no, size);
            marker_cell(ui, theme, badge, size);
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(
                    RichText::new(sign).monospace().size(size).color(sign_fg),
                )
                .wrap_mode(egui::TextWrapMode::Extend),
            );
            ui.add_space(SIGN_W - 6.0);
            ui.add(
                egui::Label::new(
                    RichText::new(&line.text)
                        .monospace()
                        .size(size)
                        .color(theme.text),
                )
                .wrap_mode(egui::TextWrapMode::Extend),
            );
        });
    });
    let rect = resp.response.rect;
    let hit = ui.interact(
        rect,
        ui.id().with(("zv-diff-row", row_key)),
        egui::Sense::click(),
    );
    if hit.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        // ホバー中の行は薄く持ち上げて、押せることを示す。
        ui.painter()
            .rect_filled(rect, 0.0, mix(Color32::TRANSPARENT, theme.accent, 0.10));
    }
    let hit = hit.on_hover_text(tr("クリックでコメントを追加/閉じる"));
    (rect.height(), hit)
}

/// 行の直下に出すコメントスレッド (既存コメント + 下書き)。
///
/// 状態の書き換えは一旦ローカル変数に溜めてからまとめて適用する。
/// 描画中に `store` を組み替えると、同フレームの列挙とインデックスが食い違う。
fn comment_thread_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    store: &mut DiffCommentStore,
    anchor: &CommentAnchor,
    line: &DiffLine,
    size: f32,
) {
    let ids: Vec<u64> = store
        .comments
        .iter()
        .filter(|c| &c.anchor == anchor)
        .map(|c| c.id)
        .collect();
    let has_draft = store.drafts.contains_key(anchor);
    if ids.is_empty() && !has_draft {
        return;
    }

    let w = ui.available_width();
    let mut remove: Option<u64> = None;
    let mut toggle: Option<u64> = None;
    let mut begin_edit: Option<u64> = None;
    let mut commit_edit: Option<u64> = None;
    let mut cancel_edit: Option<u64> = None;
    let mut submit_draft = false;
    let mut cancel_draft = false;

    egui::Frame::none()
        .fill(mix(theme.bg, theme.panel, 0.85))
        .inner_margin(egui::Margin {
            // 行番号 2 列 + コメント印の幅ぶん右へ寄せて、本文と桁を合わせる。
            left: GUTTER_COL_W * 2.0 + MARK_COL_W,
            right: 6.0,
            top: 4.0,
            bottom: 4.0,
        })
        .show(ui, |ui| {
            ui.set_min_width(w);
            ui.spacing_mut().item_spacing.y = 3.0;
            for id in ids {
                let Some((resolved, body)) = store
                    .comments
                    .iter()
                    .find(|c| c.id == id)
                    .map(|c| (c.resolved, c.body.clone()))
                else {
                    continue;
                };
                let editing = store.editing.contains_key(&id);
                ui.horizontal_wrapped(|ui| {
                    let (state, state_col) = if resolved {
                        (tr("解決済み"), theme.ok)
                    } else {
                        (tr("未解決"), theme.warn)
                    };
                    ui.label(RichText::new(state).size(size * 0.85).color(state_col));
                    let label = if resolved {
                        tr("未解決に戻す")
                    } else {
                        tr("解決")
                    };
                    if ui.small_button(label).clicked() {
                        toggle = Some(id);
                    }
                    if !editing && ui.small_button(tr("編集")).clicked() {
                        begin_edit = Some(id);
                    }
                    if ui.small_button(tr("削除")).clicked() {
                        remove = Some(id);
                    }
                });
                if editing {
                    if let Some(buf) = store.editing.get_mut(&id) {
                        ui.add(
                            egui::TextEdit::multiline(buf)
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                .font(FontId::monospace(size)),
                        );
                    }
                    ui.horizontal(|ui| {
                        if ui.small_button(tr("保存")).clicked() {
                            commit_edit = Some(id);
                        }
                        if ui.small_button(tr("取消")).clicked() {
                            cancel_edit = Some(id);
                        }
                    });
                } else {
                    let col = if resolved { theme.text_dim } else { theme.text };
                    ui.add(
                        egui::Label::new(RichText::new(body).size(size).color(col))
                            .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                }
            }

            if let Some(buf) = store.drafts.get_mut(anchor) {
                ui.add(
                    egui::TextEdit::multiline(buf)
                        .hint_text(tr("この行へのコメント"))
                        .desired_rows(2)
                        .desired_width(f32::INFINITY)
                        .font(FontId::monospace(size)),
                );
                let can_add = !buf.trim().is_empty();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(can_add, egui::Button::new(tr("追加")).small())
                        .clicked()
                    {
                        submit_draft = true;
                    }
                    if ui.small_button(tr("取消")).clicked() {
                        cancel_draft = true;
                    }
                });
            }
        });

    // --- 溜めた操作をまとめて適用 ---
    if let Some(id) = toggle {
        store.toggle_resolved(id);
    }
    if let Some(id) = begin_edit {
        let body = store
            .comments
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.body.clone())
            .unwrap_or_default();
        store.editing.insert(id, body);
    }
    if let Some(id) = commit_edit {
        if let Some(body) = store.editing.remove(&id) {
            store.edit(id, body);
        }
    }
    if let Some(id) = cancel_edit {
        store.editing.remove(&id);
    }
    if let Some(id) = remove {
        store.remove(id);
    }
    if submit_draft {
        if let Some(body) = store.drafts.remove(anchor) {
            store.add(anchor.clone(), line.text.clone(), body.trim());
        }
    }
    if cancel_draft {
        store.close_draft(anchor);
    }
}

/// コメント印の列。件数が無くても幅は確保して行のズレを防ぐ。
fn marker_cell(ui: &mut egui::Ui, theme: &Theme, badge: Option<(usize, bool)>, size: f32) {
    let (text, color) = match badge {
        // 全部解決済みなら ok 色、未解決が残っていれば accent。
        Some((n, all_resolved)) => (
            if n > 1 {
                format!("●{n}")
            } else {
                "●".to_string()
            },
            if all_resolved { theme.ok } else { theme.accent },
        ),
        None => (String::new(), theme.text_dim),
    };
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(
        egui::vec2(MARK_COL_W, h),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(text)
                        .monospace()
                        .size(size * 0.8)
                        .color(color),
                )
                .wrap_mode(egui::TextWrapMode::Extend),
            );
        },
    );
}

/// 行番号 1 列 (右寄せ)。番号が無い側は空欄。
fn gutter_cell(ui: &mut egui::Ui, theme: &Theme, pal: &DiffPalette, no: Option<usize>, size: f32) {
    let text = no.map(|n| n.to_string()).unwrap_or_default();
    let h = ui.spacing().interact_size.y;
    egui::Frame::none()
        .fill(pal.gutter_bg)
        .inner_margin(egui::Margin::symmetric(3.0, 0.0))
        .show(ui, |ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(GUTTER_COL_W, h),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(text)
                                .monospace()
                                .size(size * 0.9)
                                .color(theme.text_dim),
                        )
                        .wrap_mode(egui::TextWrapMode::Extend),
                    );
                },
            );
        });
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(h: &Hunk) -> Vec<LineKind> {
        h.lines.iter().map(|l| l.kind).collect()
    }

    fn nums(h: &Hunk) -> Vec<(Option<usize>, Option<usize>)> {
        h.lines.iter().map(|l| (l.old_no, l.new_no)).collect()
    }

    // ---- parse_range / parse_hunk_header ----

    #[test]
    fn range_with_count() {
        assert_eq!(parse_range("-10,3"), Some((10, 3)));
        assert_eq!(parse_range("+7,0"), Some((7, 0)));
    }

    #[test]
    fn range_without_count_defaults_to_one() {
        assert_eq!(parse_range("-5"), Some((5, 1)));
        assert_eq!(parse_range("+5"), Some((5, 1)));
    }

    #[test]
    fn range_rejects_garbage() {
        assert_eq!(parse_range("5,3"), None);
        assert_eq!(parse_range("-x,3"), None);
    }

    #[test]
    fn hunk_header_with_trailing_context() {
        let got = parse_hunk_header("@@ -1,4 +1,6 @@ fn main() {");
        assert_eq!(got, Some(((1, 4), (1, 6))));
    }

    #[test]
    fn hunk_header_omitted_counts() {
        assert_eq!(parse_hunk_header("@@ -3 +3 @@"), Some(((3, 1), (3, 1))));
    }

    #[test]
    fn hunk_header_rejects_non_hunk() {
        assert_eq!(parse_hunk_header("+++ b/foo.rs"), None);
        assert_eq!(parse_hunk_header("@@ broken @@"), None);
    }

    // ---- parse_unified ----

    #[test]
    fn empty_input_yields_no_files() {
        assert!(parse_unified("").is_empty());
        assert!(parse_unified("\n\n").is_empty());
    }

    #[test]
    fn simple_single_file() {
        let input = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 fn a() {}
-fn b() {}
+fn b2() {}
+fn c() {}
 fn d() {}
";
        let files = parse_unified(input);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.old_path, "src/lib.rs");
        assert_eq!(f.new_path, "src/lib.rs");
        assert_eq!(f.additions, 2);
        assert_eq!(f.deletions, 1);
        assert!(!f.is_binary && !f.is_rename);
        assert_eq!(f.hunks.len(), 1);
        let h = &f.hunks[0];
        assert_eq!(h.header, "@@ -1,3 +1,4 @@");
        assert_eq!((h.old_start, h.new_start), (1, 1));
        assert_eq!(
            kinds(h),
            vec![
                LineKind::Context,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Added,
                LineKind::Context
            ]
        );
        assert_eq!(
            nums(h),
            vec![
                (Some(1), Some(1)),
                (Some(2), None),
                (None, Some(2)),
                (None, Some(3)),
                (Some(3), Some(4)),
            ]
        );
        assert_eq!(h.lines[2].text, "fn b2() {}");
    }

    #[test]
    fn hunk_header_trailing_text_is_kept_not_parsed_as_line() {
        let input = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -10,2 +10,2 @@ impl Foo {
-    let x = 1;
+    let x = 2;
";
        let files = parse_unified(input);
        let h = &files[0].hunks[0];
        assert_eq!(h.header, "@@ -10,2 +10,2 @@ impl Foo {");
        assert_eq!((h.old_start, h.new_start), (10, 10));
        assert_eq!(h.lines.len(), 2);
        assert_eq!(h.lines[0].old_no, Some(10));
        assert_eq!(h.lines[1].new_no, Some(10));
    }

    #[test]
    fn omitted_counts_in_stream() {
        let input = "\
diff --git a/x b/x
--- a/x
+++ b/x
@@ -4 +4 @@
-old
+new
";
        let files = parse_unified(input);
        let h = &files[0].hunks[0];
        assert_eq!((h.old_start, h.new_start), (4, 4));
        assert_eq!(nums(h), vec![(Some(4), None), (None, Some(4))]);
        assert_eq!((files[0].additions, files[0].deletions), (1, 1));
    }

    #[test]
    fn multiple_hunks_track_line_numbers_independently() {
        let input = "\
diff --git a/m.rs b/m.rs
--- a/m.rs
+++ b/m.rs
@@ -1,2 +1,3 @@
 one
+two
 three
@@ -20,2 +21,2 @@
-alpha
+beta
 gamma
";
        let files = parse_unified(input);
        let f = &files[0];
        assert_eq!(f.hunks.len(), 2);
        assert_eq!(
            nums(&f.hunks[0]),
            vec![(Some(1), Some(1)), (None, Some(2)), (Some(2), Some(3))]
        );
        assert_eq!(
            nums(&f.hunks[1]),
            vec![(Some(20), None), (None, Some(21)), (Some(21), Some(22))]
        );
        assert_eq!((f.additions, f.deletions), (2, 1));
    }

    #[test]
    fn new_file_mode() {
        let input = "\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..3b18e51
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
";
        let files = parse_unified(input);
        let f = &files[0];
        assert_eq!(f.old_path, "/dev/null");
        assert_eq!(f.new_path, "new.txt");
        assert_eq!(f.display_path(), "new.txt");
        assert_eq!((f.additions, f.deletions), (2, 0));
        let h = &f.hunks[0];
        assert_eq!((h.old_start, h.new_start), (0, 1));
        assert_eq!(nums(h), vec![(None, Some(1)), (None, Some(2))]);
    }

    #[test]
    fn deleted_file_mode() {
        let input = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 3b18e51..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-hello
-world
";
        let files = parse_unified(input);
        let f = &files[0];
        assert_eq!(f.old_path, "gone.txt");
        assert_eq!(f.new_path, "/dev/null");
        assert_eq!(f.display_path(), "gone.txt");
        assert_eq!((f.additions, f.deletions), (0, 2));
        assert_eq!(nums(&f.hunks[0]), vec![(Some(1), None), (Some(2), None)]);
    }

    #[test]
    fn binary_file_is_flagged() {
        let input = "\
diff --git a/img.png b/img.png
index 1111111..2222222 100644
Binary files a/img.png and b/img.png differ
";
        let files = parse_unified(input);
        assert_eq!(files.len(), 1);
        assert!(files[0].is_binary);
        assert_eq!(files[0].new_path, "img.png");
        assert!(files[0].hunks.is_empty());
    }

    #[test]
    fn rename_without_hunks() {
        let input = "\
diff --git a/old/name.rs b/new/name.rs
similarity index 100%
rename from old/name.rs
rename to new/name.rs
";
        let files = parse_unified(input);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert!(f.is_rename);
        assert_eq!(f.old_path, "old/name.rs");
        assert_eq!(f.new_path, "new/name.rs");
        assert!(f.hunks.is_empty());
        assert_eq!(f.display_path(), "old/name.rs → new/name.rs");
    }

    #[test]
    fn rename_with_hunks() {
        let input = "\
diff --git a/a.rs b/b.rs
similarity index 88%
rename from a.rs
rename to b.rs
--- a/a.rs
+++ b/b.rs
@@ -1,2 +1,2 @@
 keep
-drop
+add
";
        let files = parse_unified(input);
        let f = &files[0];
        assert!(f.is_rename);
        assert_eq!((f.old_path.as_str(), f.new_path.as_str()), ("a.rs", "b.rs"));
        assert_eq!((f.additions, f.deletions), (1, 1));
        assert_eq!(f.hunks.len(), 1);
    }

    #[test]
    fn no_newline_marker_is_not_a_content_line() {
        let input = "\
diff --git a/n.txt b/n.txt
--- a/n.txt
+++ b/n.txt
@@ -1,2 +1,2 @@
 keep
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";
        let files = parse_unified(input);
        let f = &files[0];
        let h = &f.hunks[0];
        assert_eq!(h.lines.len(), 3);
        assert_eq!(
            kinds(h),
            vec![LineKind::Context, LineKind::Removed, LineKind::Added]
        );
        assert_eq!((f.additions, f.deletions), (1, 1));
        assert!(h.lines.iter().all(|l| !l.text.contains("No newline")));
    }

    #[test]
    fn trailing_no_newline_after_hunk_is_ignored() {
        let input = "\
diff --git a/t.txt b/t.txt
--- a/t.txt
+++ b/t.txt
@@ -1 +1 @@
-a
+b
\\ No newline at end of file
diff --git a/u.txt b/u.txt
--- a/u.txt
+++ b/u.txt
@@ -1 +1 @@
-c
+d
";
        let files = parse_unified(input);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].hunks[0].lines.len(), 2);
        assert_eq!(files[1].hunks[0].lines.len(), 2);
        assert_eq!(files[1].new_path, "u.txt");
    }

    #[test]
    fn empty_context_line_counts_as_context() {
        // git は空の文脈行を完全な空行として出すことがある。
        let input = "\
diff --git a/e.rs b/e.rs
--- a/e.rs
+++ b/e.rs
@@ -1,3 +1,3 @@
 fn a() {}

-old
+new
";
        let files = parse_unified(input);
        let h = &files[0].hunks[0];
        assert_eq!(h.lines.len(), 4);
        assert_eq!(h.lines[1].kind, LineKind::Context);
        assert_eq!(h.lines[1].text, "");
        assert_eq!((h.lines[1].old_no, h.lines[1].new_no), (Some(2), Some(2)));
    }

    #[test]
    fn plain_unified_without_git_header() {
        let input = "\
--- a/one.txt
+++ b/one.txt
@@ -1 +1 @@
-a
+b
--- a/two.txt
+++ b/two.txt
@@ -1 +1 @@
-c
+d
";
        let files = parse_unified(input);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path, "one.txt");
        assert_eq!(files[1].new_path, "two.txt");
    }

    #[test]
    fn side_paths_strip_timestamps() {
        assert_eq!(strip_side_prefix("a/src/x.rs\t2024-01-01 12:00"), "src/x.rs");
        assert_eq!(strip_side_prefix("/dev/null"), "/dev/null");
        assert_eq!(strip_side_prefix("b/plain.txt"), "plain.txt");
    }

    #[test]
    fn git_header_with_spaces_in_path() {
        let got = split_git_header("a/my dir/x.rs b/my dir/x.rs");
        assert_eq!(
            got,
            Some(("my dir/x.rs".to_string(), "my dir/x.rs".to_string()))
        );
    }

    #[test]
    fn removal_line_starting_with_dashes_inside_hunk() {
        // 本文が "--" で始まる削除行をファイルヘッダと誤認しないこと。
        let input = "\
diff --git a/s.sql b/s.sql
--- a/s.sql
+++ b/s.sql
@@ -1,2 +1,2 @@
---- old comment
+--- new comment
";
        let files = parse_unified(input);
        assert_eq!(files.len(), 1);
        let h = &files[0].hunks[0];
        assert_eq!(h.lines.len(), 2);
        assert_eq!(h.lines[0].kind, LineKind::Removed);
        assert_eq!(h.lines[0].text, "--- old comment");
        assert_eq!(h.lines[1].kind, LineKind::Added);
        assert_eq!(h.lines[1].text, "--- new comment");
    }

    #[test]
    fn realistic_multi_file_stream() {
        let input = "\
diff --git a/Cargo.toml b/Cargo.toml
index aaaaaaa..bbbbbbb 100644
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -8,6 +8,7 @@ edition = \"2021\"
 [dependencies]
 eframe = \"0.29\"
 egui = \"0.29\"
+serde = { version = \"1\", features = [\"derive\"] }
 anyhow = \"1\"
 dirs = \"5\"
 rfd = \"0.14\"
diff --git a/assets/logo.png b/assets/logo.png
index ccccccc..ddddddd 100644
Binary files a/assets/logo.png and b/assets/logo.png differ
diff --git a/src/old_name.rs b/src/new_name.rs
similarity index 97%
rename from src/old_name.rs
rename to src/new_name.rs
index eeeeeee..fffffff 100644
--- a/src/old_name.rs
+++ b/src/new_name.rs
@@ -1,5 +1,5 @@
-//! 旧モジュール
+//! 新モジュール

 pub fn run() {
     println!(\"hi\");
 }
diff --git a/src/dropped.rs b/src/dropped.rs
deleted file mode 100644
index 1234567..0000000
--- a/src/dropped.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-fn gone() {
-    // bye
-}
diff --git a/src/added.rs b/src/added.rs
new file mode 100644
index 0000000..7654321
--- /dev/null
+++ b/src/added.rs
@@ -0,0 +1,2 @@
+pub fn fresh() -> u32 { 42 }
+
";
        let files = parse_unified(input);
        assert_eq!(files.len(), 5);

        let toml = &files[0];
        assert_eq!(toml.new_path, "Cargo.toml");
        assert_eq!((toml.additions, toml.deletions), (1, 0));
        assert_eq!(toml.hunks[0].lines.len(), 7);
        assert_eq!(toml.hunks[0].header, "@@ -8,6 +8,7 @@ edition = \"2021\"");
        // 追加行は新側 11 行目 (8,9,10 が文脈)。
        let added = toml.hunks[0]
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Added)
            .unwrap();
        assert_eq!((added.old_no, added.new_no), (None, Some(11)));

        let png = &files[1];
        assert!(png.is_binary);
        assert_eq!(png.new_path, "assets/logo.png");

        let ren = &files[2];
        assert!(ren.is_rename);
        assert_eq!(ren.old_path, "src/old_name.rs");
        assert_eq!(ren.new_path, "src/new_name.rs");
        assert_eq!((ren.additions, ren.deletions), (1, 1));
        assert_eq!(ren.hunks[0].lines.len(), 6);

        let del = &files[3];
        assert_eq!(del.new_path, "/dev/null");
        assert_eq!((del.additions, del.deletions), (0, 3));

        let new = &files[4];
        assert_eq!(new.old_path, "/dev/null");
        assert_eq!((new.additions, new.deletions), (2, 0));
        assert_eq!(new.hunks[0].lines[1].text, "");
    }

    #[test]
    fn totals_across_stream() {
        let input = "\
diff --git a/a b/a
--- a/a
+++ b/a
@@ -1,2 +1,2 @@
-x
+y
diff --git a/b b/b
--- a/b
+++ b/b
@@ -1,1 +1,3 @@
 keep
+p
+q
";
        let files = parse_unified(input);
        let adds: usize = files.iter().map(|f| f.additions).sum();
        let dels: usize = files.iter().map(|f| f.deletions).sum();
        assert_eq!((adds, dels), (3, 1));
    }

    // ---- 描画ヘルパ ----

    #[test]
    fn mix_endpoints_and_midpoint() {
        let a = Color32::from_rgb(0, 0, 0);
        let b = Color32::from_rgb(100, 200, 50);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        assert_eq!(mix(a, b, 0.5), Color32::from_rgb(50, 100, 25));
        // 範囲外の t はクランプされる。
        assert_eq!(mix(a, b, 2.0), b);
        assert_eq!(mix(a, b, -1.0), a);
    }

    #[test]
    fn palette_readable_in_every_theme() {
        for t in crate::theme::all() {
            let p = DiffPalette::from_theme(&t);
            assert_ne!(p.add_bg, t.bg, "theme {} add_bg", t.name);
            assert_ne!(p.del_bg, t.bg, "theme {} del_bg", t.name);
            assert_ne!(p.add_bg, p.del_bg, "theme {} add/del", t.name);
            // 本文色 (theme.text) が着色背景に埋もれないこと。
            for bg in [p.add_bg, p.del_bg, p.hunk_bg, p.gutter_bg] {
                let d = |x: u8, y: u8| (x as i32 - y as i32).abs();
                let delta = d(t.text.r(), bg.r()) + d(t.text.g(), bg.g()) + d(t.text.b(), bg.b());
                assert!(delta > 120, "theme {} contrast too low: {}", t.name, delta);
            }
        }
    }

    #[test]
    fn split_git_header_handles_b_slash_inside_path() {
        // パスに " b/" を含むケース: 両側一致の分割を選ぶ
        assert_eq!(
            split_git_header("a/a b/c.rs b/a b/c.rs"),
            Some(("a b/c.rs".to_string(), "a b/c.rs".to_string()))
        );
        // 通常ケース
        assert_eq!(
            split_git_header("a/src/main.rs b/src/main.rs"),
            Some(("src/main.rs".to_string(), "src/main.rs".to_string()))
        );
        // リネーム (不一致) は従来どおり最後の " b/" で割る
        assert_eq!(
            split_git_header("a/old.rs b/new.rs"),
            Some(("old.rs".to_string(), "new.rs".to_string()))
        );
    }

    #[test]
    fn unquote_git_path_decodes_octal_and_escapes() {
        // 日本語ファイル名 ("日本.txt" の UTF-8 8進エスケープ)
        assert_eq!(
            unquote_git_path("\"a/\\346\\227\\245\\346\\234\\254.txt\""),
            "a/日本.txt"
        );
        // クォート無しはそのまま
        assert_eq!(unquote_git_path("a/plain.txt"), "a/plain.txt");
        // エスケープされた引用符とバックスラッシュ
        assert_eq!(unquote_git_path("\"a/q\\\"x\\\\y\""), "a/q\"x\\y");
        // strip_side_prefix 経由で a/ プレフィックスも落ちる
        assert_eq!(
            strip_side_prefix("\"a/\\346\\227\\245.txt\""),
            "日.txt"
        );
    }

    // -----------------------------------------------------------------
    // インラインレビューコメント
    // -----------------------------------------------------------------

    /// テスト用のコメント (id と解決状態は既定)。
    fn cmt(path: &str, line: usize, quote: &str, body: &str) -> DiffComment {
        DiffComment {
            id: 0,
            anchor: CommentAnchor::new(path, CommentSide::New, line),
            quote: quote.to_string(),
            body: body.to_string(),
            resolved: false,
        }
    }

    // ---- build_review_prompt ----

    #[test]
    fn review_prompt_empty_list_is_empty_string() {
        assert_eq!(build_review_prompt(&[]), "");
    }

    #[test]
    fn review_prompt_single_comment() {
        let got = build_review_prompt(&[cmt("src/foo.rs", 120, "    let x = 1;", "命名が雑")]);
        assert_eq!(
            got,
            "以下のレビューコメントに対応してください:\n\
             \n\
             @src/foo.rs:120\n\
             >     let x = 1;\n\
             命名が雑"
        );
    }

    #[test]
    fn review_prompt_sorts_lines_within_a_file() {
        // 入力はバラバラでも行番号順に並ぶ。
        let got = build_review_prompt(&[
            cmt("src/foo.rs", 180, "b", "後ろ"),
            cmt("src/foo.rs", 12, "a", "前"),
            cmt("src/foo.rs", 99, "c", "真ん中"),
        ]);
        let lines: Vec<&str> = got.lines().filter(|l| l.starts_with('@')).collect();
        assert_eq!(
            lines,
            vec!["@src/foo.rs:12", "@src/foo.rs:99", "@src/foo.rs:180"]
        );
    }

    #[test]
    fn review_prompt_groups_by_file_in_first_seen_order() {
        // zzz が先に出てくれば zzz が先 (アルファベット順にはしない)。
        let got = build_review_prompt(&[
            cmt("zzz.rs", 5, "z", "z へのコメント"),
            cmt("aaa.rs", 3, "a", "a へのコメント"),
            cmt("zzz.rs", 1, "z1", "z の先頭"),
        ]);
        let heads: Vec<&str> = got.lines().filter(|l| l.starts_with('@')).collect();
        assert_eq!(heads, vec!["@zzz.rs:1", "@zzz.rs:5", "@aaa.rs:3"]);
    }

    #[test]
    fn review_prompt_marks_old_side_and_skips_resolved_and_empty() {
        let mut removed = cmt("src/foo.rs", 42, "let old = 1;", "これは残すべき");
        removed.anchor.side = CommentSide::Old;
        let mut resolved = cmt("src/foo.rs", 43, "x", "解決済みなので出ない");
        resolved.resolved = true;
        let blank = cmt("src/foo.rs", 44, "y", "   ");
        let got = build_review_prompt(&[removed, resolved, blank]);
        assert!(got.contains("@src/foo.rs:42 (削除行)"), "{got}");
        assert!(!got.contains("解決済みなので出ない"), "{got}");
        assert!(!got.contains(":44"), "{got}");
    }

    #[test]
    fn review_prompt_all_resolved_is_empty_string() {
        let mut c = cmt("a.rs", 1, "x", "済み");
        c.resolved = true;
        assert_eq!(build_review_prompt(&[c]), "");
    }

    #[test]
    fn review_prompt_quote_keeps_backticks_and_flattens_newlines() {
        // バッククォートはコードフェンスを使わないのでそのまま通す。
        // 引用は必ず 1 行 (改行・タブは空白へ)。
        let got = build_review_prompt(&[cmt(
            "a.rs",
            1,
            "let s = `a`;\nlet t = 2;\tend",
            "本文",
        )]);
        assert!(got.contains("> let s = `a`; let t = 2; end"), "{got}");
        // 引用が 1 行であること = '>' で始まる行はちょうど 1 本。
        assert_eq!(got.lines().filter(|l| l.starts_with('>')).count(), 1);
    }

    #[test]
    fn review_prompt_truncates_long_quote_by_chars() {
        // 日本語でも UTF-8 境界を割らずに文字数で丸める。
        let long: String = "あ".repeat(MAX_QUOTE_CHARS + 50);
        let got = build_review_prompt(&[cmt("a.rs", 1, &long, "本文")]);
        let quote = got
            .lines()
            .find(|l| l.starts_with("> "))
            .expect("引用行がある");
        assert_eq!(quote.chars().count(), 2 + MAX_QUOTE_CHARS + 1); // "> " + 本体 + "…"
        assert!(quote.ends_with('…'));
    }

    #[test]
    fn review_prompt_truncates_long_body_by_chars() {
        let long: String = "長".repeat(MAX_BODY_CHARS + 10);
        let got = build_review_prompt(&[cmt("a.rs", 1, "q", &long)]);
        assert!(got.ends_with('…'), "末尾が省略記号ではない");
        // ヘッダ + パス行 + 引用行 + 本文(MAX+1文字) 程度に収まっている。
        assert!(got.chars().count() < MAX_BODY_CHARS + 200);
    }

    #[test]
    fn review_prompt_multiline_body_is_preserved() {
        let got = build_review_prompt(&[cmt("a.rs", 1, "q", "1行目\n2行目")]);
        assert!(got.ends_with("1行目\n2行目"), "{got}");
    }

    // ---- DiffCommentStore ----

    #[test]
    fn store_add_edit_delete_resolve() {
        let mut s = DiffCommentStore::default();
        let a = CommentAnchor::new("a.rs", CommentSide::New, 10);
        let id = s.add(a.clone(), "let x = 1;", "直して");
        assert_eq!(s.len(), 1);
        assert_eq!(s.actionable_len(), 1);

        assert!(s.edit(id, "やっぱりこう直して"));
        assert_eq!(s.all()[0].body, "やっぱりこう直して");
        assert!(!s.edit(id + 999, "存在しない"));

        assert!(s.toggle_resolved(id));
        assert!(s.all()[0].resolved);
        assert_eq!(s.actionable_len(), 0, "解決済みは送信対象から外れる");
        assert!(s.set_resolved(id, false));
        assert_eq!(s.actionable_len(), 1);

        assert!(s.remove(id));
        assert!(!s.remove(id), "二度目の削除は false");
        assert!(s.is_empty());
    }

    #[test]
    fn store_keeps_insertion_order_and_never_reuses_ids() {
        let mut s = DiffCommentStore::default();
        let a1 = CommentAnchor::new("a.rs", CommentSide::New, 1);
        let a2 = CommentAnchor::new("a.rs", CommentSide::New, 2);
        let i1 = s.add(a1.clone(), "q1", "1つ目");
        let i2 = s.add(a2.clone(), "q2", "2つ目");
        s.remove(i1);
        let i3 = s.add(a1.clone(), "q1", "3つ目");
        assert!(i3 > i2 && i2 > i1, "id は単調増加 ({i1},{i2},{i3})");
        let bodies: Vec<&str> = s.all().iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, vec!["2つ目", "3つ目"], "追加順が保たれる");
    }

    #[test]
    fn store_badge_counts_per_anchor() {
        let mut s = DiffCommentStore::default();
        let a = CommentAnchor::new("a.rs", CommentSide::New, 7);
        let other = CommentAnchor::new("a.rs", CommentSide::Old, 7);
        assert_eq!(s.badge(&a), None);
        let id = s.add(a.clone(), "q", "x");
        s.add(a.clone(), "q", "y");
        assert_eq!(s.badge(&a), Some((2, false)));
        assert_eq!(s.badge(&other), None, "側が違えば別の行");
        s.set_resolved(id, true);
        assert_eq!(s.badge(&a), Some((2, false)), "1件でも未解決なら未解決扱い");
        for c in s.all().to_vec() {
            s.set_resolved(c.id, true);
        }
        assert_eq!(s.badge(&a), Some((2, true)));
        assert_eq!(s.at(&a).len(), 2);
    }

    #[test]
    fn store_draft_toggles_and_marks_row_as_expanded() {
        let mut s = DiffCommentStore::default();
        let a = CommentAnchor::new("a.rs", CommentSide::New, 3);
        assert!(!s.has_ui_at(&a));
        s.toggle_draft(a.clone());
        assert!(s.has_ui_at(&a), "下書き中の行は仮想化から外す");
        s.toggle_draft(a.clone());
        assert!(!s.has_ui_at(&a));
        s.add(a.clone(), "q", "本文");
        assert!(s.has_ui_at(&a), "コメント付きの行も仮想化から外す");
        s.clear();
        assert!(!s.has_ui_at(&a));
    }

    // ---- クリック位置 → アンカー ----

    #[test]
    fn line_target_picks_new_side_for_added_and_context() {
        let src = "\
diff --git a/src/foo.rs b/src/foo.rs
--- a/src/foo.rs
+++ b/src/foo.rs
@@ -10,2 +20,3 @@ fn f() {
 ctx1
-removed
+added1
+added2
";
        let files = parse_unified(src);
        assert_eq!(files.len(), 1);
        let path = anchor_path(&files[0]);
        assert_eq!(path, "src/foo.rs");
        let got: Vec<(CommentSide, usize)> = files[0].hunks[0]
            .lines
            .iter()
            .map(|l| line_target(l).expect("行番号がある"))
            .collect();
        assert_eq!(
            got,
            vec![
                (CommentSide::New, 20), // 文脈行 → 新側
                (CommentSide::Old, 11), // 削除行 → 旧側 (10 は ctx1)
                (CommentSide::New, 21), // 追加行 → 新側
                (CommentSide::New, 22),
            ]
        );
        // アンカーはパス付きで組み立たる
        let a = line_anchor(&path, &files[0].hunks[0].lines[2]).unwrap();
        assert_eq!(a, CommentAnchor::new("src/foo.rs", CommentSide::New, 21));
    }

    #[test]
    fn line_target_handles_multiple_hunks_and_pure_add_delete() {
        let src = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1 +1,2 @@
 keep
+new
@@ -100,2 +101,1 @@
-gone
 tail
";
        let files = parse_unified(src);
        let f = &files[0];
        assert_eq!(f.hunks.len(), 2);
        let h0: Vec<_> = f.hunks[0].lines.iter().filter_map(line_target).collect();
        assert_eq!(h0, vec![(CommentSide::New, 1), (CommentSide::New, 2)]);
        let h1: Vec<_> = f.hunks[1].lines.iter().filter_map(line_target).collect();
        assert_eq!(
            h1,
            vec![(CommentSide::Old, 100), (CommentSide::New, 101)],
            "2つ目のハンクでもヘッダの開始行が効く"
        );
    }

    #[test]
    fn anchor_path_prefers_new_path_even_on_rename() {
        let src = "\
diff --git a/old.rs b/new.rs
similarity index 90%
rename from old.rs
rename to new.rs
--- a/old.rs
+++ b/new.rs
@@ -1 +1 @@
-a
+b
";
        let files = parse_unified(src);
        assert!(files[0].is_rename);
        // 表示は "old.rs → new.rs" でも、エージェントに渡すのは新パスだけ。
        assert!(files[0].display_path().contains('→'));
        assert_eq!(anchor_path(&files[0]), "new.rs");
    }

    #[test]
    fn anchor_path_falls_back_to_old_path_on_delete() {
        let src = "\
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
--- a/gone.rs
+++ /dev/null
@@ -1 +0,0 @@
-bye
";
        let files = parse_unified(src);
        assert_eq!(anchor_path(&files[0]), "gone.rs");
    }

    // ---- 再パース耐性 ----

    #[test]
    fn comments_survive_a_reparse_of_the_same_diff() {
        let src = "\
diff --git a/src/foo.rs b/src/foo.rs
--- a/src/foo.rs
+++ b/src/foo.rs
@@ -10,2 +20,3 @@
 ctx
-old
+new1
+new2
";
        let files = parse_unified(src);
        let path = anchor_path(&files[0]);
        let mut store = DiffCommentStore::default();
        // 3 行目 (追加行 new1) と削除行にコメントを付ける。
        let add_line = &files[0].hunks[0].lines[2];
        let del_line = &files[0].hunks[0].lines[1];
        store.add(
            line_anchor(&path, add_line).unwrap(),
            add_line.text.clone(),
            "追加行へのコメント",
        );
        store.add(
            line_anchor(&path, del_line).unwrap(),
            del_line.text.clone(),
            "削除行へのコメント",
        );

        // まったく同じ diff を再パース (タブ切り替え等で毎フレーム起こりうる)。
        let again = parse_unified(src);
        let path2 = anchor_path(&again[0]);
        let hits: Vec<(usize, &str)> = again[0].hunks[0]
            .lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| {
                let a = line_anchor(&path2, l)?;
                let c = store.at(&a).first().copied()?;
                Some((i, c.body.as_str()))
            })
            .collect();
        assert_eq!(
            hits,
            vec![(1, "削除行へのコメント"), (2, "追加行へのコメント")],
            "再パース後も同じ行に付いている (行インデックスではなく行番号が鍵)"
        );

        // ハンクの前に行が増えても、その行の行番号が変わらない限り追随する。
        let prompt = store.prompt();
        assert!(prompt.contains("@src/foo.rs:21"), "{prompt}");
        assert!(prompt.contains("@src/foo.rs:11 (削除行)"), "{prompt}");
    }

    #[test]
    fn store_prompt_matches_build_review_prompt() {
        let mut s = DiffCommentStore::default();
        s.add(
            CommentAnchor::new("a.rs", CommentSide::New, 1),
            "q",
            "body",
        );
        assert_eq!(s.prompt(), build_review_prompt(s.all()));
        assert!(!s.prompt().is_empty());
    }

    #[test]
    fn diff_action_defaults_to_none() {
        assert_eq!(DiffAction::default(), DiffAction::None);
    }
}
