//! Child ownership, bounded output, and an explicit environment allowlist.
use super::private::Result;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(super) fn command(path: &Path) -> Command {
    let mut command = Command::new(path);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    for name in [
        "HOME",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "ZAIVERN_HOME",
        "DOCKER_HOST",
        "DOCKER_CONTEXT",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "DISPLAY",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    command
}

#[cfg(test)]
pub(super) struct OwnedChild {
    pub(super) child: Child,
    reaped: bool,
}
#[cfg(test)]
impl OwnedChild {
    pub(super) fn spawn(command: &mut Command) -> Result<Self> {
        Ok(Self {
            child: command.spawn().map_err(|_| {
                "Cannot start child process; check executable permissions and macOS quarantine"
            })?,
            reaped: false,
        })
    }
    pub(super) fn exited(&mut self) -> Result<Option<std::process::ExitStatus>> {
        let status = self
            .child
            .try_wait()
            .map_err(|_| "Cannot inspect owned child")?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }
    pub(super) fn stop(&mut self) {
        // The unreaped Child reserves the PID. Never signal an arbitrary saved PID.
        if !self.reaped {
            crate::procx::kill_tree(self.child.id());
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}
#[cfg(test)]
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Own the tunnel's process group until its descendants have been reclaimed.
/// Unlike `OwnedChild`, exit observation MUST NOT reap the leader: its reserved
/// PID also reserves the PGID, including while the leader is a zombie.
pub(super) struct OwnedProcessGroup {
    child: Child,
    reserved: bool,
    status: Option<std::process::ExitStatus>,
    forced: bool,
}

impl OwnedProcessGroup {
    pub(super) fn spawn(command: &mut Command) -> Result<Self> {
        command.process_group(0);
        let child = command
            .spawn()
            .map_err(|_| "Cannot start owned tunnel process group")?;
        Ok(Self {
            child,
            reserved: true,
            status: None,
            forced: false,
        })
    }

    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(super) fn take_lease(&mut self) -> Result<std::process::ChildStdin> {
        self.child
            .stdin
            .take()
            .ok_or_else(|| "Missing guardian lifetime lease".into())
    }

    pub(super) fn take_stdout(&mut self) -> Result<std::process::ChildStdout> {
        self.child
            .stdout
            .take()
            .ok_or_else(|| "Missing child output".into())
    }

    #[cfg(any(not(target_os = "macos"), test))]
    pub(super) fn take_stderr(&mut self) -> Result<std::process::ChildStderr> {
        self.child
            .stderr
            .take()
            .ok_or_else(|| "Missing child error output".into())
    }

    pub(super) fn exited(&self) -> Result<bool> {
        if !self.reserved {
            return Ok(true);
        }
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        loop {
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id() as libc::id_t,
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                break;
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return Err(
                    "Cannot inspect reserved tunnel leader; cleanup state preserved".into(),
                );
            }
        }
        Ok(unsafe { info.assume_init().si_pid() } != 0)
    }

    pub(super) fn reclaim(&mut self) -> Result<std::process::ExitStatus> {
        if !self.reserved {
            return self
                .status
                .ok_or_else(|| "Missing owned process exit status".into());
        }
        // Verify that wait ownership still exists immediately before signalling.
        // ECHILD is NOT permission to use a possibly recycled PID/PGID.
        self.exited()?;
        if unsafe { libc::killpg(self.id() as libc::pid_t, libc::SIGKILL) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH)
                && !(error.raw_os_error() == Some(libc::EPERM) && self.zombie_is_only_member()?)
            {
                return Err("Cannot reclaim owned tunnel group; cleanup state preserved".into());
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.exited()? {
            if Instant::now() >= deadline {
                return Err(
                    "Owned tunnel termination is still pending; cleanup state preserved".into(),
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let status = self
            .child
            .wait()
            .map_err(|_| "Cannot reap owned tunnel leader")?;
        self.reserved = false;
        self.status = Some(status);
        Ok(status)
    }

    fn zombie_is_only_member(&self) -> Result<bool> {
        #[cfg(target_os = "macos")]
        {
            // Darwin killpg returns EPERM for a zombie-only group. Do not treat
            // arbitrary EPERM as absence: WNOWAIT must prove our leader exited,
            // and libproc must list exactly that reserved leader, nobody else.
            if !self.exited()? {
                return Ok(false);
            }
            const PROC_PGRP_ONLY: u32 = 2; // Darwin sys/proc_info.h.
            let mut members = [0 as libc::pid_t; 2];
            let bytes = unsafe {
                libc::proc_listpids(
                    PROC_PGRP_ONLY,
                    self.id(),
                    members.as_mut_ptr().cast(),
                    std::mem::size_of_val(&members) as libc::c_int,
                )
            };
            Ok(bytes == std::mem::size_of::<libc::pid_t>() as libc::c_int
                && members[0] == self.id() as libc::pid_t)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(false)
        }
    }

    pub(super) fn stop_gracefully<T>(
        &mut self,
        timeout: Duration,
        mut cleanup_guard: impl FnMut() -> Option<T>,
    ) -> Result<()> {
        if !self.reserved {
            return self.clean_exit();
        }
        if !self.exited()?
            && unsafe { libc::kill(self.id() as libc::pid_t, libc::SIGTERM) } != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        {
            return Err("Cannot request tunnel shutdown; cleanup state preserved".into());
        }
        let deadline = Instant::now() + timeout;
        loop {
            if self.exited()? {
                if let Some(_guard) = cleanup_guard() {
                    // Hold mcp.lock and verified cleanup evidence across the
                    // final sweep. A late, unadmitted MCP cannot start work.
                    self.reclaim()?;
                    return self.clean_exit();
                }
            }
            if Instant::now() >= deadline {
                self.forced = true;
                self.reclaim()?;
                return Err("Tunnel shutdown timed out; forced termination required. Cleanup state preserved; run zai chatgpt repair.".into());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn clean_exit(&self) -> Result<()> {
        if !self.forced && self.status.is_some_and(|status| status.success()) {
            Ok(())
        } else {
            Err("Tunnel exited abnormally or required forced termination; cleanup evidence does not prove successful shutdown".into())
        }
    }
}

impl Drop for OwnedProcessGroup {
    fn drop(&mut self) {
        // Error and unwind paths retain the same unreaped-leader authority.
        let _ = self.reclaim();
    }
}

/// A second owner remains inside the group while the supervisor owns its
/// unreaped leader. Either owner's death is observed by the surviving owner.
/// The stdin lease has exactly one writer, held by the supervisor, and is never
/// inherited by tunnel-client or MCP. This function runs only in __tunnel.
pub(super) fn guard_tunnel<T>(
    command: impl FnOnce(&GuardianScope) -> Result<Command>,
    timeout: Duration,
    stop: &std::sync::atomic::AtomicBool,
    mut cleanup_guard: impl FnMut() -> Option<T>,
) -> Result<()> {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let group = unsafe { libc::getpgrp() };
    if group != unsafe { libc::getpid() } {
        return Err("Tunnel guardian must own its process group".into());
    }
    struct GroupGuard(bool);
    impl Drop for GroupGuard {
        fn drop(&mut self) {
            if self.0 {
                // Membership itself pins the group identity, even if our parent
                // died and was reaped. Never use a saved parent PID or PGID.
                unsafe {
                    if libc::getpgrp() == libc::getpid() {
                        libc::killpg(libc::getpgrp(), libc::SIGKILL);
                    }
                    libc::_exit(1);
                }
            }
        }
    }
    let mut owner = GroupGuard(true);
    let parent_gone = Arc::new(AtomicBool::new(false));
    let observed = parent_gone.clone();
    // FD 0 is replaced with /dev/null for the client below; CLOEXEC also
    // protects the lease from any other future exec in this helper.
    if unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err("Cannot protect guardian lifetime lease".into());
    }
    std::thread::Builder::new()
        .name("tunnel-lifetime".into())
        .spawn(move || {
            let mut byte = [0u8; 1];
            loop {
                match std::io::stdin().read(&mut byte) {
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    _ => break,
                }
            }
            observed.store(true, Ordering::Release);
            // The main thread can be blocked by Keychain/Secret Service or
            // filesystem IO before the client exists. The lease monitor owns
            // its own deadline and can reclaim its group without that thread.
            std::thread::sleep(timeout);
            drop(GroupGuard(true));
        })
        .map_err(|_| "Cannot monitor supervisor lifetime")?;
    let mut command = command(&GuardianScope { group })?;
    if parent_gone.load(Ordering::Acquire) {
        return Err("Supervisor lifetime ended before tunnel startup".into());
    }
    command.process_group(group).stdin(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|_| "Cannot start guarded tunnel client")?;
    let mut deadline = None;
    let mut status = None;
    let mut requested = false;
    loop {
        if status.is_none() {
            status = child
                .try_wait()
                .map_err(|_| "Cannot inspect guarded tunnel client")?;
        }
        let stopping = stop.load(Ordering::Relaxed) || parent_gone.load(Ordering::Acquire);
        if stopping && !requested {
            requested = true;
            if status.is_none() {
                // The directly owned, unreaped child reserves this PID.
                if unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) } != 0
                    && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                {
                    return Err("Cannot request guarded tunnel shutdown".into());
                }
            }
        }
        if stopping || status.is_some() {
            let deadline = deadline.get_or_insert_with(|| Instant::now() + timeout);
            if let Some(status) = status {
                if let Some(_cleanup) = cleanup_guard() {
                    if parent_gone.load(Ordering::Acquire) {
                        // No supervisor remains to sweep the group. Drop kills
                        // our group including this helper; no receipt is forged.
                        return Err("Supervisor lifetime ended".into());
                    }
                    owner.0 = false;
                    return if requested && status.success() {
                        Ok(())
                    } else {
                        Err("Tunnel client exited unexpectedly or unsuccessfully".into())
                    };
                }
            }
            if Instant::now() >= *deadline {
                return Err("Guarded tunnel cleanup timed out; journal preserved".into());
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Constructed only after guard_tunnel installed both lifetime owners. Helpers
/// in this scope must remain in that group, including during credential lookup.
pub(super) struct GuardianScope {
    group: libc::pid_t,
}

struct GuardianMember<'a> {
    child: Child,
    status: Option<std::process::ExitStatus>,
    _scope: &'a GuardianScope,
}

impl<'a> GuardianMember<'a> {
    fn spawn(command: &mut Command, scope: &'a GuardianScope) -> Result<Self> {
        if scope.group != unsafe { libc::getpid() } || scope.group != unsafe { libc::getpgrp() } {
            return Err("Guardian helper ownership changed".into());
        }
        command.process_group(scope.group);
        let child = command
            .spawn()
            .map_err(|_| "Cannot start guardian helper")?;
        Ok(Self {
            child,
            status: None,
            _scope: scope,
        })
    }

    fn exited(&mut self) -> Result<bool> {
        if self.status.is_none() {
            self.status = self
                .child
                .try_wait()
                .map_err(|_| "Cannot inspect guardian helper")?;
        }
        Ok(self.status.is_some())
    }

    fn reclaim(&mut self) -> Result<std::process::ExitStatus> {
        if !self.exited()? {
            // This member does not own its process group. Only signal the
            // unreaped Child; descendants remain under the guardian's sweep.
            self.child
                .kill()
                .map_err(|_| "Cannot terminate owned guardian helper")?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self.exited()? {
                if Instant::now() >= deadline {
                    return Err("Guardian helper termination remains pending".into());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        self.status
            .ok_or_else(|| "Missing guardian helper exit status".into())
    }
}

impl Drop for GuardianMember<'_> {
    fn drop(&mut self) {
        let _ = self.reclaim();
    }
}

enum CaptureChild<'a> {
    Independent(OwnedProcessGroup),
    Guarded(GuardianMember<'a>),
}

impl CaptureChild<'_> {
    fn take_stdout(&mut self) -> Result<std::process::ChildStdout> {
        match self {
            Self::Independent(child) => child.take_stdout(),
            Self::Guarded(child) => child
                .child
                .stdout
                .take()
                .ok_or_else(|| "Missing helper output".into()),
        }
    }
    fn exited(&mut self) -> Result<bool> {
        match self {
            Self::Independent(child) => child.exited(),
            Self::Guarded(child) => child.exited(),
        }
    }
    fn reclaim(&mut self) -> Result<std::process::ExitStatus> {
        match self {
            Self::Independent(child) => child.reclaim(),
            Self::Guarded(child) => child.reclaim(),
        }
    }
}

/// Request graceful shutdown of a directly owned child without forcing cleanup
/// to stop. `false` leaves the live Child with the caller; no saved PID is used.
pub(super) fn terminate_and_wait(child: &mut Child, timeout: Duration) -> Result<bool> {
    // try_wait also handles a Child that was already reaped: never signal its
    // potentially recycled PID. Otherwise the unreaped child reserves the PID.
    if child
        .try_wait()
        .map_err(|_| "Cannot inspect owned child")?
        .is_some()
    {
        return Ok(true);
    }
    if unsafe { libc::kill(child.id() as i32, libc::SIGTERM) } != 0
        && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
        return Err("Cannot request supervisor shutdown; state preserved".into());
    }
    let deadline = Instant::now() + timeout;
    loop {
        if child
            .try_wait()
            .map_err(|_| "Cannot inspect owned child")?
            .is_some()
        {
            return Ok(true); // Reaped, but this is not a cleanup receipt.
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(remaining.min(Duration::from_millis(25)));
    }
}

pub(super) fn capture(mut command: Command, timeout: Duration, limit: usize) -> Result<Vec<u8>> {
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let (success, bytes) = capture_status(command, timeout, limit)?;
    if !success {
        return Err("Child command failed; run zai chatgpt doctor".into());
    }
    Ok(bytes)
}

pub(super) fn capture_status(
    command: Command,
    timeout: Duration,
    limit: usize,
) -> Result<(bool, Vec<u8>)> {
    capture_status_scoped(command, timeout, limit, None)
}

/// Capture bounded stderr while retaining the same owned-process-group
/// lifetime and timeout guarantees as the normal capture path. Callers must
/// classify the returned bytes locally; they must never surface them because
/// helper diagnostics can contain sensitive arguments or environment details.
#[cfg(any(not(target_os = "macos"), test))]
pub(super) fn capture_status_stderr(
    mut command: Command,
    timeout: Duration,
    limit: usize,
) -> Result<(bool, Vec<u8>)> {
    command.stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = OwnedProcessGroup::spawn(&mut command)?;
    let stderr = child.take_stderr()?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stderr.take(limit as u64 + 1).read_to_end(&mut bytes);
        let _ = tx.send((result, bytes));
    });
    let start = Instant::now();
    loop {
        if child.exited()? {
            let (read, bytes) = rx
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| "Child error output unavailable")?;
            if read.is_err() || bytes.len() > limit {
                return Err("Child error output exceeded limit or could not be read".into());
            }
            let status = child.reclaim()?;
            return Ok((status.success(), bytes));
        }
        if start.elapsed() >= timeout {
            return Err("Child command timed out".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(any(not(target_os = "macos"), test))]
pub(super) fn capture_guarded(
    scope: &GuardianScope,
    command: Command,
    timeout: Duration,
    limit: usize,
) -> Result<Vec<u8>> {
    let (success, bytes) = capture_status_scoped(command, timeout, limit, Some(scope))?;
    if !success {
        return Err("Guardian helper failed".into());
    }
    Ok(bytes)
}

fn capture_status_scoped(
    mut command: Command,
    timeout: Duration,
    limit: usize,
    scope: Option<&GuardianScope>,
) -> Result<(bool, Vec<u8>)> {
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = match scope {
        Some(scope) => CaptureChild::Guarded(GuardianMember::spawn(&mut command, scope)?),
        None => CaptureChild::Independent(OwnedProcessGroup::spawn(&mut command)?),
    };
    let stdout = child.take_stdout()?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(limit as u64 + 1).read_to_end(&mut bytes);
        let _ = tx.send((result, bytes));
    });
    let start = Instant::now();
    let mut output = None;
    loop {
        if output.is_none() {
            if let Ok((read, bytes)) = rx.try_recv() {
                if read.is_err() || bytes.len() > limit {
                    return Err("Child output exceeded limit or could not be read".into());
                }
                output = Some(bytes);
            }
        }
        if child.exited()? {
            // Only natural EOF proves complete output. Killing a pipe-holding
            // descendant first could fabricate successful, truncated Docker
            // listings. Independent captures keep their leader reserved;
            // guarded captures remain inside the live guardian's group.
            let output = output.or_else(|| {
                rx.recv_timeout(Duration::from_secs(1))
                    .ok()
                    .and_then(|(r, b)| (r.is_ok() && b.len() <= limit).then_some(b))
            });
            let status = child.reclaim()?;
            return output
                .map(|bytes| (status.success(), bytes))
                .ok_or_else(|| "Child output unavailable".into());
        }
        if start.elapsed() >= timeout {
            return Err("Child command timed out".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub(super) fn disable_core_dumps() -> Result<()> {
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0 {
        return Err("Cannot disable core dumps before loading credentials".into());
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::io::BufRead;

    const GROUP_FIXTURE: &str = "features::chatgpt::imp::process::tests::owned_group_fixture";
    static FIXTURE_STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    extern "C" fn fixture_stop(_: libc::c_int) {
        FIXTURE_STOP.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !predicate() {
            assert!(Instant::now() < deadline, "process fixture timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn owned_group_fixture() {
        use super::super::{cleanup::Cleanup, private, tests::fixture};
        let Some(root) = std::env::var_os("ZAIVERN_GROUP_FIXTURE_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let role = std::env::var("ZAIVERN_GROUP_FIXTURE_ROLE").unwrap_or_default();
        if role == "credential-helper" {
            let _lock = private::Lock::acquire(&root, "factory.lock").unwrap();
            private::write(
                &root.join("helper-group"),
                unsafe { libc::getpgrp() }.to_string().as_bytes(),
                false,
            )
            .unwrap();
            private::write(&root.join("factory-ready"), b"ready", false).unwrap();
            std::thread::sleep(Duration::from_secs(60));
            return;
        }
        if role == "capture" {
            let mut cmd = command(&std::env::current_exe().unwrap());
            cmd.args(["--exact", GROUP_FIXTURE])
                .env("ZAIVERN_GROUP_FIXTURE_ROOT", &root)
                .env("ZAIVERN_GROUP_FIXTURE_WORKER", "1")
                .process_group(unsafe { libc::getpgrp() })
                .stdout(Stdio::inherit());
            let _worker = cmd.spawn().unwrap();
            wait_until(|| root.join("worker-ready").exists());
            println!("capture-payload");
            std::process::exit(0); // Worker still owns stdout and mcp.lock.
        }
        if role == "supervisor" {
            let mut cmd = guardian_fixture_command(&root, true);
            let blocked = std::env::var_os("ZAIVERN_GROUP_FIXTURE_BLOCK_FACTORY").is_some();
            if blocked {
                cmd.env("ZAIVERN_GROUP_FIXTURE_ROLE", "blocked-factory");
            }
            if std::env::var_os("ZAIVERN_GROUP_FIXTURE_BLOCK_HELPER").is_some() {
                cmd.env("ZAIVERN_GROUP_FIXTURE_BLOCK_HELPER", "1");
            }
            let mut guardian = OwnedProcessGroup::spawn(&mut cmd).unwrap();
            let _lease = guardian.take_lease().unwrap();
            wait_until(|| {
                root.join(if blocked {
                    "factory-ready"
                } else {
                    "leader-ready"
                })
                .exists()
            });
            private::write(&root.join("supervisor-ready"), b"ready", false).unwrap();
            wait_until(|| false); // Parent test SIGKILLs this directly owned fixture.
            return;
        }
        if role == "blocked-factory" {
            guard_tunnel(
                |scope| {
                    if std::env::var_os("ZAIVERN_GROUP_FIXTURE_BLOCK_HELPER").is_some() {
                        private::write(
                            &root.join("guardian-group"),
                            scope.group.to_string().as_bytes(),
                            false,
                        )
                        .unwrap();
                        let mut cmd = command(&std::env::current_exe().unwrap());
                        cmd.args(["--exact", GROUP_FIXTURE])
                            .env("ZAIVERN_GROUP_FIXTURE_ROOT", &root)
                            .env("ZAIVERN_GROUP_FIXTURE_ROLE", "credential-helper");
                        capture_guarded(scope, cmd, Duration::from_secs(60), 4096)?;
                        return Err("blocked helper unexpectedly resumed".into());
                    }
                    let _lock = private::Lock::acquire(&root, "factory.lock").unwrap();
                    private::write(&root.join("factory-ready"), b"ready", false).unwrap();
                    // Simulate a blocked OS credential API before client creation.
                    std::thread::sleep(Duration::from_secs(60));
                    Err("blocked credential fixture unexpectedly resumed".into())
                },
                Duration::from_millis(200),
                &FIXTURE_STOP,
                || None::<()>,
            )
            .unwrap();
            return;
        }
        if role == "guardian" {
            unsafe {
                libc::signal(
                    libc::SIGTERM,
                    fixture_stop as *const () as libc::sighandler_t,
                );
            }
            let mut cmd = command(&std::env::current_exe().unwrap());
            cmd.args(["--exact", GROUP_FIXTURE])
                .env("ZAIVERN_GROUP_FIXTURE_ROOT", &root);
            if std::env::var_os("ZAIVERN_GROUP_FIXTURE_BLOCK").is_some() {
                cmd.env("ZAIVERN_GROUP_FIXTURE_BLOCK", "1");
            }
            if let Some(outcome) = std::env::var_os("ZAIVERN_GROUP_FIXTURE_EXIT") {
                cmd.env("ZAIVERN_GROUP_FIXTURE_EXIT", outcome);
            }
            let timeout = if std::env::var_os("ZAIVERN_GROUP_FIXTURE_BLOCK").is_some() {
                Duration::from_millis(200)
            } else {
                Duration::from_secs(5)
            };
            guard_tunnel(
                |_| Ok(cmd),
                timeout,
                &FIXTURE_STOP,
                || {
                    let lock = private::Lock::acquire(&root, "mcp.lock").ok()?;
                    super::super::daemon::ensure_clean(&root).ok()?;
                    Some(lock)
                },
            )
            .unwrap();
            return;
        }
        if std::env::var_os("ZAIVERN_GROUP_FIXTURE_WORKER").is_some() {
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            let _lock = private::Lock::acquire(&root, "mcp.lock").unwrap();
            private::write(&root.join("worker-ready"), b"ready", false).unwrap();
            wait_until(|| root.join("release-worker").exists());
            Cleanup::load(&root, &fixture(&root))
                .unwrap()
                .finish(false)
                .unwrap();
            return;
        }
        unsafe {
            libc::signal(
                libc::SIGTERM,
                fixture_stop as *const () as libc::sighandler_t,
            );
        }
        // Inherit the leader's group exactly as tunnel-client's stdio child does.
        let mut worker = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", GROUP_FIXTURE])
            .env("ZAIVERN_GROUP_FIXTURE_WORKER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_until(|| root.join("worker-ready").exists());
        private::write(&root.join("leader-ready"), b"ready", false).unwrap();
        wait_until(|| {
            root.join("exit-leader").exists()
                || (std::env::var_os("ZAIVERN_GROUP_FIXTURE_BLOCK").is_none()
                    && FIXTURE_STOP.load(std::sync::atomic::Ordering::Relaxed))
        });
        if root.join("exit-leader").exists() {
            // Simulate Fx stopping before its TERM-ignoring MCP child completed.
            std::process::exit(0);
        }
        private::write(&root.join("release-worker"), b"release", false).unwrap();
        assert!(worker.wait().unwrap().success());
        match std::env::var("ZAIVERN_GROUP_FIXTURE_EXIT").as_deref() {
            Ok("nonzero") => std::process::exit(1),
            Ok("signal") => unsafe {
                libc::kill(libc::getpid(), libc::SIGKILL);
            },
            _ => {}
        }
    }

    fn guardian_fixture_command(root: &Path, block: bool) -> Command {
        let mut cmd = command(&std::env::current_exe().unwrap());
        cmd.args(["--exact", GROUP_FIXTURE])
            .env("ZAIVERN_GROUP_FIXTURE_ROOT", root)
            .env("ZAIVERN_GROUP_FIXTURE_ROLE", "guardian")
            .stdin(Stdio::piped());
        if block {
            cmd.env("ZAIVERN_GROUP_FIXTURE_BLOCK", "1");
        }
        cmd
    }

    #[test]
    fn supervisor_crash_reclaims_guardian_blocked_before_client_creation() {
        blocked_factory_crash(false);
    }

    #[test]
    fn supervisor_crash_also_reclaims_blocked_credential_helper() {
        blocked_factory_crash(true);
    }

    fn blocked_factory_crash(helper: bool) {
        use super::super::{
            cleanup::{self, Cleanup},
            daemon, private,
            tests::{fixture, Temp},
        };
        let temp = Temp::new();
        let config = fixture(&temp.0);
        Cleanup::prepare(&temp.0, &config, &private::nonce().unwrap()).unwrap();
        let mut unrelated = ready_child("printf 'ready\\n'; exec sleep 30");
        let mut cmd = command(&std::env::current_exe().unwrap());
        cmd.args(["--exact", GROUP_FIXTURE])
            .env("ZAIVERN_GROUP_FIXTURE_ROOT", &temp.0)
            .env("ZAIVERN_GROUP_FIXTURE_ROLE", "supervisor")
            .env("ZAIVERN_GROUP_FIXTURE_BLOCK_FACTORY", "1");
        if helper {
            cmd.env("ZAIVERN_GROUP_FIXTURE_BLOCK_HELPER", "1");
        }
        let mut supervisor = OwnedChild::spawn(&mut cmd).unwrap();
        wait_until(|| temp.0.join("supervisor-ready").exists());
        assert_eq!(private::Lock::held(&temp.0, "factory.lock"), Ok(true));
        if helper {
            assert_eq!(
                private::read(&temp.0.join("helper-group"), 32).unwrap(),
                private::read(&temp.0.join("guardian-group"), 32).unwrap(),
                "credential helper must remain in the guardian's owned group"
            );
        }
        assert_eq!(
            unsafe { libc::kill(supervisor.child.id() as libc::pid_t, libc::SIGKILL) },
            0
        );
        wait_until(|| supervisor.exited().unwrap().is_some());
        wait_until(|| private::Lock::held(&temp.0, "factory.lock") == Ok(false));
        assert!(unrelated.exited().unwrap().is_none());
        assert!(!temp.0.join("worker-ready").exists());
        assert!(!temp.0.join("mcp.done").exists());
        assert!(!temp.0.join("shutdown.json").exists());
        assert!(daemon::ensure_clean(&temp.0).is_err());
        let _operation = private::Lock::acquire(&temp.0, "operation.lock").unwrap();
        let _runtime = private::Lock::acquire(&temp.0, "runtime.lock").unwrap();
        cleanup::reconcile(&temp.0, &config).unwrap();
        daemon::ensure_clean(&temp.0).unwrap();
    }

    #[test]
    fn capture_reclaims_pipe_holding_descendant_after_leader_exit() {
        use super::super::{private, tests::Temp};
        let temp = Temp::new();
        let mut unrelated = ready_child("printf 'ready\\n'; exec sleep 30");
        let mut cmd = command(&std::env::current_exe().unwrap());
        cmd.args(["--exact", GROUP_FIXTURE, "--nocapture"])
            .env("ZAIVERN_GROUP_FIXTURE_ROOT", &temp.0)
            .env("ZAIVERN_GROUP_FIXTURE_ROLE", "capture");
        assert!(capture_status(cmd, Duration::from_secs(5), 4096).is_err());
        wait_until(|| private::Lock::held(&temp.0, "mcp.lock") == Ok(false));
        assert!(unrelated.exited().unwrap().is_none());
    }

    #[test]
    fn cleanup_receipt_never_turns_nonzero_or_signalled_exit_into_success() {
        use super::super::{
            cleanup::Cleanup,
            daemon, private,
            tests::{fixture, Temp},
        };
        for killed in [false, true] {
            let temp = Temp::new();
            let config = fixture(&temp.0);
            let generation = private::nonce().unwrap();
            Cleanup::prepare(&temp.0, &config, &generation).unwrap();
            Cleanup::load(&temp.0, &config)
                .unwrap()
                .finish(false)
                .unwrap();
            daemon::ensure_clean(&temp.0).unwrap();
            let mut cmd = command(Path::new("/bin/sh"));
            cmd.args(["-c", if killed { "exec sleep 30" } else { "exit 1" }]);
            let mut group = OwnedProcessGroup::spawn(&mut cmd).unwrap();
            if killed {
                assert_eq!(
                    unsafe { libc::kill(group.id() as libc::pid_t, libc::SIGKILL) },
                    0
                );
            }
            wait_until(|| group.exited().unwrap());
            #[cfg(target_os = "macos")]
            assert!(group.zombie_is_only_member().unwrap());
            let stopped = group.stop_gracefully(Duration::from_secs(1), || {
                let lock = private::Lock::acquire(&temp.0, "mcp.lock").ok()?;
                daemon::ensure_clean(&temp.0).ok()?;
                Some(lock)
            });
            assert!(stopped.is_err());
            assert!(!group.reserved);
            assert!(!group.status.unwrap().success());
            assert!(group.stop_gracefully(Duration::ZERO, || Some(())).is_err());
            assert!(!temp.0.join("shutdown.json").exists());
            daemon::ensure_clean(&temp.0).unwrap();
        }
    }

    #[test]
    fn guardian_preserves_tunnel_failure_even_after_mcp_cleanup_completed() {
        use super::super::{
            cleanup::Cleanup,
            daemon, private,
            tests::{fixture, Temp},
        };
        for outcome in ["nonzero", "signal"] {
            let temp = Temp::new();
            let config = fixture(&temp.0);
            let generation = private::nonce().unwrap();
            Cleanup::prepare(&temp.0, &config, &generation).unwrap();
            Cleanup::load(&temp.0, &config)
                .unwrap()
                .admit(&generation)
                .unwrap();
            let mut cmd = guardian_fixture_command(&temp.0, false);
            cmd.env("ZAIVERN_GROUP_FIXTURE_EXIT", outcome);
            let mut guardian = OwnedProcessGroup::spawn(&mut cmd).unwrap();
            let _lease = guardian.take_lease().unwrap();
            wait_until(|| temp.0.join("leader-ready").exists());
            assert!(guardian
                .stop_gracefully(Duration::from_secs(5), || {
                    let lock = private::Lock::acquire(&temp.0, "mcp.lock").ok()?;
                    daemon::ensure_clean(&temp.0).ok()?;
                    Some(lock)
                })
                .is_err());
            daemon::ensure_clean(&temp.0).unwrap();
            assert!(!temp.0.join("shutdown.json").exists());
        }
    }

    #[test]
    fn supervisor_or_guardian_crash_releases_mcp_lock_without_losing_cleanup_debt() {
        use super::super::{
            cleanup::{self, Cleanup},
            daemon, private,
            tests::{fixture, Temp},
        };
        for crash in ["supervisor", "guardian", "neither"] {
            let temp = Temp::new();
            let config = fixture(&temp.0);
            let generation = private::nonce().unwrap();
            Cleanup::prepare(&temp.0, &config, &generation).unwrap();
            Cleanup::load(&temp.0, &config)
                .unwrap()
                .admit(&generation)
                .unwrap();
            let mut unrelated = ready_child("printf 'ready\\n'; exec sleep 30");
            if crash == "supervisor" {
                let mut cmd = command(&std::env::current_exe().unwrap());
                cmd.args(["--exact", GROUP_FIXTURE])
                    .env("ZAIVERN_GROUP_FIXTURE_ROOT", &temp.0)
                    .env("ZAIVERN_GROUP_FIXTURE_ROLE", "supervisor");
                let mut supervisor = OwnedChild::spawn(&mut cmd).unwrap();
                wait_until(|| temp.0.join("supervisor-ready").exists());
                // An actual SIGKILL closes the lease without running Drop.
                assert_eq!(
                    unsafe { libc::kill(supervisor.child.id() as libc::pid_t, libc::SIGKILL) },
                    0
                );
                wait_until(|| supervisor.exited().unwrap().is_some());
            } else {
                let mut guardian = OwnedProcessGroup::spawn(&mut guardian_fixture_command(
                    &temp.0,
                    crash == "guardian",
                ))
                .unwrap();
                let _lease = guardian.take_lease().unwrap();
                wait_until(|| temp.0.join("leader-ready").exists());
                if crash == "guardian" {
                    assert_eq!(
                        unsafe { libc::kill(guardian.id() as libc::pid_t, libc::SIGKILL) },
                        0
                    );
                    wait_until(|| guardian.exited().unwrap());
                }
                let result = guardian.stop_gracefully(Duration::from_secs(5), || {
                    let lock = private::Lock::acquire(&temp.0, "mcp.lock").ok()?;
                    daemon::ensure_clean(&temp.0).ok()?;
                    Some(lock)
                });
                assert_eq!(result.is_ok(), crash == "neither");
            }
            wait_until(|| private::Lock::held(&temp.0, "mcp.lock") == Ok(false));
            assert!(unrelated.exited().unwrap().is_none());
            if crash != "neither" {
                assert!(!temp.0.join("mcp.done").exists());
                assert!(!temp.0.join("shutdown.json").exists());
                assert!(temp.0.join("cleanup-pending.json").exists());
                assert!(daemon::ensure_clean(&temp.0).is_err());
                let _operation = private::Lock::acquire(&temp.0, "operation.lock").unwrap();
                let _runtime = private::Lock::acquire(&temp.0, "runtime.lock").unwrap();
                cleanup::reconcile(&temp.0, &config).unwrap();
            }
            daemon::ensure_clean(&temp.0).unwrap();
        }
    }

    #[test]
    fn leader_exit_retains_group_authority_until_lock_released_and_repairable() {
        use super::super::{
            cleanup::{self, Cleanup},
            daemon, private,
            tests::{fixture, Temp},
        };
        for graceful in [false, true] {
            let temp = Temp::new();
            let config = fixture(&temp.0);
            let generation = private::nonce().unwrap();
            Cleanup::prepare(&temp.0, &config, &generation).unwrap();
            Cleanup::load(&temp.0, &config)
                .unwrap()
                .admit(&generation)
                .unwrap();
            let mut cmd = command(&std::env::current_exe().unwrap());
            cmd.args(["--exact", GROUP_FIXTURE])
                .env("ZAIVERN_GROUP_FIXTURE_ROOT", &temp.0);
            let mut group = OwnedProcessGroup::spawn(&mut cmd).unwrap();
            let mut unrelated = ready_child("printf 'ready\\n'; exec sleep 30");
            wait_until(|| temp.0.join("leader-ready").exists());
            assert_eq!(private::Lock::held(&temp.0, "mcp.lock"), Ok(true));
            if !graceful {
                private::write(&temp.0.join("exit-leader"), b"exit", false).unwrap();
                wait_until(|| group.exited().unwrap());
                // Repeated observation preserves wait ownership and the PGID.
                assert!(group.exited().unwrap());
                #[cfg(target_os = "macos")]
                assert!(!group.zombie_is_only_member().unwrap());
                assert_eq!(private::Lock::held(&temp.0, "mcp.lock"), Ok(true));
            }
            let result = group.stop_gracefully(
                if graceful {
                    Duration::from_secs(5)
                } else {
                    Duration::from_millis(100)
                },
                || {
                    let lock = private::Lock::acquire(&temp.0, "mcp.lock").ok()?;
                    daemon::ensure_clean(&temp.0).ok()?;
                    Some(lock)
                },
            );
            assert_eq!(result.is_ok(), graceful);
            wait_until(|| private::Lock::held(&temp.0, "mcp.lock") == Ok(false));
            assert!(unrelated.exited().unwrap().is_none());
            assert_eq!(
                unsafe { libc::waitpid(group.id() as i32, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
            assert_eq!(daemon::ensure_clean(&temp.0).is_ok(), graceful);
            if !graceful {
                assert!(!temp.0.join("mcp.done").exists());
                assert!(!temp.0.join("shutdown.json").exists());
                assert!(temp.0.join("cleanup-pending.json").exists());
                let _operation = private::Lock::acquire(&temp.0, "operation.lock").unwrap();
                let _runtime = private::Lock::acquire(&temp.0, "runtime.lock").unwrap();
                cleanup::reconcile(&temp.0, &config).unwrap();
                daemon::ensure_clean(&temp.0).unwrap();
            }
            // A second stop cannot use the reaped identity to signal anything.
            assert_eq!(
                group.stop_gracefully(Duration::ZERO, || Some(())).is_ok(),
                graceful
            );
            assert!(unrelated.exited().unwrap().is_none());
        }
    }

    pub(in super::super) fn ready_child(script: &str) -> OwnedChild {
        let mut cmd = command(Path::new("/bin/sh"));
        cmd.args(["-c", script]).stdout(Stdio::piped());
        let mut child = OwnedChild::spawn(&mut cmd).unwrap();
        let output = child.child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut line = String::new();
            let result = std::io::BufReader::new(output).read_line(&mut line);
            let _ = tx.send((result, line));
        });
        let (result, line) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        result.unwrap();
        assert_eq!(line, "ready\n");
        child
    }

    #[test]
    fn termination_waits_for_graceful_exit_and_reaps_owned_child() {
        let mut child = ready_child(
            "trap 'sleep 0.2; exit 0' TERM; printf 'ready\\n'; while :; do sleep 1; done",
        );
        let started = Instant::now();
        let terminated = terminate_and_wait(&mut child.child, Duration::from_secs(5));
        let status = child.exited().unwrap(); // Update the guard before assertions can unwind.
        assert!(terminated.unwrap());
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert!(status.unwrap().success());
        // The OS has no waitable child left, rather than just a dead zombie.
        assert_eq!(
            unsafe { libc::waitpid(child.child.id() as i32, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn termination_is_bounded_and_retains_live_child_ownership() {
        let mut child = ready_child("trap '' TERM; printf 'ready\\n'; exec sleep 30");
        let mut unrelated = ready_child("printf 'ready\\n'; exec sleep 30");
        let started = Instant::now();
        assert!(!terminate_and_wait(&mut child.child, Duration::from_millis(100)).unwrap());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(child.exited().unwrap().is_none());
        assert!(unrelated.exited().unwrap().is_none());
        // Only the fixture owner terminates these children after the assertion.
        child.stop();
        unrelated.stop();
    }

    #[test]
    fn termination_of_already_reaped_child_returns_without_signalling() {
        let mut cmd = command(Path::new("/bin/sh"));
        cmd.args(["-c", "exit 0"]);
        let mut child = OwnedChild::spawn(&mut cmd).unwrap();
        assert!(child.child.wait().unwrap().success());
        assert!(child.exited().unwrap().is_some());
        assert!(terminate_and_wait(&mut child.child, Duration::ZERO).unwrap());
        assert!(child.exited().unwrap().is_some());
    }
}
