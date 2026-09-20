#[cfg(test)]
use super::workspace;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// One worker avoids competing writes; completed records are retained up to this
// bound. Reject admission instead of silently discarding a client's task ID.
const MAX_TASKS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum State {
    Queued,
    Running,
    WaitingApproval,
    Completed,
    Failed,
    Cancelled,
}
impl State {
    fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct Status {
    pub task_id: String,
    pub state: State,
    pub summary: String,
    pub progress: String,
    pub changed_files: Vec<String>,
    pub test_status: String,
    pub build_status: String,
    pub diff_summary: String,
    pub error: Option<String>,
    pub duration_ms: u64,
    pub cancellation_requested: bool,
}

pub(super) trait TaskStore: Send + Sync {
    fn insert(&self, status: Status) -> Result<(), String>;
    fn get(&self, id: &str) -> Result<Status, String>;
    fn update(&self, status: Status);
    fn request_cancel(&self, id: &str);
}
#[derive(Default)]
pub(super) struct InMemoryTaskStore(Mutex<BTreeMap<String, Status>>);
impl TaskStore for InMemoryTaskStore {
    fn insert(&self, status: Status) -> Result<(), String> {
        let mut tasks = self.0.lock().map_err(|_| "task store unavailable")?;
        if tasks.len() >= MAX_TASKS {
            return Err("task capacity reached; restart after collecting results".into());
        }
        tasks.insert(status.task_id.clone(), status);
        Ok(())
    }
    fn get(&self, id: &str) -> Result<Status, String> {
        self.0
            .lock()
            .map_err(|_| "task store unavailable")?
            .get(id)
            .cloned()
            .ok_or_else(|| "unknown task_id".into())
    }
    fn update(&self, status: Status) {
        if let Ok(mut tasks) = self.0.lock() {
            let mut status = status;
            status.cancellation_requested |= tasks
                .get(&status.task_id)
                .is_some_and(|s| s.cancellation_requested);
            tasks.insert(status.task_id.clone(), status);
        }
    }
    fn request_cancel(&self, id: &str) {
        if let Ok(mut tasks) = self.0.lock() {
            if let Some(status) = tasks.get_mut(id) {
                status.cancellation_requested = true;
            }
        }
    }
}

pub(super) struct Outcome {
    pub summary: String,
    pub changed_files: Vec<String>,
    pub diff_summary: String,
    pub test_status: String,
    pub build_status: String,
    pub error: Option<String>,
}
/// Transport-independent adapter: remote execution need not access a local path.
pub(super) trait TaskExecutor: Send + Sync {
    fn execute(
        &self,
        root: &std::path::Path,
        instruction: &str,
        control: &Control,
    ) -> Result<Outcome, String>;
}
pub(super) struct Control {
    pub cancelled: Arc<AtomicBool>,
    store: Arc<dyn TaskStore>,
    import_gate: Arc<Mutex<()>>,
    id: String,
}
impl Control {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    /// Linearization point: cancellation accepted before this gate prevents import.
    /// Once import owns the gate it completes and reports its actual result.
    pub fn import<T>(&self, apply: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let _guard = self
            .import_gate
            .lock()
            .map_err(|_| "import gate unavailable")?;
        if self.is_cancelled() {
            return Err("cancelled".into());
        }
        apply()
    }
    pub fn state(&self, state: State, progress: &str) {
        if let Ok(mut status) = self.store.get(&self.id) {
            status.state = state;
            status.progress = progress.to_string();
            self.store.update(status);
        }
    }
}

struct Worker {
    task_id: String,
    import_gate: Arc<Mutex<()>>,
    cancel: Arc<AtomicBool>,
    join: std::thread::JoinHandle<()>,
}
pub(super) struct ChatBridge {
    root: PathBuf,
    store: Arc<dyn TaskStore>,
    executor: Arc<dyn TaskExecutor>,
    worker: Mutex<Option<Worker>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    instruction: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskId {
    task_id: String,
}

impl ChatBridge {
    pub fn new(root: PathBuf, executor: Arc<dyn TaskExecutor>) -> Self {
        Self {
            root,
            store: Arc::new(InMemoryTaskStore::default()),
            executor,
            worker: Mutex::new(None),
        }
    }
    pub fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        match name {
            "zaivern_run_task" => {
                let args: Run =
                    serde_json::from_value(args).map_err(|_| "invalid run_task arguments")?;
                if args.instruction.trim().is_empty() || args.instruction.chars().count() > 16384 {
                    return Err("instruction must be 1..16384 characters".into());
                }
                let mut worker = self.worker.lock().map_err(|_| "worker unavailable")?;
                if worker.as_ref().is_some_and(|w| !w.join.is_finished()) {
                    return Err("another task is active".into());
                }
                if let Some(old) = worker.take() {
                    let _ = old.join.join();
                }
                let id = crate::features::cloud_execution::model::ids::new_id("mcp-");
                let status = Status {
                    task_id: id.clone(),
                    state: State::Queued,
                    summary: String::new(),
                    progress: "queued".into(),
                    changed_files: vec![],
                    test_status: "not_verified".into(),
                    build_status: "not_verified".into(),
                    diff_summary: String::new(),
                    error: None,
                    duration_ms: 0,
                    cancellation_requested: false,
                };
                self.store.insert(status)?;
                let cancel = Arc::new(AtomicBool::new(false));
                let import_gate = Arc::new(Mutex::new(()));
                let control = Control {
                    import_gate: import_gate.clone(),
                    cancelled: cancel.clone(),
                    store: self.store.clone(),
                    id: id.clone(),
                };
                let executor = self.executor.clone();
                let root = self.root.clone();
                let join = std::thread::Builder::new()
                    .name("mcp-task".into())
                    .spawn(move || {
                        let started = Instant::now();
                        control.state(State::Running, "executing");
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            executor.execute(&root, &args.instruction, &control)
                        }));
                        if let Ok(mut status) = control.store.get(&control.id) {
                            status.duration_ms =
                                started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                            match result {
                                Ok(Ok(outcome)) => {
                                    status.state = if outcome.error.is_some()
                                        || outcome.test_status == "failed"
                                        || outcome.build_status == "failed"
                                    {
                                        State::Failed
                                    } else {
                                        State::Completed
                                    };
                                    status.summary = outcome.summary;
                                    status.changed_files = outcome.changed_files;
                                    status.diff_summary = outcome.diff_summary;
                                    status.test_status = outcome.test_status;
                                    status.build_status = outcome.build_status;
                                    status.error = outcome.error;
                                }
                                Ok(Err(ref error))
                                    if control.is_cancelled() && error == "cancelled" =>
                                {
                                    status.state = State::Cancelled;
                                }
                                Ok(Err(e)) => {
                                    status.state = State::Failed;
                                    status.error = Some(e);
                                }
                                Err(_) => {
                                    status.state = State::Failed;
                                    status.error = Some("executor panicked".into());
                                }
                            }
                            status.progress = "finished".into();
                            // No instruction, agent output, file content, or credential in logs.
                            eprintln!(
                                "mcp task_id={} workspace={:?} state={:?} duration_ms={} error={}",
                                status.task_id,
                                root,
                                status.state,
                                status.duration_ms,
                                status.error.is_some()
                            );
                            control.store.update(status);
                        }
                    })
                    .map_err(|_| {
                        if let Ok(mut status) = self.store.get(&id) {
                            status.state = State::Failed;
                            status.error = Some("cannot start task worker".into());
                            self.store.update(status);
                        }
                        "cannot start task worker"
                    })?;
                *worker = Some(Worker {
                    task_id: id.clone(),
                    import_gate,
                    cancel,
                    join,
                });
                eprintln!("mcp tool=zaivern_run_task task_id={id} state=queued");
                Ok(json!({"task_id":id}))
            }
            "zaivern_task_status" | "zaivern_cancel_task" => {
                let args: TaskId =
                    serde_json::from_value(args).map_err(|_| "invalid task_id arguments")?;
                let mut status = self.store.get(&args.task_id)?;
                eprintln!(
                    "mcp tool={name} task_id={} state={:?}",
                    status.task_id, status.state
                );
                if name == "zaivern_cancel_task" && !status.state.terminal() {
                    if let Some(worker) = self
                        .worker
                        .lock()
                        .map_err(|_| "worker unavailable")?
                        .as_ref()
                    {
                        if worker.task_id != args.task_id {
                            return serde_json::to_value(self.store.get(&args.task_id)?)
                                .map_err(|_| "cannot serialize status".into());
                        }
                        let _guard = worker
                            .import_gate
                            .lock()
                            .map_err(|_| "import gate unavailable")?;
                        status = self.store.get(&args.task_id)?;
                        if status.state.terminal() {
                            return serde_json::to_value(status)
                                .map_err(|_| "cannot serialize status".into());
                        }
                        worker.cancel.store(true, Ordering::Release);
                        status.cancellation_requested = true;
                        self.store.request_cancel(&args.task_id);
                    }
                }
                serde_json::to_value(status).map_err(|_| "cannot serialize status".into())
            }
            _ => Err("unknown tool".into()),
        }
    }
}
impl Drop for ChatBridge {
    fn drop(&mut self) {
        if let Ok(worker) = self.worker.get_mut() {
            if let Some(worker) = worker.take() {
                worker.cancel.store(true, Ordering::Release);
                let _ = worker.join.join();
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    pub struct Fake;
    impl TaskExecutor for Fake {
        fn execute(
            &self,
            _: &std::path::Path,
            instruction: &str,
            control: &Control,
        ) -> Result<Outcome, String> {
            if instruction == "wait" {
                control.state(State::WaitingApproval, "approval unavailable");
                while !control.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                return Err("cancelled".into());
            }
            if instruction == "executor_error" {
                return Err("executor failed".into());
            }
            if instruction == "failed_after_cancel" {
                control.state(State::Running, "verification running");
                while !control.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            Ok(Outcome {
                error: (instruction == "import_error").then(|| "import failed".into()),
                summary: "mock".into(),
                changed_files: vec![],
                diff_summary: String::new(),
                test_status: match instruction {
                    "passed" => "passed",
                    "failed" | "failed_after_cancel" => "failed",
                    _ => "not_verified",
                }
                .into(),
                build_status: if instruction == "build_failed" {
                    "failed"
                } else {
                    "not_verified"
                }
                .into(),
            })
        }
    }
    #[test]
    fn instruction_limits_count_unicode_characters() {
        for (unit, count, accepted) in [
            ("a", 16384, true),
            ("a", 16385, false),
            ("あ", 10000, true),
            ("あ", 16384, true),
            ("あ", 16385, false),
            ("🦀", 16384, true),
            ("🦀", 16385, false),
        ] {
            let bridge = ChatBridge::new(PathBuf::from("unused"), Arc::new(Fake));
            let result = bridge.call(
                "zaivern_run_task",
                json!({"instruction":unit.repeat(count)}),
            );
            assert_eq!(result.is_ok(), accepted, "{unit} × {count}: {result:?}");
            if !accepted {
                assert_eq!(
                    result.unwrap_err(),
                    "instruction must be 1..16384 characters"
                );
            }
        }
        let bridge = ChatBridge::new(PathBuf::from("unused"), Arc::new(Fake));
        assert!(bridge
            .call("zaivern_run_task", json!({"instruction":"　 \n"}))
            .is_err());
    }

    #[test]
    fn run_without_workspace_uses_only_server_root() {
        struct Recording(Arc<Mutex<Option<PathBuf>>>);
        impl TaskExecutor for Recording {
            fn execute(
                &self,
                root: &std::path::Path,
                instruction: &str,
                control: &Control,
            ) -> Result<Outcome, String> {
                *self.0.lock().unwrap() = Some(root.to_owned());
                Fake.execute(root, instruction, control)
            }
        }
        let root = crate::test_util::unique_temp_dir("bridge", "fixed-root");
        let observed = Arc::new(Mutex::new(None));
        let bridge = ChatBridge::new(root.clone(), Arc::new(Recording(observed.clone())));
        assert!(bridge
            .call(
                "zaivern_run_task",
                json!({"instruction":"finish", "workspace":root})
            )
            .is_err());
        let id = bridge
            .call("zaivern_run_task", json!({"instruction":"finish"}))
            .unwrap();
        assert!(id["task_id"].is_string());
        bridge
            .worker
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join
            .join()
            .unwrap();
        assert_eq!(*observed.lock().unwrap(), Some(root.clone()));
        drop(bridge);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_before_import_never_calls_apply() {
        let control = Control {
            cancelled: Arc::new(AtomicBool::new(true)),
            import_gate: Arc::new(Mutex::new(())),
            store: Arc::new(InMemoryTaskStore::default()),
            id: "cancelled".into(),
        };
        assert_eq!(
            control
                .import::<()>(|| panic!("must not import"))
                .unwrap_err(),
            "cancelled"
        );
        control.cancelled.store(false, Ordering::Release);
        assert_eq!(control.import(|| Ok(42)).unwrap(), 42);
    }

    #[test]
    fn stale_task_cannot_cancel_another_worker() {
        let bridge = ChatBridge::new(PathBuf::from("unused"), Arc::new(Fake));
        let old = bridge
            .call("zaivern_run_task", json!({"instruction":"finish"}))
            .unwrap();
        bridge
            .worker
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join
            .join()
            .unwrap();
        let active = bridge
            .call("zaivern_run_task", json!({"instruction":"wait"}))
            .unwrap();
        // Model a stale nonterminal read racing replacement by a new worker.
        let mut stale = bridge.store.get(old["task_id"].as_str().unwrap()).unwrap();
        stale.state = State::Running;
        bridge.store.update(stale);
        bridge.call("zaivern_cancel_task", old).unwrap();
        assert!(!bridge
            .worker
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .cancel
            .load(Ordering::Acquire));
        bridge.call("zaivern_cancel_task", active).unwrap();
    }

    #[test]
    fn lifecycle_and_cancel() {
        let root = crate::test_util::unique_temp_dir("bridge", "lifecycle");
        std::fs::create_dir_all(&root).unwrap();
        let root = workspace::validate_root(&root).unwrap();
        let bridge = ChatBridge::new(root.clone(), Arc::new(Fake));
        for (instruction, terminal) in [
            ("finish", "completed"),
            ("passed", "completed"),
            ("failed", "failed"),
            ("failed_after_cancel", "failed"),
            ("build_failed", "failed"),
            ("executor_error", "failed"),
            ("import_error", "failed"),
            ("wait", "cancelled"),
        ] {
            let id = bridge
                .call("zaivern_run_task", json!({"instruction":instruction}))
                .unwrap();
            if instruction == "wait" || instruction == "failed_after_cancel" {
                let deadline = Instant::now() + std::time::Duration::from_secs(2);
                while bridge.call("zaivern_task_status", id.clone()).unwrap()["state"] == "queued" {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                bridge.call("zaivern_cancel_task", id.clone()).unwrap();
            }
            let deadline = Instant::now() + std::time::Duration::from_secs(2);
            loop {
                let status = bridge.call("zaivern_task_status", id.clone()).unwrap();
                if status["state"] == terminal {
                    let cancelled = bridge.call("zaivern_cancel_task", id.clone()).unwrap();
                    assert_eq!(cancelled["state"], terminal);
                    assert_eq!(
                        bridge.call("zaivern_task_status", id.clone()).unwrap()["state"],
                        terminal
                    );
                    break;
                }
                assert!(Instant::now() < deadline, "{status}");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            // Terminal status is stored just before the worker returns. Join
            // here so the next case does not race the single-worker admission.
            let worker = bridge.worker.lock().unwrap().take().unwrap();
            worker.join.join().unwrap();
        }
        drop(bridge);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_unknown_ids_arguments_and_unapproved_workspaces() {
        let root = crate::test_util::unique_temp_dir("bridge", "validation");
        std::fs::create_dir_all(&root).unwrap();
        let root = workspace::validate_root(&root).unwrap();
        let bridge = ChatBridge::new(root.clone(), Arc::new(Fake));
        for path in [
            root.clone(),
            root.join("missing"),
            root.join("../escape"),
            root.parent().unwrap().to_path_buf(),
        ] {
            assert!(bridge
                .call(
                    "zaivern_run_task",
                    json!({"instruction":"edit","workspace":path})
                )
                .is_err());
        }
        assert!(bridge
            .call(
                "zaivern_run_task",
                json!({"instruction":"edit","workspace":root,"command":"sudo rm -rf /"})
            )
            .is_err());
        for name in ["zaivern_task_status", "zaivern_cancel_task"] {
            assert!(bridge.call(name, json!({"task_id":"missing"})).is_err());
        }
        drop(bridge);
        std::fs::remove_dir_all(root).unwrap();
    }
}
