use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

pub(super) struct Temp(pub(super) std::path::PathBuf);
impl Temp {
    pub(super) fn new() -> Self {
        let path = crate::test_util::unique_temp_dir("chatgpt", "private");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn fixture(root: &Path) -> Config {
    Config {
        version: 1,
        workspace: root.join("project ' 引用 $()"),
        executable: root.join("zai ' Unicode 空白"),
        image: format!("sha256:{}", "a".repeat(64)),
        image_source: "local-agent:trusted".into(),
        docker: root.join("docker"),
        docker_endpoint: "unix:///tmp/test-docker.sock".into(),
        tunnel_id: "tunnel_0123456789abcdef0123456789abcdef".into(),
        secret_store: "private-file".into(),
        client_version: install::TESTED_VERSION.into(),
    }
}

#[test]
fn private_files_reject_symlink_hardlink_and_permissions() {
    let temp = Temp::new();
    let path = temp.0.join("key");
    private::write(&path, b"secret", false).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    symlink(&path, temp.0.join("link")).unwrap();
    assert!(private::read(&temp.0.join("link"), 100).is_err());
    assert!(private::write(&temp.0.join("link"), b"overwrite", false).is_err());
    std::fs::hard_link(&path, temp.0.join("hard")).unwrap();
    assert!(private::read(&path, 100).is_err());
    assert!(private::write(&path, b"overwrite", false).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"secret");
    std::fs::remove_file(temp.0.join("hard")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(private::read(&path, 100).is_err());
}

#[test]
fn config_profile_roundtrip_and_repeat_are_secret_free() {
    let temp = Temp::new();
    let config = fixture(&temp.0);
    for _ in 0..2 {
        config.save(&temp.0).unwrap();
        private::write(
            &temp.0.join("profile.yaml"),
            &config::profile(&config, &temp.0).unwrap(),
            false,
        )
        .unwrap();
        let loaded = Config::load(&temp.0).unwrap();
        config::verify_profile(&loaded, &temp.0).unwrap();
        assert_eq!(loaded.workspace, config.workspace);
    }
    let json: serde_json::Value =
        serde_json::from_slice(&private::read(&temp.0.join("profile.yaml"), 65536).unwrap())
            .unwrap();
    assert_eq!(
        json["control_plane"]["api_key"],
        "env:CONTROL_PLANE_API_KEY"
    );
    assert!(json["mcp"]["commands"][0]["command"]
        .as_str()
        .unwrap()
        .contains("'\"'\"'"));
    assert!(config::quote("line\nbreak").is_err());
    private::write(&temp.0.join("profile.yaml"), b"{}", false).unwrap();
    assert!(config::verify_profile(&config, &temp.0).is_err());
}

#[test]
fn exclusive_lock_survives_file_reuse_and_blocks_duplicates() {
    let temp = Temp::new();
    let first = private::Lock::acquire(&temp.0, "runtime.lock").unwrap();
    assert_eq!(private::Lock::held(&temp.0, "runtime.lock"), Ok(true));
    assert!(private::Lock::acquire(&temp.0, "runtime.lock").is_err());
    drop(first);
    assert_eq!(private::Lock::held(&temp.0, "runtime.lock"), Ok(false));
    assert!(private::Lock::acquire(&temp.0, "runtime.lock").is_ok());
    std::fs::set_permissions(
        temp.0.join("runtime.lock"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(private::Lock::held(&temp.0, "runtime.lock").is_err());
}

#[test]
fn child_environment_and_profile_never_contain_runtime_key() {
    let temp = Temp::new();
    let config = fixture(&temp.0);
    let sentinel = "sk-runtime-secret-sentinel-1234567890";
    let key = secret::Secret::from_bytes(sentinel.as_bytes().to_vec()).unwrap();
    private::write(
        &temp.0.join("profile.yaml"),
        &config::profile(&config, &temp.0).unwrap(),
        false,
    )
    .unwrap();
    let tunnel = daemon::tunnel_command(&temp.0, &config, &key, "run").unwrap();
    assert!(tunnel
        .get_envs()
        .any(|(k, v)| k == "CONTROL_PLANE_API_KEY" && v.is_some_and(|v| v == sentinel)));
    assert!(tunnel
        .get_args()
        .all(|a| !a.to_string_lossy().contains(sentinel)));
    let mcp = process::command(&config.executable);
    assert!(mcp.get_envs().all(|(k, v)| k != "CONTROL_PLANE_API_KEY"
        && k != "OPENAI_API_KEY"
        && !v.is_some_and(|v| v == sentinel)));
    assert!(
        !String::from_utf8(config::profile(&config, &temp.0).unwrap())
            .unwrap()
            .contains(sentinel)
    );
    assert!(secret::Secret::from_bytes(b"short".to_vec()).is_err());
    assert!(secret::Secret::from_bytes(format!("{sentinel}\n").into_bytes()).is_err());
}

#[test]
fn fake_child_pass_fail_and_output_limit() {
    let mut cmd = process::command(Path::new("/bin/sh"));
    cmd.args([
        "-c",
        "printf 'RESULT ok'; printf 'sk-runtime-secret-sentinel-1234567890' >&2",
    ]);
    assert_eq!(
        process::capture(cmd, Duration::from_secs(2), 100).unwrap(),
        b"RESULT ok"
    );
    let mut cmd = process::command(Path::new("/bin/sh"));
    cmd.args([
        "-c",
        "printf 'sk-runtime-secret-sentinel-1234567890' >&2; exit 1",
    ]);
    let error = process::capture(cmd, Duration::from_secs(2), 100).unwrap_err();
    assert!(!error.contains("sentinel"));
    let mut cmd = process::command(Path::new("/bin/sh"));
    cmd.args(["-c", "printf 'too much output'"]);
    assert!(process::capture(cmd, Duration::from_secs(2), 3).is_err());
}

#[test]
fn missing_key_client_docker_image_and_bad_state_fail_closed() {
    let temp = Temp::new();
    assert!(secret::load(&temp.0, "private-file").is_err());
    assert!(install::verify_installed(&temp.0).is_err());
    assert!(docker::validate_endpoint("tcp://127.0.0.1:2375").is_err());
    assert!(docker::validate_endpoint("unix:///nonexistent/zaivern-test.sock").is_err());
    assert!(docker::image(
        Path::new("/nonexistent/docker"),
        "unix:///nonexistent/socket",
        "image"
    )
    .is_err());
    private::write(
        &temp.0.join("runtime.json"),
        br#"{"pid":1,"port":80}"#,
        false,
    )
    .unwrap();
    assert!(daemon::request(&temp.0, "stop").is_err());
    let mut config = fixture(&temp.0);
    config.image = "latest".into();
    assert!(config.validate().is_err());
    config = fixture(&temp.0);
    config.tunnel_id = "tunnel_good\napi_key: secret".into();
    assert!(config.validate().is_err());
    config.tunnel_id = "tunnel_short".into();
    assert!(config.validate().is_err());
    config.tunnel_id = "tunnel_0123456789ABCDEF0123456789abcdef".into();
    assert!(config.validate().is_err());
}

#[test]
fn fifo_is_rejected_without_waiting_for_a_writer() {
    let temp = Temp::new();
    let path = temp.0.join("runtime-key");
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(private::read(&path, 4096).is_err());
}

#[test]
fn doctor_diagnostics_redact_untrusted_fields_and_ids() {
    let secret = "sk-runtime-secret-sentinel-1234567890";
    for result in ["ok", "fail"] {
        let input = serde_json::json!({"result":result,"next":secret,"checks":[
            {"id":"control_plane_api_key","status":"PASS","summary":secret,"evidence":[secret]},
            {"id":"health_listener","status":"FAIL","why":secret},
            {"id":secret,"status":secret,"summary":secret}
        ]});
        let text = doctor_report(input.to_string().as_bytes()).unwrap();
        assert!(!text.contains(secret));
        assert_eq!(
            text,
            "[PASS] control_plane_api_key\n[FAIL] health_listener\n"
        );
    }
    assert!(doctor_report(b"{}").is_err());
}

#[test]
fn failed_config_commit_keeps_secret_recovery_journal() {
    let temp = Temp::new();
    secret::remember_store(&temp.0, "private-file").unwrap();
    private::write(
        &temp.0.join("runtime-key"),
        b"sk-old-runtime-key-123456789",
        false,
    )
    .unwrap();
    secret::remember_store(&temp.0, "keychain").unwrap();
    // Fault injection: the destination is a symlink, so config commit fails.
    symlink(temp.0.join("untouched"), temp.0.join("config.json")).unwrap();
    assert!(fixture(&temp.0).save(&temp.0).is_err());
    assert_eq!(
        secret::stores(&temp.0).unwrap(),
        ["private-file", "keychain"]
    );
    secret::remove(&temp.0, "private-file").unwrap();
    secret::forget_store(&temp.0, "private-file").unwrap();
    assert_eq!(secret::stores(&temp.0).unwrap(), ["keychain"]);
    assert!(!temp.0.join("runtime-key").exists());
    assert!(!temp.0.join("untouched").exists());
}

#[test]
fn shutdown_requires_matching_mcp_cleanup_receipt() {
    let temp = Temp::new();
    daemon::ensure_clean(&temp.0).unwrap();
    let generation = private::nonce().unwrap();
    private::write(
        &temp.0.join("active-generation"),
        generation.as_bytes(),
        false,
    )
    .unwrap();
    assert!(daemon::ensure_clean(&temp.0).is_err());
    private::write(
        &temp.0.join("mcp.done"),
        private::nonce().unwrap().as_bytes(),
        false,
    )
    .unwrap();
    assert!(daemon::ensure_clean(&temp.0).is_err());
    private::write(&temp.0.join("mcp.done"), generation.as_bytes(), false).unwrap();
    daemon::ensure_clean(&temp.0).unwrap();
}

#[test]
#[ignore = "requires explicitly built zai binary; no Docker or API key"]
fn real_mcp_reexec_strips_key_before_any_startup_child() {
    let temp = Temp::new();
    let bin = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("zai");
    assert_eq!(
        crate::test_util::zai_gate_at(
            &bin,
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            env!("CARGO_PKG_VERSION")
        ),
        crate::test_util::ZaiVerdict::Usable
    );
    let mut config = fixture(&temp.0);
    // This trusted test target exposes only whether a credential was inherited.
    let fake = b"#!/bin/sh\nif [ -n \"${CONTROL_PLANE_API_KEY+x}\" ] || [ -n \"${OPENAI_API_KEY+x}\" ]; then echo LEAK >&2; exit 9; fi\nprintf 'clean\\n'\n";
    private::write(&config.executable, fake, true).unwrap();
    config.workspace = temp.0.join("unused-project");
    config.save(&temp.0).unwrap();
    let mut cmd = process::command(&bin);
    cmd.args(["chatgpt", "__mcp"])
        .arg(&temp.0)
        .env("CONTROL_PLANE_API_KEY", "sk-runtime-sentinel-123456789")
        .env("OPENAI_API_KEY", "sk-inference-must-not-inherit")
        .env("ZAIVERN_CHATGPT_GENERATION", private::nonce().unwrap())
        .env("ZAIVERN_HOME", temp.0.join("home"))
        .env_remove("LANG")
        .env_remove("LC_ALL");
    assert_eq!(
        process::capture(cmd, Duration::from_secs(15), 1024).unwrap(),
        b"clean\n"
    );
    assert!(!temp.0.join("home/panic.log").exists());
    assert!(!temp.0.join("runtime-key").exists());
}

#[test]
#[ignore = "downloads the pinned official release over HTTPS into a private temporary directory"]
fn official_release_install_and_profile_doctor() {
    let temp = Temp::new();
    install::install(&temp.0).unwrap();
    install::verify_installed(&temp.0).unwrap();
    install::install(&temp.0).unwrap();
    client_version(&temp.0).unwrap();
    let config = fixture(&temp.0);
    private::write(&config.executable, b"#!/bin/sh\nexit 0\n", true).unwrap();
    private::write(
        &temp.0.join("profile.yaml"),
        &config::profile(&config, &temp.0).unwrap(),
        false,
    )
    .unwrap();
    let key = secret::Secret::from_bytes(b"sk-runtime-test-only-not-real-12345".to_vec()).unwrap();
    let mut cmd = daemon::tunnel_command(&temp.0, &config, &key, "doctor").unwrap();
    cmd.args(["--explain", "--json"]);
    let (success, raw) = process::capture_status(cmd, Duration::from_secs(30), 65536).unwrap();
    assert!(success, "{}", doctor_report(&raw).unwrap());
    let value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(value["result"], "ok");
    assert!(!String::from_utf8(raw).unwrap().contains(key.text()));
}

#[test]
#[ignore = "requires built zai, local Docker and official client download; uses only a dummy runtime key"]
fn official_client_managed_lifecycle_and_cleanup() {
    let temp = Temp::new();
    let root = temp.0.join("bridge ' 日本語");
    private::directory(&root).unwrap();
    let bin = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("zai");
    assert_eq!(
        crate::test_util::zai_gate_at(
            &bin,
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            env!("CARGO_PKG_VERSION")
        ),
        crate::test_util::ZaiVerdict::Usable
    );
    let (docker, endpoint) = docker::detect(&fixture(&temp.0).workspace).unwrap();
    let image = std::env::var("ZAIVERN_MCP_TEST_IMAGE").expect("set immutable fixture image");
    let mut config = fixture(&temp.0);
    config.executable = bin;
    config.docker = docker;
    config.docker_endpoint = endpoint;
    config.image = image;
    std::fs::create_dir(&config.workspace).unwrap();
    config.save(&root).unwrap();
    private::write(
        &root.join("profile.yaml"),
        &config::profile(&config, &root).unwrap(),
        false,
    )
    .unwrap();
    private::write(
        &root.join("runtime-key"),
        b"sk-runtime-test-only-not-real-12345",
        false,
    )
    .unwrap();
    install::install(&root).unwrap();
    real_cleanup_failure_and_recovery(&root, &config);
    // Never use an actual account credential or a production workspace. The
    // control plane may reject this key; local MCP and health still must work.
    for _ in 0..2 {
        daemon::start(&root, false).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while daemon::health(&root, "healthz").is_err() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        let health = daemon::health(&root, "healthz");
        let running_report = cleanup::report(&root).unwrap();
        let stopped = daemon::stop(&root);
        assert_eq!(health, Ok(true), "local health listener must start");
        stopped.unwrap();
        assert!(
            running_report.contains("Current generation: RUNNING"),
            "{running_report}"
        );
        assert!(!running_report.contains("UNCONFIRMED"), "{running_report}");
        assert!(!running_report.contains("repair"), "{running_report}");
        daemon::ensure_clean(&root).unwrap();
        let stopped_report = cleanup::report(&root).unwrap();
        assert!(stopped_report.contains("Previous cleanup: CONFIRMED"));
        assert!(!stopped_report.contains("repair"));
        assert!(daemon::request(&root, "status").is_err());
    }
    assert_eq!(std::fs::read_dir(&config.workspace).unwrap().count(), 0);
}

fn real_cleanup_failure_and_recovery(root: &Path, config: &Config) {
    use crate::features::chat_bridge::imp::{CleanupTracker, ResourceKind};
    let generation = private::nonce().unwrap();
    cleanup::Cleanup::prepare(root, config, &generation).unwrap();
    let tracker = cleanup::Cleanup::load(root, config).unwrap();
    tracker.admit(&generation).unwrap();
    let (volume, labels) = tracker.register(ResourceKind::Volume).unwrap();
    let mut cmd = docker::command(&config.docker, &config.docker_endpoint);
    cmd.args(["volume", "create", "--driver", "local"])
        .args(labels)
        .arg(&volume);
    process::capture(cmd, Duration::from_secs(30), 4096).unwrap();
    tracker.created(ResourceKind::Volume, &volume).unwrap();
    // The only resource this guard can remove was registered, labelled and
    // verified above. Keep cleanup ownership even if an assertion unwinds.
    struct CleanupGuard<'a>(&'a cleanup::Cleanup);
    impl Drop for CleanupGuard<'_> {
        fn drop(&mut self) {
            let _ = self.0.finish(true);
        }
    }
    let _guard = CleanupGuard(&tracker);
    let (container, labels) = tracker.register(ResourceKind::Container).unwrap();
    let mut cmd = docker::command(&config.docker, &config.docker_endpoint);
    cmd.args([
        "container",
        "create",
        "--name",
        &container,
        "--pull=never",
        "--network=none",
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "--pids-limit=128",
        "--memory=4g",
        "--cpus=2",
        "--entrypoint=sleep",
    ])
    .args(labels)
    .arg("--mount")
    .arg(format!(
        "type=volume,source={volume},target=/workspace,volume-nocopy"
    ))
    .arg(&config.image)
    .arg("1800");
    process::capture(cmd, Duration::from_secs(30), 4096).unwrap();
    tracker
        .created(ResourceKind::Container, &container)
        .unwrap();
    let mut failing = config.clone();
    failing.docker = root.join("docker-fault-fixture");
    let script = format!(
        "#!/bin/sh\nif [ \"$3\" = volume ] && [ \"$4\" = rm ]; then exit 19; fi\nexec {} \"$@\"\n",
        config::quote(config.docker.to_str().unwrap()).unwrap()
    );
    private::write(&failing.docker, script.as_bytes(), true).unwrap();
    let _operation = private::Lock::acquire(root, "operation.lock").unwrap();
    let _runtime = private::Lock::acquire(root, "runtime.lock").unwrap();
    assert!(cleanup::reconcile(root, &failing).is_err());
    assert!(!root.join("mcp.done").exists());
    assert!(daemon::ensure_clean(root).is_err());
    assert!(cleanup::report(root)
        .unwrap()
        .contains("Pending containers: 0\nPending volumes: 1"));
    cleanup::reconcile(root, config).unwrap();
    daemon::ensure_clean(root).unwrap();
    private::remove(&failing.docker).unwrap();
}
