//! Fixed, self-owned Cargo fixtures only; production never runs host Cargo.
use super::cargo_verification::Coverage;
use super::target::cargo_test_status;
use super::workspace::Snapshot;
use crate::features::cloud_execution::{model::CollectSink, transport::run_child};
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = crate::test_util::unique_temp_dir("bridge", "frozen-workspace");
        std::fs::create_dir_all(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
    fn put(&self, path: &str, bytes: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    fn cargo(&self, args: &[&str]) -> (bool, CollectSink) {
        let mut command = crate::procx::hidden_command_raw(env!("CARGO"));
        command
            .args(args)
            .current_dir(&self.0)
            .env("CARGO_HOME", self.0.join("cargo-home"))
            .env("CARGO_TARGET_DIR", self.0.join("target"))
            .env("ZAIVERN_HOME", self.0.join("home"))
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS");
        let mut sink = CollectSink::with_limit(64 * 1024);
        let result =
            run_child(command, Duration::from_secs(30), "fixture cargo", &mut sink).unwrap();
        (result.ok(), sink)
    }
    fn container_paths(&self, text: String) -> Vec<u8> {
        text.replace(self.0.to_str().unwrap(), "/workspace")
            .into_bytes()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn actual_frozen_workspace_compilation_detects_members_locks_and_unknown_coverage() {
    for case in [
        "compile_error",
        "test_error",
        "missing_lock",
        "stale_lock",
        "healthy",
        "unused",
        "mixed",
        "include_str",
        "build_script",
    ] {
        let host = Fixture::new();
        host.put("Cargo.toml", "[package]\nname='root_fixture'\nversion='0.1.0'\nedition='2021'\n[workspace]\nmembers=['crates/foo']\ndefault-members=['.']\n");
        host.put(
            "crates/foo/Cargo.toml",
            "[package]\nname='foo'\nversion='0.1.0'\nedition='2021'\n",
        );
        host.put(
            "src/lib.rs",
            "#[test] fn root() { assert_eq!(2 + 2, 4); }\n",
        );
        host.put(
            "crates/foo/src/lib.rs",
            "#[test] fn member() { assert_eq!(2 + 2, 4); }\n",
        );
        host.put("src/unused.rs", "pub fn unused() {}\n");
        host.put("frontend/app.ts", "const valid = 1;\n");
        if case != "missing_lock" {
            host.put("Cargo.lock", if case == "stale_lock" { "version = 4\n" } else {
                "version = 4\n[[package]]\nname='foo'\nversion='0.1.0'\n[[package]]\nname='root_fixture'\nversion='0.1.0'\n"
            });
        }
        if case == "include_str" {
            host.put(
                "src/lib.rs",
                "pub const DATA: &str = include_str!(\"unused.rs\");\n",
            );
        }
        if case == "build_script" {
            // Compile-time code makes generated coverage evidence untrusted.
            host.put("build.rs", "fn main() {}\n");
        }
        let snapshot = Snapshot::read_for_task(&host.0, "Fix crates/foo/src/lib.rs").unwrap();
        let mut candidate = snapshot.files.clone();
        let member = PathBuf::from("crates/foo/src/lib.rs");
        match case {
            "compile_error" => {
                candidate.insert(member, b"this is invalid Rust;\n".to_vec());
            }
            "test_error" => {
                candidate.insert(
                    member,
                    b"#[test] fn member() { panic!(\"member failure\"); }\n".to_vec(),
                );
            }
            "unused" | "include_str" => {
                candidate.insert("src/unused.rs".into(), b"not valid Rust at all\n".to_vec());
            }
            _ => {
                candidate
                    .get_mut(Path::new("src/lib.rs"))
                    .unwrap()
                    .extend_from_slice(b"// candidate edit\n");
                candidate
                    .get_mut(&member)
                    .unwrap()
                    .extend_from_slice(b"// member edit\n");
            }
        }
        if case == "mixed" {
            candidate.insert("frontend/app.ts".into(), b"const invalid = ;\n".to_vec());
        }
        let verifier = Fixture::new();
        snapshot
            .stage_verification(&verifier.0, &candidate)
            .unwrap();
        let original_lock = std::fs::read(verifier.0.join("Cargo.lock")).ok();
        let (metadata_ok, metadata) =
            verifier.cargo(&["metadata", "--format-version=1", "--frozen"]);
        let coverage = metadata_ok
            .then(|| Coverage::from_metadata(&verifier.container_paths(metadata.stdout_text())))
            .flatten();
        let (compiled, compilation) = verifier.cargo(&[
            "test",
            "--workspace",
            "--frozen",
            "--no-run",
            "--message-format=json",
        ]);
        let roots = coverage.and_then(|c| {
            compiled
                .then(|| c.compiled_roots(&verifier.container_paths(compilation.stdout_text())))
                .flatten()
        });
        let (passed, output) = verifier.cargo(&["test", "--workspace", "--frozen"]);
        let expected = !matches!(
            case,
            "compile_error" | "test_error" | "missing_lock" | "stale_lock"
        );
        assert_eq!(
            passed,
            expected,
            "{case}: {}\n{}",
            output.stdout_text(),
            output.stderr_text()
        );
        assert_eq!(
            std::fs::read(verifier.0.join("Cargo.lock")).ok(),
            original_lock,
            "{case}"
        );
        if passed {
            let status = cargo_test_status(&snapshot.files, &candidate, roots.as_ref());
            assert_eq!(
                status,
                if case == "healthy" {
                    "passed"
                } else {
                    "not_verified"
                },
                "{case}"
            );
        }
        // All failed candidates are still isolated. Full task/import assertions
        // for these cases also run in the real Docker MCP E2E on Ubuntu CI.
        for (path, bytes) in &snapshot.files {
            assert_eq!(
                &std::fs::read(host.0.join(path)).unwrap(),
                bytes,
                "{case}: {path:?}"
            );
        }
    }
}
