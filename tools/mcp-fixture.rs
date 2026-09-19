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
            helper_only |= line.contains("UNSHARED_HELPER");
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
            assert!(!std::path::Path::new("/workspace/.git").exists());
            assert!(!std::path::Path::new("/workspace/.env").exists());
            assert!(std::net::TcpStream::connect_timeout(
                &"1.1.1.1:443".parse().unwrap(),
                std::time::Duration::from_millis(200)
            )
            .is_err());
            let file = "/workspace/src/lib.rs";
            let before = std::fs::read_to_string(file).unwrap();
            if helper_only {
                std::fs::write(
                    "/workspace/src/generated.rs",
                    "pub fn answer() -> i32 { 4 }\n",
                )
                .unwrap();
                std::fs::write(file, "mod generated;\n#[test] fn arithmetic() { assert_eq!(generated::answer(), 4); }\n").unwrap();
            } else {
                std::fs::write(file, before.replace("2 + 2, 5", "2 + 2, 4")).unwrap();
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
