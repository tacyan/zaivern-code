//! テスト専用のヘルパ群。`#[cfg(test)]` でのみコンパイルされる。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// std::env::temp_dir() 配下に一意なディレクトリを自作する（HOME 非依存）。
///
/// `prefix` はモジュールごとの名前空間（例: `"zaivern-session-test"`）、
/// `tag` はテストごとの識別子。生成されるディレクトリ名は
/// `{prefix}-{tag}-{pid}-{nanos}-{counter}` となる。
///
/// カウンタは全モジュールで共有される。これは一意性を弱めない
/// （むしろモジュール間での値の重複が起きなくなる）。
pub fn unique_temp_dir(prefix: &str, tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "{}-{}-{}-{}-{}",
        prefix,
        tag,
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("create unique temp dir");
    dir
}
