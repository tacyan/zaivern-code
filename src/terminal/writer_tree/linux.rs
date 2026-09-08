//! A subreaper is installed before the first writer starts. Orphan adoption is
//! maintained by the kernel, including descendants which change session/group.
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const ENDPOINT: &str = "ZAIVERN_WRITER_ENDPOINT";
const ARGV: &str = "ZAIVERN_WRITER_ARGV";
const NONCE: &str = "ZAIVERN_WRITER_NONCE";

#[derive(Debug)]
pub struct Prepared {
    listener: UnixListener,
    dir: PathBuf,
    nonce: String,
    boot: String,
}
#[derive(Debug)]
pub struct Supervisor {
    stream: Mutex<UnixStream>,
    proof: String,
    parent_exited: std::sync::atomic::AtomicBool,
}
#[derive(Debug)]
pub struct Observation {
    pub parent_exited: bool,
    pub done: bool,
}

impl Prepared {
    pub fn prepare(cmd: &mut portable_pty::CommandBuilder) -> Result<Self, String> {
        let boot = boot_identity().map_err(|e| e.to_string())?;
        let mut random = [0u8; 32];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut random))
            .map_err(|e| e.to_string())?;
        let nonce: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("zai-writer-{}", &nonce[..24]));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| e.to_string())?;
        let listener = UnixListener::bind(dir.join("control")).map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let argv: Vec<Vec<u8>> = cmd
            .get_argv()
            .iter()
            .map(|a| a.as_bytes().to_vec())
            .collect();
        cmd.env(ENDPOINT, dir.join("control"));
        cmd.env(NONCE, &nonce);
        cmd.env(
            ARGV,
            serde_json::to_string(&argv).map_err(|e| e.to_string())?,
        );
        *cmd.get_argv_mut() = vec![std::env::current_exe()
            .map_err(|e| e.to_string())?
            .into_os_string()];
        #[cfg(not(test))]
        cmd.arg("--zai-internal-writer-supervisor");
        #[cfg(test)]
        cmd.args([
            "--exact",
            "terminal::writer_tree::linux::launcher_probe",
            "--nocapture",
        ]);
        Ok(Self {
            listener,
            dir,
            nonce,
            boot,
        })
    }

    pub fn proof(&self) -> String {
        serde_json::to_string(&(
            self.dir.join("done").as_os_str().as_bytes(),
            &self.nonce,
            &self.boot,
        ))
        .expect("path and nonce serialize")
    }

    // This handshake runs on the writer wait worker, never on the UI.
    // Before GO the helper cannot start a writer, making startup failure safe.
    pub fn attach(&self, pid: u32) -> Result<Supervisor, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
                    let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
                    if unsafe {
                        libc::getsockopt(
                            stream.as_raw_fd(),
                            libc::SOL_SOCKET,
                            libc::SO_PEERCRED,
                            &mut cred as *mut _ as _,
                            &mut len,
                        )
                    } != 0
                    {
                        return Err(io::Error::last_os_error().to_string());
                    }
                    if cred.pid != pid as libc::pid_t || cred.uid != unsafe { libc::geteuid() } {
                        return Err("writer supervisor peer identity mismatch".into());
                    }
                    break stream;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => return Err(format!("writer supervisor handshake: {e}")),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        let mut ready = vec![0; self.nonce.len() + 1];
        stream.read_exact(&mut ready).map_err(|e| e.to_string())?;
        if ready != format!("{}R", self.nonce).as_bytes() {
            return Err("writer supervisor handshake rejected".into());
        }
        stream.write_all(b"G").map_err(|e| e.to_string())?;
        stream.set_read_timeout(None).map_err(|e| e.to_string())?;
        stream.set_nonblocking(true).map_err(|e| e.to_string())?;
        let proof = self.proof();
        Ok(Supervisor {
            stream: Mutex::new(stream),
            proof,
            parent_exited: std::sync::atomic::AtomicBool::new(false),
        })
    }
}
impl Supervisor {
    pub fn request_stop(&self) {
        // shutdown is nonblocking and cannot address a recycled process ID.
        if let Ok(stream) = self.stream.try_lock() {
            let _ = stream.shutdown(std::net::Shutdown::Write);
        }
    }
    pub fn poll(&self) -> io::Result<Observation> {
        let mut stream = self
            .stream
            .lock()
            .map_err(|_| io::Error::other("supervisor channel poisoned"))?;
        let mut bytes = [0; 64];
        let mut check_receipt = false;
        loop {
            match stream.read(&mut bytes) {
                Ok(0) => {
                    check_receipt = true;
                    break;
                }
                Ok(n) => {
                    check_receipt |= bytes[..n].contains(&b'D');
                    if bytes[..n].contains(&b'P') {
                        self.parent_exited
                            .store(true, std::sync::atomic::Ordering::Release);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let done = check_receipt && !persisted_alive(&self.proof);
        Ok(Observation {
            parent_exited: done
                || self
                    .parent_exited
                    .load(std::sync::atomic::Ordering::Acquire),
            done,
        })
    }
}

// Missing/unreadable proof is unknown, never "all writers exited". Receipts
// survive app exit so normal EOF-driven draining can release a restored lease.
pub fn persisted_alive(proof: &str) -> bool {
    let Ok((path, nonce, boot)) = serde_json::from_str::<(Vec<u8>, String, String)>(proof) else {
        return true;
    };
    if !valid_boot_identity(&boot)
        || nonce.len() != 64
        || !nonce.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return true;
    }
    if std::fs::read_to_string(PathBuf::from(std::ffi::OsString::from_vec(path)))
        .is_ok_and(|value| value == nonce)
    {
        return false;
    }
    // A reboot is another kernel proof that every writer from the old boot
    // is gone, recovering even a forcibly killed supervisor without receipt.
    boot_identity()
        .map(|current| current == boot)
        .unwrap_or(true)
}
fn valid_boot_identity(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
fn boot_identity() -> io::Result<String> {
    let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let value = value.trim();
    if !valid_boot_identity(value) {
        return Err(io::Error::other("invalid Linux boot identity"));
    }
    Ok(value.to_owned())
}

fn owned_children() -> io::Result<Vec<libc::pid_t>> {
    let mut children = Vec::new();
    // libtest adds a thread; orphan adoption may target the group leader.
    for task in std::fs::read_dir("/proc/self/task")? {
        let text = std::fs::read_to_string(task?.path().join("children"))?;
        for word in text.split_whitespace() {
            let pid = word.parse::<libc::pid_t>().map_err(io::Error::other)?;
            if pid <= 0 {
                return Err(io::Error::other("invalid child identity"));
            }
            children.push(pid);
        }
    }
    children.sort_unstable();
    children.dedup();
    Ok(children)
}

fn stop_children() -> io::Result<()> {
    for pid in owned_children()? {
        stop_child(pid)?;
    }
    Ok(())
}

fn stop_child(pid: libc::pid_t) -> io::Result<bool> {
    if pid <= 0 {
        return Err(io::Error::other("invalid child identity"));
    }
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // The sole reaper does not reap between this validation and kill.
    // An exited child still pins its PID; an unrelated replacement is
    // rejected with ECHILD. No same-user or process-group signalling.
    if unsafe {
        libc::waitid(
            libc::P_PID,
            pid as _,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT | libc::__WALL,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(false);
        }
        return Err(error);
    }
    if unsafe { info.si_pid() } != 0 {
        return Ok(false);
    }
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(true)
}

static CHILD_WAKE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
extern "C" fn child_changed(_: libc::c_int) {
    // write is async-signal-safe, nonblocking, and a full pipe already wakes
    // the supervisor. Preserve errno for interrupted code on this thread.
    unsafe {
        let saved = *libc::__errno_location();
        let fd = CHILD_WAKE.load(std::sync::atomic::Ordering::Relaxed);
        if fd >= 0 {
            libc::write(fd, b"x".as_ptr() as _, 1);
        }
        *libc::__errno_location() = saved;
    }
}
fn child_wakeup() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    CHILD_WAKE.store(write.as_raw_fd(), std::sync::atomic::Ordering::Relaxed);
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = child_changed as *const () as usize;
    action.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
    if unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) } != 0 {
        CHILD_WAKE.store(-1, std::sync::atomic::Ordering::Relaxed);
        return Err(io::Error::last_os_error());
    }
    Ok((read, write))
}

#[cfg(test)]
struct TrackingFault {
    stream: UnixStream,
    reported: bool,
    recovered: bool,
}
#[cfg(test)]
impl TrackingFault {
    fn prepare() -> io::Result<Option<Self>> {
        std::env::var_os("ZAIVERN_WRITER_TEST_TRACKING_SOCKET")
            .map(|path| {
                let stream = UnixStream::connect(path)?;
                stream.set_nonblocking(true)?;
                Ok(Self {
                    stream,
                    reported: false,
                    recovered: false,
                })
            })
            .transpose()
    }
    fn stop_children(&mut self) -> io::Result<()> {
        if self.recovered {
            return stop_children();
        }
        if !self.reported {
            self.stream.write_all(b"E")?;
            self.reported = true;
        }
        let mut release = [0];
        let recovered = match self.stream.read(&mut release) {
            Ok(0) => true, // EOF lets a failed test clean up.
            Ok(1) => release == *b"R",
            _ => false,
        };
        if recovered {
            self.recovered = true;
            stop_children()
        } else {
            Err(io::Error::from_raw_os_error(libc::EIO))
        }
    }
}

fn launch() -> io::Result<i32> {
    let endpoint = PathBuf::from(
        std::env::var_os(ENDPOINT)
            .ok_or_else(|| io::Error::other("missing supervisor endpoint"))?,
    );
    let nonce = std::env::var(NONCE).map_err(io::Error::other)?;
    let argv: Vec<Vec<u8>> = serde_json::from_str(&std::env::var(ARGV).map_err(io::Error::other)?)
        .map_err(io::Error::other)?;
    let argv: Vec<_> = argv.into_iter().map(std::ffi::OsString::from_vec).collect();
    let program = argv
        .first()
        .ok_or_else(|| io::Error::other("missing writer command"))?;
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Fail before spawning when procfs child enumeration is unavailable.
    owned_children()?;
    let mut stream = UnixStream::connect(&endpoint)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(format!("{nonce}R").as_bytes())?;
    let mut go = [0];
    stream.read_exact(&mut go)?;
    if go != *b"G" {
        return Err(io::Error::other("supervisor admission rejected"));
    }
    stream.set_read_timeout(None)?;
    stream.set_nonblocking(true)?;
    for signal in [
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGTERM,
        libc::SIGQUIT,
        libc::SIGTTIN,
        libc::SIGTTOU,
        libc::SIGTSTP,
    ] {
        unsafe {
            libc::signal(signal, libc::SIG_IGN);
        }
    }
    let (wake, _wake_writer) = child_wakeup()?;
    #[cfg(test)]
    let mut tracking_fault = TrackingFault::prepare()?;
    let mut command = std::process::Command::new(program);
    command
        .args(&argv[1..])
        .env_remove(ENDPOINT)
        .env_remove(ARGV)
        .env_remove(NONCE);
    #[cfg(test)]
    command.env_remove("ZAIVERN_WRITER_TEST_TRACKING_SOCKET");
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGCHLD, libc::SIG_DFL);
            for signal in [
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGTERM,
                libc::SIGQUIT,
                libc::SIGTTIN,
                libc::SIGTTOU,
                libc::SIGTSTP,
            ] {
                libc::signal(signal, libc::SIG_DFL);
            }
            Ok(())
        });
    }
    let mut code = 1;
    let parent = match command.spawn() {
        Ok(child) => child.id() as libc::pid_t,
        Err(e) => {
            eprintln!("writer launch failed: {e}");
            0
        }
    };
    let mut stopping = false;
    loop {
        let mut wake_bytes = [0u8; 64];
        while unsafe {
            libc::read(
                wake.as_raw_fd(),
                wake_bytes.as_mut_ptr() as _,
                wake_bytes.len(),
            )
        } > 0
        {}
        let mut control = [0; 1];
        match stream.read(&mut control) {
            Ok(_) => stopping = true,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => stopping = true,
        }
        if stopping {
            // Errors retain ownership and retry. ECHILD from waitpid below,
            // never a scan result or an elapsed timeout, establishes completion.
            #[cfg(not(test))]
            let _ = stop_children();
            #[cfg(test)]
            let _ = match &mut tracking_fault {
                Some(fault) => fault.stop_children(),
                None => stop_children(),
            };
        }
        let empty = loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG | libc::__WALL) };
            if pid > 0 {
                if pid == parent {
                    code = if libc::WIFEXITED(status) {
                        libc::WEXITSTATUS(status)
                    } else {
                        1
                    };
                    let _ = stream.write_all(b"P");
                }
                continue;
            }
            if pid == 0 {
                break false;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break error.raw_os_error() == Some(libc::ECHILD);
        };
        if empty {
            let dir = endpoint
                .parent()
                .ok_or_else(|| io::Error::other("missing receipt directory"))?;
            // Retry a transient receipt error; never exit claiming completion
            // without the proof needed by restored ownership records.
            if publish_receipt(dir, &nonce).is_ok() {
                let _ = stream.write_all(b"D");
                return Ok(code);
            }
        }
        if stopping || empty {
            // Retry backoff only: elapsed time never establishes completion.
            std::thread::sleep(Duration::from_millis(10));
        } else {
            // Child exit and app Stop wake immediately. Also recheck waitpid for
            // clone children whose exit_signal is zero (no SIGCHLD delivery).
            // The interval is not evidence of termination; only ECHILD is.
            let mut events = [
                libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            if unsafe { libc::poll(events.as_mut_ptr(), events.len() as _, 250) } < 0
                && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
            {
                stopping = true;
            }
        }
    }
}

fn publish_receipt(dir: &std::path::Path, nonce: &str) -> io::Result<()> {
    let receipt = dir.join("done.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&receipt)?;
    file.write_all(nonce.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&receipt, dir.join("done"))?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

// launch only returns errors before spawning a writer. Additionally require
// kernel ECHILD before recording a failed startup as safely complete.
fn failed_startup_receipt() {
    let mut status = 0;
    if unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG | libc::__WALL) } != -1
        || io::Error::last_os_error().raw_os_error() != Some(libc::ECHILD)
    {
        return;
    }
    if let (Some(endpoint), Ok(nonce)) = (std::env::var_os(ENDPOINT), std::env::var(NONCE)) {
        if let Some(dir) = std::path::Path::new(&endpoint).parent() {
            let _ = publish_receipt(dir, &nonce);
        }
    }
}

pub fn entry() -> ! {
    let code = match launch() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("PTY writer containment failed: {e}");
            failed_startup_receipt();
            1
        }
    };
    std::process::exit(code)
}
#[test]
fn launcher_probe() {
    if std::env::var_os(ENDPOINT).is_some() {
        entry();
    }
}

#[test]
fn missing_and_wrong_receipts_do_not_release_ownership() {
    let dir = std::env::temp_dir().join(format!("zai-receipt-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("done");
    let nonce = "a".repeat(64);
    let proof = serde_json::to_string(&(
        path.as_os_str().as_bytes(),
        &nonce,
        boot_identity().unwrap(),
    ))
    .unwrap();
    assert!(persisted_alive(&proof));
    std::fs::write(&path, "different identity").unwrap();
    assert!(persisted_alive(&proof));
    std::fs::write(&path, &nonce).unwrap();
    assert!(!persisted_alive(&proof));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn nonchild_identity_is_not_a_kill_target() {
    // A stale PID reused by a nonchild must fail kernel ownership validation.
    // Using this process itself also makes an accidental kill observable.
    assert!(!stop_child(std::process::id() as libc::pid_t).unwrap());
    assert!(stop_child(0).is_err());
    assert!(stop_child(-1).is_err());
}

#[test]
fn exited_child_remains_pinned_until_reaped() {
    let mut child = std::process::Command::new("sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        },
        0
    );
    assert!(!stop_child(child.id() as libc::pid_t).unwrap());
    assert!(child.wait().unwrap().success());
    assert!(!stop_child(child.id() as libc::pid_t).unwrap());
}

#[test]
fn clone_child_without_sigchld_is_owned_and_stopped() {
    extern "C" fn child(arg: *mut libc::c_void) -> libc::c_int {
        let fds = unsafe { &*(arg as *const [libc::c_int; 2]) };
        unsafe {
            libc::close(fds[0]);
            libc::write(fds[1], b"R".as_ptr() as _, 1);
            libc::close(fds[1]);
            loop {
                libc::pause();
            }
        }
    }
    struct OwnedClone(libc::pid_t);
    impl Drop for OwnedClone {
        fn drop(&mut self) {
            if self.0 > 0 {
                let _ = stop_child(self.0);
                unsafe {
                    libc::waitpid(self.0, std::ptr::null_mut(), libc::__WALL);
                }
            }
        }
    }
    let mut fds = [-1; 2];
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    let mut stack = vec![0u128; 4096];
    let stack_top = unsafe { stack.as_mut_ptr().add(stack.len()) };
    // An exit signal of zero is excluded by plain waitpid/waitid defaults.
    let pid = unsafe { libc::clone(child, stack_top as _, 0, &fds as *const _ as _) };
    assert!(pid > 0, "{}", io::Error::last_os_error());
    let mut owned = OwnedClone(pid);
    drop(write);
    let mut ready = [0u8];
    assert_eq!(
        unsafe { libc::read(read.as_raw_fd(), ready.as_mut_ptr() as _, 1) },
        1
    );
    assert_eq!(ready, *b"R");
    assert!(stop_child(pid).unwrap());
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::__WALL) },
        pid
    );
    owned.0 = 0;
    assert!(libc::WIFSIGNALED(status));
    assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
}

#[test]
fn previous_boot_recovers_missing_receipt_but_invalid_identity_does_not() {
    let path = std::env::temp_dir().join("zai-nonexistent-boot-proof");
    let current = boot_identity().unwrap();
    let mut previous = "00000000-0000-0000-0000-000000000000".to_owned();
    if previous == current {
        previous.replace_range(..1, "1");
    }
    let nonce = "a".repeat(64);
    let proof = serde_json::to_string(&(path.as_os_str().as_bytes(), &nonce, previous)).unwrap();
    assert!(!persisted_alive(&proof));
    let proof = serde_json::to_string(&(path.as_os_str().as_bytes(), &nonce, "unknown")).unwrap();
    assert!(persisted_alive(&proof));
}

#[test]
fn malformed_identity_cannot_match_a_receipt_and_release_ownership() {
    struct ReceiptDir(PathBuf);
    impl Drop for ReceiptDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = ReceiptDir(crate::test_util::unique_temp_dir(
        "zai-writer",
        "invalid-proof",
    ));
    std::fs::create_dir_all(&dir.0).unwrap();
    let path = dir.0.join("done");
    let current = boot_identity().unwrap();
    for (nonce, boot) in [
        (String::new(), current.clone()),
        ("g".repeat(64), current),
        ("a".repeat(64), "invalid-boot".into()),
    ] {
        std::fs::write(&path, &nonce).unwrap();
        let proof = serde_json::to_string(&(path.as_os_str().as_bytes(), nonce, boot)).unwrap();
        assert!(
            persisted_alive(&proof),
            "malformed proof matched its receipt"
        );
    }
}
