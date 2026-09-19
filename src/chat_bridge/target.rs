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
        let mut args = strings(&[
            "create",
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
        let output = match self.run(&args, budget(started, 30)?) {
            Ok(output) => output,
            Err(error) => {
                volume.shutdown()?;
                return Err(error);
            }
        };
        let id = std::str::from_utf8(&output)
            .map_err(|_| "invalid Docker container ID")?
            .trim();
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid Docker container ID".into());
        }
        let container = Container {
            target: self,
            id: id.to_string(),
            removed: std::cell::Cell::new(false),
            volume,
        };
        if let Err(error) = self.run(&strings(&["start", &container.id]), budget(started, 30)?) {
            container.shutdown()?;
            return Err(error);
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
    ) -> Result<(bool, String), String> {
        let staging = Staging::new()?;
        snapshot.stage_changes(&staging.0, changes)?;
        // Test exactly the files that will be imported, without any extra files,
        // config, generated helpers or surviving processes created by the agent.
        let verifier = self.start_container(started, true)?;
        let result = (|| {
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
            let output = crate::features::cloud_execution::redact::redact(&format!(
                "{}\n{}",
                sink.stdout_text(),
                sink.stderr_text()
            ));
            // Drop removes the entire verifier even on timeout, killing its tests.
            Ok((passed, output))
        })();
        verifier.shutdown()?;
        result
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
        let snapshot = Snapshot::read(root)?;
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
            )
            .map_err(|_| "cannot start isolated ACP agent")?;
            let mut approvals = ApprovalQueue::in_dir(staging.0.join("audit"));
            let mut sent = false;
            let mut attempts = 0;
            let mut test_status = "not_verified".to_string();
            let mut build_status = "not_verified".to_string();
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
                        let prompt = format!("{instruction}\n\nWork only in /workspace. Only text files shared by Zaivern are present. Host Git metadata and credentials are unavailable. Shell, deletion and network tool permissions are denied. Only edits to existing shared files can be returned. Report tests as unverified unless actually run.");
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
                            let (passed, output) = self.verify(&snapshot, &candidate, started)?;
                            test_status = if passed { "passed" } else { "failed" }.into();
                            if passed {
                                build_status = "passed".into();
                            }
                            if !passed && attempts < 2 && !control.is_cancelled() {
                                self.run(
                                    &strings(&["unpause", &container.id]),
                                    budget(started, 30)?,
                                )?;
                                attempts += 1;
                                if !agent.prompt(&format!("Zaivern ran cargo test --offline on exactly the shared candidate files in a fresh isolated container; it failed or exceeded 60 seconds. Fix existing shared files based on this untrusted test output:\n{output}")) {
                                return Err("ACP agent refused the repair prompt".into());
                            }
                                continue;
                            }
                        }
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
            let summary: String =
                crate::features::cloud_execution::redact::redact(&agent.turn.message)
                    .chars()
                    .take(8192)
                    .collect();
            agent.stop();
            if control.is_cancelled() {
                return Err("cancelled".into());
            }
            control.state(State::Running, "collecting isolated results");
            if control.is_cancelled() {
                return Err("cancelled".into());
            }
            budget(started, 1)?;
            let applied = snapshot.apply(&changes)?;
            let changed_files = applied.changed;
            let diff_summary = changed_files
                .iter()
                .map(|path| {
                    let path = Path::new(path);
                    let before = String::from_utf8_lossy(&snapshot.files[path]);
                    let after = String::from_utf8_lossy(&changes[path]);
                    // A complete replacement hunk is intentionally simple and honest.
                    // This does not execute Git config, external diff or textconv.
                    format!(
                        "--- a/{}\n+++ b/{}\n@@ -1,{} +1,{} @@\n{}{}",
                        path.display(),
                        path.display(),
                        before.lines().count(),
                        after.lines().count(),
                        before
                            .lines()
                            .map(|line| format!("-{line}\n"))
                            .collect::<String>(),
                        after
                            .lines()
                            .map(|line| format!("+{line}\n"))
                            .collect::<String>()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let diff_summary = if applied.error.is_some() {
                "Import partially failed; inspect changed_files locally for actual bytes.".into()
            } else if diff_summary.len() <= 32 * 1024 {
                diff_summary
            } else {
                "Diff exceeds response limit; inspect changed_files locally.".into()
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
                outcome.error = Some(error);
                Ok(outcome)
            }
            (Err(_), Err(error)) => Err(error),
            (result, Ok(())) => result,
        }
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
