//! Local Docker isolation around the existing ACP client. No bind mounts,
//! credentials, Docker socket or host filesystem capabilities reach the agent.
use super::task::{Control, Outcome, State, TaskExecutor};
use super::workspace::{Snapshot, FILE_LIMIT, SNAPSHOT_LIMIT};
use crate::acp::{AcpClient, Phase};
use crate::agents::approvals::ApprovalQueue;
use crate::features::cloud_execution::{
    model::{ids, CollectSink},
    transport::{run_child, run_child_with_stdin},
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(super) struct LocalExecutionTarget {
    image: String,
    docker: PathBuf,
    endpoint: String,
}
impl LocalExecutionTarget {
    pub fn new(image: String) -> Result<Self, String> {
        if !valid_image(&image) {
            return Err("image must be an immutable IMAGE@sha256:DIGEST reference".into());
        }
        #[cfg(not(unix))]
        return Err("MCP execution is not yet supported on this OS".into());
        #[cfg(unix)]
        {
            let docker = crate::shellenv::which("docker").ok_or("Docker CLI is required")?;
            let endpoint = if let Ok(host) = std::env::var("DOCKER_HOST") {
                host
            } else {
                let mut command = crate::procx::hidden_command_raw(&docker);
                command.args(["context", "inspect"]);
                let mut sink = CollectSink::with_limit(64 * 1024);
                let status = run_child(command, Duration::from_secs(10), "docker", &mut sink)
                    .map_err(|_| "cannot inspect Docker context")?;
                if !status.ok() || sink.truncated {
                    return Err("cannot inspect Docker context".into());
                }
                let value: serde_json::Value =
                    serde_json::from_slice(&sink.stdout).map_err(|_| "invalid Docker context")?;
                value[0]["Endpoints"]["docker"]["Host"]
                    .as_str()
                    .ok_or("Docker context has no endpoint")?
                    .to_string()
            };
            let socket = endpoint
                .strip_prefix("unix://")
                .filter(|path| Path::new(path).is_absolute())
                .ok_or("MCP requires a local Unix Docker socket; remote contexts are rejected")?;
            use std::os::unix::fs::FileTypeExt;
            if !std::fs::metadata(socket).is_ok_and(|m| m.file_type().is_socket()) {
                return Err("local Docker socket is unavailable".into());
            }
            Ok(Self {
                image,
                docker,
                endpoint,
            })
        }
    }
    fn command(&self, args: &[String]) -> std::process::Command {
        let mut command = crate::procx::hidden_command_raw(&self.docker);
        command
            .env_remove("DOCKER_HOST")
            .env_remove("DOCKER_CONTEXT")
            .args(["--host", &self.endpoint])
            .args(args);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        command
    }
    fn run(&self, args: &[String], timeout: Duration) -> Result<Vec<u8>, String> {
        let mut sink = CollectSink::with_limit(2 * FILE_LIMIT);
        let result = run_child(self.command(args), timeout, "docker", &mut sink)
            .map_err(|_| "Docker operation unavailable or timed out")?;
        if !result.ok() || sink.truncated {
            return Err("Docker operation failed or exceeded output limit".into());
        }
        Ok(sink.stdout)
    }

    fn upload(
        &self,
        container: &Container<'_>,
        files: &Path,
        started: Instant,
    ) -> Result<(), String> {
        let staging = Staging::new()?;
        let archive = staging.0.join("snapshot.tar");
        let tar = crate::shellenv::which("tar").ok_or("tar is required for snapshot transfer")?;
        let mut command = crate::procx::hidden_command_raw(&tar);
        command
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(files)
            .arg(".");
        // A private directory populated only from validated regular text files.
        let mut sink = CollectSink::with_limit(64 * 1024);
        let result = run_child(command, budget(started, 30)?, "tar", &mut sink)
            .map_err(|_| "cannot prepare snapshot archive")?;
        if !result.ok() {
            return Err("cannot prepare snapshot archive".into());
        }
        let input = std::fs::File::open(archive).map_err(|_| "cannot open snapshot archive")?;
        let command = self.command(&strings(&[
            "exec",
            "--interactive",
            &container.id,
            "tar",
            "--no-same-owner",
            "-xf",
            "-",
            "-C",
            "/workspace",
        ]));
        let result = run_child_with_stdin(
            command,
            budget(started, 30)?,
            "docker",
            &mut sink,
            input.into(),
        )
        .map_err(|_| "cannot upload snapshot")?;
        if !result.ok() {
            return Err("cannot upload snapshot".into());
        }
        Ok(())
    }

    fn start_container(&self, started: Instant, verifier: bool) -> Result<Container<'_>, String> {
        let volume = Volume::new(self, started)?;
        let mount = format!(
            "type=volume,source={},target=/workspace,volume-nocopy",
            volume.name
        );
        let container = Container {
            target: self,
            id: ids::new_id("zaivern-mcp-"),
            removed: std::cell::Cell::new(false),
            volume,
        };
        let mut args = strings(&[
            "create",
            "--name",
            &container.id,
            "--pull=never",
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--pids-limit=128",
            "--memory=4g",
            "--cpus=2",
            "--mount",
            &mount,
            "--tmpfs=/tmp:rw,nosuid,nodev,size=64m",
            "--env=HOME=/tmp/agent-home",
            "--workdir=/workspace",
        ]);
        if verifier {
            args.push("--entrypoint=sleep".into());
        }
        args.push(self.image.clone());
        if verifier {
            args.push("1800".into());
        }
        // Track our unique name before create: the daemon can create a container
        // even if its response is lost or the CLI times out. Drop still removes it.
        let launch = self
            .run(&args, budget(started, 30)?)
            .and_then(|_| self.run(&strings(&["start", &container.id]), budget(started, 30)?));
        if let Err(error) = launch {
            return Err(match container.shutdown() {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; {cleanup}"),
            });
        }
        Ok(container)
    }

    fn collect(
        &self,
        container: &Container<'_>,
        snapshot: &Snapshot,
        control: &Control,
        started: Instant,
    ) -> Result<BTreeMap<PathBuf, Vec<u8>>, String> {
        let mut changes = BTreeMap::new();
        let mut total = 0;
        for path in snapshot.files.keys() {
            if control.is_cancelled() {
                return Err("cancelled".into());
            }
            let remote = format!("{}:/workspace/{}", container.id, path.to_string_lossy());
            let archive = self.run(&strings(&["cp", &remote, "-"]), budget(started, 30)?)?;
            let bytes = regular_tar_payload(&archive)?;
            total += bytes.len();
            if total > SNAPSHOT_LIMIT {
                return Err("candidate snapshot limit exceeded".into());
            }
            changes.insert(path.clone(), bytes);
        }
        Ok(changes)
    }

    fn verify(
        &self,
        snapshot: &Snapshot,
        changes: &BTreeMap<PathBuf, Vec<u8>>,
        started: Instant,
    ) -> Result<(bool, String, Option<String>), String> {
        let staging = Staging::new()?;
        if let Err(error) = snapshot.stage_verification(&staging.0, changes) {
            return Ok((false, error, None));
        }
        // Overlay exactly the import candidate on frozen original Cargo inputs.
        // Never discover host dependencies from Agent-generated manifests/files.
        let verifier = self.start_container(started, true)?;
        let result: Result<(bool, String), String> = (|| {
            self.upload(&verifier, &staging.0, started)?;
            let mut sink = CollectSink::with_limit(64 * 1024);
            let command = self.command(&strings(&[
                "exec",
                &verifier.id,
                "cargo",
                "test",
                "--offline",
            ]));
            let result = run_child(command, budget(started, 60)?, "docker", &mut sink);
            let passed = result.as_ref().is_ok_and(|r| r.ok());
            let output = if sink.truncated {
                "Test output exceeded response limit; details omitted.".into()
            } else {
                crate::features::cloud_execution::redact::redact(&format!(
                    "{}\n{}",
                    sink.stdout_text(),
                    sink.stderr_text()
                ))
            };
            // Drop removes the entire verifier even on timeout, killing its tests.
            Ok((passed, output))
        })();
        let cleanup = verifier.shutdown();
        match (result, cleanup) {
            (Ok((passed, output)), cleanup) => Ok((passed, output, cleanup.err())),
            (Err(cause), Err(cleanup)) => Err(format!("{cause}; {cleanup}")),
            (Err(cause), Ok(())) => Err(cause),
        }
    }
}

fn budget(started: Instant, seconds: u64) -> Result<Duration, String> {
    Duration::from_secs(1800)
        .checked_sub(started.elapsed())
        .filter(|left| !left.is_zero())
        .map(|left| left.min(Duration::from_secs(seconds)))
        .ok_or_else(|| "task exceeded 30 minute limit".into())
}
fn valid_image(image: &str) -> bool {
    if let Some(digest) = image.strip_prefix("sha256:") {
        return digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit());
    }
    let Some((name, digest)) = image.split_once("@sha256:") else {
        return false;
    };
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"./:_-".contains(&b))
        && digest.len() == 64
        && digest.bytes().all(|b| b.is_ascii_hexdigit())
}
fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

struct Container<'a> {
    target: &'a LocalExecutionTarget,
    id: String,
    removed: std::cell::Cell<bool>,
    volume: Volume<'a>,
}
impl Container<'_> {
    fn shutdown(&self) -> Result<(), String> {
        if !self.removed.get() {
            self.target
                .run(
                    &strings(&["rm", "--force", &self.id]),
                    Duration::from_secs(20),
                )
                .map_err(|_| {
                    format!(
                        "container cleanup unconfirmed: {}; inspect Docker locally",
                        self.id
                    )
                })?;
            self.removed.set(true);
        }
        self.volume.shutdown()
    }
}
impl Drop for Container<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("mcp {error}");
        }
    }
}
struct Volume<'a> {
    target: &'a LocalExecutionTarget,
    name: String,
    removed: std::cell::Cell<bool>,
}
impl<'a> Volume<'a> {
    fn new(target: &'a LocalExecutionTarget, started: Instant) -> Result<Self, String> {
        let volume = Self {
            target,
            name: ids::new_id("zaivern-mcp-"),
            removed: std::cell::Cell::new(false),
        };
        target.run(
            &strings(&[
                "volume",
                "create",
                "--driver",
                "local",
                "--opt",
                "type=tmpfs",
                "--opt",
                "device=tmpfs",
                "--opt",
                "o=size=256m,exec,nosuid,nodev",
                &volume.name,
            ]),
            budget(started, 30)?,
        )?;
        Ok(volume)
    }
    fn shutdown(&self) -> Result<(), String> {
        if self.removed.get() {
            return Ok(());
        }
        self.target
            .run(
                &strings(&["volume", "rm", &self.name]),
                Duration::from_secs(20),
            )
            .map_err(|_| {
                format!(
                    "workspace volume cleanup unconfirmed: {}; inspect Docker locally",
                    self.name
                )
            })?;
        self.removed.set(true);
        Ok(())
    }
}
impl Drop for Volume<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("mcp {error}");
        }
    }
}

struct Staging(PathBuf);
impl Staging {
    fn new() -> Result<Self, String> {
        let path = std::env::temp_dir().join(ids::new_id("zaivern-mcp-"));
        std::fs::create_dir(&path).map_err(|_| "cannot create task staging directory")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| "cannot protect staging directory")?;
        }
        Ok(Self(path))
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl TaskExecutor for LocalExecutionTarget {
    fn execute(
        &self,
        root: &Path,
        instruction: &str,
        control: &Control,
    ) -> Result<Outcome, String> {
        let started = Instant::now();
        let snapshot = Snapshot::read_for_task(root, instruction)?;
        if control.is_cancelled() {
            return Err("cancelled".into());
        }
        let staging = Staging::new()?;
        let files = staging.0.join("source");
        std::fs::create_dir(&files).map_err(|_| "cannot create source staging directory")?;
        snapshot.stage(&files)?;
        // The image must already exist. Pulling, provisioning and forwarding
        // host auth/environment are deliberately outside task execution.
        let container = self.start_container(started, false)?;
        let result = (|| {
            self.upload(&container, &files, started)?;
            // Reuse the existing agent catalog; no dynamically supplied command.
            let entry = crate::agents::ACP_CATALOG
                .iter()
                .find(|entry| entry.id == "qwen-code")
                .ok_or("ACP agent catalog entry missing")?;
            let mut args = strings(&["exec", "--interactive", &container.id, entry.local_bin]);
            args.extend(entry.local_args.iter().map(|arg| arg.to_string()));
            let mut agent = AcpClient::start_command(
                entry,
                1,
                PathBuf::from("/workspace"),
                self.command(&args),
                snapshot.files.keys().cloned(),
            )
            .map_err(|_| "cannot start isolated ACP agent")?;
            let mut approvals = ApprovalQueue::in_dir(staging.0.join("audit"));
            let mut sent = false;
            let mut attempts = 0;
            let mut test_status = "not_verified".to_string();
            let build_status = "not_verified".to_string();
            let changes = loop {
                if control.is_cancelled() {
                    agent.cancel(&mut approvals);
                    agent.stop();
                    return Err("cancelled".into());
                }
                if started.elapsed() > Duration::from_secs(1800) {
                    return Err("task exceeded 30 minute limit".into());
                }
                agent.pump(&mut approvals);
                if !agent.pending_permission_ids().is_empty() {
                    control.state(
                        State::WaitingApproval,
                        "operation needs approval; cancel to stop",
                    );
                }
                match &agent.phase {
                    Phase::Idle if !sent => {
                        let shared_paths = snapshot
                            .files
                            .keys()
                            .map(|p| p.to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("\n");
                        let prompt = format!("{instruction}\n\nWork only in /workspace. Only the selected text files are present; {} eligible files were omitted. Do not assume this is the complete repository. Host Git metadata and credentials are unavailable. Shell, deletion and network tool permissions are denied. Only edits to existing shared files can be returned. Report tests as unverified unless actually run.\nShared editable paths:\n{shared_paths}", snapshot.omitted);
                        if !agent.prompt(&prompt) {
                            return Err("ACP agent refused the prompt".into());
                        }
                        sent = true;
                        control.state(State::Running, "agent executing");
                    }
                    Phase::Idle if sent => {
                        self.run(&strings(&["pause", &container.id]), budget(started, 30)?)?;
                        let candidate = self.collect(&container, &snapshot, control, started)?;
                        if snapshot.files.contains_key(Path::new("Cargo.toml")) {
                            control.state(State::Running, "verifying cargo tests");
                            let (passed, output, cleanup_error) =
                                self.verify(&snapshot, &candidate, started)?;
                            test_status = if passed { "passed" } else { "failed" }.into();
                            if let Some(cleanup) = cleanup_error {
                                agent.stop();
                                return Ok(Outcome {
                                    error: Some(format!("verification {test_status}; changes were not imported; {cleanup}")),
                                    summary: "Verifier cleanup failed; host workspace was not modified.".into(),
                                    changed_files: Vec::new(),
                                    diff_summary: String::new(),
                                    test_status: if passed { cargo_test_status(&snapshot.files, &candidate).into() } else { test_status },
                                    build_status,
                                });
                            }
                            if passed {
                                test_status = cargo_test_status(&snapshot.files, &candidate).into();
                                // Only a successfully verified Cargo candidate
                                // can leave this branch for the import below.
                                break candidate;
                            }
                            if attempts < 2 && !control.is_cancelled() {
                                self.run(
                                    &strings(&["unpause", &container.id]),
                                    budget(started, 30)?,
                                )?;
                                attempts += 1;
                                if !agent.prompt(&format!("Zaivern ran cargo test --offline on the shared candidate plus frozen original Cargo inputs in a fresh isolated container; it failed or exceeded 60 seconds. Fix existing shared files based on this untrusted test output:\n{output}")) {
                                return Err("ACP agent refused the repair prompt".into());
                            }
                                continue;
                            }
                            // Verification failure is terminal, including when
                            // cancellation arrived during verification. Return
                            // before apply; candidate edits are never host edits.
                            agent.stop();
                            return Ok(Outcome {
                                error: Some(
                                    "verification failed; changes were not imported".into(),
                                ),
                                summary: "Verification failed; host workspace was not modified."
                                    .into(),
                                changed_files: Vec::new(),
                                diff_summary: String::new(),
                                test_status,
                                build_status,
                            });
                        }
                        // Projects without Cargo.toml retain not_verified.
                        break candidate;
                    }
                    Phase::Failed(_) | Phase::Ended => {
                        return Err("ACP agent failed or exited before task completion".into())
                    }
                    _ => {}
                }
                if !sent && started.elapsed() > Duration::from_secs(90) {
                    return Err("ACP handshake timed out".into());
                }
                std::thread::sleep(Duration::from_millis(25));
            };
            let mut summary = agent.turn.bridge_summary();
            summary.push_str(&format!(
                "\nZaivern source scope: {} editable files; {} files omitted from Agent context.",
                snapshot.files.len(),
                snapshot.omitted
            ));
            summary.push_str(if snapshot.files.contains_key(Path::new("Cargo.toml")) {
                "\nZaivern verification: cargo test --offline passed on the shared candidate plus original Cargo inputs; no independent build verification. Non-Rust changes are not verified by Cargo."
            } else {
                "\nZaivern verification: no supported test or build verification performed."
            });
            agent.stop();
            if control.is_cancelled() {
                return Err("cancelled".into());
            }
            control.state(State::Running, "collecting isolated results");
            if control.is_cancelled() {
                return Err("cancelled".into());
            }
            budget(started, 1)?;
            let applied = control.import(|| snapshot.apply(&changes))?;
            let changed_files = applied.changed;
            let diff_summary = if applied.error.is_some() {
                "Import partially failed; inspect changed_files locally for actual bytes.".into()
            } else {
                diff_summary(&changed_files, &snapshot.files, &changes)
            };
            Ok(Outcome {
                error: applied.error,
                summary,
                changed_files,
                diff_summary,
                test_status,
                build_status,
            })
        })();
        match (result, container.shutdown()) {
            (Ok(mut outcome), Err(error)) => {
                outcome.error = Some(match outcome.error {
                    Some(cause) => format!("{cause}; {error}"),
                    None => error,
                });
                Ok(outcome)
            }
            (Err(cause), Err(error)) => Err(format!("{cause}; {error}")),
            (result, Ok(())) => result,
        }
    }
}

/// Cargo tests only describe the Rust test suite in the shared snapshot, not
/// every language or an independent build. Conservatively leave mixed edits
/// unverified (including manifests and data, whose coverage is unknown).
fn cargo_test_status(
    before: &BTreeMap<PathBuf, Vec<u8>>,
    after: &BTreeMap<PathBuf, Vec<u8>>,
) -> &'static str {
    if after.iter().any(|(path, bytes)| {
        before.get(path) != Some(bytes) && path.extension().is_none_or(|ext| ext != "rs")
    }) {
        "not_verified"
    } else {
        "passed"
    }
}

/// One replacement hunk per file, with three context lines. Linear in snapshot
/// size, without executing Git configuration, external diff or textconv.
/// Large replacement spans are explicitly omitted rather than returning full files.
fn diff_summary(
    paths: &[String],
    before: &BTreeMap<PathBuf, Vec<u8>>,
    after: &BTreeMap<PathBuf, Vec<u8>>,
) -> String {
    use crate::features::cloud_execution::redact::redact;
    let mut diff = String::new();
    for path in paths {
        let key = Path::new(path);
        // Redact complete documents before slicing, so a PEM block cannot lose
        // its BEGIN marker at a context boundary. Redact the final response too.
        let old = redact(&String::from_utf8_lossy(&before[key]));
        let new = redact(&String::from_utf8_lossy(&after[key]));
        let old: Vec<_> = old.split_inclusive('\n').collect();
        let new: Vec<_> = new.split_inclusive('\n').collect();
        let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        diff.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
        if prefix == old.len() && prefix == new.len() {
            diff.push_str("[Change hidden by redaction]\n");
            continue;
        }
        let start = prefix.saturating_sub(3);
        let old_end = old.len() - suffix;
        let new_end = new.len() - suffix;
        let context = suffix.min(3);
        if old_end - start + new_end - start + 2 * context > 200 {
            diff.push_str("[Replacement span exceeds 200 lines; inspect this file locally]\n");
            continue;
        }
        diff.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            if old.is_empty() { 0 } else { start + 1 },
            old_end + context - start,
            if new.is_empty() { 0 } else { start + 1 },
            new_end + context - start
        ));
        for (mark, lines) in [
            (' ', &old[start..prefix]),
            ('-', &old[prefix..old_end]),
            ('+', &new[prefix..new_end]),
            (' ', &old[old_end..old_end + context]),
        ] {
            for line in lines {
                diff.push(mark);
                diff.push_str(line);
                if !line.ends_with('\n') {
                    diff.push_str("\n\\ No newline at end of file\n");
                }
            }
        }
    }
    let diff = redact(&diff);
    if diff.len() <= 32 * 1024 {
        diff
    } else {
        "Diff exceeds response limit; inspect changed_files locally.".into()
    }
}

fn regular_tar_payload(tar: &[u8]) -> Result<Vec<u8>, String> {
    let header = tar.get(..512).ok_or("truncated output archive")?;
    if !matches!(header[156], 0 | b'0') {
        return Err("non-regular output file rejected".into());
    }
    let size_text = std::str::from_utf8(&header[124..136])
        .map_err(|_| "invalid output size")?
        .trim_matches(['\0', ' ']);
    let size = usize::from_str_radix(size_text, 8).map_err(|_| "invalid output size")?;
    if size > FILE_LIMIT {
        return Err("output file exceeds size limit".into());
    }
    let content = tar.get(512..512 + size).ok_or("truncated output file")?;
    if std::str::from_utf8(content).is_err() || content.contains(&0) {
        return Err("output is not text".into());
    }
    Ok(content.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn create_response_failure_still_cleans_owned_name_and_reports_failure() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::test_util::unique_temp_dir("bridge", "create-cleanup");
        std::fs::create_dir_all(&root).unwrap();
        let docker = root.join("docker-fixture");
        std::fs::write(
            &docker,
            r#"#!/bin/sh
shift 2
case "$1" in
volume) exit 0 ;;
create) printf '%s' "$3" > "$0.created"; exit 17 ;;
rm) printf '%s' "$3" > "$0.removed"; exit 19 ;;
esac
exit 1
"#,
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = LocalExecutionTarget {
            image: "fixture".into(),
            docker: docker.clone(),
            endpoint: "fixture".into(),
        };
        let error = target
            .start_container(Instant::now(), false)
            .err()
            .expect("create must fail");
        let created = std::fs::read_to_string(docker.with_extension("created")).unwrap();
        let removed = std::fs::read_to_string(docker.with_extension("removed")).unwrap();
        assert_eq!(created, removed);
        assert!(created.starts_with("zaivern-mcp-"));
        assert!(error.contains("Docker operation failed"), "{error}");
        assert!(error.contains("container cleanup unconfirmed"), "{error}");
        assert!(error.contains(&created), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cargo_success_does_not_verify_mixed_changes() {
        let before = BTreeMap::from([
            (PathBuf::from("src/lib.rs"), b"before".to_vec()),
            (PathBuf::from("frontend/app.ts"), b"valid".to_vec()),
            (PathBuf::from("Cargo.toml"), b"manifest".to_vec()),
        ]);
        let mut candidate = before.clone();
        candidate.insert(PathBuf::from("src/lib.rs"), b"after".to_vec());
        assert_eq!(cargo_test_status(&before, &candidate), "passed");
        for path in ["frontend/app.ts", "Cargo.toml", "data.json"] {
            let mut mixed = candidate.clone();
            mixed.insert(PathBuf::from(path), b"invalid syntax".to_vec());
            assert_eq!(cargo_test_status(&before, &mixed), "not_verified");
        }
    }
    fn summarize(before: &str, after: &str) -> String {
        diff_summary(
            &["src/lib.rs".into()],
            &BTreeMap::from([(PathBuf::from("src/lib.rs"), before.as_bytes().to_vec())]),
            &BTreeMap::from([(PathBuf::from("src/lib.rs"), after.as_bytes().to_vec())]),
        )
    }
    #[test]
    fn diff_redacts_unchanged_context_and_keeps_the_edit() {
        let before = "const API_KEY: &str = \"fixture-rust-secret\";\n// token = fixture-spaced-secret\n// {\"token\":\"fixture-json-secret\"}\nfn answer() { wrong(); }\n// api_key=fixture-private-api-value\n// token=fixture-private-token-value\n";
        let diff = summarize(before, &before.replace("wrong()", "correct()"));
        assert!(!diff.contains("fixture-private-api-value"), "{diff}");
        assert!(!diff.contains("fixture-private-token-value"), "{diff}");
        assert!(!diff.contains("fixture-rust-secret"), "{diff}");
        assert!(!diff.contains("fixture-spaced-secret"), "{diff}");
        assert!(!diff.contains("fixture-json-secret"), "{diff}");
        assert!(diff.contains("api_key=***"), "{diff}");
        assert!(diff.contains("-fn answer() { wrong(); }"), "{diff}");
        assert!(diff.contains("+fn answer() { correct(); }"), "{diff}");
    }
    #[test]
    fn diff_redacts_authorization_context() {
        for header in [
            "// AUTHORIZATION: basic fixture-auth-secret",
            r#"// {"Authorization":"Basic fixture-auth-secret"}"#,
            "// bearer fixture-auth-secret",
        ] {
            let before = format!("{header}\nfn answer() {{ wrong(); }}\n");
            let diff = summarize(&before, &before.replace("wrong()", "correct()"));
            assert!(!diff.contains("fixture-auth-secret"), "{diff}");
            assert!(diff.contains("+fn answer() { correct(); }"), "{diff}");
        }
    }

    #[test]
    fn diff_redacts_unchanged_camel_case_context() {
        for name in [
            "accessToken",
            "refreshToken",
            "clientSecret",
            "apiKey",
            "password",
        ] {
            let before = format!("const {name} = \"fixture-secret-value\";\nold();\n");
            let diff = summarize(&before, &before.replace("old();", "new();"));
            assert!(!diff.contains("fixture-secret-value"), "{diff}");
            assert!(diff.contains("***"), "{diff}");
            assert!(diff.contains("-old();\n+new();\n"), "{diff}");
        }
    }
    #[test]
    fn diff_bounds_context_and_redacts_before_response_limit() {
        let before = format!(
            "{}old\n{}",
            "unrelated\n".repeat(1000),
            "tail\n".repeat(1000)
        );
        let diff = summarize(&before, &before.replace("old\n", "new\n"));
        assert_eq!(diff.matches(" unrelated\n").count(), 3);
        assert_eq!(diff.matches(" tail\n").count(), 3);
        assert!(diff.contains("-old\n+new\n"));
        let diff = summarize(
            "old\n",
            &format!("api_key={}\nnew\n", "x".repeat(40 * 1024)),
        );
        assert!(diff.contains("+api_key=***\n+new\n"), "{diff}");
        assert!(!diff.contains("response limit"));
        assert!(summarize("old\n", &"x".repeat(40 * 1024)).contains("response limit"));
        assert!(summarize(&"old\n".repeat(201), "new\n").contains("exceeds 200 lines"));
    }
    #[test]
    fn diff_handles_empty_files_newlines_and_private_key_boundaries() {
        assert!(summarize("", "new\n").contains("@@ -0,0 +1,1 @@\n+new\n"));
        assert!(summarize("old\n", "").contains("@@ -1,1 +0,0 @@\n-old\n"));
        assert!(summarize("same", "same\n").contains("No newline at end of file"));
        let before = format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{}-----END RSA PRIVATE KEY-----\nold\n",
            "private-key-body\n".repeat(10)
        );
        let diff = summarize(&before, &before.replace("old\n", "new\n"));
        assert!(!diff.contains("private-key-body"));
        assert!(diff.contains("-old\n+new\n"));
    }
    #[test]
    fn immutable_image_and_non_regular_outputs() {
        assert!(!valid_image("agent:latest"));
        assert!(valid_image(&format!(
            "example/agent@sha256:{}",
            "a".repeat(64)
        )));
        for &kind in b"12345x" {
            let mut tar = vec![0; 512];
            tar[156] = kind;
            assert!(regular_tar_payload(&tar).is_err());
        }
        let mut tar = vec![0; 1024];
        tar[124..136].copy_from_slice(b"00000000003\0");
        tar[512..515].copy_from_slice(b"abc");
        assert_eq!(regular_tar_payload(&tar).unwrap(), b"abc");
    }
}
