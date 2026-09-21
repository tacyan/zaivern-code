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
