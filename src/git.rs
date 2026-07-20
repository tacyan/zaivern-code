//! Git 連携モジュール。
//!
//! `git` CLI (std::process::Command) を用いてワークスペースのステータスと
//! 行単位の diff マークを取得する。git が無い場合や workspace が
//! git リポジトリでない場合は、すべて空 / None を返す。

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// status --porcelain=v1 から得たファイル単位のステータス。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileStatus {
    Modified,
    Added,
    Untracked,
    Deleted,
    Renamed,
}

/// エディタのガター等に表示する行マーク。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineMark {
    Added,
    Modified,
}

const STATUS_CACHE_TTL: Duration = Duration::from_secs(2);

pub struct Git {
    workspace: PathBuf,
    /// 相対パス → ステータス (status --porcelain=v1 のパース結果)。
    status_cache: HashMap<String, FileStatus>,
    /// 最後に status を実行した時刻。None なら未実行。
    last_refresh: Option<Instant>,
    /// 相対パス → (text_hash, 行マーク) のキャッシュ。
    marks_cache: HashMap<String, (u64, Vec<(usize, LineMark)>)>,
}

impl Git {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            status_cache: HashMap::new(),
            last_refresh: None,
            marks_cache: HashMap::new(),
        }
    }

    pub fn set_workspace(&mut self, ws: PathBuf) {
        if self.workspace != ws {
            self.workspace = ws;
            self.status_cache.clear();
            self.marks_cache.clear();
            self.last_refresh = None;
        }
    }

    /// 2 秒キャッシュ付きで `git -C <ws> status --porcelain=v1` を実行しパースする。
    pub fn refresh_if_stale(&mut self) {
        if let Some(t) = self.last_refresh {
            if t.elapsed() < STATUS_CACHE_TTL {
                return;
            }
        }
        // 失敗時 (git 無し / 非 repo) も時刻は更新し、毎フレーム再実行しない。
        self.last_refresh = Some(Instant::now());
        match self.run_git(&["status", "--porcelain=v1"]) {
            Some(out) => self.status_cache = parse_porcelain_status(&out),
            None => self.status_cache.clear(),
        }
    }

    /// 相対パスのステータス (refresh_if_stale 済み前提、キャッシュから)。
    pub fn file_status(&self, rel_path: &str) -> Option<FileStatus> {
        self.status_cache.get(rel_path).copied()
    }

    /// 変更ファイル数 (status のエントリ数)。
    pub fn dirty_count(&self) -> usize {
        self.status_cache.len()
    }

    /// 指定ファイルの 0-based 行番号 → LineMark。
    /// `text_hash` が前回と同一ならキャッシュを返し、git は再実行しない。
    pub fn line_marks(&mut self, rel_path: &str, text_hash: u64) -> Vec<(usize, LineMark)> {
        if let Some((hash, marks)) = self.marks_cache.get(rel_path) {
            if *hash == text_hash {
                return marks.clone();
            }
        }
        let marks = self
            .run_git(&["diff", "--unified=0", "--", rel_path])
            .map(|out| parse_hunk_marks(&out))
            .unwrap_or_default();
        self.marks_cache
            .insert(rel_path.to_string(), (text_hash, marks.clone()));
        marks
    }

    /// `git -C <workspace> <args>` を実行。git 不在・非 repo・失敗時は None。
    fn run_git(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.workspace)
            .args(args)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout).ok()
    }
}

/// `status --porcelain=v1` の出力をパースする。
fn parse_porcelain_status(output: &str) -> HashMap<String, FileStatus> {
    let mut map = HashMap::new();
    for line in output.lines() {
        // 形式: "XY <path>" / "XY <orig> -> <path>" (XY は ASCII 2 文字 + 空白 1)
        if line.len() < 4 || !line.is_char_boundary(2) {
            continue;
        }
        let code = &line[..2];
        let mut path = line[3..].trim();
        if let Some((_, new_path)) = path.split_once(" -> ") {
            path = new_path;
        }
        let path = path.trim_matches('"');
        let status = if code == "??" {
            FileStatus::Untracked
        } else if code.contains('R') || code.contains('C') {
            FileStatus::Renamed
        } else if code.contains('A') {
            FileStatus::Added
        } else if code.contains('D') {
            FileStatus::Deleted
        } else if code.contains('M') || code.contains('T') || code.contains('U') {
            FileStatus::Modified
        } else {
            continue;
        };
        map.insert(path.to_string(), status);
    }
    map
}

/// `+c,d` / `-a,b` / `+c` (カウント省略 = 1) を (start, count) にパースする。
fn parse_range(token: &str) -> Option<(usize, usize)> {
    let body = token
        .strip_prefix('+')
        .or_else(|| token.strip_prefix('-'))?;
    let mut parts = body.splitn(2, ',');
    let start: usize = parts.next()?.trim().parse().ok()?;
    let count: usize = match parts.next() {
        Some(c) => c.trim().parse().ok()?,
        None => 1,
    };
    Some((start, count))
}

/// diff 出力中のハンクヘッダ `@@ -a,b +c,d @@` をパースし、
/// 0-based 行番号 → LineMark の一覧を返す純関数。
///
/// - b == 0            → 新ファイル側 c..c+d 行が Added
/// - b > 0 && d > 0    → 新ファイル側 c..c+d 行が Modified
/// - d == 0 (削除のみ) → マークなし
///
/// diff の +c は 1-based なので 0-based へ正規化する。b / d 省略時は 1。
pub fn parse_hunk_marks(diff_output: &str) -> Vec<(usize, LineMark)> {
    let mut marks = Vec::new();
    for line in diff_output.lines() {
        if !line.starts_with("@@") {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let _at = tokens.next(); // "@@"
        let (old_tok, new_tok) = match (tokens.next(), tokens.next()) {
            (Some(o), Some(n)) if o.starts_with('-') && n.starts_with('+') => (o, n),
            _ => continue,
        };
        let Some((_a, b)) = parse_range(old_tok) else {
            continue;
        };
        let Some((c, d)) = parse_range(new_tok) else {
            continue;
        };
        if d == 0 {
            // 削除のみ: 新ファイル側に対応行が無いためマークしない。
            continue;
        }
        let mark = if b == 0 {
            LineMark::Added
        } else {
            LineMark::Modified
        };
        let start = c.saturating_sub(1); // 1-based → 0-based
        for i in 0..d {
            marks.push((start + i, mark));
        }
    }
    marks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_only_hunk() {
        // 旧 10 行目の後に 3 行追加 → 新ファイル 1-based 11..13 → 0-based 10..12
        let out = parse_hunk_marks("@@ -10,0 +11,3 @@");
        assert_eq!(
            out,
            vec![
                (10, LineMark::Added),
                (11, LineMark::Added),
                (12, LineMark::Added),
            ]
        );
    }

    #[test]
    fn modified_hunk() {
        // 2 行変更 → 1-based 5..6 → 0-based 4..5
        let out = parse_hunk_marks("@@ -5,2 +5,2 @@ fn main()");
        assert_eq!(out, vec![(4, LineMark::Modified), (5, LineMark::Modified)]);
    }

    #[test]
    fn deleted_only_hunk_yields_no_marks() {
        // d == 0 (削除のみ) はマークなし
        let out = parse_hunk_marks("@@ -7,3 +6,0 @@");
        assert!(out.is_empty());
    }

    #[test]
    fn multiple_hunks_with_diff_noise() {
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1234567..89abcde 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,1 +1,1 @@
-old line
+new line
@@ -4,0 +5,2 @@
+added one
+added two
@@ -20,2 +21,0 @@
-gone
-gone too
";
        let out = parse_hunk_marks(diff);
        assert_eq!(
            out,
            vec![
                (0, LineMark::Modified),
                (4, LineMark::Added),
                (5, LineMark::Added),
            ]
        );
    }

    #[test]
    fn omitted_counts_default_to_one() {
        // "@@ -3 +3 @@": b, d とも省略 = 1 → 0-based 2 が Modified
        let out = parse_hunk_marks("@@ -3 +3 @@");
        assert_eq!(out, vec![(2, LineMark::Modified)]);
        // "@@ -0,0 +1 @@": b == 0, d 省略 = 1 → 0-based 0 が Added
        let out = parse_hunk_marks("@@ -0,0 +1 @@");
        assert_eq!(out, vec![(0, LineMark::Added)]);
    }

    #[test]
    fn non_hunk_lines_and_garbage_ignored() {
        assert!(parse_hunk_marks("").is_empty());
        assert!(parse_hunk_marks("hello world\n+not a hunk\n@@ broken @@").is_empty());
    }

    #[test]
    fn parse_porcelain_status_variants() {
        // 注意: 行頭の空白が porcelain フォーマットの一部なので \n エスケープで記述
        let out = " M src/app.rs\nA  src/new.rs\n?? notes.txt\n D gone.rs\nR  old.rs -> renamed.rs\n";
        let map = parse_porcelain_status(out);
        assert_eq!(map.get("src/app.rs"), Some(&FileStatus::Modified));
        assert_eq!(map.get("src/new.rs"), Some(&FileStatus::Added));
        assert_eq!(map.get("notes.txt"), Some(&FileStatus::Untracked));
        assert_eq!(map.get("gone.rs"), Some(&FileStatus::Deleted));
        assert_eq!(map.get("renamed.rs"), Some(&FileStatus::Renamed));
        assert_eq!(map.len(), 5);
    }
}
