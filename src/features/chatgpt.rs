//! Managed ChatGPT Secure MCP Tunnel lifecycle (no inference API).
#[cfg(unix)]
#[path = "../chatgpt/mod.rs"]
pub mod imp;
#[cfg(unix)]
pub use imp::cli_main;

pub const HELP: &str = "\nChatGPT Secure MCP Tunnel:\n  zai chatgpt setup [--reauth]\n  zai chatgpt start [--foreground]\n  zai chatgpt status|doctor|stop|reset|repair|test\n  Runtime API Key authenticates the tunnel only; no OpenAI inference API is used.\n";

#[cfg(not(unix))]
pub fn cli_main(_args: &[String]) -> i32 {
    eprintln!(
        "ChatGPT Bridge is supported on macOS/Linux. Windows execution is not yet supported."
    );
    1
}

pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "chatgpt",
    ..crate::feature::Feature::DEFAULT
};
