//! Synchronous JSON-RPC/stdio subset of MCP 2024-11-05.
//! No network listener, shell arguments, or filesystem tools are exposed.
use super::task::ChatBridge;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

const MAX_LINE: usize = 64 * 1024;
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
            || id.as_ref().is_some_and(|v| !(v.is_string() || v.is_i64()))
        {
            write_response(&mut output, error(Value::Null, -32600, "Invalid request"))?;
            continue;
        }
        let method = method.unwrap_or_default();
        if id.is_none() {
            if initialized && method == "notifications/initialized" {
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
                    || !params.get("clientInfo").is_some_and(Value::is_object)
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
        {"name":"zaivern_run_task","description":"Run a coding task in an isolated Zaivern agent; returns a task ID immediately.",
         "inputSchema":{"type":"object","properties":{"instruction":{"type":"string","minLength":1,"maxLength":16384},"workspace":{"type":"string"}},"required":["instruction","workspace"],"additionalProperties":false}},
        {"name":"zaivern_task_status","description":"Get task state, changes and verification status.",
         "inputSchema":{"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"],"additionalProperties":false}},
        {"name":"zaivern_cancel_task","description":"Request cancellation. Poll status until cancelled or another terminal state.",
         "inputSchema":{"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"],"additionalProperties":false}}
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_initialize_list_and_validation() {
        let root = crate::test_util::unique_temp_dir("bridge", "protocol");
        std::fs::create_dir_all(&root).unwrap();
        let bridge = ChatBridge::new(
            root.clone(),
            std::sync::Arc::new(super::super::task::tests::Fake),
        );
        let requests = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":VERSION,"clientInfo":{"name":"test","version":"1"},"capabilities":{}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"zaivern_run_task","arguments":{"instruction":"","workspace":root}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"zaivern_task_status","arguments":{"task_id":"unknown"}}}),
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
        assert_eq!(results.len(), 4);
        assert_eq!(results[0]["result"]["protocolVersion"], VERSION);
        assert_eq!(results[1]["result"]["tools"].as_array().unwrap().len(), 3);
        assert_eq!(results[2]["result"]["isError"], true);
        assert_eq!(results[3]["result"]["isError"], true);
        std::fs::remove_dir_all(root).unwrap();
    }
}
