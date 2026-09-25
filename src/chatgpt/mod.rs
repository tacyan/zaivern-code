//! Managed Secure MCP Tunnel setup. No model inference HTTP client exists here.
mod cleanup;
mod config;
mod daemon;
mod docker;
mod install;
mod private;
mod probe;
mod process;
mod secret;
#[cfg(test)]
mod tests;

use crate::i18n::tr;
use config::Config;
use private::Result;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

pub fn cli_main(args: &[String]) -> i32 {
    if args.is_empty() || args == ["--help"] || args == ["-h"] {
        println!("{}", crate::features::chatgpt::HELP);
        return 0;
    }
    match dispatch(args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("ChatGPT: {error}");
            1
        }
    }
}

fn dispatch(args: &[String]) -> Result<()> {
    install::platform(std::env::consts::OS, std::env::consts::ARCH)?;
    match args.first().map(String::as_str) {
        Some("__probe") if args.len() == 5 => {
            let workspace = Path::new(&args[1]);
            let docker = Path::new(&args[3]);
            config::validate_executable(docker, workspace)?;
            crate::features::chat_bridge::imp::serve_probe(workspace.into(), args[2].clone(), docker.into(), args[4].clone())
        }
        Some("__mcp" | "__serve" | "__supervise" | "__tunnel") if args.len() == 2 => {
            let root = Path::new(&args[1]);
            private::directory(root)?;
            match args[0].as_str() { "__mcp" => mcp_exec(root), "__serve" => managed_serve(root), "__tunnel" => daemon::tunnel_guardian(root), _ => daemon::supervise(root) }
        }
        Some("setup") if args[1..].iter().all(|a| a == "--reauth" || a == "--test") => {
            let root = private::root()?;
            setup(&root,args.iter().any(|a| a == "--reauth"))?;
            if args.iter().any(|a| a == "--test") { test(&root)?; }
            Ok(())
        }
        Some("start") if args.len() == 1 || args.get(1).is_some_and(|a| a == "--foreground") && args.len() == 2 => {
            let root = private::root()?;
            let _operation = private::Lock::acquire(&root,"operation.lock")?;
            let config = Config::load(&root)?;
            preflight(&root,&config)?;
            daemon::start(&root,args.len() == 2)
        }
        Some(action @ ("status"|"doctor"|"stop"|"reset"|"repair"|"test")) if args.len() == 1 => {
            let root = private::root()?;
            match action {
                "status" => status(&root),
                "doctor" => doctor(&root,&Config::load(&root)?),
                "stop" => daemon::stop(&root),
                "reset" => reset(&root),
                "repair" => repair(&root),
                "test" => test(&root),
                _ => unreachable!(),
            }
        }
        _ => Err("Unknown command/options. Use zai chatgpt --help. Runtime keys are never accepted as arguments.".into()),
    }
}

fn prompt(label: &str, default: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        return Err("Setup/reset requires an interactive terminal".into());
    }
    print!("{label}");
    if !default.is_empty() {
        print!(" [{default}]");
    }
    print!(": ");
    std::io::stdout()
        .flush()
        .map_err(|_| "Cannot flush prompt")?;
    let mut input = Vec::new();
    use std::io::Read;
    std::io::stdin()
        .lock()
        .take(8193)
        .read_until(b'\n', &mut input)
        .map_err(|_| "Cannot read input")?;
    if input.len() > 8192 || input.is_empty() {
        return Err("Input cancelled or too long".into());
    }
    let value = std::str::from_utf8(&input)
        .map_err(|_| "Input must be UTF-8")?
        .trim();
    Ok(if value.is_empty() {
        default.to_owned()
    } else {
        value.to_owned()
    })
}
fn confirm(label: &str, default_yes: bool) -> Result<bool> {
    let response = prompt(label, if default_yes { "Y/n" } else { "y/N" })?;
    Ok(response.eq_ignore_ascii_case("y")
        || response.eq_ignore_ascii_case("yes")
        || default_yes && response == "Y/n")
}

fn setup(root: &Path, reauth: bool) -> Result<()> {
    println!(
        "Zaivern {} — {}",
        env!("CARGO_PKG_VERSION"),
        install::platform(std::env::consts::OS, std::env::consts::ARCH)?
    );
    println!("{}", tr("chatgpt.runtime_auth_only"));
    let existing = if private::exists_checked(&root.join("config.json"))? {
        Some(Config::load(root)?)
    } else {
        None
    };
    if let Some(config) = &existing {
        if !reauth {
            println!("{}", tr("chatgpt.existing_setup"));
            match prompt(&tr("chatgpt.select"), "1")?.as_str() {
                "1" => return doctor(root, config),
                "2" => return repair(root),
                "3" => {}
                "4" => return Ok(()),
                _ => return Err("Invalid selection".into()),
            }
        }
    }
    let _operation = private::Lock::acquire(root, "operation.lock")?;
    let _runtime = private::Lock::acquire(root, "runtime.lock")
        .map_err(|_| "Stop the running bridge before reconfiguring: zai chatgpt stop")?;
    daemon::ensure_clean(root)?;
    let executable = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|_| "Cannot locate zai executable")?;
    println!("Zaivern executable: {}", executable.display());
    let workspace = existing
        .as_ref()
        .map(|c| c.workspace.clone())
        .unwrap_or(std::env::current_dir().map_err(|_| "Cannot locate current directory")?);
    println!("Workspace: {}", workspace.display());
    let workspace = if confirm(&tr("chatgpt.use_workspace"), true)? {
        workspace
    } else {
        std::path::PathBuf::from(prompt(&tr("chatgpt.workspace_path"), "")?)
    };
    let workspace = workspace
        .canonicalize()
        .map_err(|_| "Workspace does not exist")?;
    if !workspace.is_dir()
        || workspace.parent().is_none()
        || root.starts_with(&workspace)
        || workspace.starts_with(root)
    {
        return Err(
            "Workspace must be a project directory outside ChatGPT credential storage".into(),
        );
    }
    let (docker, docker_endpoint) = docker::detect(&workspace)?;
    println!("[PASS] Docker — {}", docker_endpoint);
    println!("{}", tr("chatgpt.image_contract"));
    let image_source = prompt(
        &tr("chatgpt.image_name"),
        existing
            .as_ref()
            .map(|c| c.image_source.as_str())
            .unwrap_or(""),
    )?;
    let image = match docker::image(&docker, &docker_endpoint, &image_source) {
        Ok(image) => image,
        Err(_) => {
            if !confirm(&tr("chatgpt.pull_confirm"), false)? {
                return Err(
                    "Provide/build a compatible Agent image, then rerun setup; see docs/chatgpt.md"
                        .into(),
                );
            }
            println!("{}", tr("chatgpt.pulling"));
            docker::pull(&docker, &docker_endpoint, &image_source)?
        }
    };
    println!("[PASS] Agent image: {image}");
    println!(
        "Installing/verifying official full tunnel-client {}...",
        install::TESTED_VERSION
    );
    install::install(root)?;
    client_version(root)?;
    println!("{}", tr("chatgpt.tunnel_hint"));
    let tunnel_id = prompt(
        "Tunnel ID",
        existing
            .as_ref()
            .map(|c| c.tunnel_id.as_str())
            .unwrap_or(""),
    )?;
    let mut config = Config {
        version: 1,
        workspace,
        executable,
        image,
        image_source,
        docker,
        docker_endpoint,
        tunnel_id,
        secret_store: existing
            .as_ref()
            .map(|c| c.secret_store.clone())
            .unwrap_or_else(|| "private-file".into()),
        client_version: install::TESTED_VERSION.into(),
    };
    config.validate()?;
    config.validate_runtime_paths()?;
    if reauth || existing.is_none() || secret::load(root, &config.secret_store).is_err() {
        if let Some(old) = &existing {
            secret::remember_store(root, &old.secret_store)?;
        }
        let key = secret::prompt()?;
        config.secret_store =
            secret::save(root, &key, || confirm(&tr("chatgpt.file_fallback"), false))?;
    }
    private::write(
        &root.join("profile.yaml"),
        &config::profile(&config, root)?,
        false,
    )?;
    config.save(root)?;
    if let Some(old) = &existing {
        if old.secret_store != config.secret_store {
            if secret::remove(root, &old.secret_store).is_ok() {
                secret::forget_store(root, &old.secret_store)?;
            } else {
                // The new credential/config is committed. An unavailable old
                // keyring must not disable the explicitly approved fallback.
                println!("[WARN] Previous secret store unavailable; its recovery journal was retained. Restore the keyring before reset.");
            }
        }
    }
    doctor(root, &config)?;
    println!("{}", tr("chatgpt.setup_ready"));
    Ok(())
}

fn client_version(root: &Path) -> Result<()> {
    install::verify_installed(root)?;
    let mut cmd = process::command(&root.join("tunnel-client"));
    cmd.arg("--version");
    let output = process::capture(cmd, Duration::from_secs(10), 4096)?;
    let output =
        std::str::from_utf8(&output).map_err(|_| "Invalid tunnel-client version output")?;
    if !output
        .split_whitespace()
        .any(|s| s.trim_start_matches('v').split('+').next() == Some(install::TESTED_VERSION))
    {
        return Err("Unsupported tunnel-client version; run zai chatgpt repair".into());
    }
    println!(
        "[PASS] Tunnel client (tested/supported: {}; detected: {})",
        install::TESTED_VERSION,
        install::TESTED_VERSION
    );
    Ok(())
}

fn preflight(root: &Path, config: &Config) -> Result<()> {
    config.validate()?;
    config.validate_runtime_paths()?;
    config::verify_profile(config, root)?;
    install::verify_installed(root)?;
    if !config.executable.is_file() || !config.workspace.is_dir() {
        return Err("MCP executable/workspace missing; run zai chatgpt repair".into());
    }
    if config
        .workspace
        .canonicalize()
        .map_err(|_| "Workspace unavailable")?
        != config.workspace
        || root.starts_with(&config.workspace)
        || config.workspace.starts_with(root)
    {
        return Err("Workspace path changed or includes credentials; rerun setup".into());
    }
    docker::validate_endpoint(&config.docker_endpoint)?;
    if docker::image(&config.docker, &config.docker_endpoint, &config.image)? != config.image {
        return Err("Agent image identity changed".into());
    }
    Ok(())
}

fn doctor(root: &Path, config: &Config) -> Result<()> {
    println!("Zaivern ChatGPT doctor");
    private::directory(root)?;
    print!("{}", cleanup::report(root)?);
    daemon::ensure_clean(root).or_else(|error| {
        if daemon::request(root, "status").is_ok() {
            Ok(())
        } else {
            Err(error)
        }
    })?;
    preflight(root, config)?;
    println!("[PASS] configuration / filesystem permissions\n[PASS] workspace\n[PASS] Docker local socket\n[PASS] immutable Agent image\n[PASS] tunnel ID / profile / MCP executable");
    client_version(root)?;
    let key = secret::load(root, &config.secret_store)?;
    println!("[PASS] Runtime key available (value hidden)");
    let mut cmd = daemon::tunnel_command(root, config, &key, "doctor")?;
    cmd.args(["--explain", "--json"]);
    let (success, bytes) = process::capture_status(cmd, Duration::from_secs(60), 1024 * 1024)?;
    let bytes = zeroize::Zeroizing::new(bytes);
    let report = doctor_report(&bytes)?;
    print!("{report}");
    if !success {
        return Err("Tunnel doctor failed. Check profile/MCP executable/health listener. Missing Runtime key: zai chatgpt setup --reauth. Live authentication is checked after start.".into());
    }
    println!("[PASS] tunnel-client doctor");
    probe::discovery(config)?;
    for endpoint in ["healthz", "readyz"] {
        match daemon::health(root, endpoint) {
            Ok(true) => println!("[PASS] {endpoint}"),
            Ok(false) => {
                return Err(format!(
                    "{endpoint} failed; check Runtime key, Tunnel permission and outbound HTTPS"
                ))
            }
            Err(_) => {
                println!("[WARN] {endpoint}: bridge stopped/unavailable; run zai chatgpt start")
            }
        }
    }
    live_status(root, config);
    println!("[WARN] ChatGPT Connector: invoke Zaivern once in a normal ChatGPT chat.");
    Ok(())
}

fn status(root: &Path) -> Result<()> {
    let config = Config::load(root)?;
    println!("ChatGPT Bridge");
    print!("{}", cleanup::report(root)?);
    match daemon::request(root, "status") {
        Ok(pid) => println!("Tunnel group     running (owned leader PID {pid})"),
        Err(_) => println!("Tunnel group     stopped/unknown"),
    }
    println!(
        "Health           {}",
        if daemon::health(root, "healthz").unwrap_or(false) {
            "live"
        } else {
            "unknown"
        }
    );
    println!(
        "Ready            {}",
        if daemon::health(root, "readyz").unwrap_or(false) {
            "ready"
        } else {
            "unknown"
        }
    );
    live_status(root, &config);
    println!("Workspace        {}\nAgent image      {}\nTunnel           {}\nConnector        unknown (manual ChatGPT check)",config.workspace.display(),config.image,config.tunnel_id);
    Ok(())
}

fn repair(root: &Path) -> Result<()> {
    let _operation = private::Lock::acquire(root, "operation.lock")?;
    let _runtime = private::Lock::acquire(root, "runtime.lock")
        .map_err(|_| "Stop the bridge before repair")?;
    let mut config = Config::load(root)?;
    cleanup::reconcile(root, &config)?;
    install::install(root)?;
    config.executable = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|_| "Cannot resolve zai")?;
    let (docker, endpoint) = docker::detect(&config.workspace)?;
    config.docker = docker;
    config.docker_endpoint = endpoint;
    if docker::image(&config.docker, &config.docker_endpoint, &config.image).is_err() {
        if !confirm("Configured immutable image missing. Pull its original image name and update the image ID?",false)? { return Err("Repair cancelled; original config preserved".into()); }
        config.image = docker::pull(
            &config.docker,
            &config.docker_endpoint,
            &config.image_source,
        )?;
    }
    private::write(
        &root.join("profile.yaml"),
        &config::profile(&config, root)?,
        false,
    )?;
    config.save(root)?;
    doctor(root, &config)
}

fn reset(root: &Path) -> Result<()> {
    let _operation = private::Lock::acquire(root, "operation.lock")?;
    let _runtime = private::Lock::acquire(root, "runtime.lock")
        .map_err(|_| "Stop the bridge before reset: zai chatgpt stop")?;
    daemon::ensure_clean(root)?;
    let mut stores = secret::stores(root)?;
    if let Ok(config) = Config::load(root) {
        if !stores.contains(&config.secret_store) {
            stores.push(config.secret_store);
        }
    }
    println!("{}", tr("chatgpt.reset_summary"));
    if !confirm(&tr("chatgpt.continue"), false)? {
        return Ok(());
    }
    for backend in stores {
        secret::remove(root, &backend)?;
        secret::forget_store(root, &backend)?;
    }
    for name in [
        "profile.yaml",
        "config.json",
        "runtime.json",
        "health-url",
        "secret-stores.json",
        "cleanup-pending.json",
        "active-generation",
        "mcp.done",
        "shutdown.json",
    ] {
        private::remove(&root.join(name))?;
    }
    println!("ChatGPT setup removed.");
    Ok(())
}

fn test(root: &Path) -> Result<()> {
    let config = if private::exists_checked(&root.join("config.json"))? {
        let config = Config::load(root)?;
        preflight(root, &config)?;
        if secret::load(root, &config.secret_store).is_ok() {
            doctor(root, &config)?;
        } else {
            println!("[WARN] Runtime key unavailable: tunnel doctor skipped; local E2E requires no API key.");
        }
        config
    } else {
        println!("[WARN] Setup not present: running local E2E only, without tunnel credentials.");
        let (docker, docker_endpoint) =
            docker::detect(&std::env::current_dir().map_err(|_| "Cannot locate workspace")?)?;
        Config {
            version: 1,
            workspace: std::env::temp_dir(),
            executable: std::env::current_exe().map_err(|_| "Cannot locate zai")?,
            image: String::new(),
            image_source: String::new(),
            docker,
            docker_endpoint,
            tunnel_id: String::new(),
            secret_store: String::new(),
            client_version: install::TESTED_VERSION.into(),
        }
    };
    probe::local_test(&config)?;
    for endpoint in ["healthz", "readyz"] {
        println!(
            "[{}] Tunnel {endpoint}",
            if daemon::health(root, endpoint).unwrap_or(false) {
                "PASS"
            } else {
                "WARN: not confirmed"
            }
        );
    }
    println!("{}", tr("chatgpt.manual_check"));
    Ok(())
}

fn mcp_exec(root: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    process::disable_core_dumps()?;
    let config = Config::load(root)?;
    if config::validate_executable(&config.executable, &config.workspace)? != config.executable {
        return Err("Configured MCP executable changed; run zai chatgpt repair".into());
    }
    let generation = std::env::var("ZAIVERN_CHATGPT_GENERATION")
        .ok()
        .filter(|s| cleanup::valid_nonce(s))
        .ok_or("Missing managed MCP launch generation")?;
    let mut command = process::command(&config.executable);
    command
        .env("PATH", "/usr/bin:/bin")
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_CONTEXT")
        // exec must retain the tunnel's owned group, not become a new leader.
        .process_group(unsafe { libc::getpgrp() });
    command
        .args(["chatgpt", "__serve"])
        .arg(root)
        .env("ZAIVERN_CHATGPT_GENERATION", generation)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::null());
    // The full client closes stdin and also sends TERM. Preserve EOF-driven
    // ChatBridge::drop cancellation instead of killing Rust before cleanup.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            Ok(())
        });
    }
    let _ = command.exec();
    Err("Cannot execute sanitized MCP target".into())
}

fn managed_serve(root: &Path) -> Result<()> {
    if std::env::var_os("CONTROL_PLANE_API_KEY").is_some()
        || std::env::var_os("OPENAI_API_KEY").is_some()
    {
        return Err("Managed MCP must be launched through its sanitized entry point".into());
    }
    let _execution = private::Lock::acquire(root, "mcp.lock")?;
    let generation = std::env::var("ZAIVERN_CHATGPT_GENERATION")
        .ok()
        .filter(|s| cleanup::valid_nonce(s))
        .ok_or("Missing managed MCP launch generation")?;
    let config = Config::load(root)?;
    config.validate_runtime_paths()?;
    let cleanup = std::sync::Arc::new(cleanup::Cleanup::load(root, &config)?);
    // The child can exec before its supervisor has committed runtime.json.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if daemon::generation_running(root, &generation) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err("Managed supervisor unavailable; MCP admission denied".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    cleanup.admit(&generation)?;
    crate::features::chat_bridge::imp::serve_managed(
        config.workspace,
        config.image,
        config.docker,
        config.docker_endpoint,
        cleanup.clone(),
    )?;
    // The bridge has joined its worker. Only verified resource absence can
    // commit a receipt; a failed server leaves its journal for repair.
    cleanup.finish(false)
}

fn doctor_report(bytes: &[u8]) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| "Invalid tunnel-client doctor report")?;
    if !matches!(value["result"].as_str(), Some("ok" | "fail")) {
        return Err("Missing tunnel doctor result".into());
    }
    let checks = value["checks"]
        .as_array()
        .ok_or("Missing tunnel doctor checks")?;
    let mut result = String::new();
    // Summaries/evidence/next are untrusted and may contain credentials. Only
    // fixed check names and fixed status words are allowed into diagnostics.
    for id in [
        "config_source",
        "profile_load",
        "tunnel_id",
        "control_plane_api_key",
        "mcp_target",
        "mcp_command_executable",
        "health_listener",
    ] {
        if let Some(check) = checks.iter().find(|c| c["id"] == id) {
            let state = match check["status"].as_str() {
                Some("PASS") => "PASS",
                Some("FAIL") => "FAIL",
                _ => "WARN",
            };
            result.push_str(&format!("[{state}] {id}\n"));
        }
    }
    if result.is_empty() {
        return Err("Unrecognized doctor checks".into());
    }
    Ok(result)
}

fn live_status(root: &Path, config: &Config) {
    match daemon::live_status(root, &config.tunnel_id) {
        Ok((control,mcp)) => {
            println!("[{}] Control Plane metadata", if control {"PASS"} else {"WARN: unavailable; check Runtime key, organization and Tunnels Read/Use permission"});
            println!("[{}] MCP main channel: stdio",if mcp {"PASS"} else {"WARN: not connected"});
        }
        Err(_) => println!("[WARN] Control Plane / MCP live channel unavailable; start the bridge and rerun doctor"),
    }
}
