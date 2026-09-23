//! The supervisor owns the Child; remote commands never signal a saved PID.
use super::{
    config::{self, Config},
    private::{self, Result},
    process, secret,
};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    pid: u32,
    child_pid: u32,
    port: u16,
    generation: String,
}

fn read_state(root: &Path) -> Result<State> {
    let state: State = serde_json::from_slice(&private::read(&root.join("runtime.json"), 4096)?)
        .map_err(|_| "Invalid runtime state")?;
    if state.version != 1
        || state.pid == 0
        || state.child_pid == 0
        || state.port == 0
        || state.generation.len() != 64
        || !state.generation.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("Invalid runtime identity".into());
    }
    Ok(state)
}

pub(super) fn request(root: &Path, action: &str) -> Result<u32> {
    Ok(request_state(root, action)?.child_pid)
}

pub(super) fn generation_running(root: &Path, generation: &str) -> bool {
    // The authenticated supervisor holds runtime.lock throughout its lifetime.
    // Neither a journal phase nor the PID saved in runtime.json proves liveness.
    private::Lock::held(root, "runtime.lock") == Ok(true)
        && request_state(root, "status").is_ok_and(|state| state.generation == generation)
        && private::Lock::held(root, "runtime.lock") == Ok(true)
}

fn request_state(root: &Path, action: &str) -> Result<State> {
    if !["status", "stop"].contains(&action) {
        return Err("Unknown supervisor action".into());
    }
    let state = read_state(root)?;
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, state.port)),
        Duration::from_secs(2),
    )
    .map_err(|_| "Bridge stopped or supervisor unavailable")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|_| "Cannot set IPC timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|_| "Cannot set IPC timeout")?;
    writeln!(stream, "{} {}", state.generation, action).map_err(|_| "Cannot contact supervisor")?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|_| "Cannot finish IPC request")?;
    let mut response = String::new();
    stream
        .take(256)
        .read_to_string(&mut response)
        .map_err(|_| "Supervisor response unavailable")?;
    if response != format!("{} {} {}\n", state.generation, state.pid, state.child_pid) {
        return Err("Supervisor identity mismatch; no process was signalled".into());
    }
    Ok(state)
}

fn read_supervisor_request(stream: &mut TcpStream, timeout: Duration) -> std::io::Result<Vec<u8>> {
    // Darwin inherits the nonblocking listener flag. A read timeout alone
    // does not override it. Bound the whole frame, not each arriving byte.
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(timeout))?;
    let deadline = Instant::now() + timeout;
    let mut input = vec![0; 256];
    let mut len = 0;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(&mut input[len..]) {
            Ok(0) => {
                input.truncate(len);
                return Ok(input);
            }
            Ok(n) => {
                len += n;
                if len == input.len() {
                    return Ok(input); // Oversized frames cannot match an action.
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            Err(error) => return Err(error),
        }
    }
}

pub(super) fn tunnel_command(
    root: &Path,
    config: &Config,
    secret: &secret::Secret,
    verb: &str,
) -> Result<std::process::Command> {
    config::verify_profile(config, root)?;
    let mut command = process::command(&root.join("tunnel-client"));
    command
        .args([verb, "--profile-file"])
        .arg(root.join("profile.yaml"))
        .env("CONTROL_PLANE_API_KEY", secret.text())
        .env("DOCKER_HOST", &config.docker_endpoint)
        .env("TUNNEL_CLIENT_STATE_DIR", root.join("client-state"));
    Ok(command)
}

pub(super) fn supervise(root: &Path) -> Result<()> {
    let _lock = private::Lock::acquire(root, "runtime.lock")?;
    ensure_clean(root)?;
    let config = Config::load(root)?;
    super::install::verify_installed(root)?;
    for file in ["health-url", "runtime.json"] {
        let path = root.join(file);
        if private::exists_checked(&path)? {
            private::open(&path, false)?;
            std::fs::remove_file(path).map_err(|_| "Cannot clear stopped runtime state")?;
        }
    }
    // The client creates health-url; umask also protects all client-owned state.
    unsafe {
        libc::umask(0o077);
        libc::signal(
            libc::SIGTERM,
            stop_signal as *const () as libc::sighandler_t,
        );
        libc::signal(libc::SIGINT, stop_signal as *const () as libc::sighandler_t);
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|_| "Cannot bind private supervisor listener")?;
    listener
        .set_nonblocking(true)
        .map_err(|_| "Cannot configure supervisor listener")?;
    let mut cmd = process::command(&config.executable);
    cmd.args(["chatgpt", "__tunnel"])
        .arg(root)
        .stdin(std::process::Stdio::piped());
    // Raw client output can contain prompts or credentials. Discard it; status
    // and doctor expose structured checks only. No raw log file is persisted.
    let generation = private::nonce()?;
    {
        let _execution = private::Lock::acquire(root, "mcp.lock")?;
        super::cleanup::Cleanup::prepare(root, &config, &generation)?;
    }
    cmd.env("ZAIVERN_CHATGPT_GENERATION", &generation);
    let mut child = match process::OwnedProcessGroup::spawn(&mut cmd) {
        Ok(child) => child,
        Err(error) => {
            // No child was spawned, so no task can require cleanup.
            super::cleanup::Cleanup::load(root, &config)?.finish(false)?;
            return Err(error);
        }
    };
    // Only this process owns the writer. SIGKILL/crash closes it in the kernel,
    // waking the guardian even when no Rust destructor can run here.
    let _lease = child.take_lease()?;
    let state = State {
        version: 1,
        pid: std::process::id(),
        child_pid: child.id(),
        port: listener
            .local_addr()
            .map_err(|_| "Cannot inspect supervisor listener")?
            .port(),
        generation,
    };
    private::write(
        &root.join("runtime.json"),
        &serde_json::to_vec(&state).map_err(|_| "Cannot encode runtime state")?,
        false,
    )?;
    let result = loop {
        if STOP.load(Ordering::Relaxed) {
            break Ok(());
        }
        if child.exited()? {
            break Err("Tunnel client exited; run zai chatgpt doctor".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                // A failed/slow connection never terminates the supervisor.
                // Reading the entire frame and writing the reply each have a
                // 250ms budget, so STOP and the next status remain responsive.
                if let Ok(input) = read_supervisor_request(&mut stream, Duration::from_millis(250))
                {
                    let status = format!("{} status\n", state.generation);
                    let stop = format!("{} stop\n", state.generation);
                    if input == status.as_bytes() || input == stop.as_bytes() {
                        let _ = writeln!(
                            stream,
                            "{} {} {}",
                            state.generation, state.pid, state.child_pid
                        );
                        if input == stop.as_bytes() {
                            break Ok(());
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(_) => break Err("Supervisor listener failed".into()),
        }
    };
    let shutdown = child.stop_gracefully(Duration::from_secs(45), || {
        let lock = private::Lock::acquire(root, "mcp.lock").ok()?;
        ensure_clean(root).ok()?;
        Some(lock)
    });
    let deadline = Instant::now() + Duration::from_secs(45);
    while ensure_clean(root).is_err() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let cleanup = ensure_clean(root);
    let success = result.is_ok() && shutdown.is_ok() && cleanup.is_ok();
    write_shutdown(root, &state.generation, success)?;
    // Still hold the runtime lock, so another generation cannot exist yet.
    for file in ["runtime.json", "health-url"] {
        let _ = std::fs::remove_file(root.join(file));
    }
    result.and(shutdown).and(cleanup)
}

pub(super) fn tunnel_guardian(root: &Path) -> Result<()> {
    process::disable_core_dumps()?;
    unsafe {
        libc::signal(
            libc::SIGTERM,
            stop_signal as *const () as libc::sighandler_t,
        );
        libc::signal(libc::SIGINT, stop_signal as *const () as libc::sighandler_t);
    }
    process::guard_tunnel(
        |scope| {
            let config = Config::load(root)?;
            super::install::verify_installed(root)?;
            let generation = std::env::var("ZAIVERN_CHATGPT_GENERATION")
                .map_err(|_| "Missing managed generation")?;
            if !super::cleanup::valid_nonce(&generation)
                || private::read(&root.join("active-generation"), 64)? != generation.as_bytes()
            {
                return Err("Guardian generation mismatch".into());
            }
            let key = secret::load_guarded(root, &config.secret_store, scope)?;
            let mut command = tunnel_command(root, &config, &key, "run")?;
            command.env("ZAIVERN_CHATGPT_GENERATION", generation);
            Ok(command)
        },
        Duration::from_secs(45),
        &STOP,
        || {
            let lock = private::Lock::acquire(root, "mcp.lock").ok()?;
            ensure_clean(root).ok()?;
            Some(lock)
        },
    )
}

pub(super) fn start(root: &Path, foreground: bool) -> Result<()> {
    if request(root, "status").is_ok() {
        println!("ChatGPT Bridge is already running.");
        return Ok(());
    }
    // Never remove a lock file or decide a PID is stale by probing kill(0).
    drop(private::Lock::acquire(root, "runtime.lock")?);
    ensure_clean(root)?;
    if foreground {
        return supervise(root);
    }
    let config = Config::load(root)?;
    let mut cmd = process::command(&config.executable);
    cmd.args(["chatgpt", "__supervise"]).arg(root);
    let mut child = cmd.spawn().map_err(|_| "Cannot start ChatGPT supervisor")?;
    wait_for_start(
        root,
        &mut child,
        Duration::from_secs(65),
        Duration::from_secs(15),
    )
}

fn wait_for_start(
    root: &Path,
    child: &mut std::process::Child,
    startup_timeout: Duration,
    shutdown_timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    loop {
        if request(root, "status").is_ok() {
            println!("ChatGPT Bridge started. Use zai chatgpt status.");
            return Ok(());
        }
        if child
            .try_wait()
            .map_err(|_| "Cannot inspect supervisor")?
            .is_some()
        {
            return Err(
                "Supervisor failed to start; run zai chatgpt doctor (Keychain may be locked)"
                    .into(),
            );
        }
        if started.elapsed() >= startup_timeout {
            // Allow a short graceful wait here; the supervisor retains its
            // existing 45s shutdown/cleanup budgets if it needs longer.
            if !process::terminate_and_wait(child, shutdown_timeout)? {
                return Err("Supervisor startup timed out and shutdown is still pending; state preserved. Wait, then run zai chatgpt status or doctor.".into());
            }
            return Err(
                "Supervisor startup timed out; unlock your secret store and run doctor".into(),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(super) fn stop(root: &Path) -> Result<()> {
    stop_with_timeout(root, Duration::from_secs(75))
}

fn stop_with_timeout(root: &Path, operation_timeout: Duration) -> Result<()> {
    // Keep lifecycle authority until the supervisor has completed shutdown and
    // its receipt has been authenticated. In the foreground-start case the
    // command itself owns operation.lock for the duration of supervise(), so
    // send the stop request first to let that owner exit, then acquire the
    // lock before inspecting any shutdown state. No other lifecycle operation
    // can mutate state while the supervisor still owns runtime.lock.
    let _operation = match private::Lock::acquire(root, "operation.lock") {
        Ok(lock) => lock,
        Err(_) => {
            let _ = request(root, "stop");
            acquire_operation_for_stop(root, operation_timeout)?
        }
    };
    match request(root, "stop") {
        Ok(_) => wait_for_stop(root),
        Err(_) => {
            // With lifecycle authority held, a failed request means no
            // supervisor can start concurrently.  Only the runtime lock and
            // cleanup evidence decide whether the bridge is already stopped.
            let _runtime = private::Lock::acquire(root, "runtime.lock")?;
            ensure_clean(root)?;
            println!("ChatGPT Bridge is stopped. No saved PID was signalled.");
            Ok(())
        }
    }
}

fn acquire_operation_for_stop(root: &Path, timeout: Duration) -> Result<private::Lock> {
    let deadline = Instant::now() + timeout;
    loop {
        match private::Lock::acquire(root, "operation.lock") {
            Ok(lock) => return Ok(lock),
            Err(acquire_error) => match private::Lock::held(root, "operation.lock") {
                Ok(true) | Ok(false) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(true) | Ok(false) => {
                    return Err("Another ChatGPT lifecycle operation is still running; stop was not confirmed".into())
                }
                Err(_) => return Err(acquire_error),
            },
        }
    }
}

fn wait_for_stop(root: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(100);
    let mut progress = Instant::now();
    loop {
        if let Ok(_lock) = private::Lock::acquire(root, "runtime.lock") {
            ensure_clean(root)?;
            let bytes = private::read(&root.join("shutdown.json"), 4096)?;
            let receipt: ShutdownReceipt =
                serde_json::from_slice(&bytes).map_err(|_| "Invalid shutdown receipt")?;
            let active = private::read(&root.join("active-generation"), 64)?;
            if !receipt.success || receipt.generation.as_bytes() != active.as_slice() {
                return Err("Bridge exited abnormally or required forced termination; cleanup was checked, but stop was not successful. Run doctor.".into());
            }
            println!("ChatGPT Bridge stopped.");
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("Supervisor is still stopping; state was preserved".into());
        }
        if progress.elapsed() >= Duration::from_secs(10) {
            println!("Waiting for MCP task cancellation and cleanup...");
            progress = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(super) fn ensure_clean(root: &Path) -> Result<()> {
    super::cleanup::ensure_confirmed(root)?;
    if !private::exists_checked(&root.join("active-generation"))? {
        return Ok(());
    }
    let active = private::read(&root.join("active-generation"), 64)?;
    if active.len() != 64
        || !active.iter().all(u8::is_ascii_hexdigit)
        || private::read(&root.join("mcp.done"), 64).ok().as_ref() != Some(&active)
    {
        return Err("Previous MCP cleanup is unconfirmed. State preserved; start/setup/reset blocked. Run zai chatgpt status and zai chatgpt repair.".into());
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShutdownReceipt {
    generation: String,
    success: bool,
}

pub(super) fn write_shutdown(root: &Path, generation: &str, success: bool) -> Result<()> {
    private::write(
        &root.join("shutdown.json"),
        &serde_json::to_vec(&ShutdownReceipt {
            generation: generation.into(),
            success,
        })
        .map_err(|_| "Cannot encode shutdown receipt")?,
        false,
    )
}

pub(super) fn health(root: &Path, endpoint: &str) -> Result<bool> {
    Ok(local_get(root, endpoint)?.0)
}

fn local_get(root: &Path, endpoint: &str) -> Result<(bool, Vec<u8>)> {
    request(root, "status")?;
    let bytes = private::read(&root.join("health-url"), 256)?;
    let url = std::str::from_utf8(&bytes)
        .map_err(|_| "Invalid health URL")?
        .trim();
    let port = url
        .strip_prefix("http://127.0.0.1:")
        .and_then(|p| p.trim_end_matches('/').parse::<u16>().ok())
        .filter(|p| *p != 0)
        .ok_or("Health URL must point to IPv4 loopback")?;
    if !["healthz", "readyz", "api/status"].contains(&endpoint) {
        return Err("Unknown health endpoint".into());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(2)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .get(format!("http://127.0.0.1:{port}/{endpoint}"))
        .call()
        .map_err(|_| "Health listener unavailable")?;
    let success = response.status().is_success();
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(256 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read local status")?;
    if bytes.len() > 256 * 1024 {
        return Err("Local status exceeds size limit".into());
    }
    Ok((success, bytes))
}

pub(super) fn live_status(root: &Path, tunnel: &str) -> Result<(bool, bool)> {
    let (success, bytes) = local_get(root, "api/status")?;
    if !success {
        return Err("Local status request failed".into());
    }
    parse_live_status(&bytes, tunnel)
}

fn parse_live_status(bytes: &[u8], tunnel: &str) -> Result<(bool, bool)> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| "Invalid local status")?;
    if value["control_plane_tunnel_id"] != tunnel || value["raw_http_logging_enabled"] != false {
        return Err("Live tunnel identity/security settings mismatch".into());
    }
    let control = value["tunnel_metadata"].is_object()
        && value["tunnel_metadata_error"]
            .as_str()
            .is_none_or(str::is_empty);
    let mcp = value["channels"].as_array().is_some_and(|channels| {
        channels.iter().any(|c| {
            c["name"] == "main"
                && c["enabled"] == true
                && c["transport_kind"] == "stdio"
                && c["details"].as_array().is_some_and(|details| {
                    details.iter().any(|d| {
                        d["key"] == "pid"
                            && d["value"]
                                .as_str()
                                .is_some_and(|p| p.parse::<u32>().is_ok_and(|p| p > 0))
                    })
                })
        })
    });
    Ok((control, mcp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn supervisor_accepts_split_frames_and_bounds_slow_or_unfinished_requests() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let accept = || {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "loopback accept timed out");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        };
        for slow in [false, true] {
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let mut server = accept();
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let reader = std::thread::spawn(move || {
                let result = read_supervisor_request(&mut server, Duration::from_millis(100));
                tx.send(result).unwrap();
            });
            // Keep the connection open: the request must expire even when
            // bytes keep arriving more frequently than the read timeout.
            let writer = if slow {
                Some(std::thread::spawn(move || {
                    for _ in 0..100 {
                        if client.write_all(b"x").is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }))
            } else {
                None
            };
            let error = rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
            reader.join().unwrap();
            if let Some(writer) = writer {
                writer.join().unwrap();
            }
        }
        // A later legitimate, split request still works on the same listener.
        let generation = private::nonce().unwrap();
        let expected = format!("{generation} status\n");
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut server = accept();
        let reader = std::thread::spawn(move || {
            read_supervisor_request(&mut server, Duration::from_secs(2))
        });
        client.write_all(generation.as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(25));
        client.write_all(b" status\n").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(reader.join().unwrap().unwrap(), expected.as_bytes());
    }

    #[test]
    fn startup_timeout_wait_preserves_unconfirmed_generation() {
        use super::super::{
            cleanup::Cleanup,
            tests::{fixture, Temp},
        };
        let temp = Temp::new();
        let generation = private::nonce().unwrap();
        Cleanup::prepare(&temp.0, &fixture(&temp.0), &generation).unwrap();
        let _runtime = private::Lock::acquire(&temp.0, "runtime.lock").unwrap();
        for (script, finishes) in [
            (
                "trap 'sleep 0.2; exit 0' TERM; printf 'ready\\n'; while :; do sleep 1; done",
                true,
            ),
            ("trap '' TERM; printf 'ready\\n'; exec sleep 30", false),
        ] {
            let mut child = process::tests::ready_child(script);
            let started = Instant::now();
            let result = wait_for_start(
                &temp.0,
                &mut child.child,
                Duration::ZERO,
                if finishes {
                    Duration::from_secs(5)
                } else {
                    Duration::from_millis(100)
                },
            );
            let exited = child.exited().unwrap();
            let error = result.unwrap_err();
            assert!(error.contains("startup timed out"));
            assert_eq!(error.contains("shutdown is still pending"), !finishes);
            assert_eq!(exited.is_some(), finishes);
            assert!(started.elapsed() < Duration::from_secs(6));
            assert!(private::Lock::acquire(&temp.0, "runtime.lock").is_err());
            assert!(ensure_clean(&temp.0).is_err());
            assert_eq!(
                private::read(&temp.0.join("active-generation"), 64).unwrap(),
                generation.as_bytes()
            );
            assert!(!temp.0.join("mcp.done").exists());
            assert!(!temp.0.join("shutdown.json").exists());
        }
    }

    #[test]
    fn stop_does_not_claim_stopped_while_lifecycle_operation_is_active() {
        use super::super::tests::Temp;

        let temp = Temp::new();
        let _operation = private::Lock::acquire(&temp.0, "operation.lock").unwrap();
        let error = stop_with_timeout(&temp.0, Duration::from_millis(50)).unwrap_err();
        assert!(error.contains("lifecycle operation"));
        assert!(private::Lock::held(&temp.0, "operation.lock").unwrap());
        assert!(private::Lock::acquire(&temp.0, "runtime.lock").is_ok());

        let stopped = Temp::new();
        stop_with_timeout(&stopped.0, Duration::from_millis(50)).unwrap();
        assert!(private::Lock::acquire(&stopped.0, "runtime.lock").is_ok());
    }

    #[test]
    fn stop_holds_operation_lock_until_shutdown_receipt_is_verified() {
        use super::super::tests::Temp;

        let temp = Temp::new();
        let generation = "a".repeat(64);
        let pid = std::process::id();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        private::write(
            &temp.0.join("runtime.json"),
            &serde_json::to_vec(&State {
                version: 1,
                pid,
                child_pid: pid,
                port,
                generation: generation.clone(),
            })
            .unwrap(),
            false,
        )
        .unwrap();
        private::write(
            &temp.0.join("active-generation"),
            generation.as_bytes(),
            false,
        )
        .unwrap();
        private::write(&temp.0.join("mcp.done"), generation.as_bytes(), false).unwrap();

        // Keep the supervisor's runtime lease held while the accepted stop
        // request is waiting for shutdown. This makes receipt verification a
        // real, observable phase rather than a timing assumption.
        let runtime = private::Lock::acquire(&temp.0, "runtime.lock").unwrap();
        let (accepted_tx, accepted_rx) = std::sync::mpsc::sync_channel(1);
        let expected = format!("{generation} stop\n");
        let response_generation = generation.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            stream.read_to_string(&mut request).unwrap();
            assert_eq!(request, expected);
            accepted_tx.send(()).unwrap();
            writeln!(stream, "{response_generation} {pid} {pid}").unwrap();
        });

        let root = temp.0.clone();
        let stopper = std::thread::spawn(move || stop_with_timeout(&root, Duration::from_secs(2)));
        accepted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("stop request was not accepted");
        assert!(private::Lock::acquire(&temp.0, "operation.lock").is_err());

        write_shutdown(&temp.0, &generation, true).unwrap();
        drop(runtime);
        server.join().unwrap();
        stopper.join().unwrap().unwrap();
        assert!(private::Lock::acquire(&temp.0, "operation.lock").is_ok());
    }

    #[test]
    fn live_status_checks_identity_stdio_and_unsafe_logging() {
        let mut value = serde_json::json!({"control_plane_tunnel_id":"tunnel_fixture","raw_http_logging_enabled":false,"tunnel_metadata":{},"channels":[{"name":"main","enabled":true,"transport_kind":"stdio","details":[{"key":"pid","value":"1234"}]}]});
        assert_eq!(
            parse_live_status(value.to_string().as_bytes(), "tunnel_fixture").unwrap(),
            (true, true)
        );
        assert!(parse_live_status(value.to_string().as_bytes(), "tunnel_other").is_err());
        value["channels"][0]["transport_kind"] = serde_json::json!("http-streamable");
        assert_eq!(
            parse_live_status(value.to_string().as_bytes(), "tunnel_fixture").unwrap(),
            (true, false)
        );
        value["raw_http_logging_enabled"] = serde_json::json!(true);
        assert!(parse_live_status(value.to_string().as_bytes(), "tunnel_fixture").is_err());
    }

    #[test]
    fn fake_health_ready_and_supervisor_identity() {
        let root = crate::test_util::unique_temp_dir("chatgpt", "health");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ipc = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let http = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let state = State {
            version: 1,
            pid: std::process::id(),
            child_pid: std::process::id(),
            port: ipc.local_addr().unwrap().port(),
            generation: private::nonce().unwrap(),
        };
        private::write(
            &root.join("runtime.json"),
            &serde_json::to_vec(&state).unwrap(),
            false,
        )
        .unwrap();
        private::write(
            &root.join("health-url"),
            format!("http://{}", http.local_addr().unwrap()).as_bytes(),
            false,
        )
        .unwrap();
        let replies = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = ipc.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                (&mut stream).take(256).read_to_end(&mut bytes).unwrap();
                assert_eq!(bytes, format!("{} status\n", state.generation).as_bytes());
                writeln!(
                    stream,
                    "{} {} {}",
                    state.generation, state.pid, state.child_pid
                )
                .unwrap();
            }
        });
        let health = std::thread::spawn(move || {
            for status in ["200 OK", "503 Service Unavailable"] {
                let (mut stream, _) = http.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = [0u8; 4096];
                assert!(stream.read(&mut bytes).unwrap() > 0);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                )
                .unwrap();
            }
        });
        assert!(super::health(&root, "healthz").unwrap());
        assert!(!super::health(&root, "readyz").unwrap());
        replies.join().unwrap();
        health.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn forged_pid_cannot_signal_an_unrelated_process() {
        let root = crate::test_util::unique_temp_dir("chatgpt", "forged-pid");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let state = State {
            version: 1,
            pid: std::process::id(),
            child_pid: std::process::id(),
            port: listener.local_addr().unwrap().port(),
            generation: private::nonce().unwrap(),
        };
        private::write(
            &root.join("runtime.json"),
            &serde_json::to_vec(&state).unwrap(),
            false,
        )
        .unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            (&mut stream).take(256).read_to_end(&mut bytes).unwrap();
            stream
                .write_all(b"wrong generation and identity\n")
                .unwrap();
        });
        assert!(request(&root, "stop").is_err());
        server.join().unwrap();
        // Reaching this assertion proves the forged saved PID was not killed.
        assert!(std::process::id() > 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}
