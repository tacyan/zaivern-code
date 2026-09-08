//! A publication owns its lease and durable result independently of the UI.
use super::super::integration::{self, Outcome};
use super::{IntegrationHold, RunOwner, TeamTask};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};

pub(super) struct Report {
    pub outcome: Outcome,
    pub error: Option<String>,
}

pub(super) struct Job {
    pub owner: RunOwner,
    pub task: super::TaskId,
    pub attempts: u8,
    pub dispatch_seq: u32,
    pub cancel: Arc<AtomicBool>,
    pub rx: mpsc::Receiver<Report>,
    pub ready: Option<Report>,
}

impl Drop for Job {
    fn drop(&mut self) {
        // No join, no lease release: the publisher still owns the Permit.
        self.cancel.store(true, Ordering::Release);
    }
}

impl Job {
    pub fn start(
        hold: IntegrationHold,
        task: &TeamTask,
        owner: RunOwner,
        #[cfg(test)] hook: Option<Box<dyn FnOnce() + Send>>,
    ) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let source = owner.workspace.clone();
        let files = task.files.clone();
        let identity = serde_json::json!({"run_id": owner.run_id, "source_workspace": owner.source_workspace,
            "workspace": source, "task": task.id, "attempts": task.attempts,
            "dispatch_seq": task.dispatch_seq, "session": hold.session});
        std::thread::Builder::new()
            .name("team-publication".into())
            .spawn(move || {
                let mut outcome = Outcome::default();
                let mut receipt = None;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    receipt = Some(integration::record_worker_start(&hold.permit, &identity)?);
                    #[cfg(test)]
                    if let Some(hook) = hook {
                        hook();
                    }
                    integration::publish_cancellable(
                        &hold.permit,
                        &source,
                        &files,
                        &stop,
                        &mut outcome,
                    )
                }));
                let mut error = match result {
                    Ok(result) => result.err(),
                    Err(_) => Some(
                        "統合ワーカーが異常終了しました。反映済み内容と退避記録を確認してください"
                            .into(),
                    ),
                };
                if let Some(directory) = receipt {
                    outcome
                        .notes
                        .push(format!("統合結果の記録: {}", directory.display()));
                    if let Err(why) = integration::record_worker_result(
                        &hold.permit,
                        &directory,
                        &outcome,
                        &error,
                    ) {
                        error = Some(format!(
                            "統合結果の保存に失敗しました: {why}; {}",
                            error.unwrap_or_default()
                        ));
                    }
                }
                // Sending a result proves all source I/O and lease release finished.
                // A disconnected receiver only discards delivery, not the durable receipt.
                drop(hold.permit);
                let _ = tx.send(Report { outcome, error });
            })
            .map_err(|e| format!("統合ワーカーを開始できません: {e}"))?;
        Ok(Self {
            owner,
            task: task.id,
            attempts: task.attempts,
            dispatch_seq: task.dispatch_seq,
            cancel,
            rx,
            ready: None,
        })
    }
}
