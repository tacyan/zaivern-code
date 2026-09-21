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
    Ok(state.child_pid)
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
    let key = secret::load(root, &config.secret_store)?;
    for file in ["health-url", "runtime.json"] {
        let path = root.join(file);
        if path.symlink_metadata().is_ok() {
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
    let mut cmd = tunnel_command(root, &config, &key, "run")?;
    // Raw client output can contain prompts or credentials. Discard it; status
    // and doctor expose structured checks only. No raw log file is persisted.
    let generation = private::nonce()?;
    {
        let _execution = private::Lock::acquire(root, "mcp.lock")?;
        super::cleanup::Cleanup::prepare(root, &config, &generation)?;
    }
    cmd.env("ZAIVERN_CHATGPT_GENERATION", &generation);
    let mut child = match process::OwnedChild::spawn(&mut cmd) {
        Ok(child) => child,
        Err(error) => {
            // No child was spawned, so no task can require cleanup.
            super::cleanup::Cleanup::load(root, &config)?.finish(false)?;
            return Err(error);
        }
    };
    let state = State {
        version: 1,
        pid: std::process::id(),
        child_pid: child.child.id(),
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
        if child.exited()?.is_some() {
            break Err("Tunnel client exited; run zai chatgpt doctor".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_millis(250)))
                    .ok();
                stream
                    .set_write_timeout(Some(Duration::from_millis(250)))
                    .ok();
                let mut input = Vec::new();
                if (&mut stream).take(256).read_to_end(&mut input).is_ok() {
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
    let shutdown = child.stop_gracefully();
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
        if started.elapsed() >= Duration::from_secs(65) {
            // Child is still unreaped and its identity is reserved. Ask it to
            // stop gracefully so its held tunnel Child is cleaned up as well.
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            return Err(
                "Supervisor startup timed out; unlock your secret store and run doctor".into(),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(super) fn stop(root: &Path) -> Result<()> {
    match request(root, "stop") {
        Ok(_) => {
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
        Err(_) => {
            let _lock = private::Lock::acquire(root, "runtime.lock")?;
            ensure_clean(root)?;
            println!("ChatGPT Bridge is stopped. No saved PID was signalled.");
            Ok(())
        }
    }
}

pub(super) fn ensure_clean(root: &Path) -> Result<()> {
    super::cleanup::ensure_confirmed(root)?;
    if root.join("active-generation").symlink_metadata().is_err() {
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
