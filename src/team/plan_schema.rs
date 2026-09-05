//! Planner の出力 (JSON) の**型と検証**。
//!
//! Planner は将来 LLM に置き換わるので、**出力を信用しない**のがここの役目。
//! JSON Schema に相当する検証を Rust の型 + [`validate`] で行い、通ったものだけ
//! [`super::model::TeamTask`] へ写す。
//!
//! ## 上限を必ず持つ
//!
//! 文字数・配列長・タスク数に上限が無いと、暴走した Planner の出力が
//! そのまま永続ファイルへ入り、次の起動で読めなくなる。

use serde::{Deserialize, Serialize};

use super::validation_command::ValidationCommand;

use super::model::{
    clamp_list, clamp_text, now_secs, GoalId, TeamGoal, TeamGroup, TeamId, TeamRole, TeamTask,
    TeamTaskState,
};

/// 1 つの計画に載せてよいタスク数の上限。
pub const MAX_TASKS: usize = 200;
/// チーム数の上限。
pub const MAX_TEAMS: usize = 16;

/// Planner が返す JSON の最上位。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDoc {
    pub goal: GoalDoc,
    #[serde(default)]
    pub teams: Vec<TeamDoc>,
    #[serde(default)]
    pub tasks: Vec<TaskDoc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalDoc {
    pub title: String,
    #[serde(default)]
    pub definition_of_done: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDoc {
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub lead_role: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDoc {
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub team: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub required_caps: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub validation_commands: Vec<String>,
}

/// 検証に落ちた理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaError {
    /// JSON として読めない。
    Json(String),
    /// 必須項目が空。
    Empty(&'static str),
    /// 上限超過。
    TooMany { what: &'static str, limit: usize },
    /// 依存先のキーが存在しない。
    UnknownDependency { task: String, dep: String },
    /// キーの重複。
    DuplicateKey(String),
    /// 参照しているチームが存在しない。
    UnknownTeam { task: String, team: String },
}

impl SchemaError {
    pub fn detail(&self) -> String {
        match self {
            SchemaError::Json(e) => format!("計画 JSON を読めません: {e}"),
            SchemaError::Empty(w) => format!("計画の必須項目 `{w}` が空です"),
            SchemaError::TooMany { what, limit } => {
                format!("`{what}` が上限 {limit} を超えています")
            }
            SchemaError::UnknownDependency { task, dep } => {
                format!("タスク「{task}」の依存先「{dep}」が計画にありません")
            }
            SchemaError::DuplicateKey(k) => format!("キー「{k}」が重複しています"),
            SchemaError::UnknownTeam { task, team } => {
                format!("タスク「{task}」のチーム「{team}」が計画にありません")
            }
        }
    }
}

/// 検証を通った計画。`TeamTask` の ID はここで採番する (1 始まり・登場順)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamPlan {
    pub goal: TeamGoal,
    pub teams: Vec<TeamGroup>,
    pub tasks: Vec<TeamTask>,
}

/// JSON 文字列を読んで検証する。
pub fn parse(json: &str, spec_text: &str) -> Result<TeamPlan, SchemaError> {
    let doc: PlanDoc = serde_json::from_str(json).map_err(|e| SchemaError::Json(e.to_string()))?;
    validate(doc, spec_text)
}

/// 読み込み済みの文書を検証して [`TeamPlan`] にする。
pub fn validate(doc: PlanDoc, spec_text: &str) -> Result<TeamPlan, SchemaError> {
    if doc.goal.title.trim().is_empty() {
        return Err(SchemaError::Empty("goal.title"));
    }
    if doc.goal.definition_of_done.is_empty() {
        return Err(SchemaError::Empty("goal.definition_of_done"));
    }
    if doc.tasks.is_empty() {
        return Err(SchemaError::Empty("tasks"));
    }
    if doc.tasks.len() > MAX_TASKS {
        return Err(SchemaError::TooMany {
            what: "tasks",
            limit: MAX_TASKS,
        });
    }
    if doc.teams.len() > MAX_TEAMS {
        return Err(SchemaError::TooMany {
            what: "teams",
            limit: MAX_TEAMS,
        });
    }

    // チーム。1 つも無ければ既定の 4 レーンを立てる。
    let teams: Vec<TeamGroup> = if doc.teams.is_empty() {
        default_lanes()
    } else {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for t in &doc.teams {
            if t.key.trim().is_empty() {
                return Err(SchemaError::Empty("teams[].key"));
            }
            if !seen.insert(t.key.clone()) {
                return Err(SchemaError::DuplicateKey(t.key.clone()));
            }
            out.push(TeamGroup {
                id: TeamId::new(clamp_text(t.key.trim())),
                name: clamp_text(if t.name.trim().is_empty() {
                    t.key.trim()
                } else {
                    t.name.trim()
                }),
                lead_role: if t.lead_role.trim().is_empty() {
                    TeamRole::TeamLead
                } else {
                    TeamRole::parse(&t.lead_role)
                },
            });
        }
        out
    };
    let team_keys: std::collections::BTreeSet<String> =
        teams.iter().map(|t| t.id.0.clone()).collect();

    // タスクのキー → ID
    let mut key_to_id = std::collections::BTreeMap::new();
    for (i, t) in doc.tasks.iter().enumerate() {
        if t.key.trim().is_empty() {
            return Err(SchemaError::Empty("tasks[].key"));
        }
        if key_to_id
            .insert(t.key.trim().to_string(), (i + 1) as u64)
            .is_some()
        {
            return Err(SchemaError::DuplicateKey(t.key.trim().to_string()));
        }
    }

    let goal_id = GoalId::new(format!("goal-{}", now_secs()));
    let mut tasks = Vec::with_capacity(doc.tasks.len());
    for (i, t) in doc.tasks.iter().enumerate() {
        if t.title.trim().is_empty() {
            return Err(SchemaError::Empty("tasks[].title"));
        }
        let mut deps = Vec::new();
        for d in &t.depends_on {
            let Some(id) = key_to_id.get(d.trim()) else {
                return Err(SchemaError::UnknownDependency {
                    task: t.key.trim().to_string(),
                    dep: d.trim().to_string(),
                });
            };
            deps.push(*id);
        }
        deps.sort_unstable();
        deps.dedup();

        let team_key = if t.team.trim().is_empty() {
            teams
                .first()
                .map(|x| x.id.0.clone())
                .unwrap_or_else(|| "implementation".to_string())
        } else {
            t.team.trim().to_string()
        };
        if !team_keys.contains(&team_key) {
            return Err(SchemaError::UnknownTeam {
                task: t.key.trim().to_string(),
                team: team_key,
            });
        }

        let now = now_secs();
        tasks.push(TeamTask {
            id: (i + 1) as u64,
            goal_id: goal_id.clone(),
            key: clamp_text(t.key.trim()),
            title: clamp_text(t.title.trim()),
            description: clamp_text(t.description.trim()),
            team_id: TeamId::new(team_key),
            role: role_of(&t.role, &t.title),
            dependencies: deps,
            // **正規化は既存の台帳と同じ 1 本を通す。** `src\\a.rs` と
            // `./src/a.rs` を別パターンのまま持つと、重なり判定を素通りする。
            files: clamp_list(
                t.files
                    .iter()
                    .map(|s| crate::lease::normalize_path(s))
                    .filter(|s| !s.is_empty())
                    .collect(),
            ),
            required_caps: clamp_list(
                t.required_caps
                    .iter()
                    .map(|s| s.trim().to_ascii_lowercase())
                    .collect(),
            ),
            acceptance_criteria: clamp_list(
                t.acceptance_criteria
                    .iter()
                    .map(|s| s.trim().to_string())
                    .collect(),
            ),
            // **構造へ直してから持つ。** 文字列のまま内側へ入れない
            // (判定した形と実行する形がずれる)。
            //
            // **語に割れなかった行も捨てない。** 捨てると、人が SPEC に
            // 書いた 1 行が診断も出ないまま消える (残りの行が読めていれば
            // `validate_plan` は「検証コマンドが無い」とも言わない)。
            // 行を丸ごと実行体として持てば `classify` が `Forbidden` にし、
            // `DangerousCommand` として理由つきで止まる。
            validation_commands: t
                .validation_commands
                .iter()
                .take(super::model::LIST_MAX)
                .map(|s| {
                    ValidationCommand::parse(s).unwrap_or_else(|_| ValidationCommand::unparsed(s))
                })
                .collect(),
            state: TeamTaskState::Pending,
            baseline: None,
            reported_files: Vec::new(),
            assigned_agent: None,
            assigned_session: None,
            attempts: 0,
            review_of: None,
            coordinator_task: None,
            validation: Default::default(),
            review: Default::default(),
            context: Vec::new(),
            reported_validation: Vec::new(),
            dispatch_seq: 0,
            reassign_pending: false,
            last_summary: String::new(),
            changed_files: Vec::new(),
            blockers: Vec::new(),
            created_at: now,
            updated_at: now,
        });
    }

    let goal = TeamGoal::new(
        goal_id,
        doc.goal.title.trim(),
        spec_text,
        doc.goal.definition_of_done.clone(),
    );

    Ok(TeamPlan { goal, teams, tasks })
}

/// MVP の既定レーン。**1 画面の標準は 3〜5 本**なので 4 本に留める。
/// **役割を決める。`role` が読めなければ表題の頭から拾う。**
///
/// 計画は LLM が書くので、役割を `role` 欄ではなく**表題の頭**に置くことが
/// ある (`"tester: 実際にブラウザで開いて確認する"`)。[`TeamRole::parse`] は
/// 知らない語を `Implementer` に倒すので、実機では **9 本すべてが
/// `implementer`** になっていた。
///
/// 実害は表示ではなく**指示文**である。役割ごとに違う指示文
/// (`prompt::implementer` / `tester` / `reviewer` …) を選ぶ根拠がここなので、
/// テストにもレビューにも統合にも「あなたは実装担当です」と書いて送っていた。
pub(super) fn role_of(role: &str, title: &str) -> TeamRole {
    let parsed = TeamRole::parse(role);
    // `role` 欄が実際に役割を名指ししているなら、それが最優先。
    if parsed != TeamRole::Implementer || role.trim().eq_ignore_ascii_case("implementer") {
        return parsed;
    }
    // 表題の頭 (`<役割>:` / `<役割>(補足):`) を見る。
    let head = title.trim();
    let Some(colon) = head.find(':') else {
        return parsed;
    };
    let mut word = &head[..colon];
    if let Some(paren) = word.find('(') {
        word = &word[..paren];
    }
    TeamRole::parse(word)
}

pub fn default_lanes() -> Vec<TeamGroup> {
    vec![
        TeamGroup {
            id: TeamId::new("planning"),
            name: "Planning".into(),
            lead_role: TeamRole::Planner,
        },
        TeamGroup {
            id: TeamId::new("implementation"),
            name: "Implementation".into(),
            lead_role: TeamRole::TeamLead,
        },
        TeamGroup {
            id: TeamId::new("qa"),
            name: "QA & Review".into(),
            lead_role: TeamRole::Reviewer,
        },
        TeamGroup {
            id: TeamId::new("integration"),
            name: "Integration".into(),
            lead_role: TeamRole::Integrator,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "goal": {
        "title": "認証機能を実装する",
        "definition_of_done": ["認証APIが動作する", "テストが成功する"]
      },
      "teams": [{"key":"backend","name":"Backend","lead_role":"backend_lead"}],
      "tasks": [{
        "key": "auth-api",
        "title": "認証APIを実装",
        "description": "JWT認証APIを実装する",
        "team": "backend",
        "role": "implementer",
        "depends_on": [],
        "files": ["src/auth/**"],
        "required_caps": ["rust"],
        "acceptance_criteria": ["正常系と異常系がテストされている"],
        "validation_commands": ["cargo test auth"]
      }]
    }"#;

    #[test]
    fn 仕様の例をそのまま読める() {
        let plan = parse(SAMPLE, "SPEC 本文").expect("読めるべき");
        assert_eq!(plan.goal.title, "認証機能を実装する");
        assert_eq!(plan.goal.definition_of_done.len(), 2);
        assert_eq!(plan.teams.len(), 1);
        assert_eq!(plan.teams[0].lead_role, TeamRole::TeamLead);
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].id, 1);
        assert_eq!(plan.tasks[0].role, TeamRole::Implementer);
        assert_eq!(plan.tasks[0].files, vec!["src/auth/**".to_string()]);
        assert_eq!(plan.goal.specification, "SPEC 本文");
    }

    #[test]
    fn 依存はキーからidへ解決される() {
        let json = r#"{
          "goal": {"title":"g","definition_of_done":["d"]},
          "teams": [],
          "tasks": [
            {"key":"a","title":"A","acceptance_criteria":["x"],"validation_commands":["cargo test"]},
            {"key":"b","title":"B","depends_on":["a"],"acceptance_criteria":["x"],"validation_commands":["cargo test"]}
          ]
        }"#;
        let plan = parse(json, "").unwrap();
        assert_eq!(plan.tasks[1].dependencies, vec![1]);
        // チーム未指定なら既定レーンが立つ
        assert_eq!(plan.teams.len(), 4);
    }

    #[test]
    fn 未知の依存を拒否する() {
        let json = r#"{"goal":{"title":"g","definition_of_done":["d"]},
          "tasks":[{"key":"a","title":"A","depends_on":["zzz"]}]}"#;
        assert!(matches!(
            parse(json, ""),
            Err(SchemaError::UnknownDependency { .. })
        ));
    }

    #[test]
    fn 未知のチームを拒否する() {
        let json = r#"{"goal":{"title":"g","definition_of_done":["d"]},
          "teams":[{"key":"backend","name":"B","lead_role":"lead"}],
          "tasks":[{"key":"a","title":"A","team":"frontend"}]}"#;
        assert!(matches!(
            parse(json, ""),
            Err(SchemaError::UnknownTeam { .. })
        ));
    }

    #[test]
    fn 空のdodを拒否する() {
        let json =
            r#"{"goal":{"title":"g","definition_of_done":[]},"tasks":[{"key":"a","title":"A"}]}"#;
        assert_eq!(
            parse(json, ""),
            Err(SchemaError::Empty("goal.definition_of_done"))
        );
    }

    #[test]
    fn タスクが空なら拒否する() {
        let json = r#"{"goal":{"title":"g","definition_of_done":["d"]},"tasks":[]}"#;
        assert_eq!(parse(json, ""), Err(SchemaError::Empty("tasks")));
    }

    #[test]
    fn 壊れたjsonを拒否する() {
        assert!(matches!(parse("{ nope", ""), Err(SchemaError::Json(_))));
        assert!(matches!(parse("", ""), Err(SchemaError::Json(_))));
    }

    #[test]
    fn キー重複を拒否する() {
        let json = r#"{"goal":{"title":"g","definition_of_done":["d"]},
          "tasks":[{"key":"a","title":"A"},{"key":"a","title":"B"}]}"#;
        assert!(matches!(parse(json, ""), Err(SchemaError::DuplicateKey(_))));
    }

    #[test]
    fn タスク数の上限を超えたら拒否する() {
        let mut doc = PlanDoc {
            goal: GoalDoc {
                title: "g".into(),
                definition_of_done: vec!["d".into()],
            },
            teams: Vec::new(),
            tasks: Vec::new(),
        };
        for i in 0..MAX_TASKS + 1 {
            doc.tasks.push(TaskDoc {
                key: format!("k{i}"),
                title: "t".into(),
                description: String::new(),
                team: String::new(),
                role: String::new(),
                depends_on: Vec::new(),
                files: Vec::new(),
                required_caps: Vec::new(),
                acceptance_criteria: vec!["x".into()],
                validation_commands: vec!["cargo test".into()],
            });
        }
        assert!(matches!(
            validate(doc, ""),
            Err(SchemaError::TooMany { what: "tasks", .. })
        ));
    }

    #[test]
    fn 語に割れなかった検証コマンドを黙って捨てない() {
        // **捨てると診断が 1 行も出ない。** 残りの行が読めていれば
        // 「検証コマンドが無い」にもならないので、人が SPEC に書いた
        // 1 行だけが何事も無かったように消える。
        let doc = PlanDoc {
            goal: GoalDoc {
                title: "t".into(),
                definition_of_done: vec!["done".into()],
            },
            teams: Vec::new(),
            tasks: vec![TaskDoc {
                key: "k".into(),
                title: "t".into(),
                description: String::new(),
                team: String::new(),
                role: String::new(),
                depends_on: Vec::new(),
                files: Vec::new(),
                required_caps: Vec::new(),
                acceptance_criteria: vec!["x".into()],
                validation_commands: vec![
                    "cargo test auth".into(),
                    // 引用符が閉じていない = `parse` が断る行。
                    "cargo test \"unclosed".into(),
                ],
            }],
        };
        let plan = validate(doc, "").expect("計画");
        assert_eq!(
            plan.tasks[0].validation_commands.len(),
            2,
            "読めなかった 1 行を黙って捨てた"
        );
        let issues = super::super::graph::validate_plan(&plan.tasks, &plan.goal.definition_of_done);
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, super::super::graph::PlanIssue::DangerousCommand { .. })),
            "読めなかった行が理由つきで止まらない: {issues:?}"
        );
    }
}

#[cfg(test)]
mod role_from_title_tests {
    use super::*;

    /// **実機の計画では 9 本すべてが `implementer` に潰れていた。**
    ///
    /// 表題は役割を名乗っているのに `role` 欄が読めず、
    /// [`TeamRole::parse`] が既定の `Implementer` へ倒していた。実害は
    /// 表示ではなく**指示文**で、テストにもレビューにも統合にも
    /// 「あなたは実装担当です」と書いて送っていた。
    #[test]
    fn 表題が役割を名乗っていれば拾う() {
        // 実機の表題そのもの。
        for (title, want) in [
            ("planner: 依頼にある中身を実装前に文章で固める", TeamRole::Planner),
            ("architect: ファイル構成と 3D の実現方式を確定する", TeamRole::Architect),
            ("implementer(markup): ページの HTML を書く", TeamRole::Implementer),
            ("implementer(style): スタイルを書く", TeamRole::Implementer),
            ("tester: 実際にブラウザで開いて確認する", TeamRole::Tester),
            ("reviewer: index.html を読み、契約と照合する", TeamRole::Reviewer),
            ("integrator: 各タスクの成果物を 1 つのサイトとして繋ぐ", TeamRole::Integrator),
        ] {
            assert_eq!(role_of("", title), want, "表題から拾えない: {title}");
        }
    }

    /// **`role` 欄が名乗っているなら、そちらが優先。**
    /// 表題の頭が偶然役割の語でも、欄の指定を上書きしない。
    #[test]
    fn 役割欄があればそちらを使う() {
        assert_eq!(role_of("tester", "implementer: x"), TeamRole::Tester);
        assert_eq!(role_of("reviewer", "planner: x"), TeamRole::Reviewer);
        // 明示された implementer は、表題に引っ張られない。
        assert_eq!(role_of("implementer", "tester: x"), TeamRole::Implementer);
    }

    /// **名乗っていないものは今までどおり実装担当。**
    /// 勝手な読み替えで、普通のタスクの指示文を変えない。
    #[test]
    fn 名乗っていなければ実装担当のまま() {
        for title in [
            "ページの HTML を書く",
            "3D: シーンを実装する",
            "対応: ナビの折り返し",
            "",
        ] {
            assert_eq!(role_of("", title), TeamRole::Implementer, "{title}");
        }
    }
}
