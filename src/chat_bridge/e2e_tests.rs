//! Real-binary + real-container test, explicitly run in Ubuntu CI. No LLM/API key.
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if self.0.try_wait().ok().flatten().is_none() {
            crate::procx::kill_tree(self.0.id());
            let _ = self.0.wait();
        }
    }
}

#[test]
#[ignore = "requires Docker and an explicitly built tools/mcp-fixture.Dockerfile image"]
fn real_stdio_container_agent_edit_test_diff_and_cancel() {
    let image = std::env::var("ZAIVERN_MCP_TEST_IMAGE").expect("set immutable fixture image ID");
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
    let directory = crate::test_util::unique_temp_dir("bridge", "container-e2e");
    let workspace = directory.join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname=\"bridge_fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    for name in ["local_dep", "patched"] {
        let dependency = workspace.join("vendor").join(name);
        std::fs::create_dir_all(dependency.join("src")).unwrap();
        std::fs::write(
            dependency.join("Cargo.toml"),
            format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2021'\n"),
        )
        .unwrap();
        std::fs::write(
            dependency.join("src/lib.rs"),
            "// VERIFICATION_ONLY_SENTINEL_7f10a2\npub fn answer() -> u32 { 4 }\n",
        )
        .unwrap();
    }
    let manifest = std::fs::read_to_string(workspace.join("Cargo.toml")).unwrap();
    let lock = "version = 4\n[[package]]\nname='bridge_fixture'\nversion='0.1.0'\n";
    std::fs::write(workspace.join("Cargo.lock"), lock).unwrap();
    std::fs::write(
        workspace.join("src/lib.rs"),
        "#[test]\nfn arithmetic() { assert_eq!(2 + 2, 5); }\n",
    )
    .unwrap();
    std::fs::write(workspace.join(".env"), "SENTINEL=not-for-agent").unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let mut command = crate::procx::hidden_command_raw(&bin);
    command
        .args(["mcp", "serve", "--workspace"])
        .arg(&workspace)
        .args(["--image", &image])
        .env("ZAIVERN_HOME", directory.join("home"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut server = Server(command.spawn().unwrap());
    let output = server.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(output).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let mut sequence = 0;
    let mut call = |method: &str, params: Value| -> Value {
        sequence += 1;
        writeln!(
            server.0.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0","id":sequence,"method":method,"params":params})
        )
        .unwrap();
        server.0.stdin.as_mut().unwrap().flush().unwrap();
        let response = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("MCP response timeout");
        const SENTINEL: &str = "VERIFICATION_ONLY_SENTINEL_7f10a2";
        let encoded: String = SENTINEL.bytes().map(|b| format!("{b:02x}")).collect();
        assert!(!response.contains(SENTINEL));
        assert!(!response.contains(&encoded));
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], sequence);
        response
    };
    // A modern client can discover and list on fresh stdio without initialize.
    let metadata = json!({
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}
    });
    let modern_discovery = call("server/discover", json!({"_meta":metadata}));
    assert_eq!(modern_discovery["result"]["resultType"], "complete");
    let modern_list = call("tools/list", json!({"_meta":metadata}));
    assert_eq!(modern_list["result"]["resultType"], "complete");
    assert_eq!(modern_list["result"]["tools"].as_array().unwrap().len(), 3);
    let init = call(
        "initialize",
        json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}),
    );
    assert!(init.get("result").is_some(), "{init}");
    // A request uses a temporary borrow; release it to send the notification.
    writeln!(
        server.0.stdin.as_mut().unwrap(),
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    let mut call = |method: &str, params: Value| -> Value {
        sequence += 1;
        writeln!(
            server.0.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0","id":sequence,"method":method,"params":params})
        )
        .unwrap();
        server.0.stdin.as_mut().unwrap().flush().unwrap();
        let response = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("MCP response timeout");
        const SENTINEL: &str = "VERIFICATION_ONLY_SENTINEL_7f10a2";
        let encoded: String = SENTINEL.bytes().map(|b| format!("{b:02x}")).collect();
        assert!(!response.contains(SENTINEL));
        assert!(!response.contains(&encoded));
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], sequence);
        response
    };
    // Registration must discover the server as well as list tools. HTTP 200
    // from the tunnel is not sufficient if this is a JSON-RPC error.
    let discovery = call("server/discover", json!({}));
    assert!(discovery.get("error").is_none(), "{discovery}");
    assert_eq!(discovery["result"]["resultType"], "complete");
    assert_eq!(discovery["result"]["capabilities"], json!({"tools":{}}));
    assert_eq!(
        discovery["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "zaivern-chat-bridge"
    );
    let listed = call("tools/list", json!({}));
    assert_eq!(listed["result"]["tools"], modern_list["result"]["tools"]);
    assert_eq!(
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "zaivern_run_task",
            "zaivern_task_status",
            "zaivern_cancel_task"
        ]
    );
    for instruction in [
        "Fix the failing test and show the diff",
        "REPAIR_SUCCESS",
        "MIXED_FRONTEND",
        "WAIT_FOREVER",
        "UNSHARED_HELPER",
        "VERIFY_CANCEL",
        "WORKSPACE_COMPILE_ERROR",
        "WORKSPACE_TEST_FAILURE",
        "WORKSPACE_PASS",
        "UNUSED_RUST",
        "READONLY_INPUTS",
        "LOCK_MISSING",
        "LOCK_STALE",
        "EXFILTRATE_VERIFIER",
        "ORACLE_EVEN",
        "ORACLE_ODD",
        "NO_CARGO",
    ] {
        if instruction == "REPAIR_SUCCESS" {
            std::fs::write(
                workspace.join("src/lib.rs"),
                "#[test]\nfn arithmetic() { assert_eq!(2 + 2, 5); }\n",
            )
            .unwrap();
        }
        let hidden = matches!(
            instruction,
            "EXFILTRATE_VERIFIER" | "ORACLE_EVEN" | "ORACLE_ODD"
        );
        if hidden {
            std::fs::write(workspace.join("Cargo.toml"), format!("{manifest}\n[dependencies]\nlocal_dep={{path='vendor/local_dep'}}\npatched='0.1.0'\n[patch.crates-io]\npatched={{path='vendor/patched'}}\n")).unwrap();
            std::fs::write(workspace.join("Cargo.lock"), "version = 4\n[[package]]\nname='bridge_fixture'\nversion='0.1.0'\ndependencies=['local_dep','patched']\n[[package]]\nname='local_dep'\nversion='0.1.0'\n[[package]]\nname='patched'\nversion='0.1.0'\n").unwrap();
            let bit = usize::from(instruction == "ORACLE_ODD");
            std::fs::write(
                workspace.join("vendor/local_dep/src/lib.rs"),
                format!(
                    "// {bit} VERIFICATION_ONLY_SENTINEL_7f10a2\npub fn answer() -> u32 {{ 4 }}\n"
                ),
            )
            .unwrap();
            std::fs::write(workspace.join("src/lib.rs"), "pub fn original() {}\n").unwrap();
        }
        if instruction.starts_with("WORKSPACE_") {
            std::fs::write(
                workspace.join("Cargo.toml"),
                format!("{manifest}\n[workspace]\nmembers=['crates/foo']\ndefault-members=['.']\n"),
            )
            .unwrap();
            std::fs::create_dir_all(workspace.join("crates/foo/src")).unwrap();
            std::fs::write(
                workspace.join("crates/foo/Cargo.toml"),
                "[package]\nname='foo'\nversion='0.1.0'\nedition='2021'\n",
            )
            .unwrap();
            std::fs::write(
                workspace.join("crates/foo/src/lib.rs"),
                "#[test] fn member() { assert_eq!(2 + 2, 5); }\n",
            )
            .unwrap();
            std::fs::write(
                workspace.join("Cargo.lock"),
                format!("{lock}[[package]]\nname='foo'\nversion='0.1.0'\n"),
            )
            .unwrap();
            std::fs::write(
                workspace.join("src/lib.rs"),
                "#[test] fn root() { assert_eq!(2 + 2, 4); }\n",
            )
            .unwrap();
        }
        if instruction == "UNUSED_RUST" {
            std::fs::write(workspace.join("src/unused.rs"), "pub fn unused() {}\n").unwrap();
        }
        if instruction == "LOCK_MISSING" {
            std::fs::remove_file(workspace.join("Cargo.lock")).unwrap();
        }
        if instruction == "LOCK_STALE" {
            std::fs::write(workspace.join("Cargo.lock"), "version = 4\n").unwrap();
        }
        if instruction == "MIXED_FRONTEND" {
            std::fs::create_dir_all(workspace.join("frontend")).unwrap();
            std::fs::write(workspace.join("frontend/app.ts"), "const valid = 1;\n").unwrap();
            std::fs::write(
                workspace.join("src/lib.rs"),
                "#[test]\nfn arithmetic() { assert_eq!(2 + 2, 5); }\n",
            )
            .unwrap();
        }
        if instruction == "NO_CARGO" {
            std::fs::remove_file(workspace.join("Cargo.toml")).unwrap();
            std::fs::write(
                workspace.join("src/lib.rs"),
                "#[test]\nfn arithmetic() { assert_eq!(2 + 2, 5); }\n",
            )
            .unwrap();
        }
        let original_lib = std::fs::read(workspace.join("src/lib.rs")).unwrap();
        let original_manifest = std::fs::read(workspace.join("Cargo.toml")).ok();
        let original_lock = std::fs::read(workspace.join("Cargo.lock")).ok();
        let original_member = std::fs::read(workspace.join("crates/foo/src/lib.rs")).ok();
        let original_dependency =
            std::fs::read(workspace.join("vendor/local_dep/src/lib.rs")).unwrap();
        let expect_failure = matches!(
            instruction,
            "UNSHARED_HELPER"
                | "VERIFY_CANCEL"
                | "WORKSPACE_COMPILE_ERROR"
                | "WORKSPACE_TEST_FAILURE"
                | "LOCK_MISSING"
                | "LOCK_STALE"
        );
        let original_env = std::fs::read(workspace.join(".env")).unwrap();
        let response = call(
            "tools/call",
            json!({"name":"zaivern_run_task","arguments":{"instruction":instruction}}),
        );
        assert_eq!(response["result"]["isError"], false, "{response}");
        let id: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        if instruction == "WAIT_FOREVER" || instruction == "VERIFY_CANCEL" {
            let deadline = Instant::now() + Duration::from_secs(90);
            loop {
                let response = call(
                    "tools/call",
                    json!({"name":"zaivern_task_status","arguments":id}),
                );
                let status: Value = serde_json::from_str(
                    response["result"]["content"][0]["text"].as_str().unwrap(),
                )
                .unwrap();
                assert_ne!(status["state"], "failed", "{status}");
                let progress = if instruction == "VERIFY_CANCEL" {
                    "verifying cargo tests"
                } else {
                    "agent executing"
                };
                if status["progress"] == progress {
                    break;
                }
                assert!(Instant::now() < deadline, "{status}");
                std::thread::sleep(Duration::from_millis(100));
            }
            let response = call(
                "tools/call",
                json!({"name":"zaivern_cancel_task","arguments":id}),
            );
            assert_eq!(response["result"]["isError"], false);
        }
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let response = call(
                "tools/call",
                json!({"name":"zaivern_task_status","arguments":id}),
            );
            let status: Value =
                serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            if status["state"] == "failed" {
                assert!(expect_failure, "{status}");
                assert_eq!(status["test_status"], "failed", "{status}");
                assert_eq!(status["changed_files"], json!([]), "{status}");
                assert!(
                    status["error"]
                        .as_str()
                        .unwrap()
                        .contains("verification failed; changes were not imported"),
                    "{status}"
                );
                assert_eq!(
                    std::fs::read(workspace.join("src/lib.rs")).unwrap(),
                    original_lib
                );
                assert_eq!(
                    std::fs::read(workspace.join("Cargo.lock")).ok(),
                    original_lock
                );
                assert_eq!(
                    std::fs::read(workspace.join("crates/foo/src/lib.rs")).ok(),
                    original_member
                );
                assert!(!workspace.join("src/generated.rs").exists());
                assert_eq!(
                    std::fs::read(workspace.join("Cargo.toml")).ok(),
                    original_manifest
                );
                assert_eq!(std::fs::read(workspace.join(".env")).unwrap(), original_env);
                break;
            }
            if status["state"] == "completed" {
                assert_ne!(instruction, "WAIT_FOREVER");
                assert!(!expect_failure, "{status}");
                let verification = if hidden
                    || matches!(instruction, "NO_CARGO" | "MIXED_FRONTEND" | "UNUSED_RUST")
                {
                    "not_verified"
                } else {
                    "passed"
                };
                assert_eq!(status["test_status"], verification, "{status}");
                assert_eq!(status["build_status"], "not_verified", "{status}");
                if hidden {
                    let summary = status["summary"].as_str().unwrap();
                    assert!(
                        summary.contains(
                            "not run because Cargo verification requires unshared inputs"
                        ),
                        "{status}"
                    );
                    assert!(!summary.contains("frozen passed"), "{status}");
                    assert!(
                        summary.contains("prompt_count=1"),
                        "repair oracle: {status}"
                    );
                }
                if instruction == "REPAIR_SUCCESS" {
                    assert!(
                        status["summary"]
                            .as_str()
                            .unwrap()
                            .contains("prompt_count=2"),
                        "{status}"
                    );
                }
                if instruction == "MIXED_FRONTEND" {
                    assert_eq!(
                        status["changed_files"],
                        json!(["frontend/app.ts", "src/lib.rs"])
                    );
                    assert!(status["summary"]
                        .as_str()
                        .unwrap()
                        .contains("cargo test --workspace --frozen passed"));
                } else {
                    let changed = match instruction {
                        "WORKSPACE_PASS" => "crates/foo/src/lib.rs",
                        "UNUSED_RUST" => "src/unused.rs",
                        _ => "src/lib.rs",
                    };
                    assert_eq!(status["changed_files"], json!([changed]), "{status}");
                }
                let expected_line = match instruction {
                    "WORKSPACE_PASS" => "+#[test] fn member() { assert_eq!(2 + 2, 4); }",
                    "UNUSED_RUST" => "+this is invalid Rust;",
                    "READONLY_INPUTS" => "+#[test] fn inputs_are_immutable()",
                    "EXFILTRATE_VERIFIER" => "+#[test] fn exfiltrate()",
                    "ORACLE_EVEN" | "ORACLE_ODD" => "+#[test] fn oracle()",
                    _ => "+fn arithmetic()",
                };
                assert!(
                    status["diff_summary"]
                        .as_str()
                        .unwrap()
                        .contains(expected_line),
                    "{status}"
                );
                if instruction == "WORKSPACE_PASS" {
                    assert_eq!(
                        std::fs::read_to_string(workspace.join("crates/foo/src/lib.rs")).unwrap(),
                        "#[test] fn member() { assert_eq!(2 + 2, 4); }\n"
                    );
                }
                if instruction == "UNUSED_RUST" {
                    assert_eq!(
                        std::fs::read_to_string(workspace.join("src/unused.rs")).unwrap(),
                        "this is invalid Rust;\n"
                    );
                }
                assert_eq!(
                    std::fs::read(workspace.join("Cargo.lock")).ok(),
                    original_lock
                );
                assert_eq!(
                    std::fs::read(workspace.join("Cargo.toml")).ok(),
                    original_manifest
                );
                assert_eq!(
                    std::fs::read(workspace.join("vendor/local_dep/src/lib.rs")).unwrap(),
                    original_dependency
                );
                break;
            }
            if status["state"] == "cancelled" {
                assert_eq!(instruction, "WAIT_FOREVER");
                break;
            }
            assert!(Instant::now() < deadline, "{status}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    assert!(!workspace.join("src/generated.rs").exists());
    drop(server);
    std::fs::remove_dir_all(directory).unwrap();
}
