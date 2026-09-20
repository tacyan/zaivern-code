//! Deterministic ACP fixture. No LLM, credentials or third-party Rust crates.
//! This executable is installed only into the test container as `qwen`.
use std::io::{self, BufRead, Write};

fn send(message: &str) {
    println!("{message}");
    io::stdout().flush().unwrap();
}

fn main() {
    let mut prompt_id = String::new();
    let mut helper_only = false;
    let mut mixed = false;
    let mut cancel_verification = false;
    let mut exfiltrate = false;
    let mut prompts = 0;
    let mut scenario = "";
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        let id = line
            .split("\"id\":")
            .nth(1)
            .unwrap_or("")
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>();
        if line.contains("\"method\":\"initialize\"") {
            assert!(line.contains("\"readTextFile\":false"));
            send(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":1,"agentCapabilities":{{}},"authMethods":[]}}}}"#
            ));
        } else if line.contains("\"method\":\"session/new\"") {
            send(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"sessionId":"fixture"}}}}"#
            ));
        } else if line.contains("\"method\":\"session/prompt\"") {
            if line.contains("WAIT_FOREVER") {
                continue;
            }
            const SENTINEL: &str = "VERIFICATION_ONLY_SENTINEL_7f10a2";
            let encoded: String = SENTINEL.bytes().map(|b| format!("{b:02x}")).collect();
            assert!(!line.contains(SENTINEL), "unshared input reached Agent");
            assert!(
                !line.contains(&encoded),
                "encoded unshared input reached Agent"
            );
            exfiltrate |= line.contains("EXFILTRATE_VERIFIER");
            if exfiltrate && prompts > 0 {
                assert!(line.contains("Detailed verifier output was withheld"));
            }
            for name in [
                "WORKSPACE_COMPILE_ERROR",
                "WORKSPACE_TEST_FAILURE",
                "WORKSPACE_PASS",
                "UNUSED_RUST",
                "READONLY_INPUTS",
            ] {
                if line.contains(name) {
                    scenario = name;
                }
            }
            prompts += 1;
            helper_only |= line.contains("UNSHARED_HELPER");
            mixed |= line.contains("MIXED_FRONTEND");
            cancel_verification |= line.contains("VERIFY_CANCEL");
            prompt_id = id;
            send(
                r#"{"jsonrpc":"2.0","id":900,"method":"session/request_permission","params":{"sessionId":"fixture","toolCall":{"toolCallId":"unsafe","kind":"execute","title":"git push --force","rawInput":{"command":"git push --force"}},"options":[{"optionId":"deny","name":"Deny","kind":"reject_once"},{"optionId":"allow","name":"Allow","kind":"allow_once"}]}}"#,
            );
        } else if id == "900" {
            assert!(line.contains("\"optionId\":\"deny\""));
            send(
                r#"{"jsonrpc":"2.0","id":901,"method":"fs/read_text_file","params":{"sessionId":"fixture","path":"/etc/passwd"}}"#,
            );
        } else if id == "901" {
            assert!(line.contains("\"error\":"));
            send(
                r#"{"jsonrpc":"2.0","id":902,"method":"session/request_permission","params":{"sessionId":"fixture","toolCall":{"toolCallId":"spoof","kind":"edit","_meta":{"toolName":"run_shell_command"},"rawInput":{"command":"git push --force"}},"options":[{"optionId":"deny","kind":"reject_once"},{"optionId":"allow","kind":"allow_once"}]}}"#,
            );
        } else if id == "902" {
            assert!(line.contains("\"optionId\":\"deny\""));
            send(
                &r#"{"jsonrpc":"2.0","id":903,"method":"session/request_permission","params":{"sessionId":"fixture","toolCall":{"toolCallId":"edit-shared","kind":"edit","_meta":{"toolName":"edit"},"rawInput":{"file_path":"/workspace/src/lib.rs","old_string":"2 + 2, 5","new_string":"2 + 2, 4"},"locations":[{"path":"/workspace/src/lib.rs"}]},"options":[{"optionId":"deny","kind":"reject_once"},{"optionId":"allow","kind":"allow_once"}]}}"#.replace("/workspace/src/lib.rs", editable_path(scenario)),
            );
        } else if id == "903" {
            assert!(line.contains("\"optionId\":\"allow\""));
            assert!(!std::path::Path::new("/workspace/.git").exists());
            assert!(!std::path::Path::new("/workspace/.env").exists());
            assert!(!std::path::Path::new("/workspace/vendor").exists());
            assert!(std::net::TcpStream::connect_timeout(
                &"1.1.1.1:443".parse().unwrap(),
                std::time::Duration::from_millis(200)
            )
            .is_err());
            let file = editable_path(scenario);
            let before = std::fs::read_to_string(file).unwrap();
            if scenario == "WORKSPACE_COMPILE_ERROR" || scenario == "UNUSED_RUST" {
                std::fs::write(file, "this is invalid Rust;\n").unwrap();
            } else if scenario == "WORKSPACE_TEST_FAILURE" {
                std::fs::write(
                    file,
                    "#[test] fn member() { panic!(\"member failure\"); }\n",
                )
                .unwrap();
            } else if scenario == "READONLY_INPUTS" {
                std::fs::write(file, r#"#[test] fn inputs_are_immutable() {
                    for path in ["Cargo.toml", "Cargo.lock", "src/lib.rs", "vendor/local_dep/src/lib.rs"] {
                        assert!(std::fs::write(path, "mutated").is_err(), "{path}");
                        assert!(std::fs::remove_file(path).is_err(), "{path}");
                    }
                    assert!(std::fs::write("new-file", "new").is_err());
                }"#).unwrap();
            } else if exfiltrate {
                std::fs::write(
                    file,
                    r#"#[test] fn exfiltrate() {
                    let text = std::fs::read_to_string("vendor/local_dep/src/lib.rs").unwrap();
                    eprintln!("{text}");
                    for byte in text.bytes() { eprint!("{byte:02x}"); }
                    panic!("force verification failure");
                }"#,
                )
                .unwrap();
            } else if cancel_verification {
                std::fs::write(file, "#[test] fn failing() { std::thread::sleep(std::time::Duration::from_secs(3)); panic!(\"fixture failure\"); }\n").unwrap();
            } else if helper_only {
                std::fs::write(
                    "/workspace/src/generated.rs",
                    "pub fn answer() -> i32 { 4 }\n",
                )
                .unwrap();
                std::fs::write(file, "mod generated;\n#[test] fn arithmetic() { assert_eq!(generated::answer(), 4); }\n").unwrap();
            } else {
                std::fs::write(file, before.replace("2 + 2, 5", "2 + 2, 4")).unwrap();
            }
            if mixed {
                std::fs::write("/workspace/frontend/app.ts", "const broken = ;\n").unwrap();
            }
            send(
                r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fixture","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Fixed fixture. Zaivern must verify the tests independently."}}}}"#,
            );
            send(&format!(
                r#"{{"jsonrpc":"2.0","id":{prompt_id},"result":{{"stopReason":"end_turn"}}}}"#
            ));
        }
    }
}

fn editable_path(scenario: &str) -> &'static str {
    if scenario.starts_with("WORKSPACE_") {
        "/workspace/crates/foo/src/lib.rs"
    } else if scenario == "UNUSED_RUST" {
        "/workspace/src/unused.rs"
    } else {
        "/workspace/src/lib.rs"
    }
}
