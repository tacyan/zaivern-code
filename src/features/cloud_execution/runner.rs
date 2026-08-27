//! **仕事を 1 本走らせる** — 用意 → 実行 → 持ち帰り → 片付け。
//!
//! ## ここが持ってよい状態 (§35)
//!
//! [`ExecutionJobState`] だけ。**エージェントが考え中か / 承認待ちかは持たない。**
//! それは既存の [`crate::supervisor`] が唯一の持ち主で、ここに 2 つ目の
//! 状態機械を作ると、どちらが正しいかを誰も言えなくなる。
//!
//! ## 途中で失敗しても枠を返す
//!
//! 枠 (`active_jobs`) は [`SlotGuard`] が握る。どこで早期 return しても
//! Drop で必ず返るので、失敗のたびに実行先が痩せていくことがない。
//!
//! ## 持ち帰れなかったら片付けない (§30)
//!
//! ディスクの節約よりデータを失わないほうが大事。片付けるのは
//! **結果を手元に持てたときだけ**。

use std::path::PathBuf;
use std::time::Duration;

use super::command::LaunchSpec;
use super::git_workspace::{self, RemoteWorkspace};
use super::model::{
    ids, CloudError, EventSink, ExecutionJob, ExecutionJobState, ExecutionTarget,
    JobId, TransportKind,
};
use super::registry::SlotGuard;
use super::store;
use super::transport::{self, ssh::SshOptions};

/// 1 本の仕事の指定。
#[derive(Debug, Clone)]
pub struct JobSpec {
    pub target: ExecutionTarget,
    pub launch: LaunchSpec,
    /// 手元のリポジトリ (git で持ち帰るときだけ要る)。
    pub local_repo: Option<PathBuf>,
    /// ワークスペースキー ([`crate::history::workspace_key`])。
    pub workspace_key: String,
    /// リモートに worktree を作って、そこで走らせるか。
    ///
    /// `false` なら単発の実行 (`zai cloud exec`)。`true` なら
    /// 用意 → 実行 → 持ち帰り → 片付け (`zai cloud job run`)。
    pub isolated: bool,
    pub timeout: Duration,
}

/// 出力を受けながら仕事を 1 本走らせる。
///
/// **呼んだスレッドで最後まで走る。** UI から呼ぶときは [`spawn`] を使う。
pub fn run(spec: &JobSpec, sink: &mut dyn EventSink) -> Result<ExecutionJob, CloudError> {
    let job_id = JobId::new(ids::new_id("j-"));
    let mut job = ExecutionJob {
        id: job_id.clone(),
        target: spec.target.id.clone(),
        state: ExecutionJobState::Queued,
        command: spec.launch.display(),
        workspace: None,
        result_ref: String::new(),
        started_unix: store::now_unix(),
        ended_unix: 0,
        exit_code: None,
        message: String::new(),
    };

    // **枠を先に取る。** 取れなければ何も始めない (載せてから断ると、
    // 用意だけ済んだ worktree が残る)。手元の実行先は台帳に載っていないので
    // 枠も数えない。
    let _slot = if spec.target.transport == TransportKind::Local {
        SlotGuard::none(&spec.target.id)
    } else {
        match SlotGuard::claim(&spec.target.id) {
            Ok(g) => g,
            Err(e) => {
                job.state = ExecutionJobState::Failed;
                job.message = e.to_string();
                job.ended_unix = store::now_unix();
                let _ = store::upsert_job(&job);
                return Err(e);
            }
        }
    };

    let outcome = run_inner(spec, &mut job, sink);
    job.ended_unix = store::now_unix();
    match &outcome {
        Ok(()) => {
            job.state = if job.exit_code == Some(0) {
                ExecutionJobState::Succeeded
            } else {
                ExecutionJobState::Failed
            };
        }
        Err(e) => {
            job.state = ExecutionJobState::Failed;
            job.message = e.to_string();
        }
    }
    // 記録は残す (失敗したことも記録の一部)
    let _ = store::upsert_job(&job);
    outcome.map(|()| job)
}

fn run_inner(
    spec: &JobSpec,
    job: &mut ExecutionJob,
    sink: &mut dyn EventSink,
) -> Result<(), CloudError> {
    let transport = transport::for_target(&spec.target, spec.timeout);
    let opts = ssh_options(spec.timeout);

    if !spec.isolated {
        // 単発。用意も持ち帰りもしない。
        job.state = ExecutionJobState::Running;
        let _ = store::upsert_job(job);
        let mut req = spec.launch.to_request();
        req.timeout = Some(spec.timeout);
        let r = transport.exec(&spec.target, &req, sink)?;
        job.exit_code = r.exit_code;
        return Ok(());
    }

    let local_repo = spec.local_repo.clone().ok_or_else(|| {
        CloudError::config("分離した作業場で走らせるには git リポジトリの中で実行してください")
    })?;
    let ws = RemoteWorkspace::new(&spec.workspace_key, job.id.as_str())?;
    job.workspace = Some(ws.dir.as_str().to_string());

    job.state = ExecutionJobState::Preparing;
    let _ = store::upsert_job(job);
    git_workspace::prepare(
        transport.as_ref(),
        &spec.target,
        &local_repo,
        &ws,
        &opts,
        spec.timeout,
    )?;

    job.state = ExecutionJobState::Running;
    let _ = store::upsert_job(job);
    let mut req = spec.launch.to_request();
    // **仕事は自分の worktree の中で走る。** 呼び出し側が cwd を指定していても
    // ここで上書きする (分離が指定の取り違えで崩れないようにする)。
    req.cwd = Some(ws.dir.as_str().to_string());
    req.timeout = Some(spec.timeout);
    let r = transport.exec(&spec.target, &req, sink)?;
    job.exit_code = r.exit_code;

    // **実行が失敗しても持ち帰る。** 失敗したときこそ、何が起きたかが
    // 手元で見たいものになる。
    let (result_ref, snapshotted) = git_workspace::collect(
        transport.as_ref(),
        &spec.target,
        &local_repo,
        &ws,
        &opts,
        spec.timeout,
    )?;
    job.result_ref = result_ref;
    if snapshotted {
        job.message = "未コミットの変更を輸送用コミットにして持ち帰りました".to_string();
    }

    // ここまで来た = 手元に結果がある。**このときだけ片付ける** (§30)。
    if let Err(e) = git_workspace::cleanup(transport.as_ref(), &spec.target, &ws) {
        // 片付けの失敗で仕事を失敗にしない (結果はもう手元にある)
        job.message = format!("{} / 片付けに失敗: {e}", job.message);
    }
    Ok(())
}

/// SSH の呼び方 (仕事から使う分)。
pub fn ssh_options(timeout: Duration) -> SshOptions {
    SshOptions {
        connect_timeout_secs: timeout.as_secs().clamp(5, 120),
        ..SshOptions::default()
    }
}

/// 単発で実行して、出力と終了コードを返す (`zai cloud exec`)。
pub fn exec_once(
    target: &ExecutionTarget,
    launch: &LaunchSpec,
    timeout: Duration,
    sink: &mut dyn EventSink,
) -> Result<ExecutionJob, CloudError> {
    run(
        &JobSpec {
            target: target.clone(),
            launch: launch.clone(),
            local_repo: None,
            workspace_key: String::new(),
            isolated: false,
            timeout,
        },
        sink,
    )
}

/// 記録を新しい順に返す。
pub fn recent_jobs(limit: usize) -> Result<Vec<ExecutionJob>, CloudError> {
    let mut jobs = store::load_jobs()?;
    jobs.reverse();
    jobs.truncate(limit);
    Ok(jobs)
}

/// 走ったままになっている仕事を数える (`doctor` 用)。
pub fn unfinished(jobs: &[ExecutionJob]) -> Vec<&ExecutionJob> {
    jobs.iter().filter(|j| !j.state.is_final()).collect()
}

/// 出力を溜めながら走らせる。
///
/// **製品の経路は使わない** (CLI は標準出力へ素通しする [`run`] を呼ぶ) —
/// これは「走らせた結果を全部見たい」試験のための入口。
#[cfg(test)]
pub fn run_collecting(
    spec: &JobSpec,
) -> (Result<ExecutionJob, CloudError>, super::model::CollectSink) {
    let mut sink = super::model::CollectSink::default();
    let out = run(spec, &mut sink);
    (out, sink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::cloud_execution::model::ExecutionTarget;
    use crate::features::cloud_execution::provider::static_ssh::{make_target, SshTargetSpec};
    use crate::features::cloud_execution::test_support::home_guard;

    fn local_spec(program: &str, args: Vec<String>) -> JobSpec {
        JobSpec {
            target: ExecutionTarget::local(2),
            launch: LaunchSpec::new(program, args),
            local_repo: None,
            workspace_key: String::new(),
            isolated: false,
            timeout: Duration::from_secs(30),
        }
    }

    fn echo(text: &str) -> JobSpec {
        if cfg!(windows) {
            local_spec("cmd", vec!["/C".into(), format!("echo {text}")])
        } else {
            local_spec("echo", vec![text.into()])
        }
    }

    #[test]
    fn 単発の実行は記録に残る() {
        let _home = home_guard("runner-once");
        let (out, sink) = run_collecting(&echo("hello"));
        let job = out.expect("走る");
        assert_eq!(job.state, ExecutionJobState::Succeeded);
        assert_eq!(job.exit_code, Some(0));
        assert!(sink.stdout_text().contains("hello"));

        let jobs = recent_jobs(10).expect("読める");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job.id);
        // 走らせた中身が 1 行で残る
        assert!(jobs[0].command.contains("hello"), "{}", jobs[0].command);
    }

    #[test]
    fn 失敗も記録に残る() {
        let _home = home_guard("runner-fail");
        let spec = if cfg!(windows) {
            local_spec("cmd", vec!["/C".into(), "exit 7".into()])
        } else {
            local_spec("sh", vec!["-c".into(), "exit 7".into()])
        };
        let (out, _) = run_collecting(&spec);
        let job = out.expect("走る (終了コードは 7)");
        assert_eq!(job.exit_code, Some(7));
        assert_eq!(job.state, ExecutionJobState::Failed);
        assert_eq!(recent_jobs(10).expect("読める").len(), 1);
    }

    #[test]
    fn 枠が無ければ何も始めない() {
        let _home = home_guard("runner-no-capacity");
        let t = make_target(&SshTargetSpec {
            name: "dev-01".into(),
            host: "example.com".into(),
            user: "zaivern".into(),
            max_jobs: 1,
            ..SshTargetSpec::default()
        })
        .expect("組める");
        store::save_targets(std::slice::from_ref(&t)).expect("書ける");
        // 1 枠を先に埋める
        let _held = SlotGuard::claim(&t.id).expect("取れる");

        let spec = JobSpec {
            target: t.clone(),
            launch: LaunchSpec::new("true", vec![]),
            local_repo: None,
            workspace_key: String::new(),
            isolated: false,
            timeout: Duration::from_secs(5),
        };
        let (out, sink) = run_collecting(&spec);
        let e = out.expect_err("断る");
        assert!(matches!(e, CloudError::NoCapacity(_)), "{e:?}");
        // **ssh を 1 度も呼んでいない** (呼んでいたら出力か遅延が出る)
        assert!(sink.stdout_text().is_empty());
        // 断ったことも記録に残る
        let jobs = recent_jobs(10).expect("読める");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, ExecutionJobState::Failed);
    }

    #[test]
    fn 途中で失敗しても枠は返る() {
        let _home = home_guard("runner-slot-returned");
        let t = make_target(&SshTargetSpec {
            name: "dev-01".into(),
            host: "example.com".into(),
            user: "zaivern".into(),
            max_jobs: 1,
            ..SshTargetSpec::default()
        })
        .expect("組める");
        store::save_targets(std::slice::from_ref(&t)).expect("書ける");

        // 分離を頼むがリポジトリを渡さない → 途中で失敗する
        let spec = JobSpec {
            target: t.clone(),
            launch: LaunchSpec::new("true", vec![]),
            local_repo: None,
            workspace_key: "0123456789abcdef".into(),
            isolated: true,
            timeout: Duration::from_secs(5),
        };
        let (out, _) = run_collecting(&spec);
        assert!(out.is_err());
        // 枠が返っていること (返らないと実行先が 1 回ごとに痩せる)
        let after = store::load_targets().expect("読める");
        assert_eq!(after[0].capacity.active_jobs, 0);
    }

    #[test]
    fn 別スレッドから走らせても置き場を共有する() {
        let _home = home_guard("runner-thread");
        let dir = store::cloud_dir();
        let spec = echo("via-thread");
        let handle = std::thread::spawn(move || {
            // 置き場の差し替えはスレッドごとなので、走らせる側でも指し直す
            store::set_test_dir(Some(dir));
            run_collecting(&spec)
        });
        let (out, sink) = handle.join().expect("終わる");
        assert!(out.is_ok(), "{out:?}");
        assert!(sink.stdout_text().contains("via-thread"), "{}", sink.stdout_text());
        assert_eq!(recent_jobs(10).expect("読める").len(), 1);
    }

    #[test]
    fn 走ったままの仕事を数える() {
        let jobs = vec![
            ExecutionJob {
                id: JobId::new("a"),
                target: crate::features::cloud_execution::model::TargetId::new("t"),
                state: ExecutionJobState::Running,
                command: String::new(),
                workspace: None,
                result_ref: String::new(),
                started_unix: 0,
                ended_unix: 0,
                exit_code: None,
                message: String::new(),
            },
            ExecutionJob {
                id: JobId::new("b"),
                target: crate::features::cloud_execution::model::TargetId::new("t"),
                state: ExecutionJobState::Succeeded,
                command: String::new(),
                workspace: None,
                result_ref: String::new(),
                started_unix: 0,
                ended_unix: 0,
                exit_code: Some(0),
                message: String::new(),
            },
        ];
        assert_eq!(unfinished(&jobs).len(), 1);
        assert_eq!(unfinished(&jobs)[0].id.as_str(), "a");
    }

    /// **エージェントの状態機械を作っていないこと**を固定する (§35)。
    #[test]
    fn エージェントの状態を持たない() {
        let src = include_str!("runner.rs").replace("\r\n", "\n");
        let body = src.split("#[cfg(test)]").next().unwrap_or_default();
        for banned in [
            "Thinking",
            "Editing",
            "WaitingApproval",
            "Reviewing",
            "Approval",
        ] {
            assert!(
                !body.contains(banned),
                "{banned} を持っている。エージェントの状態は既存の Supervisor の担当で、\n\
                 ここに 2 つ目の状態機械を作らない"
            );
        }
        // 持ってよいのは基盤の状態だけ
        assert!(body.contains("ExecutionJobState"));
    }
}
