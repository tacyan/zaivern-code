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
    sweep_stale_dirs();
    dir
}

/// 置き去りになった古いテスト用ディレクトリを 1 プロセスに 1 回だけ掃く。
///
/// 多くのテストは後始末をしないので、`$TMPDIR` に `zaivern-*` が積み上がる
/// (実測: 3441 個 / 251MB)。それ自体は無駄なだけだが、**並列実行の速度に効く**
/// のが問題だった。`worktree_base` はリポジトリの隣を worktree の置き場にするため、
/// テストが一時ディレクトリ直下にリポジトリを作ると worktree が共有の `$TMPDIR`
/// 直下へ生まれる。エントリ数が膨れた共有ディレクトリで `git worktree add` を
/// 並列に撃つとディレクトリロックで詰まり、**単独 2 秒のテストが 90 秒**を超えて
/// nextest の slow-timeout に当たり、実行全体が中断した。
///
/// 置き場そのものはテスト側 (`race` / `worktree` の `fixture_repo`) を
/// 一段深く掘って直したが、掃除もしておかないと同じ状態へ戻る。
///
/// **安全側の作り**:
/// * 消すのは `$TMPDIR` 直下の `zaivern-` で始まるディレクトリだけ
/// * **2 時間以上更新が無いものだけ** — 並走している別のテストプロセスの
///   作業ディレクトリを巻き込まないため (CI の 1 実行は数分で終わる)
/// * 失敗は全部黙って無視する (掃除でテストを落とさない)
fn sweep_stale_dirs() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        const STALE: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        let now = std::time::SystemTime::now();
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with("zaivern-") {
                continue;
            }
            let stale = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age > STALE);
            if stale && e.path().is_dir() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    });
}
