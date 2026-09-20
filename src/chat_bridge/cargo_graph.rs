//! Resolve only original Cargo manifests. Never run Cargo metadata on the host,
//! and never use an Agent candidate to authorize another host read.
use super::workspace::{allowed_verification, Snapshot, VERIFICATION_LIMIT};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use toml::Value;

const PACKAGE_LIMIT: usize = 128;
pub(super) struct CargoGraph {
    pub packages: BTreeMap<PathBuf, Value>,
    pub manifests: BTreeMap<PathBuf, Vec<u8>>,
    pub dependencies: BTreeSet<PathBuf>,
}

pub(super) fn discover(snapshot: &Snapshot) -> Result<CargoGraph, String> {
    let mut graph = CargoGraph {
        packages: BTreeMap::new(),
        manifests: BTreeMap::new(),
        dependencies: BTreeSet::new(),
    };
    if !snapshot.exists(Path::new("Cargo.toml"))? {
        return Ok(graph);
    }
    let mut pending = BTreeSet::from([PathBuf::new()]);
    let mut workspace = None;
    let mut bytes = 0;
    while let Some(package) = pending.pop_first() {
        if graph.packages.contains_key(&package) {
            continue;
        }
        if graph.packages.len() >= PACKAGE_LIMIT {
            return Err("Cargo local package limit exceeded".into());
        }
        snapshot.validate_local_directory(&package)?;
        let path = package.join("Cargo.toml");
        let original = snapshot
            .read_text(&path)?
            .ok_or("Cargo manifest excluded by source safety policy")?;
        bytes += original.len();
        if bytes > VERIFICATION_LIMIT {
            return Err("Cargo manifest byte limit exceeded".into());
        }
        let manifest: Value = std::str::from_utf8(&original)
            .map_err(|_| "invalid Cargo manifest")?
            .parse()
            .map_err(|_| "invalid Cargo manifest")?;
        if package.as_os_str().is_empty() {
            workspace = manifest.get("workspace").cloned();
        } else if manifest.get("workspace").is_some() {
            return Err("nested Cargo workspace is unsupported; use its own server root".into());
        }
        if let Some(parent) = manifest.get("package").and_then(|v| v.get("workspace")) {
            let path = local_path(
                &package,
                parent.as_str().ok_or("invalid package.workspace")?,
            )?;
            if !path.as_os_str().is_empty() {
                return Err("external or nested package.workspace is unsupported".into());
            }
        }
        let mut local_dependencies = BTreeSet::new();
        dependencies(
            &manifest,
            &package,
            workspace.as_ref(),
            &mut local_dependencies,
        )?;
        if let Some(targets) = manifest.get("target").and_then(Value::as_table) {
            for target in targets.values() {
                dependencies(
                    target,
                    &package,
                    workspace.as_ref(),
                    &mut local_dependencies,
                )?;
            }
        }
        for overrides in ["patch", "replace"] {
            if let Some(table) = manifest.get(overrides).and_then(Value::as_table) {
                if overrides == "patch" {
                    for registry in table.values() {
                        dependency_table(registry, &package, None, &mut local_dependencies)?;
                    }
                } else {
                    dependency_table(
                        &Value::Table(table.clone()),
                        &package,
                        None,
                        &mut local_dependencies,
                    )?;
                }
            }
        }
        graph.dependencies.extend(
            local_dependencies
                .iter()
                .filter(|p| !p.as_os_str().is_empty())
                .cloned(),
        );
        pending.extend(local_dependencies);
        if let Some(ws) = manifest.get("workspace") {
            let excluded = member_paths(snapshot, &package, ws.get("exclude"))?;
            for member in member_paths(snapshot, &package, ws.get("members"))? {
                if !excluded.contains(&member) {
                    pending.insert(member);
                }
            }
        }
        if pending.len() > PACKAGE_LIMIT {
            return Err("Cargo local package limit exceeded".into());
        }
        graph.manifests.insert(path, original);
        graph.packages.insert(package, manifest);
    }
    Ok(graph)
}

fn dependencies(
    value: &Value,
    base: &Path,
    workspace: Option<&Value>,
    pending: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    for kind in [
        "dependencies",
        "dev-dependencies",
        "build-dependencies",
        "dev_dependencies",
        "build_dependencies",
    ] {
        if let Some(table) = value.get(kind) {
            dependency_table(table, base, workspace, pending)?;
        }
    }
    Ok(())
}

fn dependency_table(
    value: &Value,
    base: &Path,
    workspace: Option<&Value>,
    pending: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    for (name, dependency) in value.as_table().ok_or("invalid Cargo dependency table")? {
        let inherited = dependency.get("workspace").and_then(Value::as_bool) == Some(true);
        let (dependency, base) = if inherited {
            (
                workspace
                    .and_then(|w| w.get("dependencies"))
                    .and_then(|d| d.get(name))
                    .ok_or("unresolved workspace dependency")?,
                Path::new(""),
            )
        } else {
            (dependency, base)
        };
        if let Some(path) = dependency.get("path") {
            pending.insert(local_path(
                base,
                path.as_str().ok_or("invalid Cargo dependency path")?,
            )?);
        }
    }
    Ok(())
}

/// Relative normal components only. Even root-contained `..` is deliberately
/// unsupported in this MVP, matching the workspace traversal prohibition.
fn local_path(base: &Path, raw: &str) -> Result<PathBuf, String> {
    let relative = Path::new(raw);
    if raw.is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err("Cargo path must be relative without parent traversal".into());
    }
    let mut result = base.to_path_buf();
    for part in relative.components() {
        if let Component::Normal(name) = part {
            result.push(name);
        }
    }
    if !result.as_os_str().is_empty() && !allowed_verification(&result) {
        return Err("Cargo path denied by sensitive file policy".into());
    }
    Ok(result)
}

// Cargo's common `crates/*` member form, without broad recursive glob traversal.
// Unsupported pattern syntax fails explicitly rather than silently losing crates.
fn member_paths(
    snapshot: &Snapshot,
    base: &Path,
    value: Option<&Value>,
) -> Result<BTreeSet<PathBuf>, String> {
    let mut result = BTreeSet::new();
    let Some(value) = value else {
        return Ok(result);
    };
    for entry in value
        .as_array()
        .ok_or("invalid workspace members/exclude")?
    {
        let raw = entry.as_str().ok_or("invalid workspace member")?;
        if raw.contains(['?', '[', ']', '{', '}']) || raw.matches('*').count() > 1 {
            return Err("unsupported Cargo workspace member pattern".into());
        }
        let path = local_path(base, raw)?;
        if let Some((prefix, suffix)) = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.split_once('*'))
        {
            let parent = path.parent().ok_or("invalid workspace member")?;
            if parent.to_string_lossy().contains('*') {
                return Err("unsupported Cargo workspace member pattern".into());
            }
            for name in snapshot.names(parent)? {
                let Some(text) = name.to_str() else { continue };
                if text.starts_with(prefix) && text.ends_with(suffix) {
                    let member = parent.join(&name);
                    if !allowed_verification(&member) {
                        return Err("sensitive workspace member denied".into());
                    }
                    result.insert(member);
                }
            }
        } else if raw.contains('*') {
            return Err("only final-component Cargo member wildcards are supported".into());
        } else {
            result.insert(path);
        }
        if result.len() > PACKAGE_LIMIT {
            return Err("Cargo member limit exceeded".into());
        }
    }
    Ok(result)
}

/// Package build input trees, not arbitrary repository contents. Extra target
/// paths are allowed only when declared by an original manifest. Missing custom
/// build inputs fail inside the verifier; there is no host-side fallback.
pub(super) fn inputs(
    snapshot: &Snapshot,
    package: &Path,
    manifest: &Value,
    packages: &BTreeSet<PathBuf>,
) -> Result<BTreeSet<PathBuf>, String> {
    let mut paths = BTreeSet::new();
    let mut trees: BTreeSet<_> = [
        "src",
        "tests",
        "examples",
        "benches",
        "assets",
        "resources",
        "i18n",
    ]
    .into_iter()
    .map(|name| package.join(name))
    .collect();
    for kind in ["lib", "bin", "test", "bench", "example"] {
        if let Some(value) = manifest.get(kind) {
            let targets = value
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(std::slice::from_ref(value));
            for target in targets {
                if let Some(path) = target.get("path") {
                    let path =
                        local_path(package, path.as_str().ok_or("invalid Cargo target path")?)?;
                    paths.insert(path.clone());
                    let parent = path.parent().ok_or("invalid target path")?;
                    if parent != package {
                        trees.insert(parent.to_path_buf());
                    }
                }
            }
        }
    }
    if let Some(build) = manifest
        .get("package")
        .and_then(|p| p.get("build"))
        .and_then(Value::as_str)
    {
        paths.insert(local_path(package, build)?);
    }
    // Only package-root regular files (Cargo.lock, build.rs, etc.), no unrelated
    // docs/downloads/repositories. Walk each declared tree with the same safety checks.
    for name in snapshot.names(package)? {
        let path = package.join(&name);
        if allowed_verification(&path) && !packages.contains(&path) && snapshot.is_file(&path)? {
            paths.insert(path);
        }
    }
    let mut excluded = packages.clone();
    excluded.remove(package);
    for tree in trees {
        if tree
            .components()
            .any(|c| c.as_os_str().eq_ignore_ascii_case("vendor"))
            && !packages.iter().any(|p| {
                tree.starts_with(p)
                    && p.components()
                        .any(|c| c.as_os_str().eq_ignore_ascii_case("vendor"))
            })
        {
            return Err("vendor input must belong to an original Cargo dependency package".into());
        }
        if !allowed_verification(&tree) || excluded.contains(&tree) {
            continue;
        }
        if snapshot.exists(&tree)? {
            paths.extend(snapshot.walk(&tree, true, &excluded)?);
        }
        if paths.len() > 8192 {
            return Err("Cargo verification file limit exceeded".into());
        }
    }
    for path in &paths {
        if path
            .components()
            .any(|c| c.as_os_str().eq_ignore_ascii_case("vendor"))
            && !packages.iter().any(|p| {
                path.starts_with(p)
                    && p.components()
                        .any(|c| c.as_os_str().eq_ignore_ascii_case("vendor"))
            })
        {
            return Err("vendor input must belong to an original Cargo dependency package".into());
        }
    }
    Ok(paths)
}
