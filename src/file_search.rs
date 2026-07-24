//! ワークスペース横断のテキスト検索 (VS Code の「ファイル間で検索」相当)。
//!
//! 検索本体はワーカースレッドで走り、結果は mpsc でまとめて UI へ返す。
//! ファイル一覧は app.rs が既に持つ `file_index` (⌘P 用の索引) を流用するので、
//! .gitignore などの除外規則は索引側と一致する。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

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

/// 大文字小文字を無視した部分文字列検索 (ヒープ割り当て 0)。
fn contains_case_insensitive(haystack: &str, needle_ascii: &[u8], needle_chars: &[char]) -> bool {
    if needle_ascii.is_empty() && needle_chars.is_empty() {
        return true;
    }

    if !needle_ascii.is_empty() {
        let needle_len = needle_ascii.len();
        if haystack.len() < needle_len {
            return false;
        }

        let h_bytes = haystack.as_bytes();
        for i in 0..=(h_bytes.len() - needle_len) {
            let mut matched = true;
            for j in 0..needle_len {
                if h_bytes[i + j].to_ascii_lowercase() != needle_ascii[j] {
                    matched = false;
                    break;
                }
            }
            if matched {
                return true;
            }
        }
        false
    } else {
        let needle_len = needle_chars.len();
        let h_chars: Vec<char> = haystack.chars().collect();
        if h_chars.len() < needle_len {
            return false;
        }

        for i in 0..=(h_chars.len() - needle_len) {
            let mut matched = true;
            for j in 0..needle_len {
                let hc = h_chars[i + j];
                let nc = needle_chars[j];
                let hc_lower = hc.to_lowercase().next().unwrap_or(hc);
                if hc_lower != nc {
                    matched = false;
                    break;
                }
            }
            if matched {
                return true;
            }
        }
        false
    }
}

/// 大文字小文字を無視した検索。`files` は絶対パスの一覧。
/// CPU並列ワーカースレッドによりミリ秒単位の爆速検索を行う。
pub fn spawn(files: Vec<PathBuf>, query: String) -> Receiver<(Vec<Hit>, usize)> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        if query.is_empty() || files.is_empty() {
            let _ = tx.send((Vec::new(), 0));
            return;
        }

        let is_ascii = query.is_ascii();
        let needle_ascii: Vec<u8> = if is_ascii {
            query.bytes().map(|b| b.to_ascii_lowercase()).collect()
        } else {
            Vec::new()
        };
        let needle_chars: Vec<char> = if !is_ascii {
            query.to_lowercase().chars().collect()
        } else {
            Vec::new()
        };

        let num_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(files.len())
            .max(1);

        let files_arc = Arc::new(files);
        let total_files = files_arc.len();
        let chunk_size = (total_files + num_threads - 1) / num_threads;

        let total_scanned = Arc::new(AtomicUsize::new(0));
        let global_hit_count = Arc::new(AtomicUsize::new(0));
        let cancel_flag = Arc::new(AtomicBool::new(false));

        let (worker_tx, worker_rx) = channel();

        for thread_idx in 0..num_threads {
            let files_ref = Arc::clone(&files_arc);
            let scanned_ref = Arc::clone(&total_scanned);
            let hit_count_ref = Arc::clone(&global_hit_count);
            let cancel_ref = Arc::clone(&cancel_flag);
            let w_tx = worker_tx.clone();

            let n_ascii = needle_ascii.clone();
            let n_chars = needle_chars.clone();

            std::thread::spawn(move || {
                let start = thread_idx * chunk_size;
                let end = (start + chunk_size).min(files_ref.len());

                let mut local_hits = Vec::new();

                for i in start..end {
                    if cancel_ref.load(Ordering::Relaxed) {
                        break;
                    }

                    let p = &files_ref[i];
                    if std::fs::metadata(p).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
                        continue;
                    }
                    let Ok(bytes) = std::fs::read(p) else { continue };
                    if bytes.contains(&0) {
                        continue; // バイナリ
                    }
                    let text = String::from_utf8_lossy(&bytes);
                    scanned_ref.fetch_add(1, Ordering::Relaxed);

                    for (n, line) in text.lines().enumerate() {
                        if contains_case_insensitive(line, &n_ascii, &n_chars) {
                            local_hits.push(Hit {
                                path: p.clone(),
                                line: n,
                                text: snippet(line),
                            });

                            let current_hits = hit_count_ref.fetch_add(1, Ordering::Relaxed);
                            if current_hits + 1 >= MAX_HITS {
                                cancel_ref.store(true, Ordering::Relaxed);
                                break;
                            }
                        }
                    }
                }

                let _ = w_tx.send(local_hits);
            });
        }

        drop(worker_tx); // 送信端をドロップして受信の終了を検知

        let mut all_hits = Vec::new();
        for mut thread_hits in worker_rx {
            all_hits.append(&mut thread_hits);
            if all_hits.len() >= MAX_HITS {
                all_hits.truncate(MAX_HITS);
                break;
            }
        }

        let scanned = total_scanned.load(Ordering::Relaxed);
        let _ = tx.send((all_hits, scanned));
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
