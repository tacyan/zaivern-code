//! Host executable trust shared by managed and manual MCP entry points.
use std::path::{Path, PathBuf};

/// Resolve installation symlinks, then reject Agent-writable executable paths.
/// Same-user concurrent replacement is outside this filesystem trust boundary.
pub(crate) fn validate_executable(path: &Path, workspace: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::MetadataExt;
    if !path.is_absolute() || !workspace.is_absolute() {
        return Err("Host executable and workspace paths must be absolute".into());
    }
    let resolved = path.canonicalize().map_err(|_| "Executable unavailable")?;
    // Cleanup must remain possible after the original workspace is removed.
    let resolved_workspace = workspace.canonicalize().ok();
    if resolved.starts_with(workspace)
        || resolved_workspace
            .as_ref()
            .is_some_and(|w| resolved.starts_with(w))
    {
        return Err("Host executables must be outside the Agent workspace".into());
    }
    let metadata =
        std::fs::symlink_metadata(&resolved).map_err(|_| "Cannot inspect host executable")?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || ![0, unsafe { libc::geteuid() }].contains(&metadata.uid())
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o111 == 0
    {
        return Err("Unsafe host executable ownership, permissions, type or links".into());
    }
    Ok(resolved)
}
