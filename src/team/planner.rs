//! Planner 境界 — SPEC から [`super::plan_schema::TeamPlan`] を作る層。
//!
//! ## なぜトレイトにするのか
//!
//! Planner はいずれ LLM (Claude / Codex / Gemini) になる。**Provider 固有の
//! 処理が Team Runtime へ滲み出すと、`match provider { … }` の巨大な分岐が
//! Runtime の真ん中に生える** — 禁止事項に挙がっているとおり。
//! だから入口を [`TeamPlanner`] 1 本にし、Runtime はトレイト越しにしか
//! 呼ばない。
//!
//! ## StaticPlanner が本体である理由
//!
//! テストと CI が外部 LLM を要求してはいけない。[`StaticPlanner`] は
//! SPEC.md の見出し・箇条書きを読んで決定的に計画を組む — **同じ入力なら
//! 必ず同じ計画**になるので、E2E をネットワーク無しで再現できる。
//! LLM Planner を足すときも、この出力形式 (`plan_schema`) を守らせる。

use super::plan_schema::{self, GoalDoc, PlanDoc, SchemaError, TaskDoc, TeamDoc, TeamPlan};
use super::validation_defaults::DetectError;

/// Planner へ渡す材料。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanInput {
    /// SPEC の本文。
    pub spec: String,
    /// SPEC の出所 (ファイル名 / `"(直接入力)"`)。表示だけに使う。
    pub source: String,
    /// 最大同時 ManagedSession 数。計画の粒度の目安になる。
    pub agent_count: usize,
    /// レビューを必須にするか。
    pub review_required: bool,
    /// ワークスペースの根。
    ///
    /// **SPEC が検証コマンドを書いていないとき、ここを見て候補を決める**
    /// ([`super::validation_defaults::detect`])。固定値にすると
    /// Next.js のリポジトリで `cargo test` を走らせることになる。
    pub workspace_root: std::path::PathBuf,
    /// チームに置く役割 (フォームの「エージェントプリセット」)。
    ///
    /// **空なら既定** (実装 + レビュー)。ここに `Tester` が入っていれば
    /// テスト専任のレーンを立て、`Architect` が入っていれば設計タスクを
    /// 先頭に置く。**選んだのに何も変わらない**のは、押せるのに何も
    /// 起きないボタンと同じ嘘になる。
    pub roles: Vec<super::model::TeamRole>,
}

/// 計画に失敗した理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// SPEC が空 / 短すぎる。
    EmptySpec,
    /// SPEC が大きすぎる。
    SpecTooLarge { bytes: usize, limit: usize },
    /// Planner の出力が schema を満たさない。
    Schema(SchemaError),
    /// SPEC に書かれた検証コマンドが**語に割れない**。
    ///
    /// 黙って捨てると既定へ落ちて、利用者が書いたものと違う検証が走る。
    InvalidValidationCommand { command: String, reason: String },
    /// SPEC に書かれた検証コマンドが**実行を許されていない**。
    ForbiddenValidationCommand { command: String, reason: String },
    /// 検証コマンドの自動決定が、**設定ファイルを読めずに**失敗した。
    ///
    /// 「目印が無い」([`DetectError::Undetermined`]) と「目印はあるが候補を
    /// 出せない」([`DetectError::NoCandidate`]) は道具が無いだけなので検証
    /// なしで進んでよいが、**読めない設定ファイルは別物**である。壊れた
    /// `package.json` を空の候補へ畳むと、走らせられる検証が存在しない
    /// フォルダ (素の HTML など) と区別が付かなくなり、完了が**レビュー
    /// 承認だけ**で決まる状態のまま素通りする。
    ValidationDetectionFailed { reason: String },
}

impl PlanError {
    pub fn detail(&self) -> String {
        match self {
            PlanError::EmptySpec => "SPEC が空です。実装したい内容を書いてください。".to_string(),
            PlanError::SpecTooLarge { bytes, limit } => {
                format!("SPEC が大きすぎます ({bytes} バイト / 上限 {limit})")
            }
            PlanError::Schema(e) => e.detail(),
            PlanError::InvalidValidationCommand { command, reason } => format!(
                "検証コマンドを解釈できません: `{command}` ({reason})。\
                 シェルの記法 (`&&` `|` `;` など) は使えません。\
                 1 行に 1 コマンドで書いてください"
            ),
            PlanError::ForbiddenValidationCommand { command, reason } => {
                format!("検証コマンドを実行できません: `{command}` ({reason})")
            }
            // 文面は検出器 ([`DetectError::detail`]) から来たものをそのまま
            // 使う。**同じ失敗に 2 通りの説明を作らない。**
            PlanError::ValidationDetectionFailed { reason } => reason.clone(),
        }
    }
}

/// SPEC の上限。これを超えると Planner へ渡さない
/// (LLM Planner でも文脈に収まらないし、静的解析でも意味を成さない)。
pub const SPEC_MAX_BYTES: usize = 512 * 1024;

/// 計画を作るもの。
pub trait TeamPlanner {
    fn plan(&self, input: PlanInput) -> Result<TeamPlan, PlanError>;
    /// 表示用の名前。
    fn name(&self) -> &'static str;
}

// ── SPEC.md の読み取り (純粋関数) ────────────────────────────────────

/// 見出し 1 つと、その下の箇条書き。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecSection {
    /// `#` の数 (1〜6)。
    pub level: u8,
    pub title: String,
    /// 箇条書き (`-` / `*` / `1.`)。
    pub bullets: Vec<String>,
    /// フェンス外の地の文。
    pub prose: Vec<String>,
}

/// SPEC.md を見出し単位に割る。**コードフェンスの中は読まない**
/// (サンプルコードの `- foo` を要件と読むと、計画が汚染される)。
pub fn parse_sections(spec: &str) -> Vec<SpecSection> {
    let text = spec.replace("\r\n", "\n");
    let mut out: Vec<SpecSection> = Vec::new();
    let mut fence: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        // フェンスの開閉。``` と ~~~ の両方。
        if let Some(open) = fence.clone() {
            if trimmed.starts_with(&open) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```".to_string());
            continue;
        }
        if trimmed.starts_with("~~~") {
            fence = Some("~~~".to_string());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            let extra = rest.chars().take_while(|c| *c == '#').count();
            let level = (1 + extra).min(6) as u8;
            let title = rest[extra..].trim().to_string();
            out.push(SpecSection {
                level,
                title,
                bullets: Vec::new(),
                prose: Vec::new(),
            });
            continue;
        }
        let Some(cur) = out.last_mut() else {
            // 見出しより前の地の文は捨てずに「前書き」節へ入れる。
            out.push(SpecSection {
                level: 1,
                title: String::new(),
                bullets: Vec::new(),
                prose: if trimmed.is_empty() {
                    Vec::new()
                } else {
                    vec![trimmed.to_string()]
                },
            });
            continue;
        };
        if let Some(b) = bullet_text(trimmed) {
            if !b.is_empty() {
                cur.bullets.push(b);
            }
        } else if !trimmed.is_empty() {
            cur.prose.push(trimmed.to_string());
        }
    }
    out
}

/// 箇条書き行なら中身を返す。
fn bullet_text(line: &str) -> Option<String> {
    for p in ["- [ ] ", "- [x] ", "- ", "* ", "+ "] {
        if let Some(r) = line.strip_prefix(p) {
            return Some(r.trim().to_string());
        }
    }
    // `1. ` / `12) `
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && digits <= 3 {
        let rest = &line[digits..];
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some(r.trim().to_string());
        }
    }
    None
}

/// 見出しが「完了条件」を表しているか。日英どちらの書き方も拾う。
fn is_dod_heading(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "definition of done",
        "dod",
        "acceptance",
        "完了条件",
        "受入",
        "受け入れ",
        "done",
    ];
    NEEDLES.iter().any(|n| t.contains(n))
}

/// 見出しが「やること (タスク)」を表しているか。
fn is_task_heading(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "task",
        "todo",
        "requirement",
        "feature",
        "scope",
        "work",
        "タスク",
        "要件",
        "機能",
        "作業",
        "実装",
    ];
    NEEDLES.iter().any(|n| t.contains(n))
}

/// 見出しが「検証」を表しているか。
fn is_validation_heading(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "validation",
        "verify",
        "test command",
        "検証",
        "テストコマンド",
    ];
    NEEDLES.iter().any(|n| t.contains(n))
}

/// タイトル行から、括弧書きのファイル指定を取り出す。
///
/// `認証APIを実装 (src/auth/**)` → (`認証APIを実装`, `["src/auth/**"]`)
fn split_files(title: &str) -> (String, Vec<String>) {
    let Some(open) = title.rfind(['(', '（']) else {
        return (title.trim().to_string(), Vec::new());
    };
    let close = title.rfind([')', '）']);
    let Some(close) = close else {
        return (title.trim().to_string(), Vec::new());
    };
    if close < open {
        return (title.trim().to_string(), Vec::new());
    }
    let inner = &title[open
        + title[open..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(1)..close];
    // ファイルらしさ: `/` か `.` を含み、空白で区切られた語がパスに見える
    let parts: Vec<String> = inner
        .split([',', '、', ' '])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let looks_like_paths = !parts.is_empty()
        && parts
            .iter()
            .all(|p| p.contains('/') || p.contains('.') || p.contains('*'));
    if looks_like_paths {
        (title[..open].trim().to_string(), parts)
    } else {
        (title.trim().to_string(), Vec::new())
    }
}

/// SPEC を読んで計画を組む決定的 Planner。
///
/// **同じ SPEC なら必ず同じ計画**になる (時刻以外)。E2E とテストはこれを使う。
#[derive(Clone, Copy, Debug, Default)]
pub struct StaticPlanner;

impl StaticPlanner {
    /// SPEC → Planner 出力 JSON 相当の文書。
    ///
    /// LLM Planner を足すときも「この形を返す」のが契約になる。
    ///
    /// **検証コマンドの扱いは 3 通りに分かれる。** SPEC に書かれた指定が
    /// 1 件でも通らなければ断る。指定が無いときは自動決定を試し、
    /// 道具が無いだけなら検証なしで通し、**設定ファイルが読めないときは
    /// 断る** ([`PlanError::ValidationDetectionFailed`])。
    pub fn compose(&self, input: &PlanInput) -> Result<PlanDoc, PlanError> {
        let sections = parse_sections(&input.spec);

        // 表題: 最初の非空見出し。無ければ SPEC の最初の行。
        let title = sections
            .iter()
            .find(|s| !s.title.is_empty())
            .map(|s| s.title.clone())
            .or_else(|| {
                input
                    .spec
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .map(|l| l.to_string())
            })
            .unwrap_or_else(|| "Team Run".to_string());

        // Definition of Done
        let mut dod: Vec<String> = sections
            .iter()
            .filter(|s| is_dod_heading(&s.title))
            .flat_map(|s| s.bullets.clone())
            .collect();

        // 検証コマンド (SPEC が指定していれば使う。危険なものはここで落とす)
        // **SPEC の文字列はここで構造へ直す。** 以降は構造のまま運ぶ。
        let mut spelled: Vec<String> = sections
            .iter()
            .filter(|s| is_validation_heading(&s.title))
            .flat_map(|s| s.bullets.iter().chain(s.prose.iter()).cloned())
            .map(|s| s.trim_matches('`').trim().to_string())
            .collect();
        spelled.dedup();

        // **SPEC の指定が最優先。1 件でも通らなければ計画を作らない。**
        //
        // 以前はここで `filter(|s| parse_command(s).is_ok())` と書いて
        // いたので、`npm test && npm run lint` のような行が**黙って消え**、
        // 残りが 0 件になると既定 (`cargo test`) へ落ちていた。
        // 利用者から見ると「書いた検証と違うものが走る」ことになる。
        let mut validations: Vec<String> = Vec::new();
        for raw in &spelled {
            match super::graph::parse_command(raw) {
                Ok(_) => validations.push(raw.clone()),
                // 断り方は種類で分けるが、**理由の文面は 1 か所** (`CommandReject`)
                // から取る。ここで組み直すと、同じ不許可に 2 通りの説明ができる。
                Err(e) => {
                    let reason = e.reason().to_string();
                    let command = raw.clone();
                    return Err(match e {
                        super::graph::CommandReject::Syntax(_) => {
                            PlanError::InvalidValidationCommand { command, reason }
                        }
                        super::graph::CommandReject::Forbidden(_) => {
                            PlanError::ForbiddenValidationCommand { command, reason }
                        }
                    });
                }
            }
        }

        // 指定が無いときだけ、**リポジトリの実体を見て**候補を決める。
        //
        // **道具が無いだけなら計画は止めない。** 素の HTML やデザインだけの
        // フォルダには Cargo.toml も package.json も無い。そこで断ると
        // 「検証コマンドを書いてください」以外に進みようがなくなり、
        // Team がその手の仕事にまったく使えなくなる (実際に
        // 「綺麗な美容室の HTML を作って」で詰まった)。
        //
        // **代わりに、検証なしであることを隠さない。** 検証 0 本の計画は
        // [`super::graph::validate_plan`] が通すが、完了は**レビュー承認
        // だけ**で決まる状態になるので、盤面がそれを出す
        // ([`super::view_model::TeamSnapshot::unvalidated`])。
        //
        // **ただし「決められない」と「読めない」は別物である。**
        // `unwrap_or_default()` で全部を空へ畳むと、壊れた `package.json` の
        // リポジトリが「道具の無いフォルダ」と同じ扱いになり、**壊れた設定の
        // まま検証なしで走って、レビュー承認だけで完了できる**。
        // だから [`DetectError`] は variant ごとに分けて扱う。
        if validations.is_empty() {
            match super::validation_defaults::detect(&input.workspace_root) {
                // 候補は自動生成なので、**ここでも同じ関門を通す**
                // (許可リストの外にある候補を作ってしまったら、それは
                //  候補の作り方の不具合であって、通してよい理由にならない)。
                Ok(found) => validations = runnable_only(found),
                // 目印が無い / 目印はあるが候補を出せない。走らせられる
                // 検証がそもそも存在しないので、**検証なしのまま進む**。
                Err(DetectError::Undetermined | DetectError::NoCandidate { .. }) => {}
                // **読めないものを黙って無視しない。** 理由は検出器に
                // 言わせる (同じ説明を 2 か所に書かない)。
                Err(e @ DetectError::Unreadable { .. }) => {
                    return Err(PlanError::ValidationDetectionFailed {
                        reason: e.detail(),
                    });
                }
            }
        }
        validations.truncate(4);

        // 既定の DoD は、**検証コマンドが決まってから**組む。
        //
        // 走らせるものが 1 本も無いのに「検証コマンドが成功している」を
        // 残すと、DoD はレビューの照合表なので**達成できない条件**を毎回
        // 突きつけることになる。無いものを「成功している」と書かない。
        if dod.is_empty() {
            // SPEC が DoD を書いていないなら、**最低限の DoD を必ず持たせる**。
            // 空のまま進めると「本人の申告で完了」になる。
            dod = vec![
                "必要な実装が完了している".to_string(),
                "すべての受入基準を満たしている".to_string(),
            ];
            if !validations.is_empty() {
                dod.push("検証コマンドが成功している".to_string());
            }
            dod.push("レビューが承認されている".to_string());
            dod.push("未解決の Critical 指摘が無い".to_string());
            if !validations.is_empty() {
                dod.push("最終統合テストが成功している".to_string());
            }
        }

        // タスク: 「タスク / 要件」見出しの箇条書き。無ければ全見出しの箇条書き。
        let mut raw_tasks: Vec<String> = sections
            .iter()
            .filter(|s| is_task_heading(&s.title))
            .flat_map(|s| s.bullets.clone())
            .collect();
        if raw_tasks.is_empty() {
            raw_tasks = sections
                .iter()
                .filter(|s| !is_dod_heading(&s.title) && !is_validation_heading(&s.title))
                .flat_map(|s| s.bullets.clone())
                .collect();
        }
        if raw_tasks.is_empty() {
            // 箇条書きが 1 つも無い SPEC でも、見出しをタスクにして進める。
            raw_tasks = sections
                .iter()
                .filter(|s| s.level >= 2 && !s.title.is_empty())
                .filter(|s| !is_dod_heading(&s.title) && !is_validation_heading(&s.title))
                .map(|s| s.title.clone())
                .collect();
        }
        if raw_tasks.is_empty() {
            raw_tasks = vec![title.clone()];
        }
        // 実装タスクは「最大同時数の 2 倍」までに抑える。
        // 細かく割りすぎると割り当てが往復するだけで進まない。
        let cap = input.agent_count.max(1).saturating_mul(2).clamp(2, 24);
        raw_tasks.truncate(cap);

        // **選んだ役割でレーンが変わる。** 選択が計画に何の影響も与えない
        // なら、その選択肢は嘘になる。
        use super::model::TeamRole as R;
        let roles = if input.roles.is_empty() {
            vec![R::Implementer, R::Reviewer]
        } else {
            input.roles.clone()
        };
        let mut teams = vec![TeamDoc {
            key: "implementation".into(),
            name: "Implementation".into(),
            lead_role: "team_lead".into(),
        }];
        if roles.contains(&R::Architect) {
            teams.insert(
                0,
                TeamDoc {
                    key: "architecture".into(),
                    name: "Architecture".into(),
                    lead_role: "architect".into(),
                },
            );
        }
        if roles.contains(&R::Reviewer) || roles.contains(&R::Tester) {
            teams.push(TeamDoc {
                key: "qa".into(),
                name: "QA & Review".into(),
                lead_role: "reviewer".into(),
            });
        }
        teams.push(TeamDoc {
            key: "integration".into(),
            name: "Integration".into(),
            lead_role: "integrator".into(),
        });

        let mut tasks: Vec<TaskDoc> = Vec::new();
        let mut impl_keys: Vec<String> = Vec::new();
        // 設計担当を選んだなら、実装の前に 1 本置く (実装はこれに依存する)。
        let design_key = roles.contains(&R::Architect).then(|| {
            let key = "design".to_string();
            tasks.push(TaskDoc {
                key: key.clone(),
                title: format!("{title} の設計をまとめる"),
                description: format!(
                    "SPEC ({}) から、実装へ入る前に決めておくことを 1 枚にまとめる。",
                    input.source
                ),
                team: "architecture".into(),
                role: "architect".into(),
                depends_on: Vec::new(),
                files: Vec::new(),
                required_caps: Vec::new(),
                acceptance_criteria: vec![
                    "実装が迷わない粒度まで決まっている".to_string(),
                    "SPEC と矛盾していない".to_string(),
                ],
                validation_commands: validations.clone(),
            });
            key
        });
        for (i, t) in raw_tasks.iter().enumerate() {
            let (label, files) = split_files(t);
            let key = format!("impl-{:02}", i + 1);
            impl_keys.push(key.clone());
            tasks.push(TaskDoc {
                key,
                title: label.clone(),
                description: format!("{}\n\n出典: {}", label, input.source),
                team: "implementation".into(),
                role: "implementer".into(),
                depends_on: design_key.iter().cloned().collect(),
                files,
                required_caps: Vec::new(),
                acceptance_criteria: vec![
                    format!("{label} が SPEC の記述どおりに動作する"),
                    "正常系と異常系の両方がテストされている".to_string(),
                ],
                validation_commands: validations.clone(),
            });
        }

        // 統合タスク。**全実装タスクの完了に依存する。**
        tasks.push(TaskDoc {
            key: "integrate".into(),
            title: "最終統合と全体検証".into(),
            description: "全タスクの成果を統合し、整形・ビルド・テストを通す。\
                push / PR 作成 / merge / deploy は行わない。"
                .into(),
            team: "integration".into(),
            role: "integrator".into(),
            depends_on: impl_keys,
            files: Vec::new(),
            required_caps: Vec::new(),
            acceptance_criteria: vec![
                "すべてのタスクが完了している".to_string(),
                "整形・ビルド・テストが成功する".to_string(),
                "未解決のレビュー指摘が無い".to_string(),
            ],
            validation_commands: validations,
        });

        Ok(PlanDoc {
            goal: GoalDoc {
                title,
                definition_of_done: dod,
            },
            teams,
            tasks,
        })
    }
}


/// 自動決定した候補のうち、**実行できるものだけ**を残す。
///
/// **SPEC が書いた指定には使わない。** 人が書いたものを黙って落とすと
/// 「書いたのに走らない」が理由なしで起きるので、そちらは `compose` が
/// 種類を分けて断る (`InvalidValidationCommand` / `ForbiddenValidationCommand`)。
/// ここで落ちるのは**こちらが勝手に作った候補**だけなので、黙って捨ててよい。
///
/// `compose` の外に置くのは、番人 (`wiring_tests::検証コマンドの断り方を種類で分けている`)
/// が `compose` の本体に `is_ok()` が現れないことを見ているため。
fn runnable_only(mut v: Vec<String>) -> Vec<String> {
    v.retain(|raw| super::graph::parse_command(raw).is_ok());
    v
}

impl TeamPlanner for StaticPlanner {
    fn plan(&self, input: PlanInput) -> Result<TeamPlan, PlanError> {
        if input.spec.trim().is_empty() {
            return Err(PlanError::EmptySpec);
        }
        if input.spec.len() > SPEC_MAX_BYTES {
            return Err(PlanError::SpecTooLarge {
                bytes: input.spec.len(),
                limit: SPEC_MAX_BYTES,
            });
        }
        let doc = self.compose(&input)?;
        // **JSON の境界を必ず通す。**
        //
        // LLM Planner は JSON 文字列を返すので、検証経路は
        // [`plan_schema::parse`] になる。静的 Planner だけ型のまま
        // [`plan_schema::validate`] へ渡すと、**その経路が製品では一度も
        // 走らない** — 直列化できない形が入り込んでも誰も気付かず、
        // LLM Planner を足した日に初めて壊れる。1 計画につき 1 回の
        // 往復なので、費用より「同じ関門を通っている」ことを採る。
        let json = serde_json::to_string(&doc)
            .map_err(|e| PlanError::Schema(SchemaError::Json(e.to_string())))?;
        plan_schema::parse(&json, &input.spec).map_err(PlanError::Schema)
    }

    fn name(&self) -> &'static str {
        "static"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **ワークスペースを持たない入力。**
    ///
    /// 空のパスは「どのリポジトリでもない」なので、検証の自動決定は
    /// 必ず断られる (`detect` が相対パスを cwd 基準で解決してしまう事故を
    /// 防ぐため、空は明示的に弾いてある)。だからここを使うテストの SPEC は
    /// 検証を自分で書く。自動決定そのものは `検証を自動決定する` 群が見る。
    fn input(spec: &str) -> PlanInput {
        PlanInput {
            spec: spec.to_string(),
            source: "SPEC.md".into(),
            agent_count: 4,
            review_required: true,
            workspace_root: std::path::PathBuf::new(),
            roles: Vec::new(),
        }
    }

    /// 目印つきの一時ワークスペースを持つ入力。
    fn input_in(spec: &str, ws: &std::path::Path) -> PlanInput {
        PlanInput {
            workspace_root: ws.to_path_buf(),
            ..input(spec)
        }
    }

    fn tmp_ws(name: &str) -> std::path::PathBuf {
        let d = crate::test_util::unique_temp_dir("zaivern-team-planner", name);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const SPEC: &str = "\
# 認証機能

## 要件
- ログイン API を実装する (src/auth/login.rs)
- トークン更新 API を実装する (src/auth/refresh.rs)

## 完了条件
- 認証 API が動作する
- テストが成功する

## 検証
- cargo test auth
";

    #[test]
    fn 見出しと箇条書きを読む() {
        let secs = parse_sections(SPEC);
        assert_eq!(secs[0].title, "認証機能");
        assert_eq!(secs[1].title, "要件");
        assert_eq!(secs[1].bullets.len(), 2);
        assert_eq!(secs[2].bullets.len(), 2);
    }

    #[test]
    fn コードフェンスの中は読まない() {
        let spec = "# t\n## 要件\n```\n- これはサンプル\n```\n- 本物\n";
        let secs = parse_sections(spec);
        let req = secs.iter().find(|s| s.title == "要件").unwrap();
        assert_eq!(req.bullets, vec!["本物".to_string()], "{:?}", req.bullets);
    }

    #[test]
    fn 静的プランナーは決定的() {
        let p = StaticPlanner;
        let a = p.compose(&input(SPEC)).unwrap();
        let b = p.compose(&input(SPEC)).unwrap();
        assert_eq!(a, b, "同じ SPEC で違う計画が出た");
    }

    #[test]
    fn タスクとdodと検証を組み立てる() {
        let plan = StaticPlanner.plan(input(SPEC)).expect("計画できるべき");
        assert_eq!(plan.goal.title, "認証機能");
        assert_eq!(plan.goal.definition_of_done.len(), 2);
        // 実装 2 本 + 統合 1 本
        assert_eq!(plan.tasks.len(), 3);
        assert_eq!(plan.tasks[0].files, vec!["src/auth/login.rs".to_string()]);
        assert_eq!(plan.tasks[0].title, "ログイン API を実装する");
        assert_eq!(
            plan.tasks[0]
                .validation_commands
                .iter()
                .map(|c| c.display())
                .collect::<Vec<_>>(),
            vec!["cargo test auth".to_string()]
        );
        // 統合は全実装に依存する
        let last = plan.tasks.last().unwrap();
        assert_eq!(last.dependencies, vec![1, 2]);
        assert_eq!(last.role, super::super::model::TeamRole::Integrator);
    }

    #[test]
    fn dodが書かれていなければ既定を必ず入れる() {
        let plan = StaticPlanner
            .plan(input("# a\n\n## 要件\n- x を作る\n## 検証\n- cargo test\n"))
            .unwrap();
        assert!(
            plan.goal.definition_of_done.len() >= 6,
            "既定 DoD が入っていない: {:?}",
            plan.goal.definition_of_done
        );
    }

    #[test]
    fn 空のspecを拒否する() {
        assert_eq!(
            StaticPlanner.plan(input("   \n\n")),
            Err(PlanError::EmptySpec)
        );
    }

    #[test]
    fn 巨大なspecを拒否する() {
        let big = "a".repeat(SPEC_MAX_BYTES + 1);
        assert!(matches!(
            StaticPlanner.plan(input(&big)),
            Err(PlanError::SpecTooLarge { .. })
        ));
    }

    #[test]
    fn 危険な検証コマンドはspecにあっても採らない() {
        // **黙って消さない。** 以前はここで落として既定へ落ちていたので、
        // 「`git push` と書いたのに `cargo test` が走る」ことになっていた。
        let spec = "# a\n## 要件\n- x\n## 検証\n- git push origin main\n";
        let e = StaticPlanner
            .plan(input(spec))
            .expect_err("危険なコマンドを含む SPEC を受理した");
        match &e {
            PlanError::ForbiddenValidationCommand { command, .. } => {
                assert!(command.contains("git push"), "どれが駄目なのか言わない: {e:?}");
            }
            other => panic!("方針の拒否として返っていない: {other:?}"),
        }
        assert!(e.detail().contains("git push"), "説明に原文が無い");
    }

    #[test]
    fn 解釈できない検証コマンドは黙って捨てない() {
        // `&&` はシェルの記法。**書き方を直せば通る**ので、方針の拒否とは
        // 別の種類で返す (混ぜると、直せる人が直さなくなる)。
        let spec = "# a\n## 要件\n- x\n## 検証\n- npm test && npm run lint\n";
        let e = StaticPlanner
            .plan(input(spec))
            .expect_err("シェル記法を含む SPEC を受理した");
        match &e {
            PlanError::InvalidValidationCommand { command, .. } => {
                assert!(command.contains("&&"), "原文が入っていない: {e:?}");
            }
            other => panic!("構文の誤りとして返っていない: {other:?}"),
        }
        // **既定へ落ちていない。**
        assert!(
            !e.detail().contains("cargo"),
            "既定へ落ちた形跡がある: {}",
            e.detail()
        );
    }

    #[test]
    fn 閉じていない引用符も黙って捨てない() {
        let spec = "# a\n## 要件\n- x\n## 検証\n- cargo test \"abc\n";
        assert!(
            matches!(
                StaticPlanner.plan(input(spec)),
                Err(PlanError::InvalidValidationCommand { .. })
            ),
            "閉じていない引用符を受理した"
        );
    }

    #[test]
    fn 空の検証コマンドも黙って捨てない() {
        // **落とし穴を残さない。** 「空なら無視」と書くと、空だけの
        // 検証節が「何も書かれていない」と同じ扱いになり、既定へ落ちる。
        // 割れないものはすべて構文の誤りとして返す。
        for raw in ["", "   ", "\u{a0}"] {
            let cmd = format!("# a\n## 要件\n- x\n## 検証\n{raw}\n");
            let secs = parse_sections(&cmd);
            let has = secs
                .iter()
                .filter(|s| is_validation_heading(&s.title))
                .any(|s| !s.bullets.is_empty() || !s.prose.is_empty());
            if !has {
                // その綴りは検証節に 1 行も残らない = 「書いていない」。
                continue;
            }
            assert!(
                matches!(
                    StaticPlanner.plan(input(&cmd)),
                    Err(PlanError::InvalidValidationCommand { .. })
                        | Err(PlanError::ForbiddenValidationCommand { .. })
                ),
                "空の検証コマンド {raw:?} を黙って捨てた"
            );
        }
    }

    #[test]
    fn 明示指定は自動決定より優先する() {
        // Rust の目印があるワークスペースでも、SPEC が書いたものだけを使う。
        let d = tmp_ws("explicit");
        std::fs::write(d.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let spec = "# a\n## 要件\n- x\n## 検証\n- go test ./...\n";
        let plan = StaticPlanner.plan(input_in(spec, &d)).unwrap();
        let cmds: Vec<String> = plan
            .tasks
            .iter()
            .flat_map(|t| t.validation_commands.iter().map(|c| c.display()))
            .collect();
        assert!(cmds.iter().any(|c| c.contains("go test")), "{cmds:?}");
        assert!(
            !cmds.iter().any(|c| c.contains("cargo")),
            "明示指定があるのに既定を足した: {cmds:?}"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    /// 目印から候補が決まることを、計画まで通して見る。
    fn 自動決定される(name: &str, mark: &[(&str, &str)], want: &str, deny: &str) {
        let d = tmp_ws(name);
        for (f, body) in mark {
            std::fs::write(d.join(f), body).unwrap();
        }
        let spec = "# a\n## 要件\n- x を作る\n";
        let plan = StaticPlanner
            .plan(input_in(spec, &d))
            .unwrap_or_else(|e| panic!("{name}: 計画できるべき: {}", e.detail()));
        let cmds: Vec<String> = plan
            .tasks
            .iter()
            .flat_map(|t| t.validation_commands.iter().map(|c| c.display()))
            .collect();
        assert!(
            cmds.iter().any(|c| c.contains(want)),
            "{name}: `{want}` が出ない: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.contains(deny)),
            "{name}: `{deny}` が混ざった: {cmds:?}"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn 検証を自動決定する_rust() {
        自動決定される(
            "rust",
            &[("Cargo.toml", "[package]\nname = \"x\"\n")],
            "cargo test",
            // `"go test"` は **`car-go test`** に部分一致してしまうので使わない。
            "npm",
        );
    }

    #[test]
    fn 検証を自動決定する_go() {
        // **`cargo` が 1 文字も出てこないこと**が、この番人の要点。
        自動決定される("go", &[("go.mod", "module x\n")], "go test ./...", "cargo");
    }

    #[test]
    fn 検証を自動決定する_node() {
        自動決定される(
            "node",
            &[
                (
                    "package.json",
                    "{\"scripts\":{\"test\":\"vitest run\",\"lint\":\"eslint .\"}}",
                ),
                ("package-lock.json", "{}"),
            ],
            "npm run test",
            "cargo",
        );
    }

    /// 計画に載った検証コマンドを、見出しの一覧で返す。
    fn 載った検証(plan: &TeamPlan) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for t in &plan.tasks {
            for c in &t.validation_commands {
                let d = c.display();
                if !out.contains(&d) {
                    out.push(d);
                }
            }
        }
        out
    }

    #[test]
    fn 検証を自動決定できなくても計画は通るが検証は空のまま() {
        // 目印が 1 つも無いなら、**Rust だと決めつけない**。
        // ただし**計画そのものは止めない** — 素の HTML やデザインだけの
        // フォルダには走らせられる検証が存在しないので、断ると Team が
        // その手の仕事にまったく使えなくなる。
        let d = tmp_ws("plain-html");
        std::fs::write(d.join("index.html"), "<!doctype html>\n<h1>salon</h1>\n").unwrap();
        std::fs::write(d.join("style.css"), "h1 { color: teal; }\n").unwrap();
        // **本当にどの目印も無いこと**を先に確かめる。目印を置いたまま
        // 「検証が空だ」と言っても、それは別の話をしている。
        for marker in [
            "Cargo.toml",
            "go.mod",
            "package.json",
            "pyproject.toml",
            "pytest.ini",
            "setup.cfg",
            "setup.py",
            "requirements.txt",
        ] {
            assert!(!d.join(marker).exists(), "{marker} が残っている");
        }
        let plan = StaticPlanner
            .plan(input_in("# a\n## 要件\n- x を作る\n", &d))
            .expect("道具が無いだけで計画を断ってはいけない");
        for t in &plan.tasks {
            assert!(
                t.validation_commands.is_empty(),
                "検証を勝手に作った: {:?}",
                t.validation_commands
            );
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn testスクリプトが無いnodeでは勝手にnpm_testを作らない() {
        // `DetectError::NoCandidate` の側。**目印はあるが走らせられる検証が
        // 無い**だけなので、検証なしで計画は通る。
        let d = tmp_ws("node-nodefs");
        std::fs::write(
            d.join("package.json"),
            "{\"scripts\":{\"dev\":\"next dev\",\"build\":\"next build\",\"start\":\"next start\"}}",
        )
        .unwrap();
        std::fs::write(d.join("package-lock.json"), "{}").unwrap();
        // test / lint / typecheck / check は 1 つも無い。
        let body = std::fs::read_to_string(d.join("package.json")).unwrap();
        for name in ["test", "lint", "typecheck", "check"] {
            assert!(
                !body.contains(&format!("\"{name}\"")),
                "{name} を書いてしまっている"
            );
        }
        let plan = StaticPlanner
            .plan(input_in("# a\n## 要件\n- x を作る\n", &d))
            .expect("script が無いだけで計画を断ってはいけない");
        for t in &plan.tasks {
            assert!(
                t.validation_commands.is_empty(),
                "定義されていない npm script を候補にした: {:?}",
                t.validation_commands
            );
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn 正常なnodeプロジェクトでは自動検証が計画に残る() {
        // 「読めないものを断る」ようにした結果、**読めるものまで断って
        // いない**ことを見る。壊すのは簡単なので、対になる番人を置く。
        let d = tmp_ws("node-healthy");
        std::fs::write(
            d.join("package.json"),
            "{\"scripts\":{\"test\":\"vitest run\",\"lint\":\"eslint .\",\"dev\":\"next dev\"}}",
        )
        .unwrap();
        std::fs::write(d.join("package-lock.json"), "{}").unwrap();
        let plan = StaticPlanner
            .plan(input_in("# a\n## 要件\n- x を作る\n", &d))
            .expect("正常な package.json で計画できない");
        assert_eq!(
            載った検証(&plan),
            vec!["npm run test".to_string(), "npm run lint".to_string()],
            "自動検証が消えた / 順序が変わった"
        );
        for t in &plan.tasks {
            assert!(
                !t.validation_commands.is_empty(),
                "検証の無いタスクが混ざった: {}",
                t.title
            );
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn 壊れたpackage_jsonは検証なしとして通さない() {
        // **回帰そのもの。** `detect()` の戻りを `unwrap_or_default()` で
        // 畳むと、`DetectError::Unreadable` が「候補なし」と同じ空配列に
        // なる。壊れた Node.js プロジェクトが**検証なし・レビュー承認だけ**
        // で完了できてしまうので、ここは必ず計画エラーにする。
        let d = tmp_ws("broken-package");
        std::fs::write(d.join("package.json"), "{broken").unwrap();
        std::fs::write(d.join("package-lock.json"), "{}").unwrap();

        let result = StaticPlanner.plan(input_in("# a\n## 要件\n- x を作る\n", &d));

        assert!(
            matches!(result, Err(PlanError::ValidationDetectionFailed { .. })),
            "壊れた package.json を検証なしとして通してはいけない: {result:?}"
        );
        let why = result.unwrap_err().detail();
        // **どのファイルが読めなかったのか**を言う。
        assert!(
            why.contains("package.json"),
            "どれが読めないのか言わない: {why}"
        );
        // **解析が失敗した理由**を、検出器の文面のまま持っている
        // (ここで組み直すと、同じ失敗に 2 通りの説明ができる)。
        let parse_err = serde_json::from_str::<serde_json::Value>("{broken")
            .expect_err("この綴りは JSON として壊れている")
            .to_string();
        assert!(
            why.contains(&parse_err),
            "解析失敗の理由が落ちている: {why} / 期待: {parse_err}"
        );
        // 「目印が見つからない」(`Undetermined`) と混ぜていない。
        assert!(
            !why.contains("目印が見つかりません"),
            "読めないのを目印無しと同じ説明にしている: {why}"
        );

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn 決められないのと読めないのを分けている() {
        // 3 つの理由を**同じ入口から**通して、分岐が variant ごとに
        // 分かれていることを振る舞いで見る (文字列判定ではない)。
        use super::super::validation_defaults::{self, DetectError};
        let spec = "# a\n## 要件\n- x を作る\n";

        // 目印なし → Undetermined → 検証なしで通る
        let d = tmp_ws("split-undetermined");
        assert_eq!(validation_defaults::detect(&d), Err(DetectError::Undetermined));
        assert!(StaticPlanner.plan(input_in(spec, &d)).is_ok(), "目印なしで断った");
        std::fs::remove_dir_all(&d).ok();

        // 目印あり・候補なし → NoCandidate → 検証なしで通る
        let d = tmp_ws("split-nocandidate");
        std::fs::write(d.join("package.json"), "{\"scripts\":{\"dev\":\"x\"}}").unwrap();
        std::fs::write(d.join("package-lock.json"), "{}").unwrap();
        assert!(
            matches!(
                validation_defaults::detect(&d),
                Err(DetectError::NoCandidate { .. })
            ),
            "前提が崩れている"
        );
        assert!(
            StaticPlanner.plan(input_in(spec, &d)).is_ok(),
            "候補が無いだけで断った"
        );
        std::fs::remove_dir_all(&d).ok();

        // 読めない → Unreadable → 断る
        let d = tmp_ws("split-unreadable");
        std::fs::write(d.join("package.json"), "{ not json").unwrap();
        std::fs::write(d.join("package-lock.json"), "{}").unwrap();
        assert!(
            matches!(
                validation_defaults::detect(&d),
                Err(DetectError::Unreadable { .. })
            ),
            "前提が崩れている"
        );
        assert!(
            matches!(
                StaticPlanner.plan(input_in(spec, &d)),
                Err(PlanError::ValidationDetectionFailed { .. })
            ),
            "読めないものを通した"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn 箇条書きが無いspecでも計画できる() {
        let plan = StaticPlanner
            .plan(input("# 目的\n本文だけ。\n\n## 設計\n説明。\n## 検証\n- cargo test\n"))
            .expect("計画できるべき");
        assert!(!plan.tasks.is_empty());
    }

    #[test]
    fn エージェント数でタスク数を抑える() {
        let mut spec = String::from("# t\n## 要件\n");
        for i in 0..50 {
            spec.push_str(&format!("- 項目 {i}\n"));
        }
        spec.push_str("## 検証\n- cargo test\n");
        let mut inp = input(&spec);
        inp.agent_count = 2;
        let plan = StaticPlanner.plan(inp).unwrap();
        // 上限 2*2=4 の実装 + 統合 1
        assert_eq!(plan.tasks.len(), 5);
    }

    #[test]
    fn 役割の選択が計画を変える() {
        use super::super::model::TeamRole as R;
        // 既定 (実装 + レビュー): 実装 / QA / 統合 の 3 レーン
        let base = StaticPlanner.plan(input(SPEC)).unwrap();
        let lanes: Vec<&str> = base.teams.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(lanes, vec!["implementation", "qa", "integration"]);
        assert!(base.tasks.iter().all(|t| t.role != R::Architect));

        // 設計担当を選ぶと、設計レーンと設計タスクが増え、実装がそれに依存する
        let mut with_arch = input(SPEC);
        with_arch.roles = vec![R::Architect, R::Implementer, R::Reviewer];
        let p = StaticPlanner.plan(with_arch).unwrap();
        let lanes: Vec<&str> = p.teams.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            lanes,
            vec!["architecture", "implementation", "qa", "integration"]
        );
        let design = p
            .tasks
            .iter()
            .find(|t| t.role == R::Architect)
            .expect("設計タスクが立つ");
        for t in p.tasks.iter().filter(|t| t.role == R::Implementer) {
            assert!(
                t.dependencies.contains(&design.id),
                "#{} が設計に依存していない",
                t.id
            );
        }

        // レビューを外すと QA レーンが消える
        let mut no_qa = input(SPEC);
        no_qa.roles = vec![R::Implementer];
        let p2 = StaticPlanner.plan(no_qa).unwrap();
        let lanes: Vec<&str> = p2.teams.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(lanes, vec!["implementation", "integration"]);
    }

    #[test]
    fn ファイル指定でない括弧は表題に残す() {
        let (t, f) = split_files("ログイン API を実装する (重要)");
        assert_eq!(t, "ログイン API を実装する (重要)");
        assert!(f.is_empty());
    }
}
