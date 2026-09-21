use super::private::{self, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub(super) version: u32,
    pub(super) workspace: PathBuf,
    pub(super) executable: PathBuf,
    pub(super) image: String,
    pub(super) image_source: String,
    pub(super) docker: PathBuf,
    pub(super) docker_endpoint: String,
    pub(super) tunnel_id: String,
    pub(super) secret_store: String,
    pub(super) client_version: String,
}
impl Config {
    pub(super) fn validate(&self) -> Result<()> {
        if self.version != 1
            || !super::install::SUPPORTED_VERSIONS.contains(&self.client_version.as_str())
        {
            return Err("Unsupported ChatGPT configuration/client version".into());
        }
        if !self.workspace.is_absolute()
            || !self.executable.is_absolute()
            || !self.docker.is_absolute()
        {
            return Err("ChatGPT paths must be absolute".into());
        }
        // v0.0.14 runtimeconfig.ValidateTunnelID: tunnel_<32 lowercase letters or digits>.
        if !self.tunnel_id.strip_prefix("tunnel_").is_some_and(|id| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        }) {
            return Err("Invalid Tunnel ID; copy it from OpenAI Platform Tunnels".into());
        }
        if !self
            .image
            .strip_prefix("sha256:")
            .is_some_and(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err("Agent image must resolve to an immutable Docker image ID".into());
        }
        let socket = self
            .docker_endpoint
            .strip_prefix("unix://")
            .ok_or("A local Unix Docker socket is required")?;
        if !Path::new(socket).is_absolute() {
            return Err("Docker socket must be absolute".into());
        }
        if !["keychain", "secret-service", "private-file"].contains(&self.secret_store.as_str()) {
            return Err("Unknown secret store".into());
        }
        Ok(())
    }
    pub(super) fn load(root: &Path) -> Result<Self> {
        let value: Self =
            serde_json::from_slice(&private::read(&root.join("config.json"), 64 * 1024)?)
                .map_err(|_| "Invalid ChatGPT configuration; run zai chatgpt setup")?;
        value.validate()?;
        Ok(value)
    }
    pub(super) fn save(&self, root: &Path) -> Result<()> {
        self.validate()?;
        private::write(
            &root.join("config.json"),
            &serde_json::to_vec_pretty(self).map_err(|_| "Cannot encode configuration")?,
            false,
        )
    }
}

// tunnel-client v0.0.14 uses shellwords to split into exec.Command argv; it
// does not execute a shell. Quote each argument, then JSON-encode the string.
pub(super) fn quote(value: &str) -> Result<String> {
    if value.contains(['\0', '\n', '\r']) {
        return Err("MCP command paths cannot contain NUL/newlines".into());
    }
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

pub(super) fn profile(config: &Config, root: &Path) -> Result<Vec<u8>> {
    config.validate()?;
    let command = [
        config
            .executable
            .to_str()
            .ok_or("Executable path must be UTF-8")?,
        "chatgpt",
        "__mcp",
        root.to_str().ok_or("State path must be UTF-8")?,
    ]
    .into_iter()
    .map(quote)
    .collect::<Result<Vec<_>>>()?
    .join(" ");
    // JSON is a YAML subset accepted by the client's strict yaml.v3 decoder.
    // Only the secret reference is serialized, never the resolved value.
    serde_json::to_vec_pretty(&serde_json::json!({
        "config_version":1,
        "control_plane":{"tunnel_id":config.tunnel_id,"api_key":"env:CONTROL_PLANE_API_KEY"},
        "mcp":{"commands":[{"channel":"main","command":command}],"stdio_send_initialized_notification":true},
        "health":{"listen_addr":"127.0.0.1:0","url_file":root.join("health-url")},
        "admin_ui":{"allow_remote":false,"open_browser":false},
        "log":{"level":"info","format":"json","http_raw_unsafe":false}
    })).map_err(|_| "Cannot encode tunnel profile".into())
}

pub(super) fn verify_profile(config: &Config, root: &Path) -> Result<()> {
    let actual = private::read(&root.join("profile.yaml"), 64 * 1024)?;
    if actual != profile(config, root)? {
        return Err("Managed tunnel profile changed; run zai chatgpt repair".into());
    }
    Ok(())
}
