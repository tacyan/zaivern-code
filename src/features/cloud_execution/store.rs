//! `~/.zaivern/cloud/` の**置き場**と、書きかけを残さない置き換え (§39)。
//!
//! ```text
//! ~/.zaivern/cloud/
//! ├── providers.json   … Provider プロファイル (**秘密は入らない**。環境変数の名前だけ)
//! ├── targets.json     … 実行先の一覧と、いま何本走っているか
//! ├── jobs.json        … 仕事の記録 (基盤の状態だけ。エージェントの状態は持たない)
//! └── known_hosts      … Zaivern 専用の known_hosts (§15)
//! ```
//!
//! ## 書きかけを残さない
//!
//! [`write_atomic`] は **一時ファイルへ書く → fsync → rename** の順。
//! 途中で電源が落ちても「元のまま」か「新しい内容」のどちらかにしかならない。
//!
//! ## 読み書きの取り合い
//!
//! 同時実行枠 (`active_jobs`) は**読んで足して書く**ので、素朴に置き換えると
//! 2 つのインスタンスが同時に枠を取って上限を超える。[`with_targets`] が
//! ロックファイルで直列化する。
//!
//! **Windows は削除が *delete pending* を経る** ので、混んでいるときに
//! `create_new` が `AlreadyExists` ではなく `ACCESS_DENIED` を返す。
//! ここを「壊れている」と扱うと**いちばん混んでいるときにだけ**台帳が
//! 使えなくなるので、Windows でだけ取り合いとして待つ
//! (`lease.rs` が同じ罠を踏んで得た結論をそのまま使う)。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::model::{CloudError, ExecutionJob, ExecutionTarget};
use super::provider::ProviderProfile;

/// 保存形式の版。読めない版は**黙って捨てず**に理由を返す。
pub const FORMAT_VERSION: u32 = 1;

/// ロックを待つ上限。**進捗が見える限り延ばす**のではなく、ここは
/// 「短い臨界区間しか無い」ことが分かっているので固定でよい。
const LOCK_WAIT: Duration = Duration::from_millis(4000);
/// これより古いロックは持ち主が死んだものとして横取りする。
const LOCK_STALE: Duration = Duration::from_secs(60);

/// `~/.zaivern/cloud`。**`ZAIVERN_HOME` で差し替わる** ([`crate::config::zaivern_dir`])
/// ので、テストは実 `~/.zaivern` に触れない。
///
/// **テストでの差し替えはスレッドローカル**にしてある。`ZAIVERN_HOME` を
/// 書き換えると**同時に走っている他のテスト**まで巻き込む (このリポジトリは
/// カウンタで同じ罠を踏んだ)。スレッドごとに分けておけば、並列に走っても
/// 互いの置き場を壊さない。
pub fn cloud_dir() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(d) = test_dir_override() {
            return d;
        }
    }
    crate::config::zaivern_dir().join("cloud")
}

#[cfg(test)]
thread_local! {
    // `thread_local!` に `///` を付けない (rustdoc がマクロ展開の中身を
    // 文書化しないので unused_doc_comments で -D warnings が落ちる)。
    static TEST_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn test_dir_override() -> Option<PathBuf> {
    TEST_DIR.with(|d| d.borrow().clone())
}

/// テスト用の置き場を差し替える (このスレッドだけ)。
#[cfg(test)]
pub fn set_test_dir(dir: Option<PathBuf>) {
    TEST_DIR.with(|d| *d.borrow_mut() = dir);
}

/// Zaivern 専用の known_hosts (§15)。利用者の `~/.ssh/known_hosts` を汚さない。
pub fn known_hosts_path() -> PathBuf {
    cloud_dir().join("known_hosts")
}

pub fn providers_path() -> PathBuf {
    cloud_dir().join("providers.json")
}

pub fn targets_path() -> PathBuf {
    cloud_dir().join("targets.json")
}

pub fn jobs_path() -> PathBuf {
    cloud_dir().join("jobs.json")
}

/// 置き場を作る (無ければ)。**中身は作らない** — 空のファイルを置くと
/// 「設定済み」に見える。
pub fn ensure_dir() -> Result<PathBuf, CloudError> {
    let dir = cloud_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| CloudError::io(format!("{} を作れません: {e}", dir.display())))?;
    Ok(dir)
}

/// 保存されている実行先の一覧。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub targets: Vec<ExecutionTarget>,
}

/// 保存されている Provider プロファイルの一覧。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub providers: Vec<ProviderProfile>,
}

/// 保存されている仕事の記録。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub jobs: Vec<ExecutionJob>,
}

fn default_version() -> u32 {
    FORMAT_VERSION
}

/// 記録として残す仕事の数の上限。**無限に伸ばさない** (jobs.json が
/// 起動のたびに読まれるので、伸び続けると起動が遅くなる)。
pub const MAX_JOBS_KEPT: usize = 500;

// ───────────────────────── 読み書き ─────────────────────────

/// JSON を読む。**ファイルが無いのは失敗ではない** (まだ何も設定していない)。
fn read_json<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> Result<T, CloudError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(e) => {
            return Err(CloudError::io(format!(
                "{} を読めません: {e}",
                path.display()
            )))
        }
    };
    if raw.trim().is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(&raw).map_err(|e| {
        CloudError::config(format!(
            "{} を読めません ({e})。\n\
             壊れている場合は、そのファイルを退避してから作り直してください",
            path.display()
        ))
    })
}

/// 同じディレクトリへ一時ファイルを作って rename する原子的な書き込み。
///
/// **fsync してから rename する。** しないと、rename だけが先に見えて
/// 中身が空のファイルが残ることがある。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CloudError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)
        .map_err(|e| CloudError::io(format!("{} を作れません: {e}", dir.display())))?;
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cloud".to_string());
    let tmp = dir.join(format!(
        ".{stem}.zv-cloud-{}-{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));

    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        // 失敗しても続ける環境がある (ネットワーク FS 等)。順序の保証が
        // 効かないだけで、書けていないわけではない。
        let _ = f.sync_all();
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| CloudError::io(format!("{} を書けません: {e}", path.display())))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), CloudError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| CloudError::io(format!("JSON へ変換できません: {e}")))?;
    write_atomic(path, format!("{text}\n").as_bytes())
}

// ───────────────────────── 実行先 ─────────────────────────

pub fn load_targets() -> Result<Vec<ExecutionTarget>, CloudError> {
    Ok(read_json::<TargetFile>(&targets_path())?.targets)
}

pub fn save_targets(targets: &[ExecutionTarget]) -> Result<(), CloudError> {
    write_json(
        &targets_path(),
        &TargetFile {
            version: FORMAT_VERSION,
            targets: targets.to_vec(),
        },
    )
}

/// 読んで直して書く。**ロックで直列化する** (枠の取り合いがあるため)。
pub fn with_targets<T>(
    f: impl FnOnce(&mut Vec<ExecutionTarget>) -> T,
) -> Result<T, CloudError> {
    ensure_dir()?;
    let _guard = FileLock::acquire(&cloud_dir().join("targets.lock"))?;
    let mut targets = load_targets()?;
    let out = f(&mut targets);
    save_targets(&targets)?;
    Ok(out)
}

// ───────────────────────── Provider ─────────────────────────

pub fn load_providers() -> Result<Vec<ProviderProfile>, CloudError> {
    let list = read_json::<ProviderFile>(&providers_path())?.providers;
    // 秘密として伏せる環境変数の名前を、読んだ時点で登録する。
    // **ここで登録しないと、後から出るエラーで伏せ損ねる。**
    for p in &list {
        if !p.token_env.is_empty() {
            super::redact::register_secret_env(&p.token_env);
        }
    }
    Ok(list)
}

pub fn save_providers(providers: &[ProviderProfile]) -> Result<(), CloudError> {
    // **保存の直前に、秘密を持っていないことを確かめる** (§40)。
    // 「保存しないつもり」ではなく、保存する経路そのもので止める。
    for p in providers {
        p.assert_no_secret()?;
    }
    write_json(
        &providers_path(),
        &ProviderFile {
            version: FORMAT_VERSION,
            providers: providers.to_vec(),
        },
    )
}

// ───────────────────────── 仕事 ─────────────────────────

pub fn load_jobs() -> Result<Vec<ExecutionJob>, CloudError> {
    Ok(read_json::<JobFile>(&jobs_path())?.jobs)
}

pub fn save_jobs(jobs: &[ExecutionJob]) -> Result<(), CloudError> {
    let start = jobs.len().saturating_sub(MAX_JOBS_KEPT);
    write_json(
        &jobs_path(),
        &JobFile {
            version: FORMAT_VERSION,
            jobs: jobs[start..].to_vec(),
        },
    )
}

/// 1 件を足すか、同じ ID があれば置き換える。
pub fn upsert_job(job: &ExecutionJob) -> Result<(), CloudError> {
    ensure_dir()?;
    let _guard = FileLock::acquire(&cloud_dir().join("jobs.lock"))?;
    let mut jobs = load_jobs()?;
    match jobs.iter_mut().find(|j| j.id == job.id) {
        Some(slot) => *slot = job.clone(),
        None => jobs.push(job.clone()),
    }
    save_jobs(&jobs)
}

// ───────────────────────── ロック ─────────────────────────

/// 排他ロック。**持ち主が死んでも詰まらない** (古いロックは横取りする)。
pub struct FileLock {
    path: PathBuf,
}

impl FileLock {
    pub fn acquire(path: &Path) -> Result<Self, CloudError> {
        let deadline = std::time::Instant::now() + LOCK_WAIT;
        let mut wait = Duration::from_millis(2);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(e) if is_contended(&e) => {
                    if stale(path) {
                        // 持ち主が死んでいる。横取りして次の周で取り直す。
                        let _ = std::fs::remove_file(path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(CloudError::timeout(format!(
                            "{} のロックを取れません (他のインスタンスが握ったままかもしれません)",
                            path.display()
                        )));
                    }
                    std::thread::sleep(wait);
                    wait = (wait * 2).min(Duration::from_millis(64));
                }
                Err(e) => {
                    return Err(CloudError::io(format!(
                        "{} のロックを作れません: {e}",
                        path.display()
                    )))
                }
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// **取り合い**か、本物の失敗か。
///
/// Windows は削除が *delete pending* を経るので、混んでいるときに
/// `create_new` が `AlreadyExists` ではなく `ACCESS_DENIED` を返す。
/// unix の `PermissionDenied` は本物の権限問題なので即失敗のままにする。
pub fn is_contended(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::AlreadyExists {
        return true;
    }
    cfg!(windows) && e.raw_os_error() == Some(5)
}

fn stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() > LOCK_STALE)
        .unwrap_or(false)
}

/// いまの unix 時刻 (秒)。
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::cloud_execution::provider::{ProviderKind, ProviderProfile};
    use crate::features::cloud_execution::test_support::{home_guard, target, TargetOpts};

    #[test]
    fn store_atomic_round_trip() {
        let _home = home_guard("store-round-trip");
        // まだ何も無い状態でも失敗しない
        assert!(load_targets().expect("読める").is_empty());
        assert!(load_providers().expect("読める").is_empty());
        assert!(load_jobs().expect("読める").is_empty());

        let t = target("dev-01", TargetOpts::default());
        save_targets(std::slice::from_ref(&t)).expect("書ける");
        let back = load_targets().expect("読める");
        assert_eq!(back, vec![t.clone()], "書いたものがそのまま戻る");

        // 書きかけ (.tmp) が残っていない
        let leftovers: Vec<_> = std::fs::read_dir(cloud_dir())
            .expect("読める")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "書きかけが残っている: {leftovers:?}");
    }

    #[test]
    fn store_never_serializes_secrets() {
        let _home = home_guard("store-no-secrets");
        const TOKEN: &str = "super-secret-test-token";
        // 環境変数には本物らしい値が入っている状態で保存する
        let profile = ProviderProfile {
            name: "hetzner-eu".into(),
            kind: ProviderKind::Hetzner,
            token_env: "HCLOUD_TOKEN".into(),
            location: "fsn1".into(),
            server_type: "cx33".into(),
            image: "ubuntu-24.04".into(),
            ssh_key: "zaivern".into(),
            ssh_user: "zaivern".into(),
            max_jobs: 4,
            api_base: String::new(),
            identity_file: None,
        };
        save_providers(&[profile]).expect("書ける");

        // **保存の経路そのものが秘密を止めること。**
        // 「名前しか入れていないので出ない」だけを見ると、止める処理を
        // 外しても緑のままになる (実際にわざと外して確かめたら通ってしまった)。
        let carrying = ProviderProfile {
            name: "bad".into(),
            kind: ProviderKind::Hetzner,
            // 環境変数の**名前**の欄に、値そのものを入れてしまった状態
            token_env: TOKEN.into(),
            ..ProviderProfile::default()
        };
        let e = save_providers(&[carrying]).expect_err("断る");
        assert!(matches!(e, CloudError::Security(_)), "{e:?}");
        assert!(!format!("{e}").contains(TOKEN), "断る理由に値が出ている: {e}");
        // 断ったのだから、ファイルは書き換わっていない
        let raw = std::fs::read_to_string(providers_path()).expect("読める");
        assert!(!raw.contains("\"bad\""), "断ったのに書いている:\n{raw}");
        assert!(
            !raw.contains(TOKEN),
            "トークンの値が保存されている:\n{raw}"
        );
        // 名前は残る (doctor が「設定されているか」を言うのに要る)
        assert!(raw.contains("HCLOUD_TOKEN"), "名前まで消している:\n{raw}");
        assert!(!raw.to_lowercase().contains("bearer"), "{raw}");
    }

    #[test]
    fn 壊れたファイルは黙って捨てない() {
        let _home = home_guard("store-broken");
        ensure_dir().expect("作れる");
        std::fs::write(targets_path(), "{ this is not json").expect("書ける");
        let e = load_targets().expect_err("失敗する");
        // 「空だった」ことにして利用者の設定を消さない
        assert!(matches!(e, CloudError::Config(_)), "{e:?}");
        assert!(format!("{e}").contains("targets.json"), "{e}");
    }

    #[test]
    fn 読んで直して書くのは直列化される() {
        let _home = home_guard("store-lock");
        with_targets(|t| t.push(target("a", TargetOpts::default()))).expect("書ける");
        with_targets(|t| t.push(target("b", TargetOpts::default()))).expect("書ける");
        assert_eq!(load_targets().expect("読める").len(), 2);
        // ロックファイルは残さない
        assert!(!cloud_dir().join("targets.lock").exists());
    }

    #[test]
    fn ロックは取り合いだけを待つ() {
        use std::io::ErrorKind;
        assert!(is_contended(&std::io::Error::from(ErrorKind::AlreadyExists)));
        // unix の権限エラーは本物の失敗 (待っても直らない)
        let denied = std::io::Error::from(ErrorKind::PermissionDenied);
        assert_eq!(is_contended(&denied), false, "権限エラーを待ってしまう");
    }

    #[test]
    fn 仕事の記録は上限で切る() {
        let _home = home_guard("store-jobs-cap");
        use crate::features::cloud_execution::model::{
            ExecutionJob, ExecutionJobState, JobId, TargetId,
        };
        let jobs: Vec<ExecutionJob> = (0..MAX_JOBS_KEPT + 10)
            .map(|i| ExecutionJob {
                id: JobId::new(format!("j{i:04}")),
                target: TargetId::new("t"),
                state: ExecutionJobState::Succeeded,
                command: String::new(),
                workspace: None,
                result_ref: String::new(),
                started_unix: 0,
                ended_unix: 0,
                exit_code: Some(0),
                message: String::new(),
            })
            .collect();
        save_jobs(&jobs).expect("書ける");
        let back = load_jobs().expect("読める");
        assert_eq!(back.len(), MAX_JOBS_KEPT);
        // 残るのは新しいほう
        assert_eq!(back[0].id.as_str(), "j0010");
    }
}
