//! `zai team <sub>` — CLI の入口。
//!
//! ## 何をする層か
//!
//! * `plan`   — 計画を作って表示する。**エージェントは 1 体も起こさない**
//! * `run`    — GUI を起動し、Team 画面を開いて Plan Preview まで進める
//! * `status` — 保存された状態を読んで出す
//! * `resume` — 未完了 Run を GUI で開き直す
//! * `stop`   — 新規割り当てを止める (kill は GUI 側の承認ゲート)
//! * `reset`  — 消す対象を出してから、明示確認の上で消す
//!
//! `plan` / `status` は `--json` を持つ。
//!
//! ## run はヘッドレスではない
//!
//! `zai team run SPEC.md --agents 4` は**既存の GUI を起動する**。
//! 既に動いているインスタンスがあれば二重起動せず、投函箱
//! ([`super::launch`]) 越しにそちらへ渡す。

use std::path::{Path, PathBuf};

use super::graph::{self, PhaseStatus};
use super::launch;
use super::model::*;
use super::persistence::{self, LoadOutcome};
use super::plan_schema::TeamPlan;
use super::planner::{PlanInput, StaticPlanner, TeamPlanner};

const EXIT_OK: i32 = 0;
const EXIT_ERR: i32 = 1;
/// MVP で未対応の指定 (`--headless`)。**黙って無視しない。**
const EXIT_UNSUPPORTED: i32 = 2;

pub const HELP: &str = "\
zai team — SPEC を渡して AI 開発チームを動かす

  zai team plan <SPEC.md> [--agents N] [--json]
        SPEC を解析して計画を表示する (エージェントは起動しない)
  zai team run <SPEC.md> [--agents N] [--yes]
        Zaivern を起動し、Team 画面で Plan Preview を開く
        --yes は Start Team の確認だけを省く (権限昇格・破壊的操作・
        push / merge / deploy の確認は省けない)
  zai team status [--json]
        保存された Team Run の状態を表示する
  zai team resume
        未完了の Team Run を GUI で開き直す
  zai team stop
        新規割り当てを止める (実行中エージェントの停止は承認が要る)
  zai team reset [--dry-run] [--yes]
        保存された Team Run を消す (既定は消す対象の表示のみ)

  共通:
    --agents N   最大同時セッション数 (1〜64、既定 4)
    --workspace <dir>  対象フォルダ (既定はカレント)
";

/// 引数から `--name <値>` を取り出す。
fn take_opt(args: &[String], name: &str) -> (Option<String>, Vec<String>) {
    let mut out = Vec::new();
    let mut found = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            if i + 1 < args.len() {
                found = Some(args[i + 1].clone());
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        // `--name=値` も受ける
        if let Some(v) = args[i].strip_prefix(&format!("{name}=")) {
            found = Some(v.to_string());
            i += 1;
            continue;
        }
        out.push(args[i].clone());
        i += 1;
    }
    (found, out)
}

/// 引数から `--flag` を取り出す。
fn take_flag(args: &[String], name: &str) -> (bool, Vec<String>) {
    let mut out = Vec::new();
    let mut found = false;
    for a in args {
        if a == name {
            found = true;
        } else {
            out.push(a.clone());
        }
    }
    (found, out)
}

/// 解析済みの共通オプション。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommonOpts {
    pub agents: usize,
    pub json: bool,
    pub yes: bool,
    pub dry_run: bool,
    pub headless: bool,
    pub workspace: Option<PathBuf>,
    /// 残った位置引数。
    pub rest: Vec<String>,
}

/// 共通オプションを取り出す (純関数・テストで固定する)。
pub fn parse_common(args: &[String]) -> Result<CommonOpts, String> {
    let (agents_raw, rest) = take_opt(args, "--agents");
    let (ws_raw, rest) = take_opt(&rest, "--workspace");
    let (json, rest) = take_flag(&rest, "--json");
    let (yes, rest) = take_flag(&rest, "--yes");
    let (dry_run, rest) = take_flag(&rest, "--dry-run");
    let (headless, rest) = take_flag(&rest, "--headless");
    let agents = match agents_raw {
        None => 4,
        Some(s) => s
            .parse::<usize>()
            .map_err(|_| format!("--agents に数値を指定してください: {s}"))?,
    };
    if agents == 0 || agents > launch::MAX_AGENTS {
        return Err(format!(
            "--agents は 1〜{} の範囲で指定してください (指定: {agents})",
            launch::MAX_AGENTS
        ));
    }
    if let Some(bad) = rest.iter().find(|a| a.starts_with("--")) {
        return Err(format!("不明なオプションです: {bad}"));
    }
    Ok(CommonOpts {
        agents,
        json,
        yes,
        dry_run,
        headless,
        workspace: ws_raw.map(PathBuf::from),
        rest,
    })
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn err(msg: impl std::fmt::Display) -> i32 {
    eprintln!("{msg}");
    EXIT_ERR
}

/// 計画を人が読める形にする。
pub fn render_plan(plan: &TeamPlan, agents: usize) -> String {
    let mut s = String::new();
    s.push_str(&format!("Goal: {}\n", plan.goal.title));
    s.push_str("Definition of Done:\n");
    for d in &plan.goal.definition_of_done {
        s.push_str(&format!("  - {d}\n"));
    }
    s.push_str(&format!(
        "\nPlanner: {} / チーム {} レーン / 最大 {} セッション\n",
        StaticPlanner.name(),
        plan.teams.len(),
        agents
    ));
    for t in &plan.teams {
        s.push_str(&format!(
            "  [{}] {} — {}\n",
            t.id,
            t.name,
            t.lead_role.key()
        ));
    }
    s.push_str(&format!("\nタスク ({} 件)\n", plan.tasks.len()));
    for t in &plan.tasks {
        let deps = if t.dependencies.is_empty() {
            "-".to_string()
        } else {
            t.dependencies
                .iter()
                .map(|d| format!("#{d}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        s.push_str(&format!(
            "  #{:<3} {:<40} {:<12} 依存:{:<10} files:{}\n",
            t.id,
            trunc(&t.title, 40),
            t.role.key(),
            deps,
            if t.files.is_empty() {
                "(未申告)".to_string()
            } else {
                t.files.join(" ")
            }
        ));
    }
    s
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let cut: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// 計画を JSON にする (`--json`)。
pub fn plan_json(plan: &TeamPlan) -> String {
    let tasks: Vec<serde_json::Value> = plan
        .tasks
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "key": t.key,
                "title": t.title,
                "team": t.team_id.as_str(),
                "role": t.role.key(),
                "depends_on": t.dependencies,
                "files": t.files,
                "required_caps": t.required_caps,
                "acceptance_criteria": t.acceptance_criteria,
                "validation_commands": t.validation_commands,
            })
        })
        .collect();
    let teams: Vec<serde_json::Value> = plan
        .teams
        .iter()
        .map(|t| serde_json::json!({"key": t.id.as_str(), "name": t.name, "lead_role": t.lead_role.key()}))
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "goal": {
            "id": plan.goal.id.as_str(),
            "title": plan.goal.title,
            "definition_of_done": plan.goal.definition_of_done,
        },
        "teams": teams,
        "tasks": tasks,
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

/// 保存された状態を人が読める形にする。
pub fn render_status(s: &persistence::Saved) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Goal: {} [{}]\n実行 ID: {} / SPEC: {}\n",
        s.goal.title,
        s.goal.status.key(),
        s.run.run_id,
        s.run.spec_source
    ));
    let done = s
        .tasks
        .iter()
        .filter(|t| t.state == TeamTaskState::Completed)
        .count();
    out.push_str(&format!(
        "進捗: {done}/{} 完了 / 最大 {} セッション / 一時停止:{}\n\n",
        s.tasks.len(),
        s.run.agent_count,
        if s.run.paused { "はい" } else { "いいえ" }
    ));
    out.push_str("タスク\n");
    for t in &s.tasks {
        out.push_str(&format!(
            "  #{:<3} {:<40} {:<18} 担当:{}\n",
            t.id,
            trunc(&t.title, 40),
            t.state.key(),
            t.assigned_agent
                .as_ref()
                .map(|a| a.0.clone())
                .unwrap_or_else(|| "-".into())
        ));
    }
    out.push_str("\nエージェント\n");
    for a in &s.agents {
        out.push_str(&format!(
            "  {:<16} {:<12} {:<10} {}\n",
            a.id.0,
            a.role.key(),
            a.state.key(),
            match a.kind {
                AgentKind::ManagedSession => "managed",
                AgentKind::ReportedSubAgent => "reported",
            }
        ));
    }
    let blockers: Vec<String> = s
        .tasks
        .iter()
        .flat_map(|t| t.blockers.iter().map(move |b| format!("#{}: {b}", t.id)))
        .collect();
    if !blockers.is_empty() {
        out.push_str("\nblocker\n");
        for b in &blockers {
            out.push_str(&format!("  - {b}\n"));
        }
    }
    if !s.decisions.is_empty() {
        out.push_str("\n人の判断待ち\n");
        for d in &s.decisions {
            out.push_str(&format!("  [{}] {}\n", d.kind.key(), d.reason));
        }
    }
    out
}

/// 保存された状態を JSON にする。
pub fn status_json(s: &persistence::Saved) -> String {
    // 状態ごとの件数。**全状態を必ず出す** (0 件の状態を落とすと、読む側が
    // 「その状態が無い」のか「集計されていない」のか区別できない)。
    let mut by_state = serde_json::Map::new();
    for st in TeamTaskState::ALL {
        let n = s.tasks.iter().filter(|t| t.state == st).count();
        by_state.insert(st.key().to_string(), serde_json::json!(n));
    }
    let goal_completed = s.goal.status == GoalStatus::Completed;
    let phases: Vec<serde_json::Value> = graph::phases(&s.tasks, goal_completed)
        .into_iter()
        .map(|(p, st)| {
            serde_json::json!({
                "phase": p.key(),
                "status": st.key(),
                "running": st == PhaseStatus::Running,
            })
        })
        .collect();
    let events: Vec<serde_json::Value> = s
        .events
        .iter()
        .rev()
        .take(20)
        .map(|e| serde_json::json!({"at": e.at, "kind": e.kind.key(), "summary": e.summary}))
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "run_id": s.run.run_id,
        "by_state": by_state,
        "phases": phases,
        "events": events,
        "goal": {"title": s.goal.title, "status": s.goal.status.key()},
        "paused": s.run.paused,
        "stopped": s.run.stopped,
        "agent_count": s.run.agent_count,
        "tasks": s.tasks.iter().map(|t| serde_json::json!({
            "id": t.id,
            "title": t.title,
            "state": t.state.key(),
            "attempts": t.attempts,
            "assigned_agent": t.assigned_agent.as_ref().map(|a| a.0.clone()),
            "validation_ok": t.validation.passed(&t.validation_commands),
            "review_approved": t.review.approved(),
            "blockers": t.blockers,
        })).collect::<Vec<_>>(),
        "agents": s.agents.iter().map(|a| serde_json::json!({
            "id": a.id.0,
            "role": a.role.key(),
            "state": a.state.key(),
            "kind": match a.kind { AgentKind::ManagedSession => "managed", AgentKind::ReportedSubAgent => "reported" },
            "parent": a.parent_id.as_ref().map(|p| p.0.clone()),
        })).collect::<Vec<_>>(),
        "decisions": s.decisions.iter().map(|d| serde_json::json!({
            "kind": d.kind.key(), "reason": d.reason,
        })).collect::<Vec<_>>(),
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

/// SPEC を読んで計画する (`plan` と `run` の共通部)。
fn plan_from_spec(spec_path: &Path, ws: &Path, agents: usize) -> Result<TeamPlan, String> {
    let req = launch::build(ws, spec_path, agents, false).map_err(|e| e.detail())?;
    StaticPlanner
        .plan(PlanInput {
            spec: req.spec_text,
            source: spec_path.display().to_string(),
            agent_count: agents,
            review_required: true,
        })
        .map_err(|e| e.detail())
}

/// `zai team <sub>` の本体。
pub fn cli_main(argv: &[String]) -> i32 {
    if argv.is_empty() || argv.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", HELP.trim_end());
        return EXIT_OK;
    }
    let sub = argv[0].clone();
    let opts = match parse_common(&argv[1..]) {
        Ok(o) => o,
        Err(e) => return err(e),
    };
    let ws = opts.workspace.clone().unwrap_or_else(cwd);

    match sub.as_str() {
        "plan" => {
            let Some(spec) = opts.rest.first() else {
                return err("SPEC ファイルを指定してください: zai team plan SPEC.md");
            };
            match plan_from_spec(&resolve(&ws, spec), &ws, opts.agents) {
                Ok(plan) => {
                    // **配る前に計画そのものを検証する。**
                    let issues = graph::validate_plan(&plan.tasks, &plan.goal.definition_of_done);
                    if opts.json {
                        println!("{}", plan_json(&plan));
                    } else {
                        print!("{}", render_plan(&plan, opts.agents));
                    }
                    if !issues.is_empty() {
                        eprintln!("\n計画に問題があります:");
                        for i in &issues {
                            eprintln!("  - {}", i.detail());
                        }
                        return EXIT_ERR;
                    }
                    EXIT_OK
                }
                Err(e) => err(e),
            }
        }
        "run" => {
            if opts.headless {
                eprintln!(
                    "--headless は未実装です (MVP 対象外)。\n\
                     GUI 起動での実行を使ってください: zai team run SPEC.md --agents N"
                );
                return EXIT_UNSUPPORTED;
            }
            let Some(spec) = opts.rest.first() else {
                return err("SPEC ファイルを指定してください: zai team run SPEC.md --agents 4");
            };
            let spec_path = resolve(&ws, spec);
            // 起動する前に、計画できることを確かめる (GUI を開いてから
            // 「SPEC が読めません」と出すのは遅い)。
            if let Err(e) = plan_from_spec(&spec_path, &ws, opts.agents) {
                return err(e);
            }
            let req = match launch::build(&ws, &spec_path, opts.agents, opts.yes) {
                Ok(r) => r,
                Err(e) => return err(e.detail()),
            };
            if let Err(e) = launch::post(&req) {
                return err(e.detail());
            }
            println!(
                "Team Run の起動要求を渡しました (SPEC: {} / 最大 {} セッション)",
                req.spec_path.display(),
                req.agent_count
            );
            // **二重起動しない。** 実行中インスタンスがあれば、そちらが
            // 投函を拾って Team 画面へ切り替える。
            if crate::cli::read_instance_file().is_some() {
                println!("実行中の Zaivern が Team 画面を開きます。");
                EXIT_OK
            } else {
                println!("Zaivern を起動します…");
                // 呼び出し側 (cli.rs) が GUI 起動へ落とすための合図。
                EXIT_LAUNCH_GUI
            }
        }
        "status" => match persistence::load(&persistence::team_dir(&ws)) {
            LoadOutcome::Loaded(s) => {
                if opts.json {
                    println!("{}", status_json(&s));
                } else {
                    print!("{}", render_status(&s));
                }
                EXIT_OK
            }
            LoadOutcome::Empty => {
                if opts.json {
                    println!("{{\"run_id\":null}}");
                } else {
                    println!("このワークスペースには Team Run がありません。");
                }
                EXIT_OK
            }
            LoadOutcome::Corrupt { backed_up, reason } => err(format!(
                "保存された状態を読めません: {reason}\n退避しました: {}",
                backed_up.join(", ")
            )),
            LoadOutcome::Newer { found } => err(format!(
                "保存された状態の版 ({found}) が新しすぎます。Zaivern を更新してください。"
            )),
        },
        "resume" => {
            let dir = persistence::team_dir(&ws);
            if !persistence::has_run(&dir) {
                return err("再開できる Team Run がありません。");
            }
            println!("未完了の Team Run を開きます: {}", dir.display());
            if crate::cli::read_instance_file().is_some() {
                println!("実行中の Zaivern が Team 画面を開きます。");
                EXIT_OK
            } else {
                EXIT_LAUNCH_GUI
            }
        }
        "stop" => {
            let dir = persistence::team_dir(&ws);
            match persistence::load(&dir) {
                LoadOutcome::Loaded(mut s) => {
                    s.run.paused = true;
                    s.run.stopped = true;
                    s.run.updated_at = now_secs();
                    match persistence::save(&dir, &s) {
                        Ok(()) => {
                            println!(
                                "新規割り当てを停止しました。\n\
                                 実行中エージェントの停止は Zaivern の承認を通します \
                                 (Team 画面の Stop)。"
                            );
                            EXIT_OK
                        }
                        Err(e) => err(e.detail()),
                    }
                }
                LoadOutcome::Empty => err("停止できる Team Run がありません。"),
                LoadOutcome::Corrupt { reason, .. } => err(reason),
                LoadOutcome::Newer { found } => {
                    err(format!("保存された状態の版 ({found}) が新しすぎます。"))
                }
            }
        }
        "reset" => {
            let dir = persistence::team_dir(&ws);
            let targets = persistence::reset_targets(&dir);
            if targets.is_empty() {
                println!("消すものはありません。");
                return EXIT_OK;
            }
            println!("消す対象 ({} 件):", targets.len());
            for t in &targets {
                println!("  {}", t.display());
            }
            if opts.dry_run || !opts.yes {
                println!("\n実際に消すには --yes を付けてください (--dry-run は表示のみ)。");
                return EXIT_OK;
            }
            match persistence::reset(&dir) {
                Ok(n) => {
                    println!("{n} 件を削除しました。");
                    EXIT_OK
                }
                Err(e) => err(e.detail()),
            }
        }
        other => err(format!(
            "不明なサブコマンドです: team {other}\n\n{}",
            HELP.trim_end()
        )),
    }
}

/// `zai team run` が「GUI を起こしてほしい」と伝えるための終了コード。
///
/// `cli.rs` はこの値を受け取ったら CLI 終了ではなく**GUI 起動へ落とす**。
/// 普通の終了コードとして外へ漏らさない。
pub const EXIT_LAUNCH_GUI: i32 = -1;

/// SPEC のパスを解決する (相対ならワークスペース基準)。
fn resolve(ws: &Path, spec: &str) -> PathBuf {
    let p = PathBuf::from(spec);
    if p.is_absolute() {
        p
    } else {
        ws.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn 既定のエージェント数は4() {
        let o = parse_common(&v(&["SPEC.md"])).unwrap();
        assert_eq!(o.agents, 4);
        assert_eq!(o.rest, vec!["SPEC.md".to_string()]);
        assert!(!o.json && !o.yes && !o.headless);
    }

    #[test]
    fn オプションを読む() {
        let o = parse_common(&v(&["SPEC.md", "--agents", "8", "--json", "--yes"])).unwrap();
        assert_eq!(o.agents, 8);
        assert!(o.json && o.yes);
        assert_eq!(o.rest, vec!["SPEC.md".to_string()]);
        let o2 = parse_common(&v(&["--agents=2", "S.md"])).unwrap();
        assert_eq!(o2.agents, 2);
    }

    #[test]
    fn 範囲外のエージェント数を拒否する() {
        assert!(parse_common(&v(&["--agents", "0"])).is_err());
        assert!(parse_common(&v(&["--agents", "65"])).is_err());
        assert!(parse_common(&v(&["--agents", "x"])).is_err());
    }

    #[test]
    fn 不明なオプションを拒否する() {
        assert!(parse_common(&v(&["--nope"])).is_err());
    }

    #[test]
    fn headlessは未対応として扱う() {
        let o = parse_common(&v(&["SPEC.md", "--headless"])).unwrap();
        assert!(o.headless);
        // 実際の戻り値も「未対応」を明示する
        let dir = crate::test_util::unique_temp_dir("zaivern-team-cli", "headless");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SPEC.md"), "# a\n## 要件\n- x\n").unwrap();
        let code = cli_main(&v(&[
            "run",
            "SPEC.md",
            "--headless",
            "--workspace",
            &dir.display().to_string(),
        ]));
        assert_eq!(code, EXIT_UNSUPPORTED);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn planはエージェントを起動しない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-team-cli", "plan");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SPEC.md"),
            "# 認証\n## 要件\n- A を作る (src/a.rs)\n## 検証\n- cargo test\n",
        )
        .unwrap();
        let code = cli_main(&v(&[
            "plan",
            "SPEC.md",
            "--workspace",
            &dir.display().to_string(),
        ]));
        assert_eq!(code, EXIT_OK);
        // 投函箱も状態も作られない = 何も起動していない
        assert!(!launch::launch_path(&dir).exists());
        assert!(!persistence::has_run(&persistence::team_dir(&dir)));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(persistence::team_dir(&dir)).ok();
    }

    #[test]
    fn planの表示に必要な情報が出る() {
        let plan = StaticPlanner
            .plan(PlanInput {
                spec: "# 認証\n## 要件\n- A を作る (src/a.rs)\n".into(),
                source: "SPEC.md".into(),
                agent_count: 4,
                review_required: true,
            })
            .unwrap();
        let s = render_plan(&plan, 4);
        assert!(s.contains("Goal: 認証"));
        assert!(s.contains("Definition of Done"));
        assert!(s.contains("src/a.rs"));
        assert!(s.contains("integrator"));
        let j = plan_json(&plan);
        let parsed: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(parsed["goal"]["title"], "認証");
        assert!(parsed["tasks"].as_array().unwrap().len() >= 2);
    }

    #[test]
    fn statusは保存が無ければ静かに終わる() {
        let dir = crate::test_util::unique_temp_dir("zaivern-team-cli", "status");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            cli_main(&v(&["status", "--workspace", &dir.display().to_string()])),
            EXIT_OK
        );
        assert_eq!(
            cli_main(&v(&[
                "status",
                "--json",
                "--workspace",
                &dir.display().to_string()
            ])),
            EXIT_OK
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resetは確認なしでは消さない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-team-cli", "reset");
        std::fs::create_dir_all(&dir).unwrap();
        let tdir = persistence::team_dir(&dir);
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(tdir.join("schema.json"), "{\"version\":1}").unwrap();
        std::fs::write(tdir.join("run.json"), "{}").unwrap();
        // --yes 無しでは消さない
        assert_eq!(
            cli_main(&v(&["reset", "--workspace", &dir.display().to_string()])),
            EXIT_OK
        );
        assert!(tdir.join("run.json").exists(), "確認なしで消してしまった");
        // --dry-run も消さない
        assert_eq!(
            cli_main(&v(&[
                "reset",
                "--dry-run",
                "--yes",
                "--workspace",
                &dir.display().to_string()
            ])),
            EXIT_OK
        );
        assert!(tdir.join("run.json").exists(), "--dry-run で消してしまった");
        // --yes なら消す
        assert_eq!(
            cli_main(&v(&[
                "reset",
                "--yes",
                "--workspace",
                &dir.display().to_string()
            ])),
            EXIT_OK
        );
        assert!(!tdir.join("run.json").exists());
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&tdir).ok();
    }

    #[test]
    fn 不明なサブコマンドを拒否する() {
        assert_eq!(cli_main(&v(&["fly"])), EXIT_ERR);
    }

    #[test]
    fn ヘルプは全サブコマンドを載せる() {
        for sub in ["plan", "run", "status", "resume", "stop", "reset"] {
            assert!(HELP.contains(&format!("zai team {sub}")), "{sub} が無い");
        }
        assert_eq!(cli_main(&v(&["--help"])), EXIT_OK);
        assert_eq!(cli_main(&[]), EXIT_OK);
    }

    #[test]
    fn runはワークスペース外のspecを断る() {
        let a = crate::test_util::unique_temp_dir("zaivern-team-cli", "run-a");
        let b = crate::test_util::unique_temp_dir("zaivern-team-cli", "run-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let spec = b.join("SPEC.md");
        std::fs::write(&spec, "# x\n## 要件\n- y\n").unwrap();
        let code = cli_main(&v(&[
            "run",
            &spec.display().to_string(),
            "--workspace",
            &a.display().to_string(),
        ]));
        assert_eq!(code, EXIT_ERR);
        assert!(!launch::launch_path(&a).exists());
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn runは起動要求を投函する() {
        let dir = crate::test_util::unique_temp_dir("zaivern-team-cli", "run-post");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SPEC.md"), "# x\n## 要件\n- y を作る\n").unwrap();
        let code = cli_main(&v(&[
            "run",
            "SPEC.md",
            "--agents",
            "3",
            "--yes",
            "--workspace",
            &dir.display().to_string(),
        ]));
        // 実行中インスタンスの有無で 0 か EXIT_LAUNCH_GUI
        assert!(code == EXIT_OK || code == EXIT_LAUNCH_GUI, "{code}");
        let req = launch::take(&std::fs::canonicalize(&dir).unwrap(), now_secs())
            .expect("投函されているべき");
        assert_eq!(req.agent_count, 3);
        assert!(req.auto_start);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(persistence::team_dir(&dir)).ok();
    }
}
