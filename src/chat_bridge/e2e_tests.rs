//! Opt-in real-binary + real-container test. No LLM or API credential is used.
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
        serde_json::from_str(
            &rx.recv_timeout(Duration::from_secs(10))
                .expect("MCP response timeout"),
        )
        .unwrap()
    };
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
        serde_json::from_str(
            &rx.recv_timeout(Duration::from_secs(10))
                .expect("MCP response timeout"),
        )
        .unwrap()
    };
    assert_eq!(
        call("tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    for instruction in [
        "Fix the failing test and show the diff",
        "WAIT_FOREVER",
        "UNSHARED_HELPER",
    ] {
        let response = call(
            "tools/call",
            json!({"name":"zaivern_run_task","arguments":{"instruction":instruction,"workspace":workspace}}),
        );
        assert_eq!(response["result"]["isError"], false, "{response}");
        let id: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        if instruction == "WAIT_FOREVER" {
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
                if status["progress"] == "agent executing" {
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
            assert_ne!(status["state"], "failed", "{status}");
            if status["state"] == "completed" {
                assert_ne!(instruction, "WAIT_FOREVER");
                if instruction == "UNSHARED_HELPER" {
                    assert_eq!(status["test_status"], "failed", "{status}");
                    assert!(!workspace.join("src/generated.rs").exists());
                    break;
                }
                assert_eq!(status["test_status"], "passed", "{status}");
                assert_eq!(status["changed_files"], json!(["src/lib.rs"]));
                assert!(status["diff_summary"]
                    .as_str()
                    .unwrap()
                    .contains("+fn arithmetic()"));
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
