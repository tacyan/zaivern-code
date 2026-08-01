//! unified diff のパースと、VS Code 相当の diff ビュー。
//!
//! `git diff` / `gh pr diff` が吐く unified 形式をそのまま受け取り、
//! ファイル単位 → ハンク単位 → 行単位に分解して描画する。
//!
//! 表示は **一列 (インライン) / 並列 (左右 2 列)** の 2 モード
//! ([`DiffMode`]) で、幅が足りなければ自動で一列へ縮退する。
//! 並列でも左右のセルは**同じ 1 本の行矩形**の中に置くので、対応する行の
//! 高さは構造的に必ず揃い、スクロールの同期処理そのものが要らない
//! (= 毎フレームの再描画要求もゼロ)。
//!
//! **判断は全部 GUI に依存しない純関数へ切り出してある**:
//!
//! | 何を決めるか | 関数 |
//! |---|---|
//! | 幅 → 桁割り / 縮退 | [`diff_layout`] |
//! | 左右の行の対応付け | [`align_hunk`] |
//! | 語単位ハイライト | [`word_diff`] |
//! | 未変更行の折りたたみ | [`fold_context_runs`] |
//! | 変更箇所のジャンプ先 | [`change_blocks`] / [`next_change_index`] |
//!
//! パース部 (`parse_unified`) も純関数。ハンクヘッダの解釈は
//! `git::parse_range` / `git::parse_hunk_marks` と同じ流儀
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

// ===========================================================================
// 表示の決定層 — **GUI に一切依存しない純関数**
//
// 並列表示の行揃え・幅による縮退・語単位ハイライト・文脈行の折りたたみ・
// 変更箇所のジャンプ先は、すべてここで決めてテーブルテストで固定する。
// 描画側 (下の「描画」節) はここが返した値をそのまま絵にするだけ。
// ===========================================================================

/// 差分の表示モード。**独立した bool を並べない** — 2 つのビューが同時に
/// 描かれる事故を型で潰すため、必ずこの 1 つの列挙型で持つ。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiffMode {
    /// 1 列に `+` / `-` を混ぜる表示。狭い幅ではこちらへ自動縮退する。
    Inline,
    /// 左 = 変更前 / 右 = 変更後 の 2 列 (VS Code の既定)。
    #[default]
    SideBySide,
}

impl DiffMode {
    /// config の文字列から。未知の値は既定 (並列)。
    pub fn from_config_str(s: &str) -> DiffMode {
        match s.trim().to_ascii_lowercase().as_str() {
            "inline" | "unified" | "1" => DiffMode::Inline,
            _ => DiffMode::SideBySide,
        }
    }

    /// config へ書く文字列。
    pub fn config_str(self) -> &'static str {
        match self {
            DiffMode::Inline => "inline",
            DiffMode::SideBySide => "side_by_side",
        }
    }

    pub fn toggled(self) -> DiffMode {
        match self {
            DiffMode::Inline => DiffMode::SideBySide,
            DiffMode::SideBySide => DiffMode::Inline,
        }
    }

    /// UI に出す名前。
    pub fn label(self) -> String {
        match self {
            DiffMode::Inline => tr("一列 (インライン)"),
            DiffMode::SideBySide => tr("並列 (左右)"),
        }
    }
}

/// 行番号 1 列の幅。
const GUTTER_COL_W: f32 = 34.0;
/// `+` / `-` の記号列の幅。
const SIGN_W: f32 = 12.0;
/// コメントマーカー列の幅 (常に確保して行のズレを防ぐ)。
const MARK_COL_W: f32 = 16.0;
/// 左右ペインの間の溝。
const PANE_GAP: f32 = 8.0;

/// 本文が読める最小幅。
///
/// 差分の本文は等幅 12.5px で描くので 1 桁およそ 7.5px。240px ≒ 32 桁で、
/// これは「インデント 1 段 + 識別子 + 演算子」がぎりぎり読める最小単位。
/// これを下回ると左右に並べても両側とも読めず、横スクロールも無い
/// (行は `available_width()` に収める規約) ため、並べる意味が無くなる。
const MIN_CODE_W: f32 = 240.0;

/// 1 ペインが要求する最小幅 = 行番号 + コメント印 + 記号 + 本文。
const PANE_MIN_W: f32 = GUTTER_COL_W + MARK_COL_W + SIGN_W + MIN_CODE_W;

/// これ未満の可用幅では並列をやめてインラインへ**自動縮退**する。
///
/// 2 ペイン + 溝 = 34+16+12+240 の 2 倍 + 8 = 612px。
/// VS Code の `diffEditor.renderSideBySideInlineBreakpoint` は既定 900px だが、
/// あちらは各ペインにミニマップと独立したスクロールバーを抱えている。
/// こちらはどちらも持たないぶん、同じ読みやすさをより狭い幅で出せる。
pub const SIDE_BY_SIDE_MIN_W: f32 = PANE_MIN_W * 2.0 + PANE_GAP;

/// 1 ペインの桁割り。x は差分ビューの左端を 0 とした相対座標。
///
/// **不変条件**: `x >= 0`、`x + width <= 可用幅`、
/// `gutter_w + mark_w + sign_w + text_w == width`、すべて非負。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneCols {
    pub x: f32,
    pub width: f32,
    /// 行番号列の合計幅 (`cols` 本ぶん)。
    pub gutter_w: f32,
    /// 行番号列の本数。インライン = 2 (旧/新)、並列 = 1。
    pub cols: u8,
    pub mark_w: f32,
    pub sign_w: f32,
    /// 本文の左端 (絶対ではなく差分ビュー左端からの相対)。
    pub text_x: f32,
    pub text_w: f32,
}

/// 幅とモードから決まる差分ビューのレイアウト。
#[derive(Clone, Debug, PartialEq)]
pub struct DiffLayout {
    /// ユーザーが選んでいるモード。
    pub requested: DiffMode,
    /// 実際に描くモード (幅が足りなければ縮退後)。
    pub mode: DiffMode,
    /// 使える横幅 (スナップ済み)。
    pub width: f32,
    /// 左右ペインの溝 (インラインでは 0)。
    pub gap: f32,
    /// インライン = 1 枚、並列 = 2 枚 (左, 右)。
    pub panes: Vec<PaneCols>,
}

impl DiffLayout {
    /// 幅が足りなくてインラインへ落とされたか。
    pub fn degraded(&self) -> bool {
        self.requested != self.mode
    }
}

/// 残り幅から `want` を切り出す。足りなければ残り全部 (負にはしない)。
fn cut(rem: &mut f32, want: f32, ppp: f32) -> f32 {
    let w = crate::theme::snap_len(want.max(0.0), ppp).min(*rem).max(0.0);
    *rem -= w;
    w
}

/// 1 ペインの桁割りを作る。**幅が足りないときは右の列から順に潰れる**ので、
/// どんなに狭くても合計がペイン幅を超えない。
fn pane_cols(x: f32, width: f32, cols: u8, ppp: f32) -> PaneCols {
    let width = width.max(0.0);
    let mut rem = width;
    let gutter_w = cut(&mut rem, GUTTER_COL_W * cols as f32, ppp);
    let mark_w = cut(&mut rem, MARK_COL_W, ppp);
    let sign_w = cut(&mut rem, SIGN_W, ppp);
    PaneCols {
        x,
        width,
        gutter_w,
        cols,
        mark_w,
        sign_w,
        text_x: x + gutter_w + mark_w + sign_w,
        // 端数はすべて本文が吸う = 合計はぴったり width。
        text_w: rem,
    }
}

/// 可用幅と希望モードから、実際に描くレイアウトを決める**純関数**。
///
/// `ppp` (pixels per point) を受けるのは、桁の境界を物理ピクセルへ揃える
/// ため (`theme::snap_len`)。小数のまま置くと epaint が文字位置だけを丸めて
/// 桁間隔が揺れる — 端末セルと同じ罠。
pub fn diff_layout(available_w: f32, mode: DiffMode, ppp: f32) -> DiffLayout {
    let ppp = if ppp.is_finite() && ppp > 0.0 { ppp } else { 1.0 };
    let w = if available_w.is_finite() {
        crate::theme::snap_len(available_w.max(0.0), ppp)
    } else {
        0.0
    };
    let effective = if mode == DiffMode::SideBySide && w >= SIDE_BY_SIDE_MIN_W {
        DiffMode::SideBySide
    } else {
        DiffMode::Inline
    };
    let (gap, panes) = match effective {
        DiffMode::Inline => (0.0, vec![pane_cols(0.0, w, 2, ppp)]),
        DiffMode::SideBySide => {
            let gap = crate::theme::snap_len(PANE_GAP, ppp);
            let left_w = crate::theme::snap_len(((w - gap) * 0.5).max(0.0), ppp);
            let right_x = (left_w + gap).min(w);
            let right_w = (w - right_x).max(0.0);
            (
                gap,
                vec![
                    pane_cols(0.0, left_w, 1, ppp),
                    pane_cols(right_x, right_w, 1, ppp),
                ],
            )
        }
    };
    DiffLayout {
        requested: mode,
        mode: effective,
        width: w,
        gap,
        panes,
    }
}

// ---------------------------------------------------------------------------
// 並列表示の行揃え
// ---------------------------------------------------------------------------

/// ハンク内の 1 行を指す。`idx` は [`Hunk::lines`] の添字。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineRef {
    pub idx: usize,
}

impl LineRef {
    fn at(idx: usize) -> LineRef {
        LineRef { idx }
    }
}

/// ハンクを「左 (変更前) / 右 (変更後)」の行の対に畳む**純関数**。
///
/// * 文脈行は左右に同じ行が並ぶ。
/// * 連続する削除と、それに続く連続する追加は**置換の組**として同じ高さに並べる。
/// * 数が合わない側は `None` (= 反対側に空のプレースホルダ行を置く)。
///
/// 追加が先に来て削除が後に来る (順序が逆の) ハンクでも、塊の切れ目で
/// いったん確定させるので行が入れ替わらない。
pub fn align_hunk(hunk: &Hunk) -> Vec<(Option<LineRef>, Option<LineRef>)> {
    let mut out: Vec<(Option<LineRef>, Option<LineRef>)> = Vec::with_capacity(hunk.lines.len());
    let mut dels: Vec<usize> = Vec::new();
    let mut adds: Vec<usize> = Vec::new();

    fn flush(
        dels: &mut Vec<usize>,
        adds: &mut Vec<usize>,
        out: &mut Vec<(Option<LineRef>, Option<LineRef>)>,
    ) {
        let n = dels.len().max(adds.len());
        for i in 0..n {
            out.push((
                dels.get(i).copied().map(LineRef::at),
                adds.get(i).copied().map(LineRef::at),
            ));
        }
        dels.clear();
        adds.clear();
    }

    for (i, line) in hunk.lines.iter().enumerate() {
        match line.kind {
            LineKind::Removed => {
                // 追加のあとに削除が来たら、そこで塊が切れている。
                if !adds.is_empty() {
                    flush(&mut dels, &mut adds, &mut out);
                }
                dels.push(i);
            }
            LineKind::Added => adds.push(i),
            LineKind::Context => {
                flush(&mut dels, &mut adds, &mut out);
                out.push((Some(LineRef::at(i)), Some(LineRef::at(i))));
            }
        }
    }
    flush(&mut dels, &mut adds, &mut out);
    out
}

// ---------------------------------------------------------------------------
// 語単位 (書記素クラスタ単位) のハイライト
// ---------------------------------------------------------------------------

/// 行内の「変わった部分」のバイト範囲 (半開区間)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WordSpan {
    pub start: usize,
    pub end: usize,
}

/// 語単位判定を諦める行長 (バイト)。これを超える行は行全体を塗る。
const WORD_DIFF_MAX_BYTES: usize = 8_192;
/// 語単位判定を諦める DP のマス数。`|old| * |new|` がこれを超えたら諦める。
/// 500x500 = 25 万マス。1 行の比較で数 ms を超えさせない上限。
const WORD_DIFF_MAX_CELLS: usize = 250_000;
/// トークン 1 個どうしを文字単位で精査するときの上限 (128x128)。
const WORD_DIFF_REFINE_MAX_CELLS: usize = 16_384;
/// 精査した結果、語のこの割合以上が変わっていたら語ごと塗る。
/// 点在する塗りは「どこが変わったか」を却って分かりにくくする。
const REFINE_COVER_RATIO: f32 = 0.6;

/// ASCII の識別子を作る文字か (ここだけ 1 トークンにまとめる)。
///
/// **ASCII に限る**のが要点。日本語や絵文字までまとめると 1 行が 1 トークンに
/// なり、語単位ハイライトが行全体の塗りに退化する (CJK では実際にそうなる)。
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// 行をトークン列 (バイト範囲) に割る。
///
/// ASCII の識別子は 1 トークン、それ以外は**書記素クラスタ 1 個**が 1 トークン。
/// クラスタ単位なので、結合濁点・異体字セレクタ・ZWJ 絵文字・国旗を割らない
/// (= 4 バイト文字の途中で範囲が切れることが原理的に起きない)。
fn tokenize(s: &str) -> Vec<(usize, usize)> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < s.len() {
        if is_word_byte(bytes[i]) {
            let start = i;
            while i < s.len() && is_word_byte(bytes[i]) {
                i += 1;
            }
            out.push((start, i));
        } else {
            let end = crate::textenc::grapheme_end(s, i);
            // grapheme_end は必ず前に進むが、万一に備えて止まらない保険。
            let end = if end > i { end } else { i + 1 };
            out.push((i, end.min(s.len())));
            i = end;
        }
    }
    out
}

/// 連続するトークン範囲を 1 つの [`WordSpan`] にまとめる。
fn merge_spans(ranges: &[(usize, usize)]) -> Vec<WordSpan> {
    let mut out: Vec<WordSpan> = Vec::new();
    for &(s, e) in ranges {
        match out.last_mut() {
            Some(last) if last.end == s => last.end = e,
            _ => out.push(WordSpan { start: s, end: e }),
        }
    }
    out
}

/// 書記素クラスタ 1 個ずつのバイト範囲。
fn cluster_ranges(s: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < s.len() {
        let end = crate::textenc::grapheme_end(s, i);
        let end = if end > i { end } else { i + 1 };
        let end = end.min(s.len());
        out.push((i, end));
        i = end;
    }
    out
}

/// 先頭・末尾で一致するトークン数 `(接頭辞, 接尾辞)`。区間は重ならない。
fn common_affix(av: &[&str], bv: &[&str]) -> (usize, usize) {
    let mut p = 0usize;
    while p < av.len() && p < bv.len() && av[p] == bv[p] {
        p += 1;
    }
    let mut s = 0usize;
    while s < av.len() - p && s < bv.len() - p && av[av.len() - 1 - s] == bv[bv.len() - 1 - s] {
        s += 1;
    }
    (p, s)
}

/// トークン列の LCS を取り、「同じ位置で置き換わった塊」ごとに
/// `(旧側のトークン, 新側のトークン)` を返す。
///
/// マス数が `max_cells` を超えるときは `None` (呼び出し側が諦める)。
/// 計算量・記憶量ともに `|a| * |b|` なので、上限はここで必ず効かせる。
#[allow(clippy::type_complexity)]
fn lcs_groups(
    a: &[(usize, usize)],
    av: &[&str],
    b: &[(usize, usize)],
    bv: &[&str],
    max_cells: usize,
) -> Option<Vec<(Vec<(usize, usize)>, Vec<(usize, usize)>)>> {
    let (n, m) = (av.len(), bv.len());
    if n.saturating_mul(m) > max_cells {
        return None;
    }
    // dp[i][j] = av[i..] と bv[j..] の LCS 長
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[at(i, j)] = if av[i] == bv[j] {
                dp[at(i + 1, j + 1)] + 1
            } else {
                dp[at(i + 1, j)].max(dp[at(i, j + 1)])
            };
        }
    }
    let mut groups: Vec<(Vec<(usize, usize)>, Vec<(usize, usize)>)> = Vec::new();
    let mut cur: (Vec<(usize, usize)>, Vec<(usize, usize)>) = (Vec::new(), Vec::new());
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if av[i] == bv[j] {
            if !cur.0.is_empty() || !cur.1.is_empty() {
                groups.push(std::mem::take(&mut cur));
            }
            i += 1;
            j += 1;
        } else if dp[at(i + 1, j)] >= dp[at(i, j + 1)] {
            cur.0.push(a[i]);
            i += 1;
        } else {
            cur.1.push(b[j]);
            j += 1;
        }
    }
    while i < n {
        cur.0.push(a[i]);
        i += 1;
    }
    while j < m {
        cur.1.push(b[j]);
        j += 1;
    }
    if !cur.0.is_empty() || !cur.1.is_empty() {
        groups.push(cur);
    }
    Some(groups)
}

/// トークン 1 個どうしの置換を、書記素クラスタ単位でさらに細かく突き合わせる。
///
/// `count` → `counter` を「語まるごと」ではなく「増えた `er` だけ」の塗りにする
/// (VS Code と同じ粒度)。ただし**塗りが点在すると却って読みにくい**ので、
/// 変更が語の大半 ([`REFINE_COVER_RATIO`] 以上) に及ぶときや、塗りが 3 つ以上に
/// 割れるときは語ごと塗りへ戻す。
fn refine_pair(
    old: &str,
    a: (usize, usize),
    new: &str,
    b: (usize, usize),
) -> (Vec<(usize, usize)>, Vec<(usize, usize)>) {
    let whole = || (vec![a], vec![b]);
    let (os, ns) = (&old[a.0..a.1], &new[b.0..b.1]);
    let ar = cluster_ranges(os);
    let br = cluster_ranges(ns);
    let av: Vec<&str> = ar.iter().map(|&(x, y)| &os[x..y]).collect();
    let bv: Vec<&str> = br.iter().map(|&(x, y)| &ns[x..y]).collect();
    let (p, s) = common_affix(&av, &bv);
    if p == 0 && s == 0 {
        // 1 文字も共有しないなら刻む意味が無い。
        return whole();
    }
    let (am, bm) = (&ar[p..ar.len() - s], &br[p..br.len() - s]);
    let (amv, bmv) = (&av[p..av.len() - s], &bv[p..bv.len() - s]);
    let Some(groups) = lcs_groups(am, amv, bm, bmv, WORD_DIFF_REFINE_MAX_CELLS) else {
        return whole();
    };
    if groups.len() > 2 {
        return whole();
    }
    let mut oc: Vec<(usize, usize)> = Vec::new();
    let mut nc: Vec<(usize, usize)> = Vec::new();
    for (d, i) in groups {
        oc.extend(d.into_iter().map(|(x, y)| (a.0 + x, a.0 + y)));
        nc.extend(i.into_iter().map(|(x, y)| (b.0 + x, b.0 + y)));
    }
    let covered = |spans: &[(usize, usize)], total: usize| -> f32 {
        if total == 0 {
            return 0.0;
        }
        spans.iter().map(|(x, y)| y - x).sum::<usize>() as f32 / total as f32
    };
    if covered(&oc, a.1 - a.0) >= REFINE_COVER_RATIO
        || covered(&nc, b.1 - b.0) >= REFINE_COVER_RATIO
    {
        return whole();
    }
    (oc, nc)
}

/// 置換された行の対から「変わった部分」だけを取り出す**純関数**。
///
/// 戻り値は `(旧側の範囲, 新側の範囲)`。`None` は**語単位を諦めた**印で、
/// 呼び出し側は行全体を塗る (極端に長い行で O(n²) を走らせないための逃げ)。
/// 完全に同じ行なら `Some((空, 空))`。
///
/// 手順は 2 段:
/// 1. 共通接頭辞・接尾辞を**トークン単位**で削り、残った中央部を LCS で突き合わせる。
///    トークンは「ASCII の識別子 1 個」か「書記素クラスタ 1 個」なので、
///    日本語のように空白の無い言語でも文字単位まで刻める。
/// 2. 1 対 1 の置換だけ、さらに**書記素クラスタ単位**で精査する
///    ([`refine_pair`])。`count` → `counter` が語まるごとの塗りにならない。
///
/// 前後が一致している行 (実際の差分のほとんど) では中央部が数トークンまで
/// 縮むので、上限に当たること自体が稀。
pub fn word_diff(old: &str, new: &str) -> Option<(Vec<WordSpan>, Vec<WordSpan>)> {
    if old == new {
        return Some((Vec::new(), Vec::new()));
    }
    if old.len() > WORD_DIFF_MAX_BYTES || new.len() > WORD_DIFF_MAX_BYTES {
        return None;
    }
    let ar = tokenize(old);
    let br = tokenize(new);
    let av: Vec<&str> = ar.iter().map(|&(x, y)| &old[x..y]).collect();
    let bv: Vec<&str> = br.iter().map(|&(x, y)| &new[x..y]).collect();
    let (p, s) = common_affix(&av, &bv);
    let (am, bm) = (&ar[p..ar.len() - s], &br[p..br.len() - s]);
    let (amv, bmv) = (&av[p..av.len() - s], &bv[p..bv.len() - s]);
    let groups = lcs_groups(am, amv, bm, bmv, WORD_DIFF_MAX_CELLS)?;
    let mut oc: Vec<(usize, usize)> = Vec::new();
    let mut nc: Vec<(usize, usize)> = Vec::new();
    for (d, i) in groups {
        if let ([one], [two]) = (&d[..], &i[..]) {
            let (a2, b2) = refine_pair(old, *one, new, *two);
            oc.extend(a2);
            nc.extend(b2);
        } else {
            oc.extend(d);
            nc.extend(i);
        }
    }
    Some((merge_spans(&oc), merge_spans(&nc)))
}

// ---------------------------------------------------------------------------
// 未変更行の折りたたみ
// ---------------------------------------------------------------------------

/// 畳む行の区間 (半開区間)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoldRun {
    pub start: usize,
    pub end: usize,
}

impl FoldRun {
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }
}

/// 変更の前後に残す文脈行の数 (VS Code の既定と同じ 3 行)。
pub const CONTEXT_KEEP: usize = 3;

/// 連続する文脈行のうち、前後 `keep` 行を残した**中央部**を畳む**純関数**。
///
/// 畳んだ結果が 2 行に満たない塊は畳まない (「⋯ 1 行を展開」は
/// 元の 1 行より場所を食うだけで得が無い)。つまり `2*keep + 2` 行以上
/// 続いたときにだけ畳む。`keep == 0` なら 2 行以上の塊すべてが対象。
pub fn fold_context_runs(kinds: &[LineKind], keep: usize) -> Vec<FoldRun> {
    let min_run = keep * 2 + 2;
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < kinds.len() {
        if kinds[i] != LineKind::Context {
            i += 1;
            continue;
        }
        let start = i;
        while i < kinds.len() && kinds[i] == LineKind::Context {
            i += 1;
        }
        if i - start >= min_run {
            out.push(FoldRun {
                start: start + keep,
                end: i - keep,
            });
        }
    }
    out
}

/// `line` を隠す折りたたみがあれば、その区間を返す。
pub fn fold_covering(runs: &[FoldRun], line: usize) -> Option<FoldRun> {
    runs.iter()
        .find(|r| line >= r.start && line < r.end)
        .copied()
}

// ---------------------------------------------------------------------------
// 変更箇所のジャンプ (F7 / ⇧F7)
// ---------------------------------------------------------------------------

/// 変更の塊の先頭位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChangeAnchor {
    pub file: usize,
    pub hunk: usize,
    /// [`Hunk::lines`] の添字。
    pub line: usize,
}

/// 「変更の塊」= 文脈行に挟まれた追加/削除のひと続き、の先頭を全部集める
/// **純関数**。F7 / ⇧F7 の飛び先はこの列を順に辿る。
pub fn change_blocks(files: &[FileDiff]) -> Vec<ChangeAnchor> {
    let mut out = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        for (hi, h) in f.hunks.iter().enumerate() {
            let mut prev_changed = false;
            for (li, l) in h.lines.iter().enumerate() {
                let changed = l.kind != LineKind::Context;
                if changed && !prev_changed {
                    out.push(ChangeAnchor {
                        file: fi,
                        hunk: hi,
                        line: li,
                    });
                }
                prev_changed = changed;
            }
        }
    }
    out
}

/// 現在位置と向きから次の変更番号を決める**純関数**。
///
/// * まだどこにも居ない (`None`) なら、前向きは先頭・後ろ向きは末尾へ。
/// * 端を越えたら反対側へ回り込む。
/// * 変更が 0 件なら `None` (呼び出し側は「変更はありません」と伝えるだけ)。
pub fn next_change_index(cur: Option<usize>, delta: i32, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let forward = delta >= 0;
    let Some(cur) = cur else {
        return Some(if forward { 0 } else { len - 1 });
    };
    let cur = cur.min(len - 1);
    Some(if forward {
        (cur + 1) % len
    } else {
        (cur + len - 1) % len
    })
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
    /// 語単位で「ここが変わった」を示す濃いめの塗り (行の地色より強い)。
    add_word_bg: Color32,
    del_word_bg: Color32,
    /// 片側にしか行が無いときの空プレースホルダ (VS Code の斜線帯に相当)。
    void_bg: Color32,
    gutter_bg: Color32,
    hunk_bg: Color32,
    add_fg: Color32,
    del_fg: Color32,
}

impl DiffPalette {
    fn from_theme(t: &Theme) -> Self {
        // ライトテーマは地の明度が高く、同じ比率では色が沈むので濃いめに混ぜる。
        let tint = if t.dark { 0.18 } else { 0.26 };
        // 語単位の塗りは行の地色の 2 倍強度。行の中で「ここ」が分かる濃さ。
        let word = if t.dark { 0.42 } else { 0.5 };
        DiffPalette {
            add_bg: mix(t.bg, t.ok, tint),
            del_bg: mix(t.bg, t.err, tint),
            add_word_bg: mix(t.bg, t.ok, word),
            del_word_bg: mix(t.bg, t.err, word),
            void_bg: mix(t.bg, t.panel, if t.dark { 0.55 } else { 0.75 }),
            gutter_bg: mix(t.bg, t.panel, 0.7),
            hunk_bg: mix(t.bg, t.accent_soft, 0.9),
            // 記号 (+/-) は本文より強調するが、テーマ色を保つ。
            add_fg: mix(t.text, t.ok, if t.dark { 0.65 } else { 0.55 }),
            del_fg: mix(t.text, t.err, if t.dark { 0.65 } else { 0.55 }),
        }
    }
}

// ---------------------------------------------------------------------------
// 表示モード / ジャンプ要求の受け渡し
//
// diff_ui の**シグネチャを変えずに**アプリ側と状態を共有するための口。
// 既存の `take_pending_review_prompt` と同じ流儀 (型で衝突を防ぐ newtype を
// egui の一時データへ置く) に揃えてある。
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
struct ModeCell(DiffMode);

fn mode_id() -> egui::Id {
    egui::Id::new("zv-diff-mode")
}

/// いま使う表示モード。未設定なら既定 (並列)。
pub fn diff_mode(ctx: &egui::Context) -> DiffMode {
    ctx.data(|d| d.get_temp::<ModeCell>(mode_id()))
        .unwrap_or_default()
        .0
}

/// 表示モードを差し替える (config の既定の反映・トグル・パレットから使う)。
pub fn set_diff_mode(ctx: &egui::Context, mode: DiffMode) {
    ctx.data_mut(|d| d.insert_temp(mode_id(), ModeCell(mode)));
}

/// 変更箇所ジャンプの依頼。`frame` を持つのは、差分ビューが出ていない
/// フレームに撃たれた依頼が**後で暴発しない**ようにするため。
/// `Default` は egui の `remove_temp` の型境界を満たすためだけに要る
/// (`delta == 0` は「何も頼まれていない」に等しく、無害な値)。
#[derive(Clone, Copy, Debug, Default)]
struct JumpRequest {
    delta: i32,
    frame: u64,
}

fn jump_id() -> egui::Id {
    egui::Id::new("zv-diff-jump")
}

/// 次 (`delta > 0`) / 前 (`delta < 0`) の変更へ飛ぶよう頼む。
pub fn request_jump(ctx: &egui::Context, delta: i32) {
    let frame = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| d.insert_temp(jump_id(), JumpRequest { delta, frame }));
}

/// 依頼を 1 回だけ取り出す。1 フレーム以上前の依頼は捨てる
/// (差分ビューが出ていないときに撃たれた F7 は、静かに無かったことにする)。
fn take_jump(ctx: &egui::Context) -> Option<i32> {
    let req = ctx.data_mut(|d| d.remove_temp::<JumpRequest>(jump_id()))?;
    (ctx.cumulative_pass_nr().saturating_sub(req.frame) <= 1).then_some(req.delta)
}

/// 差分ビューがアプリへ伝えたい一言 (「変更はありません」など)。
#[derive(Clone, Debug, Default)]
struct PendingNotice(String);

fn notice_id() -> egui::Id {
    egui::Id::new("zv-diff-notice")
}

/// 差分ビューからの通知を **1 回だけ**取り出す。app.rs 側でトーストにする。
pub fn take_pending_notice(ctx: &egui::Context) -> Option<String> {
    ctx.data_mut(|d| d.remove_temp::<PendingNotice>(notice_id()))
        .map(|n| n.0)
        .filter(|s| !s.is_empty())
}

fn set_notice(ctx: &egui::Context, msg: String) {
    ctx.data_mut(|d| d.insert_temp(notice_id(), PendingNotice(msg)));
}

/// いま選ばれている変更番号 (F7 で進む位置)。
#[derive(Clone, Copy, Debug, Default)]
struct NavCell(Option<usize>);

/// 展開済みの折りたたみ (ファイル, ハンク, 区間の先頭)。既定は「全部畳む」。
#[derive(Clone, Debug, Default)]
struct UnfoldedCell(std::collections::HashSet<(usize, usize, usize)>);

/// ファイル差分ごとの構文色キャッシュ。`key` が変わったときだけ計算し直す。
#[derive(Clone)]
struct HlCell {
    key: u64,
    /// ハンクを跨いだ通し番号 → その行の (開始, 終了, 色)。
    spans: std::sync::Arc<Vec<Vec<(usize, usize, Color32)>>>,
}

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

/// diff を表示する。スクロールは呼び出し側の責務。
///
/// 表示モード (一列 / 並列) は [`diff_mode`] が返す**単一の列挙型**で決まり、
/// 幅が足りなければ [`diff_layout`] が自動で一列へ縮退させる。
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

/// 構文ハイライトを諦める差分の大きさ (バイト)。
/// これを超えるファイル差分は素の色で描く (`editor::LargeFileMode` と同じ考え方)。
const DIFF_HL_MAX_BYTES: usize = 200_000;

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

    let ppp = ui.ctx().pixels_per_point();
    let mode = diff_mode(ui.ctx());
    // ファイルの中身は CollapsingHeader のぶんだけ字下げされる。行はその
    // **内側の幅**に収めなければならないので、操作バーの表示判断も同じ幅で行う
    // (実際の桁割りは下で body の available_width から取り直す)。
    let outer = diff_layout(
        (ui.available_width() - ui.spacing().indent).max(0.0),
        mode,
        ppp,
    );
    let action = diff_toolbar_ui(ui, theme, comments, size, &outer);

    // --- F7 / ⇧F7: 次/前の変更へ ---------------------------------------
    let nav_id = ui.id().with("zv-diff-nav");
    let mut jump_to: Option<ChangeAnchor> = None;
    if let Some(delta) = take_jump(ui.ctx()) {
        let blocks = change_blocks(files);
        let cur = ui
            .data(|d| d.get_temp::<NavCell>(nav_id))
            .unwrap_or_default()
            .0;
        match next_change_index(cur, delta, blocks.len()) {
            Some(n) => {
                ui.data_mut(|d| d.insert_temp(nav_id, NavCell(Some(n))));
                jump_to = blocks.get(n).copied();
            }
            // 変更が 0 件: 何も動かさず、ひとこと伝えるだけ。
            None => set_notice(ui.ctx(), tr("変更はありません")),
        }
    }

    // --- 文脈行の折りたたみ (既定は畳む。展開したものだけ覚える) --------
    let unfold_id = ui.id().with("zv-diff-unfold");
    let mut unfolded = ui
        .data(|d| d.get_temp::<UnfoldedCell>(unfold_id))
        .unwrap_or_default()
        .0;
    let mut unfold_toggle: Option<(usize, usize, usize)> = None;

    let font = FontId::monospace(size);
    let row_h = crate::theme::snap_len(ui.fonts(|f| f.row_height(&font)) + 2.0, ppp);

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
                // 字下げされた**この ui の幅**で桁を割る。外側の幅で割ると
                // 右ペインが字下げのぶんだけ右へはみ出す。
                let lay = diff_layout(ui.available_width(), mode, ppp);
                let syntax = file_line_colors(ui, theme, file, &path, fi);
                let clip = ui.clip_rect();
                // ファイル内の通し行番号 (syntax の添字)。
                let mut ord = 0usize;
                for (hi, hunk) in file.hunks.iter().enumerate() {
                    hunk_header_ui(ui, theme, &pal, hunk, size);
                    let base = ord;
                    ord += hunk.lines.len();
                    let kinds: Vec<LineKind> = hunk.lines.iter().map(|l| l.kind).collect();
                    let folds = fold_context_runs(&kinds, CONTEXT_KEEP);
                    let rows: Vec<(Option<LineRef>, Option<LineRef>)> = match lay.mode {
                        // 一列: 左右の区別が無いので「左だけ」に全行を流す。
                        DiffMode::Inline => (0..hunk.lines.len())
                            .map(|i| (Some(LineRef::at(i)), None))
                            .collect(),
                        DiffMode::SideBySide => align_hunk(hunk),
                    };
                    for (left, right) in rows {
                        // 折りたたみ対象かどうかは「文脈行の添字」で決まる。
                        let li = left.or(right).map(|r| r.idx).unwrap_or(0);
                        if let Some(run) = fold_covering(&folds, li) {
                            if !unfolded.contains(&(fi, hi, run.start)) {
                                if li == run.start
                                    && fold_row_ui(ui, theme, &pal, &lay, run.len(), size, row_h)
                                {
                                    unfold_toggle = Some((fi, hi, run.start));
                                }
                                continue;
                            }
                        }
                        let scroll_here = jump_to.is_some_and(|a| {
                            a.file == fi
                                && a.hunk == hi
                                && (left.is_some_and(|r| r.idx == a.line)
                                    || right.is_some_and(|r| r.idx == a.line))
                        });
                        let mut cx = RowCtx {
                            ui: &mut *ui,
                            theme,
                            pal: &pal,
                            lay: &lay,
                            comments: &mut *comments,
                            syntax: &syntax,
                        };
                        diff_row_ui(
                            &mut cx,
                            RowArgs {
                                hunk,
                                base,
                                left,
                                right,
                                path: &path,
                                size,
                                row_h,
                                clip,
                                scroll_here,
                            },
                        );
                    }
                }
            });
        ui.add_space(6.0);
    }

    if let Some(k) = unfold_toggle {
        if !unfolded.remove(&k) {
            unfolded.insert(k);
        }
        ui.data_mut(|d| d.insert_temp(unfold_id, UnfoldedCell(unfolded)));
    }
    action
}

/// 表示モードのボタンを**このフレームで最初の 1 回だけ**描くための札。
///
/// Git のレビュー画面は「1 ファイル = 1 回」`diff_ui_with_actions` を呼ぶので、
/// 素朴に描くとファイルの数だけ同じトグルが並ぶ (= 同じ操作への到達経路が
/// 何本もある状態)。札を取れた最初の 1 回にだけ描いて重複を潰す。
fn claim_mode_toggle(ctx: &egui::Context) -> bool {
    let id = egui::Id::new("zv-diff-toolbar-pass");
    let now = ctx.cumulative_pass_nr();
    if ctx.data(|d| d.get_temp::<u64>(id)) == Some(now) {
        return false;
    }
    ctx.data_mut(|d| d.insert_temp(id, now));
    true
}

/// 差分ビューの操作バー。表示モードの切替が**常に**ここから届く
/// (キーバインドを覚えていなくても使える到達経路)。
/// レビュー操作はコメントが 1 件でもあるときだけ足す。
/// **両方とも出すものが無ければバー自体を描かない** (空の帯を残さない)。
fn diff_toolbar_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    store: &mut DiffCommentStore,
    size: f32,
    lay: &DiffLayout,
) -> DiffAction {
    let show_mode = claim_mode_toggle(ui.ctx());
    let show_review = !store.is_empty();
    if !show_mode && !show_review {
        return DiffAction::None;
    }
    let mut action = DiffAction::None;
    ui.horizontal_wrapped(|ui| {
        if show_mode {
            let (icon, next) = match lay.mode {
                DiffMode::SideBySide => ("⇋", DiffMode::Inline),
                DiffMode::Inline => ("≡", DiffMode::SideBySide),
            };
            if ui
                .add(egui::Button::new(
                    RichText::new(format!("{icon} {}", lay.mode.label())).size(size),
                ))
                .on_hover_text(trf(
                    "{next} に切り替えます (F7 / ⇧F7 で変更箇所を移動)",
                    &[("next", next.label())],
                ))
                .clicked()
            {
                // 押した時点の「実際の表示」の逆へ。縮退中に押しても
                // 「並列を選んだのに一列のまま」にはならない。
                set_diff_mode(ui.ctx(), next);
            }
            if lay.degraded() {
                ui.add(
                    egui::Label::new(
                        RichText::new(tr("幅が狭いため一列"))
                            .color(theme.text_dim)
                            .size(size * 0.9),
                    )
                    .wrap_mode(egui::TextWrapMode::Truncate),
                )
                .on_hover_text(tr(
                    "並列表示は片側が読める幅を確保できないため、自動で一列にしています",
                ));
            }
        }
        if !show_review {
            return;
        }
        if show_mode {
            ui.separator();
        }
        ui.add(
            egui::Label::new(
                RichText::new(trf(
                    "レビューコメント {n} 件 (未解決 {a} 件)",
                    &[
                        ("n", store.len().to_string()),
                        ("a", store.actionable_len().to_string()),
                    ],
                ))
                .color(theme.text_dim)
                .size(size),
            )
            .wrap_mode(egui::TextWrapMode::Truncate),
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
///
/// **0 のバッジは出さない** (リネームのみ・モード変更だけのファイルに
/// `+0 -0` が並んでも情報がゼロで、パスが読みにくくなるだけ)。
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
        if file.additions > 0 {
            job.append(&format!("  +{}", file.additions), 0.0, fmt(theme.ok));
        }
        if file.deletions > 0 {
            job.append(&format!("  -{}", file.deletions), 0.0, fmt(theme.err));
        }
    }
    job
}

/// ハンク見出しの左右の余白 (片側)。
const HUNK_PAD_X: f32 = 4.0;

/// ハンク見出し (`@@ ... @@`) — アクセント色の帯で本文と区別する。
fn hunk_header_ui(ui: &mut egui::Ui, theme: &Theme, pal: &DiffPalette, hunk: &Hunk, size: f32) {
    // 余白は帯の**内側**なので、可用幅から先に引く。引き忘れると帯が
    // 左右の余白ぶんだけ広がり、可用領域からはみ出す。
    let w = (ui.available_width() - HUNK_PAD_X * 2.0).max(0.0);
    egui::Frame::none()
        .fill(pal.hunk_bg)
        .inner_margin(egui::Margin::symmetric(HUNK_PAD_X, 2.0))
        .show(ui, |ui| {
            ui.set_min_width(w);
            ui.add(
                egui::Label::new(
                    RichText::new(&hunk.header)
                        .monospace()
                        .size(size)
                        .color(theme.accent),
                )
                .wrap_mode(egui::TextWrapMode::Truncate),
            )
            .on_hover_text(&hunk.header);
        });
}

/// ファイル差分の全行を 1 パスで色分けし、ハンクを跨いだ通し番号で引ける表を返す。
///
/// **再計算は鍵が変わったときだけ**。鍵は「パス・配色テーマ・ハンクの形
/// (ヘッダと行数)・本文の合計バイト数・増減行数」から作る — 毎フレーム
/// 数百 KB の本文をハッシュすると描画フレームのコストが素直に乗るので、
/// **バイトを読まない要素だけ**で作る。中身だけが変わってこの鍵が一致する
/// ことは実用上ありえない (行数も合計長も動く)。
fn file_line_colors(
    ui: &egui::Ui,
    theme: &Theme,
    file: &FileDiff,
    path: &str,
    fi: usize,
) -> std::sync::Arc<Vec<Vec<(usize, usize, Color32)>>> {
    use std::hash::{Hash, Hasher};
    let id = ui.id().with(("zv-diff-hl", fi));
    let total: usize = file
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .map(|l| l.text.len() + 1)
        .sum();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    theme.syntect_theme.hash(&mut hasher);
    total.hash(&mut hasher);
    file.additions.hash(&mut hasher);
    file.deletions.hash(&mut hasher);
    for h in &file.hunks {
        h.header.hash(&mut hasher);
        h.lines.len().hash(&mut hasher);
    }
    let key = hasher.finish();
    if let Some(c) = ui.data(|d| d.get_temp::<HlCell>(id)) {
        if c.key == key {
            return c.spans;
        }
    }
    let lines: Vec<&str> = file
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .map(|l| l.text.as_str())
        .collect();
    let spans = if total > DIFF_HL_MAX_BYTES {
        Vec::new()
    } else {
        let hl = crate::highlight::shared();
        let lang = hl.lang_for(
            Some(std::path::Path::new(path)),
            lines.first().copied().unwrap_or(""),
        );
        hl.line_spans(&lines, &lang, &theme.syntect_theme)
    };
    let arc = std::sync::Arc::new(spans);
    ui.data_mut(|d| {
        d.insert_temp(
            id,
            HlCell {
                key,
                spans: arc.clone(),
            },
        )
    });
    arc
}

/// 1 行ぶんの `LayoutJob`。構文色 (前景) と語単位の変更 (背景) を重ねる。
///
/// 範囲の境界だけで区切って `append` するので、手間は区間数に比例するだけ。
fn line_job(
    text: &str,
    syntax: &[(usize, usize, Color32)],
    words: &[WordSpan],
    font: &FontId,
    fg: Color32,
    word_bg: Color32,
) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    if text.is_empty() {
        return job;
    }
    let n = text.len();
    let mut cuts: Vec<usize> = Vec::with_capacity(syntax.len() * 2 + words.len() * 2 + 2);
    cuts.push(0);
    cuts.push(n);
    for (s, e, _) in syntax {
        cuts.push((*s).min(n));
        cuts.push((*e).min(n));
    }
    for w in words {
        cuts.push(w.start.min(n));
        cuts.push(w.end.min(n));
    }
    // 文字境界でない切れ目は捨てる (色より本文の正しさが先)。
    cuts.retain(|&c| text.is_char_boundary(c));
    cuts.sort_unstable();
    cuts.dedup();
    for pair in cuts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if b <= a {
            continue;
        }
        let color = syntax
            .iter()
            .find(|(s, e, _)| a >= *s && a < *e)
            .map(|(_, _, c)| *c)
            .unwrap_or(fg);
        let background = if words.iter().any(|w| a >= w.start && a < w.end) {
            word_bg
        } else {
            Color32::TRANSPARENT
        };
        job.append(
            &text[a..b],
            0.0,
            TextFormat {
                font_id: font.clone(),
                color,
                background,
                ..Default::default()
            },
        );
    }
    job
}

/// 1 ペインに描くもの。`line` が `None` なら「反対側にしか行が無い」空欄。
struct Cell<'a> {
    line: Option<&'a DiffLine>,
    /// 行番号列に出す番号 (左から順)。使う本数は [`PaneCols::cols`]。
    nums: [Option<usize>; 2],
    badge: Option<(usize, bool)>,
    syntax: &'a [(usize, usize, Color32)],
    words: Vec<WordSpan>,
}

/// 行の描画に要る参照をひとまとめにする (引数の数を抑えるため)。
struct RowCtx<'a> {
    ui: &'a mut egui::Ui,
    theme: &'a Theme,
    pal: &'a DiffPalette,
    lay: &'a DiffLayout,
    comments: &'a mut DiffCommentStore,
    syntax: &'a std::sync::Arc<Vec<Vec<(usize, usize, Color32)>>>,
}

/// 行 1 本ぶんの位置情報。
struct RowArgs<'a> {
    hunk: &'a Hunk,
    /// ファイル内の通し番号の起点 (`syntax` の添字は `base + idx`)。
    base: usize,
    left: Option<LineRef>,
    right: Option<LineRef>,
    path: &'a str,
    size: f32,
    row_h: f32,
    clip: egui::Rect,
    /// この行へスクロールして見せる (F7 の飛び先)。
    scroll_here: bool,
}

/// 対応する 1 行 (一列なら 1 セル、並列なら左右 2 セル) を描く。
///
/// **左右のセルは同じ 1 本の矩形の中に置く**ので、行の高さは構造的に必ず揃う。
/// スクロールの同期処理そのものが不要になり、毎フレームの再描画要求もゼロ。
fn diff_row_ui(cx: &mut RowCtx, args: RowArgs) {
    let lay: &DiffLayout = cx.lay;
    let theme: &Theme = cx.theme;
    let pal: &DiffPalette = cx.pal;
    let syntax_all = cx.syntax;
    let side_by_side = lay.mode == DiffMode::SideBySide;

    let line_of = |r: Option<LineRef>| r.and_then(|r| args.hunk.lines.get(r.idx));
    let left_line = line_of(args.left);
    let right_line = line_of(args.right);

    // コメントのアンカー (並列は左右それぞれ、一列は 1 本)。
    let anchor_of = |l: Option<&DiffLine>| l.and_then(|l| line_anchor(args.path, l));
    let left_anchor = anchor_of(left_line);
    let right_anchor = if side_by_side {
        anchor_of(right_line)
    } else {
        None
    };
    // 並列でも文脈行は左右が同じ行 = 同じアンカー。スレッドを二重に出さない。
    let right_anchor = match (&left_anchor, right_anchor) {
        (Some(a), Some(b)) if *a == b => None,
        (_, b) => b,
    };

    let expanded = [&left_anchor, &right_anchor]
        .into_iter()
        .flatten()
        .any(|a| cx.comments.has_ui_at(a));

    // 仮想化: 画面外で、かつコメントも下書きも無い行はウィジェットを作らず
    // 高さだけ確保する。数千行の PR でも毎フレームの手間は可視行ぶんだけ。
    let top = cx.ui.cursor().top();
    let offscreen = top + args.row_h < args.clip.top() || top > args.clip.bottom();
    if offscreen && !expanded {
        let (_, rect) = cx
            .ui
            .allocate_space(egui::vec2(lay.width.max(1.0), args.row_h));
        if args.scroll_here {
            cx.ui.scroll_to_rect(rect, Some(egui::Align::Center));
        }
        return;
    }

    let (rect, resp) = cx.ui.allocate_exact_size(
        egui::vec2(lay.width.max(1.0), args.row_h),
        egui::Sense::click(),
    );
    if args.scroll_here {
        cx.ui.scroll_to_rect(rect, Some(egui::Align::Center));
    }

    // --- 語単位ハイライト: 置換の組にだけ効かせる ---
    let (mut lw, mut rw) = (Vec::new(), Vec::new());
    if let (Some(o), Some(n)) = (left_line, right_line) {
        if o.kind == LineKind::Removed && n.kind == LineKind::Added {
            if let Some((a, b)) = word_diff(&o.text, &n.text) {
                lw = a;
                rw = b;
            }
        }
    }

    let span_of = |r: Option<LineRef>| -> &[(usize, usize, Color32)] {
        r.and_then(|r| syntax_all.get(args.base + r.idx))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    };

    let cells: Vec<Cell> = if side_by_side {
        vec![
            Cell {
                line: left_line,
                nums: [left_line.and_then(|l| l.old_no), None],
                badge: left_anchor.as_ref().and_then(|a| cx.comments.badge(a)),
                syntax: span_of(args.left),
                words: lw,
            },
            Cell {
                line: right_line,
                nums: [right_line.and_then(|l| l.new_no), None],
                badge: right_anchor.as_ref().and_then(|a| cx.comments.badge(a)),
                syntax: span_of(args.right),
                words: rw,
            },
        ]
    } else {
        vec![Cell {
            line: left_line,
            nums: [
                left_line.and_then(|l| l.old_no),
                left_line.and_then(|l| l.new_no),
            ],
            badge: left_anchor.as_ref().and_then(|a| cx.comments.badge(a)),
            syntax: span_of(args.left),
            words: Vec::new(),
        }]
    };

    for (cols, cell) in lay.panes.iter().zip(cells.iter()) {
        paint_cell(cx.ui, theme, pal, cols, rect, cell, args.size);
    }
    drop(cells);

    // --- クリック: 押されたペインの行へコメントを開く ---
    if resp.hovered() {
        cx.ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        cx.ui
            .painter()
            .rect_filled(rect, 0.0, mix(Color32::TRANSPARENT, theme.accent, 0.10));
    }
    let resp = resp.on_hover_text(tr("クリックでコメントを追加/閉じる"));
    if resp.clicked() {
        let x = resp
            .interact_pointer_pos()
            .map(|p| p.x - rect.left())
            .unwrap_or(0.0);
        let right_side = side_by_side && lay.panes.len() > 1 && x >= lay.panes[1].x;
        let target = if right_side {
            right_anchor.clone().or_else(|| left_anchor.clone())
        } else {
            left_anchor.clone().or_else(|| right_anchor.clone())
        };
        if let Some(a) = target {
            cx.comments.toggle_draft(a);
        }
    }

    // --- 行の下のコメントスレッド (左右どちらの行にも打てる) ---
    let indent = lay
        .panes
        .first()
        .map(|c| c.gutter_w + c.mark_w)
        .unwrap_or(0.0);
    for (anchor, line) in [(left_anchor, left_line), (right_anchor, right_line)] {
        if let (Some(a), Some(l)) = (anchor, line) {
            if cx.comments.has_ui_at(&a) {
                comment_thread_ui(cx.ui, theme, cx.comments, &a, l, args.size, indent);
            }
        }
    }
}

/// 1 ペインぶんを塗る。**ペインの外へは 1px も描かない** (必ずクリップする)。
fn paint_cell(
    ui: &egui::Ui,
    theme: &Theme,
    pal: &DiffPalette,
    cols: &PaneCols,
    row: egui::Rect,
    cell: &Cell,
    size: f32,
) {
    let x0 = row.left() + cols.x;
    let pane = egui::Rect::from_min_max(
        egui::pos2(x0, row.top()),
        egui::pos2(x0 + cols.width, row.bottom()),
    );
    let p = ui.painter().with_clip_rect(pane.intersect(ui.clip_rect()));
    let (bg, sign_fg, sign) = match cell.line {
        None => (pal.void_bg, theme.text_dim, ""),
        Some(l) => match l.kind {
            LineKind::Added => (pal.add_bg, pal.add_fg, "+"),
            LineKind::Removed => (pal.del_bg, pal.del_fg, "-"),
            LineKind::Context => (theme.bg, theme.text_dim, " "),
        },
    };
    p.rect_filled(pane, 0.0, bg);
    let Some(line) = cell.line else {
        // 反対側にしか行が無い = ここには何も無い、を地色で示す。
        return;
    };
    // 行番号列
    if cols.gutter_w > 0.0 {
        p.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x0, row.top()),
                egui::pos2(x0 + cols.gutter_w, row.bottom()),
            ),
            0.0,
            pal.gutter_bg,
        );
        let n = cols.cols.max(1) as f32;
        let sub = cols.gutter_w / n;
        let num_font = FontId::monospace(size * 0.9);
        for (i, no) in cell.nums.iter().take(cols.cols as usize).enumerate() {
            let Some(no) = no else { continue };
            p.text(
                egui::pos2(x0 + sub * (i as f32 + 1.0) - 3.0, row.top() + 1.0),
                egui::Align2::RIGHT_TOP,
                no.to_string(),
                num_font.clone(),
                theme.text_dim,
            );
        }
    }
    // コメント印
    if let Some((n, all_resolved)) = cell.badge {
        let text = if n > 1 {
            format!("●{n}")
        } else {
            "●".to_string()
        };
        p.text(
            egui::pos2(x0 + cols.gutter_w + 2.0, row.top() + 1.0),
            egui::Align2::LEFT_TOP,
            text,
            FontId::monospace(size * 0.8),
            if all_resolved { theme.ok } else { theme.accent },
        );
    }
    // +/- の記号
    if !sign.trim().is_empty() {
        p.text(
            egui::pos2(x0 + cols.gutter_w + cols.mark_w + 2.0, row.top() + 1.0),
            egui::Align2::LEFT_TOP,
            sign,
            FontId::monospace(size),
            sign_fg,
        );
    }
    // 本文
    if cols.text_w <= 0.0 {
        return;
    }
    let word_bg = match line.kind {
        LineKind::Added => pal.add_word_bg,
        LineKind::Removed => pal.del_word_bg,
        LineKind::Context => Color32::TRANSPARENT,
    };
    let job = line_job(
        &line.text,
        cell.syntax,
        &cell.words,
        &FontId::monospace(size),
        theme.text,
        word_bg,
    );
    let galley = ui.fonts(|f| f.layout_job(job));
    p.galley(
        egui::pos2(row.left() + cols.text_x, row.top() + 1.0),
        galley,
        theme.text,
    );
}

/// 「⋯ N 行を展開」の 1 行。押されたら true。
fn fold_row_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    pal: &DiffPalette,
    lay: &DiffLayout,
    hidden: usize,
    size: f32,
    row_h: f32,
) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(lay.width.max(1.0), row_h), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
        p.rect_filled(rect, 0.0, pal.gutter_bg);
        let text_x = lay.panes.first().map(|c| c.text_x).unwrap_or(0.0);
        p.text(
            egui::pos2(rect.left() + text_x, rect.top() + 1.0),
            egui::Align2::LEFT_TOP,
            trf("⋯ {n} 行を展開", &[("n", hidden.to_string())]),
            FontId::monospace(size),
            if resp.hovered() {
                theme.accent
            } else {
                theme.text_dim
            },
        );
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(tr("変更のない行を表示します")).clicked()
}

/// コメントスレッドの右余白。
const THREAD_PAD_RIGHT: f32 = 6.0;

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
    indent: f32,
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

    let avail = ui.available_width();
    // 本文と桁を合わせる字下げ。狭い幅では本文の場所が無くなるので
    // 使える幅の 1/4 で頭打ちにする (どの幅でも見切れない)。
    let indent = indent.max(0.0).min(avail * 0.25);
    // 左右の余白は枠の**内側**なので、最小幅から先に引いておく
    // (引かないと枠が余白ぶん広がって可用領域をはみ出す)。
    let w = (avail - indent - THREAD_PAD_RIGHT).max(0.0);
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
            // 行番号列 + コメント印の幅ぶん右へ寄せて、本文と桁を合わせる。
            left: indent,
            right: THREAD_PAD_RIGHT,
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
                                // ID はコメント id から作る。省くと egui は
                                // **並び順から自動採番**するため、上の行が
                                // 増減した瞬間にカーソル/選択が別のコメントへ
                                // 移る (編集中に解決印を付けると起きる)。
                                .id_salt(("zv-diff-comment-edit", id))
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

    // =======================================================================
    // 表示の決定層 (純関数) のテーブルテスト
    // =======================================================================

    // ---- diff_layout: 幅 → 桁割り / 縮退 ----------------------------------

    /// レイアウトの不変条件を全部まとめて検査する。
    ///
    /// 1. どのペインも可用領域 `[0, width]` の外へ出ない
    /// 2. ペイン同士が重ならない (左の右端 <= 右の左端)
    /// 3. ペイン内の桁 (行番号 / 印 / 記号 / 本文) の合計がペイン幅ぴったり
    /// 4. 幅は非負
    fn assert_layout_sane(lay: &DiffLayout, want_w: f32, ppp: f32, why: &str) {
        assert!(lay.width >= 0.0, "{why}: 幅が負");
        assert!(
            lay.width <= want_w + 0.001,
            "{why}: スナップで可用幅を超えた ({} > {want_w})",
            lay.width
        );
        assert!(!lay.panes.is_empty(), "{why}: ペインが 0 枚");
        assert_eq!(
            lay.panes.len(),
            match lay.mode {
                DiffMode::Inline => 1,
                DiffMode::SideBySide => 2,
            },
            "{why}: モードとペイン数が食い違う"
        );
        let mut prev_right = 0.0f32;
        for (i, p) in lay.panes.iter().enumerate() {
            assert!(p.x >= -0.001, "{why}[{i}]: 左端が領域外 ({})", p.x);
            assert!(
                p.x + p.width <= lay.width + 0.001,
                "{why}[{i}]: 右端が領域外 ({} > {})",
                p.x + p.width,
                lay.width
            );
            assert!(
                p.x >= prev_right - 0.001,
                "{why}[{i}]: 前のペインと重なっている ({} < {prev_right})",
                p.x
            );
            for (name, v) in [
                ("gutter_w", p.gutter_w),
                ("mark_w", p.mark_w),
                ("sign_w", p.sign_w),
                ("text_w", p.text_w),
                ("width", p.width),
            ] {
                assert!(v >= 0.0, "{why}[{i}]: {name} が負 ({v})");
            }
            let sum = p.gutter_w + p.mark_w + p.sign_w + p.text_w;
            assert!(
                (sum - p.width).abs() < 0.001,
                "{why}[{i}]: 桁の合計 {sum} がペイン幅 {} と合わない",
                p.width
            );
            assert!(
                (p.text_x - (p.x + p.gutter_w + p.mark_w + p.sign_w)).abs() < 0.001,
                "{why}[{i}]: 本文の左端がずれている"
            );
            // 物理ピクセルへ揃っている (端末セルと同じ規約)
            for (name, v) in [("x", p.x), ("gutter_w", p.gutter_w), ("mark_w", p.mark_w)] {
                let px = v * ppp;
                assert!(
                    (px - px.round()).abs() < 0.001,
                    "{why}[{i}]: {name} が物理ピクセルに揃っていない ({v} @ ppp={ppp})"
                );
            }
            prev_right = p.x + p.width + lay.gap;
        }
    }

    #[test]
    fn diff_layout_table() {
        // (可用幅, 希望モード, 期待する実モード, 何を見ているか)
        let table: &[(f32, DiffMode, DiffMode, &str)] = &[
            (320.0, DiffMode::SideBySide, DiffMode::Inline, "スマホ幅は一列へ縮退"),
            (320.0, DiffMode::Inline, DiffMode::Inline, "一列指定はそのまま"),
            (611.0, DiffMode::SideBySide, DiffMode::Inline, "閾値の 1px 下は一列"),
            (612.0, DiffMode::SideBySide, DiffMode::SideBySide, "閾値ちょうどは並列"),
            (900.0, DiffMode::SideBySide, DiffMode::SideBySide, "900x700 の中央ビュー"),
            (1200.0, DiffMode::SideBySide, DiffMode::SideBySide, "1200x300 の横長"),
            (1200.0, DiffMode::Inline, DiffMode::Inline, "広くても一列指定は一列"),
            (0.0, DiffMode::SideBySide, DiffMode::Inline, "幅 0 でも壊れない"),
            (0.0, DiffMode::Inline, DiffMode::Inline, "幅 0 の一列"),
            (1.0, DiffMode::SideBySide, DiffMode::Inline, "幅 1px"),
            (60.0, DiffMode::Inline, DiffMode::Inline, "行番号すら入らない幅"),
            (f32::NAN, DiffMode::SideBySide, DiffMode::Inline, "NaN は幅 0 扱い"),
            (f32::INFINITY, DiffMode::SideBySide, DiffMode::Inline, "無限も幅 0 扱い"),
        ];
        for &(w, req, want, why) in table {
            for ppp in [1.0f32, 1.5, 2.0] {
                let lay = diff_layout(w, req, ppp);
                assert_eq!(lay.mode, want, "{why} (w={w} ppp={ppp})");
                assert_eq!(lay.requested, req, "{why}: 希望モードを覚えていない");
                let cap = if w.is_finite() { w.max(0.0) } else { 0.0 };
                assert_layout_sane(&lay, cap.max(lay.width), ppp, why);
            }
        }
    }

    #[test]
    fn diff_layout_degraded_flag_only_when_downgraded() {
        assert!(diff_layout(320.0, DiffMode::SideBySide, 1.0).degraded());
        assert!(!diff_layout(320.0, DiffMode::Inline, 1.0).degraded());
        assert!(!diff_layout(900.0, DiffMode::SideBySide, 1.0).degraded());
    }

    #[test]
    fn diff_layout_side_by_side_halves_are_balanced_and_readable() {
        for w in [612.0f32, 900.0, 1200.0, 1920.0, 2560.0] {
            let lay = diff_layout(w, DiffMode::SideBySide, 1.0);
            assert_eq!(lay.mode, DiffMode::SideBySide, "w={w}");
            let (l, r) = (&lay.panes[0], &lay.panes[1]);
            assert!(
                (l.width - r.width).abs() <= 1.0,
                "w={w}: 左右の幅が偏っている ({} vs {})",
                l.width,
                r.width
            );
            assert!(l.cols == 1 && r.cols == 1, "w={w}: 並列は行番号 1 列ずつ");
            assert!(
                l.text_w >= MIN_CODE_W - 1.0 && r.text_w >= MIN_CODE_W - 1.0,
                "w={w}: 並べたのに本文が読める幅を割っている ({} / {})",
                l.text_w,
                r.text_w
            );
            // 溝が本当に空いている (左の右端 < 右の左端)
            assert!(l.x + l.width <= r.x, "w={w}: 溝が無い");
            assert!(r.x - (l.x + l.width) >= 4.0, "w={w}: 溝が狭すぎる");
        }
    }

    #[test]
    fn diff_layout_inline_has_two_number_columns() {
        let lay = diff_layout(900.0, DiffMode::Inline, 1.0);
        assert_eq!(lay.panes.len(), 1);
        assert_eq!(lay.panes[0].cols, 2, "一列は旧/新の 2 列を出す");
        assert_eq!(lay.panes[0].gutter_w, GUTTER_COL_W * 2.0);
        assert_eq!(lay.gap, 0.0, "一列に溝は要らない");
    }

    /// 極端な画面サイズでも全矩形が可用領域に収まり重ならない。
    #[test]
    fn diff_layout_extreme_sizes_keep_every_rect_inside() {
        // (幅, 高さ, 何の画面か) — 高さは判断に影響しないが、実際に使う組を並べる
        let screens: &[(f32, f32, &str)] = &[
            (320.0, 640.0, "スマホ縦"),
            (900.0, 700.0, "既定の小ウィンドウ"),
            (1200.0, 300.0, "横長の下部パネル"),
            (480.0, 900.0, "サイドバーに入れた細い差分"),
            (2560.0, 1440.0, "外部ディスプレイ"),
            (100.0, 100.0, "極小"),
        ];
        for &(w, h, why) in screens {
            for req in [DiffMode::Inline, DiffMode::SideBySide] {
                for ppp in [1.0f32, 2.0] {
                    let lay = diff_layout(w, req, ppp);
                    assert_layout_sane(&lay, w, ppp, &format!("{why} {h}px 高 {req:?}"));
                    // 「1 枚でも領域外へ出たら見切れる」— 面積の合計でも押さえる
                    let total: f32 = lay.panes.iter().map(|p| p.width).sum();
                    assert!(
                        total + lay.gap * (lay.panes.len() - 1) as f32 <= lay.width + 0.001,
                        "{why}: ペインの合計幅が可用幅を超えた"
                    );
                }
            }
        }
    }

    // ---- align_hunk: 左右の行揃え ----------------------------------------

    fn hunk_of(spec: &[(LineKind, &str)]) -> Hunk {
        let mut lines = Vec::new();
        let (mut o, mut n) = (1usize, 1usize);
        for (k, t) in spec {
            let (old_no, new_no) = match k {
                LineKind::Context => {
                    let v = (Some(o), Some(n));
                    o += 1;
                    n += 1;
                    v
                }
                LineKind::Removed => {
                    let v = (Some(o), None);
                    o += 1;
                    v
                }
                LineKind::Added => {
                    let v = (None, Some(n));
                    n += 1;
                    v
                }
            };
            lines.push(DiffLine {
                kind: *k,
                old_no,
                new_no,
                text: (*t).to_string(),
            });
        }
        Hunk {
            header: "@@ -1 +1 @@".into(),
            old_start: 1,
            new_start: 1,
            lines,
        }
    }

    fn aligned(spec: &[(LineKind, &str)]) -> Vec<(Option<usize>, Option<usize>)> {
        align_hunk(&hunk_of(spec))
            .into_iter()
            .map(|(l, r)| (l.map(|x| x.idx), r.map(|x| x.idx)))
            .collect()
    }

    #[test]
    fn align_hunk_table() {
        use LineKind::{Added as A, Context as C, Removed as R};
        // (入力の行種, 期待する (左, 右) の並び, 何を見ているか)
        let table: &[(&[(LineKind, &str)], &[(Option<usize>, Option<usize>)], &str)] = &[
            (&[], &[], "空ハンク"),
            (
                &[(C, "a"), (C, "b")],
                &[(Some(0), Some(0)), (Some(1), Some(1))],
                "文脈行だけ: 左右に同じ行",
            ),
            (
                &[(A, "x"), (A, "y")],
                &[(None, Some(0)), (None, Some(1))],
                "追加のみ: 左は空プレースホルダ",
            ),
            (
                &[(R, "x"), (R, "y")],
                &[(Some(0), None), (Some(1), None)],
                "削除のみ: 右は空プレースホルダ",
            ),
            (
                &[(R, "old"), (A, "new")],
                &[(Some(0), Some(1))],
                "1 対 1 の置換は同じ高さに並ぶ",
            ),
            (
                &[(R, "a"), (R, "b"), (A, "c")],
                &[(Some(0), Some(2)), (Some(1), None)],
                "削除 2 追加 1: 余った削除の右は空",
            ),
            (
                &[(R, "a"), (A, "b"), (A, "c")],
                &[(Some(0), Some(1)), (None, Some(2))],
                "削除 1 追加 2: 余った追加の左は空",
            ),
            (
                &[(C, "h"), (R, "a"), (A, "b"), (C, "t")],
                &[(Some(0), Some(0)), (Some(1), Some(2)), (Some(3), Some(3))],
                "文脈に挟まれた置換",
            ),
            (
                &[(A, "x"), (R, "y")],
                &[(None, Some(0)), (Some(1), None)],
                "追加のあとの削除は別の塊 (順序が入れ替わらない)",
            ),
            (
                &[(R, "a"), (A, "b"), (R, "c"), (A, "d")],
                &[(Some(0), Some(1)), (Some(2), Some(3))],
                "置換が 2 連: それぞれ組になる",
            ),
        ];
        for (input, want, why) in table {
            assert_eq!(&aligned(input)[..], *want, "{why}");
        }
    }

    #[test]
    fn align_hunk_every_line_appears_exactly_once() {
        use LineKind::{Added as A, Context as C, Removed as R};
        let spec: Vec<(LineKind, &str)> = vec![
            (C, "0"),
            (R, "1"),
            (R, "2"),
            (A, "3"),
            (C, "4"),
            (A, "5"),
            (C, "6"),
            (R, "7"),
        ];
        let rows = aligned(&spec);
        let mut seen = vec![0usize; spec.len()];
        for (l, r) in &rows {
            if let Some(i) = l {
                seen[*i] += 1;
            }
            // 文脈行は左右に同じ添字が出るので二重に数えない
            if let Some(i) = r {
                if Some(*i) != *l {
                    seen[*i] += 1;
                }
            }
        }
        assert!(
            seen.iter().all(|&n| n == 1),
            "各行はちょうど 1 回だけ現れる: {seen:?}"
        );
    }

    // ---- word_diff: 語単位 (書記素クラスタ単位) ハイライト ------------------

    fn spans_text<'a>(s: &'a str, sp: &[WordSpan]) -> Vec<&'a str> {
        sp.iter().map(|w| &s[w.start..w.end]).collect()
    }

    #[test]
    fn word_diff_table() {
        // (旧, 新, 旧側の変更部分, 新側の変更部分, 何を見ているか)
        let table: &[(&str, &str, &[&str], &[&str], &str)] = &[
            ("same", "same", &[], &[], "同じ行は変更なし"),
            (
                "let a = 1;",
                "let a = 2;",
                &["1"],
                &["2"],
                "1 トークンだけ変わる",
            ),
            (
                "fn foo(x: i32)",
                "fn foo(x: u32)",
                &["i"],
                &["u"],
                "型名の変わった 1 文字だけ (語の中まで精査する)",
            ),
            (
                "let count = 0;",
                "let counter = 0;",
                &[],
                &["er"],
                "語の末尾に足した分だけ塗る",
            ),
            (
                "value_old",
                "value_new",
                &["old"],
                &["new"],
                "語の後半が総取り替えなら塗りは 1 かたまり",
            ),
            ("abc", "abcdef", &[], &["def"], "末尾に追加"),
            ("abcdef", "abc", &["def"], &[], "末尾を削除"),
            ("xyz", "", &["xyz"], &[], "行が空になる"),
            ("", "xyz", &[], &["xyz"], "空行に足す"),
            // --- 日本語 (空白が無いので書記素クラスタ単位で刻めないと壊れる) ---
            (
                "こんにちは世界",
                "こんばんは世界",
                &["にち"],
                &["ばん"],
                "日本語のみ: 変わった 2 文字だけ",
            ),
            (
                "値を取得する",
                "値を設定する",
                &["取得"],
                &["設定"],
                "日本語の語の入れ替え",
            ),
            (
                "日本語",
                "日本語です",
                &[],
                &["です"],
                "日本語の末尾に追加",
            ),
            // --- 絵文字 (サロゲート相当の 4 バイト文字・ZWJ・肌色修飾子) ---
            (
                "ok 🚀 go",
                "ok 🎉 go",
                &["🚀"],
                &["🎉"],
                "4 バイト絵文字の入れ替え",
            ),
            (
                "👨\u{200D}👩\u{200D}👧 family",
                "👩\u{200D}👩\u{200D}👦 family",
                &["👨\u{200D}👩\u{200D}👧"],
                &["👩\u{200D}👩\u{200D}👦"],
                "ZWJ 絵文字は 1 かたまりとして入れ替わる",
            ),
            (
                "👍\u{1F3FB}",
                "👍\u{1F3FF}",
                &["👍\u{1F3FB}"],
                &["👍\u{1F3FF}"],
                "肌色修飾子だけ違う = クラスタごと差し替え",
            ),
            (
                "か\u{3099}き",
                "かき",
                &["か\u{3099}"],
                &["か"],
                "結合濁点を割らない",
            ),
            (
                "🇯🇵 と 🇺🇸",
                "🇯🇵 と 🇫🇷",
                &["🇺🇸"],
                &["🇫🇷"],
                "国旗 (地域表示記号 2 つ) を割らない",
            ),
        ];
        for (old, new, wo, wn, why) in table {
            let (a, b) = word_diff(old, new).unwrap_or_else(|| panic!("{why}: 諦めてはいけない"));
            assert_eq!(spans_text(old, &a), *wo, "{why}: 旧側");
            assert_eq!(spans_text(new, &b), *wn, "{why}: 新側");
        }
    }

    #[test]
    fn word_diff_spans_are_valid_char_boundaries() {
        // 日本語・絵文字・ASCII が混ざった行でも、返る範囲は必ず文字境界で
        // 昇順・重なりなし (これを破ると `&s[a..b]` が panic する)。
        let old = "let 名前 = \"🚀 起動\"; // 旧";
        let new = "let 名前 = \"🎉 完了\"; // 新";
        let (a, b) = word_diff(old, new).expect("諦めない長さ");
        for (s, spans) in [(old, &a), (new, &b)] {
            let mut prev = 0usize;
            for w in spans.iter() {
                assert!(w.start < w.end, "空の範囲を返した");
                assert!(w.start >= prev, "範囲が重なっている / 逆順");
                assert!(s.is_char_boundary(w.start), "開始が文字境界でない");
                assert!(s.is_char_boundary(w.end), "終了が文字境界でない");
                let _ = &s[w.start..w.end]; // panic しないこと
                prev = w.end;
            }
        }
    }

    #[test]
    fn word_diff_gives_up_on_pathological_lines() {
        // 1. バイト長の上限
        let long_a = "a".repeat(WORD_DIFF_MAX_BYTES + 1);
        let long_b = "b".repeat(WORD_DIFF_MAX_BYTES + 1);
        assert!(
            word_diff(&long_a, &long_b).is_none(),
            "長すぎる行は語単位を諦める (行全体を塗る)"
        );
        // 2. DP のマス数の上限。共通接頭辞・接尾辞が無い別々のトークン列。
        let a: String = (0..1200).map(|i| format!("a{i} ")).collect();
        let b: String = (0..1200).map(|i| format!("b{i} ")).collect();
        assert!(a.len() <= WORD_DIFF_MAX_BYTES && b.len() <= WORD_DIFF_MAX_BYTES);
        assert!(
            word_diff(&a, &b).is_none(),
            "マス数が上限を超えたら諦める (O(n²) を走らせない)"
        );
        // 3. 上限すれすれは諦めない (閾値が厳しすぎて実用行まで殺していない)
        let c: String = (0..200).map(|i| format!("c{i} ")).collect();
        let d: String = (0..200).map(|i| format!("d{i} ")).collect();
        assert!(word_diff(&c, &d).is_some(), "実用サイズの行は語単位で出す");
    }

    #[test]
    fn word_diff_long_but_similar_lines_still_work() {
        // 前後がそっくりな長い行は、接頭辞・接尾辞を削った結果すぐ小さくなる。
        let head = "x".repeat(3000);
        let old = format!("{head} alpha {head}");
        let new = format!("{head} beta {head}");
        let (a, b) = word_diff(&old, &new).expect("接頭辞・接尾辞を削れば上限に当たらない");
        assert_eq!(spans_text(&old, &a), vec!["alpha"]);
        assert_eq!(spans_text(&new, &b), vec!["beta"]);
    }

    // ---- fold_context_runs: 未変更行の折りたたみ ---------------------------

    fn kinds_of(s: &str) -> Vec<LineKind> {
        s.chars()
            .map(|c| match c {
                '+' => LineKind::Added,
                '-' => LineKind::Removed,
                _ => LineKind::Context,
            })
            .collect()
    }

    #[test]
    fn fold_context_runs_table() {
        // (行種の並び, keep, 期待する畳む区間, 何を見ているか)
        let table: &[(&str, usize, &[(usize, usize)], &str)] = &[
            ("", 3, &[], "空"),
            ("...", 3, &[], "3 行の文脈は畳まない"),
            (".......", 3, &[], "7 行 = 2*keep+1 はまだ畳まない"),
            ("........", 3, &[(3, 5)], "8 行 = 2*keep+2 で 2 行畳む"),
            (
                "..........",
                3,
                &[(3, 7)],
                "10 行なら前後 3 行を残して 4 行畳む",
            ),
            ("+++", 3, &[], "変更行だけなら畳まない"),
            (
                "........+........",
                3,
                &[(3, 5), (12, 14)],
                "変更を挟んだ 2 つの塊をそれぞれ畳む",
            ),
            ("..", 0, &[(0, 2)], "keep=0 なら 2 行から畳む"),
            (".", 0, &[], "1 行は畳まない (⋯ の方が場所を食う)"),
            (
                "+........+",
                3,
                &[(4, 6)],
                "前後が変更行でも中央の文脈だけ畳む",
            ),
        ];
        for (spec, keep, want, why) in table {
            let got: Vec<(usize, usize)> = fold_context_runs(&kinds_of(spec), *keep)
                .into_iter()
                .map(|r| (r.start, r.end))
                .collect();
            assert_eq!(&got[..], *want, "{why} ({spec:?} keep={keep})");
        }
    }

    #[test]
    fn fold_context_runs_never_hides_a_changed_line() {
        let spec = "..+.....-.........+..";
        let kinds = kinds_of(spec);
        for run in fold_context_runs(&kinds, CONTEXT_KEEP) {
            for (i, kind) in kinds.iter().enumerate().take(run.end).skip(run.start) {
                assert_eq!(*kind, LineKind::Context, "変更行 {i} を畳もうとした");
            }
            assert!(run.len() >= 2, "1 行だけ畳むのは無意味");
        }
    }

    #[test]
    fn fold_covering_finds_the_hiding_run() {
        let runs = fold_context_runs(&kinds_of(".........."), 3);
        assert_eq!(runs.len(), 1);
        assert!(fold_covering(&runs, 2).is_none(), "残す行は隠れない");
        assert_eq!(fold_covering(&runs, 3).map(|r| r.start), Some(3));
        assert_eq!(fold_covering(&runs, 6).map(|r| r.start), Some(3));
        assert!(fold_covering(&runs, 7).is_none(), "区間は半開");
    }

    // ---- change_blocks / next_change_index: F7 のジャンプ ------------------

    #[test]
    fn change_blocks_finds_each_run_once() {
        let files = parse_unified(
            "diff --git a/a.rs b/a.rs\n\
             --- a/a.rs\n+++ b/a.rs\n\
             @@ -1,7 +1,7 @@\n \
             ctx0\n-old1\n+new1\n ctx1\n ctx2\n-old2\n+new2\n ctx3\n\
             diff --git a/b.rs b/b.rs\n\
             --- a/b.rs\n+++ b/b.rs\n\
             @@ -1,2 +1,3 @@\n ctx\n+added\n",
        );
        let blocks = change_blocks(&files);
        assert_eq!(
            blocks,
            vec![
                ChangeAnchor { file: 0, hunk: 0, line: 1 },
                ChangeAnchor { file: 0, hunk: 0, line: 5 },
                ChangeAnchor { file: 1, hunk: 0, line: 1 },
            ],
            "連続した追加/削除は 1 つの塊として数える"
        );
    }

    #[test]
    fn change_blocks_is_empty_without_changes() {
        let files = parse_unified(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n a\n b\n",
        );
        assert!(change_blocks(&files).is_empty(), "文脈行だけなら 0 件");
        assert!(change_blocks(&[]).is_empty(), "ファイルが無ければ 0 件");
    }

    #[test]
    fn next_change_index_table() {
        // (現在位置, 向き, 件数, 期待, 何を見ているか)
        let table: &[(Option<usize>, i32, usize, Option<usize>, &str)] = &[
            (None, 1, 0, None, "変更 0 件は前向きでも None"),
            (None, -1, 0, None, "変更 0 件は後ろ向きでも None"),
            (Some(0), 1, 0, None, "件数 0 なら位置があっても None"),
            (None, 1, 3, Some(0), "初回の F7 は先頭へ"),
            (None, -1, 3, Some(2), "初回の ⇧F7 は末尾へ"),
            (Some(0), 1, 3, Some(1), "次へ"),
            (Some(2), 1, 3, Some(0), "最後の次は先頭へ回り込む"),
            (Some(0), -1, 3, Some(2), "先頭の前は末尾へ回り込む"),
            (Some(1), -1, 3, Some(0), "前へ"),
            (Some(9), 1, 3, Some(0), "件数が減っていても範囲内へ丸める"),
            (Some(0), 0, 3, Some(1), "delta 0 は前向き扱い"),
            (None, 1, 1, Some(0), "1 件しかないとき"),
            (Some(0), 1, 1, Some(0), "1 件のときは自分へ戻る"),
            (Some(0), -1, 1, Some(0), "1 件のときは逆向きでも自分"),
        ];
        for &(cur, delta, len, want, why) in table {
            assert_eq!(next_change_index(cur, delta, len), want, "{why}");
        }
    }

    #[test]
    fn next_change_index_cycles_through_everything() {
        let n = 5;
        let mut cur = None;
        let mut seen = Vec::new();
        for _ in 0..n {
            cur = next_change_index(cur, 1, n);
            seen.push(cur.unwrap());
        }
        assert_eq!(seen, vec![0, 1, 2, 3, 4], "一巡で全部を通る");
        assert_eq!(next_change_index(cur, 1, n), Some(0), "そのあと先頭へ戻る");
    }

    // =======================================================================
    // 実描画 (ヘッドレス) — 並列表示の行揃えと、左右どちらにもコメントが打てること
    // =======================================================================

    /// 描かれた**テキスト**の矩形を本文で探す (`painter.galley` 経由の行本文)。
    fn galley_rect(shapes: &[egui::epaint::ClippedShape], needle: &str) -> Option<egui::Rect> {
        fn walk(s: &egui::Shape, needle: &str, out: &mut Option<egui::Rect>) {
            if out.is_some() {
                return;
            }
            match s {
                egui::Shape::Text(t) => {
                    if t.galley.job.text == needle {
                        *out = Some(t.visual_bounding_rect());
                    }
                }
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, needle, out)),
                _ => {}
            }
        }
        let mut out = None;
        for c in shapes {
            walk(&c.shape, needle, &mut out);
        }
        out
    }

    fn sbs_diff() -> Vec<FileDiff> {
        parse_unified(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n\
             @@ -1,3 +1,3 @@\n ctxline\n-oldline\n+newline\n",
        )
    }

    fn render(
        ctx: &egui::Context,
        theme: &Theme,
        files: &[FileDiff],
        store: &mut DiffCommentStore,
        size: (f32, f32),
        events: Vec<egui::Event>,
    ) -> Vec<egui::epaint::ClippedShape> {
        let raw = egui::RawInput {
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size.0, size.1),
            )),
            ..Default::default()
        };
        ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                diff_ui_with_actions(ui, theme, files, store);
            });
        })
        .shapes
    }

    fn click_at(at: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    #[test]
    fn side_by_side_pairs_sit_at_the_same_height() {
        let ctx = egui::Context::default();
        let theme = crate::theme::all()[0].clone();
        set_diff_mode(&ctx, DiffMode::SideBySide);
        let files = sbs_diff();
        let mut store = DiffCommentStore::default();
        // 1 フレーム目はレイアウト確定用 (折りたたみヘッダの高さが決まる)
        let _ = render(&ctx, &theme, &files, &mut store, (900.0, 700.0), Vec::new());
        let shapes = render(&ctx, &theme, &files, &mut store, (900.0, 700.0), Vec::new());

        let old = galley_rect(&shapes, "oldline").expect("旧側の行が描かれている");
        let new = galley_rect(&shapes, "newline").expect("新側の行が描かれている");
        assert!(
            (old.top() - new.top()).abs() < 0.5,
            "置換の対が同じ高さに並んでいない: {old:?} vs {new:?}"
        );
        let lay = diff_layout(900.0, DiffMode::SideBySide, ctx.pixels_per_point());
        let mid = lay.panes[1].x;
        assert!(old.left() < mid, "旧側が左ペインに無い ({} >= {mid})", old.left());
        assert!(new.left() >= mid, "新側が右ペインに無い ({} < {mid})", new.left());
        assert!(
            new.right() <= 900.0 + 0.5,
            "右ペインの本文が画面からはみ出した: {new:?}"
        );
    }

    #[test]
    fn side_by_side_takes_comments_on_both_sides() {
        let ctx = egui::Context::default();
        let theme = crate::theme::all()[0].clone();
        set_diff_mode(&ctx, DiffMode::SideBySide);
        let files = sbs_diff();
        let mut store = DiffCommentStore::default();
        let _ = render(&ctx, &theme, &files, &mut store, (900.0, 700.0), Vec::new());
        let shapes = render(&ctx, &theme, &files, &mut store, (900.0, 700.0), Vec::new());
        let old = galley_rect(&shapes, "oldline").expect("旧側の行");
        let new = galley_rect(&shapes, "newline").expect("新側の行");

        // 左 (旧側) をクリック → 旧側の行番号にコメントの下書きが開く
        let p = old.center();
        let _ = render(&ctx, &theme, &files, &mut store, (900.0, 700.0), click_at(p, true));
        let _ = render(&ctx, &theme, &files, &mut store, (900.0, 700.0), click_at(p, false));
        assert!(
            store.drafts.keys().any(|a| a.side == CommentSide::Old),
            "左 (変更前) の行にコメントが打てない: {:?}",
            store.drafts.keys().collect::<Vec<_>>()
        );

        // 右 (新側) をクリック → 新側の行番号にも打てる
        let p = new.center();
        let _ = render(&ctx, &theme, &files, &mut store, (900.0, 700.0), click_at(p, true));
        let _ = render(&ctx, &theme, &files, &mut store, (900.0, 700.0), click_at(p, false));
        assert!(
            store.drafts.keys().any(|a| a.side == CommentSide::New),
            "右 (変更後) の行にコメントが打てない: {:?}",
            store.drafts.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn narrow_view_degrades_to_inline_and_still_takes_comments() {
        let ctx = egui::Context::default();
        let theme = crate::theme::all()[0].clone();
        // 並列を選んでいても、320px では一列で描かれる
        set_diff_mode(&ctx, DiffMode::SideBySide);
        let files = sbs_diff();
        let mut store = DiffCommentStore::default();
        let _ = render(&ctx, &theme, &files, &mut store, (320.0, 640.0), Vec::new());
        let shapes = render(&ctx, &theme, &files, &mut store, (320.0, 640.0), Vec::new());
        let old = galley_rect(&shapes, "oldline").expect("旧側の行");
        let new = galley_rect(&shapes, "newline").expect("新側の行");
        assert!(
            (old.top() - new.top()).abs() > 1.0,
            "一列なら上下に並ぶはず: {old:?} / {new:?}"
        );
        assert!(old.left() < 320.0 && new.left() < 320.0, "本文が画面外");

        let p = new.center();
        let _ = render(&ctx, &theme, &files, &mut store, (320.0, 640.0), click_at(p, true));
        let _ = render(&ctx, &theme, &files, &mut store, (320.0, 640.0), click_at(p, false));
        assert_eq!(store.drafts.len(), 1, "一列でも行コメントが打てる");
    }

    /// 描かれた**塗り矩形**を全部集める。テキストは長い行が意図的に
    /// はみ出して (クリップされて) 描かれるので対象にしない。
    fn filled_rects(shapes: &[egui::epaint::ClippedShape]) -> Vec<egui::Rect> {
        fn walk(s: &egui::Shape, out: &mut Vec<egui::Rect>) {
            match s {
                egui::Shape::Rect(r) => out.push(r.rect),
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        for c in shapes {
            walk(&c.shape, &mut out);
        }
        out
    }

    #[test]
    fn every_painted_row_stays_inside_the_viewport() {
        // 極端なサイズで描いて、塗り矩形もクリップ矩形も画面の横幅を出ないこと。
        // 折りたたみ見出しの字下げぶんを勘定に入れ忘れると、並列の右ペインが
        // ここで画面外へ出る (実際にそう書いて落ちた)。
        for (w, h) in [(900.0f32, 700.0f32), (1200.0, 300.0), (320.0, 640.0), (700.0, 500.0)] {
            let ctx = egui::Context::default();
            let theme = crate::theme::all()[0].clone();
            set_diff_mode(&ctx, DiffMode::SideBySide);
            let files = sbs_diff();
            // コメントスレッドと操作バーも描かせる (枠の余白ぶん広がりやすい所)
            let mut store = DiffCommentStore::default();
            store.add(
                CommentAnchor::new("a.rs", CommentSide::New, 2),
                "newline",
                "ここを直して。長めの本文でも枠が広がってはいけない。",
            );
            store.toggle_draft(CommentAnchor::new("a.rs", CommentSide::Old, 2));
            let _ = render(&ctx, &theme, &files, &mut store, (w, h), Vec::new());
            let shapes = render(&ctx, &theme, &files, &mut store, (w, h), Vec::new());
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, h));
            for r in filled_rects(&shapes) {
                if !r.is_positive() {
                    continue;
                }
                assert!(
                    r.min.x >= screen.min.x - 0.5 && r.max.x <= screen.max.x + 0.5,
                    "{w}x{h}: 塗り矩形が横にはみ出した {r:?} (画面 {screen:?})"
                );
            }
            for c in &shapes {
                let r = c.clip_rect;
                if !r.is_positive() {
                    continue;
                }
                assert!(
                    r.min.x >= screen.min.x - 0.5 && r.max.x <= screen.max.x + 0.5,
                    "{w}x{h}: クリップ矩形が横にはみ出した {r:?}"
                );
            }
        }
    }

    #[test]
    fn side_by_side_panes_do_not_overlap_when_painted() {
        // 左ペインの塗りが右ペインの領域へ入らない (= 行が重ならない)。
        let (w, h) = (1000.0f32, 700.0f32);
        let ctx = egui::Context::default();
        let theme = crate::theme::all()[0].clone();
        set_diff_mode(&ctx, DiffMode::SideBySide);
        let files = sbs_diff();
        let mut store = DiffCommentStore::default();
        let _ = render(&ctx, &theme, &files, &mut store, (w, h), Vec::new());
        let shapes = render(&ctx, &theme, &files, &mut store, (w, h), Vec::new());
        let old = galley_rect(&shapes, "oldline").expect("旧側の行");
        let new = galley_rect(&shapes, "newline").expect("新側の行");
        assert!(
            old.right() <= new.left(),
            "左ペインの本文が右ペインへ食い込んだ: {old:?} / {new:?}"
        );
        // 溝ぶんは必ず空いている
        assert!(new.left() - old.right() >= PANE_GAP - 0.5);
    }

    #[test]
    fn f7_jumps_and_reports_when_there_is_nothing_to_jump_to() {
        let ctx = egui::Context::default();
        let theme = crate::theme::all()[0].clone();
        // 変更が 0 件の差分に F7 → 「変更はありません」を積むだけ
        let no_change = parse_unified(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n a\n b\n",
        );
        let mut store = DiffCommentStore::default();
        request_jump(&ctx, 1);
        let _ = render(&ctx, &theme, &no_change, &mut store, (900.0, 700.0), Vec::new());
        assert_eq!(
            take_pending_notice(&ctx).as_deref(),
            Some(tr("変更はありません").as_str()),
            "変更が無いときは静かに知らせるだけ"
        );
        assert!(take_pending_notice(&ctx).is_none(), "通知は 1 回きり");

        // 変更のある差分では通知を出さない (黙って飛ぶ)
        let files = sbs_diff();
        request_jump(&ctx, 1);
        let _ = render(&ctx, &theme, &files, &mut store, (900.0, 700.0), Vec::new());
        assert!(take_pending_notice(&ctx).is_none(), "飛べるときは黙って飛ぶ");
    }

    #[test]
    fn stale_jump_requests_are_dropped() {
        // 差分ビューが出ていないフレームで撃たれた F7 は、あとから暴発しない。
        let ctx = egui::Context::default();
        let theme = crate::theme::all()[0].clone();
        let files = sbs_diff();
        let mut store = DiffCommentStore::default();
        request_jump(&ctx, 1);
        // 差分を描かないフレームを 3 枚流す
        for _ in 0..3 {
            let _ = ctx.run(egui::RawInput::default(), |_| {});
        }
        let _ = render(&ctx, &theme, &files, &mut store, (900.0, 700.0), Vec::new());
        assert!(
            take_pending_notice(&ctx).is_none(),
            "古い依頼が生き残って何かを起こした"
        );
    }

    #[test]
    fn mode_toggle_is_drawn_once_per_pass_even_with_many_files() {
        // Git のレビュー画面は 1 ファイルずつ diff_ui を呼ぶ。
        // それでもモード切替ボタンは 1 個しか出ない。
        let ctx = egui::Context::default();
        let theme = crate::theme::all()[0].clone();
        set_diff_mode(&ctx, DiffMode::SideBySide);
        let files = sbs_diff();
        let mut store = DiffCommentStore::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 700.0),
            )),
            ..Default::default()
        };
        let shapes = ctx
            .run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    for i in 0..4 {
                        ui.push_id(i, |ui| {
                            diff_ui_with_actions(ui, &theme, &files, &mut store);
                        });
                    }
                });
            })
            .shapes;
        let label = DiffMode::SideBySide.label();
        let mut n = 0usize;
        fn count(s: &egui::Shape, needle: &str, n: &mut usize) {
            match s {
                egui::Shape::Text(t) => {
                    if t.galley.job.text.contains(needle) {
                        *n += 1;
                    }
                }
                egui::Shape::Vec(v) => v.iter().for_each(|s| count(s, needle, n)),
                _ => {}
            }
        }
        for c in &shapes {
            count(&c.shape, &label, &mut n);
        }
        assert_eq!(n, 1, "モード切替ボタンが {n} 個描かれた (1 個であるべき)");
    }


    // ---- DiffMode: config との往復 ----------------------------------------

    #[test]
    fn diff_mode_config_roundtrip() {
        for m in [DiffMode::Inline, DiffMode::SideBySide] {
            assert_eq!(DiffMode::from_config_str(m.config_str()), m);
            assert_eq!(m.toggled().toggled(), m);
            assert_ne!(m.toggled(), m);
            assert!(!m.label().is_empty());
        }
        assert_eq!(
            DiffMode::default(),
            DiffMode::SideBySide,
            "既定は並列 (狭いときだけ diff_layout が一列へ落とす)"
        );
        // 未知の値・空文字・大文字混じりは既定へ倒す (設定を壊しても動く)
        for s in ["", "  ", "SIDE_BY_SIDE", "なにか", "split"] {
            assert_eq!(DiffMode::from_config_str(s), DiffMode::SideBySide, "{s:?}");
        }
        for s in ["inline", "Inline", " UNIFIED ", "1"] {
            assert_eq!(DiffMode::from_config_str(s), DiffMode::Inline, "{s:?}");
        }
    }
}
