//! launchd creates a private kernel resource coalition before any writer starts.
//! Fork, process-group changes and orphaning preserve coalition membership.
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
const JOB: &str = "ZAIVERN_WRITER_JOB";

fn valid_boot_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn boot_id() -> io::Result<String> {
    let mut value = [0u8; 128];
    let mut len = value.len();
    if unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            value.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let bytes = value
        .get(..len)
        .ok_or_else(|| io::Error::other("invalid boot identity size"))?;
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let value = std::str::from_utf8(&bytes[..end]).map_err(io::Error::other)?;
    if !valid_boot_id(value) {
        return Err(io::Error::other("invalid boot identity"));
    }
    Ok(value.to_owned())
}

unsafe extern "C" {
    fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut libc::c_void, size: i32) -> i32;
    fn proc_listpids(kind: u32, info: u32, buffer: *mut libc::c_void, size: i32) -> i32;
}

type Usage = unsafe extern "C" fn(u64, *mut libc::c_void, usize) -> i32;
type Signal = unsafe extern "C" fn(*const u32, i32) -> i32;
struct Api {
    usage: Usage,
    signal: Signal,
}

fn api() -> io::Result<&'static Api> {
    static API: std::sync::OnceLock<Option<Api>> = std::sync::OnceLock::new();
    API.get_or_init(|| unsafe {
        let usage = libc::dlsym(
            libc::RTLD_DEFAULT,
            c"coalition_info_resource_usage".as_ptr(),
        );
        let signal = libc::dlsym(libc::RTLD_DEFAULT, c"proc_signal_with_audittoken".as_ptr());
        if usage.is_null() || signal.is_null() {
            None
        } else {
            Some(Api {
                usage: std::mem::transmute::<*mut libc::c_void, Usage>(usage),
                signal: std::mem::transmute::<*mut libc::c_void, Signal>(signal),
            })
        }
    })
    .as_ref()
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "macOS lacks identity-safe writer containment APIs",
        )
    })
}

fn coalition(pid: i32) -> io::Result<u64> {
    let mut info = [0u64; 5];
    let size = std::mem::size_of_val(&info) as i32;
    if unsafe { proc_pidinfo(pid, 20, 0, info.as_mut_ptr().cast(), size) } != size {
        return Err(io::Error::last_os_error());
    }
    if info[0] == 0 {
        return Err(io::Error::other("missing resource coalition"));
    }
    Ok(info[0])
}

fn version(pid: i32) -> io::Result<u32> {
    let mut info = [0u64; 7];
    let size = std::mem::size_of_val(&info) as i32;
    if unsafe { proc_pidinfo(pid, 17, 0, info.as_mut_ptr().cast(), size) } != size {
        return Err(io::Error::last_os_error());
    }
    Ok(info[4] as u32)
}

fn living(cid: u64) -> io::Result<u64> {
    // Stable prefix: tasks_started/tasks_exited; the kernel copies only the
    // smaller of its structure size and the supplied buffer size.
    let mut usage = [0u64; 64];
    if unsafe {
        (api()?.usage)(
            cid,
            usage.as_mut_ptr().cast(),
            std::mem::size_of_val(&usage),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    usage[0]
        .checked_sub(usage[1])
        .ok_or_else(|| io::Error::other("invalid coalition counters"))
}

fn signal_identity(pid: i32, idversion: u32) -> io::Result<()> {
    let mut token = [0u32; 8];
    token[5] = pid as u32;
    token[7] = idversion;
    // XNU resolves pid+pidversion and holds its proc reference while signalling.
    // Checking a version before ordinary kill(pid) would still be racy.
    let result = unsafe { (api()?.signal)(token.as_ptr(), libc::SIGKILL) };
    if result == 0 || result == libc::ESRCH {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}

fn stop_members(cid: u64) -> io::Result<()> {
    let bytes = unsafe { proc_listpids(1, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pids = vec![0i32; (bytes as usize / 4).saturating_add(256)];
    let size = i32::try_from(pids.len() * 4).map_err(io::Error::other)?;
    let actual = unsafe { proc_listpids(1, 0, pids.as_mut_ptr().cast(), size) };
    if actual <= 0 {
        return Err(io::Error::last_os_error());
    }
    let mut failure = None;
    for &pid in pids.iter().take(actual as usize / 4) {
        if pid <= 0 || pid == std::process::id() as i32 {
            continue;
        }
        let before = match version(pid) {
            Ok(value) => value,
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => continue,
            Err(e) => {
                failure = Some(e);
                continue;
            }
        };
        let group = match coalition(pid) {
            Ok(value) => value,
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => continue,
            Err(e) => {
                failure = Some(e);
                continue;
            }
        };
        if group != cid {
            continue;
        }
        match version(pid) {
            Ok(after) if before == after => {
                if let Err(e) = signal_identity(pid, before) {
                    failure = Some(e);
                }
            }
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
            Err(e) => failure = Some(e),
        }
    }
    // A truncated census only delays Stop. The kernel counter, never this
    // census, determines when there are no writers left.
    failure.map_or(Ok(()), Err)
}

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
        api().map_err(|e| e.to_string())?;
        let boot = boot_id().map_err(|e| e.to_string())?;
        let mut random = [0u8; 32];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut random))
            .map_err(|e| e.to_string())?;
        let nonce: String = random.iter().map(|b| format!("{b:02x}")).collect();
        // Darwin sockaddr_un paths are short; per-user TMPDIR can itself
        // consume most of that limit. A random 0700 directory keeps isolation.
        let dir = PathBuf::from("/private/tmp").join(format!("zai-writer-{}", &nonce[..24]));
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
        cmd.env_remove(JOB);
        // The launchd writer, not this transport bridge, acquires the terminal
        // as its controlling tty. Darwin cannot detach a session leader later.
        cmd.set_controlling_tty(false);
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
            "terminal::writer_tree::macos::launcher_probe",
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
                    let mut peer: libc::pid_t = 0;
                    let mut len = std::mem::size_of_val(&peer) as libc::socklen_t;
                    if unsafe {
                        libc::getsockopt(
                            stream.as_raw_fd(),
                            0, // SOL_LOCAL
                            2, // LOCAL_PEERPID
                            &mut peer as *mut _ as _,
                            &mut len,
                        )
                    } != 0
                    {
                        return Err(io::Error::last_os_error().to_string());
                    }
                    if peer != pid as libc::pid_t {
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
        // Darwin accept inherits the listener's nonblocking flag.
        stream.set_nonblocking(false).map_err(|e| e.to_string())?;
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
    if !valid_boot_id(&boot) || nonce.len() != 64 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return true;
    }
    match boot_id() {
        Ok(current) if current != boot => return false,
        Err(_) => return true,
        _ => {}
    }
    let path = PathBuf::from(std::ffi::OsString::from_vec(path));
    if std::fs::read_to_string(&path).is_ok_and(|value| value == nonce) {
        return false;
    }
    let Some(dir) = path.parent() else {
        return true;
    };
    let Ok(text) = std::fs::read_to_string(dir.join("coalition")) else {
        return true;
    };
    let Ok((cid, identity)) = serde_json::from_str::<(u64, String)>(&text) else {
        return true;
    };
    if cid == 0 || identity != nonce {
        return true;
    }
    // XNU assigns coalition IDs monotonically, independently of recyclable PIDs.
    // ESRCH for a previously authenticated coalition means it has been reaped.
    // This recovers if the supervisor died after its final task left but before
    // it could publish a receipt. Other OS errors retain ownership for retry.
    !coalition_gone(living(cid))
}

fn coalition_gone(observation: io::Result<u64>) -> bool {
    matches!(observation, Ok(0))
        || matches!(observation, Err(ref e) if e.raw_os_error() == Some(libc::ESRCH))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Request {
    argv: Vec<Vec<u8>>,
    env: Vec<(Vec<u8>, Vec<u8>)>,
    cwd: Vec<u8>,
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn publish_receipt(dir: &std::path::Path, nonce: &str) -> io::Result<()> {
    write_private(&dir.join("done.tmp"), nonce.as_bytes())?;
    std::fs::rename(dir.join("done.tmp"), dir.join("done"))?;
    std::fs::File::open(dir)?.sync_all()
}

fn pass_terminal(stream: &UnixStream) -> io::Result<()> {
    let mut byte = *b"F";
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(12) };
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(12);
        std::ptr::copy_nonoverlapping([0i32, 1, 2].as_ptr(), libc::CMSG_DATA(header).cast(), 3);
        if libc::sendmsg(stream.as_raw_fd(), &msg, 0) != 1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn receive_terminal(stream: &UnixStream) -> io::Result<Vec<OwnedFd>> {
    let mut byte = [0u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    unsafe {
        if libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) != 1 {
            return Err(io::Error::last_os_error());
        }
        let header = libc::CMSG_FIRSTHDR(&msg);
        if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
            || (*header).cmsg_len != libc::CMSG_LEN(12)
        {
            return Err(io::Error::other("invalid PTY descriptor handoff"));
        }
        let mut result = Vec::new();
        for fd in std::slice::from_raw_parts(libc::CMSG_DATA(header).cast::<i32>(), 3) {
            result.push(OwnedFd::from_raw_fd(*fd));
            if libc::fcntl(*fd, libc::F_SETFD, libc::FD_CLOEXEC) == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(result)
    }
}

fn helper_args() -> io::Result<Vec<String>> {
    let exe = std::env::current_exe()?
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::other("launchd requires a UTF-8 helper executable path"))?;
    #[cfg(not(test))]
    let args = vec![exe, "--zai-internal-writer-supervisor".into()];
    #[cfg(test)]
    let args = vec![
        exe,
        "--exact".into(),
        "terminal::writer_tree::macos::launcher_probe".into(),
        "--nocapture".into(),
    ];
    Ok(args)
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

struct JobCleanup {
    service: String,
    proof: String,
    admitted: bool,
}
impl Drop for JobCleanup {
    fn drop(&mut self) {
        // A bridge error must not destroy the only surviving supervisor while
        // its descendants can still write. Socket EOF asks it to drain first.
        if self.admitted && persisted_alive(&self.proof) {
            return;
        }
        let _ = std::process::Command::new("/bin/launchctl")
            .args(["bootout", &self.service])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn bridge(endpoint: &std::path::Path, nonce: &str) -> io::Result<i32> {
    let dir = endpoint
        .parent()
        .ok_or_else(|| io::Error::other("missing writer directory"))?;
    struct Admission<'a> {
        dir: &'a std::path::Path,
        nonce: &'a str,
        admitted: bool,
    }
    impl Drop for Admission<'_> {
        fn drop(&mut self) {
            if !self.admitted {
                let _ = std::fs::remove_file(self.dir.join("request"));
                let _ = publish_receipt(self.dir, self.nonce);
            }
        }
    }
    let mut admission = Admission {
        dir,
        nonce,
        admitted: false,
    };
    let mut ui = UnixStream::connect(endpoint)?;
    ui.set_read_timeout(Some(Duration::from_secs(10)))?;
    ui.write_all(format!("{nonce}R").as_bytes())?;
    let mut go = [0];
    if ui.read_exact(&mut go).is_err() || go != *b"G" {
        publish_receipt(dir, nonce)?;
        return Err(io::Error::other("writer admission closed"));
    }
    let setup = || -> io::Result<(UnixListener, JobCleanup)> {
        let request = Request {
            argv: serde_json::from_str(&std::env::var(ARGV).map_err(io::Error::other)?)
                .map_err(io::Error::other)?,
            env: std::env::vars_os()
                .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
                .collect(),
            cwd: std::env::current_dir()?.as_os_str().as_bytes().to_vec(),
        };
        write_private(
            &dir.join("request"),
            &serde_json::to_vec(&request).map_err(io::Error::other)?,
        )?;
        let listener = UnixListener::bind(dir.join("job-control"))?;
        listener.set_nonblocking(true)?;
        let label = format!("dev.zaivern.writer.{}", &nonce[..24]);
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let args = helper_args()?
            .iter()
            .map(|a| format!("<string>{}</string>", xml(a)))
            .collect::<String>();
        let endpoint = endpoint
            .to_str()
            .ok_or_else(|| io::Error::other("non UTF-8 launchd endpoint"))?;
        let plist = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array>{args}</array><key>RunAtLoad</key><true/><key>AbandonProcessGroup</key><true/><key>EnvironmentVariables</key><dict><key>{ENDPOINT}</key><string>{}</string><key>{NONCE}</key><string>{nonce}</string><key>{JOB}</key><string>1</string></dict></dict></plist>", xml(endpoint));
        let path = dir.join("job.plist");
        write_private(&path, plist.as_bytes())?;
        let output = std::process::Command::new("/bin/launchctl")
            .args(["bootstrap", &domain])
            .arg(&path)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "macOS writer containment requires a launchd GUI domain: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok((
            listener,
            JobCleanup {
                service: format!("{domain}/{label}"),
                proof: serde_json::to_string(&(
                    dir.join("done").as_os_str().as_bytes(),
                    nonce,
                    boot_id()?,
                ))
                .map_err(io::Error::other)?,
                admitted: false,
            },
        ))
    };
    let (listener, mut cleanup) = match setup() {
        Ok(value) => value,
        Err(e) => {
            publish_receipt(dir, nonce)?;
            return Err(e);
        }
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut job = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(e) => {
                publish_receipt(dir, nonce)?;
                return Err(e);
            }
        }
    };
    job.set_nonblocking(false)?;
    job.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut hello = vec![0; nonce.len()];
    job.read_exact(&mut hello)?;
    if hello != nonce.as_bytes() {
        return Err(io::Error::other("launchd helper identity mismatch"));
    }
    let mut peer_token = [0u32; 8];
    let mut size = std::mem::size_of_val(&peer_token) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            job.as_raw_fd(),
            0,
            6, // LOCAL_PEERTOKEN: immutable peer identity, including pidversion
            peer_token.as_mut_ptr().cast(),
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let peer = peer_token[5] as i32;
    if version(peer)? != peer_token[7] {
        return Err(io::Error::other("launchd peer identity changed"));
    }
    let cid = coalition(peer)?;
    if version(peer)? != peer_token[7] {
        return Err(io::Error::other("launchd peer identity changed"));
    }
    if cid == coalition(std::process::id() as i32)? || living(cid)? != 1 {
        return Err(io::Error::other(
            "launchd did not provide a dedicated writer coalition",
        ));
    }
    write_private(
        &dir.join("coalition"),
        &serde_json::to_vec(&(cid, nonce)).map_err(io::Error::other)?,
    )?;
    // The bridge was spawned without acquiring a controlling terminal. The
    // launchd shell can therefore acquire it in its own new session.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    pass_terminal(&job)?;
    job.set_read_timeout(None)?;
    job.set_nonblocking(true)?;
    ui.set_read_timeout(None)?;
    ui.set_nonblocking(true)?;
    job.write_all(b"G")?;
    admission.admitted = true;
    cleanup.admitted = true;
    let mut stopping = false;
    let proof = cleanup.proof.clone();
    loop {
        let mut bytes = [0; 64];
        match ui.read(&mut bytes) {
            Ok(_) => stopping = true,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => stopping = true,
        }
        if stopping {
            let _ = job.shutdown(std::net::Shutdown::Write);
        }
        let mut check_receipt = false;
        match job.read(&mut bytes) {
            Ok(0) => {
                // If the job dies unexpectedly, its kernel coalition remains
                // authoritative. Recover by draining it with identity signals.
                if !persisted_alive(&proof) {
                    break;
                }
                let _ = stop_members(cid);
            }
            Ok(n) => {
                let _ = ui.write_all(&bytes[..n]);
                check_receipt = bytes[..n].contains(&b'D');
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => {
                let _ = stop_members(cid);
                check_receipt = true;
            }
        }
        if check_receipt && !persisted_alive(&proof) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = ui.write_all(b"D");
    Ok(std::fs::read_to_string(dir.join("exit-code"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1))
}

fn launch_job(endpoint: &std::path::Path, nonce: &str) -> io::Result<i32> {
    let dir = endpoint
        .parent()
        .ok_or_else(|| io::Error::other("missing writer directory"))?;
    let mut stream = UnixStream::connect(dir.join("job-control"))?;
    stream.write_all(nonce.as_bytes())?;
    let fds = receive_terminal(&stream)?;
    let mut go = [0];
    stream.read_exact(&mut go)?;
    if go != *b"G" {
        return Err(io::Error::other("launchd writer admission rejected"));
    }
    let cid = coalition(std::process::id() as i32)?;
    let expected: (u64, String) =
        serde_json::from_slice(&std::fs::read(dir.join("coalition"))?).map_err(io::Error::other)?;
    if expected != (cid, nonce.to_owned()) || living(cid)? != 1 {
        return Err(io::Error::other("writer coalition authentication failed"));
    }
    let request: Request =
        serde_json::from_slice(&std::fs::read(dir.join("request"))?).map_err(io::Error::other)?;
    // The environment is a private handoff payload, not a persistent log.
    std::fs::remove_file(dir.join("request"))?;
    // Finish every fallible channel setup before admitting executable work.
    stream.set_nonblocking(true)?;
    #[cfg(test)]
    let mut tracking_fault = TrackingFault::prepare(&request)?;
    let argv: Vec<_> = request
        .argv
        .into_iter()
        .map(std::ffi::OsString::from_vec)
        .collect();
    let program = argv
        .first()
        .ok_or_else(|| io::Error::other("missing writer executable"))?;
    let mut command = std::process::Command::new(program);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(request.env.into_iter().map(|(k, v)| {
            (
                std::ffi::OsString::from_vec(k),
                std::ffi::OsString::from_vec(v),
            )
        }))
        .env_remove(ENDPOINT)
        .env_remove(ARGV)
        .env_remove(NONCE)
        .env_remove(JOB)
        .current_dir(PathBuf::from(std::ffi::OsString::from_vec(request.cwd)));
    #[cfg(test)]
    command.env_remove("ZAIVERN_WRITER_TEST_TRACKING_SOCKET");
    let raw: Vec<_> = fds.iter().map(AsRawFd::as_raw_fd).collect();
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(raw[0], libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            for (target, fd) in raw.iter().enumerate() {
                if libc::dup2(*fd, target as i32) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            for sig in [
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGTERM,
                libc::SIGQUIT,
                libc::SIGTTIN,
                libc::SIGTTOU,
                libc::SIGTSTP,
            ] {
                libc::signal(sig, libc::SIG_DFL);
            }
            Ok(())
        });
    }
    let mut code = 1;
    let mut child = match command.spawn() {
        Ok(child) => Some(child),
        Err(e) => {
            eprintln!("writer launch failed: {e}");
            None
        }
    };
    let mut parent_exited = child.is_none();
    let mut stopping = false;
    loop {
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(_) => stopping = true,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => stopping = true,
        }
        if stopping {
            #[cfg(not(test))]
            let _ = stop_members(cid);
            #[cfg(test)]
            let _ = match &mut tracking_fault {
                Some(fault) => fault.stop_members(cid),
                None => stop_members(cid),
            };
        }
        if let Some(child) = child.as_mut() {
            if !parent_exited {
                if let Ok(Some(status)) = child.try_wait() {
                    code = status.code().unwrap_or(1);
                    parent_exited = true;
                    let _ = stream.write_all(b"P");
                }
            }
        }
        // Only this helper remains; it never starts more processes after this
        // point. Escaped groups, orphaned descendants and short-lived forks
        // remain counted by the kernel without a userspace discovery window.
        if parent_exited
            && matches!(living(cid), Ok(1))
            && write_private(&dir.join("exit-code"), code.to_string().as_bytes()).is_ok()
            && publish_receipt(dir, nonce).is_ok()
        {
            let _ = stream.write_all(b"D");
            return Ok(code);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
struct TrackingFault {
    stream: UnixStream,
    reported: bool,
    recovered: bool,
}
#[cfg(test)]
impl TrackingFault {
    fn prepare(request: &Request) -> io::Result<Option<Self>> {
        request
            .env
            .iter()
            .find(|(key, _)| key == b"ZAIVERN_WRITER_TEST_TRACKING_SOCKET")
            .map(|(_, path)| {
                let stream =
                    UnixStream::connect(PathBuf::from(std::ffi::OsString::from_vec(path.clone())))?;
                stream.set_nonblocking(true)?;
                Ok(Self {
                    stream,
                    reported: false,
                    recovered: false,
                })
            })
            .transpose()
    }
    fn stop_members(&mut self, cid: u64) -> io::Result<()> {
        if self.recovered {
            return stop_members(cid);
        }
        if !self.reported {
            self.stream.write_all(b"E")?;
            self.reported = true;
        }
        let mut release = [0];
        let recovered = match self.stream.read(&mut release) {
            Ok(0) => true,
            Ok(1) => release == *b"R",
            _ => false,
        };
        if recovered {
            self.recovered = true;
            stop_members(cid)
        } else {
            Err(io::Error::from_raw_os_error(libc::EIO))
        }
    }
}

pub fn entry() -> ! {
    let result = (|| -> io::Result<i32> {
        let endpoint = std::env::var_os(ENDPOINT)
            .ok_or_else(|| io::Error::other("missing writer endpoint"))?;
        let nonce = std::env::var(NONCE).map_err(io::Error::other)?;
        if std::env::var_os(JOB).is_some() {
            launch_job(std::path::Path::new(&endpoint), &nonce)
        } else {
            bridge(std::path::Path::new(&endpoint), &nonce)
        }
    })();
    std::process::exit(match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("PTY writer containment failed: {e}");
            1
        }
    })
}

#[test]
fn launcher_probe() {
    if std::env::var_os(ENDPOINT).is_some() {
        entry();
    }
}

#[test]
fn wrong_pidversion_cannot_signal_a_live_process() {
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = ChildGuard(
        std::process::Command::new("/bin/sh")
            .args(["-c", "printf R; read value"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut ready = [0];
    child
        .0
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut ready)
        .unwrap();
    assert_eq!(ready, *b"R");
    let pid = child.0.id() as i32;
    let actual = version(pid).unwrap();
    // Same numerical PID, different incarnation: the real kernel must reject
    // this token without touching the process, not a mocked string predicate.
    signal_identity(pid, actual.wrapping_add(1)).unwrap();
    assert!(child.0.try_wait().unwrap().is_none());
    signal_identity(pid, actual).unwrap();
    assert!(!child.0.wait().unwrap().success());
}

#[test]
fn tracking_errors_retain_ownership_and_recovery_releases_it() {
    for errno in [libc::EPERM, libc::EIO, libc::ENOMEM] {
        assert!(!coalition_gone(Err(io::Error::from_raw_os_error(errno))));
    }
    assert!(!coalition_gone(Ok(1)));
    assert!(coalition_gone(Ok(0)));
    assert!(coalition_gone(Err(io::Error::from_raw_os_error(
        libc::ESRCH
    ))));
    let mut cmd = portable_pty::CommandBuilder::new("/bin/true");
    let prepared = Prepared::prepare(&mut cmd).unwrap();
    let proof = prepared.proof();
    assert!(persisted_alive(&proof));
    for damaged in [
        "",
        "unknown",
        "not-a-boot-id",
        "00000000000000000000000000000000000000",
    ] {
        let proof = serde_json::to_string(&(
            prepared.dir.join("done").as_os_str().as_bytes(),
            &prepared.nonce,
            damaged,
        ))
        .unwrap();
        assert!(persisted_alive(&proof));
    }
    write_private(&prepared.dir.join("done"), b"wrong nonce").unwrap();
    assert!(persisted_alive(&proof));
    publish_receipt(&prepared.dir, &prepared.nonce).unwrap();
    assert!(!persisted_alive(&proof));
    std::fs::remove_dir_all(&prepared.dir).unwrap();
}
