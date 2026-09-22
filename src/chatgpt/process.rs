//! Child ownership, bounded output, and an explicit environment allowlist.
use super::private::Result;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(super) fn command(path: &Path) -> Command {
    let mut command = Command::new(path);
    command.env_clear();
    for name in [
        "HOME",
        "PATH",
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

pub(super) struct OwnedChild {
    pub(super) child: Child,
    reaped: bool,
}
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
impl OwnedChild {
    pub(super) fn stop_gracefully(&mut self) -> Result<()> {
        if self.reaped {
            return Ok(());
        }
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(45);
        while self.exited()?.is_none() {
            if Instant::now() >= deadline {
                self.stop();
                return Err("Tunnel shutdown timed out; forced termination required. Run doctor before restarting.".into());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Ok(())
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.stop();
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
    mut command: Command,
    timeout: Duration,
    limit: usize,
) -> Result<(bool, Vec<u8>)> {
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = OwnedChild::spawn(&mut command)?;
    let stdout = child.child.stdout.take().ok_or("Missing child output")?;
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
        if let Some(status) = child.exited()? {
            return output
                .or_else(|| {
                    rx.recv_timeout(Duration::from_secs(1))
                        .ok()
                        .and_then(|(r, b)| (r.is_ok() && b.len() <= limit).then_some(b))
                })
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
