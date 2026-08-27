//! パス絞り込み用の最小 glob。`include` / `exclude` がこれを使う。
//!
//! `*` (1 区画の中の任意) / `?` (1 文字) / `**` (0 個以上の区画) を解する。
//! `/` を含まないパターンは**どの区画にも当たる** (`tests` がどの階層の
//! `tests/` にも当たる) — gitignore と同じ直観に寄せてある。
//!
//! `ignore.rs` の gitignore 実装を使わないのは、こちらが**利用者が明示した
//! 絞り込み**を扱う層で、リポジトリの無視設定とは意味が違うため
//! (`.gitignore` に載っていても `--include` で明示されたら見に行く)。

/// 1 区画を 1 パターン区画に当てる (`*` と `?` だけ)。
fn seg_match(pat: &[char], s: &[char]) -> bool {
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut backtrack) = (usize::MAX, 0usize);
    while si < s.len() {
        if pi < pat.len() && (pat[pi] == '?' || pat[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < pat.len() && pat[pi] == '*' {
            star = pi;
            backtrack = si;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            backtrack += 1;
            si = backtrack;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}

fn match_segments(pat: &[Vec<char>], path: &[&str]) -> bool {
    if pat.is_empty() {
        return path.is_empty();
    }
    if pat[0].len() == 2 && pat[0][0] == '*' && pat[0][1] == '*' {
        for i in 0..=path.len() {
            if match_segments(&pat[1..], &path[i..]) {
                return true;
            }
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    let s: Vec<char> = path[0].chars().collect();
    seg_match(&pat[0], &s) && match_segments(&pat[1..], &path[1..])
}

/// `path` (相対・`/` か `\` 区切り) が `pattern` に当たるか。
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let norm = path.replace('\\', "/");
    let segs: Vec<&str> = norm
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let pat_norm = pattern.replace('\\', "/");
    let pat_norm = pat_norm.trim_start_matches("./");
    let pat: Vec<Vec<char>> = pat_norm
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().collect())
        .collect();
    if pat.is_empty() {
        return false;
    }
    if match_segments(&pat, &segs) {
        return true;
    }
    // `/` を含まないパターンは、どの 1 区画にも当たる
    if pat.len() == 1 {
        return segs
            .iter()
            .any(|s| seg_match(&pat[0], &s.chars().collect::<Vec<char>>()));
    }
    // 複数区画のパターンは、パスの途中からでも当たる
    // (`tests/**` は `crates/foo/tests/bar.rs` も外す)
    (1..segs.len()).any(|i| match_segments(&pat, &segs[i..]))
}

/// どれか 1 つでも当たるか。
pub fn any_match(patterns: &[String], path: &str) -> bool {
    patterns.iter().any(|p| glob_match(p, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_star_matches_any_depth() {
        assert!(glob_match("**/test/**", "src/test/foo.rs"));
        assert!(glob_match("**/test/**", "a/b/c/test/d/e.rs"));
        assert!(glob_match("**/test/**", "test/foo.rs"));
        assert!(!glob_match("**/test/**", "src/testing/foo.rs"));
    }

    #[test]
    fn star_stays_within_segment() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/a/main.rs"));
        assert!(glob_match("*_test.go", "pkg/http_test.go"));
    }

    #[test]
    fn bare_name_matches_any_segment() {
        assert!(glob_match("tests", "crates/foo/tests/bar.rs"));
        assert!(glob_match("node_modules", "node_modules/x/y.js"));
        assert!(!glob_match("tests", "crates/foo/src/bar.rs"));
    }

    #[test]
    fn prefix_pattern_matches_nested_root() {
        assert!(glob_match("tests/**", "tests/mcp.rs"));
        assert!(glob_match("tests/**", "crates/foo/tests/mcp.rs"));
        assert!(!glob_match("tests/**", "src/mcp.rs"));
    }

    #[test]
    fn question_mark_and_windows_paths() {
        assert!(glob_match("a?c.rs", "a-c.rs"));
        assert!(glob_match("**/test/**", "src\\test\\foo.rs"));
    }

    #[test]
    fn dir_paths_match_for_pruning() {
        assert!(glob_match("**/test/**", "src/test"));
        assert!(glob_match("target/**", "target"));
    }

    /// 空のパターン / 空のパス で panic しない。
    #[test]
    fn 空の入力で落ちない() {
        assert!(!glob_match("", "a/b"));
        assert!(!glob_match("*.rs", ""));
        assert!(!any_match(&[], "a/b"));
        assert!(any_match(&["*.rs".to_string()], "a/b.rs"));
    }

    /// `*` の連なりで指数時間にならない (バックトラックは 1 段だけ)。
    #[test]
    fn 星の連なりでも即座に返る() {
        let pat = "*".repeat(40) + "b";
        let path = "a".repeat(60);
        assert!(!glob_match(&pat, &path));
    }
}
