//! A terminal's display lifetime is shorter than its writer lifetime.
//! The wait thread owns the unreaped Unix leader, so its PID/PGID cannot be
//! recycled while descendants are inspected or signalled. UI code only reads
//! completion or requests cancellation; it never waits for a process scan.
#[cfg(windows)]
#[path = "writer_tree/windows.rs"]
pub mod windows;

#[cfg(test)]
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(any(unix, test))]
use std::sync::Mutex;
#[cfg(test)]
use std::sync::Weak;

/// Capture identity from the actual Session, never by looking up a possibly
/// recycled PID at delivery time.
pub struct Identity {
    pub pid: u32,
    pub tree: Option<Arc<Tree>>,
}
impl Identity {
    #[cfg(test)]
    pub fn for_test(pid: u32) -> Option<Self> {
        Some(Self {
            pid,
            tree: Tree::lookup(pid),
        })
    }
}

#[derive(Debug)]
pub struct Tree {
    pub done: AtomicBool,
    parent_exited: AtomicBool,
    stop: AtomicBool,
    #[cfg(unix)]
    pinned: Mutex<Option<u32>>,
    #[cfg(windows)]
    pub job: windows::Job,
}
#[cfg(test)]
static TREES: Mutex<BTreeMap<u32, Weak<Tree>>> = Mutex::new(BTreeMap::new());

impl Tree {
    pub fn register(pid: Option<u32>, #[cfg(windows)] job: windows::Job) -> Arc<Self> {
        let tree = Arc::new(Self {
            done: AtomicBool::new(false),
            parent_exited: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            #[cfg(unix)]
            pinned: Mutex::new(pid),
            #[cfg(windows)]
            job,
        });
        #[cfg(all(windows, not(test)))]
        let _ = pid;
        #[cfg(test)]
        if let Some(pid) = pid {
            let mut registry = TREES.lock().unwrap_or_else(|e| e.into_inner());
            registry.retain(|_, tree| tree.strong_count() != 0);
            registry.insert(pid, Arc::downgrade(&tree));
        }
        tree
    }
    #[cfg(test)]
    pub fn lookup(pid: u32) -> Option<Arc<Self>> {
        TREES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&pid)
            .and_then(Weak::upgrade)
    }
    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        #[cfg(windows)]
        self.job.request_stop();
        #[cfg(unix)]
        self.signal_pinned(|pid| {
            // A kernel signal only: no process lookup or completion wait on UI.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        });
    }
    pub fn draining(&self) -> bool {
        self.parent_exited.load(Ordering::Acquire) && !self.finished()
    }
    #[cfg(unix)]
    fn signal_pinned(&self, mut signal: impl FnMut(u32)) {
        if let Ok(pinned) = self.pinned.try_lock() {
            if let Some(pid) = *pinned {
                signal(pid);
            }
        }
    }
}

#[cfg(unix)]
fn group_has_writers(pid: u32) -> Option<bool> {
    // ps works on both Darwin and Linux; this is exclusively a wait-thread call.
    // Ignore zombies: they cannot write and orphan reaping need not delay a Run.
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,pgid=,stat="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&output.stdout).ok()?;
    let mut found = false;
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let member: u32 = words.next()?.parse().ok()?;
        let group: u32 = words.next()?.parse().ok()?;
        let state = words.next()?;
        if member != pid && group == pid && !state.starts_with('Z') {
            found = true;
        }
    }
    Some(found)
}

#[cfg(unix)]
pub fn wait(
    child: &mut (dyn portable_pty::Child + Send + Sync),
    tree: &Tree,
    exited: &AtomicBool,
) -> std::io::Result<portable_pty::ExitStatus> {
    let pid = child
        .process_id()
        .filter(|p| *p > 0 && *p <= i32::MAX as u32)
        .ok_or_else(|| std::io::Error::other("PTY writer identity unavailable"))?;
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // WNOWAIT pins the leader even after exit. Only this thread may reap it.
        let result =
            unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            *tree.pinned.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return Err(error); // Unknown is never completion and never a kill target.
        }
        if tree.stop.load(Ordering::Acquire) {
            // The successful waitid above proves this is still our unreaped child.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        if unsafe { info.si_pid() } != 0 {
            tree.parent_exited.store(true, Ordering::Release);
            exited.store(true, Ordering::Release);
            if group_has_writers(pid) == Some(false) {
                let mut pinned = tree.pinned.lock().unwrap_or_else(|e| e.into_inner());
                *pinned = None;
                let status = child.wait()?;
                tree.done.store(true, Ordering::Release);
                return Ok(status);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(windows)]
pub fn wait(
    child: &mut (dyn portable_pty::Child + Send + Sync),
    tree: &Tree,
    exited: &AtomicBool,
) -> std::io::Result<portable_pty::ExitStatus> {
    let process = child
        .as_raw_handle()
        .ok_or_else(|| std::io::Error::other("PTY process handle unavailable"))?;
    // Idle CLIs sleep in the kernel. Stop wakes this worker without a UI join.
    tree.job.wait_parent_or_stop(process)?;
    if tree.stop.load(Ordering::Acquire) {
        tree.job.stop();
        child.kill()?; // Owned process handle, never a recycled PID lookup.
    }
    let status = child.wait()?;
    tree.parent_exited.store(true, Ordering::Release);
    exited.store(true, Ordering::Release);
    while tree.job.quiescent() != Some(true) {
        if tree.stop.load(Ordering::Acquire) {
            tree.job.stop();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    tree.done.store(true, Ordering::Release);
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn recycled_pid_does_not_retarget_an_old_tree() {
        let old = Tree::register(Some(42));
        // Exactly the same transition as the wait thread, before child.wait().
        *old.pinned.lock().unwrap() = None;
        old.done.store(true, Ordering::Release);
        let replacement = Tree::register(Some(42));
        let mut signalled = vec![];
        old.signal_pinned(|pid| signalled.push(pid));
        assert!(signalled.is_empty());
        replacement.signal_pinned(|pid| signalled.push(pid));
        assert_eq!(signalled, vec![42]);
        assert!(Arc::ptr_eq(&Tree::lookup(42).unwrap(), &replacement));
        assert!(old.finished());
        assert!(!replacement.finished());
    }

    #[test]
    fn ordinary_parent_exit_completes_writer_and_reaper() {
        let session = crate::terminal::Session::spawn(
            812346,
            crate::terminal::SpawnSpec {
                title: "natural exit".into(),
                preset_name: "probe".into(),
                icon: String::new(),
                command: "exit 0".into(),
                cwd: std::env::temp_dir(),
                env: Default::default(),
                log_path: None,
            },
            eframe::egui::Context::default(),
        )
        .unwrap();
        let tree = session.writer_tree.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !tree.finished() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(session.exited.load(Ordering::Acquire));
        assert_eq!(session.live_process_id(), None);
        let handle = crate::terminal::reap_tracked(session);
        while !handle.is_finished() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
}
