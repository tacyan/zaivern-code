//! Bounded JSON-RPC/stdio MCP server, with legacy initialization and modern
//! per-request discovery. Only the tools capability is advertised.
//! No network listener, shell arguments, or filesystem tools are exposed.
use super::task::ChatBridge;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

// 16384 code points need at most 192 KiB as JSON surrogate-pair escapes.
// Reserve another 64 KiB for the envelope while keeping reads bounded.
const MAX_LINE: usize = 256 * 1024;
// The tools-only stdio surface is compatible with these legacy revisions.
// A modern revision cannot be negotiated through the legacy initialize RPC.
const VERSION: &str = "2025-11-25";
const MODERN_VERSION: &str = "2026-07-28";
const VERSIONS: &[&str] = &[MODERN_VERSION, VERSION, "2025-06-18", "2024-11-05"];
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_CLIENT: &str = "io.modelcontextprotocol/clientInfo";

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
        let modern = match request_mode(&params, &id) {
            Ok(modern) => modern,
            Err(response) => {
                write_response(&mut output, response)?;
                continue;
            }
        };
        let result = match method {
            "initialize" if !modern => {
                if !params.get("protocolVersion").is_some_and(Value::is_string)
                    || !params.get("clientInfo").is_some_and(valid_implementation)
                    || !params.get("capabilities").is_some_and(Value::is_object)
                {
                    Err((-32602, "Invalid initialize parameters"))
                } else {
                    initialized = true;
                    ready = false;
                    let requested = params["protocolVersion"].as_str().unwrap_or_default();
                    let version = if VERSIONS[1..].contains(&requested) {
                        requested
                    } else {
                        VERSION
                    };
                    Ok(
                        json!({"protocolVersion":version,"capabilities":capabilities(),
                        "serverInfo":server_info()}),
                    )
                }
            }
            // Discovery is safe before initialization and never authorizes a
            // legacy tools/call. Its schema is the published 2026-07-28 one.
            "server/discover" => Ok(json!({
                "resultType":"complete", "supportedVersions":VERSIONS,
                "cacheScope":"private", "ttlMs":0,
                "capabilities":capabilities(),
                "_meta":{"io.modelcontextprotocol/serverInfo":server_info()}
            })),
            "ping" => Ok(json!({})),
            "tools/list" | "tools/call" if !modern && !ready => {
                Err((-32000, "Initialize the connection first"))
            }
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
                Ok(mut value) => {
                    if modern {
                        value["resultType"] = json!("complete");
                        value["_meta"] =
                            json!({"io.modelcontextprotocol/serverInfo":server_info()});
                        if method == "tools/list" {
                            value["cacheScope"] = json!("private");
                            value["ttlMs"] = json!(0);
                        }
                    }
                    json!({"jsonrpc":"2.0","id":id,"result":value})
                }
                Err((code, message)) => error(id, code, message),
            },
        )?;
    }
    Ok(())
}

fn capabilities() -> Value {
    json!({"tools":{}})
}

fn server_info() -> Value {
    json!({"name":"zaivern-chat-bridge","version":env!("CARGO_PKG_VERSION")})
}

fn valid_implementation(info: &Value) -> bool {
    info.get("name").is_some_and(Value::is_string)
        && info.get("version").is_some_and(Value::is_string)
}

// Modern metadata is scoped to this request, never inferred from discovery or
// cached on the connection. Legacy callers may still use unrelated _meta keys.
fn request_mode(params: &Value, id: &Value) -> Result<bool, Value> {
    let invalid = || error(id.clone(), -32602, "Invalid request metadata");
    let Some(meta) = params.get("_meta") else {
        return Ok(false);
    };
    let meta = meta.as_object().ok_or_else(invalid)?;
    let Some(version) = meta.get(META_VERSION) else {
        return if meta.contains_key(META_CAPABILITIES) || meta.contains_key(META_CLIENT) {
            Err(invalid())
        } else {
            Ok(false)
        };
    };
    let version = version.as_str().ok_or_else(invalid)?;
    if !VERSIONS.contains(&version) {
        let mut response = error(id.clone(), -32022, "Unsupported protocol version");
        response["error"]["data"] = json!({"supported":VERSIONS,"requested":version});
        return Err(response);
    }
    if version != MODERN_VERSION {
        return Ok(false);
    }
    if !meta.get(META_CAPABILITIES).is_some_and(Value::is_object)
        || meta
            .get(META_CLIENT)
            .is_some_and(|info| !valid_implementation(info))
    {
        return Err(invalid());
    }
    Ok(true)
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
        {"name":"zaivern_run_task","description":"Run a coding task in an isolated Zaivern agent; returns a task ID immediately. The task may overwrite existing files in the server-configured workspace after successful Cargo verification, or without verification when required inputs are unshared or the project is unsupported.",
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
    fn exchange(requests: &[Value]) -> Vec<Value> {
        let bridge = ChatBridge::new(
            std::path::PathBuf::from("unused"),
            std::sync::Arc::new(super::super::task::tests::Fake),
        );
        let input = requests
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let mut output = Vec::new();
        serve(io::Cursor::new(input), &mut output, &bridge).unwrap();
        std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn modern_meta() -> Value {
        json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28",
            "io.modelcontextprotocol/clientCapabilities":{},
            "io.modelcontextprotocol/clientInfo":{"name":"discovery-regression","version":"1"}})
    }

    fn assert_tools(result: &Value) {
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "zaivern_run_task",
                "zaivern_task_status",
                "zaivern_cancel_task"
            ]
        );
        for (tool, argument) in tools.iter().zip(["instruction", "task_id", "task_id"]) {
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["required"], json!([argument]));
            assert_eq!(schema["additionalProperties"], false);
            assert_eq!(schema["properties"].as_object().unwrap().len(), 1);
            assert_eq!(schema["properties"][argument]["type"], "string");
            assert_eq!(tool["annotations"]["openWorldHint"], false);
        }
    }

    #[test]
    fn legacy_versions_are_negotiated_without_claiming_unknown_versions() {
        for (requested, expected) in [
            ("2024-11-05", "2024-11-05"),
            ("2025-06-18", "2025-06-18"),
            ("2025-11-25", "2025-11-25"),
            // 2025-03-26 required batching, which this bounded stdio server
            // does not implement. Offer a supported legacy revision instead.
            ("2025-03-26", "2025-11-25"),
            ("2026-07-28", "2025-11-25"),
            ("unknown", "2025-11-25"),
        ] {
            let init = json!({"jsonrpc":"2.0","id":-1,"method":"initialize","params":{"protocolVersion":requested,"clientInfo":{"name":"test","version":"1"},"capabilities":{}}});
            let rows = exchange(&[
                init.clone(),
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                init,
                json!({"jsonrpc":"2.0","id":"before-notification","method":"tools/list"}),
                json!({"jsonrpc":"2.0","id":"before-call","method":"tools/call","params":{"name":"zaivern_run_task","arguments":{"instruction":"must not run"}}}),
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                json!({"jsonrpc":"2.0","id":"list","method":"tools/list"}),
            ]);
            for row in &rows[..2] {
                assert_eq!(row["id"], -1);
                assert_eq!(row["result"]["protocolVersion"], expected);
                assert_eq!(row["result"]["capabilities"], json!({"tools":{}}));
                assert_eq!(row["result"]["serverInfo"]["name"], "zaivern-chat-bridge");
                assert_eq!(
                    row["result"]["serverInfo"]["version"],
                    env!("CARGO_PKG_VERSION")
                );
            }
            assert_eq!(rows[2]["error"]["code"], -32000);
            assert_eq!(rows[3]["error"]["code"], -32000);
            assert_tools(&rows[4]["result"]);
        }
    }

    #[test]
    fn modern_discovery_and_inline_calls_do_not_need_or_change_legacy_initialization() {
        for discover_first in [false, true] {
            let mut requests = Vec::new();
            if discover_first {
                requests.push(json!({"jsonrpc":"2.0","id":"discover","method":"server/discover","params":{"_meta":modern_meta()}}));
            }
            requests.extend([
                json!({"jsonrpc":"2.0","id":"list","method":"tools/list","params":{"_meta":modern_meta()}}),
                json!({"jsonrpc":"2.0","id":u64::MAX,"method":"tools/call","params":{"_meta":modern_meta(),"name":"zaivern_task_status","arguments":{"task_id":"unknown"}}}),
                json!({"jsonrpc":"2.0","id":"legacy-list","method":"tools/list"}),
                json!({"jsonrpc":"2.0","id":"probe","method":"server/discover"}),
                json!({"jsonrpc":"2.0","id":"legacy-call","method":"tools/call","params":{"name":"zaivern_run_task","arguments":{"instruction":"must not run"}}}),
            ]);
            let rows = exchange(&requests);
            assert_eq!(rows.len(), requests.len());
            let offset = usize::from(discover_first);
            assert_tools(&rows[offset]["result"]);
            assert_eq!(rows[offset]["result"]["resultType"], "complete");
            assert_eq!(rows[offset]["result"]["cacheScope"], "private");
            assert_eq!(rows[offset]["result"]["ttlMs"], 0);
            assert_eq!(rows[offset + 1]["id"], u64::MAX);
            assert_eq!(rows[offset + 1]["result"]["resultType"], "complete");
            assert_eq!(rows[offset + 1]["result"]["isError"], true);
            assert_eq!(rows[offset + 1]["result"]["content"][0]["type"], "text");
            assert_eq!(rows[offset + 2]["error"]["code"], -32000);
            assert_eq!(rows[offset + 4]["error"]["code"], -32000);
            let discovery = &rows[offset + 3]["result"];
            assert_eq!(discovery["cacheScope"], "private");
            assert_eq!(discovery["ttlMs"], 0);
            assert_eq!(
                discovery["supportedVersions"],
                json!(["2026-07-28", "2025-11-25", "2025-06-18", "2024-11-05"])
            );
            assert_eq!(
                discovery["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
                "zaivern-chat-bridge"
            );
            assert!(
                discovery.get("serverInfo").is_none(),
                "use the published schema, not the draft SEP"
            );
        }
    }

    #[test]
    fn modern_version_errors_and_invalid_metadata_do_not_execute_tools() {
        let mut unsupported = modern_meta();
        unsupported[META_VERSION] = json!("2099-01-01");
        let rows = exchange(&[
            json!({"jsonrpc":"2.0","id":"unsupported","method":"tools/call","params":{"_meta":unsupported,"name":"zaivern_run_task","arguments":{"instruction":"must not run"}}}),
            json!({"jsonrpc":"2.0","id":"retry","method":"server/discover","params":{"_meta":modern_meta()}}),
        ]);
        assert_eq!(
            rows[0],
            json!({"jsonrpc":"2.0","id":"unsupported","error":{"code":-32022,"message":"Unsupported protocol version","data":{"supported":VERSIONS,"requested":"2099-01-01"}}})
        );
        assert!(rows[1].get("error").is_none());
        for meta in [
            json!(null),
            json!([]),
            json!({META_VERSION:42}),
            json!({META_VERSION:MODERN_VERSION}),
            json!({META_CAPABILITIES:{}}),
            json!({META_VERSION:MODERN_VERSION,META_CAPABILITIES:[]}),
            json!({META_VERSION:MODERN_VERSION,META_CAPABILITIES:{},META_CLIENT:{"name":"missing version"}}),
        ] {
            let rows = exchange(&[
                json!({"jsonrpc":"2.0","id":"bad","method":"tools/call","params":{"_meta":meta,"name":"zaivern_run_task","arguments":{"instruction":"must not run"}}}),
            ]);
            assert_eq!(rows[0]["error"]["code"], -32602, "{rows:?}");
            assert_eq!(rows[0]["id"], "bad");
        }
    }

    #[test]
    fn discovery_never_exposes_host_operations_or_unadvertised_capabilities() {
        let mut requests = vec![
            json!({"jsonrpc":"2.0","id":0,"method":"server/discover","params":{"_meta":modern_meta()}}),
        ];
        for method in [
            "unknown",
            "resources/list",
            "resources/templates/list",
            "prompts/list",
        ] {
            requests.push(json!({"jsonrpc":"2.0","id":method,"method":method,"params":{"_meta":modern_meta()}}));
        }
        for name in [
            "execute",
            "delete",
            "move",
            "fetch",
            "zaivern_execute",
            "zaivern_delete",
            "zaivern_move",
            "zaivern_fetch",
        ] {
            requests.push(json!({"jsonrpc":"2.0","id":name,"method":"tools/call","params":{"_meta":modern_meta(),"name":name,"arguments":{}}}));
        }
        requests.push(json!({"jsonrpc":"2.0","id":"workspace","method":"tools/call","params":{"_meta":modern_meta(),"name":"zaivern_run_task","arguments":{"instruction":"edit","workspace":"/outside"}}}));
        let rows = exchange(&requests);
        assert_eq!(rows.len(), requests.len());
        assert_eq!(rows[0]["result"]["capabilities"], json!({"tools":{}}));
        for (i, row) in rows.iter().enumerate().take(13).skip(1) {
            assert_eq!(row["id"], requests[i]["id"]);
            assert_eq!(row["error"]["code"], if i < 5 { -32601 } else { -32602 });
            assert!(row.get("result").is_none());
        }
        assert_eq!(rows[13]["result"]["isError"], true);
        // Unknown methods have the standard error even before any handshake.
        assert_eq!(
            exchange(&[json!({"jsonrpc":"2.0","id":1,"method":"unknown"})])[0]["error"]["code"],
            -32601
        );
    }

    #[test]
    fn chatgpt_discovery_sequence() {
        let bridge = ChatBridge::new(
            std::path::PathBuf::from("unused"),
            std::sync::Arc::new(super::super::task::tests::Fake),
        );
        let requests = [
            json!({"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":"2024-11-05","clientInfo":{"name":"discovery-regression","version":"1"},"capabilities":{}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":"discover","method":"server/discover"}),
            json!({"jsonrpc":"2.0","id":0,"method":"tools/list"}),
        ];
        let input = requests
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let mut output = Vec::new();
        serve(io::Cursor::new(input), &mut output, &bridge).unwrap();
        let rows: Vec<Value> = std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 3, "notifications must not receive a response");
        assert_eq!(rows[1]["id"], "discover");
        assert!(rows[1].get("error").is_none(), "{rows:?}");
        assert_eq!(rows[1]["result"]["resultType"], "complete");
        assert_eq!(rows[1]["result"]["capabilities"], json!({"tools":{}}));
        assert_eq!(rows[2]["id"], 0);
        assert_tools(&rows[2]["result"]);
    }

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
