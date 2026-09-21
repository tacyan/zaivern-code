use super::private::Result;
use super::process;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) fn detect() -> Result<(PathBuf, String)> {
    let binary = crate::shellenv::which("docker")
        .ok_or("Docker CLI missing; install/start Docker, then retry setup")?
        .canonicalize()
        .map_err(|_| "Cannot resolve Docker executable")?;
    let endpoint = if let Ok(host) = std::env::var("DOCKER_HOST") {
        host
    } else {
        let mut cmd = process::command(&binary);
        cmd.args(["context", "inspect"]);
        let data = process::capture(cmd, Duration::from_secs(10), 65536).map_err(|_| {
            "Docker context lookup failed or timed out; check docker context ls, then retry setup"
        })?;
        let value: serde_json::Value =
            serde_json::from_slice(&data).map_err(|_| "Invalid Docker context")?;
        value[0]["Endpoints"]["docker"]["Host"]
            .as_str()
            .ok_or("Docker context has no endpoint")?
            .to_owned()
    };
    validate_endpoint(&endpoint)?;
    let mut cmd = command(&binary, &endpoint);
    cmd.args(["info", "--format", "{{.OSType}}"]);
    if process::capture(cmd, Duration::from_secs(15), 1024).map_err(|_| {
        "Docker daemon unavailable or timed out; start Docker and retry zai chatgpt doctor"
    })? != b"linux\n"
    {
        return Err("Docker must run Linux containers".into());
    }
    Ok((binary, endpoint))
}

pub(super) fn validate_endpoint(endpoint: &str) -> Result<()> {
    let socket = endpoint
        .strip_prefix("unix://")
        .filter(|p| Path::new(p).is_absolute())
        .ok_or(
            "Only local Unix Docker sockets are supported; remote Docker contexts are rejected",
        )?;
    if !std::fs::metadata(socket).is_ok_and(|m| m.file_type().is_socket()) {
        return Err("Local Docker socket unavailable; start Docker and retry".into());
    }
    Ok(())
}

pub(super) fn command(binary: &Path, endpoint: &str) -> std::process::Command {
    let mut cmd = process::command(binary);
    cmd.env_remove("DOCKER_HOST")
        .env_remove("DOCKER_CONTEXT")
        .args(["--host", endpoint]);
    cmd
}

pub(super) fn image(binary: &Path, endpoint: &str, source: &str) -> Result<String> {
    if source.is_empty()
        || source.starts_with('-')
        || source.len() > 512
        || source.chars().any(char::is_whitespace)
    {
        return Err("Invalid Docker image name".into());
    }
    let mut cmd = command(binary, endpoint);
    cmd.args(["image", "inspect", source]);
    let data = process::capture(cmd, Duration::from_secs(15), 256 * 1024)?;
    let value: serde_json::Value =
        serde_json::from_slice(&data).map_err(|_| "Invalid Docker image metadata")?;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        v => v,
    };
    if value[0]["Os"] != "linux" || value[0]["Architecture"] != arch {
        return Err(
            "Agent image architecture mismatch; build/pull the native Linux architecture".into(),
        );
    }
    let id = value[0]["Id"]
        .as_str()
        .ok_or("Missing immutable image ID")?;
    if !id
        .strip_prefix("sha256:")
        .is_some_and(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("Docker did not return an immutable image ID".into());
    }
    Ok(id.to_owned())
}

pub(super) fn pull(binary: &Path, endpoint: &str, source: &str) -> Result<String> {
    if source.is_empty() || source.starts_with('-') || source.chars().any(char::is_whitespace) {
        return Err("Invalid Docker image name".into());
    }
    let mut cmd = command(binary, endpoint);
    cmd.args(["pull", source]);
    process::capture(cmd, Duration::from_secs(600), 4 * 1024 * 1024)?;
    image(binary, endpoint, source)
}
