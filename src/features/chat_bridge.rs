//! CLI-only MCP entry point; no additional chat UI.
#[path = "../chat_bridge/mod.rs"]
pub mod imp;
pub use imp::{cli_main, HELP};

pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "chat_bridge",
    ..crate::feature::Feature::DEFAULT
};
