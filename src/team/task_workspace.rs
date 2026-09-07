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

// 新規隔離先へ現状を反映する。hard link は使わず、編集が元ファイルへ波及しないようにする。
fn copy_current(source: &Path, destination: &Path, count: &mut usize) -> Result<(), String> {
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
        if std::fs::symlink_metadata(source.join(entry.file_name())).is_err() {
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
            copy_current(&entry.path(), &to, count)?;
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
    let initial = match existing {
        Some(task) => baseline(&source, &task.files)?,
        None => snapshot(&source)?,
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
        copy_current(&source, &root.join(&relative), &mut 0)?;
        if snapshot(&root.join(&relative))? != initial {
            return Err("隔離準備中に元フォルダが更新されました。以前の担当内容を保護するため準備を停止しました".into());
        }
        let metadata = serde_json::to_vec(&Ready {
            source: source.clone(),
            baseline: Some(initial.clone()),
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
    },
    Link(PathBuf),
}

pub(super) fn file_entry(bytes: &[u8], metadata: &std::fs::Metadata) -> Entry {
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = {
        let _ = metadata;
        false
    };
    Entry::File {
        fingerprint: super::changeset::Fingerprint {
            hash: crate::history::fnv1a64(bytes),
            len: bytes.len() as u64,
        },
        executable,
    }
}

pub(super) fn snapshot(root: &Path) -> Result<Snapshot, String> {
    Ok(frozen_files(root)?
        .into_iter()
        .filter(|(_, (entry, _))| matches!(entry, Entry::File { .. }))
        .map(|(path, (entry, _))| (path, entry))
        .collect())
}

/// Freeze bytes before applying anything. Never follow links or special files.
pub(super) fn frozen_files(
    root: &Path,
) -> Result<std::collections::BTreeMap<String, (Entry, Vec<u8>)>, String> {
    fn walk(
        root: &Path,
        dir: &Path,
        out: &mut std::collections::BTreeMap<String, (Entry, Vec<u8>)>,
        budget: &mut u64,
    ) -> Result<(), String> {
        for item in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
            let item = item.map_err(|e| e.to_string())?;
            if skipped(&item.file_name())
                || (dir.file_name().is_some_and(|n| n == ".claude")
                    && item.file_name() == "worktrees")
            {
                continue;
            }
            let path = item.path();
            let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if meta.is_dir() {
                walk(root, &path, out, budget)?;
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
            let (entry, bytes) = if meta.is_symlink() {
                (
                    Entry::Link(std::fs::read_link(&path).map_err(|e| e.to_string())?),
                    Vec::new(),
                )
            } else if meta.is_file() {
                if meta.len() > *budget {
                    return Err("統合ファイルの読み取り上限を超えました".into());
                }
                use std::io::Read;
                let mut bytes = Vec::new();
                std::fs::File::open(&path)
                    .map_err(|e| e.to_string())?
                    .take(*budget + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                if bytes.len() as u64 > *budget {
                    return Err("統合ファイルの読み取り上限を超えました".into());
                }
                *budget -= bytes.len() as u64;
                (file_entry(&bytes, &meta), bytes)
            } else {
                return Err(format!("通常ファイルではありません: {relative}"));
            };
            if out.len() >= 100_000 {
                return Err("統合ファイル数の上限を超えました".into());
            }
            out.insert(relative, (entry, bytes));
        }
        Ok(())
    }
    let mut out = std::collections::BTreeMap::new();
    let mut budget = super::changeset::MAX_HASH_BYTES;
    walk(root, root, &mut out, &mut budget)?;
    Ok(out)
}

pub(super) fn baseline(source: &Path, files: &[String]) -> Result<Snapshot, String> {
    let scope = scope(files).ok_or("隔離タスクではありません")?;
    let root = checked_root(source, scope)?;
    let record: Ready = serde_json::from_slice(
        &std::fs::read(root.with_extension("ready.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    record.baseline.ok_or_else(|| {
        "旧隔離先に統合の基準点がありません。元ファイルを保護するため統合を保留しました".into()
    })
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
            b"local edit\n"
        );
        assert_eq!(
            std::fs::read(source.join("untracked.md")).unwrap(),
            b"untracked\n"
        );
        assert!(!source.join("deleted.md").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
