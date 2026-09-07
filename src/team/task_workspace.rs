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
}

fn checked_root(source: &Path, scope: &str) -> Result<PathBuf, String> {
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

fn skipped(name: &std::ffi::OsStr) -> bool {
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
    let repository = crate::git::discover_toplevel(&source);
    let head = crate::worktree::git_out(&source, &["rev-parse", "--verify", "HEAD^{commit}"]).ok();
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
        let metadata = serde_json::to_vec(&Ready {
            source: source.clone(),
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
