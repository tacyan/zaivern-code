//! Team Run ごとの git worktree 隔離。
//!
//! Run 間で同じ working tree を共有すると、変更の帰属・検証・停止の
//! どれも分離できない。この層は、未信頼の保存データから削除対象を
//! 決めないため、置き場を `home + source workspace key + run_id` から必ず
//! 再計算する。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Run 専用 worktree の保存形。元 workspace と実行 workspace を
/// 別の欄にし、復元時に必ず再検証する。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunWorkspace {
    pub source_workspace: String,
    pub repository_root: String,
    pub worktree_root: String,
    pub execution_workspace: String,
}

/// Run worktree の未保存変更。`git status --porcelain=v1 -z` が1件でも
///返したら dirty で、tracked / untracked / staged / deleted / renamed を含む。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeState {
    Clean,
    Dirty,
}

/// `ZAIVERN_HOME` 直下の専用親フォルダ。
pub const DIR_NAME: &str = "team-worktrees";

fn canonical_dir(path: &Path, what: &str) -> Result<PathBuf, String> {
    let p = path
        .canonicalize()
        .map_err(|e| format!("{what} を確認できません ({}): {e}", path.display()))?;
    // Windows の canonicalize は `\\?\C:\...` を返す。これを文字列で
    // Git for Windows へ渡すと MSYS が `//?/C:/...` に変換し、worktree の
    // `.git` を作れない。実体解決は維持したまま共通の素の形へ戻す。
    let p = crate::pathx::plain(p);
    if !p.is_dir() {
        return Err(format!("{what} がフォルダではありません: {}", p.display()));
    }
    Ok(p)
}

/// `home` がまだ無い Start 前でも、存在する親を canonicalize してから
/// 末尾を戻す。macOS の `/var` → `/private/var` のような別名が、作成の
/// 前後で保存値と決定パスを食い違わせないため。
fn normalized_home(home: &Path) -> Result<PathBuf, String> {
    if let Ok(path) = home.canonicalize() {
        return Ok(crate::pathx::plain(path));
    }
    let name = home
        .file_name()
        .ok_or_else(|| "ZAIVERN_HOME の名前を決められません".to_string())?;
    let parent = home
        .parent()
        .ok_or_else(|| "ZAIVERN_HOME の親を決められません".to_string())?
        .canonicalize()
        .map_err(|e| format!("ZAIVERN_HOME の親を確認できません: {e}"))?;
    let parent = crate::pathx::plain(parent);
    Ok(parent.join(name))
}

/// 未信頼の `run_id` から親や他 Run を指せない、唯一の配置先。
pub fn expected_root(home: &Path, source_workspace: &Path, run_id: &str) -> Result<PathBuf, String> {
    if !super::outbox::valid_run_id(run_id) {
        return Err(format!("run_id {run_id:?} は worktree の名前に使えません"));
    }
    let source = canonical_dir(source_workspace, "元 workspace")?;
    let base = normalized_home(home)?
        .join(DIR_NAME)
        .join(crate::history::workspace_key(&source));
    super::outbox::safe_child(&base, run_id)
        .ok_or_else(|| format!("run_id {run_id:?} から安全な worktree 置き場を作れません"))
}

/// Zaivern が作る中間ディレクトリに symlink を挟まない。
/// `home` 自体はアプリが決めた信頼済みルートなので、その下の2段だけを見る。
fn ensure_plain_layout(home: &Path, keyed_base: &Path) -> Result<(), String> {
    for p in [home.join(DIR_NAME), keyed_base.to_path_buf()] {
        match std::fs::symlink_metadata(&p) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!("worktree 置き場に symlink は使えません: {}", p.display()));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(format!("worktree 置き場がフォルダではありません: {}", p.display()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "worktree 置き場を確認できません ({}): {e}",
                    p.display()
                ));
            }
        }
    }
    Ok(())
}

fn source_layout(source_workspace: &Path) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let source = canonical_dir(source_workspace, "元 workspace")?;
    let repo = crate::worktree::repo_root(&source)?;
    let repo = canonical_dir(&repo, "git リポジトリ")?;
    let relative = source
        .strip_prefix(&repo)
        .map_err(|_| "workspace が git リポジトリの内側にありません".to_string())?
        .to_path_buf();
    // HEAD の無い repo は worktree で隔離できない。
    crate::worktree::git_out(&repo, &["rev-parse", "--verify", "HEAD^{commit}"])
        .map_err(|e| format!("コミットの無い workspace では Team Run を開始できません: {e}"))?;
    Ok((source, repo, relative))
}

fn plain_target(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "Run 専用 worktree の代わりに symlink があります: {}",
            path.display()
        )),
        Ok(meta) if !meta.is_dir() => Err(format!(
            "Run 専用 worktree の位置がフォルダではありません: {}",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(e) => Err(format!("Run 専用 worktree を確認できません: {e}")),
    }
}

fn existing(
    source: &Path,
    repo: &Path,
    relative: &Path,
    root: &Path,
) -> Result<RunWorkspace, String> {
    plain_target(root)?;
    let actual_root = canonical_dir(root, "Run 専用 worktree")?;
    let registered = PathBuf::from(crate::worktree::git_out(
        &actual_root,
        &["rev-parse", "--show-toplevel"],
    )?);
    let registered = canonical_dir(&registered, "登録済み worktree")?;
    if registered != actual_root {
        return Err("既存フォルダはこの Run の git worktree ではありません".to_string());
    }
    // 同じ common git directory に属することを確認する。別 repo を同じ
    // 決定パスに置いても引き取らない。
    let common = |at: &Path| -> Result<PathBuf, String> {
        let raw = crate::worktree::git_out(at, &["rev-parse", "--git-common-dir"])?;
        let p = PathBuf::from(raw);
        let p = if p.is_absolute() { p } else { at.join(p) };
        p.canonicalize()
            .map(crate::pathx::plain)
            .map_err(|e| format!("git common directory を確認できません: {e}"))
    };
    if common(repo)? != common(&actual_root)? {
        return Err("既存 worktree は元 workspace と別の repository です".to_string());
    }
    let execution = canonical_dir(&actual_root.join(relative), "Run の実行 workspace")?;
    if !execution.starts_with(&actual_root) {
        return Err("Run の実行 workspace が専用 worktree の外です".to_string());
    }
    Ok(RunWorkspace {
        source_workspace: source.display().to_string(),
        repository_root: repo.display().to_string(),
        worktree_root: actual_root.display().to_string(),
        execution_workspace: execution.display().to_string(),
    })
}

/// この [`create`] 呼び出しが `git worktree add` に成功した直後の後始末。
/// 削除先は引数や保存データから受け取らず、同じ安全境界
/// ([`expected_root`]) から再計算する。
fn rollback_created(
    home: &Path,
    source_workspace: &Path,
    repo: &Path,
    run_id: &str,
) -> Result<(), String> {
    let target = expected_root(home, source_workspace, run_id)?;
    match std::fs::symlink_metadata(&target) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 本体だけが先に消えた場合、今回の stale な Git 登録を掃く。
            return crate::worktree::git_out(repo, &["worktree", "prune", "--expire", "now"])
                .map(|_| ())
                .map_err(|e| format!("git worktree prune に失敗: {e}"));
        }
        Err(e) => return Err(format!("作成したworktreeを確認できません: {e}")),
    }
    // relative を空にすると、対象rootそのものについて symlink / Git top-level /
    // common git directory を既存の安全境界で検証できる。別repositoryや
    // 作成後に置換されたフォルダには `git worktree remove` を実行しない。
    existing(source_workspace, repo, Path::new(""), &target)
        .map_err(|e| format!("作成したworktreeを安全に検証できないため削除しません: {e}"))?;

    #[cfg(test)]
    if fault_inject::take_rollback_failure() {
        return Err("(テスト) 新規worktreeのロールバックに失敗".to_string());
    }
    let target_text = target.to_string_lossy().into_owned();
    crate::worktree::git_out(
        repo,
        &["worktree", "remove", "--force", &target_text],
    )
    .map_err(|e| format!("git worktree remove --force に失敗: {e}"))?;
    match std::fs::symlink_metadata(&target) {
        Ok(_) => Err(format!(
            "git が作成済みworktreeを削除しませんでした: {}",
            target.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("worktreeの削除結果を確認できません: {e}")),
    }
}

/// 元 workspace の HEAD から、Run 専用の detached worktree を作る。
/// 元 workspace の index / working tree は読むだけで変更しない。
pub fn create(home: &Path, source_workspace: &Path, run_id: &str) -> Result<RunWorkspace, String> {
    let (source, repo, relative) = source_layout(source_workspace)?;
    let root = expected_root(home, &source, run_id)?;
    let base = root
        .parent()
        .ok_or_else(|| "worktree 置き場の親を決められません".to_string())?;
    ensure_plain_layout(home, base)?;
    if std::fs::symlink_metadata(&root).is_ok() {
        // worktree 作成後、対応の保存前に落ちた残骸は、同じ repo /
        // 同じ決定パスの正しい worktree と検証できたときだけ引き継ぐ。
        return existing(&source, &repo, &relative, &root);
    }
    std::fs::create_dir_all(base)
        .map_err(|e| format!("worktree 置き場を作れません ({}): {e}", base.display()))?;
    ensure_plain_layout(home, base)?;

    let root_text = root.to_string_lossy().into_owned();
    crate::worktree::git_out(&repo, &["worktree", "add", "--detach", &root_text, "HEAD"])
        .map_err(|e| format!("Run {run_id} の専用 worktree を作成できません: {e}"))?;

    match existing(&source, &repo, &relative, &root) {
        Ok(workspace) => Ok(workspace),
        Err(original) => match rollback_created(home, &source, &repo, run_id) {
            Ok(()) => Err(original),
            Err(cleanup) => Err(format!(
                "{original}\nRun {run_id} の新規worktreeの後始末にも失敗しました: {cleanup}"
            )),
        },
    }
}

/// 保存された対応が、現在の元 workspace と決定的な配置先に
/// 一致するときだけ、実行 workspace を返す。
pub fn restore(
    home: &Path,
    source_workspace: &Path,
    run_id: &str,
    saved: &RunWorkspace,
) -> Result<PathBuf, String> {
    let (source, repo, relative) = source_layout(source_workspace)?;
    let expected = expected_root(home, &source, run_id)?;
    if Path::new(&saved.source_workspace) != source
        || Path::new(&saved.repository_root) != repo
        || Path::new(&saved.worktree_root) != expected
    {
        return Err("Run の worktree 対応が現在の workspace と一致しません".to_string());
    }
    let verified = existing(&source, &repo, &relative, &expected)?;
    if &verified != saved {
        return Err("Run の実行 workspace 対応が壊れています".to_string());
    }
    Ok(PathBuf::from(verified.execution_workspace))
}

/// 対応の保存前に落ちた場合の後始末用。決定パスに何も無ければ
/// `None`、同じ repository の正しい worktree なら検証済み記録を返す。
pub fn discover(
    home: &Path,
    source_workspace: &Path,
    run_id: &str,
) -> Result<Option<RunWorkspace>, String> {
    // まだ開始していない旧 Run は、Git repo でなくても片付けられる。
    // 決定パスに何も無いことを先に見て、ある場合だけ git を検証する。
    let source = canonical_dir(source_workspace, "元 workspace")?;
    let root = expected_root(home, &source, run_id)?;
    match std::fs::symlink_metadata(&root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Run 専用 worktree を確認できません: {e}")),
        Ok(_) => {
            let (_, repo, relative) = source_layout(&source)?;
            existing(&source, &repo, &relative, &root).map(Some)
        }
    }
}

/// 指定 Run の worktree だけを git に外させる。保存されたパスを
/// そのまま削除対象にはしない。
fn verified_for_removal(
    home: &Path,
    source_workspace: &Path,
    run_id: &str,
    saved: &RunWorkspace,
) -> Result<(PathBuf, PathBuf), String> {
    let (source, repo, relative) = source_layout(source_workspace)?;
    let expected = expected_root(home, &source, run_id)?;
    if Path::new(&saved.source_workspace) != source
        || Path::new(&saved.repository_root) != repo
        || Path::new(&saved.worktree_root) != expected
    {
        return Err(format!(
            "Run の worktree 対応が現在の workspace と一致しないため削除しません \
             (source={} / {}, repo={} / {}, root={} / {})",
            saved.source_workspace,
            source.display(),
            saved.repository_root,
            repo.display(),
            saved.worktree_root,
            expected.display()
        ));
    }
    match std::fs::symlink_metadata(&expected) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::worktree::git_out(&repo, &["worktree", "prune"])
                .map_err(|e| format!("worktree 本体はありませんが Git 登録を掃除できません: {e}"))?;
            return Ok((repo, expected));
        }
        Err(e) => return Err(format!("Run 専用 worktree を確認できません: {e}")),
        Ok(_) => {}
    }
    let verified = existing(&source, &repo, &relative, &expected)?;
    if &verified != saved {
        return Err("Run の worktree 対応を完全には検証できないため削除しません".to_string());
    }
    Ok((repo, expected))
}

/// 対象Runのworktree全体に未保存変更があるか、Gitの実測で確認する。
pub fn change_state(
    home: &Path,
    source_workspace: &Path,
    run_id: &str,
    saved: &RunWorkspace,
) -> Result<ChangeState, String> {
    let (_, expected) = verified_for_removal(home, source_workspace, run_id, saved)?;
    if !expected.exists() {
        return Ok(ChangeState::Clean);
    }
    let status = crate::worktree::git_out(
        &expected,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    Ok(if status.is_empty() {
        ChangeState::Clean
    } else {
        ChangeState::Dirty
    })
}

/// 未保存変更が無いRunだけを削除する。検査と削除の間に変更が増えても、
/// `--force`なしのGit自身の再検査が拒否する。
pub fn remove_clean(
    home: &Path,
    source_workspace: &Path,
    run_id: &str,
    saved: &RunWorkspace,
) -> Result<(), String> {
    let (repo, expected) = verified_for_removal(home, source_workspace, run_id, saved)?;
    if !expected.exists() {
        return Ok(());
    }
    if change_state(home, source_workspace, run_id, saved)? == ChangeState::Dirty {
        return Err("Run の worktree に未保存の成果物があるため削除しません".to_string());
    }
    #[cfg(test)]
    if fault_inject::take_remove_failure() {
        return Err("(テスト) git worktree の削除に失敗".to_string());
    }
    #[cfg(test)]
    if fault_inject::take_dirty_before_remove() {
        std::fs::write(expected.join("late-change.txt"), "late\n")
            .map_err(|e| format!("(テスト) 競合変更を作れません: {e}"))?;
    }
    let dir = expected.to_string_lossy().into_owned();
    crate::worktree::git_out(&repo, &["worktree", "remove", &dir])?;
    crate::worktree::git_out(&repo, &["worktree", "prune"])?;
    if std::fs::symlink_metadata(&expected).is_ok() {
        return Err(format!("git が worktree を削除しませんでした: {}", expected.display()));
    }
    Ok(())
}

/// ユーザーが成果物の破棄を明示承認したRunだけを強制削除する。
pub fn remove_discarded(
    home: &Path,
    source_workspace: &Path,
    run_id: &str,
    saved: &RunWorkspace,
) -> Result<(), String> {
    let (repo, expected) = verified_for_removal(home, source_workspace, run_id, saved)?;
    if !expected.exists() {
        return Ok(());
    }
    #[cfg(test)]
    if fault_inject::take_remove_failure() {
        return Err("(テスト) git worktree の削除に失敗".to_string());
    }
    let dir = expected.to_string_lossy().into_owned();
    crate::worktree::git_out(&repo, &["worktree", "remove", "--force", &dir])?;
    crate::worktree::git_out(&repo, &["worktree", "prune"])?;
    if std::fs::symlink_metadata(&expected).is_ok() {
        return Err(format!("git が worktree を削除しませんでした: {}", expected.display()));
    }
    Ok(())
}

#[cfg(test)]
fn remove(
    home: &Path,
    source_workspace: &Path,
    run_id: &str,
    saved: &RunWorkspace,
) -> Result<(), String> {
    remove_discarded(home, source_workspace, run_id, saved)
}

#[cfg(test)]
pub mod fault_inject {
    use std::cell::Cell;

    thread_local! {
        static FAIL_REMOVE: Cell<bool> = const { Cell::new(false) };
        static FAIL_ROLLBACK: Cell<bool> = const { Cell::new(false) };
        static DIRTY_BEFORE_REMOVE: Cell<bool> = const { Cell::new(false) };
    }

    pub fn fail_remove_once() {
        FAIL_REMOVE.with(|flag| flag.set(true));
    }

    pub fn fail_rollback_once() {
        FAIL_ROLLBACK.with(|flag| flag.set(true));
    }

    pub fn dirty_before_remove_once() {
        DIRTY_BEFORE_REMOVE.with(|flag| flag.set(true));
    }

    pub(super) fn take_remove_failure() -> bool {
        FAIL_REMOVE.with(|flag| flag.replace(false))
    }

    pub(super) fn take_rollback_failure() -> bool {
        FAIL_ROLLBACK.with(|flag| flag.replace(false))
    }

    pub(super) fn take_dirty_before_remove() -> bool {
        DIRTY_BEFORE_REMOVE.with(|flag| flag.replace(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn force_remove_for_cleanup(repo: &Path, target: &Path) {
        let target = target.to_string_lossy().into_owned();
        let _ = crate::worktree::git_out(repo, &["worktree", "remove", "--force", &target]);
        let _ = crate::worktree::git_out(repo, &["worktree", "prune"]);
    }

    fn repo(tag: &str) -> Option<(PathBuf, PathBuf)> {
        let root = crate::test_util::unique_temp_dir("zai-team-worktree", tag);
        let home = root.join(".home");
        std::fs::create_dir_all(&root).ok()?;
        std::fs::write(root.join("seed.txt"), "seed\n").ok()?;
        let ok = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !ok(&["init", "-q"])
            || !ok(&["config", "user.email", "team@example.invalid"])
            || !ok(&["config", "user.name", "Team"])
            || !ok(&["add", "seed.txt"])
            || !ok(&["commit", "-q", "-m", "seed"])
        {
            std::fs::remove_dir_all(&root).ok();
            return None;
        }
        Some((root, home))
    }

    #[test]
    fn 二本のrunを別worktreeに作り一方だけ消せる() {
        let Some((root, home)) = repo("two") else {
            return;
        };
        std::fs::write(root.join("user-change.txt"), "keep\n").unwrap();
        let a = create(&home, &root, "run-a").expect("A");
        let b = create(&home, &root, "run-b").expect("B");
        assert_ne!(a.execution_workspace, b.execution_workspace);
        std::fs::write(Path::new(&a.execution_workspace).join("a.txt"), "a").unwrap();
        assert!(!Path::new(&b.execution_workspace).join("a.txt").exists());
        remove(&home, &root, "run-a", &a).expect("A だけ削除");
        assert!(!Path::new(&a.worktree_root).exists());
        assert!(Path::new(&b.worktree_root).is_dir(), "B まで削除した");
        assert_eq!(std::fs::read_to_string(root.join("user-change.txt")).unwrap(), "keep\n");
        remove(&home, &root, "run-b", &b).expect("B 削除");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn git_statusは全種類の未保存変更をdirtyとして検出する() {
        let Some((root, home)) = repo("dirty-kinds") else {
            return;
        };
        for (run_id, kind) in [
            ("run-tracked", "tracked"),
            ("run-untracked", "untracked"),
            ("run-staged", "staged"),
            ("run-deleted", "deleted"),
            ("run-renamed", "renamed"),
        ] {
            let created = create(&home, &root, run_id).expect("worktreeを作れる");
            let worktree = Path::new(&created.execution_workspace);
            match kind {
                "tracked" => std::fs::write(worktree.join("seed.txt"), "changed\n").unwrap(),
                "untracked" => std::fs::write(worktree.join("new.txt"), "new\n").unwrap(),
                "staged" => {
                    std::fs::write(worktree.join("staged.txt"), "staged\n").unwrap();
                    crate::worktree::git_out(worktree, &["add", "staged.txt"]).unwrap();
                }
                "deleted" => std::fs::remove_file(worktree.join("seed.txt")).unwrap(),
                "renamed" => {
                    crate::worktree::git_out(worktree, &["mv", "seed.txt", "renamed.txt"])
                        .unwrap();
                }
                _ => unreachable!(),
            }
            assert_eq!(
                change_state(&home, &root, run_id, &created).unwrap(),
                ChangeState::Dirty,
                "{kind}を見落とした"
            );
            assert!(
                remove_clean(&home, &root, run_id, &created).is_err(),
                "{kind}を確認なしで削除した"
            );
            remove_discarded(&home, &root, run_id, &created).expect("明示破棄で片付く");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cleanは通常削除でき削除直前の変更は非force削除が拒否する() {
        let Some((root, home)) = repo("clean-and-race") else {
            return;
        };
        let clean = create(&home, &root, "run-clean").unwrap();
        assert_eq!(
            change_state(&home, &root, "run-clean", &clean).unwrap(),
            ChangeState::Clean
        );
        remove_clean(&home, &root, "run-clean", &clean).expect("clean worktreeは削除できる");

        let raced = create(&home, &root, "run-raced").unwrap();
        fault_inject::dirty_before_remove_once();
        assert!(
            remove_clean(&home, &root, "run-raced", &raced).is_err(),
            "再検査後に増えた成果物を削除した"
        );
        assert!(Path::new(&raced.worktree_root).exists(), "競合時にworktreeを消した");
        assert!(
            Path::new(&raced.execution_workspace)
                .join("late-change.txt")
                .exists(),
            "競合で追加された成果物が残っていない"
        );
        remove_discarded(&home, &root, "run-raced", &raced).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn headに無いsubdirの検証失敗は新規worktreeを戻し同じidで再試行できる() {
        let Some((root, home)) = repo("rollback-missing-subdir") else {
            return;
        };
        let source = root.join("draft");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("keep.txt"), "user change\n").unwrap();
        let run_id = "run-missing-subdir";
        let target = expected_root(&home, &source, run_id).unwrap();

        let first = create(&home, &source, run_id).expect_err("HEADに無いsubdirは検証で止まる");
        let target_remained = std::fs::symlink_metadata(&target).is_ok();
        let listed_after_first = crate::worktree::git_out(
            &root,
            &["worktree", "list", "--porcelain"],
        )
        .unwrap();
        let user_file = std::fs::read_to_string(source.join("keep.txt")).unwrap();

        crate::worktree::git_out(&root, &["add", "draft/keep.txt"]).unwrap();
        crate::worktree::git_out(&root, &["commit", "-q", "-m", "add draft"]).unwrap();
        let retry = create(&home, &source, run_id);
        let retry_ok = retry.is_ok();
        if let Ok(created) = retry {
            remove(&home, &source, run_id, &created).unwrap();
        } else {
            force_remove_for_cleanup(&root, &target);
        }
        std::fs::remove_dir_all(&root).ok();

        assert!(first.contains("Run の実行 workspace"), "元の検証エラーが不明: {first}");
        assert!(!target_remained, "検証に失敗した新規worktree本体が残った");
        assert!(
            !listed_after_first.contains(run_id),
            "検証に失敗したworktreeのGit登録が残った: {listed_after_first}"
        );
        assert_eq!(user_file, "user change\n", "元workspaceの未コミット変更を壊した");
        assert!(retry_ok, "残骸のため同じRun IDで再試行できない");
    }

    #[test]
    fn 作成前からあるrootは検証に失敗しても削除しない() {
        let Some((root, home)) = repo("preexisting-root") else {
            return;
        };
        let run_id = "run-preexisting";
        let target = expected_root(&home, &root, run_id).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let canary = target.join("canary.txt");
        std::fs::write(&canary, "keep\n").unwrap();

        let error = create(&home, &root, run_id).expect_err("既存rootを引き取ってはいけない");
        let canary_after = std::fs::read_to_string(&canary).unwrap();
        std::fs::remove_dir_all(&root).ok();

        assert!(error.contains("既存フォルダ"), "検証エラーが不明: {error}");
        assert_eq!(canary_after, "keep\n", "今回作っていないrootを削除した");
    }

    #[test]
    fn 新規worktreeの検証とrollbackが両方失敗した理由を返す() {
        let Some((root, home)) = repo("rollback-failure") else {
            return;
        };
        let source = root.join("draft");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("keep.txt"), "user change\n").unwrap();
        let run_id = "run-rollback-failure";
        let target = expected_root(&home, &source, run_id).unwrap();
        fault_inject::fail_rollback_once();

        let error = create(&home, &source, run_id).expect_err("両方の失敗を返す");
        let target_remained = std::fs::symlink_metadata(&target).is_ok();
        force_remove_for_cleanup(&root, &target);
        std::fs::remove_dir_all(&root).ok();

        assert!(error.contains("Run の実行 workspace"), "元エラーが消えた: {error}");
        assert!(
            error.contains("新規worktreeの後始末にも失敗")
                && error.contains("ロールバックに失敗"),
            "cleanupエラーが消えた: {error}"
        );
        assert!(target_remained, "fault injectionが後始末失敗を再現していない");
    }

    #[test]
    fn 不正idとsymlinkは削除対象にできない() {
        let Some((root, home)) = repo("unsafe") else {
            return;
        };
        for bad in ["", ".", "..", "a/b", "a\\b", "C:drive"] {
            assert!(expected_root(&home, &root, bad).is_err(), "{bad:?}");
        }
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("canary"), "alive").unwrap();
        let target = expected_root(&home, &root, "run-link").unwrap();
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &target).unwrap();
            let fake = RunWorkspace {
                source_workspace: root.canonicalize().unwrap().display().to_string(),
                repository_root: root.canonicalize().unwrap().display().to_string(),
                worktree_root: target.display().to_string(),
                execution_workspace: target.display().to_string(),
            };
            assert!(remove(&home, &root, "run-link", &fake).is_err());
            assert!(outside.join("canary").exists(), "symlink の先を削除した");
        }
        let rogue = expected_root(&home, &root, "run-other-repo").unwrap();
        std::fs::create_dir_all(&rogue).unwrap();
        std::fs::write(rogue.join("rogue.txt"), "do not delete\n").unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&rogue)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        assert!(git(&["init", "-q"]));
        let fake = RunWorkspace {
            source_workspace: root.canonicalize().unwrap().display().to_string(),
            repository_root: root.canonicalize().unwrap().display().to_string(),
            worktree_root: rogue.canonicalize().unwrap().display().to_string(),
            execution_workspace: rogue.canonicalize().unwrap().display().to_string(),
        };
        assert!(restore(&home, &root, "run-other-repo", &fake).is_err());
        assert!(remove(&home, &root, "run-other-repo", &fake).is_err());
        assert!(rogue.join("rogue.txt").exists(), "別repositoryを削除した");
        std::fs::remove_dir_all(&root).ok();
    }
}
