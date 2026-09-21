use super::{
    config::Config,
    private::{self, Result},
    process,
};
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

struct Mcp {
    child: process::OwnedChild,
    replies: std::sync::mpsc::Receiver<Vec<u8>>,
    id: u64,
}
impl Mcp {
    fn start(config: &Config) -> Result<Self> {
        let mut cmd = process::command(&config.executable);
        let mut paths = vec![config
            .docker
            .parent()
            .ok_or("Invalid Docker executable path")?
            .to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        cmd.env(
            "PATH",
            std::env::join_paths(paths).map_err(|_| "Invalid executable search path")?,
        );
        cmd.args(["mcp", "serve", "--workspace"])
            .arg(&config.workspace)
            .args(["--image", &config.image])
            .env("DOCKER_HOST", &config.docker_endpoint)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let mut child = process::OwnedChild::spawn(&mut cmd)?;
        let output = child.child.stdout.take().ok_or("Missing MCP stdout")?;
        let (tx, replies) = std::sync::mpsc::sync_channel(8);
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(output);
            loop {
                let mut line = Vec::new();
                match (&mut reader)
                    .take(256 * 1024 + 1)
                    .read_until(b'\n', &mut line)
                {
                    Ok(n) if n > 0 && n <= 256 * 1024 => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        });
        Ok(Self {
            child,
            replies,
            id: 0,
        })
    }
    fn send(&mut self, value: Value) -> Result<()> {
        let input = self.child.child.stdin.as_mut().ok_or("MCP stdin closed")?;
        writeln!(input, "{value}")
            .and_then(|_| input.flush())
            .map_err(|_| "MCP request failed".into())
    }
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.id += 1;
        self.send(json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params}))?;
        let raw = self
            .replies
            .recv_timeout(Duration::from_secs(15))
            .map_err(|_| "MCP response timeout; check Docker and workspace")?;
        let value: Value = serde_json::from_slice(&raw).map_err(|_| "Invalid MCP JSON response")?;
        if value["jsonrpc"] != "2.0" || value["id"] != self.id || value.get("error").is_some() {
            return Err("MCP response failed protocol validation".into());
        }
        value
            .get("result")
            .cloned()
            .ok_or_else(|| "MCP result missing".into())
    }
    fn discover(&mut self) -> Result<()> {
        let init = self.call("initialize",json!({"protocolVersion":"2025-11-25","clientInfo":{"name":"zaivern-doctor","version":env!("CARGO_PKG_VERSION")},"capabilities":{}}))?;
        if init["protocolVersion"] != "2025-11-25" || init["capabilities"].get("tools").is_none() {
            return Err("MCP initialize capability/version mismatch".into());
        }
        println!("[PASS] MCP initialize");
        self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))?;
        let discovery = self.call("server/discover", json!({}))?;
        if discovery["resultType"] != "complete" || discovery["capabilities"].get("tools").is_none()
        {
            return Err("MCP discovery failed".into());
        }
        println!("[PASS] server/discover");
        let tools = self.call("tools/list", json!({}))?;
        validate_tools(&tools)?;
        println!("[PASS] tools/list (exactly 3 public tools)");
        Ok(())
    }
    fn tool(&mut self, name: &str, args: Value) -> Result<Value> {
        let value = self.call("tools/call", json!({"name":name,"arguments":args}))?;
        if value["isError"] != false {
            return Err("MCP tool reported an error".into());
        }
        serde_json::from_str(
            value["content"][0]["text"]
                .as_str()
                .ok_or("Missing MCP tool content")?,
        )
        .map_err(|_| "Invalid MCP tool content".into())
    }
}
impl Drop for Mcp {
    fn drop(&mut self) {
        self.child.child.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(35);
        while self.child.exited().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

fn validate_tools(value: &Value) -> Result<()> {
    let tools = value["tools"].as_array().ok_or("Missing MCP tools list")?;
    let mut names = tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    if names
        != [
            "zaivern_cancel_task",
            "zaivern_run_task",
            "zaivern_task_status",
        ]
        || tools.len() != 3
        || tools.iter().any(|t| t["inputSchema"]["type"] != "object")
    {
        return Err("MCP public tool boundary/schema mismatch".into());
    }
    Ok(())
}

pub(super) fn discovery(config: &Config) -> Result<()> {
    Mcp::start(config)?.discover()
}

struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("zai-chatgpt-{}", private::nonce()?));
        private::directory(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn local_test(config: &Config) -> Result<()> {
    let temporary = Temporary::new()?;
    let build = temporary.0.join("fixture");
    private::directory(&build)?;
    private::write(
        &build.join("Dockerfile"),
        include_bytes!("../../tools/mcp-fixture.Dockerfile"),
        false,
    )?;
    private::write(
        &build.join("mcp-fixture.rs"),
        include_bytes!("../../tools/mcp-fixture.rs"),
        false,
    )?;
    println!("Building isolated deterministic ACP fixture (no model/API key)...");
    let mut cmd = super::docker::command(&config.docker, &config.docker_endpoint);
    cmd.args(["build", "--quiet"]).arg(&build);
    let bytes = process::capture(cmd, Duration::from_secs(600), 4 * 1024 * 1024)?;
    let image = std::str::from_utf8(&bytes)
        .map_err(|_| "Invalid fixture build output")?
        .lines()
        .last()
        .ok_or("Missing fixture image ID")?;
    let image = super::docker::image(&config.docker, &config.docker_endpoint, image)?;
    let workspace = temporary.0.join("workspace");
    private::directory(&workspace)?;
    private::directory(&workspace.join("src"))?;
    private::write(
        &workspace.join("Cargo.toml"),
        b"[package]\nname='bridge_fixture'\nversion='0.1.0'\nedition='2021'\n",
        false,
    )?;
    private::write(
        &workspace.join("Cargo.lock"),
        b"version = 4\n[[package]]\nname='bridge_fixture'\nversion='0.1.0'\n",
        false,
    )?;
    private::write(
        &workspace.join("src/lib.rs"),
        b"#[test]\nfn arithmetic() { assert_eq!(2 + 2, 5); }\n",
        false,
    )?;
    private::write(&workspace.join(".env"), b"SENTINEL=not-for-agent\n", false)?;
    let mut isolated = config.clone();
    isolated.image = image;
    isolated.workspace = workspace.clone();
    let mut mcp = Mcp::start(&isolated)?;
    mcp.discover()?;
    let task = mcp.tool(
        "zaivern_run_task",
        json!({"instruction":"Fix the failing test and show the diff"}),
    )?;
    println!("[PASS] zaivern_run_task");
    let status = wait(&mut mcp, &task, false)?;
    if status["state"] != "completed"
        || status["test_status"] != "passed"
        || status["diff_summary"].as_str().is_none_or(str::is_empty)
        || std::fs::read_to_string(workspace.join("src/lib.rs"))
            .map_err(|_| "Cannot read fixture result")?
            .contains("2 + 2, 5")
    {
        return Err("Fixture edit/test/diff verification failed".into());
    }
    println!("[PASS] zaivern_task_status\n[PASS] file edit\n[PASS] offline test\n[PASS] diff");
    let task = mcp.tool("zaivern_run_task", json!({"instruction":"WAIT_FOREVER"}))?;
    wait(&mut mcp, &task, true)?;
    mcp.tool("zaivern_cancel_task", task.clone())?;
    if wait(&mut mcp, &task, false)?["state"] != "cancelled" {
        return Err("Fixture cancel failed".into());
    }
    println!("[PASS] cancel");
    drop(mcp);
    let path = temporary.0.clone();
    drop(temporary);
    if Path::new(&path).exists() {
        return Err("Temporary workspace cleanup failed".into());
    }
    println!("[PASS] temporary workspace cleanup\nLocal E2E: PASS");
    Ok(())
}

fn wait(mcp: &mut Mcp, task: &Value, running: bool) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut reported = Instant::now();
    loop {
        let status = mcp.tool("zaivern_task_status", task.clone())?;
        let terminal = matches!(
            status["state"].as_str(),
            Some("completed" | "cancelled" | "failed")
        );
        if (running && status["progress"] == "agent executing") || (!running && terminal) {
            return Ok(status);
        }
        if terminal || Instant::now() >= deadline {
            return Err("Fixture task failed or timed out".into());
        }
        if reported.elapsed() >= Duration::from_secs(10) {
            println!("Waiting for isolated fixture...");
            reported = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
