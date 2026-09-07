//! A terminal's display lifetime is shorter than its writer lifetime.
//! Supported Unix targets admit writers through a kernel-backed supervisor.
//! UI code only reads completion or requests cancellation; the wait worker
//! owns process observation and reaping.
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
#[path = "writer_tree/windows.rs"]
pub mod windows;
#[cfg(target_os = "linux")]
pub use linux as unix;
#[cfg(target_os = "macos")]
pub use macos as unix;

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
    #[cfg(all(test, unix))]
    pub observation_epoch: std::sync::atomic::AtomicU64,
    parent_exited: AtomicBool,
    stop: AtomicBool,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    supervisor: Option<unix::Prepared>,
    #[cfg(unix)]
    pinned: Mutex<Option<u32>>,
    #[cfg(windows)]
    pub job: windows::Job,
}
#[cfg(test)]
static TREES: Mutex<BTreeMap<u32, Weak<Tree>>> = Mutex::new(BTreeMap::new());

impl Tree {
    #[cfg(any(test, not(any(target_os = "linux", target_os = "macos"))))]
    pub fn register(pid: Option<u32>, #[cfg(windows)] job: windows::Job) -> Arc<Self> {
        Self::register_inner(
            pid,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            None,
            #[cfg(windows)]
            job,
        )
    }
    fn register_inner(
        pid: Option<u32>,
        #[cfg(any(target_os = "linux", target_os = "macos"))] supervisor: Option<unix::Prepared>,
        #[cfg(windows)] job: windows::Job,
    ) -> Arc<Self> {
        let tree = Arc::new(Self {
            done: AtomicBool::new(false),
            #[cfg(all(test, unix))]
            observation_epoch: std::sync::atomic::AtomicU64::new(0),
            parent_exited: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            supervisor,
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
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn supervised(pid: Option<u32>, prepared: unix::Prepared) -> Arc<Self> {
        Self::register_inner(pid, Some(prepared))
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn proof(&self) -> Option<String> {
        self.supervisor.as_ref().map(unix::Prepared::proof)
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        #[cfg(windows)]
        self.job.request_stop();
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
        self.signal_pinned(|pid| {
            // A kernel signal only: no process lookup or completion wait on UI.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        });
    }
    #[cfg(all(unix, any(test, not(any(target_os = "linux", target_os = "macos")))))]
    fn signal_pinned(&self, mut signal: impl FnMut(u32)) {
        if let Ok(pinned) = self.pinned.try_lock() {
            if let Some(pid) = *pinned {
                signal(pid);
            }
        }
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
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

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
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
            let writers = group_has_writers(pid);
            if writers == Some(false) {
                let mut pinned = tree.pinned.lock().unwrap_or_else(|e| e.into_inner());
                *pinned = None;
                let status = child.wait()?;
                tree.done.store(true, Ordering::Release);
                #[cfg(test)]
                tree.observation_epoch.fetch_add(1, Ordering::Release);
                return Ok(status);
            }
            #[cfg(test)]
            tree.observation_epoch.fetch_add(1, Ordering::Release);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn wait(
    child: &mut (dyn portable_pty::Child + Send + Sync),
    tree: &Tree,
    exited: &AtomicBool,
) -> std::io::Result<portable_pty::ExitStatus> {
    let prepared = tree
        .supervisor
        .as_ref()
        .ok_or_else(|| std::io::Error::other("writer supervisor unavailable"))?;
    let pid = child
        .process_id()
        .ok_or_else(|| std::io::Error::other("writer supervisor identity unavailable"))?;
    let supervisor = prepared.attach(pid);
    loop {
        if let Ok(supervisor) = &supervisor {
            if tree.stop.load(Ordering::Acquire) {
                supervisor.request_stop();
            }
            match supervisor.poll() {
                Ok(observation) => {
                    if observation.parent_exited {
                        tree.parent_exited.store(true, Ordering::Release);
                        exited.store(true, Ordering::Release);
                    }
                    if observation.done {
                        break;
                    }
                    #[cfg(test)]
                    tree.observation_epoch.fetch_add(1, Ordering::Release);
                }
                Err(_) if !unix::persisted_alive(&prepared.proof()) => break,
                Err(_) => {}
            }
        } else if !unix::persisted_alive(&prepared.proof()) {
            // Admission failed, but the supervisor proved it had no writers.
            break;
        }
        // A channel/OS error is unknown. Retry; only kernel-backed proof closes custody.
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    *tree.pinned.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let status = child.wait();
    tree.parent_exited.store(true, Ordering::Release);
    exited.store(true, Ordering::Release);
    tree.done.store(true, Ordering::Release);
    #[cfg(test)]
    tree.observation_epoch.fetch_add(1, Ordering::Release);
    status
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
