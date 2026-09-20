//! Conservative evidence from Cargo inside the isolated verifier only.
//! dep-info includes include_str!/data and is writable by build scripts, so it
//! cannot attest that an arbitrary .rs was compiled as Rust. Only crate roots
//! reported by a successful no-run compilation without compile-time user code
//! qualify. Unknown evidence reduces coverage; it never grants a host read.
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

type Target = (String, String, Vec<String>);
pub(super) struct Coverage {
    targets: BTreeMap<Target, PathBuf>,
}

impl Coverage {
    pub(super) fn from_metadata(bytes: &[u8]) -> Option<Self> {
        let value: Value = serde_json::from_slice(bytes).ok()?;
        if value.get("version")?.as_u64()? != 1 {
            return None;
        }
        let packages = value.get("packages")?.as_array()?;
        let nodes = value.get("resolve")?.get("nodes")?.as_array()?;
        if packages.is_empty() || packages.len() > 8192 || nodes.is_empty() {
            return None;
        }
        let ids: BTreeSet<_> = packages
            .iter()
            .map(|p| p.get("id")?.as_str())
            .collect::<Option<_>>()?;
        if nodes.iter().any(|n| {
            n.get("id")
                .and_then(Value::as_str)
                .is_none_or(|id| !ids.contains(id))
        }) {
            return None;
        }
        let mut targets = BTreeMap::new();
        for package in packages {
            let id = package.get("id")?.as_str()?;
            let rows = package.get("targets")?.as_array()?;
            if rows.is_empty() {
                return None;
            }
            for target in rows {
                let kinds = strings(target.get("kind")?)?;
                let types = strings(target.get("crate_types")?)?;
                // Check all resolved dependencies, not only workspace packages.
                // Build scripts/proc macros can forge stdout/artifacts while compiling.
                if kinds.is_empty()
                    || types.is_empty()
                    || kinds.iter().any(|s| {
                        !matches!(
                            s.as_str(),
                            "lib"
                                | "rlib"
                                | "dylib"
                                | "staticlib"
                                | "cdylib"
                                | "bin"
                                | "test"
                                | "example"
                                | "bench"
                        )
                    })
                    || types.iter().any(|s| {
                        !matches!(
                            s.as_str(),
                            "lib" | "rlib" | "dylib" | "staticlib" | "cdylib" | "bin"
                        )
                    })
                {
                    return None;
                }
                let source = target.get("src_path")?.as_str()?;
                if let Some(path) = workspace_path(source) {
                    let key = (
                        id.to_string(),
                        target.get("name")?.as_str()?.to_string(),
                        kinds,
                    );
                    if targets.insert(key, path).is_some() {
                        return None;
                    }
                }
            }
        }
        Some(Self { targets })
    }

    pub(super) fn compiled_roots(self, bytes: &[u8]) -> Option<BTreeSet<PathBuf>> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut roots = BTreeSet::new();
        let mut finished = false;
        for line in text.lines() {
            let value: Value = serde_json::from_str(line).ok()?;
            if finished {
                return None;
            }
            match value.get("reason")?.as_str()? {
                "compiler-artifact" => {
                    let target = value.get("target")?;
                    let key = (
                        value.get("package_id")?.as_str()?.to_string(),
                        target.get("name")?.as_str()?.to_string(),
                        strings(target.get("kind")?)?,
                    );
                    if let Some(path) = self.targets.get(&key) {
                        if workspace_path(target.get("src_path")?.as_str()?).as_ref() != Some(path)
                        {
                            return None;
                        }
                        roots.insert(path.clone());
                    }
                }
                "compiler-message" => {}
                "build-finished" => {
                    if !value.get("success")?.as_bool()? {
                        return None;
                    }
                    finished = true;
                }
                _ => return None,
            }
        }
        finished.then_some(roots)
    }
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_string))
        .collect()
}
fn workspace_path(text: &str) -> Option<PathBuf> {
    let path = Path::new(text).strip_prefix("/workspace").ok()?;
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return None;
    }
    Some(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target() -> Value {
        json!({"name":"fixture","kind":["lib"],"crate_types":["lib"],"src_path":"/workspace/src/lib.rs"})
    }
    fn metadata() -> Value {
        json!({"version":1,"packages":[{"id":"fixture","targets":[target()]}],"resolve":{"nodes":[{"id":"fixture"}]}})
    }
    fn compilation() -> String {
        format!(
            "{}\n{}\n",
            json!({"reason":"compiler-artifact","package_id":"fixture","target":target()}),
            json!({"reason":"build-finished","success":true})
        )
    }
    #[test]
    fn evidence_is_limited_to_actual_crate_roots_and_complete_compilation() {
        let coverage =
            || Coverage::from_metadata(&serde_json::to_vec(&metadata()).unwrap()).unwrap();
        let roots = coverage().compiled_roots(compilation().as_bytes()).unwrap();
        assert_eq!(roots, BTreeSet::from([PathBuf::from("src/lib.rs")]));
        assert!(!roots.contains(Path::new("src/unused.rs")));
        assert!(!roots.contains(Path::new("src/include_str_data.rs")));
        for invalid in [
            "".into(),
            compilation().replace("true", "false"),
            compilation().replace("/workspace/src/lib.rs", "/workspace/src/unused.rs"),
            format!("{}forged", compilation()),
        ] {
            assert!(coverage().compiled_roots(invalid.as_bytes()).is_none());
        }
    }
    #[test]
    fn missing_unknown_or_compile_time_code_evidence_fails_closed() {
        for kind in ["custom-build", "proc-macro", "unknown"] {
            let mut data = metadata();
            // A transitive dependency is enough to invalidate stdout provenance.
            data["packages"].as_array_mut().unwrap().push(json!({"id":"dependency","targets":[{"name":"dependency","kind":[kind],"crate_types":["lib"],"src_path":"/cache/dependency/lib.rs"}]}));
            assert!(Coverage::from_metadata(&serde_json::to_vec(&data).unwrap()).is_none());
        }
        for data in [json!({}), json!({"version":1,"packages":[],"resolve":null})] {
            assert!(Coverage::from_metadata(&serde_json::to_vec(&data).unwrap()).is_none());
        }
        assert!(workspace_path("/workspace/../secret.rs").is_none());
        assert!(workspace_path("/outside/lib.rs").is_none());
    }
}
