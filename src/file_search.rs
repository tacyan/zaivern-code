//! ワークスペース横断のテキスト検索 (VS Code の「ファイル間で検索」相当)。
//!
//! 検索本体はワーカースレッドで走り、結果は mpsc でまとめて UI へ返す。
//! ファイル一覧は app.rs が既に持つ `file_index` (⌘P 用の索引) を流用するので、
//! .gitignore などの除外規則は索引側と一致する。

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

/// 1 ヒット = (ファイル, 0-based 行番号, 行テキスト)。
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub path: PathBuf,
    pub line: usize,
    pub text: String,
}

pub const MAX_HITS: usize = 500;
/// これより大きいファイルは検索しない (バイナリ/生成物対策)。
const MAX_FILE_BYTES: u64 = 1_500_000;
/// 表示用スニペットの最大文字数。
const MAX_SNIPPET_CHARS: usize = 240;

/// 大文字小文字を無視した検索。`files` は絶対パスの一覧。
/// 返り値のチャネルに (ヒット一覧, 検索済みファイル数) が一度だけ届く。
pub fn spawn(files: Vec<PathBuf>, query: String) -> Receiver<(Vec<Hit>, usize)> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut hits = Vec::new();
        let mut scanned = 0usize;
        let needle = query.to_lowercase();
        'outer: for p in files {
            if std::fs::metadata(&p).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else { continue };
            if bytes.contains(&0) {
                continue; // バイナリ
            }
            let text = String::from_utf8_lossy(&bytes);
            scanned += 1;
            for (n, line) in text.lines().enumerate() {
                if line.to_lowercase().contains(&needle) {
                    hits.push(Hit {
                        path: p.clone(),
                        line: n,
                        text: snippet(line),
                    });
                    if hits.len() >= MAX_HITS {
                        break 'outer;
                    }
                }
            }
        }
        let _ = tx.send((hits, scanned));
    });
    rx
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zv-fsearch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn collect(files: Vec<PathBuf>, q: &str) -> (Vec<Hit>, usize) {
        spawn(files, q.to_string())
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("search thread result")
    }

    #[test]
    fn finds_case_insensitive_matches_with_line_numbers() {
        let dir = tmp("basic");
        let a = dir.join("a.txt");
        std::fs::write(&a, "hello\nWorld HELLO\nnope\n").unwrap();
        let (hits, scanned) = collect(vec![a.clone()], "hello");
        assert_eq!(scanned, 1);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0], Hit { path: a.clone(), line: 0, text: "hello".into() });
        assert_eq!(hits[1].line, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skips_binary_and_missing_files() {
        let dir = tmp("bin");
        let b = dir.join("b.bin");
        std::fs::write(&b, [0u8, 1, 2, b'h', b'i']).unwrap();
        let (hits, scanned) =
            collect(vec![b, Path::new("/nonexistent-zv/x.txt").to_path_buf()], "hi");
        assert!(hits.is_empty());
        assert_eq!(scanned, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn caps_hits_at_max() {
        let dir = tmp("cap");
        let c = dir.join("c.txt");
        std::fs::write(&c, "match\n".repeat(MAX_HITS + 50)).unwrap();
        let (hits, _) = collect(vec![c], "match");
        assert_eq!(hits.len(), MAX_HITS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snippet_trims_and_caps() {
        assert_eq!(snippet("   abc  "), "abc");
        let long = "あ".repeat(MAX_SNIPPET_CHARS + 10);
        let s = snippet(&long);
        assert!(s.chars().count() == MAX_SNIPPET_CHARS + 1 && s.ends_with('…'));
    }
}
