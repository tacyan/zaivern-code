//! 担当単位の隔離。Git が使える場合は linked worktree、使えない場合は作業コピー。
use super::model::TeamTask;
use std::path::{Component, Path, PathBuf};

pub const ROOT: &str = ".zai-team-worktrees";

pub fn scope(files: &[String]) -> Option<&str> {
    if files.len() != 1 {
        return None;
    }
    let value = files[0].strip_suffix("/**")?;
    let parts: Vec<_> = value.split('/').collect();
    if parts.len() != 3
        || parts[0] != ROOT
        || !super::outbox::valid_run_id(parts[1])
        || !parts[2]
            .strip_prefix("part-")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    Some(value)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Ready {
    source: PathBuf,
    relative: PathBuf,
    git: bool,
    #[serde(default)]
    baseline: Option<Snapshot>,
    #[serde(default)]
    excluded: std::collections::BTreeSet<PathBuf>,
}

pub(super) fn checked_root(source: &Path, scope: &str) -> Result<PathBuf, String> {
    let mut path = source.to_path_buf();
    for part in Path::new(scope).components() {
        let Component::Normal(name) = part else {
            return Err("隔離先の相対パスが不正です".into());
        };
        path.push(name);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!(
                "隔離先にシンボリックリンクがあります: {}",
                path.display()
            ));
        }
    }
    Ok(path)
}

pub fn execution(
    source: &Path,
    files: &[String],
) -> Result<Option<(PathBuf, String, bool)>, String> {
    let Some(scope) = scope(files) else {
        return Ok(None);
    };
    let source = source
        .canonicalize()
        .map(crate::pathx::plain)
        .map_err(|e| e.to_string())?;
    let root = checked_root(&source, scope)?;
    let record: Ready = serde_json::from_slice(
        &std::fs::read(root.with_extension("ready.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    if record.source != source
        || record.relative.is_absolute()
        || record
            .relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("隔離先と元フォルダの対応が不正です".into());
    }
    let workspace = root.join(&record.relative);
    let canonical = workspace
        .canonicalize()
        .map(crate::pathx::plain)
        .map_err(|e| e.to_string())?;
    if !canonical.starts_with(&root) {
        return Err("隔離先の外へ出るパスです".into());
    }
    let relative = canonical
        .strip_prefix(&source)
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .replace('\\', "/");
    Ok(Some((canonical, relative, record.git)))
}

pub fn ready(source: &Path, tasks: &[TeamTask]) -> bool {
    tasks
        .iter()
        .all(|t| scope(&t.files).is_none() || execution(source, &t.files).is_ok())
}

pub(super) fn skipped(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            ".git"
                | ".zai-team-worktrees"
                | ".zai-team-parts"
                | "node_modules"
                | "target"
                | ".venv"
        )
    )
}

// Freeze exclusions once: only Git-ignored, untracked generated directories.
// Ignored source/config/assets remain eligible; without Git retain everything
// except the pre-existing internal/dependency exclusions above.
fn generated_directory(name: &str) -> bool {
    // The context list also includes IDE/config/vendor directories: those are
    // not disposable publication artifacts even when ignored by Git.
    (crate::context::walk::SKIP_DIRS.contains(&name)
        && matches!(
            name,
            "dist" | "build" | "out" | ".next" | ".nuxt" | ".cache" | "coverage" | "__pycache__"
        ))
        || name == "target-msrv"
}

fn exclusions(source: &Path) -> std::collections::BTreeSet<PathBuf> {
    let mut out = std::collections::BTreeSet::new();
    let Ok(tracked) = crate::worktree::git_out(source, &["ls-files", "-z", "--cached"]) else {
        return out;
    };
    let mut candidates: std::collections::BTreeSet<PathBuf> = crate::context::walk::SKIP_DIRS
        .iter()
        .filter(|name| generated_directory(name))
        .map(PathBuf::from)
        .collect();
    candidates.insert(PathBuf::from("target-msrv"));
    if let Ok(ignored) = crate::worktree::git_out(
        source,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
    ) {
        for path in ignored.split('\0').filter(|p| !p.is_empty()) {
            for ancestor in Path::new(path).ancestors() {
                if ancestor
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(generated_directory)
                {
                    candidates.insert(ancestor.to_path_buf());
                }
            }
        }
    }
    let candidates: Vec<_> = candidates
        .into_iter()
        .filter(|path| {
            !tracked
                .split('\0')
                .any(|p| !p.is_empty() && Path::new(p).starts_with(path))
        })
        .map(|path| format!("{}/", path.to_string_lossy().replace('\\', "/")))
        .collect();
    // Batch subprocesses. Unrepresentable/quoted output is conservatively retained.
    for batch in candidates.chunks(128) {
        let mut args = vec!["-c", "core.quotePath=false", "check-ignore", "--"];
        args.extend(batch.iter().map(String::as_str));
        if let Ok(ignored) = crate::worktree::git_out(source, &args) {
            for path in ignored
                .lines()
                .filter(|p| batch.iter().any(|candidate| candidate == p))
            {
                out.insert(PathBuf::from(path.trim_end_matches('/')));
            }
        }
    }
    out
}

fn excluded(root: &Path, path: &Path, exclusions: &std::collections::BTreeSet<PathBuf>) -> bool {
    path.strip_prefix(root).is_ok_and(|relative| {
        exclusions.iter().any(|p| {
            relative.starts_with(p)
                && (relative != p || std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()))
        })
    })
}

// 新規隔離先へ現状を反映する。hard link は使わず、編集が元ファイルへ波及しないようにする。
fn copy_current(
    source: &Path,
    destination: &Path,
    count: &mut usize,
    root: &Path,
    exclusions: &std::collections::BTreeSet<PathBuf>,
) -> Result<(), String> {
    if std::fs::symlink_metadata(destination)
        .is_ok_and(|m| m.file_type().is_symlink() || !m.is_dir())
    {
        std::fs::remove_file(destination).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(destination).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if skipped(&entry.file_name()) {
            continue;
        }
        // Staged deletions can still exist in the detached HEAD checkout.
        // Remove excluded artifacts there too, keeping copy and scans aligned.
        if excluded(root, &source.join(entry.file_name()), exclusions)
            || std::fs::symlink_metadata(source.join(entry.file_name())).is_err()
        {
            let ty = entry.file_type().map_err(|e| e.to_string())?;
            if ty.is_dir() {
                std::fs::remove_dir_all(entry.path())
            } else {
                std::fs::remove_file(entry.path())
            }
            .map_err(|e| e.to_string())?;
        }
    }
    for entry in std::fs::read_dir(source).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        if skipped(&name)
            || excluded(root, &entry.path(), exclusions)
            || (source.file_name().is_some_and(|n| n == ".claude") && name == "worktrees")
        {
            continue;
        }
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        // 外部リンクや循環は追わない。必要な依存は担当が元の環境を読み取り参照する。
        if ty.is_symlink() {
            // A tracked regular file may have become a link in the open folder.
            // Do not leave its stale HEAD content in the detached worktree.
            let to = destination.join(&name);
            if let Ok(meta) = std::fs::symlink_metadata(&to) {
                if meta.is_dir() {
                    std::fs::remove_dir_all(&to)
                } else {
                    std::fs::remove_file(&to)
                }
                .map_err(|e| e.to_string())?;
            }
            continue;
        }
        *count += 1;
        if *count > 100_000 {
            return Err("隔離コピーの対象が10万件を超えました".into());
        }
        let to = destination.join(name);
        if ty.is_dir() {
            copy_current(&entry.path(), &to, count, root, exclusions)?;
        } else if ty.is_file() {
            if std::fs::symlink_metadata(&to).is_ok_and(|m| m.file_type().is_symlink()) {
                std::fs::remove_file(&to).map_err(|e| e.to_string())?;
            }
            if to.is_dir() {
                std::fs::remove_dir_all(&to).map_err(|e| e.to_string())?;
            }
            std::fs::copy(entry.path(), &to)
                .map_err(|e| format!("隔離コピーに失敗: {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

pub fn prepare(source: &Path, tasks: &[TeamTask]) -> Result<(), String> {
    let source = source
        .canonicalize()
        .map(crate::pathx::plain)
        .map_err(|e| e.to_string())?;
    if tasks.iter().all(|task| scope(&task.files).is_none()) {
        return Ok(());
    }
    let repository = crate::git::discover_toplevel(&source);
    let head = crate::worktree::git_out(&source, &["rev-parse", "--verify", "HEAD^{commit}"]).ok();
    // One shared isolation point. On interrupted preparation, keep the original
    // baseline rather than silently rebasing older workers onto newer user edits.
    let existing = tasks
        .iter()
        .find(|task| execution(&source, &task.files).is_ok_and(|v| v.is_some()));
    let excluded = match existing {
        Some(task) => read_ready(&source, &task.files)?.excluded,
        None => exclusions(&source),
    };
    let initial = match existing {
        Some(task) => baseline(&source, &task.files)?,
        None => snapshot(&source, &excluded)?
            .into_iter()
            .filter(|(_, e)| matches!(e, Entry::File { .. }))
            .collect(),
    };
    for task in tasks {
        let Some(scope) = scope(&task.files) else {
            continue;
        };
        if execution(&source, &task.files).is_ok() {
            continue;
        }
        let root = checked_root(&source, scope)?;
        std::fs::create_dir_all(root.parent().ok_or("隔離先の親がありません")?)
            .map_err(|e| e.to_string())?;
        let (git, relative) = if let (Some(repo), Some(head)) = (&repository, &head) {
            if !root.exists() {
                crate::worktree::git_out(
                    repo,
                    &[
                        "worktree",
                        "add",
                        "--detach",
                        &root.to_string_lossy(),
                        head.trim(),
                    ],
                )?;
            }
            let actual =
                crate::git::discover_toplevel(&root).ok_or("隔離 worktree を作成できません")?;
            if actual != root || !root.join(".git").is_file() {
                return Err("既存フォルダは隔離 worktree ではありません".into());
            }
            (
                true,
                source
                    .strip_prefix(repo)
                    .map_err(|e| e.to_string())?
                    .to_path_buf(),
            )
        } else {
            // Git 未導入・リポジトリ未作成・初回コミット前でも実装を開始できる。
            (false, PathBuf::new())
        };
        copy_current(&source, &root.join(&relative), &mut 0, &source, &excluded)?;
        if snapshot(&root.join(&relative), &excluded)? != initial {
            return Err("隔離準備中に元フォルダが更新されました。以前の担当内容を保護するため準備を停止しました".into());
        }
        let metadata = serde_json::to_vec(&Ready {
            source: source.clone(),
            baseline: Some(initial.clone()),
            excluded: excluded.clone(),
            relative,
            git,
        })
        .map_err(|e| e.to_string())?;
        let temporary = root.with_extension("ready.tmp");
        std::fs::write(&temporary, metadata).map_err(|e| e.to_string())?;
        std::fs::rename(temporary, root.with_extension("ready.json")).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Immutable content baseline, including links so a removed link cannot become a deletion.
pub(super) type Snapshot = std::collections::BTreeMap<String, Entry>;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum Entry {
    File {
        fingerprint: super::changeset::Fingerprint,
        executable: bool,
        // None identifies legacy baselines; never infer old Unix permissions.
        #[serde(default)]
        mode: Option<u32>,
    },
    Link(PathBuf),
}

pub(super) fn file_entry(bytes: &[u8], metadata: &std::fs::Metadata) -> Entry {
    entry_with_fingerprint(
        super::changeset::Fingerprint {
            hash: crate::history::fnv1a64(bytes),
            len: bytes.len() as u64,
        },
        metadata,
    )
}

fn entry_with_fingerprint(
    fingerprint: super::changeset::Fingerprint,
    metadata: &std::fs::Metadata,
) -> Entry {
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode() & 0o7777)
    };
    #[cfg(not(unix))]
    let mode = {
        let _ = metadata;
        None
    };
    Entry::File {
        fingerprint,
        executable: mode.is_some_and(|m| m & 0o111 != 0),
        mode,
    }
}

/// Hash with bounded memory, independently of the publication byte budget.
pub(super) fn streamed_entry(path: &Path) -> Result<Entry, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    let mut hash = crate::history::Fnv1a64::default();
    let mut len = 0;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        len += n as u64;
    }
    Ok(entry_with_fingerprint(
        super::changeset::Fingerprint {
            hash: hash.finish(),
            len,
        },
        &meta,
    ))
}

pub(super) fn snapshot(
    root: &Path,
    exclusions: &std::collections::BTreeSet<PathBuf>,
) -> Result<Snapshot, String> {
    fn walk(
        root: &Path,
        dir: &Path,
        exclusions: &std::collections::BTreeSet<PathBuf>,
        out: &mut Snapshot,
    ) -> Result<(), String> {
        for item in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
            let item = item.map_err(|e| e.to_string())?;
            let path = item.path();
            if skipped(&item.file_name())
                || excluded(root, &path, exclusions)
                || (dir.file_name().is_some_and(|n| n == ".claude")
                    && item.file_name() == "worktrees")
            {
                continue;
            }
            let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if meta.is_dir() {
                walk(root, &path, exclusions, out)?;
                continue;
            }
            #[cfg(not(windows))]
            if item.file_name().to_string_lossy().contains('\\') {
                return Err("区切り文字を含むファイル名は安全に統合できません".into());
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_str()
                .ok_or("ファイル名を表現できません")?
                .replace('\\', "/");
            let entry = if meta.is_symlink() {
                Entry::Link(std::fs::read_link(&path).map_err(|e| e.to_string())?)
            } else if meta.is_file() {
                streamed_entry(&path)?
            } else {
                return Err(format!("通常ファイルではありません: {relative}"));
            };
            if out.len() >= 100_000 {
                return Err("統合ファイル数の上限を超えました".into());
            }
            out.insert(relative, entry);
        }
        Ok(())
    }
    let mut out = Snapshot::new();
    walk(root, root, exclusions, &mut out)?;
    Ok(out)
}

/// Keep only changed bytes, and compare the frozen bytes to the scanned entry.
/// Failure happens before publication, never as a false empty difference.
pub(super) fn frozen_files(
    root: &Path,
    source: &Path,
    files: &[String],
    baseline: &Snapshot,
) -> Result<std::collections::BTreeMap<String, (Entry, Vec<u8>)>, String> {
    use std::io::Read;
    let scanned = snapshot(root, &read_ready(source, files)?.excluded)?;
    let mut out = std::collections::BTreeMap::new();
    let mut budget = super::changeset::MAX_HASH_BYTES;
    for (relative, entry) in scanned {
        let mut bytes = Vec::new();
        if baseline.get(&relative) != Some(&entry) && matches!(entry, Entry::File { .. }) {
            let path = checked_root(root, &relative)?;
            let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
            let meta = file.metadata().map_err(|e| e.to_string())?;
            if meta.len() > budget {
                return Err("統合の変更内容が読み取り上限64MiBを超えたため保留しました".into());
            }
            file.take(budget + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            if bytes.len() as u64 > budget {
                return Err("統合の変更内容が読み取り上限64MiBを超えたため保留しました".into());
            }
            if file_entry(&bytes, &meta) != entry {
                return Err(format!("統合候補の取得中に更新されました: {relative}"));
            }
            budget -= bytes.len() as u64;
        }
        out.insert(relative, (entry, bytes));
    }
    Ok(out)
}

fn read_ready(source: &Path, files: &[String]) -> Result<Ready, String> {
    let scope = scope(files).ok_or("隔離タスクではありません")?;
    let root = checked_root(source, scope)?;
    serde_json::from_slice(
        &std::fs::read(root.with_extension("ready.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

pub(super) fn baseline(source: &Path, files: &[String]) -> Result<Snapshot, String> {
    let baseline = read_ready(source, files)?.baseline.ok_or_else(|| {
        "旧隔離先に統合の基準点がありません。元ファイルを保護するため統合を保留しました".to_string()
    })?;
    #[cfg(unix)]
    if baseline
        .values()
        .any(|entry| matches!(entry, Entry::File { mode: None, .. }))
    {
        return Err(
            "旧隔離先に権限の基準点がありません。元ファイルを保護するため統合を保留しました".into(),
        );
    }
    Ok(baseline)
}

#[cfg(test)]
mod missing_git_tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn git実行ファイルなしでも既存リポジトリを独立コピーする() {
        const CHILD_SOURCE: &str = "ZAIVERN_TEST_TASK_WORKSPACE_WITHOUT_GIT";
        if let Some(source) = std::env::var_os(CHILD_SOURCE) {
            use super::super::planner::{PlanInput, StaticPlanner, TeamPlanner};
            let source = PathBuf::from(source);
            // GUI 用 resolver はログインシェルや既知のインストール先も補う。
            // 解決結果を子プロセスでだけ注入して、実際の製品起動経路を通す。
            crate::shellenv::initialize_test_user_path(std::env::var_os("PATH").unwrap());
            let unavailable = crate::procx::hidden_command("git")
                .arg("--version")
                .output();
            assert!(
                matches!(unavailable, Err(ref e) if e.kind() == std::io::ErrorKind::NotFound),
                "子プロセスでは Git 実行ファイルが見つからない必要がある: {unavailable:?}"
            );
            assert!(source.join(".git").is_dir());
            let mut opts = super::super::runtime::RunOptions {
                agent_count: 2,
                ..Default::default()
            };
            let spec = super::super::planner::prepare_direct_request("HTMLを作成する", &mut opts);
            let plan = StaticPlanner
                .plan(PlanInput {
                    spec,
                    source: "request".into(),
                    agent_count: opts.agent_count,
                    review_required: opts.review_required,
                    workspace_root: source.clone(),
                    roles: vec![super::super::model::TeamRole::Implementer],
                })
                .unwrap();
            prepare(&source, &plan.tasks).unwrap();
            assert!(ready(&source, &plan.tasks));
            for task in &plan.tasks {
                let (copy, relative, actual_git) =
                    execution(&source, &task.files).unwrap().unwrap();
                assert!(!actual_git);
                assert!(copy.starts_with(source.join(ROOT)));
                assert_eq!(copy, source.join(relative));
                assert!(!copy.join(".git").exists());
                assert_eq!(
                    std::fs::read(copy.join("tracked.md")).unwrap(),
                    b"local edit\n"
                );
                assert_eq!(
                    std::fs::read(copy.join("untracked.md")).unwrap(),
                    b"untracked\n"
                );
                assert!(!copy.join("deleted.md").exists());
                std::fs::write(copy.join("tracked.md"), b"worker edit\n").unwrap();
                std::fs::remove_file(copy.join("untracked.md")).unwrap();
                assert_eq!(
                    std::fs::read(source.join("tracked.md")).unwrap(),
                    b"local edit\n"
                );
                assert_eq!(
                    std::fs::read(source.join("untracked.md")).unwrap(),
                    b"untracked\n"
                );
            }
            let permit = super::super::integration::try_acquire(&source, "missing-git")
                .unwrap()
                .unwrap();
            let outcome =
                super::super::integration::publish(&permit, &source, &plan.tasks[0].files).unwrap();
            assert!(outcome.conflicts.is_empty(), "{outcome:?}");
            assert_eq!(outcome.changed, ["tracked.md", "untracked.md"]);
            assert_eq!(
                std::fs::read(source.join("tracked.md")).unwrap(),
                b"worker edit\n"
            );
            assert!(!source.join("untracked.md").exists());
            std::fs::write(source.parent().unwrap().join("child-verified"), b"ok").unwrap();
            return;
        }

        let dir = crate::test_util::unique_temp_dir("team-workspace", "missing-git-binary");
        let source = dir.join("source");
        std::fs::create_dir_all(&source).unwrap();
        let source = crate::pathx::plain(source.canonicalize().unwrap());
        std::fs::write(source.join("tracked.md"), b"committed\n").unwrap();
        std::fs::write(source.join("deleted.md"), b"committed deletion\n").unwrap();
        super::super::gitinit::prepare(&source).unwrap();
        let head = crate::worktree::git_out(&source, &["rev-parse", "--verify", "HEAD"]).unwrap();
        std::fs::write(source.join("tracked.md"), b"local edit\n").unwrap();
        std::fs::write(source.join("untracked.md"), b"untracked\n").unwrap();
        std::fs::remove_file(source.join("deleted.md")).unwrap();
        let empty_bin = dir.join("empty-bin");
        std::fs::create_dir_all(&empty_bin).unwrap();
        let relative_module = module_path!().split_once("::").unwrap().1;
        let test_name =
            format!("{relative_module}::git実行ファイルなしでも既存リポジトリを独立コピーする");
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test_name, "--nocapture"])
            .current_dir(&source)
            .env("PATH", &empty_bin)
            .env("ZAIVERN_HOME", dir.join("state"))
            .env(CHILD_SOURCE, &source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(dir.join("child-verified")).unwrap(), b"ok");
        assert_eq!(
            crate::worktree::git_out(&source, &["rev-parse", "--verify", "HEAD"]).unwrap(),
            head
        );
        assert_eq!(
            std::fs::read(source.join("tracked.md")).unwrap(),
            b"worker edit\n"
        );
        assert!(!source.join("untracked.md").exists());
        assert!(!source.join("deleted.md").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
