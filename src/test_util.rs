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

/// 置き去りになった古いテスト用ディレクトリを掃く。**1 時間に 1 回だけ**。
///
/// 多くのテストは後始末をしないので、`$TMPDIR` に `zaivern-*` が積み上がる
/// (実測: 3441 個 / 251MB)。それ自体は無駄なだけだが、**並列実行の速度に効く**。
/// `worktree_base` はリポジトリの隣を worktree の置き場にするため、テストが
/// 一時ディレクトリ直下にリポジトリを作ると worktree が共有の `$TMPDIR` 直下へ
/// 生まれ、エントリ数が膨れた共有ディレクトリで `git worktree add` を並列に
/// 撃つとディレクトリロックで取り合いになる。
///
/// ## 「1 プロセス 1 回」ではなく「1 時間に 1 回」である理由
///
/// nextest は**テスト 1 件につき 1 プロセス**を起こす (約 2900 プロセス)。
/// 「1 プロセス 1 回」にすると全プロセスが `$TMPDIR` を読み切ることになり、
/// しかも走行中はテスト自身がエントリを増やすので **O(n²)** になる。
/// 実測でこれが全体実行を数百秒へ膨らませ、git 系テストを slow-timeout へ
/// 追い込んだ (掃除のつもりが渋滞の原因だった)。
/// スタンプファイルの mtime を見て間引けば、各プロセスの負担は `stat` 1 回で済む。
///
/// **安全側の作り**:
/// * 消すのは `$TMPDIR` 直下の `zaivern-` で始まるディレクトリだけ
/// * **2 時間以上更新が無いものだけ** — 並走している別のテストプロセスの
///   作業ディレクトリを巻き込まないため
/// * スタンプの作成に失敗したら**掃除しない** (競合で二重に走らせない)
/// * 失敗は全部黙って無視する (掃除でテストを落とさない)
fn sweep_stale_dirs() {
    const STALE: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);
    const EVERY: std::time::Duration = std::time::Duration::from_secs(60 * 60);
    let tmp = std::env::temp_dir();
    let stamp = tmp.join(".zaivern-sweep-stamp");
    let now = std::time::SystemTime::now();
    // まだ新しいスタンプがあるなら何もしない (ここが全プロセスの通り道)。
    if let Ok(age) = stamp
        .metadata()
        .and_then(|m| m.modified())
        .and_then(|t| now.duration_since(t).map_err(std::io::Error::other))
    {
        if age < EVERY {
            return;
        }
    }
    // 先にスタンプを更新して、同時に走った他プロセスを弾く。
    if std::fs::write(&stamp, b"").is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&tmp) else {
        return;
    };
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
}
