//! Runs in ordinary CI: no Docker, network, model, or API credential.
use super::workspace::{Snapshot, FILE_LIMIT, SNAPSHOT_LIMIT};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

struct Fixture(PathBuf);
impl Fixture {
    fn new(tag: &str) -> Self {
        let path = crate::test_util::unique_temp_dir("bridge-snapshot", tag);
        std::fs::create_dir_all(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
    fn put(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn package(&self, prefix: &str, name: &str, extra: &str) {
        self.put(
            &format!("{prefix}Cargo.toml"),
            &format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2021'\n{extra}"),
        );
        self.put(
            &format!("{prefix}src/lib.rs"),
            "pub fn answer() -> u32 { 4 }\n",
        );
    }
    fn read(&self) -> Result<Snapshot, String> {
        Snapshot::read_for_task(&self.0, "Fix src/lib.rs")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn directory_enumeration_is_repeatable_and_large_source_selection_is_bounded() {
    let f = Fixture::new("large");
    f.package("", "large", "");
    // ~24 MiB of real Rust text, plus an unrelated large documentation tree.
    let source = format!(
        "// {}\npub fn answer() -> u32 {{ 4 }}\n",
        "x".repeat(750_000)
    );
    for index in 0..32 {
        f.put(&format!("src/module_{index:02}.rs"), &source);
    }
    for index in 0..16 {
        f.put(&format!("docs/manual_{index}.md"), &"d".repeat(750_000));
    }
    let snapshot = Snapshot::read_for_task(&f.0, "Fix src/module_31.rs").unwrap();
    assert!(snapshot.files.contains_key(Path::new("src/module_31.rs")));
    assert!(snapshot.files.contains_key(Path::new("Cargo.toml")));
    assert!(snapshot.omitted > 0);
    assert!(snapshot.files.len() <= 1024);
    assert!(snapshot.files.values().map(Vec::len).sum::<usize>() <= SNAPSHOT_LIMIT);
    let mut first = snapshot.names(Path::new("")).unwrap();
    first.sort();
    let mut second = snapshot.names(Path::new("")).unwrap();
    second.sort();
    assert_eq!(first, second);
    assert!(!first.is_empty());
    let stage = Fixture::new("large-stage");
    snapshot
        .stage_verification(&stage.0, &snapshot.files)
        .unwrap();
    assert!(stage.0.join("src/module_00.rs").exists());
    assert!(stage.0.join("src/module_31.rs").exists());
    assert!(!stage.0.join("docs/manual_0.md").exists());
}

#[test]
fn this_repository_can_initialize_a_task_snapshot() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap();
    let snapshot = Snapshot::read_for_task(&root, "Fix src/chat_bridge/task.rs").unwrap();
    assert!(snapshot
        .files
        .contains_key(Path::new("src/chat_bridge/task.rs")));
    assert!(snapshot.files.values().map(Vec::len).sum::<usize>() <= SNAPSHOT_LIMIT);
    assert!(snapshot.files.keys().all(|p| !p.starts_with("vendor")));
}

#[test]
fn local_dependency_candidate_is_verified_offline_but_never_editable() {
    for patch in [false, true] {
        let f = Fixture::new("path-dep");
        let dependency = if patch {
            "[dependencies]\nlocal_dep='0.1.0'\n[patch.crates-io]\nlocal_dep={path='vendor/local_dep'}\n"
        } else {
            "[dependencies]\nlocal_dep={path='vendor/local_dep'}\n"
        };
        f.package("", "root_fixture", dependency);
        f.package("vendor/local_dep/", "local_dep", "");
        f.put(
            "src/lib.rs",
            "#[test] fn answer() { assert_eq!(local_dep::answer(), 5); }\n",
        );
        f.put("vendor/unrelated/src/lib.rs", "unrelated sentinel");
        f.put("vendor/local_dep/.env", "TOKEN=sentinel");
        f.put("vendor/local_dep/credentials.json", "sentinel");
        f.put("vendor/local_dep/secret.pem", "sentinel");
        let snapshot = f.read().unwrap();
        assert!(snapshot.files.keys().all(|p| !p.starts_with("vendor")));
        let agent = Fixture::new("agent-stage");
        snapshot.stage(&agent.0).unwrap();
        assert!(!agent.0.join("vendor").exists());
        let mut candidate = snapshot.files.clone();
        candidate.insert(
            "src/lib.rs".into(),
            b"#[test] fn answer() { assert_eq!(local_dep::answer(), 4); }\n".to_vec(),
        );
        let verifier = Fixture::new("verifier-stage");
        snapshot
            .stage_verification(&verifier.0, &candidate)
            .unwrap();
        assert_eq!(
            std::fs::read(verifier.0.join("src/lib.rs")).unwrap(),
            candidate[Path::new("src/lib.rs")]
        );
        assert!(verifier.0.join("vendor/local_dep/src/lib.rs").exists());
        assert!(!verifier.0.join("vendor/unrelated").exists());
        for secret in [".env", "credentials.json", "secret.pem"] {
            assert!(!verifier.0.join("vendor/local_dep").join(secret).exists());
        }
        // Only our fixed fixture code is run on the host. Production always uses Docker.
        let mut command = crate::procx::hidden_command_raw(env!("CARGO"));
        command
            .args(["test", "--offline"])
            .current_dir(&verifier.0)
            .env("CARGO_HOME", verifier.0.join("cargo-home"))
            .env("CARGO_TARGET_DIR", verifier.0.join("target"))
            .env("ZAIVERN_HOME", verifier.0.join("home"))
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS");
        let mut sink = crate::features::cloud_execution::model::CollectSink::with_limit(64 * 1024);
        let result = crate::features::cloud_execution::transport::run_child(
            command,
            std::time::Duration::from_secs(30),
            "cargo",
            &mut sink,
        )
        .unwrap();
        assert!(
            result.ok(),
            "{}\n{}",
            sink.stdout_text(),
            sink.stderr_text()
        );
        assert!(sink.stdout_text().contains("1 passed"));
        // Malicious shared code can read and encode unshared inputs in the verifier.
        // The sentinel is ordinary text, deliberately not a credential pattern.
        const SENTINEL: &str = "VERIFICATION_ONLY_SENTINEL_7f10a2";
        let frozen = Fixture::new("exfiltration");
        frozen.package("", "exfiltration", dependency);
        frozen.package("vendor/local_dep/", "local_dep", "");
        frozen.put(
            "vendor/local_dep/src/lib.rs",
            &format!("// {SENTINEL}\npub fn answer() -> u32 {{ 4 }}\n"),
        );
        frozen.put(
            "src/lib.rs",
            r#"#[test] fn exfiltrate() {
            let text = std::fs::read_to_string("vendor/local_dep/src/lib.rs").unwrap();
            eprintln!("{text}");
            for byte in text.bytes() { eprint!("{byte:02x}"); }
            panic!("force verification failure");
        }"#,
        );
        let unshared = frozen.read().unwrap();
        assert!(unshared.has_verification_only());
        assert!(!unshared
            .files
            .values()
            .any(|bytes| String::from_utf8_lossy(bytes).contains(SENTINEL)));
        let stage = Fixture::new("exfiltration-stage");
        unshared
            .stage_verification(&stage.0, &unshared.files)
            .unwrap();
        let mut command = crate::procx::hidden_command_raw(env!("CARGO"));
        command
            .args(["test", "--offline"])
            .current_dir(&stage.0)
            .env("CARGO_HOME", stage.0.join("cargo-home"))
            .env("CARGO_TARGET_DIR", stage.0.join("target"))
            .env("ZAIVERN_HOME", stage.0.join("home"))
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS");
        let mut sink = crate::features::cloud_execution::model::CollectSink::with_limit(64 * 1024);
        let result = crate::features::cloud_execution::transport::run_child(
            command,
            std::time::Duration::from_secs(30),
            "cargo",
            &mut sink,
        )
        .unwrap();
        assert!(!result.ok());
        let raw = format!("{}\n{}", sink.stdout_text(), sink.stderr_text());
        let encoded: String = SENTINEL.bytes().map(|b| format!("{b:02x}")).collect();
        assert!(raw.contains(SENTINEL), "{raw}");
        assert!(raw.contains(&encoded), "{raw}");
        assert!(crate::features::cloud_execution::redact::redact(&raw).contains(SENTINEL));
        let feedback = super::target::verification_feedback(&unshared, &sink);
        let agent_visible_data = super::target::repair_prompt(&feedback);
        assert!(!agent_visible_data.contains(SENTINEL));
        assert!(!agent_visible_data.contains(&encoded));
        assert!(agent_visible_data.contains("Detailed verifier output was withheld"));
        // Without unshared inputs the same failure output remains useful.
        let shared = Fixture::new("shared-only");
        shared.package("", "shared_only", "");
        let shared = shared.read().unwrap();
        assert!(!shared.has_verification_only());
        let feedback = super::target::verification_feedback(&shared, &sink);
        assert!(feedback.contains("force verification failure"));

        // A candidate cannot acquire an unshared editable path or trigger discovery.
        candidate.insert(
            "vendor/local_dep/src/lib.rs".into(),
            b"malicious edit".to_vec(),
        );
        assert!(snapshot
            .stage_verification(&verifier.0, &candidate)
            .is_err());
        assert!(snapshot.apply(&candidate).is_err());
    }
}

#[test]
fn workspace_members_inheritance_target_and_recursive_dependencies() {
    let f = Fixture::new("workspace");
    f.put("Cargo.toml", "[workspace]\nresolver='2'\nmembers=['crates/*']\nexclude=['crates/excluded']\n[workspace.dependencies]\nshared={path='vendor/shared'}\nunused={path='vendor/not_needed'}\n");
    f.package("crates/app/", "app", "[dependencies]\nshared={workspace=true}\n[target.'cfg(unix)'.dev-dependencies]\nhelper={path='support/helper'}\n");
    f.package(
        "crates/app/support/helper/",
        "helper",
        "[build-dependencies]\nleaf={path='leaf'}\n",
    );
    f.package("crates/app/support/helper/leaf/", "leaf", "");
    f.package("vendor/shared/", "shared", "");
    f.package("crates/excluded/", "excluded", "");
    let snapshot = Snapshot::read_for_task(&f.0, "Fix crates/app/src/lib.rs").unwrap();
    assert!(snapshot
        .files
        .contains_key(Path::new("crates/app/src/lib.rs")));
    assert!(!snapshot
        .files
        .contains_key(Path::new("vendor/shared/src/lib.rs")));
    assert!(!snapshot
        .files
        .contains_key(Path::new("crates/app/support/helper/src/lib.rs")));
    let verifier = Fixture::new("workspace-stage");
    snapshot
        .stage_verification(&verifier.0, &snapshot.files)
        .unwrap();
    for path in [
        "vendor/shared/src/lib.rs",
        "crates/app/support/helper/src/lib.rs",
        "crates/app/support/helper/leaf/src/lib.rs",
    ] {
        assert!(verifier.0.join(path).exists(), "{path}");
    }
    assert!(!verifier.0.join("vendor/not_needed").exists());
}

#[test]
fn dependency_paths_fail_closed_and_candidate_manifest_never_expands_host_reads() {
    for path in [
        "../outside",
        "/tmp/outside",
        "vendor/../../outside",
        ".ssh",
        "vendor/.credentials",
        "config",
        "C:\\private",
    ] {
        let f = Fixture::new("escape");
        f.package(
            "",
            "root_fixture",
            &format!("[dependencies]\nx={{path='{path}'}}\n"),
        );
        assert!(f.read().is_err(), "{path}");
    }
    let f = Fixture::new("candidate-path");
    f.package("", "root_fixture", "");
    f.package("vendor/not_authorized/", "not_authorized", "");
    let snapshot = f.read().unwrap();
    let verifier = Fixture::new("candidate-stage");
    for path in [
        "vendor/not_authorized",
        "../../private",
        "/Users/user/secret",
    ] {
        let mut candidate = snapshot.files.clone();
        candidate.insert("Cargo.toml".into(), format!("[package]\nname='root_fixture'\nversion='0.1.0'\n[dependencies]\nx={{path='{path}'}}\n").into_bytes());
        snapshot
            .stage_verification(&verifier.0, &candidate)
            .unwrap();
        assert!(!verifier.0.join("vendor").exists());
        assert_eq!(
            std::fs::read(verifier.0.join("Cargo.toml")).unwrap(),
            candidate[Path::new("Cargo.toml")]
        );
    }
}

#[test]
fn dependency_links_and_external_modifications_cannot_be_imported() {
    let f = Fixture::new("linked-dep");
    let outside = Fixture::new("outside-dep");
    outside.package("", "dep", "");
    f.package(
        "",
        "root_fixture",
        "[dependencies]\ndep={path='vendor/dep'}\n",
    );
    std::fs::create_dir_all(f.0.join("vendor")).unwrap();
    std::os::unix::fs::symlink(&outside.0, f.0.join("vendor/dep")).unwrap();
    assert!(f.read().is_err());
    std::fs::remove_file(f.0.join("vendor/dep")).unwrap();
    f.package("vendor/dep/", "dep", "");
    std::fs::remove_file(f.0.join("vendor/dep/src/lib.rs")).unwrap();
    std::fs::hard_link(
        outside.0.join("src/lib.rs"),
        f.0.join("vendor/dep/src/lib.rs"),
    )
    .unwrap();
    assert!(f.read().is_err());
    std::fs::remove_file(f.0.join("vendor/dep/src/lib.rs")).unwrap();
    f.put("vendor/dep/src/lib.rs", "pub fn answer() -> u32 { 4 }");
    let snapshot = f.read().unwrap();
    let mut candidate = snapshot.files.clone();
    candidate.insert("src/lib.rs".into(), b"candidate edit".to_vec());
    f.put("vendor/dep/src/lib.rs", "external edit");
    assert!(snapshot.apply(&candidate).is_err());
    assert_eq!(
        std::fs::read(f.0.join("src/lib.rs")).unwrap(),
        snapshot.files[Path::new("src/lib.rs")]
    );
}

#[test]
fn unsupported_patterns_and_unregistered_vendor_targets_fail_closed() {
    for manifest in [
        "[workspace]\nmembers=['crates/**/foo']\n",
        "[package]\nname='x'\nversion='0.1.0'\nbuild='vendor/unregistered/build.rs'\n",
        "[package]\nname='x'\nversion='0.1.0'\n[[bin]]\nname='x'\npath='vendor/unregistered/main.rs'\n",
    ] {
        let f = Fixture::new("unregistered");
        f.put("Cargo.toml", manifest);
        f.put("vendor/unregistered/build.rs", "fn main() {}");
        f.put("vendor/unregistered/main.rs", "fn main() {}");
        assert!(f.read().is_err());
    }
}

#[test]
fn unsafe_or_oversized_cargo_source_never_yields_partial_verified_suite() {
    for source in [
        "// -----BEGIN PRIVATE KEY-----\nprivate fixture".to_string(),
        "x".repeat(FILE_LIMIT + 1),
    ] {
        let f = Fixture::new("unsafe-source");
        f.package("", "root_fixture", "");
        f.put("tests/required.rs", &source);
        let snapshot = f.read().unwrap();
        assert!(!snapshot.files.contains_key(Path::new("tests/required.rs")));
        let verifier = Fixture::new("unsafe-stage");
        assert!(snapshot
            .stage_verification(&verifier.0, &snapshot.files)
            .is_err());
    }
}

#[test]
fn unrelated_metadata_path_is_not_a_dependency_and_cycles_are_bounded() {
    let f = Fixture::new("metadata");
    f.package("", "root_fixture", "[package.metadata]\npath='/private/secret'\n[dependencies]\nself_ref={package='root_fixture',path='.'}\n");
    let snapshot = f.read().unwrap();
    assert_eq!(snapshot.files.len(), 2);
    let candidate = BTreeMap::new();
    let verifier = Fixture::new("missing-candidate");
    assert!(snapshot
        .stage_verification(&verifier.0, &candidate)
        .is_err());
}

#[test]
fn aggregate_byte_file_and_package_quotas_are_enforced_on_real_snapshots() {
    let f = Fixture::new("byte-quota");
    f.package("", "quota_fixture", "");
    let source = format!("//{}\n", "x".repeat(FILE_LIMIT - 4));
    for index in 0..65 {
        f.put(&format!("src/m{index}.rs"), &source);
    }
    let error = f
        .read()
        .err()
        .expect("verification bytes must stay bounded");
    assert!(error.contains("verification snapshot limit"), "{error}");

    let f = Fixture::new("file-quota");
    f.package("", "quota_fixture", "");
    for index in 0..8193 {
        f.put(&format!("src/m{index}.rs"), "// tiny\n");
    }
    let error = f
        .read()
        .err()
        .expect("verification file count must stay bounded");
    assert!(error.contains("verification file limit"), "{error}");

    let f = Fixture::new("package-quota");
    let mut deps = "[dependencies]\n".to_string();
    for index in 0..129 {
        deps.push_str(&format!("d{index}={{path='vendor/d{index}'}}\n"));
    }
    f.package("", "quota_fixture", &deps);
    let error = f.read().err().expect("dependency graph must stay bounded");
    assert!(error.contains("package limit"), "{error}");
}

#[test]
fn excluded_rust_names_cannot_silently_remove_failing_tests() {
    for path in [
        "tests/config.rs",
        "tests/id_failure.rs",
        "src/config/module.rs",
        "tests/.hidden/failing.rs",
    ] {
        let f = Fixture::new("excluded-test");
        f.package("", "root_fixture", "");
        f.put(path, "#[test] fn must_fail() { panic!(\"failure\"); }");
        let snapshot = f.read().unwrap();
        assert!(!snapshot.files.contains_key(Path::new(path)));
        let stage = Fixture::new("excluded-stage");
        assert!(
            snapshot
                .stage_verification(&stage.0, &snapshot.files)
                .is_err(),
            "{path}"
        );
        assert!(!stage.0.join(path).exists());
    }
}

#[test]
fn workspace_member_used_as_local_dependency_remains_verification_only() {
    let f = Fixture::new("member-dependency");
    f.put("Cargo.toml", "[workspace]\nresolver='2'\nmembers=['crates/app','crates/core']\n[workspace.dependencies]\ncore_local={path='crates/core'}\n");
    f.package(
        "crates/app/",
        "app",
        "[dependencies]\ncore_local={workspace=true}\n",
    );
    f.package("crates/core/", "core_local", "");
    let snapshot = Snapshot::read_for_task(&f.0, "Fix crates/core/src/lib.rs").unwrap();
    assert!(!snapshot
        .files
        .contains_key(Path::new("crates/core/src/lib.rs")));
    assert!(snapshot
        .files
        .contains_key(Path::new("crates/app/src/lib.rs")));
    let stage = Fixture::new("member-dependency-stage");
    snapshot
        .stage_verification(&stage.0, &snapshot.files)
        .unwrap();
    assert!(stage.0.join("crates/core/src/lib.rs").exists());
}
