//! Write-ahead Docker ownership journal. A receipt never substitutes for missing
//! cleanup evidence. All callers hold mcp.lock; repair also holds runtime.lock.
use super::{
    config::Config,
    docker,
    private::{self, Result},
    process,
};
use crate::features::chat_bridge::imp::{CleanupTracker, ResourceKind};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

const FILE: &str = "cleanup-pending.json";
const PREPARED: &str = "cleanup-prepared.json";
// 128 admitted tasks, at most 7 containers + 4 volumes per task. Retain the
// completed entries until the generation is retired; never silently evict debt.
const MAX_RESOURCES: usize = 2048;
const MAX_BYTES: usize = 1024 * 1024;
const OWNER: &str = "org.zaivern.chatgpt.owner";
const GENERATION: &str = "org.zaivern.chatgpt.generation";
const RESOURCE: &str = "org.zaivern.chatgpt.resource";

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Open,
    Running,
    Reconciling,
    Confirmed,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResourceState {
    Intent,
    // Persist the completed rejection before querying Docker: a failed absence
    // query must remain retryable without losing this evidence.
    Rejected,
    Created,
    Removed,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Resource {
    kind: ResourceKind,
    token: String,
    name: String,
    state: ResourceState,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    generation: String,
    owner: String,
    endpoint: String,
    phase: Phase,
    resources: Vec<Resource>,
}

pub(super) struct Cleanup {
    root: PathBuf,
    config: Config,
    journal: Mutex<Journal>,
}

pub(super) fn valid_nonce(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn owner(root: &Path) -> String {
    super::install::sha256(root.as_os_str().as_encoded_bytes())
}

fn active(root: &Path) -> Result<String> {
    let value = String::from_utf8(private::read(&root.join("active-generation"), 64)?)
        .map_err(|_| "Invalid active generation")?;
    if !valid_nonce(&value) {
        return Err("Invalid active generation".into());
    }
    Ok(value)
}

fn read(root: &Path) -> Result<Journal> {
    let journal = read_named(root, FILE)?;
    if journal.generation != active(root)? {
        return Err("Cleanup journal generation mismatch; no Docker resources were changed".into());
    }
    Ok(journal)
}

fn read_named(root: &Path, file: &str) -> Result<Journal> {
    private::directory(root)?;
    let journal: Journal = serde_json::from_slice(&private::read(&root.join(file), MAX_BYTES)?)
        .map_err(|_| "Invalid cleanup journal; no Docker resources were changed")?;
    if journal.version != 1
        || !valid_nonce(&journal.generation)
        || journal.owner != owner(root)
        || journal.resources.len() > MAX_RESOURCES
        || !journal
            .endpoint
            .strip_prefix("unix://")
            .is_some_and(|p| Path::new(p).is_absolute())
    {
        return Err("Cleanup journal identity mismatch; no Docker resources were changed".into());
    }
    let mut names = std::collections::BTreeSet::new();
    for resource in &journal.resources {
        if !valid_nonce(&resource.token)
            || resource.name != format!("zaivern-mcp-{}-{}", journal.generation, resource.token)
            || !names.insert(&resource.name)
        {
            return Err(
                "Invalid cleanup resource identity; no Docker resources were changed".into(),
            );
        }
    }
    if journal.phase == Phase::Confirmed
        && journal
            .resources
            .iter()
            .any(|r| r.state != ResourceState::Removed)
    {
        return Err("Invalid cleanup confirmation".into());
    }
    Ok(journal)
}

fn write(root: &Path, journal: &Journal) -> Result<()> {
    let bytes = serde_json::to_vec(journal).map_err(|_| "Cannot encode cleanup journal")?;
    if bytes.len() > MAX_BYTES {
        return Err("Cleanup journal limit reached".into());
    }
    private::write(&root.join(FILE), &bytes, false)
}

impl Cleanup {
    pub(super) fn prepare(root: &Path, config: &Config, generation: &str) -> Result<()> {
        if !valid_nonce(generation) {
            return Err("Invalid cleanup generation".into());
        }
        let journal = Journal {
            version: 1,
            generation: generation.into(),
            owner: owner(root),
            endpoint: config.docker_endpoint.clone(),
            phase: Phase::Open,
            resources: Vec::new(),
        };
        // Write-ahead activation: no client may spawn until all three commits
        // finish. Repair can complete a crash between these writes safely.
        private::write(
            &root.join(PREPARED),
            &serde_json::to_vec(&journal).map_err(|_| "Cannot encode prepared cleanup journal")?,
            false,
        )?;
        private::write(
            &root.join("active-generation"),
            generation.as_bytes(),
            false,
        )?;
        write(root, &journal)?;
        private::remove(&root.join(PREPARED))
    }

    pub(super) fn load(root: &Path, config: &Config) -> Result<Self> {
        let journal = read(root)?;
        if journal.endpoint != config.docker_endpoint {
            return Err("Cleanup Docker endpoint changed; restore the configured local socket before repair".into());
        }
        Ok(Self {
            root: root.into(),
            config: config.clone(),
            journal: Mutex::new(journal),
        })
    }

    pub(super) fn admit(&self, generation: &str) -> Result<()> {
        let mut journal = self
            .journal
            .lock()
            .map_err(|_| "Cleanup journal unavailable")?;
        if journal.generation != generation || journal.phase != Phase::Open {
            return Err(
                "Managed MCP generation is closed or already used; run stop, repair, then start"
                    .into(),
            );
        }
        journal.phase = Phase::Running;
        write(&self.root, &journal)
    }

    fn labels(journal: &Journal, resource: &Resource) -> BTreeMap<String, String> {
        BTreeMap::from([
            (OWNER.into(), journal.owner.clone()),
            (GENERATION.into(), journal.generation.clone()),
            (RESOURCE.into(), resource.token.clone()),
        ])
    }

    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        if super::config::validate_executable(&self.config.docker, &self.config.workspace)?
            != self.config.docker
        {
            return Err("Configured Docker executable changed; cleanup state preserved".into());
        }
        docker::validate_endpoint(&self.config.docker_endpoint)?;
        let mut command = docker::command(&self.config.docker, &self.config.docker_endpoint);
        command.args(args);
        process::capture(command, Duration::from_secs(20), MAX_BYTES).map_err(|_| {
            "Docker cleanup query/removal failed; state preserved. Retry zai chatgpt repair".into()
        })
    }

    fn exists(&self, resource: &Resource) -> Result<bool> {
        // A failed inspect is not proof of absence (daemon down/permission error).
        // Require a successful listing; compare complete names, never substrings.
        let filter = format!("name={}", resource.name);
        let args = match resource.kind {
            ResourceKind::Container => vec![
                "container",
                "ls",
                "--all",
                "--format",
                "{{.Names}}",
                "--filter",
                &filter,
            ],
            ResourceKind::Volume => {
                vec!["volume", "ls", "--format", "{{.Name}}", "--filter", &filter]
            }
        };
        let output = self.run(&args)?;
        let names = std::str::from_utf8(&output).map_err(|_| "Invalid Docker resource listing")?;
        Ok(names.lines().any(|name| name == resource.name))
    }

    fn inspect_owned(&self, journal: &Journal, resource: &Resource) -> Result<String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Identity {
            id: String,
            name: String,
            labels: BTreeMap<String, String>,
        }
        let (kind, format) = match resource.kind {
            ResourceKind::Container => (
                "container",
                r#"{"id":{{json .Id}},"name":{{json .Name}},"labels":{{json .Config.Labels}}}"#,
            ),
            ResourceKind::Volume => (
                "volume",
                r#"{"id":{{json .Name}},"name":{{json .Name}},"labels":{{json .Labels}}}"#,
            ),
        };
        let bytes = self.run(&[kind, "inspect", "--format", format, &resource.name])?;
        let identity: Identity =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid Docker ownership metadata")?;
        if identity.name.trim_start_matches('/') != resource.name
            || Self::labels(journal, resource)
                .iter()
                .any(|(key, value)| identity.labels.get(key) != Some(value))
            || match resource.kind {
                ResourceKind::Container => !valid_nonce(&identity.id),
                ResourceKind::Volume => identity.id != resource.name,
            }
        {
            return Err("Docker resource ownership mismatch; nothing was removed".into());
        }
        Ok(identity.id)
    }

    fn remove_one(&self, journal: &mut Journal, index: usize) -> Result<()> {
        let resource = &journal.resources[index];
        if self.exists(resource)? {
            let id = self.inspect_owned(journal, resource)?;
            // Persist proof that create completed before attempting removal.
            journal.resources[index].state = ResourceState::Created;
            write(&self.root, journal)?;
            match journal.resources[index].kind {
                ResourceKind::Container => {
                    self.run(&["container", "rm", "--force", &id])?;
                }
                ResourceKind::Volume => {
                    self.run(&["volume", "rm", &id])?;
                }
            }
        } else if resource.state == ResourceState::Intent {
            // A lost create response can still be executing in Docker. Do not
            // turn a single empty list into a fabricated cleanup receipt.
            return Err("Docker creation outcome is unresolved; journal retained. Restore Docker and retry repair after the resource appears".into());
        }
        if self.exists(&journal.resources[index])? {
            return Err("Docker resource still exists; cleanup remains unconfirmed".into());
        }
        journal.resources[index].state = ResourceState::Removed;
        write(&self.root, journal)
    }

    pub(super) fn finish(&self, recovery: bool) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        let mut progress = std::time::Instant::now();
        let mut journal = self
            .journal
            .lock()
            .map_err(|_| "Cleanup journal unavailable")?;
        journal.phase = Phase::Reconciling;
        write(&self.root, &journal)?; // Revoke admission before any Docker action.
                                      // Containers first: a failed volume removal must not lose the already
                                      // removed container's evidence. Every entry remains available on retry.
        for kind in [ResourceKind::Container, ResourceKind::Volume] {
            for index in 0..journal.resources.len() {
                if journal.resources[index].kind == kind
                    && journal.resources[index].state != ResourceState::Removed
                {
                    if std::time::Instant::now() >= deadline {
                        return Err("Cleanup reconciliation reached its two-minute budget; progress saved. Retry zai chatgpt repair".into());
                    }
                    if recovery && progress.elapsed() >= Duration::from_secs(10) {
                        println!("Verifying owned Docker cleanup; progress is saved...");
                        progress = std::time::Instant::now();
                    }
                    self.remove_one(&mut journal, index)?;
                }
            }
        }
        journal.phase = Phase::Confirmed;
        write(&self.root, &journal)?;
        private::write(
            &self.root.join("mcp.done"),
            journal.generation.as_bytes(),
            false,
        )?;
        if recovery {
            super::daemon::write_shutdown(&self.root, &journal.generation, true)?;
        }
        Ok(())
    }
}

impl CleanupTracker for Cleanup {
    fn register(&self, kind: ResourceKind) -> Result<(String, Vec<String>)> {
        let mut journal = self
            .journal
            .lock()
            .map_err(|_| "Cleanup journal unavailable")?;
        if journal.phase != Phase::Running || journal.resources.len() >= MAX_RESOURCES {
            return Err("Cleanup journal closed or resource limit reached; task rejected".into());
        }
        let token = private::nonce()?;
        let name = format!("zaivern-mcp-{}-{token}", journal.generation);
        let resource = Resource {
            kind,
            token,
            name: name.clone(),
            state: ResourceState::Intent,
        };
        let labels = Self::labels(&journal, &resource)
            .into_iter()
            .flat_map(|(key, value)| ["--label".into(), format!("{key}={value}")])
            .collect();
        journal.resources.push(resource);
        write(&self.root, &journal)?;
        Ok((name, labels))
    }

    fn created(&self, kind: ResourceKind, name: &str) -> Result<()> {
        let mut journal = self
            .journal
            .lock()
            .map_err(|_| "Cleanup journal unavailable")?;
        let index = journal
            .resources
            .iter()
            .position(|r| r.kind == kind && r.name == name)
            .ok_or("Unregistered Docker resource")?;
        self.inspect_owned(&journal, &journal.resources[index])?;
        journal.resources[index].state = ResourceState::Created;
        write(&self.root, &journal)
    }

    fn rejected(&self, kind: ResourceKind, name: &str) -> Result<()> {
        let mut journal = self
            .journal
            .lock()
            .map_err(|_| "Cleanup journal unavailable")?;
        if journal.phase != Phase::Running {
            return Err("Cleanup generation is closed".into());
        }
        let resource = journal
            .resources
            .iter_mut()
            .find(|r| r.kind == kind && r.name == name)
            .ok_or("Unregistered Docker resource")?;
        if resource.state != ResourceState::Intent {
            return Err("Docker create outcome already recorded".into());
        }
        resource.state = ResourceState::Rejected;
        write(&self.root, &journal)
    }

    fn remove(&self, kind: ResourceKind, name: &str) -> Result<()> {
        let mut journal = self
            .journal
            .lock()
            .map_err(|_| "Cleanup journal unavailable")?;
        let index = journal
            .resources
            .iter()
            .position(|r| r.kind == kind && r.name == name)
            .ok_or("Unregistered Docker resource")?;
        self.remove_one(&mut journal, index)
    }
}

pub(super) fn ensure_confirmed(root: &Path) -> Result<()> {
    if private::exists_checked(&root.join(PREPARED))? {
        return Err("Generation activation interrupted; run zai chatgpt repair".into());
    }
    if private::exists_checked(&root.join(FILE))? && read(root)?.phase != Phase::Confirmed {
        return Err("Previous cleanup: UNCONFIRMED. Run zai chatgpt repair; start/setup/reset remain blocked".into());
    }
    Ok(())
}

pub(super) fn report(root: &Path) -> Result<String> {
    if private::exists_checked(&root.join(PREPARED))? {
        read_named(root, PREPARED)?;
        return Ok("Previous cleanup: UNCONFIRMED (activation interrupted)\nRecovery: zai chatgpt repair\n".into());
    }
    if !private::exists_checked(&root.join(FILE))? {
        return Ok(if super::daemon::ensure_clean(root).is_ok() {
            "Previous cleanup: confirmed / no generation\n".into()
        } else {
            "Previous cleanup: UNCONFIRMED (legacy state without resource journal). Recovery: local Docker ownership audit required; state preserved\n".into()
        });
    }
    let journal = read(root)?;
    let count = |kind| {
        journal
            .resources
            .iter()
            .filter(|r| r.kind == kind && r.state != ResourceState::Removed)
            .count()
    };
    let (status, recovery) = if matches!(journal.phase, Phase::Open | Phase::Running)
        && super::daemon::generation_running(root, &journal.generation)
    {
        ("Current generation: RUNNING\nCleanup tracking: active", "")
    } else if journal.phase == Phase::Confirmed && super::daemon::ensure_clean(root).is_ok() {
        ("Previous cleanup: CONFIRMED", "")
    } else {
        (
            "Previous cleanup: UNCONFIRMED",
            "Recovery: zai chatgpt repair\n",
        )
    };
    Ok(format!(
        "{status}\nPending containers: {}\nPending volumes: {}\n{recovery}",
        count(ResourceKind::Container),
        count(ResourceKind::Volume)
    ))
}

pub(super) fn reconcile(root: &Path, config: &Config) -> Result<()> {
    // Caller holds operation/runtime locks. Never inspect/kill a saved PID.
    let _execution = private::Lock::acquire(root, "mcp.lock")
        .map_err(|_| "MCP cleanup is still running; wait, then retry zai chatgpt repair")?;
    if private::exists_checked(&root.join(PREPARED))? {
        let prepared = read_named(root, PREPARED)?;
        if prepared.phase != Phase::Open
            || !prepared.resources.is_empty()
            || prepared.endpoint != config.docker_endpoint
        {
            return Err("Invalid prepared generation; state preserved".into());
        }
        if private::exists_checked(&root.join(FILE))? {
            let previous = read_named(root, FILE)?;
            if !(previous.phase == Phase::Confirmed
                || previous.generation == prepared.generation
                    && previous.phase == Phase::Open
                    && previous.resources.is_empty())
            {
                return Err(
                    "Prepared generation conflicts with outstanding cleanup; state preserved"
                        .into(),
                );
            }
        } else if private::exists_checked(&root.join("active-generation"))?
            && active(root)? != prepared.generation
            && private::read(&root.join("mcp.done"), 64)? != active(root)?.as_bytes()
        {
            return Err("Previous generation cleanup is unconfirmed".into());
        }
        private::write(
            &root.join("active-generation"),
            prepared.generation.as_bytes(),
            false,
        )?;
        write(root, &prepared)?;
        private::remove(&root.join(PREPARED))?;
    }
    if super::daemon::ensure_clean(root).is_ok() && !private::exists_checked(&root.join(FILE))? {
        return Ok(());
    }
    Cleanup::load(root, config)?.finish(true)?;
    super::daemon::ensure_clean(root)
}

#[cfg(test)]
mod tests {
    use super::super::{
        daemon,
        tests::{fixture, Temp},
    };
    use super::*;
    use std::os::unix::{
        fs::{symlink, PermissionsExt},
        net::UnixListener,
    };

    struct Harness {
        temp: Temp,
        _socket: UnixListener,
        config: Config,
        generation: String,
    }
    impl Harness {
        fn new() -> Self {
            let temp = Temp::new();
            // macOS Unix socket paths are limited to 104 bytes.
            let socket_path =
                std::env::temp_dir().join(format!("zc-{}", &private::nonce().unwrap()[..12]));
            let socket = UnixListener::bind(&socket_path).unwrap();
            let mut config = fixture(&temp.0);
            config.docker_endpoint = format!("unix://{}", socket_path.display());
            let script = br#"#!/bin/sh
base="${0%/*}"
shift 2
kind="$1"
verb="$2"
shift 2
for arg in "$@"; do name="$arg"; done
printf '%s %s %s\n' "$kind" "$verb" "$name" >> "$base/commands"
case "$verb" in
create)
  if [ "$kind" = volume ]; then
    printf 'Error response from daemon: create %s: invalid option: "invalid-option"\n' "$name" >&2
  else
    printf '%s\n' 'Error response from daemon: No such image: sha256:0000' >&2
  fi
  exit 1 ;;
ls)
  [ ! -f "$base/daemon-down" ] || exit 1
  name="${name#name=}"
  [ ! -f "$base/$name.present" ] || printf '%s\n' "$name"
  exit 0 ;;
inspect) cat "$base/$name.identity"; exit $? ;;
rm)
  [ ! -f "$base/fail-$kind" ] || exit 1
  if [ "$kind" = container ]; then name=$(cat "$base/$name.name") || exit 1; fi
  rm -f "$base/$name.present"
  exit 0 ;;
esac
exit 1
"#;
            private::write(&config.docker, script, true).unwrap();
            let generation = private::nonce().unwrap();
            private::write(
                &temp.0.join("active-generation"),
                generation.as_bytes(),
                false,
            )
            .unwrap();
            Cleanup::prepare(&temp.0, &config, &generation).unwrap();
            Self {
                temp,
                _socket: socket,
                config,
                generation,
            }
        }
        fn cleanup(&self) -> Cleanup {
            Cleanup::load(&self.temp.0, &self.config).unwrap()
        }
        fn resource(&self, cleanup: &Cleanup, kind: ResourceKind) -> String {
            let (name, _) = cleanup.register(kind).unwrap();
            let journal = cleanup.journal.lock().unwrap();
            let resource = journal.resources.iter().find(|r| r.name == name).unwrap();
            let id = if kind == ResourceKind::Container {
                private::nonce().unwrap()
            } else {
                name.clone()
            };
            private::write(&self.temp.0.join(format!("{name}.present")), b"", false).unwrap();
            private::write(&self.temp.0.join(format!("{name}.identity")),
                &serde_json::to_vec(&serde_json::json!({"id":id,"name":name,"labels":Cleanup::labels(&journal, resource)})).unwrap(), false).unwrap();
            private::write(
                &self.temp.0.join(format!("{id}.name")),
                name.as_bytes(),
                false,
            )
            .unwrap();
            drop(journal);
            cleanup.created(kind, &name).unwrap();
            name
        }
        fn fail(&self, kind: &str) {
            private::write(&self.temp.0.join(format!("fail-{kind}")), b"", false).unwrap();
        }
        fn reconcile(&self) -> Result<()> {
            let _operation = private::Lock::acquire(&self.temp.0, "operation.lock")?;
            let _runtime = private::Lock::acquire(&self.temp.0, "runtime.lock")?;
            reconcile(&self.temp.0, &self.config)
        }
    }
    impl Drop for Harness {
        fn drop(&mut self) {
            let _ =
                std::fs::remove_file(self.config.docker_endpoint.strip_prefix("unix://").unwrap());
        }
    }

    // Exercise the real nonce/identity IPC, not an injected "running" boolean.
    fn report_with_supervisor(
        h: &Harness,
        generation: &str,
        valid_reply: bool,
        hold_lock: bool,
    ) -> String {
        use std::io::{Read, Write};
        let runtime = private::Lock::acquire(&h.temp.0, "runtime.lock").unwrap();
        let _runtime = hold_lock.then_some(runtime);
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let pid = std::process::id();
        private::write(
            &h.temp.0.join("runtime.json"),
            &serde_json::to_vec(&serde_json::json!({
                "version":1, "pid":pid, "child_pid":pid,
                "port":listener.local_addr().unwrap().port(), "generation":generation
            }))
            .unwrap(),
            false,
        )
        .unwrap();
        if !hold_lock {
            let text = report(&h.temp.0).unwrap();
            assert!(
                matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
            );
            return text;
        }
        let generation = generation.to_owned();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "missing status request"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            // Darwin can inherit O_NONBLOCK from the listening socket; use
            // the bounded blocking read consistently on both Unix platforms.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = String::new();
            (&mut stream)
                .take(256)
                .read_to_string(&mut request)
                .unwrap();
            assert_eq!(request, format!("{generation} status\n"));
            if valid_reply {
                writeln!(stream, "{generation} {pid} {pid}").unwrap();
            } else {
                writeln!(stream, "wrong identity").unwrap();
            }
        });
        let report = report(&h.temp.0).unwrap();
        server.join().unwrap();
        report
    }

    #[test]
    fn report_active_generation_requires_authenticated_matching_supervisor() {
        let h = Harness::new();
        for phase in [Phase::Open, Phase::Running] {
            if phase == Phase::Running {
                let cleanup = h.cleanup();
                cleanup.admit(&h.generation).unwrap();
                cleanup.register(ResourceKind::Container).unwrap();
                cleanup.register(ResourceKind::Volume).unwrap();
            }
            let text = report_with_supervisor(&h, &h.generation, true, true);
            assert!(text.contains("Current generation: RUNNING\nCleanup tracking: active"));
            assert!(!text.contains("UNCONFIRMED"));
            assert!(!text.contains("repair"));
            let count = usize::from(phase == Phase::Running);
            assert!(text.contains(&format!(
                "Pending containers: {count}\nPending volumes: {count}"
            )));
            // A display observation never produces a cleanup receipt/admission.
            assert!(daemon::ensure_clean(&h.temp.0).is_err());
            assert!(!h.temp.0.join("mcp.done").exists());
            for text in [
                report_with_supervisor(&h, &private::nonce().unwrap(), true, true),
                report_with_supervisor(&h, &h.generation, false, true),
                report_with_supervisor(&h, &h.generation, true, false),
                report(&h.temp.0).unwrap(), // saved identity, no live listener
            ] {
                assert!(text.contains("UNCONFIRMED"));
                assert!(text.contains("Recovery: zai chatgpt repair"));
            }
        }
    }

    #[test]
    fn report_stopped_phases_and_legacy_remain_fail_closed() {
        let h = Harness::new();
        for phase in [Phase::Running, Phase::Reconciling] {
            let mut journal = read(&h.temp.0).unwrap();
            journal.phase = phase;
            write(&h.temp.0, &journal).unwrap();
            let text = report(&h.temp.0).unwrap();
            assert!(text.contains("UNCONFIRMED"));
            assert!(text.contains("Recovery: zai chatgpt repair"));
        }
        h.reconcile().unwrap();
        let text = report(&h.temp.0).unwrap();
        assert!(text.contains("Previous cleanup: CONFIRMED"));
        assert!(!text.contains("repair"));
        assert!(text.contains("Pending containers: 0\nPending volumes: 0"));
        private::remove(&h.temp.0.join(FILE)).unwrap();
        private::remove(&h.temp.0.join("mcp.done")).unwrap();
        let text = report(&h.temp.0).unwrap();
        assert!(text.contains("UNCONFIRMED (legacy state without resource journal)"));
        assert!(text.contains("ownership audit required; state preserved"));
        assert!(daemon::ensure_clean(&h.temp.0).is_err());
    }

    #[test]
    fn cleanup_failure_blocks_start_reset_and_repair_reconciles() {
        let h = Harness::new();
        let cleanup = h.cleanup();
        cleanup.admit(&h.generation).unwrap();
        let container = h.resource(&cleanup, ResourceKind::Container);
        let volume = h.resource(&cleanup, ResourceKind::Volume);
        h.fail("container");
        assert!(cleanup.remove(ResourceKind::Container, &container).is_err());
        assert!(!h.temp.0.join("mcp.done").exists());
        assert!(daemon::start(&h.temp.0, false).is_err());
        assert!(super::super::reset(&h.temp.0).is_err());
        assert!(report(&h.temp.0)
            .unwrap()
            .contains("Pending containers: 1\nPending volumes: 1"));
        assert!(h.reconcile().is_err());
        assert!(!h.temp.0.join("mcp.done").exists());
        private::remove(&h.temp.0.join("fail-container")).unwrap();
        h.reconcile().unwrap();
        daemon::ensure_clean(&h.temp.0).unwrap();
        for name in [container, volume] {
            assert!(!h.temp.0.join(format!("{name}.present")).exists());
        }
        assert_eq!(
            private::read(&h.temp.0.join("mcp.done"), 64).unwrap(),
            h.generation.as_bytes()
        );
        assert!(report(&h.temp.0)
            .unwrap()
            .contains("Previous cleanup: CONFIRMED\nPending containers: 0\nPending volumes: 0"));
        assert!(h.cleanup().admit(&h.generation).is_err());
        h.reconcile().unwrap();
    }

    #[test]
    fn partial_reconciliation_preserves_debt_and_retries() {
        let h = Harness::new();
        let cleanup = h.cleanup();
        cleanup.admit(&h.generation).unwrap();
        let container = h.resource(&cleanup, ResourceKind::Container);
        let volume = h.resource(&cleanup, ResourceKind::Volume);
        h.fail("volume");
        assert!(h.reconcile().is_err());
        assert!(!h.temp.0.join(format!("{container}.present")).exists());
        assert!(h.temp.0.join(format!("{volume}.present")).exists());
        assert!(!h.temp.0.join("mcp.done").exists());
        assert!(report(&h.temp.0)
            .unwrap()
            .contains("Pending containers: 0\nPending volumes: 1"));
        private::remove(&h.temp.0.join("fail-volume")).unwrap();
        h.reconcile().unwrap();
        daemon::ensure_clean(&h.temp.0).unwrap();
    }

    #[test]
    fn missing_resources_require_successful_absence_query() {
        let h = Harness::new();
        let cleanup = h.cleanup();
        cleanup.admit(&h.generation).unwrap();
        let name = h.resource(&cleanup, ResourceKind::Volume);
        private::remove(&h.temp.0.join(format!("{name}.present"))).unwrap();
        private::write(&h.temp.0.join("daemon-down"), b"", false).unwrap();
        assert!(h.reconcile().is_err());
        assert!(!h.temp.0.join("mcp.done").exists());
        private::remove(&h.temp.0.join("daemon-down")).unwrap();
        h.reconcile().unwrap(); // Manual removal is recoverable, too.
        assert!(!std::fs::read_to_string(h.temp.0.join("commands"))
            .unwrap()
            .contains(" rm "));
    }

    #[test]
    fn forged_journal_and_unsafe_files_cannot_reach_docker() {
        for mutation in [
            "id",
            "unknown",
            "generation",
            "symlink",
            "hardlink",
            "mode",
            "endpoint",
            "oversized",
        ] {
            let h = Harness::new();
            let cleanup = h.cleanup();
            cleanup.admit(&h.generation).unwrap();
            cleanup.register(ResourceKind::Volume).unwrap();
            let path = h.temp.0.join(FILE);
            let mut json: serde_json::Value =
                serde_json::from_slice(&private::read(&path, MAX_BYTES).unwrap()).unwrap();
            match mutation {
                "id" => {
                    json["resources"][0]["name"] = "unrelated-production-volume".into();
                }
                "unknown" => {
                    json["resources"][0]["execute"] = "bad".into();
                }
                "generation" => {
                    json["generation"] = private::nonce().unwrap().into();
                }
                "endpoint" => {
                    json["endpoint"] = "tcp://remote:2375".into();
                }
                "symlink" => {
                    std::fs::rename(&path, h.temp.0.join("original")).unwrap();
                    symlink(h.temp.0.join("original"), &path).unwrap();
                }
                "hardlink" => {
                    std::fs::hard_link(&path, h.temp.0.join("alias")).unwrap();
                }
                "mode" => {
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                        .unwrap();
                }
                "oversized" => {
                    private::write(&path, &vec![b' '; MAX_BYTES + 1], false).unwrap();
                }
                _ => unreachable!(),
            }
            if ["id", "unknown", "generation", "endpoint"].contains(&mutation) {
                private::write(&path, &serde_json::to_vec(&json).unwrap(), false).unwrap();
            }
            assert!(h.reconcile().is_err(), "{mutation}");
            assert!(!h.temp.0.join("commands").exists(), "{mutation}");
            assert!(!h.temp.0.join("mcp.done").exists());
        }
    }

    #[test]
    fn foreign_labels_reject_use_and_deletion_even_with_valid_name() {
        for kind in [ResourceKind::Container, ResourceKind::Volume] {
            let h = Harness::new();
            let cleanup = h.cleanup();
            cleanup.admit(&h.generation).unwrap();
            let name = h.resource(&cleanup, kind);
            let path = h.temp.0.join(format!("{name}.identity"));
            let mut json: serde_json::Value =
                serde_json::from_slice(&private::read(&path, MAX_BYTES).unwrap()).unwrap();
            json["labels"][OWNER] = "foreign-owner".into();
            private::write(&path, &serde_json::to_vec(&json).unwrap(), false).unwrap();
            assert!(cleanup.created(kind, &name).is_err());
            assert!(h.reconcile().is_err());
            assert!(!std::fs::read_to_string(h.temp.0.join("commands"))
                .unwrap()
                .contains(" rm "));
            assert!(h.temp.0.join(format!("{name}.present")).exists());
        }
    }

    #[test]
    fn orphan_mcp_lock_and_closed_generation_prevent_reentry() {
        let h = Harness::new();
        let lock = private::Lock::acquire(&h.temp.0, "mcp.lock").unwrap();
        assert!(h.reconcile().is_err());
        assert!(!h.temp.0.join("mcp.done").exists());
        drop(lock);
        h.reconcile().unwrap();
        assert!(h.cleanup().admit(&h.generation).is_err());
        let next = private::nonce().unwrap();
        private::write(&h.temp.0.join("active-generation"), next.as_bytes(), false).unwrap();
        Cleanup::prepare(&h.temp.0, &h.config, &next).unwrap();
        assert!(h.cleanup().admit(&h.generation).is_err());
        h.cleanup().admit(&next).unwrap();
        assert!(h.cleanup().admit(&next).is_err());
    }

    #[test]
    fn receipt_commit_failure_is_retryable_without_losing_resources() {
        let h = Harness::new();
        let cleanup = h.cleanup();
        cleanup.admit(&h.generation).unwrap();
        h.resource(&cleanup, ResourceKind::Volume);
        symlink(h.temp.0.join("unrelated"), h.temp.0.join("mcp.done")).unwrap();
        assert!(h.reconcile().is_err());
        assert!(daemon::ensure_clean(&h.temp.0).is_err());
        assert!(!h.temp.0.join("unrelated").exists());
        std::fs::remove_file(h.temp.0.join("mcp.done")).unwrap();
        h.reconcile().unwrap();
        daemon::ensure_clean(&h.temp.0).unwrap();
    }

    #[test]
    fn final_shutdown_receipt_failure_is_retried() {
        let h = Harness::new();
        symlink(h.temp.0.join("unrelated"), h.temp.0.join("shutdown.json")).unwrap();
        assert!(h.reconcile().is_err());
        assert!(!h.temp.0.join("unrelated").exists());
        std::fs::remove_file(h.temp.0.join("shutdown.json")).unwrap();
        h.reconcile().unwrap();
        let receipt: serde_json::Value =
            serde_json::from_slice(&private::read(&h.temp.0.join("shutdown.json"), 4096).unwrap())
                .unwrap();
        assert_eq!(receipt["generation"], h.generation);
        assert_eq!(receipt["success"], true);
    }

    #[test]
    fn definite_create_rejection_is_durable_and_repairable_for_both_resource_kinds() {
        use crate::features::chat_bridge::imp::create::{self, CreateOutcome};
        for kind in [ResourceKind::Container, ResourceKind::Volume] {
            let h = Harness::new();
            let cleanup = h.cleanup();
            cleanup.admit(&h.generation).unwrap();
            let (name, _) = cleanup.register(kind).unwrap();
            let mut command = docker::command(&h.config.docker, &h.config.docker_endpoint);
            command.args([
                if kind == ResourceKind::Container {
                    "container"
                } else {
                    "volume"
                },
                "create",
                &name,
            ]);
            assert_eq!(
                create::run(command, Duration::from_secs(5), kind, &name),
                CreateOutcome::DefinitelyNotCreated
            );
            cleanup.rejected(kind, &name).unwrap();
            assert!(read(&h.temp.0).unwrap().resources[0].state == ResourceState::Rejected);
            // Losing Docker after the rejection must retain the proof for retry.
            private::write(&h.temp.0.join("daemon-down"), b"", false).unwrap();
            assert!(h.reconcile().is_err());
            let journal = read(&h.temp.0).unwrap();
            assert!(journal.resources[0].state == ResourceState::Rejected);
            assert!(journal.phase == Phase::Reconciling);
            assert!(!h.temp.0.join("mcp.done").exists());
            assert!(!h.temp.0.join("shutdown.json").exists());
            assert!(daemon::ensure_clean(&h.temp.0).is_err());
            let pending = report(&h.temp.0).unwrap();
            assert!(pending.contains(if kind == ResourceKind::Container {
                "Pending containers: 1"
            } else {
                "Pending volumes: 1"
            }));
            private::remove(&h.temp.0.join("daemon-down")).unwrap();
            h.reconcile().unwrap();
            let journal = read(&h.temp.0).unwrap();
            assert!(journal.resources[0].state == ResourceState::Removed);
            assert!(journal.phase == Phase::Confirmed);
            assert_eq!(
                private::read(&h.temp.0.join("mcp.done"), 64).unwrap(),
                h.generation.as_bytes()
            );
            let receipt: serde_json::Value = serde_json::from_slice(
                &private::read(&h.temp.0.join("shutdown.json"), 4096).unwrap(),
            )
            .unwrap();
            assert_eq!(receipt["generation"], h.generation);
            assert_eq!(receipt["success"], true);
            daemon::ensure_clean(&h.temp.0).unwrap();
            let report = report(&h.temp.0).unwrap();
            assert!(report.contains("Pending containers: 0"));
            assert!(report.contains("Pending volumes: 0"));
            let commands = std::fs::read_to_string(h.temp.0.join("commands")).unwrap();
            assert!(!commands.lines().any(|line| line.contains(" rm ")));
        }
    }

    #[test]
    fn rejected_create_still_requires_ownership_when_resource_exists() {
        for kind in [ResourceKind::Container, ResourceKind::Volume] {
            let h = Harness::new();
            let cleanup = h.cleanup();
            cleanup.admit(&h.generation).unwrap();
            let name = h.resource(&cleanup, kind);
            // Model a daemon that created the resource before returning an error.
            let mut journal = read(&h.temp.0).unwrap();
            journal.resources[0].state = ResourceState::Rejected;
            write(&h.temp.0, &journal).unwrap();
            let identity_path = h.temp.0.join(format!("{name}.identity"));
            let valid = private::read(&identity_path, 4096).unwrap();
            let mut foreign: serde_json::Value = serde_json::from_slice(&valid).unwrap();
            foreign["labels"][RESOURCE] = serde_json::json!(private::nonce().unwrap());
            private::write(
                &identity_path,
                &serde_json::to_vec(&foreign).unwrap(),
                false,
            )
            .unwrap();
            assert!(h.reconcile().is_err());
            assert!(!h.temp.0.join("mcp.done").exists());
            assert!(!std::fs::read_to_string(h.temp.0.join("commands"))
                .unwrap()
                .contains(" rm "));
            private::write(&identity_path, &valid, false).unwrap();
            h.reconcile().unwrap();
            assert!(!h.temp.0.join(format!("{name}.present")).exists());
            daemon::ensure_clean(&h.temp.0).unwrap();
        }
    }

    #[test]
    fn lost_create_response_retains_intent_and_blocks_receipts_for_both_kinds() {
        use crate::features::chat_bridge::imp::create::{self, CreateOutcome};
        for kind in [ResourceKind::Container, ResourceKind::Volume] {
            let h = Harness::new();
            let cleanup = h.cleanup();
            cleanup.admit(&h.generation).unwrap();
            let (name, _) = cleanup.register(kind).unwrap();
            let mut command = process::command(Path::new("/bin/sh"));
            command.args(["-c", "printf 'error during connect: EOF\\n' >&2; exit 1"]);
            assert_eq!(
                create::run(command, Duration::from_secs(5), kind, &name),
                CreateOutcome::Unknown
            );
            for _ in 0..2 {
                assert!(h.reconcile().is_err());
                let journal = read(&h.temp.0).unwrap();
                assert!(journal.phase == Phase::Reconciling);
                assert!(journal.resources[0].state == ResourceState::Intent);
                assert!(!h.temp.0.join("mcp.done").exists());
                assert!(!h.temp.0.join("shutdown.json").exists());
                assert!(daemon::ensure_clean(&h.temp.0).is_err());
            }
            assert!(!std::fs::read_to_string(h.temp.0.join("commands"))
                .unwrap()
                .contains(" rm "));
        }
    }

    #[test]
    fn daemon_partial_volume_creation_cannot_commit_an_absence_receipt() {
        use crate::features::chat_bridge::imp::create::{self, CreateOutcome};
        let h = Harness::new();
        let cleanup = h.cleanup();
        cleanup.admit(&h.generation).unwrap();
        let (name, _) = cleanup.register(ResourceKind::Volume).unwrap();
        // Moby local Root.Create can leave an unlisted directory on data-mkdir
        // failure. It can reappear as a volume after daemon restart.
        let message = format!("Error response from daemon: create {name}: error while creating volume data path: no space left on device");
        let mut command = process::command(Path::new("/bin/sh"));
        command.args([
            "-c",
            "printf '%s\\n' \"$1\" >&2; exit 1",
            "fixture",
            &message,
        ]);
        assert_eq!(
            create::run(command, Duration::from_secs(5), ResourceKind::Volume, &name),
            CreateOutcome::Unknown
        );
        assert!(h.reconcile().is_err());
        assert!(read(&h.temp.0).unwrap().resources[0].state == ResourceState::Intent);
        assert!(!h.temp.0.join("mcp.done").exists());
        assert!(!h.temp.0.join("shutdown.json").exists());
        assert!(daemon::ensure_clean(&h.temp.0).is_err());
        assert!(report(&h.temp.0).unwrap().contains("Pending volumes: 1"));
        assert!(!std::fs::read_to_string(h.temp.0.join("commands"))
            .unwrap()
            .contains(" rm "));
    }

    #[test]
    fn uncertain_create_is_not_mistaken_for_absence() {
        let h = Harness::new();
        let cleanup = h.cleanup();
        cleanup.admit(&h.generation).unwrap();
        cleanup.register(ResourceKind::Volume).unwrap();
        assert!(h.reconcile().is_err());
        assert!(!h.temp.0.join("mcp.done").exists());
    }

    #[test]
    fn interrupted_activation_is_recovered_before_any_child_can_start() {
        for step in ["prepared", "active", "journal"] {
            let h = Harness::new();
            h.reconcile().unwrap();
            let next = private::nonce().unwrap();
            let mut prepared = read(&h.temp.0).unwrap();
            prepared.generation = next.clone();
            prepared.phase = Phase::Open;
            private::write(
                &h.temp.0.join(PREPARED),
                &serde_json::to_vec(&prepared).unwrap(),
                false,
            )
            .unwrap();
            if step != "prepared" {
                private::write(&h.temp.0.join("active-generation"), next.as_bytes(), false)
                    .unwrap();
            }
            if step == "journal" {
                write(&h.temp.0, &prepared).unwrap();
            }
            assert!(daemon::ensure_clean(&h.temp.0).is_err());
            let text = report(&h.temp.0).unwrap();
            assert!(text.contains("activation interrupted"));
            assert!(text.contains("Recovery: zai chatgpt repair"));
            h.reconcile().unwrap();
            daemon::ensure_clean(&h.temp.0).unwrap();
            assert_eq!(active(&h.temp.0).unwrap(), next);
            assert!(!h.temp.0.join(PREPARED).exists());
            assert!(!h.temp.0.join("commands").exists());
        }
    }
}
