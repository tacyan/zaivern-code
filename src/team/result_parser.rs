//! エージェントの**構造化報告**を読む純粋モジュール。
//!
//! ## なぜ terminal UI から切り離すのか
//!
//! 画面テキストの部分一致で状態を判定すると必ず嘘をつく
//! (`Read(src/error_handling.rs)` を「エラー」と数えた実例がある)。
//! ここは**明示的に囲まれたブロックだけ**を読み、それ以外の文字は
//! 1 バイトも解釈しない。
//!
//! ```text
//! [ZAI-TEAM-RESULT]
//! { …JSON… }
//! [/ZAI-TEAM-RESULT]
//! ```
//!
//! ## 拒否する条件 (これが機能の中核)
//!
//! 「エージェントが完了と言った」だけで Completed にしないので、
//! 次のどれかに当たったら**受け取らない**:
//!
//! * JSON が壊れている / 上限を超えている
//! * `task_id` が割り当てと違う
//! * `agent_id` が割り当てと違う
//! * 担当外のファイルを変更している
//! * 受入基準を満たしたと言えるだけの根拠 (validation) が無い
//! * validation が失敗している
//! * blocker が残っている

use serde::{Deserialize, Serialize};

use super::model::{AgentId, TaskId, TeamTask, ValidationRun};

/// 報告ブロックの開始・終了マーカー。
pub const RESULT_OPEN: &str = "[ZAI-TEAM-RESULT]";
pub const RESULT_CLOSE: &str = "[/ZAI-TEAM-RESULT]";
/// サブエージェントイベントの開始・終了マーカー。
pub const EVENT_OPEN: &str = "[ZAI-TEAM-EVENT]";
pub const EVENT_CLOSE: &str = "[/ZAI-TEAM-EVENT]";

/// 1 ブロックの本文の上限 (バイト)。
pub const BLOCK_MAX_BYTES: usize = 16 * 1024;
/// 1 回の走査で拾うブロック数の上限。
pub const BLOCKS_PER_SCAN: usize = 16;
/// 走査する画面テキストの上限。**毎フレーム全履歴を舐めない**ための線。
pub const SCAN_MAX_BYTES: usize = 256 * 1024;
/// 配列の要素数上限。
pub const ARRAY_MAX: usize = 64;
/// 親子階層の深さ上限。
pub const MAX_DEPTH: usize = 4;

// ── 完了報告 ─────────────────────────────────────────────────────────

/// 報告の JSON。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultDoc {
    pub task_id: TaskId,
    pub agent_id: String,
    pub status: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub validation: Vec<ValidationDoc>,
    #[serde(default)]
    pub blockers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationDoc {
    pub command: String,
    pub exit_code: i32,
}

/// 報告を受け取らなかった理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// JSON として読めない。
    BadJson(String),
    /// 大きすぎる。
    TooLarge { bytes: usize },
    /// 配列が長すぎる。
    ArrayTooLong { field: &'static str },
    /// タスク ID が割り当てと違う。
    TaskMismatch { got: TaskId, want: TaskId },
    /// エージェント ID が割り当てと違う。
    AgentMismatch { got: String, want: String },
    /// 担当外のファイルを変更している。
    OutOfScopeFiles(Vec<String>),
    /// 受入基準に対する検証が実行されていない。
    ValidationMissing(Vec<String>),
    /// 検証が失敗している。
    ValidationFailed(Vec<String>),
    /// blocker が残っている。
    BlockersRemain(Vec<String>),
    /// 未知の status。
    UnknownStatus(String),
}

impl RejectReason {
    pub fn detail(&self) -> String {
        match self {
            RejectReason::BadJson(e) => format!("報告の JSON を読めません: {e}"),
            RejectReason::TooLarge { bytes } => format!("報告が大きすぎます ({bytes} バイト)"),
            RejectReason::ArrayTooLong { field } => format!("`{field}` の要素が多すぎます"),
            RejectReason::TaskMismatch { got, want } => {
                format!("報告のタスク #{got} が担当 #{want} と一致しません")
            }
            RejectReason::AgentMismatch { got, want } => {
                format!("報告のエージェント「{got}」が担当「{want}」と一致しません")
            }
            RejectReason::OutOfScopeFiles(f) => {
                format!("担当外のファイルを変更しています: {}", f.join(", "))
            }
            RejectReason::ValidationMissing(c) => {
                format!("検証コマンドが実行されていません: {}", c.join(", "))
            }
            RejectReason::ValidationFailed(c) => format!("検証が失敗しています: {}", c.join(", ")),
            RejectReason::BlockersRemain(b) => format!("未解決の blocker: {}", b.join(", ")),
            RejectReason::UnknownStatus(s) => format!("未知の status「{s}」"),
        }
    }
}

/// 報告が主張している結末。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportedStatus {
    Completed,
    Blocked,
    Failed,
}

/// 受理された報告。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedResult {
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub status: ReportedStatus,
    pub summary: String,
    pub changed_files: Vec<String>,
    pub validation: Vec<ValidationRun>,
    pub blockers: Vec<String>,
}

/// 囲まれたブロックを全部取り出す (開始 → 終了の順で、入れ子は無いものとする)。
///
/// **上限つき。** 走査対象そのものも末尾 [`SCAN_MAX_BYTES`] だけを見る。
pub fn extract_blocks(text: &str, open: &str, close: &str) -> Vec<String> {
    let text = tail_bytes(text, SCAN_MAX_BYTES);
    let mut out = Vec::new();
    let mut rest = text;
    while out.len() < BLOCKS_PER_SCAN {
        let Some(i) = rest.find(open) else { break };
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(close) else { break };
        let body = &after[..j];
        if body.len() <= BLOCK_MAX_BYTES {
            out.push(body.trim().to_string());
        }
        rest = &after[j + close.len()..];
    }
    out
}

/// 末尾 `max` バイトを文字境界で切って返す。
fn tail_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// ブロック本文を報告として読む (照合はしない)。
pub fn parse_result(body: &str) -> Result<ResultDoc, RejectReason> {
    if body.len() > BLOCK_MAX_BYTES {
        return Err(RejectReason::TooLarge { bytes: body.len() });
    }
    let doc: ResultDoc =
        serde_json::from_str(body).map_err(|e| RejectReason::BadJson(e.to_string()))?;
    if doc.changed_files.len() > ARRAY_MAX {
        return Err(RejectReason::ArrayTooLong {
            field: "changed_files",
        });
    }
    if doc.validation.len() > ARRAY_MAX {
        return Err(RejectReason::ArrayTooLong {
            field: "validation",
        });
    }
    if doc.blockers.len() > ARRAY_MAX {
        return Err(RejectReason::ArrayTooLong { field: "blockers" });
    }
    Ok(doc)
}

/// 報告を、割り当てられたタスクと突き合わせて受理するか決める。
///
/// **ここが「完了」の関門**。落ちた理由はそのまま人へ出す。
pub fn accept(doc: ResultDoc, task: &TeamTask) -> Result<AcceptedResult, RejectReason> {
    if doc.task_id != task.id {
        return Err(RejectReason::TaskMismatch {
            got: doc.task_id,
            want: task.id,
        });
    }
    let want_agent = task
        .assigned_agent
        .as_ref()
        .map(|a| a.0.clone())
        .unwrap_or_default();
    if !want_agent.is_empty() && doc.agent_id.trim() != want_agent {
        return Err(RejectReason::AgentMismatch {
            got: doc.agent_id.clone(),
            want: want_agent,
        });
    }

    let status = match doc.status.trim().to_ascii_lowercase().as_str() {
        "completed" | "done" | "complete" => ReportedStatus::Completed,
        "blocked" => ReportedStatus::Blocked,
        "failed" | "error" => ReportedStatus::Failed,
        other => return Err(RejectReason::UnknownStatus(other.to_string())),
    };

    let changed: Vec<String> = doc
        .changed_files
        .iter()
        .map(|f| crate::lease::normalize_path(f))
        .filter(|f| !f.is_empty())
        .collect();
    let validation: Vec<ValidationRun> = doc
        .validation
        .iter()
        // **自己申告なので `result` は付けない。** 実測 (`ValidationOutcome`)
        // と同じ形にすると、画面でも保存でも見分けが付かなくなる。
        .map(|v| ValidationRun {
            command: v.command.trim().to_string(),
            exit_code: v.exit_code,
            result: None,
            output: None,
        })
        .collect();

    // 完了を主張していないなら、ここから先の関門は通さない
    // (blocked / failed はそのまま受け取り、状態遷移側が扱う)。
    if status != ReportedStatus::Completed {
        return Ok(AcceptedResult {
            task_id: task.id,
            agent_id: AgentId::new(doc.agent_id.trim()),
            status,
            summary: super::model::clamp_text(&doc.summary),
            changed_files: changed,
            validation,
            blockers: doc.blockers.clone(),
        });
    }

    // 1) 担当外のファイルを触っていないか。
    //    **担当ファイルが未申告 (空) のタスクは照合しない** — 何を触ってよいかを
    //    こちらが言っていないのに咎めるのは筋が通らない。
    if !task.files.is_empty() {
        let out_of_scope: Vec<String> = changed
            .iter()
            .filter(|f| !task.files.iter().any(|p| crate::lease::overlaps(p, f)))
            .cloned()
            .collect();
        if !out_of_scope.is_empty() {
            return Err(RejectReason::OutOfScopeFiles(out_of_scope));
        }
    }

    // 2) 検証が実行されているか。
    let missing: Vec<String> = task
        .validation_commands
        .iter()
        .map(|c| c.display())
        .filter(|label| !validation.iter().any(|v| v.command == *label))
        .collect();
    if !missing.is_empty() {
        return Err(RejectReason::ValidationMissing(missing));
    }

    // 3) 検証が成功しているか。
    let failed: Vec<String> = validation
        .iter()
        .filter(|v| v.exit_code != 0)
        .map(|v| v.command.clone())
        .collect();
    if !failed.is_empty() {
        return Err(RejectReason::ValidationFailed(failed));
    }

    // 4) blocker が残っていないか。
    let blockers: Vec<String> = doc
        .blockers
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if !blockers.is_empty() {
        return Err(RejectReason::BlockersRemain(blockers));
    }

    Ok(AcceptedResult {
        task_id: task.id,
        agent_id: AgentId::new(doc.agent_id.trim()),
        status,
        summary: super::model::clamp_text(&doc.summary),
        changed_files: changed,
        validation,
        blockers: Vec::new(),
    })
}

// ── サブエージェントイベント ─────────────────────────────────────────

/// 親エージェントが報告するイベントの JSON。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventDoc {
    pub kind: String,
    #[serde(default)]
    pub agent_id: String,
    #[serde(default)]
    pub parent_id: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub task_id: Option<TaskId>,
    #[serde(default)]
    pub action: String,
}

/// 受け付けるイベント種別。**表に無い語は拒否する** (捏造を通さない)。
pub const EVENT_KINDS: &[&str] = &[
    "sub_agent_started",
    "sub_agent_progress",
    "sub_agent_blocked",
    "sub_agent_completed",
    "sub_agent_failed",
    "task_started",
    "task_validation_started",
    "task_validation_completed",
    "review_started",
    "review_completed",
];

/// イベントを断った理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventReject {
    BadJson(String),
    TooLarge {
        bytes: usize,
    },
    UnknownKind(String),
    /// 親が実在しない。
    UnknownParent(String),
    /// サブエージェントなのに親が指定されていない。
    ParentMissing,
    /// 親子が循環している。
    ParentCycle(String),
    /// 階層が深すぎる。
    TooDeep {
        depth: usize,
    },
    /// 同じ ID のエージェントが既にいる (別の親の下に)。
    DuplicateAgent(String),
    /// **報告元と関係のないエージェントの下へ生やそうとした。**
    ForeignParent {
        parent: String,
        reporter: String,
    },
    /// タスク ID が親の担当と違う。
    TaskMismatch {
        got: TaskId,
        want: TaskId,
    },
    /// 本文が長すぎる。
    ActionTooLong,
    /// エージェント ID が空。
    AgentIdMissing,
}

impl EventReject {
    pub fn detail(&self) -> String {
        match self {
            EventReject::BadJson(e) => format!("イベントの JSON を読めません: {e}"),
            EventReject::TooLarge { bytes } => format!("イベントが大きすぎます ({bytes} バイト)"),
            EventReject::UnknownKind(k) => format!("未知のイベント種別「{k}」"),
            EventReject::UnknownParent(p) => format!("親エージェント「{p}」が存在しません"),
            EventReject::ParentMissing => "サブエージェントには親が必要です".to_string(),
            EventReject::ParentCycle(a) => format!("親子関係が循環しています ({a})"),
            EventReject::TooDeep { depth } => format!("親子階層が深すぎます ({depth} 段)"),
            EventReject::DuplicateAgent(a) => format!("エージェント ID「{a}」が重複しています"),
            EventReject::ForeignParent { parent, reporter } => {
                format!("`{reporter}` は `{parent}` の下へサブエージェントを生やせません")
            }
            EventReject::TaskMismatch { got, want } => {
                format!("イベントのタスク #{got} が親の担当 #{want} と一致しません")
            }
            EventReject::ActionTooLong => "イベント本文が長すぎます".to_string(),
            EventReject::AgentIdMissing => "エージェント ID が空です".to_string(),
        }
    }
}

/// イベント本文を読む (照合はしない)。
pub fn parse_event(body: &str) -> Result<EventDoc, EventReject> {
    if body.len() > BLOCK_MAX_BYTES {
        return Err(EventReject::TooLarge { bytes: body.len() });
    }
    let doc: EventDoc =
        serde_json::from_str(body).map_err(|e| EventReject::BadJson(e.to_string()))?;
    if !EVENT_KINDS.contains(&doc.kind.trim()) {
        return Err(EventReject::UnknownKind(doc.kind.clone()));
    }
    if doc.action.len() > 1_000 {
        return Err(EventReject::ActionTooLong);
    }
    Ok(doc)
}

/// 既存のエージェント表 (ID → 親 ID) に対して、このイベントを受け入れてよいか。
///
/// `reporter` はこのイベントを出した ManagedSession のエージェント ID。
/// `reporter_task` はその担当タスク。
pub fn check_event(
    doc: &EventDoc,
    known: &[(AgentId, Option<AgentId>)],
    reporter: &AgentId,
    reporter_task: Option<TaskId>,
) -> Result<(), EventReject> {
    let is_sub = doc.kind.starts_with("sub_agent_");
    if is_sub {
        if doc.agent_id.trim().is_empty() {
            return Err(EventReject::AgentIdMissing);
        }
        let parent = doc.parent_id.trim();
        if parent.is_empty() {
            return Err(EventReject::ParentMissing);
        }
        // 親は実在するエージェントでなければならない。
        if !known.iter().any(|(id, _)| id.0 == parent) {
            return Err(EventReject::UnknownParent(parent.to_string()));
        }
        // 自分が自分の親になれない。
        if parent == doc.agent_id.trim() {
            return Err(EventReject::ParentCycle(parent.to_string()));
        }
        // 既に別の親の下に居る同名エージェントは拒否する。
        if let Some((_, existing_parent)) = known.iter().find(|(id, _)| id.0 == doc.agent_id.trim())
        {
            let same = existing_parent.as_ref().map(|p| p.0.as_str()) == Some(parent);
            if !same {
                return Err(EventReject::DuplicateAgent(doc.agent_id.trim().to_string()));
            }
        }
        // **報告元の系統の下にしか生やせない。** ここを「実在する親なら
        // 誰でもよい」にすると、あるセッションが**別のエージェントの下へ
        // 偽の子**をぶら下げられる (画面の組織図が嘘になる)。
        if !is_self_or_descendant(known, parent, reporter) {
            return Err(EventReject::ForeignParent {
                parent: parent.to_string(),
                reporter: reporter.0.clone(),
            });
        }
        // 親をたどって循環と深さを見る。
        let depth = ancestry_depth(known, parent, doc.agent_id.trim())?;
        if depth + 1 > MAX_DEPTH {
            return Err(EventReject::TooDeep { depth: depth + 1 });
        }
    }
    // タスク ID は、報告元が担当しているタスクと一致していなければならない。
    if let (Some(got), Some(want)) = (doc.task_id, reporter_task) {
        if got != want {
            return Err(EventReject::TaskMismatch { got, want });
        }
    }
    Ok(())
}

/// `parent` が `reporter` 自身か、その子孫か。
///
/// 入れ子のサブエージェントは、それを起こしたセッションが報告するので
/// 「自分の系統の下」までは許す。系統をたどれない (親の鎖が切れている)
/// ものは許さない — 迷ったら断る側へ倒す。
fn is_self_or_descendant(
    known: &[(AgentId, Option<AgentId>)],
    parent: &str,
    reporter: &AgentId,
) -> bool {
    if parent == reporter.0 {
        return true;
    }
    let mut cur = parent.to_string();
    let mut seen = std::collections::BTreeSet::new();
    while seen.insert(cur.clone()) {
        let Some((_, up)) = known.iter().find(|(id, _)| id.0 == cur) else {
            return false;
        };
        match up {
            Some(p) if p.0 == reporter.0 => return true,
            Some(p) => cur = p.0.clone(),
            None => return false,
        }
    }
    false
}

/// `parent` から根までたどった段数。途中に `child` が現れたら循環。
fn ancestry_depth(
    known: &[(AgentId, Option<AgentId>)],
    parent: &str,
    child: &str,
) -> Result<usize, EventReject> {
    let mut depth = 1usize;
    let mut cur = parent.to_string();
    let mut seen = std::collections::BTreeSet::new();
    loop {
        if cur == child {
            return Err(EventReject::ParentCycle(child.to_string()));
        }
        if !seen.insert(cur.clone()) {
            return Err(EventReject::ParentCycle(cur));
        }
        let next = known
            .iter()
            .find(|(id, _)| id.0 == cur)
            .and_then(|(_, p)| p.clone());
        match next {
            Some(p) => {
                cur = p.0;
                depth += 1;
                if depth > MAX_DEPTH + 2 {
                    return Err(EventReject::TooDeep { depth });
                }
            }
            None => return Ok(depth),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::task;
    use super::*;

    fn assigned() -> TeamTask {
        let mut t = task(12, "auth", &[]);
        t.assigned_agent = Some(AgentId::new("backend-api-1"));
        t.files = vec!["src/auth.rs".to_string()];
        t.validation_commands =
        vec![super::super::validation_command::ValidationCommand::parse("cargo test auth").unwrap()];
        t
    }

    const GOOD: &str = r#"{
      "task_id": 12,
      "agent_id": "backend-api-1",
      "status": "completed",
      "summary": "JWT middlewareを実装",
      "changed_files": ["src/auth.rs"],
      "validation": [{"command": "cargo test auth", "exit_code": 0}],
      "blockers": []
    }"#;

    #[test]
    fn 囲まれたブロックだけを読む() {
        let screen = format!("ほかの出力\n{RESULT_OPEN}\n{GOOD}\n{RESULT_CLOSE}\nさらに出力\n");
        let blocks = extract_blocks(&screen, RESULT_OPEN, RESULT_CLOSE);
        assert_eq!(blocks.len(), 1);
        let doc = parse_result(&blocks[0]).expect("読めるべき");
        assert_eq!(doc.task_id, 12);
    }

    #[test]
    fn 閉じていないブロックは拾わない() {
        let screen = format!("{RESULT_OPEN}\n{GOOD}\n");
        assert!(extract_blocks(&screen, RESULT_OPEN, RESULT_CLOSE).is_empty());
    }

    #[test]
    fn 正しい報告を受理する() {
        let doc = parse_result(GOOD).unwrap();
        let acc = accept(doc, &assigned()).expect("受理されるべき");
        assert_eq!(acc.status, ReportedStatus::Completed);
        assert_eq!(acc.changed_files, vec!["src/auth.rs".to_string()]);
    }

    #[test]
    fn 不正なjsonを拒否する() {
        assert!(matches!(
            parse_result("{ nope"),
            Err(RejectReason::BadJson(_))
        ));
    }

    #[test]
    fn タスクid不一致を拒否する() {
        let mut doc = parse_result(GOOD).unwrap();
        doc.task_id = 99;
        assert_eq!(
            accept(doc, &assigned()),
            Err(RejectReason::TaskMismatch { got: 99, want: 12 })
        );
    }

    #[test]
    fn エージェントid不一致を拒否する() {
        let mut doc = parse_result(GOOD).unwrap();
        doc.agent_id = "someone-else".into();
        assert!(matches!(
            accept(doc, &assigned()),
            Err(RejectReason::AgentMismatch { .. })
        ));
    }

    #[test]
    fn 担当外ファイルの変更を拒否する() {
        let mut doc = parse_result(GOOD).unwrap();
        doc.changed_files.push("src/other.rs".into());
        assert_eq!(
            accept(doc, &assigned()),
            Err(RejectReason::OutOfScopeFiles(vec!["src/other.rs".into()]))
        );
    }

    #[test]
    fn 検証未実行を拒否する() {
        let mut doc = parse_result(GOOD).unwrap();
        doc.validation.clear();
        assert_eq!(
            accept(doc, &assigned()),
            Err(RejectReason::ValidationMissing(vec![
                "cargo test auth".into()
            ]))
        );
    }

    #[test]
    fn 検証失敗を拒否する() {
        let mut doc = parse_result(GOOD).unwrap();
        doc.validation[0].exit_code = 1;
        assert_eq!(
            accept(doc, &assigned()),
            Err(RejectReason::ValidationFailed(vec![
                "cargo test auth".into()
            ]))
        );
    }

    #[test]
    fn blockerが残っていたら拒否する() {
        let mut doc = parse_result(GOOD).unwrap();
        doc.blockers.push("migration 仕様待ち".into());
        assert!(matches!(
            accept(doc, &assigned()),
            Err(RejectReason::BlockersRemain(_))
        ));
    }

    #[test]
    fn 未知のstatusを拒否する() {
        let mut doc = parse_result(GOOD).unwrap();
        doc.status = "almost".into();
        assert!(matches!(
            accept(doc, &assigned()),
            Err(RejectReason::UnknownStatus(_))
        ));
    }

    #[test]
    fn 大きすぎる報告を拒否する() {
        let big = format!("{{\"x\":\"{}\"}}", "a".repeat(BLOCK_MAX_BYTES));
        assert!(matches!(
            parse_result(&big),
            Err(RejectReason::TooLarge { .. })
        ));
    }

    #[test]
    fn 配列が長すぎる報告を拒否する() {
        let files: Vec<String> = (0..ARRAY_MAX + 1).map(|i| format!("\"f{i}.rs\"")).collect();
        let json = format!(
            "{{\"task_id\":12,\"agent_id\":\"backend-api-1\",\"status\":\"completed\",\"changed_files\":[{}]}}",
            files.join(",")
        );
        assert!(matches!(
            parse_result(&json),
            Err(RejectReason::ArrayTooLong { .. })
        ));
    }

    #[test]
    fn 拾うブロック数に上限がある() {
        let one = format!("{RESULT_OPEN}{{}}{RESULT_CLOSE}");
        let many = one.repeat(BLOCKS_PER_SCAN + 5);
        assert_eq!(
            extract_blocks(&many, RESULT_OPEN, RESULT_CLOSE).len(),
            BLOCKS_PER_SCAN
        );
    }

    // ── イベント ──

    const EV: &str = r#"{
      "kind": "sub_agent_started",
      "agent_id": "backend-test-1",
      "parent_id": "backend-lead",
      "role": "tester",
      "task_id": 12,
      "action": "authentication testsを作成中"
    }"#;

    fn known() -> Vec<(AgentId, Option<AgentId>)> {
        vec![(AgentId::new("backend-lead"), None)]
    }

    #[test]
    fn 正しいイベントを受け入れる() {
        let doc = parse_event(EV).expect("読めるべき");
        assert!(check_event(&doc, &known(), &AgentId::new("backend-lead"), Some(12)).is_ok());
    }

    #[test]
    fn 未知の種別を拒否する() {
        let doc = r#"{"kind":"hack_the_planet","agent_id":"x","parent_id":"backend-lead"}"#;
        assert!(matches!(parse_event(doc), Err(EventReject::UnknownKind(_))));
    }

    #[test]
    fn 未知の親を拒否する() {
        let mut doc = parse_event(EV).unwrap();
        doc.parent_id = "ghost".into();
        assert_eq!(
            check_event(&doc, &known(), &AgentId::new("backend-lead"), Some(12)),
            Err(EventReject::UnknownParent("ghost".into()))
        );
    }

    #[test]
    fn 親の無いサブエージェントを拒否する() {
        let mut doc = parse_event(EV).unwrap();
        doc.parent_id = String::new();
        assert_eq!(
            check_event(&doc, &known(), &AgentId::new("backend-lead"), Some(12)),
            Err(EventReject::ParentMissing)
        );
    }

    #[test]
    fn 親子循環を拒否する() {
        // lead の親が sub、sub の親が lead になろうとする
        let k = vec![
            (AgentId::new("backend-lead"), Some(AgentId::new("sub"))),
            (AgentId::new("sub"), Some(AgentId::new("backend-lead"))),
        ];
        let mut doc = parse_event(EV).unwrap();
        doc.agent_id = "sub".into();
        doc.parent_id = "backend-lead".into();
        assert!(matches!(
            check_event(&doc, &k, &AgentId::new("backend-lead"), Some(12)),
            Err(EventReject::ParentCycle(_))
        ));
    }

    #[test]
    fn 自分が自分の親になれない() {
        let mut doc = parse_event(EV).unwrap();
        doc.agent_id = "backend-lead".into();
        assert!(matches!(
            check_event(&doc, &known(), &AgentId::new("backend-lead"), Some(12)),
            Err(EventReject::ParentCycle(_))
        ));
    }

    #[test]
    fn 深すぎる階層を拒否する() {
        let mut k: Vec<(AgentId, Option<AgentId>)> = vec![(AgentId::new("l0"), None)];
        for i in 1..=MAX_DEPTH {
            k.push((
                AgentId::new(format!("l{i}")),
                Some(AgentId::new(format!("l{}", i - 1))),
            ));
        }
        let mut doc = parse_event(EV).unwrap();
        doc.agent_id = "deep".into();
        doc.parent_id = format!("l{MAX_DEPTH}");
        assert!(matches!(
            check_event(&doc, &k, &AgentId::new("l0"), None),
            Err(EventReject::TooDeep { .. })
        ));
    }

    #[test]
    fn 別の親の下に同じidを作らせない() {
        let k = vec![
            (AgentId::new("backend-lead"), None),
            (AgentId::new("other-lead"), None),
            (
                AgentId::new("backend-test-1"),
                Some(AgentId::new("other-lead")),
            ),
        ];
        let doc = parse_event(EV).unwrap();
        assert_eq!(
            check_event(&doc, &k, &AgentId::new("backend-lead"), Some(12)),
            Err(EventReject::DuplicateAgent("backend-test-1".into()))
        );
    }

    #[test]
    fn タスクid不一致のイベントを拒否する() {
        let doc = parse_event(EV).unwrap();
        assert_eq!(
            check_event(&doc, &known(), &AgentId::new("backend-lead"), Some(99)),
            Err(EventReject::TaskMismatch { got: 12, want: 99 })
        );
    }

    #[test]
    fn 長すぎる本文を拒否する() {
        let json = format!(
            "{{\"kind\":\"sub_agent_progress\",\"agent_id\":\"x\",\"parent_id\":\"y\",\"action\":\"{}\"}}",
            "a".repeat(1001)
        );
        assert_eq!(parse_event(&json), Err(EventReject::ActionTooLong));
    }

    #[test]
    fn 画面の曖昧な文字列からは何も作らない() {
        let screen = "Starting sub agent backend-test-1 for task 12...\n\
                      sub_agent_started backend-test-1\n";
        assert!(extract_blocks(screen, EVENT_OPEN, EVENT_CLOSE).is_empty());
        assert!(extract_blocks(screen, RESULT_OPEN, RESULT_CLOSE).is_empty());
    }

    #[test]
    fn 他人の下へサブエージェントを生やせない() {
        // **報告元と関係のないエージェントの下へは生やせない。** ここを
        // 「実在する親なら誰でもよい」にすると、あるセッションが別の
        // エージェントの下へ偽の子をぶら下げられる (組織図が嘘になる)。
        let known = vec![
            (AgentId::new("agent-1"), None),
            (AgentId::new("agent-2"), None),
            (AgentId::new("agent-1-sub"), Some(AgentId::new("agent-1"))),
        ];
        let doc = EventDoc {
            kind: "sub_agent_started".into(),
            agent_id: "fake".into(),
            parent_id: "agent-2".into(),
            task_id: None,
            action: String::new(),
            role: String::new(),
        };
        // agent-1 が agent-2 の下へ生やそうとする → 断る
        let got = check_event(&doc, &known, &AgentId::new("agent-1"), None);
        assert!(
            matches!(got, Err(EventReject::ForeignParent { .. })),
            "他人の下へ生やせてしまった: {got:?}"
        );
        // 自分の下なら通る
        let mine = EventDoc {
            parent_id: "agent-1".into(),
            ..doc.clone()
        };
        assert!(check_event(&mine, &known, &AgentId::new("agent-1"), None).is_ok());
        // 自分の子の下 (入れ子) も通る
        let nested = EventDoc {
            parent_id: "agent-1-sub".into(),
            ..doc.clone()
        };
        assert!(check_event(&nested, &known, &AgentId::new("agent-1"), None).is_ok());
    }
}
