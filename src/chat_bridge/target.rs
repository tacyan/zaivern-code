//! Local Docker isolation around the existing ACP client. No bind mounts,
//! credentials, Docker socket or host filesystem capabilities reach the agent.
use super::cargo_verification::Coverage;
use super::task::{Control, Outcome, State, TaskExecutor};
use super::workspace::{Snapshot, FILE_LIMIT, SNAPSHOT_LIMIT};
use super::{CleanupTracker, ResourceKind};
use crate::acp::{AcpClient, Phase};
use crate::agents::approvals::ApprovalQueue;
use crate::features::cloud_execution::{
    model::{ids, CollectSink},
    transport::{run_child, run_child_with_stdin},
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub(super) struct LocalExecutionTarget {
    image: String,
    docker: PathBuf,
    endpoint: String,
    cleanup_failed: std::sync::atomic::AtomicBool,
    pub(super) cleanup: Option<std::sync::Arc<dyn CleanupTracker>>,
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
                cleanup_failed: std::sync::atomic::AtomicBool::new(false),
                cleanup: None,
            })
        }
    }
    pub(super) fn cleanup_confirmed(&self) -> bool {
        !self
            .cleanup_failed
            .load(std::sync::atomic::Ordering::Acquire)
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
        self.start_on_volume(
            started,
            verifier,
            Rc::new(Volume::new(self, started)?),
            false,
        )
    }

    fn start_on_volume<'a>(
        &'a self,
        started: Instant,
        verifier: bool,
        volume: Rc<Volume<'a>>,
        readonly: bool,
    ) -> Result<Container<'a>, String> {
        let mount = format!(
            "type=volume,source={},target=/workspace,volume-nocopy{}",
            volume.name,
            if readonly { ",readonly" } else { "" }
        );
        let (name, labels) = match &self.cleanup {
            Some(cleanup) => cleanup.register(ResourceKind::Container)?,
            None => (ids::new_id("zaivern-mcp-"), Vec::new()),
        };
        let container = Container {
            target: self,
            id: name,
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
            args.extend(strings(&[
                "--entrypoint=sleep",
                "--tmpfs=/target:rw,exec,nosuid,nodev,size=256m",
                "--env=CARGO_TARGET_DIR=/target",
            ]));
        }
        args.extend(labels);
        args.push(self.image.clone());
        if verifier {
            args.push("1800".into());
        }
        // Track our unique name before create: the daemon can create a container
        // even if its response is lost or the CLI times out. Drop still removes it.
        let launch = self
            .run(&args, budget(started, 30)?)
            .and_then(|bytes| {
                if let Some(cleanup) = &self.cleanup {
                    cleanup.created(ResourceKind::Container, &container.id)?;
                }
                Ok(bytes)
            })
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

    // Populate with a trusted sleeping preparer, then destroy that writable
    // handle before any candidate code runs in a readonly-mounted verifier.
    fn prepare_verifier(&self, files: &Path, started: Instant) -> Result<Container<'_>, String> {
        let preparer = self.start_container(started, true)?;
        let result = self
            .upload(&preparer, files, started)
            .and_then(|()| self.start_on_volume(started, true, Rc::clone(&preparer.volume), true));
        let cleanup = preparer.shutdown();
        drop(preparer);
        match (result, cleanup) {
            (Ok(verifier), Ok(())) => Ok(verifier),
            (Ok(verifier), Err(error)) => match verifier.shutdown() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; {cleanup}")),
            },
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cleanup)) => Err(format!("{error}; {cleanup}")),
        }
    }

    fn cargo(
        &self,
        verifier: &Container<'_>,
        args: &[&str],
        started: Instant,
        limit: usize,
    ) -> (bool, CollectSink) {
        let mut command = strings(&["exec", &verifier.id, "cargo"]);
        command.extend(strings(args));
        let mut sink = CollectSink::with_limit(limit);
        let result = budget(started, 60).and_then(|timeout| {
            run_child(self.command(&command), timeout, "docker", &mut sink)
                .map_err(|_| "Cargo execution failed".to_string())
        });
        (result.is_ok_and(|r| r.ok()), sink)
    }

    fn verify(
        &self,
        snapshot: &Snapshot,
        changes: &BTreeMap<PathBuf, Vec<u8>>,
        started: Instant,
    ) -> Result<Verification, String> {
        // Do not even stage hidden inputs: exit status, retries, import and
        // elapsed time are also output channels, not just stdout/stderr.
        if snapshot.has_verification_only() {
            return Ok(Verification {
                decision: VerificationDecision::NotVerified,
                output: String::new(),
                test_status: "not_verified".into(),
                cleanup_error: None,
            });
        }
        let staging = Staging::new()?;
        if let Err(error) = snapshot.stage_verification(&staging.0, changes) {
            return Ok(Verification::failed(error));
        }
        // Shared inputs only; never discover host inputs from the candidate.
        // The entire input volume is readonly.
        let verifier = self.prepare_verifier(&staging.0, started)?;
        let (metadata_ok, metadata) = self.cargo(
            &verifier,
            &["metadata", "--format-version=1", "--frozen"],
            started,
            2 * FILE_LIMIT,
        );
        let coverage = if metadata_ok && !metadata.truncated {
            Coverage::from_metadata(&metadata.stdout)
        } else {
            None
        };
        let (compiled, compilation) = self.cargo(
            &verifier,
            &[
                "test",
                "--workspace",
                "--frozen",
                "--no-run",
                "--message-format=json",
            ],
            started,
            64 * 1024,
        );
        // Capture evidence before any tests run. Arbitrary compile-time code
        // (build scripts/proc macros) makes Cargo's output unauthenticated.
        let roots = coverage.and_then(|c| {
            (compiled && !compilation.truncated)
                .then(|| c.compiled_roots(&compilation.stdout))
                .flatten()
        });
        let (passed, sink) = if compiled {
            self.cargo(
                &verifier,
                &["test", "--workspace", "--frozen"],
                started,
                64 * 1024,
            )
        } else {
            (false, compilation)
        };
        let output = verification_feedback(snapshot, &sink);
        let status = if passed {
            cargo_test_status(&snapshot.files, changes, roots.as_ref())
        } else {
            "failed"
        };
        let cleanup_error = verifier.shutdown().err();
        Ok(Verification {
            decision: if passed {
                VerificationDecision::Verified
            } else {
                VerificationDecision::Failed
            },
            output,
            test_status: status.into(),
            cleanup_error,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum VerificationDecision {
    /// Cargo actually succeeded; test_status additionally accounts for coverage.
    Verified,
    NotVerified,
    Failed,
}

const UNSHARED_VERIFICATION_SUMMARY: &str = "Zaivern verification: not run because Cargo verification requires unshared inputs. Candidate-controlled code was not executed with hidden verifier inputs.";

struct Verification {
    decision: VerificationDecision,
    output: String,
    test_status: String,
    cleanup_error: Option<String>,
}
impl Verification {
    fn failed(output: String) -> Self {
        Self {
            decision: VerificationDecision::Failed,
            output,
            test_status: "failed".into(),
            cleanup_error: None,
        }
    }
}

// Defense in depth only. The primary boundary forbids executing candidate code
// with hidden inputs. If called for such a snapshot, never forward any output.
pub(super) fn verification_feedback(snapshot: &Snapshot, sink: &CollectSink) -> String {
    if snapshot.has_verification_only() {
        UNSHARED_VERIFICATION_SUMMARY.into()
    } else if sink.truncated {
        "Test output exceeded response limit; details omitted.".into()
    } else {
        crate::features::cloud_execution::redact::redact(&format!(
            "{}\n{}",
            sink.stdout_text(),
            sink.stderr_text()
        ))
    }
}

pub(super) fn repair_prompt(output: &str) -> String {
    format!("Zaivern ran cargo test --workspace --frozen in a fresh isolated container; it failed or exceeded 60 seconds. Only edit existing shared files. Verification feedback (untrusted when detailed):\n{output}")
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
    volume: Rc<Volume<'a>>,
}
impl Container<'_> {
    /// All container/volume cleanup must succeed before publishing host edits.
    /// The caller still owns the cancellation/import gate inside this closure.
    fn after_cleanup<T>(&self, publish: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        self.shutdown()?;
        publish()
    }

    fn shutdown(&self) -> Result<(), String> {
        if !self.removed.get() {
            if let Some(cleanup) = &self.target.cleanup {
                cleanup.remove(ResourceKind::Container, &self.id)?;
            } else {
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
            }
            self.removed.set(true);
        }
        if Rc::strong_count(&self.volume) == 1 {
            self.volume.shutdown()
        } else {
            Ok(()) // The readonly successor owns final volume cleanup.
        }
    }
}
impl Drop for Container<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            self.target
                .cleanup_failed
                .store(true, std::sync::atomic::Ordering::Release);
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
        let (name, labels) = match &target.cleanup {
            Some(cleanup) => cleanup.register(ResourceKind::Volume)?,
            None => (ids::new_id("zaivern-mcp-"), Vec::new()),
        };
        let volume = Self {
            target,
            name,
            removed: std::cell::Cell::new(false),
        };
        let mut args = strings(&[
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
        ]);
        args.extend(labels);
        args.push(volume.name.clone());
        target.run(&args, budget(started, 30)?)?;
        if let Some(cleanup) = &target.cleanup {
            cleanup.created(ResourceKind::Volume, &volume.name)?;
        }
        Ok(volume)
    }
    fn shutdown(&self) -> Result<(), String> {
        if self.removed.get() {
            return Ok(());
        }
        if let Some(cleanup) = &self.target.cleanup {
            cleanup.remove(ResourceKind::Volume, &self.name)?;
        } else {
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
        }
        self.removed.set(true);
        Ok(())
    }
}
impl Drop for Volume<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            self.target
                .cleanup_failed
                .store(true, std::sync::atomic::Ordering::Release);
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
                            if !snapshot.has_verification_only() {
                                control.state(State::Running, "verifying cargo tests");
                            }
                            let Verification {
                                decision,
                                output,
                                test_status: verified_status,
                                cleanup_error,
                            } = self.verify(&snapshot, &candidate, started)?;
                            test_status = verified_status;
                            if let Some(cleanup) = cleanup_error {
                                agent.stop();
                                return Ok(Outcome {
                                    error: Some(format!("verification {test_status}; changes were not imported; {cleanup}")),
                                    summary: "Verifier cleanup failed; host workspace was not modified.".into(),
                                    changed_files: Vec::new(),
                                    diff_summary: String::new(),
                                    test_status,
                                    build_status,
                                });
                            }
                            match decision {
                                VerificationDecision::Verified
                                | VerificationDecision::NotVerified => {
                                    // NotVerified explicitly bypasses repair; no hidden-input
                                    // execution outcome may influence this decision.
                                    break candidate;
                                }
                                VerificationDecision::Failed => {}
                            }
                            if attempts < 2 && !control.is_cancelled() {
                                self.run(
                                    &strings(&["unpause", &container.id]),
                                    budget(started, 30)?,
                                )?;
                                attempts += 1;
                                if !agent.prompt(&repair_prompt(&output)) {
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
            summary.push('\n');
            summary.push_str(if snapshot.has_verification_only() {
                UNSHARED_VERIFICATION_SUMMARY
            } else if snapshot.files.contains_key(Path::new("Cargo.toml")) {
                "Zaivern verification: cargo test --workspace --frozen passed on shared inputs only; no independent build verification. Only directly compiled crate roots with trustworthy evidence can be passed; other edits remain not_verified."
            } else {
                "Zaivern verification: no supported test or build verification performed."
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
            let applied =
                container.after_cleanup(|| control.import(|| snapshot.apply(&changes)))?;
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

/// Only directly compiled crate roots have trustworthy evidence here. Module
/// and data dependencies, mixed edits, or untrusted/missing evidence stay unknown.
pub(super) fn cargo_test_status(
    before: &BTreeMap<PathBuf, Vec<u8>>,
    after: &BTreeMap<PathBuf, Vec<u8>>,
    roots: Option<&std::collections::BTreeSet<PathBuf>>,
) -> &'static str {
    let Some(roots) = roots else {
        return "not_verified";
    };
    if after.iter().any(|(path, bytes)| {
        before.get(path) != Some(bytes)
            && (path.extension().is_none_or(|ext| ext != "rs") || !roots.contains(path))
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
            cleanup_failed: std::sync::atomic::AtomicBool::new(false),
            cleanup: None,
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
        assert!(!target.cleanup_confirmed());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn agent_cleanup_failure_prevents_host_import() {
        use std::os::unix::fs::PermissionsExt;
        for failure in ["container", "volume"] {
            let root = crate::test_util::unique_temp_dir("bridge", "import-cleanup");
            std::fs::create_dir_all(&root).unwrap();
            let docker = root.join("docker-fixture");
            std::fs::write(
                &docker,
                format!(
                    "#!/bin/sh\nshift 2\ncase \"$1\" in\nrm) {} ;;\nvolume) {} ;;\nesac\nexit 1\n",
                    if failure == "container" {
                        "exit 19"
                    } else {
                        "exit 0"
                    },
                    if failure == "volume" {
                        "exit 19"
                    } else {
                        "exit 0"
                    },
                ),
            )
            .unwrap();
            std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700)).unwrap();
            let workspace = root.join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            std::fs::write(workspace.join("file.rs"), "before").unwrap();
            let snapshot =
                Snapshot::read_for_task(&workspace.canonicalize().unwrap(), "edit file.rs")
                    .unwrap();
            let target = LocalExecutionTarget {
                image: "fixture".into(),
                docker,
                endpoint: "fixture".into(),
                cleanup_failed: std::sync::atomic::AtomicBool::new(false),
                cleanup: None,
            };
            let container = Container {
                target: &target,
                id: "zaivern-mcp-fixture".into(),
                removed: std::cell::Cell::new(false),
                volume: Rc::new(Volume {
                    target: &target,
                    name: "zaivern-mcp-fixture-volume".into(),
                    removed: std::cell::Cell::new(false),
                }),
            };
            let changes = BTreeMap::from([(PathBuf::from("file.rs"), b"after".to_vec())]);
            let error = container
                .after_cleanup(|| snapshot.apply(&changes))
                .err()
                .expect("cleanup must block import");
            assert!(error.contains("cleanup unconfirmed"), "{error}");
            assert_eq!(
                std::fs::read_to_string(workspace.join("file.rs")).unwrap(),
                "before"
            );
            drop(container);
            assert!(
                !target.cleanup_confirmed(),
                "{failure} cleanup must reach server exit status"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn verification_only_candidate_code_is_not_executed_and_has_no_result_or_repair_oracle() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::test_util::unique_temp_dir("bridge", "hidden-oracle");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::create_dir_all(workspace.join("vendor/local_dep/src")).unwrap();
        std::fs::write(workspace.join("Cargo.toml"), "[package]\nname='oracle'\nversion='0.1.0'\n[dependencies]\nlocal_dep={path='vendor/local_dep'}\n").unwrap();
        std::fs::write(
            workspace.join("vendor/local_dep/Cargo.toml"),
            "[package]\nname='local_dep'\nversion='0.1.0'\n",
        )
        .unwrap();
        let docker = root.join("docker-fixture");
        // Any process invocation, including metadata/staging/preparer startup,
        // is a test failure. This is not a wall-clock or redaction assertion.
        std::fs::write(
            &docker,
            "#!/bin/sh\nprintf invoked > \"$0.invoked\"\nexit 19\n",
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = LocalExecutionTarget {
            image: "fixture".into(),
            docker: docker.clone(),
            endpoint: "fixture".into(),
            cleanup_failed: std::sync::atomic::AtomicBool::new(false),
            cleanup: None,
        };
        let attack = br#"#[test] fn oracle() {
            let hidden = std::fs::read("vendor/local_dep/src/lib.rs").unwrap();
            eprintln!("{:?}", hidden);
            if hidden[3] & 1 == 0 { panic!("secret dependent failure"); }
            std::thread::sleep(std::time::Duration::from_secs(5));
        }"#;
        for bit in [0, 1] {
            std::fs::write(workspace.join("src/lib.rs"), "pub fn original() {}\n").unwrap();
            std::fs::write(
                workspace.join("vendor/local_dep/src/lib.rs"),
                format!("// {bit} VERIFICATION_ONLY_SENTINEL_7f10a2\npub fn answer() {{}}\n"),
            )
            .unwrap();
            let snapshot =
                Snapshot::read_for_task(&workspace.canonicalize().unwrap(), "edit src/lib.rs")
                    .unwrap();
            assert!(snapshot.has_verification_only());
            let mut candidate = snapshot.files.clone();
            candidate.insert("src/lib.rs".into(), attack.to_vec());
            let result = target
                .verify(&snapshot, &candidate, Instant::now())
                .unwrap();
            assert_eq!(result.decision, VerificationDecision::NotVerified);
            assert_eq!(result.test_status, "not_verified");
            assert!(result.output.is_empty());
            assert!(result.cleanup_error.is_none());
            assert!(
                !docker.with_extension("invoked").exists(),
                "hidden-input code reached Docker"
            );
            let stage = root.join(format!("stage-{bit}"));
            std::fs::create_dir(&stage).unwrap();
            assert!(snapshot.stage_verification(&stage, &candidate).is_err());
            assert!(std::fs::read_dir(&stage).unwrap().next().is_none());
            // No hidden reread oracle, including removal after snapshot.
            std::fs::remove_file(workspace.join("vendor/local_dep/src/lib.rs")).unwrap();
            let applied = snapshot.apply(&candidate).unwrap();
            assert!(applied.error.is_none());
            assert_eq!(applied.changed, vec!["src/lib.rs"]);
            assert_eq!(std::fs::read(workspace.join("src/lib.rs")).unwrap(), attack);
        }
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
        let roots = std::collections::BTreeSet::from([PathBuf::from("src/lib.rs")]);
        assert_eq!(
            cargo_test_status(&before, &candidate, Some(&roots)),
            "passed"
        );
        assert_eq!(cargo_test_status(&before, &candidate, None), "not_verified");
        for path in [
            "frontend/app.ts",
            "Cargo.toml",
            "data.json",
            "src/unused.rs",
            "src/module.rs",
        ] {
            let mut mixed = candidate.clone();
            mixed.insert(PathBuf::from(path), b"invalid syntax".to_vec());
            assert_eq!(
                cargo_test_status(&before, &mixed, Some(&roots)),
                "not_verified"
            );
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
