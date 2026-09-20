//! MCP transport is independent of task storage and execution.
mod cargo_graph;
#[cfg(test)]
mod e2e_tests;
mod protocol;
#[cfg(all(test, unix))]
mod snapshot_tests;
mod target;
mod task;
mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

pub const HELP: &str = "\nMCP (ChatGPT bridge):\n  zai mcp serve --workspace ABSOLUTE_PATH --image IMAGE@sha256:DIGEST\n  See docs/chatgpt.md for isolation, agent setup and connection requirements.\n";

pub fn cli_main(args: &[String]) -> i32 {
    if args.is_empty() || args.iter().any(|v| v == "--help" || v == "-h") {
        println!("{HELP}");
        return 0;
    }
    match serve(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("MCP: {e}");
            1
        }
    }
}

fn serve(args: &[String]) -> Result<(), String> {
    if args.first().map(String::as_str) != Some("serve") {
        return Err("expected mcp serve".into());
    }
    let mut root = None;
    let mut image = None;
    for pair in args[1..].chunks(2) {
        let [key, value] = pair else {
            return Err("missing option value".into());
        };
        match key.as_str() {
            "--workspace" if root.is_none() => root = Some(PathBuf::from(value)),
            "--image" if image.is_none() => image = Some(value.clone()),
            _ => return Err("unknown or duplicate option".into()),
        }
    }
    let root = workspace::validate_root(&root.ok_or("--workspace is required")?)?;
    let target = target::LocalExecutionTarget::new(image.ok_or("--image is required")?)?;
    let bridge = task::ChatBridge::new(root, Arc::new(target));
    protocol::serve(std::io::stdin().lock(), std::io::stdout().lock(), &bridge)
        .map_err(|e| e.to_string())
}
