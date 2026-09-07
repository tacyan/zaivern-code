//! Cross-run integration ownership and content-based, conservative publication.
use super::task_workspace::{self, Entry};
use crate::lease::{self, Claim, Holder};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);
const HOLDER: &str = "zai-team-integration";
const RETIRED: &str = "zai-team-integration-writer";
// Failed release I/O must not strand a live-process, non-expiring lease.
// This is a retry queue, not another ownership ledger; lease remains authoritative.
static RELEASES: std::sync::Mutex<Vec<(PathBuf, Holder)>> = std::sync::Mutex::new(Vec::new());

#[derive(Debug)]
pub struct Permit {
    source: PathBuf,
    store: PathBuf,
    holder: Holder,
    writer: Option<std::sync::Arc<crate::terminal::writer_tree::Tree>>,
}

impl Permit {
    /// Persist the actual writer tree alongside the publisher PID carried in the
    /// session token. Either may still write: the agent builds, the app publishes.
    pub fn handoff_writer(&mut self, pid: Option<u32>) -> Result<(), String> {
        if self.writer.is_some() {
            return Ok(());
        }
        self.handoff(pid, None)
    }

    pub fn handoff_identity(
        &mut self,
        writer: crate::terminal::writer_tree::Identity,
    ) -> Result<(), String> {
        self.handoff(Some(writer.pid), writer.tree)
    }

    fn handoff(
        &mut self,
        pid: Option<u32>,
        writer: Option<std::sync::Arc<crate::terminal::writer_tree::Tree>>,
    ) -> Result<(), String> {
        // An already-exited terminal no longer exposes its PID. Do not erase
        // the writer recorded before delivery: it is still the recovery proof.
        let Some(pid) = pid.filter(|pid| *pid != 0) else {
            return Ok(());
        };
        let mut replacement = self.holder.clone();
        #[cfg(windows)]
        if let Some(tree) = &writer {
            let token = self
                .holder
                .session
                .rsplit_once('|')
                .map_or(self.holder.session.as_str(), |(_, token)| token);
            replacement.session = format!("{}|{token}", tree.job.name);
        }
        replacement.agent = RETIRED.into();
        replacement.pid = pid;
        let updated = lease::with_store_retry(&self.store, |state| {
            let Some(lease) = state
                .leases
                .iter_mut()
                .find(|lease| lease.holder.same(&self.holder))
            else {
                return false;
            };
            lease.holder = replacement.clone();
            true
        })?;
        if !updated {
            return Err("統合の所有権を停止処理へ引き継げません".into());
        }
        self.writer = writer;
        self.holder = replacement;
        Ok(())
    }

    /// Ownership outlives the Runtime and the UI. On process exit the persistent
    /// writer PID protects the lease even if this waiting thread disappears.
    pub fn release_after(self, handle: crate::terminal::ReapHandle) {
        if self
            .writer
            .as_ref()
            .is_some_and(|tree| !handle.tracks(tree))
        {
            self.release_when_writer_stops();
            return;
        }
        if handle.is_finished() {
            drop(self);
            return;
        }
        std::thread::spawn(move || {
            while !handle.is_finished() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            drop(self);
        });
    }

    /// A discarded Runtime cannot publish anymore, but its writer may still
    /// be running until the queued Stop reaches the terminal/reaper.
    pub fn writer_draining(&self) -> bool {
        self.writer.as_ref().is_some_and(|tree| tree.draining())
    }

    pub fn writer_stopped(&self) -> bool {
        if let Some(tree) = &self.writer {
            return tree.finished();
        }
        #[cfg(test)]
        if self.holder.pid == std::process::id() {
            return true;
        } // Synthetic SessionObs.
        self.holder.agent != RETIRED || !crate::terminal::process_tree_alive(self.holder.pid)
    }

    pub fn release_when_writer_stops(self) {
        if self.writer_stopped() {
            drop(self);
            return;
        }
        std::thread::spawn(move || {
            while !self.writer_stopped() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            drop(self);
        });
    }

    pub fn retire(mut self, handle: crate::terminal::ReapHandle) -> Result<(), String> {
        if self
            .writer
            .as_ref()
            .is_some_and(|tree| !handle.tracks(tree))
        {
            self.release_when_writer_stops();
            return Ok(());
        }
        if handle.is_finished() {
            return Ok(());
        }
        let result = self.handoff_writer(handle.process_id());
        self.release_after(handle);
        result
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if lease::with_store_retry(&self.store, |store| {
            lease::release(store, &self.holder);
        })
        .is_err()
        {
            RELEASES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((self.store.clone(), self.holder.clone()));
        }
    }
}

#[cfg(test)]
pub fn try_acquire(source: &Path, run_id: &str) -> Result<Option<Permit>, String> {
    let permit = try_acquire_at(
        source,
        run_id,
        &format!("{}/integration.json", task_workspace::ROOT),
    )?;
    if let Some(permit) = &permit {
        recover_publications(&permit.source)?;
    }
    Ok(permit)
}

/// Dispatch only acquires ownership; recovery I/O runs in the publisher thread.
pub(super) fn try_acquire_for_dispatch(
    source: &Path,
    run_id: &str,
) -> Result<Option<Permit>, String> {
    try_acquire_at(
        source,
        run_id,
        &format!("{}/integration.json", task_workspace::ROOT),
    )
}

/// Different isolated parts have different leases, preserving parallel workers
/// while preventing a restored Run from reusing a still-active writer's part.
pub fn try_acquire_task(
    source: &Path,
    files: &[String],
    run_id: &str,
) -> Result<Option<Permit>, String> {
    let scope = task_workspace::scope(files).ok_or("隔離担当の所有範囲がありません")?;
    try_acquire_at(source, run_id, &format!("{scope}.lease.json"))
}

fn persisted_writer_alive(holder: &Holder) -> bool {
    #[cfg(windows)]
    {
        // A legacy parent PID cannot prove that orphaned Windows writers died.
        holder
            .session
            .split_once('|')
            .is_none_or(|(job, _)| crate::terminal::writer_tree::windows::persisted_alive(job))
    }
    #[cfg(unix)]
    {
        crate::terminal::process_tree_alive(holder.pid)
    }
}

fn try_acquire_at(
    source: &Path,
    run_id: &str,
    relative_store: &str,
) -> Result<Option<Permit>, String> {
    let source = source
        .canonicalize()
        .map(crate::pathx::plain)
        .map_err(|e| e.to_string())?;
    let store = task_workspace::checked_root(&source, relative_store)?;
    std::fs::create_dir_all(store.parent().ok_or("所有権台帳の親がありません")?)
        .map_err(|e| e.to_string())?;
    for extra in [
        store.with_extension("lock"),
        store.with_extension(format!("tmp{}", std::process::id())),
    ] {
        if std::fs::symlink_metadata(extra).is_ok_and(|m| m.is_symlink()) {
            return Err("統合台帳にシンボリックリンクがあります".into());
        }
    }
    {
        let mut pending = RELEASES.lock().unwrap_or_else(|e| e.into_inner());
        let mut index = 0;
        while index < pending.len() {
            if pending[index].0 != store {
                index += 1;
                continue;
            }
            match lease::with_store(&store, |state| lease::release(state, &pending[index].1)) {
                Ok(_) => {
                    pending.remove(index);
                }
                Err(e) if lease::is_lock_busy(&e) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }
    let holder = Holder {
        agent: HOLDER.into(),
        session: format!(
            "{run_id}:{}:{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ),
        cwd: source.to_string_lossy().into_owned(),
        pid: std::process::id(),
    };
    let claimed = lease::with_store(&store, |state| {
        // A handed-off lease protects both the writer and the publisher. The
        // agent may exit before the app finishes applying the frozen candidate.
        state.leases.retain(|l| {
            l.holder.pid == 0
                || match l.holder.agent.as_str() {
                    HOLDER => crate::instances::pid_alive(l.holder.pid),
                    RETIRED => {
                        let publisher = l
                            .holder
                            .session
                            .rsplit(':')
                            .nth(1)
                            .and_then(|pid| pid.parse::<u32>().ok());
                        // Unknown identity cannot prove that the publisher died.
                        publisher.is_none_or(|pid| pid == 0 || crate::instances::pid_alive(pid))
                            || persisted_writer_alive(&l.holder)
                    }
                    _ => true,
                }
        });
        lease::try_claim(
            state,
            &holder,
            &["**".into()],
            lease::now_secs(),
            u64::MAX,
            &crate::instances::pid_alive,
        )
    });
    let claimed = match claimed {
        Ok(claimed) => claimed,
        Err(e) if lease::is_lock_busy(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    match claimed {
        Claim::Granted(_) => Ok(Some(Permit {
            source,
            store,
            holder,
            writer: None,
        })),
        Claim::Refused { .. } => Ok(None),
    }
}

#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Outcome {
    pub changed: Vec<String>,
    pub conflicts: Vec<String>,
    pub notes: Vec<String>,
}

fn checked_file(source: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative
        .split('/')
        .any(|p| task_workspace::skipped(std::ffi::OsStr::new(p)))
    {
        return Err("内部ファイルは統合できません".into());
    }
    task_workspace::checked_root(source, relative)
}

fn current(
    source: &Path,
    relative: &str,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Option<Entry>, String> {
    let path = checked_file(source, relative)?;
    entry_at_cancellable(&path, cancel)
}

fn entry_at(path: &Path) -> Result<Option<Entry>, String> {
    // Once captured, finish the comparison/install/recovery of this file before
    // honoring Stop. The pre-capture comparison can stop between read chunks.
    entry_at_cancellable(path, &std::sync::atomic::AtomicBool::new(false))
}

fn entry_at_cancellable(
    path: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Option<Entry>, String> {
    task_workspace::check_cancel(cancel)?;
    match std::fs::symlink_metadata(path) {
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.to_string()),
        Ok(meta) if meta.is_file() => Ok(Some(task_workspace::streamed_entry_cancellable(
            path, cancel,
        )?)),
        Ok(_) => Err("通常ファイルではありません".into()),
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PublicationRecord {
    relative: String,
    directory: String,
}

fn sync_record(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    std::fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn pending_publications(source: &Path) -> Result<PathBuf, String> {
    task_workspace::checked_root(
        source,
        &format!("{}/publications/pending", task_workspace::ROOT),
    )
}

// Called only with the cross-Run integration lease held. Interrupted captures
// are restored create-only; completed deletions must never be resurrected.
#[cfg(test)]
fn recover_publications(source: &Path) -> Result<(), String> {
    recover_publications_cancellable(source, &std::sync::atomic::AtomicBool::new(false))
}

fn recover_publications_cancellable(
    source: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    let pending = pending_publications(source)?;
    let entries = match std::fs::read_dir(&pending) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    for entry in entries {
        task_workspace::check_cancel(cancel)?;
        let path = entry.map_err(|e| e.to_string())?.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.len() > 65536 {
            return Err("統合の回復記録が不正です".into());
        }
        let record: PublicationRecord =
            serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        if !record
            .directory
            .starts_with(&format!("{}/publications/", task_workspace::ROOT))
        {
            return Err("統合の退避先が不正です".into());
        }
        let done = task_workspace::checked_root(source, &format!("{}/done", record.directory))?;
        let completed = match std::fs::symlink_metadata(&done) {
            Ok(meta) if meta.is_file() => true,
            Ok(_) => return Err("統合の完了記録が不正です".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.to_string()),
        };
        let backup =
            task_workspace::checked_root(source, &format!("{}/original", record.directory))?;
        if !completed {
            match std::fs::symlink_metadata(&backup) {
                Ok(meta) if meta.is_file() => {
                    let target = checked_file(source, &record.relative)?;
                    std::fs::create_dir_all(target.parent().ok_or("復元先の親がありません")?)
                        .map_err(|e| e.to_string())?;
                    checked_file(source, &record.relative)?;
                    match std::fs::hard_link(&backup, &target) {
                        Ok(()) => sync_directory(target.parent().ok_or("復元先の親がありません")?)?,
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Ok(_) => return Err("退避した統合元が通常ファイルではありません".into()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        std::fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    sync_directory(&pending)
}

fn has_descendant<T>(entries: &std::collections::BTreeMap<String, T>, relative: &str) -> bool {
    let prefix = format!("{relative}/");
    entries
        .range(prefix.clone()..)
        .next()
        .is_some_and(|(path, _)| path.starts_with(&prefix))
}

// Inspect every actual child, including ignored files and links. A replacement
// must not hide a concurrent user's content behind the workspace exclusion policy.
fn check_replacement_tree(
    source: &Path,
    relative: &str,
    baseline: &task_workspace::Snapshot,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    task_workspace::check_cancel(cancel)?;
    for item in std::fs::read_dir(checked_file(source, relative)?).map_err(|e| e.to_string())? {
        let item = item.map_err(|e| e.to_string())?;
        let child = format!(
            "{relative}/{}",
            item.file_name()
                .to_str()
                .ok_or("ファイル名を表現できません")?
        );
        let path = checked_file(source, &child)?;
        let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if meta.is_dir() && has_descendant(baseline, &child) {
            check_replacement_tree(source, &child, baseline, cancel)?;
        } else {
            let conflict = || format!("置換先に追加・変更された内容があります: {child}");
            let expected = baseline.get(&child).ok_or_else(conflict)?;
            if !meta.is_file()
                || expected != &task_workspace::streamed_entry_cancellable(&path, cancel)?
            {
                return Err(conflict());
            }
        }
    }
    Ok(())
}

fn remove_replaced_directories(
    source: &Path,
    relative: &str,
    baseline: &task_workspace::Snapshot,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    // Only known ancestors of old files are eligible. remove_dir is the atomic
    // emptiness check; never recursively remove source contents or new directories.
    let mut dirs = BTreeSet::new();
    let prefix = format!("{relative}/");
    for (path, _) in baseline
        .range(prefix.clone()..)
        .take_while(|(p, _)| p.starts_with(&prefix))
    {
        for dir in Path::new(path).ancestors().skip(1) {
            if !dir.starts_with(relative) {
                break;
            }
            dirs.insert(dir.to_string_lossy().replace('\\', "/"));
        }
    }
    let mut dirs: Vec<_> = dirs.into_iter().collect();
    dirs.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
    for dir in dirs {
        task_workspace::check_cancel(cancel)?;
        let path = checked_file(source, &dir)?;
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir(&path).map_err(|e| {
                format!("空でないか変更されたディレクトリを保持しました: {dir}: {e}")
            })?,
            Ok(_) if dir == relative => {} // Already installed or a user file: compare normally.
            Ok(_) => return Err(format!("ディレクトリが変更されました: {dir}")),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

pub(super) fn record_worker_start(
    permit: &Permit,
    identity: &serde_json::Value,
) -> Result<PathBuf, String> {
    let directory = task_workspace::checked_root(
        &permit.source,
        &format!(
            "{}/publications/worker-{}-{}",
            task_workspace::ROOT,
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ),
    )?;
    std::fs::create_dir_all(directory.parent().ok_or("記録先の親がありません")?)
        .map_err(|e| e.to_string())?;
    std::fs::create_dir(&directory).map_err(|e| e.to_string())?;
    sync_record(
        &directory.join("job.json"),
        &serde_json::to_vec(identity).map_err(|e| e.to_string())?,
    )?;
    sync_directory(&directory)?;
    Ok(directory)
}

pub(super) fn record_worker_result(
    permit: &Permit,
    directory: &Path,
    outcome: &Outcome,
    error: &Option<String>,
) -> Result<(), String> {
    let relative = directory
        .strip_prefix(&permit.source)
        .map_err(|e| e.to_string())?;
    task_workspace::checked_root(&permit.source, &relative.to_string_lossy())?;
    sync_record(
        &directory.join("result.json"),
        &serde_json::to_vec(&serde_json::json!({"outcome": outcome, "error": error}))
            .map_err(|e| e.to_string())?,
    )?;
    sync_directory(directory)
}

/// Keep the permit until this call and the old writer have both finished.
/// Only differences from the immutable isolation baseline are eligible.
#[cfg(test)]
pub fn publish(
    permit: &Permit,
    source: &Path,
    assemble_files: &[String],
) -> Result<Outcome, String> {
    publish_inner(permit, source, assemble_files, |_, _| {})
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum PublishStage {
    BeforeCapture,
    AfterCapture,
    BeforeInstall,
}

#[cfg(test)]
fn publish_inner(
    permit: &Permit,
    source: &Path,
    assemble_files: &[String],
    #[cfg(test)] mut checkpoint: impl FnMut(PublishStage, &str),
) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    publish_impl(
        permit,
        source,
        assemble_files,
        &std::sync::atomic::AtomicBool::new(false),
        &mut outcome,
        #[cfg(test)]
        &mut checkpoint,
    )?;
    Ok(outcome)
}

pub(super) fn publish_cancellable(
    permit: &Permit,
    source: &Path,
    files: &[String],
    cancel: &std::sync::atomic::AtomicBool,
    outcome: &mut Outcome,
) -> Result<(), String> {
    publish_impl(
        permit,
        source,
        files,
        cancel,
        outcome,
        #[cfg(test)]
        &mut |_, _| {},
    )
}

fn publish_impl(
    permit: &Permit,
    source: &Path,
    assemble_files: &[String],
    cancel: &std::sync::atomic::AtomicBool,
    outcome: &mut Outcome,
    #[cfg(test)] checkpoint: &mut dyn FnMut(PublishStage, &str),
) -> Result<(), String> {
    task_workspace::check_cancel(cancel)?;
    let source = source
        .canonicalize()
        .map(crate::pathx::plain)
        .map_err(|e| e.to_string())?;
    if source != permit.source {
        return Err("統合先と所有権が一致しません".into());
    }
    let held = lease::with_store(&permit.store, |store| {
        store.leases.iter().any(|l| l.holder.same(&permit.holder))
    })?;
    if !held {
        return Err("統合の所有権が失われました".into());
    }
    recover_publications_cancellable(&permit.source, cancel)?;
    let (workspace, _, _) = task_workspace::execution(&source, assemble_files)?
        .ok_or("統合担当が隔離されていません")?;
    let baseline = task_workspace::baseline(&source, assemble_files)?;
    let candidate =
        task_workspace::frozen_files(&workspace, &source, assemble_files, &baseline, cancel)?;
    let paths: BTreeSet<_> = baseline.keys().chain(candidate.keys()).cloned().collect();
    let replacements: BTreeSet<_> = candidate
        .keys()
        .filter(|path| !baseline.contains_key(*path) && has_descendant(&baseline, path))
        .cloned()
        .collect();
    let mut blocked = BTreeSet::new();
    for path in &replacements {
        task_workspace::check_cancel(cancel)?;
        if checked_file(&source, path).is_ok_and(|p| p.is_dir()) {
            if let Err(why) = check_replacement_tree(&source, path, &baseline, cancel) {
                blocked.insert(path.clone());
                outcome.notes.push(format!("{path}: {why}"));
            }
        }
    }
    let candidate_entries: task_workspace::Snapshot = candidate
        .iter()
        .map(|(path, (entry, _))| {
            let mut entry = entry.clone();
            #[cfg(unix)]
            if let Entry::File {
                mode: Some(mode), ..
            } = &mut entry
            {
                *mode &= 0o777;
            }
            #[cfg(not(unix))]
            let _ = &mut entry;
            (path.clone(), entry)
        })
        .collect();
    for path in baseline
        .keys()
        .filter(|path| !candidate.contains_key(*path) && has_descendant(&candidate, path))
    {
        if checked_file(&source, path).is_ok_and(|p| p.is_dir()) {
            if let Err(why) = check_replacement_tree(&source, path, &candidate_entries, cancel) {
                blocked.insert(path.clone());
                outcome.notes.push(format!("{path}: {why}"));
            }
        }
    }
    let mut paths: Vec<_> = paths.into_iter().collect();
    // Remove old leaves first; file -> directory removes the old parent before
    // creating children. Successful deletions remain journaled on partial failure.
    paths.sort_by_cached_key(|p| {
        (
            candidate.contains_key(p),
            std::cmp::Reverse(p.matches('/').count()),
            p.clone(),
        )
    });
    for relative in paths {
        task_workspace::check_cancel(cancel)?;
        if Path::new(&relative)
            .ancestors()
            .any(|p| blocked.contains(&p.to_string_lossy().replace('\\', "/")))
        {
            outcome.conflicts.push(relative);
            continue;
        }
        let before = baseline.get(&relative);
        let after = candidate.get(&relative).map(|(entry, _)| entry);
        if before == after {
            continue;
        }
        // Compare the full mode above to detect worker changes, but publication
        // never grants special bits. This also makes retries match installed bytes.
        let mut installed = after.cloned();
        #[cfg(unix)]
        if let Some(Entry::File {
            mode: Some(mode), ..
        }) = &mut installed
        {
            *mode &= 0o777;
        }
        #[cfg(not(unix))]
        let _ = &mut installed;
        let after = installed.as_ref();
        if matches!(before, Some(Entry::Link(_))) || matches!(after, Some(Entry::Link(_))) {
            outcome.conflicts.push(relative);
            continue;
        }
        if replacements.contains(&relative) {
            let removal = remove_replaced_directories(&source, &relative, &baseline, cancel);
            if let Err(why) = removal {
                outcome.notes.push(format!("{relative}: {why}"));
                outcome.conflicts.push(relative);
                continue;
            }
        }
        // A prior attempt may already have replaced the old file by a directory.
        // Its children are still checked independently, including create-only writes.
        let old_file_is_directory = after.is_none()
            && before.is_some()
            && has_descendant(&candidate, &relative)
            && checked_file(&source, &relative).is_ok_and(|p| p.is_dir());
        let actual = match if old_file_is_directory {
            Ok(None)
        } else {
            current(&source, &relative, cancel)
        } {
            Ok(actual) => actual,
            Err(_) => {
                outcome.conflicts.push(relative);
                continue;
            }
        };
        if actual.as_ref() == after {
            outcome.changed.push(relative); // Already applied: restart/retry is idempotent.
            continue;
        }
        if actual.as_ref() != before {
            outcome.conflicts.push(relative);
            continue;
        }
        // Each publication owns a new directory; neither saved originals nor
        // temporary candidate bytes can overwrite a previous publication.
        let result = (|| -> Result<(), String> {
            let target = checked_file(&source, &relative)?;
            let publication = format!(
                "{}/publications/{}-{}",
                task_workspace::ROOT,
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let directory = task_workspace::checked_root(&source, &publication)?;
            std::fs::create_dir_all(directory.parent().ok_or("退避先の親がありません")?)
                .map_err(|e| e.to_string())?;
            std::fs::create_dir(&directory).map_err(|e| e.to_string())?;
            let record = PublicationRecord {
                relative: relative.clone(),
                directory: publication.clone(),
            };
            let record_path = directory.join("record.json");
            sync_record(
                &record_path,
                &serde_json::to_vec(&record).map_err(|e| e.to_string())?,
            )?;
            let pending = pending_publications(&source)?;
            std::fs::create_dir_all(&pending).map_err(|e| e.to_string())?;
            let pending_record = pending
                .join(directory.file_name().ok_or("回復記録名がありません")?)
                .with_extension("json");
            std::fs::hard_link(&record_path, &pending_record).map_err(|e| e.to_string())?;
            sync_directory(&directory)?;
            sync_directory(&pending)?;
            let temporary = directory.join("candidate");
            if let Some((entry, bytes)) = candidate.get(&relative) {
                use std::io::Write;
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(&temporary).map_err(|e| e.to_string())?;
                file.write_all(bytes).map_err(|e| e.to_string())?;
                #[cfg(unix)]
                if let Entry::File { mode, .. } = entry {
                    use std::os::unix::fs::PermissionsExt;
                    // Exact ordinary bits, including intentional isolated chmod.
                    // The baseline/current/captured comparisons protect concurrent chmod.
                    // Never restore setuid/setgid/sticky bits on replacement content.
                    let mode = mode.ok_or("統合候補の権限が不明です")? & 0o777;
                    file.set_permissions(std::fs::Permissions::from_mode(mode))
                        .map_err(|e| e.to_string())?;
                }
                #[cfg(not(unix))]
                let _ = entry;
                file.sync_all().map_err(|e| e.to_string())?;
            }
            #[cfg(test)]
            checkpoint(PublishStage::BeforeCapture, &relative);
            task_workspace::check_cancel(cancel)?;
            checked_file(&source, &relative)?;
            let backup = directory.join("original");
            let captured = before.is_some();
            if captured {
                // Rename captures the actual directory entry, even if an editor
                // replaced it after our comparison. Keep this inode permanently:
                // writers with an already-open descriptor may still update it.
                std::fs::rename(&target, &backup).map_err(|e| e.to_string())?;
                outcome.notes.push(format!(
                    "{relative} の反映前ファイルを {publication}/original に保持しました（開いたままの編集内容もこの退避先に残ります）"
                ));
                #[cfg(test)]
                checkpoint(PublishStage::AfterCapture, &relative);
            }
            let install = (|| -> Result<(), String> {
                if captured && entry_at(&backup)?.as_ref() != before {
                    return Err("比較後に元ファイルが更新されました".into());
                }
                #[cfg(test)]
                checkpoint(PublishStage::BeforeInstall, &relative);
                checked_file(&source, &relative)?;
                if after.is_some() {
                    std::fs::create_dir_all(target.parent().ok_or("保存先の親がありません")?)
                        .map_err(|e| e.to_string())?;
                    checked_file(&source, &relative)?;
                    // Atomic create-only: an editor creating the destination
                    // concurrently always wins. Unsupported filesystems fail closed.
                    std::fs::hard_link(&temporary, &target).map_err(|e| e.to_string())?;
                } else if std::fs::symlink_metadata(&target).is_ok() {
                    return Err("削除対象に新しいファイルが作成されました".into());
                }
                Ok(())
            })();
            if install.is_err() && captured && checked_file(&source, &relative).is_ok() {
                // Never replace a file created while the original was captured.
                // Failure leaves the preserved original at the announced location.
                let _ = std::fs::hard_link(&backup, &target);
            }
            let _ = std::fs::remove_file(&temporary);
            if install.is_ok() {
                outcome.changed.push(relative.clone());
                sync_directory(target.parent().ok_or("保存先の親がありません")?)?;
                sync_record(&directory.join("done"), b"completed")?;
                sync_directory(&directory)?;
                std::fs::remove_file(&pending_record).map_err(|e| e.to_string())?;
                sync_directory(&pending)?;
            }
            install
        })();
        if let Err(error) = result {
            outcome.notes.push(format!("{relative}: {error}"));
            outcome.conflicts.push(relative);
            continue;
        }
    }
    outcome.changed.sort();
    outcome.conflicts.sort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::team::imp::planner::{PlanInput, StaticPlanner, TeamPlanner};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(tag: &str) -> Self {
            let root = crate::test_util::unique_temp_dir("zai-integration", tag);
            std::fs::create_dir_all(&root).unwrap();
            Self(root.canonicalize().map(crate::pathx::plain).unwrap())
        }
        fn write(&self, name: &str, body: &str) {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.0.join(name)).unwrap()
        }
        fn isolate(&self, run: &str) -> (Vec<String>, PathBuf) {
            let plan = StaticPlanner
                .plan(PlanInput {
                    spec: format!(
                        "{}\nWebサイトを実装する",
                        super::super::planner::IMPLEMENTATION_ONLY
                    ),
                    source: "request".into(),
                    agent_count: 1,
                    review_required: false,
                    workspace_root: self.0.clone(),
                    roles: Vec::new(),
                })
                .unwrap();
            let mut task = plan.tasks[0].clone();
            task.files = vec![format!("{}/{run}/part-0/**", task_workspace::ROOT)];
            task_workspace::prepare(&self.0, std::slice::from_ref(&task)).unwrap();
            let (root, _, _) = task_workspace::execution(&self.0, &task.files)
                .unwrap()
                .unwrap();
            (task.files, root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn publish_replaces_directory_with_file_and_retries_idempotently() {
        let f = Fixture::new("directory-to-file");
        f.write("config/nested/settings.json", "baseline");
        let (files, work) = f.isolate("run");
        std::fs::remove_dir_all(work.join("config")).unwrap();
        std::fs::write(work.join("config"), "flat config").unwrap();
        let permit = try_acquire(&f.0, "run").unwrap().unwrap();
        for _ in 0..2 {
            let outcome = publish(&permit, &f.0, &files).unwrap();
            assert!(outcome.conflicts.is_empty(), "{outcome:?}");
            assert_eq!(f.read("config"), "flat config");
        }
    }

    #[test]
    fn publish_replaces_file_with_directory_and_retries_idempotently() {
        let f = Fixture::new("file-to-directory");
        f.write("config", "baseline");
        let (files, work) = f.isolate("run");
        std::fs::remove_file(work.join("config")).unwrap();
        std::fs::create_dir(work.join("config")).unwrap();
        std::fs::write(work.join("config/settings.json"), "nested config").unwrap();
        let permit = try_acquire(&f.0, "run").unwrap().unwrap();
        for _ in 0..2 {
            let outcome = publish(&permit, &f.0, &files).unwrap();
            assert!(outcome.conflicts.is_empty(), "{outcome:?}");
            assert_eq!(f.read("config/settings.json"), "nested config");
        }
    }

    #[test]
    fn directory_replacement_preserves_added_and_modified_children() {
        for modified in [false, true] {
            let f = Fixture::new("replacement-conflict");
            f.write("config/settings.json", "baseline");
            let (files, work) = f.isolate("run");
            std::fs::remove_dir_all(work.join("config")).unwrap();
            std::fs::write(work.join("config"), "candidate").unwrap();
            let user_path = if modified {
                "config/settings.json"
            } else {
                "config/user.txt"
            };
            f.write(user_path, "user edit");
            let permit = try_acquire(&f.0, "run").unwrap().unwrap();
            let outcome = publish(&permit, &f.0, &files).unwrap();
            assert!(outcome.conflicts.contains(&"config".into()));
            assert!(outcome.changed.is_empty());
            assert_eq!(f.read(user_path), "user edit");
            assert!(f.0.join("config/settings.json").is_file());
        }
    }

    #[test]
    fn replacement_races_preserve_user_content_and_retry_partial_publication() {
        for stage in [PublishStage::BeforeCapture, PublishStage::BeforeInstall] {
            let f = Fixture::new("replacement-race");
            f.write("config/settings.json", "baseline");
            let (files, work) = f.isolate("run");
            std::fs::remove_dir_all(work.join("config")).unwrap();
            std::fs::write(work.join("config"), "candidate").unwrap();
            let permit = try_acquire(&f.0, "run").unwrap().unwrap();
            let mut raced = false;
            let result = publish_inner(&permit, &f.0, &files, |at, path| {
                if !raced
                    && at == stage
                    && path
                        == if stage == PublishStage::BeforeCapture {
                            "config/settings.json"
                        } else {
                            "config"
                        }
                {
                    raced = true;
                    f.write(
                        if stage == PublishStage::BeforeCapture {
                            "config/user.txt"
                        } else {
                            "config"
                        },
                        "user edit",
                    );
                }
            })
            .unwrap();
            assert!(raced);
            assert!(result.conflicts.contains(&"config".into()), "{result:?}");
            assert!(result.changed.contains(&"config/settings.json".into()));
            let user_path = if stage == PublishStage::BeforeCapture {
                "config/user.txt"
            } else {
                "config"
            };
            assert_eq!(f.read(user_path), "user edit");
            // Explicitly remove this test's concurrent edit, then retry the same candidate.
            std::fs::remove_file(f.0.join(user_path)).unwrap();
            for _ in 0..2 {
                let outcome = publish(&permit, &f.0, &files).unwrap();
                assert!(outcome.conflicts.is_empty(), "{outcome:?}");
                assert_eq!(f.read("config"), "candidate");
            }
        }
    }

    #[test]
    fn interrupted_directory_replacement_recovers_and_retries() {
        for stage in [PublishStage::AfterCapture, PublishStage::BeforeInstall] {
            let f = Fixture::new("replacement-interrupted");
            f.write("config/settings.json", "baseline");
            let (files, work) = f.isolate("run");
            std::fs::remove_dir_all(work.join("config")).unwrap();
            std::fs::write(work.join("config"), "candidate").unwrap();
            let permit = try_acquire(&f.0, "run").unwrap().unwrap();
            let interrupted = std::panic::catch_unwind(|| {
                publish_inner(&permit, &f.0, &files, |at, path| {
                    if at == stage
                        && path
                            == if stage == PublishStage::AfterCapture {
                                "config/settings.json"
                            } else {
                                "config"
                            }
                    {
                        panic!("simulated interruption");
                    }
                })
            });
            assert!(interrupted.is_err());
            drop(permit);
            let permit = try_acquire(&f.0, "retry").unwrap().unwrap();
            for _ in 0..2 {
                let outcome = publish(&permit, &f.0, &files).unwrap();
                assert!(outcome.conflicts.is_empty(), "{outcome:?}");
                assert_eq!(f.read("config"), "candidate");
            }
        }
    }

    #[test]
    fn inverse_replacement_does_not_accept_a_users_new_directory() {
        let f = Fixture::new("inverse-user-directory");
        f.write("config", "baseline");
        let (files, work) = f.isolate("run");
        std::fs::remove_file(work.join("config")).unwrap();
        std::fs::create_dir(work.join("config")).unwrap();
        std::fs::write(work.join("config/settings.json"), "candidate").unwrap();
        std::fs::remove_file(f.0.join("config")).unwrap();
        f.write("config/user.txt", "personal");
        let permit = try_acquire(&f.0, "run").unwrap().unwrap();
        let outcome = publish(&permit, &f.0, &files).unwrap();
        assert!(outcome.conflicts.contains(&"config".into()));
        assert!(!f.0.join("config/settings.json").exists());
        assert_eq!(f.read("config/user.txt"), "personal");
    }

    #[test]
    fn cancellation_finishes_captured_file_and_records_partial_publication() {
        let f = Fixture::new("cancel-boundary");
        f.write("a", "old a");
        f.write("b", "old b");
        let (files, work) = f.isolate("run");
        std::fs::write(work.join("a"), "new a").unwrap();
        std::fs::write(work.join("b"), "new b").unwrap();
        let permit = try_acquire(&f.0, "run").unwrap().unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut outcome = Outcome::default();
        let result = publish_impl(
            &permit,
            &f.0,
            &files,
            &cancel,
            &mut outcome,
            &mut |at, path| {
                if at == PublishStage::AfterCapture && path == "a" {
                    cancel.store(true, Ordering::Release);
                }
            },
        );
        assert!(result.is_err());
        assert_eq!(outcome.changed, ["a"]);
        assert_eq!(f.read("a"), "new a");
        assert_eq!(f.read("b"), "old b");
        let retry = publish(&permit, &f.0, &files).unwrap();
        assert!(retry.conflicts.is_empty());
        assert_eq!(f.read("b"), "new b");
    }

    #[cfg(unix)]
    #[test]
    fn publish_preserves_private_modes_and_detects_permission_conflicts() {
        use std::os::unix::fs::PermissionsExt;
        for mode in [0o600, 0o700] {
            let f = Fixture::new("private-mode");
            f.write("file", "baseline");
            std::fs::set_permissions(f.0.join("file"), std::fs::Permissions::from_mode(mode))
                .unwrap();
            let (files, work) = f.isolate("run");
            std::fs::write(work.join("file"), "candidate").unwrap();
            std::fs::write(work.join("new"), "private").unwrap();
            std::fs::set_permissions(work.join("new"), std::fs::Permissions::from_mode(0o600))
                .unwrap();
            let permit = try_acquire(&f.0, "run").unwrap().unwrap();
            let result = publish(&permit, &f.0, &files).unwrap();
            assert!(result.conflicts.is_empty(), "{result:?}");
            assert_eq!(
                std::fs::metadata(f.0.join("file"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o7777,
                mode
            );
            assert_eq!(
                std::fs::metadata(f.0.join("new"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o7777,
                0o600
            );
        }
        for late in [false, true] {
            let f = Fixture::new("permission-conflict");
            f.write("file", "baseline");
            std::fs::set_permissions(f.0.join("file"), std::fs::Permissions::from_mode(0o600))
                .unwrap();
            let (files, work) = f.isolate("run");
            std::fs::write(work.join("file"), "candidate").unwrap();
            if !late {
                std::fs::set_permissions(f.0.join("file"), std::fs::Permissions::from_mode(0o640))
                    .unwrap();
            }
            let permit = try_acquire(&f.0, "run").unwrap().unwrap();
            let result = publish_inner(&permit, &f.0, &files, |stage, _| {
                if late && stage == PublishStage::BeforeCapture {
                    std::fs::set_permissions(
                        f.0.join("file"),
                        std::fs::Permissions::from_mode(0o640),
                    )
                    .unwrap();
                }
            })
            .unwrap();
            assert_eq!(result.conflicts, ["file"]);
            assert_eq!(f.read("file"), "baseline");
            assert_eq!(
                std::fs::metadata(f.0.join("file"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o640
            );
        }
    }

    #[test]
    fn publish_small_change_with_large_unchanged_tree() {
        for git in [false, true] {
            let f = Fixture::new("large-baseline");
            f.write("source", "committed");
            if git {
                super::super::gitinit::prepare(&f.0).unwrap();
            }
            f.write("source", "uncommitted");
            f.write("untracked", "keep");
            for name in ["asset-a", "asset-b"] {
                std::fs::File::create(f.0.join(name))
                    .unwrap()
                    .set_len(33 * 1024 * 1024)
                    .unwrap();
            }
            let (files, work) = f.isolate("run");
            assert_eq!(
                std::fs::read_to_string(work.join("source")).unwrap(),
                "uncommitted"
            );
            assert_eq!(
                std::fs::read_to_string(work.join("untracked")).unwrap(),
                "keep"
            );
            std::fs::write(work.join("source"), "candidate").unwrap();
            let permit = try_acquire(&f.0, "run").unwrap().unwrap();
            let result = publish(&permit, &f.0, &files).unwrap();
            assert_eq!(result.changed, ["source"]);
            assert!(result.conflicts.is_empty());
            assert_eq!(f.read("source"), "candidate");
            assert_eq!(f.read("untracked"), "keep");
        }
    }

    #[test]
    fn publish_excludes_only_frozen_generated_directories() {
        for git in [false, true] {
            let f = Fixture::new("generated-policy");
            f.write("source", "baseline");
            f.write("dist/source.rs", "tracked source");
            f.write("build/old", "generated original");
            if git {
                super::super::gitinit::prepare(&f.0).unwrap();
            }
            f.write(
                ".gitignore",
                "dist/\nbuild/\n.next/\n.nuxt\ntarget-msrv/\n.env\nassets/\n",
            );
            if git {
                crate::worktree::git_out(&f.0, &["rm", "--cached", "--", "build/old"]).unwrap();
            }
            f.write(".env", "private config");
            f.write(".nuxt", "ordinary file, not a directory");
            f.write("assets/local", "required asset");
            let generated = if git { ".next" } else { "target" };
            f.write(&format!("{generated}/existing"), "keep generated original");
            let (files, work) = f.isolate("run");
            assert!(!work.join(generated).exists());
            assert_eq!(
                std::fs::read_to_string(work.join(".nuxt")).unwrap(),
                "ordinary file, not a directory"
            );
            if git {
                assert!(!work.join("build").exists());
            }
            assert_eq!(
                std::fs::read_to_string(work.join(".env")).unwrap(),
                "private config"
            );
            assert_eq!(
                std::fs::read_to_string(work.join("assets/local")).unwrap(),
                "required asset"
            );
            std::fs::create_dir_all(work.join(generated)).unwrap();
            std::fs::File::create(work.join(generated).join("large"))
                .unwrap()
                .set_len(super::super::changeset::MAX_HASH_BYTES + 1)
                .unwrap();
            if git {
                std::fs::create_dir_all(work.join("target-msrv")).unwrap();
                std::fs::File::create(work.join("target-msrv/large"))
                    .unwrap()
                    .set_len(super::super::changeset::MAX_HASH_BYTES + 1)
                    .unwrap();
            }
            // Later ignore edits cannot change the frozen target set.
            std::fs::write(work.join(".gitignore"), "").unwrap();
            std::fs::write(work.join("dist/source.rs"), "edited source").unwrap();
            std::fs::write(work.join("source"), "candidate").unwrap();
            let permit = try_acquire(&f.0, "run").unwrap().unwrap();
            let result = publish(&permit, &f.0, &files).unwrap();
            assert!(result.conflicts.is_empty(), "{result:?}");
            assert_eq!(f.read("dist/source.rs"), "edited source");
            assert_eq!(f.read("source"), "candidate");
            assert_eq!(
                f.read(&format!("{generated}/existing")),
                "keep generated original"
            );
            assert!(!f.0.join(generated).join("large").exists());
            assert_eq!(f.read("build/old"), "generated original");
        }
    }

    #[test]
    fn publish_oversize_changes_are_explicitly_held_before_any_write() {
        for git in [false, true] {
            let f = Fixture::new("oversize-change");
            f.write("a-source", "baseline");
            if git {
                super::super::gitinit::prepare(&f.0).unwrap();
            }
            let (files, work) = f.isolate("run");
            std::fs::write(work.join("a-source"), "candidate").unwrap();
            std::fs::File::create(work.join("z-large"))
                .unwrap()
                .set_len(super::super::changeset::MAX_HASH_BYTES + 1)
                .unwrap();
            let permit = try_acquire(&f.0, "run").unwrap().unwrap();
            let error = publish(&permit, &f.0, &files).unwrap_err();
            assert!(error.contains("64MiB") && error.contains("保留"), "{error}");
            assert_eq!(f.read("a-source"), "baseline");
            assert!(!f.0.join("z-large").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn publish_intentional_chmod_and_legacy_baseline() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new("intentional-mode");
        f.write("file", "baseline");
        std::fs::set_permissions(f.0.join("file"), std::fs::Permissions::from_mode(0o600)).unwrap();
        let (files, work) = f.isolate("run");
        std::fs::set_permissions(work.join("file"), std::fs::Permissions::from_mode(0o710))
            .unwrap();
        let permit = try_acquire(&f.0, "run").unwrap().unwrap();
        let result = publish(&permit, &f.0, &files).unwrap();
        assert_eq!(result.changed, ["file"]);
        assert_eq!(
            std::fs::metadata(f.0.join("file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o710
        );
        assert!(publish(&permit, &f.0, &files).unwrap().conflicts.is_empty());
        let record = work.with_extension("ready.json");
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        json["baseline"]["file"]["File"]
            .as_object_mut()
            .unwrap()
            .remove("mode");
        json.as_object_mut().unwrap().remove("excluded");
        std::fs::write(&record, serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(task_workspace::execution(&f.0, &files).unwrap().is_some());
        assert!(publish(&permit, &f.0, &files)
            .unwrap_err()
            .contains("権限の基準点"));
        assert_eq!(f.read("file"), "baseline");
    }

    #[cfg(unix)]
    #[test]
    fn publish_freezes_bytes_and_drops_special_bits() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new("frozen-private-candidate");
        f.write("script", "baseline");
        let (files, work) = f.isolate("run");
        std::fs::write(work.join("script"), "frozen candidate").unwrap();
        std::fs::set_permissions(work.join("script"), std::fs::Permissions::from_mode(0o6700))
            .unwrap();
        let permit = try_acquire(&f.0, "run").unwrap().unwrap();
        let result = publish_inner(&permit, &f.0, &files, |stage, _| {
            if stage == PublishStage::BeforeInstall {
                std::fs::write(work.join("script"), "late worker edit").unwrap();
                std::fs::set_permissions(
                    work.join("script"),
                    std::fs::Permissions::from_mode(0o777),
                )
                .unwrap();
            }
        })
        .unwrap();
        assert_eq!(result.changed, ["script"]);
        assert_eq!(f.read("script"), "frozen candidate");
        assert_eq!(
            std::fs::metadata(f.0.join("script"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
    }

    #[test]
    fn 捕捉直後の中断は次の所有権取得で復元し正常削除は戻さない() {
        let f = Fixture::new("capture-recovery");
        f.write("file", "baseline");
        let (files, work) = f.isolate("run");
        std::fs::remove_file(work.join("file")).unwrap();
        let permit = try_acquire(&f.0, "run").unwrap().unwrap();
        let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = publish_inner(&permit, &f.0, &files, |stage, _| {
                if stage == PublishStage::AfterCapture {
                    panic!("simulated crash");
                }
            });
        }));
        assert!(interrupted.is_err());
        assert!(!f.0.join("file").exists());
        drop(permit);
        let next = try_acquire(&f.0, "restart").unwrap().unwrap();
        assert_eq!(f.read("file"), "baseline");
        let result = publish(&next, &f.0, &files).unwrap();
        assert_eq!(result.changed, ["file"]);
        assert!(!f.0.join("file").exists());
        drop(next);
        let _last = try_acquire(&f.0, "again").unwrap().unwrap();
        assert!(!f.0.join("file").exists());
        assert_eq!(
            std::fs::read_dir(pending_publications(&f.0).unwrap())
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn 比較後の更新は置換でも削除でも捕捉して元へ戻す() {
        for delete in [false, true] {
            let f = Fixture::new("capture-edit");
            f.write("file", "baseline");
            let (files, work) = f.isolate("run");
            if delete {
                std::fs::remove_file(work.join("file")).unwrap();
            } else {
                std::fs::write(work.join("file"), "candidate").unwrap();
            }
            let permit = try_acquire(&f.0, "run").unwrap().unwrap();
            let result = publish_inner(&permit, &f.0, &files, |stage, relative| {
                if stage == PublishStage::BeforeCapture {
                    f.write(relative, "late user edit");
                }
            })
            .unwrap();
            assert_eq!(result.conflicts, ["file"]);
            assert!(result.changed.is_empty());
            assert_eq!(f.read("file"), "late user edit");
            assert!(result.notes.iter().any(|n| n.contains("/original")));
        }
    }

    #[test]
    fn 新規公開と復元は割り込んだユーザーファイルを上書きしない() {
        for existing in [false, true] {
            let f = Fixture::new("create-race");
            if existing {
                f.write("file", "baseline");
            }
            let (files, work) = f.isolate("run");
            std::fs::write(work.join("file"), "candidate").unwrap();
            let permit = try_acquire(&f.0, "run").unwrap().unwrap();
            let result = publish_inner(&permit, &f.0, &files, |stage, relative| {
                if stage == PublishStage::BeforeInstall {
                    f.write(relative, "new user file");
                }
            })
            .unwrap();
            assert_eq!(result.conflicts, ["file"]);
            assert_eq!(f.read("file"), "new user file");
            if existing {
                let saved = std::fs::read_dir(f.0.join(task_workspace::ROOT).join("publications"))
                    .unwrap()
                    .map(|entry| entry.unwrap().path().join("original"))
                    .find(|path| path.is_file())
                    .unwrap();
                assert_eq!(std::fs::read_to_string(saved).unwrap(), "baseline");
            }
        }
    }

    #[test]
    fn 公開後も古い開いたファイルへの編集を退避先に保持する() {
        use std::io::{Seek, SeekFrom, Write};
        let f = Fixture::new("open-writer");
        f.write("file", "baseline");
        let (files, work) = f.isolate("run");
        std::fs::write(work.join("file"), "candidate").unwrap();
        let mut old = std::fs::OpenOptions::new()
            .write(true)
            .open(f.0.join("file"))
            .unwrap();
        let permit = try_acquire(&f.0, "run").unwrap().unwrap();
        let result = publish(&permit, &f.0, &files).unwrap();
        assert_eq!(result.changed, ["file"]);
        old.seek(SeekFrom::Start(0)).unwrap();
        old.write_all(b"user after publish").unwrap();
        old.sync_all().unwrap();
        assert_eq!(f.read("file"), "candidate");
        let saved = std::fs::read_dir(f.0.join(task_workspace::ROOT).join("publications"))
            .unwrap()
            .map(|entry| entry.unwrap().path().join("original"))
            .find(|path| path.is_file())
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(saved).unwrap(),
            "user after publish"
        );
        assert!(result.notes.iter().any(|n| n.contains("/original")));
    }

    #[test]
    fn 終了済みセッションの不明pidは保存したwriterを消さない() {
        let f = Fixture::new("known-writer");
        let mut permit = try_acquire(&f.0, "run").unwrap().unwrap();
        permit.handoff_writer(Some(std::process::id())).unwrap();
        permit.handoff_writer(None).unwrap();
        permit.handoff_writer(Some(0)).unwrap();
        lease::with_store(&permit.store, |state| {
            assert_eq!(state.leases[0].holder.pid, std::process::id());
            assert_eq!(state.leases[0].holder.agent, RETIRED);
        })
        .unwrap();
        assert!(try_acquire(&f.0, "next").unwrap().is_none());
        drop(permit);
        assert!(try_acquire(&f.0, "next").unwrap().is_some());
    }

    #[test]
    fn 終了待ちへ移した所有権は実プロセスと完了札を保持する() {
        let f = Fixture::new("shutdown");
        let permit = try_acquire(&f.0, "closing").unwrap().unwrap();
        let handle = crate::terminal::ReapHandle::for_test_process(std::process::id());
        permit.retire(handle.clone()).unwrap();
        assert!(try_acquire(&f.0, "next").unwrap().is_none());
        let store = f.0.join(task_workspace::ROOT).join("integration.json");
        lease::with_store(&store, |state| {
            assert_eq!(state.leases[0].holder.agent, RETIRED);
            assert_eq!(state.leases[0].holder.pid, std::process::id());
        })
        .unwrap();
        handle.finish_for_test();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if try_acquire(&f.0, "next").unwrap().is_some() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "終了札後に所有権が解放されなかった"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn writer_process_probe() {
        let Some(path) = std::env::var_os("ZAI_INTEGRATION_WRITER_PROBE") else {
            return;
        };
        std::fs::write(path, "ready").unwrap();
        use std::io::Read;
        let _ = std::io::stdin().read(&mut [0u8; 1]);
    }

    fn check_writer_lifecycle(publisher_alive: bool) {
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let f = Fixture::new("surviving-writer");
        let ready = f.0.join("writer-ready");
        let relative_module = module_path!()
            .split_once("::")
            .map(|(_, path)| path)
            .unwrap_or(module_path!());
        let probe_name = format!("{relative_module}::writer_process_probe");
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &probe_name, "--nocapture"])
                .env("ZAI_INTEGRATION_WRITER_PROBE", &ready)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "writerが起動前に終了した"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "writerの起動待ちが終了しなかった"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut permit = try_acquire(&f.0, "old-app").unwrap().unwrap();
        #[cfg(unix)]
        permit.handoff_writer(Some(child.0.id())).unwrap();
        #[cfg(windows)]
        {
            // The probe has acknowledged readiness and is blocked on stdin;
            // unlike an arbitrary CLI it cannot fork before test assignment.
            let job = crate::terminal::writer_tree::windows::Job::for_test_assign(child.0.id());
            let tree = crate::terminal::writer_tree::Tree::register(Some(child.0.id()), job);
            permit
                .handoff_identity(crate::terminal::writer_tree::Identity {
                    pid: child.0.id(),
                    tree: Some(tree),
                })
                .unwrap();
        }
        let permit = if publisher_alive {
            Some(permit)
        } else {
            let dead_pid = u32::MAX / 2;
            assert!(!crate::instances::pid_alive(dead_pid));
            lease::with_store(&permit.store, |state| {
                let owner = state
                    .leases
                    .iter_mut()
                    .find(|lease| lease.holder.same(&permit.holder))
                    .unwrap();
                // Simulate the app process having disappeared while its writer survived.
                let token = format!("old-app:{dead_pid}:1");
                owner.holder.session = owner
                    .holder
                    .session
                    .split_once('|')
                    .map_or_else(|| token.clone(), |(job, _)| format!("{job}|{token}"));
            })
            .unwrap();
            std::mem::forget(permit);
            None
        };
        assert!(try_acquire(&f.0, "new-app").unwrap().is_none());
        drop(child.0.stdin.take());
        child.0.wait().unwrap();
        if publisher_alive {
            assert!(
                try_acquire(&f.0, "new-app").unwrap().is_none(),
                "writer終了だけでpublish中のappから所有権を奪った"
            );
            drop(permit);
        }
        assert!(try_acquire(&f.0, "new-app").unwrap().is_some());
    }

    #[cfg(windows)]
    #[test]
    fn legacy_parent_only_writer_identity_is_not_completion_proof() {
        let f = Fixture::new("legacy-writer");
        let permit = try_acquire(&f.0, "old-app").unwrap().unwrap();
        let dead = u32::MAX / 2;
        assert!(!crate::instances::pid_alive(dead));
        lease::with_store(&permit.store, |state| {
            state.leases[0].holder.agent = RETIRED.into();
            state.leases[0].holder.pid = dead;
            state.leases[0].holder.session = format!("old-app:{dead}:1");
        })
        .unwrap();
        drop(permit); // The old token no longer matches the persisted legacy owner.
        assert!(try_acquire(&f.0, "new-app").unwrap().is_none());
    }

    #[test]
    fn アプリ終了相当の退避後も子が生存中は回収せず終了後に回収する() {
        check_writer_lifecycle(false);
    }

    #[test]
    fn 子が先に終了しても統合中のアプリから所有権を奪わない() {
        check_writer_lifecycle(true);
    }

    #[test]
    fn 隔離担当は同じpartだけを排他し別担当を並列に取得できる() {
        let f = Fixture::new("parts");
        let a = vec![format!("{}/run-parts/part-1/**", task_workspace::ROOT)];
        let b = vec![format!("{}/run-parts/part-2/**", task_workspace::ROOT)];
        let first = try_acquire_task(&f.0, &a, "original").unwrap().unwrap();
        assert!(try_acquire_task(&f.0, &a, "restored").unwrap().is_none());
        let other = try_acquire_task(&f.0, &b, "parallel").unwrap().unwrap();
        drop(first);
        assert!(try_acquire_task(&f.0, &a, "restored").unwrap().is_some());
        assert!(try_acquire_task(&f.0, &b, "another").unwrap().is_none());
        drop(other);
    }

    #[test]
    fn 同じファイルの二つのrunは先行成果を上書きしない() {
        let f = Fixture::new("same");
        f.write("shared.txt", "original");
        let (a, wa) = f.isolate("run-a");
        let (b, wb) = f.isolate("run-b");
        std::fs::write(wa.join("shared.txt"), "first").unwrap();
        std::fs::write(wb.join("shared.txt"), "second").unwrap();
        let first = try_acquire(&f.0, "a").unwrap().unwrap();
        assert!(try_acquire(&f.0, "b").unwrap().is_none());
        assert_eq!(publish(&first, &f.0, &a).unwrap().changed, ["shared.txt"]);
        drop(first);
        let second = try_acquire(&f.0, "b").unwrap().unwrap();
        assert_eq!(
            publish(&second, &f.0, &b).unwrap().conflicts,
            ["shared.txt"]
        );
        assert_eq!(f.read("shared.txt"), "first");
    }

    #[test]
    fn 異なるファイルの二つのrunは変更だけを統合し未追跡を保持する() {
        let f = Fixture::new("different");
        f.write("one.txt", "one");
        f.write("two.txt", "two");
        f.write("untracked.txt", "keep");
        let (a, wa) = f.isolate("run-a");
        let (b, wb) = f.isolate("run-b");
        std::fs::write(wa.join("one.txt"), "first").unwrap();
        std::fs::write(wb.join("two.txt"), "second").unwrap();
        let p = try_acquire(&f.0, "a").unwrap().unwrap();
        assert!(publish(&p, &f.0, &a).unwrap().conflicts.is_empty());
        drop(p);
        let p = try_acquire(&f.0, "b").unwrap().unwrap();
        let result = publish(&p, &f.0, &b).unwrap();
        assert_eq!(result.changed, ["two.txt"]);
        assert!(result.conflicts.is_empty());
        let repeated = publish(&p, &f.0, &b).unwrap();
        assert_eq!(repeated.changed, result.changed);
        assert_eq!(repeated.conflicts, result.conflicts);
        assert!(repeated.notes.is_empty());
        assert_eq!(f.read("one.txt"), "first");
        assert_eq!(f.read("two.txt"), "second");
        assert_eq!(f.read("untracked.txt"), "keep");
    }

    #[test]
    fn ユーザーの更新削除追加と削除対象の更新を保護する() {
        let f = Fixture::new("user");
        for name in ["edited", "removed", "delete-conflict", "delete-ok"] {
            f.write(name, "original");
        }
        let (files, work) = f.isolate("run-user");
        std::fs::write(work.join("edited"), "agent").unwrap();
        std::fs::write(work.join("removed"), "agent").unwrap();
        std::fs::write(work.join("added"), "agent").unwrap();
        std::fs::remove_file(work.join("delete-conflict")).unwrap();
        std::fs::remove_file(work.join("delete-ok")).unwrap();
        f.write("edited", "person");
        f.write("added", "person");
        f.write("delete-conflict", "person");
        std::fs::remove_file(f.0.join("removed")).unwrap();
        let permit = try_acquire(&f.0, "run-user").unwrap().unwrap();
        let result = publish(&permit, &f.0, &files).unwrap();
        assert_eq!(
            result.conflicts,
            ["added", "delete-conflict", "edited", "removed"]
        );
        assert_eq!(result.changed, ["delete-ok"]);
        assert_eq!(f.read("edited"), "person");
        assert_eq!(f.read("added"), "person");
        assert_eq!(f.read("delete-conflict"), "person");
        assert!(!f.0.join("removed").exists());
        assert!(!f.0.join("delete-ok").exists());
    }

    #[test]
    fn 所有権は停止待ち中に保持され解放後に再取得できる() {
        let f = Fixture::new("stop");
        let p = try_acquire(&f.0, "first").unwrap().unwrap();
        for _ in 0..3 {
            assert!(try_acquire(&f.0, "retry").unwrap().is_none());
        }
        drop(p);
        let p = try_acquire(&f.0, "retry").unwrap().unwrap();
        assert!(try_acquire(&f.0, "third").unwrap().is_none());
        drop(p);
        assert!(try_acquire(&f.0, "third").unwrap().is_some());
    }

    #[test]
    fn 死亡した専用統合所有者だけを期限前に回収する() {
        let f = Fixture::new("dead");
        let p = try_acquire(&f.0, "first").unwrap().unwrap();
        lease::with_store(&p.store, |s| {
            s.leases[0].holder.pid = u32::MAX / 2;
        })
        .unwrap();
        assert!(!crate::instances::pid_alive(u32::MAX / 2));
        let reclaimed = try_acquire(&f.0, "second").unwrap().unwrap();
        drop(p); // The stale guard cannot release the new owner's lease.
        assert!(try_acquire(&f.0, "third").unwrap().is_none());
        drop(reclaimed);
    }

    #[test]
    fn gitありと初回コミット前でも現在の未コミット内容を基準にする() {
        for committed in [false, true] {
            let f = Fixture::new(if committed { "git" } else { "unborn" });
            let git = |args: &[&str]| {
                let output = std::process::Command::new("git")
                    .current_dir(&f.0)
                    .args([
                        "-c",
                        "core.fsmonitor=false",
                        "-c",
                        "user.name=Test",
                        "-c",
                        "user.email=test@example.invalid",
                        "-c",
                        "commit.gpgsign=false",
                    ])
                    .args(args)
                    .output()
                    .expect("Git is required by the CI test environment");
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            };
            git(&["init"]);
            f.write("tracked.txt", "committed");
            if committed {
                git(&["add", "tracked.txt"]);
                git(&[
                    "-c",
                    "core.hooksPath=disabled-hooks",
                    "commit",
                    "-m",
                    "initial",
                ]);
            }
            f.write("tracked.txt", "uncommitted");
            f.write("untracked.txt", "personal");
            let (files, work) = f.isolate("run-git");
            assert_eq!(
                std::fs::read_to_string(work.join("tracked.txt")).unwrap(),
                "uncommitted"
            );
            assert_eq!(
                task_workspace::execution(&f.0, &files).unwrap().unwrap().2,
                committed
            );
            std::fs::write(work.join("tracked.txt"), "implemented").unwrap();
            let permit = try_acquire(&f.0, "git").unwrap().unwrap();
            assert_eq!(
                publish(&permit, &f.0, &files).unwrap().changed,
                ["tracked.txt"]
            );
            assert_eq!(f.read("tracked.txt"), "implemented");
            assert_eq!(f.read("untracked.txt"), "personal");
        }
    }

    #[test]
    fn 中断した準備は既存担当の基準点をユーザー更新へ置き換えない() {
        let f = Fixture::new("partial");
        f.write("shared", "original");
        let (files, work) = f.isolate("run-partial");
        f.write("shared", "user changed");
        let plan = StaticPlanner
            .plan(PlanInput {
                spec: format!(
                    "{}\nファイルを実装する",
                    super::super::planner::IMPLEMENTATION_ONLY
                ),
                source: "request".into(),
                agent_count: 1,
                review_required: false,
                workspace_root: f.0.clone(),
                roles: Vec::new(),
            })
            .unwrap();
        let mut old = plan.tasks[0].clone();
        old.files = files;
        let mut new = old.clone();
        new.files = vec![format!("{}/run-partial/part-1/**", task_workspace::ROOT)];
        assert!(task_workspace::prepare(&f.0, &[old, new]).is_err());
        assert_eq!(f.read("shared"), "user changed");
        assert_eq!(
            std::fs::read_to_string(work.join("shared")).unwrap(),
            "original"
        );
    }

    #[test]
    fn 解放書込みの失敗は後続取得時に再試行する() {
        let f = Fixture::new("release-retry");
        let p = try_acquire(&f.0, "first").unwrap().unwrap();
        let store = p.store.clone();
        let valid = std::fs::read(&store).unwrap();
        std::fs::write(&store, "broken json").unwrap();
        drop(p);
        std::fs::write(&store, valid).unwrap();
        assert!(try_acquire(&f.0, "second").unwrap().is_some());
    }

    #[test]
    fn 基準点のない旧隔離先は内容を保持して統合を拒否する() {
        let f = Fixture::new("legacy");
        f.write("keep", "original");
        let (files, work) = f.isolate("run-old");
        std::fs::write(work.join("keep"), "candidate").unwrap();
        let record =
            f.0.join(task_workspace::scope(&files).unwrap())
                .with_extension("ready.json");
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        json.as_object_mut().unwrap().remove("baseline");
        std::fs::write(record, serde_json::to_vec(&json).unwrap()).unwrap();
        let permit = try_acquire(&f.0, "old").unwrap().unwrap();
        assert!(publish(&permit, &f.0, &files).is_err());
        assert_eq!(f.read("keep"), "original");
        assert_eq!(
            std::fs::read_to_string(work.join("keep")).unwrap(),
            "candidate"
        );
    }

    #[cfg(unix)]
    #[test]
    fn リンクとリンク化した祖先へは統合しない() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new("links");
        let outside = Fixture::new("outside");
        f.write("folder/file", "before");
        f.write("other", "before");
        outside.write("file", "private");
        outside.write("other", "private");
        let (files, work) = f.isolate("run-links");
        std::fs::write(work.join("folder/file"), "agent").unwrap();
        std::fs::write(work.join("other"), "agent").unwrap();
        std::fs::remove_dir_all(f.0.join("folder")).unwrap();
        symlink(&outside.0, f.0.join("folder")).unwrap();
        std::fs::remove_file(f.0.join("other")).unwrap();
        symlink(outside.0.join("other"), f.0.join("other")).unwrap();
        symlink(outside.0.join("file"), work.join("new-link")).unwrap();
        let permit = try_acquire(&f.0, "links").unwrap().unwrap();
        assert_eq!(
            publish(&permit, &f.0, &files).unwrap().conflicts,
            ["folder/file", "new-link", "other"]
        );
        assert_eq!(outside.read("file"), "private");
        assert_eq!(outside.read("other"), "private");
    }
}
