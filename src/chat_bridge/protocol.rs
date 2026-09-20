//! Synchronous JSON-RPC/stdio subset of MCP 2024-11-05.
//! No network listener, shell arguments, or filesystem tools are exposed.
use super::task::ChatBridge;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

// 16384 code points need at most 192 KiB as JSON surrogate-pair escapes.
// Reserve another 64 KiB for the envelope while keeping reads bounded.
const MAX_LINE: usize = 256 * 1024;
const VERSION: &str = "2024-11-05";

pub(super) fn serve(
    mut input: impl BufRead,
    mut output: impl Write,
    bridge: &ChatBridge,
) -> io::Result<()> {
    let mut initialized = false;
    let mut ready = false;
    loop {
        let mut bytes = Vec::new();
        // Limit before allocating/parsing; an oversized frame closes the stream.
        let n =
            std::io::Read::take(&mut input, (MAX_LINE + 1) as u64).read_until(b'\n', &mut bytes)?;
        if n == 0 {
            break;
        }
        if n > MAX_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP frame too large",
            ));
        }
        let request: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => {
                write_response(&mut output, error(Value::Null, -32700, "Parse error"))?;
                continue;
            }
        };
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str);
        if !request.is_object()
            || request.get("jsonrpc") != Some(&json!("2.0"))
            || method.is_none()
            || id
                .as_ref()
                .is_some_and(|v| !(v.is_string() || v.is_i64() || v.is_u64()))
        {
            write_response(&mut output, error(Value::Null, -32600, "Invalid request"))?;
            continue;
        }
        let method = method.unwrap_or_default();
        if id.is_none() {
            if initialized
                && method == "notifications/initialized"
                && request.get("params").is_none_or(Value::is_object)
            {
                ready = true;
            }
            continue;
        }
        let id = id.unwrap_or(Value::Null);
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        if !params.is_object() {
            write_response(
                &mut output,
                error(id, -32602, "Parameters must be an object"),
            )?;
            continue;
        }
        let result = match method {
            "initialize" if !initialized => {
                if !params.get("protocolVersion").is_some_and(Value::is_string)
                    || !params.get("clientInfo").is_some_and(|info| {
                        info.get("name").is_some_and(Value::is_string)
                            && info.get("version").is_some_and(Value::is_string)
                    })
                    || !params.get("capabilities").is_some_and(Value::is_object)
                {
                    Err((-32602, "Invalid initialize parameters"))
                } else {
                    initialized = true;
                    Ok(
                        json!({"protocolVersion":VERSION,"capabilities":{"tools":{}},
                        "serverInfo":{"name":"zaivern-chat-bridge","version":env!("CARGO_PKG_VERSION")}}),
                    )
                }
            }
            "ping" => Ok(json!({})),
            _ if !ready => Err((-32000, "Initialize the connection first")),
            "tools/list" => Ok(json!({"tools":tools()})),
            "tools/call" => match params.get("name").and_then(Value::as_str) {
                Some(
                    name @ ("zaivern_run_task" | "zaivern_task_status" | "zaivern_cancel_task"),
                ) => {
                    let args = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    let (value, failed) = match bridge.call(name, args) {
                        Ok(value) => (value, false),
                        Err(message) => (json!({"error":message}), true),
                    };
                    Ok(
                        json!({"content":[{"type":"text","text":value.to_string()}],"isError":failed}),
                    )
                }
                _ => Err((-32602, "Unknown tool")),
            },
            _ => Err((-32601, "Method not found")),
        };
        write_response(
            &mut output,
            match result {
                Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
                Err((code, message)) => error(id, code, message),
            },
        )?;
    }
    Ok(())
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
fn write_response(out: &mut impl Write, value: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, &value)?;
    out.write_all(b"\n")?;
    out.flush()
}
fn tools() -> Value {
    json!([
        {"name":"zaivern_run_task","description":"Run a coding task in an isolated Zaivern agent; returns a task ID immediately. The task may overwrite existing files in the server-configured workspace after successful Cargo verification, or without verification for unsupported projects.",
         "annotations":{"readOnlyHint":false,"openWorldHint":false,"destructiveHint":true},
         "inputSchema":{"type":"object","properties":{"instruction":{"type":"string","minLength":1,"maxLength":16384}},"required":["instruction"],"additionalProperties":false}},
        {"name":"zaivern_task_status","description":"Get task state, changes and verification status.",
         "annotations":{"readOnlyHint":true,"openWorldHint":false,"destructiveHint":false},
         "inputSchema":{"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"],"additionalProperties":false}},
        {"name":"zaivern_cancel_task","description":"Request cancellation. Poll status until cancelled or another terminal state.",
         "annotations":{"readOnlyHint":false,"openWorldHint":false,"destructiveHint":false},
         "inputSchema":{"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"],"additionalProperties":false}}
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_frame_budget_is_separate_from_character_validation() {
        for (unit, encoded) in [
            ("a", "a"),
            ("あ", "あ"),
            ("🦀", "🦀"),
            ("あ", r"\u3042"),
            ("🦀", r"\ud83e\udd80"),
        ] {
            for count in [16384, 16385] {
                let bridge = ChatBridge::new(
                    std::path::PathBuf::from("unused"),
                    std::sync::Arc::new(super::super::task::tests::Fake),
                );
                let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":VERSION,"clientInfo":{"name":"test","version":"1"},"capabilities":{}}});
                let ready = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
                let call = format!(
                    r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"zaivern_run_task","arguments":{{"instruction":"{}"}}}}}}"#,
                    encoded.repeat(count)
                );
                assert!(call.len() + 1 <= MAX_LINE);
                let decoded: Value = serde_json::from_str(&call).unwrap();
                assert_eq!(
                    decoded["params"]["arguments"]["instruction"],
                    unit.repeat(count)
                );
                let ping = json!({"jsonrpc":"2.0","id":3,"method":"ping"});
                let input = format!("{initialize}\n{ready}\n{call}\n{ping}\n");
                let mut output = Vec::new();
                serve(io::Cursor::new(input), &mut output, &bridge).unwrap();
                let rows: Vec<Value> = std::str::from_utf8(&output)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(rows.len(), 3);
                assert_eq!(rows[1]["result"]["isError"], count > 16384, "{rows:?}");
                if count > 16384 {
                    assert!(rows[1]["result"]["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("1..16384 characters"));
                }
                assert_eq!(rows[2]["result"], json!({}), "stream must remain open");
            }
        }
    }

    #[test]
    fn protocol_envelopes_and_initialization_order() {
        let bridge = ChatBridge::new(
            std::path::PathBuf::from("unused"),
            std::sync::Arc::new(super::super::task::tests::Fake),
        );
        let initialize = json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":VERSION,"clientInfo":{"name":"test","version":"1"},"capabilities":{}}});
        let requests = [
            "{malformed".into(),
            json!({"jsonrpc":"2.0","id":null,"method":"ping"}).to_string(),
            json!({"jsonrpc":"2.0","id":true,"method":"ping"}).to_string(),
            json!({"jsonrpc":"2.0","id":1.5,"method":"ping"}).to_string(),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string(),
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string(),
            initialize.to_string(),
            json!({"jsonrpc":"2.0","method":"notifications/initialized","params":[]}).to_string(),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}).to_string(),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string(),
            json!({"jsonrpc":"2.0","method":"unknown"}).to_string(),
            json!({"jsonrpc":"2.0","id":u64::MAX,"method":"ping"}).to_string(),
            json!({"jsonrpc":"2.0","id":"unknown","method":"unknown"}).to_string(),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"zaivern_run_task","arguments":{"instruction":"edit","workspace":"/tmp"}}}).to_string(),
        ];
        let mut output = Vec::new();
        serve(io::Cursor::new(requests.join("\n")), &mut output, &bridge).unwrap();
        let rows: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 10);
        for (index, code) in [
            (0, -32700),
            (1, -32600),
            (2, -32600),
            (3, -32600),
            (4, -32000),
            (6, -32000),
            (8, -32601),
        ] {
            assert_eq!(rows[index]["error"]["code"], code, "{rows:?}");
        }
        assert_eq!(rows[7]["id"], u64::MAX);
        assert_eq!(rows[9]["result"]["isError"], true);
        let schema = tools();
        assert_eq!(schema[0]["inputSchema"]["required"], json!(["instruction"]));
        assert!(schema[0]["inputSchema"]["properties"]
            .get("workspace")
            .is_none());
    }
    #[test]
    fn startup_initialize_list_and_validation() {
        let root = crate::test_util::unique_temp_dir("bridge", "protocol");
        std::fs::create_dir_all(&root).unwrap();
        let root = super::super::workspace::validate_root(&root).unwrap();
        let bridge = ChatBridge::new(
            root.clone(),
            std::sync::Arc::new(super::super::task::tests::Fake),
        );
        let requests = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":VERSION,"clientInfo":{"name":"test","version":"1"},"capabilities":{}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"zaivern_run_task","arguments":{"instruction":""}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"zaivern_task_status","arguments":{"task_id":"unknown"}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":[]}),
            json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"unknown"}}),
            json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"zaivern_run_task","arguments":{"instruction":"passed"}}}),
        ];
        let input = requests
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let mut output = Vec::new();
        serve(io::Cursor::new(input), &mut output, &bridge).unwrap();
        let results: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(results.len(), 7);
        assert_eq!(results[0]["result"]["protocolVersion"], VERSION);
        assert_eq!(results[1]["result"]["tools"].as_array().unwrap().len(), 3);
        for (name, read_only, destructive) in [
            ("zaivern_run_task", false, true),
            ("zaivern_task_status", true, false),
            ("zaivern_cancel_task", false, false),
        ] {
            let tool = results[1]["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap();
            assert_eq!(
                tool["annotations"]["readOnlyHint"].as_bool(),
                Some(read_only)
            );
            assert_eq!(tool["annotations"]["openWorldHint"].as_bool(), Some(false));
            assert_eq!(
                tool["annotations"]["destructiveHint"].as_bool(),
                Some(destructive)
            );
        }
        assert_eq!(results[2]["result"]["isError"], true);
        assert_eq!(results[3]["result"]["isError"], true);
        assert_eq!(results[4]["error"]["code"], -32602);
        assert_eq!(results[5]["error"]["code"], -32602);
        assert_eq!(results[6]["result"]["isError"], false);
        let task: Value =
            serde_json::from_str(results[6]["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert!(task["task_id"].as_str().is_some_and(|id| !id.is_empty()));
        let mut oversized = io::Cursor::new(vec![b'x'; MAX_LINE * 2]);
        let error = serve(&mut oversized, Vec::new(), &bridge).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(oversized.position(), (MAX_LINE + 1) as u64);
        drop(bridge);
        std::fs::remove_dir_all(root).unwrap();
    }
}
