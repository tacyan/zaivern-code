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
pub fn is_validation_heading(title: &str) -> bool {
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
    // `(files: a b)` / `(ファイル: a b)` の**札は剥がす**。spec_writer は
    // この形を書かせているのに、ここが札を語として数えると「パスに見えない
    // 語がある」で丸ごと捨て、本文走査の予備経路が拾った分だけが残る
    // (`assets/vendor/**` のような glob は予備経路が拾えず、実機で落ちた)。
    let inner = inner.trim();
    let inner = ["files:", "file:", "ファイル:", "ファイル："]
        .iter()
        .find_map(|label| {
            inner
                .get(..label.len())
                .filter(|head| head.eq_ignore_ascii_case(label))
                .map(|_| &inner[label.len()..])
        })
        .unwrap_or(inner);
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

/// 1 つの節から拾う担当ファイルの上限。
///
/// 節がファイル一覧を丸ごと列挙していると、そのタスクがリポジトリの
/// ほとんどを抱え込む。担当が重なると [`super::scheduler`] は
/// **重なった相手を同時に走らせない**ので、拾いすぎは並列そのものを殺す。
const MAX_FILES_FROM_TEXT: usize = 8;

/// 文面に**実際に現れた**ファイルらしいトークンを拾う (純関数)。
///
/// **でっち上げない。** 返すのは入力に出てくる部分文字列だけで、
/// 「この節ならこのファイルだろう」という推測はしない。
///
/// 日本語は分かち書きしないので、空白では割れない (`index.htmlを書く`)。
/// ASCII の英数字と `. / \ _ - *` だけを 1 つの塊として拾い、
/// **非 ASCII (かな・漢字) が区切りになる**ようにしてある。
///
/// 採否は [`crate::kanban::looks_like_path`] (リポジトリで 1 本だけの
/// 「パスらしさ」の規則) を土台に、散文向けの条件を 1 つ足す —
/// **拡張子を持つこと**。報告行と違って散文には版番号や語がそのまま
/// 混ざるので、`src/auth` のような拡張子なしまで採ると推測になる。
fn files_in_text(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if !is_token_char(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_token_char(chars[i]) {
            i += 1;
        }
        let lead = start.checked_sub(1).map(|p| chars[p]);
        let run: String = chars[start..i].iter().collect();
        if let Some(p) = file_token(&run, lead) {
            if !out.contains(&p) {
                out.push(p);
                if out.len() >= MAX_FILES_FROM_TEXT {
                    break;
                }
            }
        }
    }
    out
}

/// パスを構成しうる文字か (ASCII のみ。かな・漢字は区切りになる)。
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '\\' | '_' | '-' | '*')
}

/// 1 つの塊を担当ファイルとして採るか。`lead` は塊の直前の文字。
fn file_token(run: &str, lead: Option<char>) -> Option<String> {
    // **URL とパッケージ指定の途中を拾わない。**
    // `https://example.com/a.js` は `https` と `//example.com/a.js` に割れ、
    // 後半の直前が `:` になる。`three@0.159.0/build/three.module.js` なら `@`。
    if matches!(lead, Some(':') | Some('@')) {
        return None;
    }
    // 強調の `*` と、英文の文末ピリオドは飾りなので落とす。
    let tok = run.trim_matches('*').trim_end_matches('.');
    if !crate::kanban::looks_like_path(tok) {
        return None;
    }
    // **拡張子を持つものだけ。** 末尾の区間が `名前.拡張子` の形であること。
    let last = tok.rsplit(['/', '\\']).next().unwrap_or(tok);
    let (stem, ext) = last.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || ext.len() > 8 {
        return None;
    }
    if !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    // 版番号 (`1.2.3` / `0.159.0`) を弾く。拡張子には必ず英字がある。
    if !ext.chars().any(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    // **正規化は台帳と同じ 1 本を通す** (`src\a.rs` と `./src/a.rs` を
    // 別物のまま持つと、重なり判定を素通りする)。
    let norm = crate::lease::normalize_path(tok);
    (!norm.is_empty()).then_some(norm)
}

/// 実装タスク 1 件ぶんの素。**見出しと、その節の本文**。
///
/// 本文まで持ち回るのは、**担当ファイルが本文にしか書かれない SPEC が
/// 実在するから**。実機の Run では 7 タスク全部が `files: (未申告)` になり、
/// (1) ファイル所有リースが 1 本も張れず 6 体が同じファイルを触りうる
/// (2) 変更の帰属ができず「報告と実測が食い違います」の誤った却下が 6 件
/// 記録された。見出しだけを持ち回っていると、本文は二度と読めない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSeed {
    /// 見出し (または箇条書き) の原文。ファイル指定の括弧も付いたまま。
    pub title: String,
    /// 見出しに続く地の文と箇条書き。箇条書きから起こした素では空。
    pub body: String,
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

        // タスクの見出しと本文 (選び方は `implementation_seeds` に 1 本だけ置く)。
        let mut raw_tasks = implementation_seeds(&sections, &title);
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

        // **SPEC が既に役割で割れているなら、同じ役割の仕事を二重に置かない。**
        //
        // 実機で 13 本の計画のうち 4 本 (`#1 分割と受入基準` / `#2 設計` /
        // `#12 テスト` / `#13 最終統合`) が**まるごと重複**していた。SPEC 側に
        // すでに `planner:` `architect:` `tester:` `integrator:` の担当が
        // あったのに、その上へ同じ役割の骨組みをもう一度積んでいた。
        //
        // 害は本数だけではない。骨組みの `テスト` は**全実装の完了に依存し**、
        // `最終統合` は**全部に依存する**ので、重複したぶんがそのまま
        // *待ち*になる。1 時間かけてホームページの index.html すら
        // 出来なかった Run で、この 4 本は**1 度も動かなかった**。
        //
        // 段取りが仕事より高くつくなら、そのチームは 1 体より遅い。
        let covered: std::collections::BTreeSet<R> = raw_tasks
            .iter()
            .map(|t| super::plan_schema::role_of("", &t.title))
            .collect();
        let want = |r: R| roles.contains(&r) && !covered.contains(&r);
        let mut teams = vec![TeamDoc {
            key: "implementation".into(),
            name: "Implementation".into(),
            lead_role: "team_lead".into(),
        }];
        if roles.contains(&R::Planner) {
            teams.insert(
                0,
                TeamDoc {
                    key: "planning".into(),
                    name: "Planning".into(),
                    lead_role: "planner".into(),
                },
            );
        }
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
                // **選んだほうを頭に据える。** 固定で "reviewer" にすると、
                // テスト担当だけを選んだ人の盤面に居ないはずのレビュー担当が
                // 立ち、レビュー担当を外しても何も変わらない (選択の嘘)。
                lead_role: if roles.contains(&R::Reviewer) {
                    "reviewer".into()
                } else {
                    "tester".into()
                },
            });
        }
        teams.push(TeamDoc {
            key: "integration".into(),
            name: "Integration".into(),
            lead_role: "integrator".into(),
        });

        let raw_roles: Vec<R> = raw_tasks
            .iter()
            .map(|task| super::plan_schema::role_of("", &task.title))
            .collect();
        let raw_keys: Vec<String> = (0..raw_tasks.len())
            .map(|i| format!("impl-{:02}", i + 1))
            .collect();
        let keys_for = |role: R| {
            raw_roles
                .iter()
                .zip(&raw_keys)
                .filter(|(candidate, _)| **candidate == role)
                .map(|(_, key)| key.clone())
                .collect::<Vec<_>>()
        };

        let mut tasks: Vec<TaskDoc> = Vec::new();
        let mut impl_keys: Vec<String> = Vec::new();
        // 計画担当を選んだなら、**いちばん先頭**に 1 本置く。
        //
        // 以前は Planner を選んでも lane もタスクも作られず、選択が計画に
        // 何の影響も与えなかった (押せるのに何も起きないボタンと同じ嘘)。
        // 盤面の「1. Goal の分析」も、担当するタスクが 1 件も無いせいで
        // **何もしていないのに ✓** と出ていた ([`super::graph::phases`] は
        // 空のフェーズを「通過済み」として扱う)。
        let plan_key = want(R::Planner).then(|| {
            let key = "plan".to_string();
            tasks.push(TaskDoc {
                key: key.clone(),
                title: format!("{title} の分割と受入基準を確定する"),
                description: format!(
                    "SPEC ({}) を読み、以降のタスクが迷わない粒度まで\
                     分割と受入基準を確定する。コードは書かない。",
                    input.source
                ),
                team: "planning".into(),
                role: "planner".into(),
                depends_on: Vec::new(),
                files: Vec::new(),
                required_caps: Vec::new(),
                acceptance_criteria: vec![
                    "SPEC の項目がすべてどれかのタスクに落ちている".to_string(),
                    "各タスクの受入基準が、満たしたかどうかを測れる形になっている".to_string(),
                ],
                validation_commands: Vec::new(),
            });
            key
        });
        let planning_keys: Vec<String> = plan_key
            .iter()
            .cloned()
            .chain(keys_for(R::Planner))
            .collect();
        // 設計担当を選んだなら、実装の前に 1 本置く (実装はこれに依存する)。
        let design_key = want(R::Architect).then(|| {
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
                // 分割済みでも、Planner が確定する前の設計を最終設計として
                // 扱わない。役割を選んだ以上は依存を順序として表す。
                depends_on: planning_keys.clone(),
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
        let design_keys: Vec<String> = design_key
            .iter()
            .cloned()
            .chain(keys_for(R::Architect))
            .collect();
        // **2 つ以上の節に出てくるファイルは、誰の持ち物でもない。**
        //
        // 散文から拾うと、書く側だけでなく**読む側の節にも同じ名前が出る**
        // (`index.html` は markup が書き、style が合わせ、tester が開く)。
        // ライブラリ名も同じで、`Three.js` は architect / markup / 3d の
        // 3 節に現れる。これを全部「担当ファイル」にすると、重なり判定が
        // 働いて**3 タスクが直列化する** — 実測で 5 本中 3 本が止まった。
        // 「待っている役割が多すぎる」の再発そのものなので、**節をまたぐ
        // 名前は落とす**。残るのは「その節にしか出てこない = 明らかに
        // そこが書くもの」だけ。
        let shared: std::collections::BTreeSet<String> = {
            let mut seen: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            for t in raw_tasks.iter() {
                let (_, declared) = split_files(&t.title);
                let found = if declared.is_empty() {
                    files_in_text(&format!("{}\n{}", t.title, t.body))
                } else {
                    declared
                };
                for f in found.into_iter().collect::<std::collections::BTreeSet<_>>() {
                    *seen.entry(f).or_insert(0) += 1;
                }
            }
            seen.into_iter()
                .filter(|(_, n)| *n >= 2)
                .map(|(f, _)| f)
                .collect()
        };
        for (i, t) in raw_tasks.iter().enumerate() {
            let (label, mut files) = split_files(&t.title);
            // **見出しで見つからなければ、その節の文面から拾う。**
            //
            // 実機の SPEC は担当ファイルを括弧ではなく**説明の側**に書いて
            // いた (`… index.html を書く。`)。見出しだけを見ていたので
            // 7 タスク全部が `files: (未申告)` になり、リースが張れず、
            // 変更の帰属もできなかった。**拾えなければ空のまま**にする —
            // 埋めてよいのは文面に現れた文字列だけで、推測はしない。
            if files.is_empty() {
                files = files_in_text(&format!("{}\n{}", t.title, t.body));
                files.retain(|f| !shared.contains(f));
            }
            let key = raw_keys[i].clone();
            let role = raw_roles[i];
            impl_keys.push(key.clone());
            tasks.push(TaskDoc {
                key,
                title: label.clone(),
                description: format!("{}\n\n出典: {}", label, input.source),
                team: "implementation".into(),
                // **見出しが役割を名乗っているなら、その役割で立てる。**
                // 一律 `implementer` にすると、テストにもレビューにも統合にも
                // 「あなたは実装担当です」という指示文が飛ぶ
                // (指示文を選ぶ根拠がこの欄しかない)。
                role: role.key().to_string(),
                // Planner → Architect → Implementer の確定順を守る。分割済み
                // SPEC でも、設計が未確定のまま実装を始めれば同じファイルを
                // 作り直すか、互換性のない成果を最後に競合させることになる。
                // 同じ段の実装タスク同士は依存させないので、設計後は並列。
                // **整合担当だけは他を待つ。** 実測で、依存を持たない
                // `integrator:` の節が即座に配られ、「#1/#2/#4/#5/#6 が
                // 未完了」という当然の理由で `blocked` → 人の判断待ちに
                // なった (骨組みの重複を消したときに、依存を落としすぎた)。
                // 待たせれば担当の席が空くので、そのぶんレビューへ回せる。
                depends_on: if role == R::Integrator {
                    (0..raw_tasks.len())
                        .filter(|j| *j != i)
                        .map(|j| raw_keys[j].clone())
                        // 骨組みの書き物も待つ (**まだ書かれていない設計を
                        // 前提に締めない**)。既存の統合タスクと同じ考え方。
                        .chain(plan_key.iter().cloned())
                        .chain(design_key.iter().cloned())
                        .collect()
                } else if matches!(role, R::Tester | R::Reviewer) {
                    // **確かめる担当は、確かめるものができてから配る。**
                    //
                    // 実測 (2 体の HP): 依存の無い `tester:` が実装と同時に配られ、
                    // 「作業ツリーには SPEC.md しか無い」と伝言して待ちに入り、
                    // 180 秒画面が動かないので**停滞として人へ上げられた**
                    // (+496 秒で `needs_user`)。待っているだけの担当を停滞と
                    // 呼ぶのは正しくないが、そもそも配らなければ起きない —
                    // 席も 1 つ空き、そのぶんトークンも使わない。
                    (0..raw_tasks.len())
                        .filter(|j| {
                            *j != i && raw_roles[*j] == R::Implementer
                        })
                        .map(|j| raw_keys[j].clone())
                        .collect()
                } else if role == R::Planner {
                    Vec::new()
                } else if role == R::Architect {
                    planning_keys.clone()
                } else if role == R::Implementer && !design_keys.is_empty() {
                    design_keys.clone()
                } else {
                    planning_keys.clone()
                },
                files,
                required_caps: Vec::new(),
                acceptance_criteria: vec![
                    format!("{label} が SPEC の記述どおりに動作する"),
                    "正常系と異常系の両方がテストされている".to_string(),
                ],
                validation_commands: validations.clone(),
            });
        }

        // テストタスク。**「テスト担当」を選んだときだけ**置く。
        //
        // 以前は Tester を選んでも QA のレーンが空のまま立つだけで、
        // **テスト担当の仕事が 1 件も作られなかった** (選べるのに何も
        // 変わらない = 押せるのに何も起きないボタンと同じ嘘)。
        let test_key = want(R::Tester).then(|| {
            let key = "test".to_string();
            tasks.push(TaskDoc {
                key: key.clone(),
                title: format!("{title} のテストを書いて通す"),
                description: "実装された振る舞いに対してテストを書き、\
                     実際に走らせて通るところまで持っていく。\
                     実装そのものは直さず、直しが要るなら blocker として挙げる。"
                    .into(),
                team: "qa".into(),
                role: "tester".into(),
                // 実装成果だけを待つ。`impl_keys` には SPEC 側の Reviewer /
                // Integrator も含まれるため全部を待つと、後段から Tester へ
                // 戻る依存ができて循環する。設計は Implementer 経由で待てる。
                depends_on: keys_for(R::Implementer),
                files: Vec::new(),
                required_caps: Vec::new(),
                acceptance_criteria: vec![
                    "正常系と異常系の両方にテストがある".to_string(),
                    "追加したテストが実際に成功する".to_string(),
                ],
                validation_commands: validations.clone(),
            });
            key
        });
        // SPEC 自身が Integrator を持ち、Tester だけを骨組みで補った場合も、
        // 最終テストより先に統合を配らない。raw task は test_key より先に
        // 構築されるため、生成後に依存を結ぶ。
        if let Some(test) = &test_key {
            for task in tasks.iter_mut().filter(|task| task.role == R::Integrator.key()) {
                if !task.depends_on.contains(test) {
                    task.depends_on.push(test.clone());
                }
            }
        }

        // 統合タスク。**全実装タスクの完了に依存する。**
        // **統合は全部を待つ。** 並行させた計画・設計の書き物も含める —
        // 含めないと、まだ書かれていない設計を前提に締めることになる。
        let integrate_deps: Vec<String> = impl_keys
            .iter()
            .cloned()
            .chain(test_key.clone())
            .chain(plan_key.clone())
            .chain(design_key.clone())
            .collect();
        // **統合は既定で置く** (役割の選択とは無関係)。SPEC 側に統合担当が
        // 居るときだけ、二重になるので置かない。
        if !covered.contains(&R::Integrator) {
        tasks.push(TaskDoc {
            key: "integrate".into(),
            title: "最終統合と全体検証".into(),
            description: "全タスクの成果を統合し、整形・ビルド・テストを通す。\
                push / PR 作成 / merge / deploy は行わない。"
                .into(),
            team: "integration".into(),
            role: "integrator".into(),
            depends_on: integrate_deps,
            files: Vec::new(),
            required_caps: Vec::new(),
            acceptance_criteria: vec![
                "すべてのタスクが完了している".to_string(),
                "整形・ビルド・テストが成功する".to_string(),
                "未解決のレビュー指摘が無い".to_string(),
            ],
            validation_commands: validations,
        });
        }

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


/// 「もう分割されている」と言える実装タスクの数。
///
/// これに満たない SPEC では、分割そのものが仕事になるので
/// 計画 → 設計 → 実装 と直列に繋ぐ。満たしていれば**待たせない**。
pub const MIN_PARALLEL_TASKS: usize = 2;

/// SPEC から**実装タスクの見出し**を選ぶ (純関数)。
///
/// 「タスク / 要件」見出しの箇条書き → 全見出しの箇条書き → 見出しそのもの →
/// 最後は表題 1 件、の順に降りる。
///
/// **`compose` から切り出してあるのは、「この SPEC では何件に分かれるか」を
/// 計画を作らずに知りたい側が居るから** ([`needs_spec_rewrite`])。
/// 物差しを 2 つ持つと、「短いと言われたのに計画は分かれた」/ その逆が起きる。
pub fn implementation_titles(sections: &[SpecSection], title: &str) -> Vec<String> {
    implementation_seeds(sections, title)
        .into_iter()
        .map(|s| s.title)
        .collect()
}

/// [`implementation_titles`] と**同じ選び方**で、本文も一緒に返す。
///
/// 選び方を 2 か所に持つと「数えたときと違う分かれ方をする」ので、
/// 数える側 ([`implementation_titles`]) はここを呼ぶだけにしてある。
pub fn implementation_seeds(sections: &[SpecSection], title: &str) -> Vec<TaskSeed> {
    /// 箇条書きから起こした素 (本文は持たない — 行そのものが全部)。
    fn from_bullet(b: &str) -> TaskSeed {
        TaskSeed {
            title: b.to_string(),
            body: String::new(),
        }
    }
    let mut raw: Vec<TaskSeed> = sections
        .iter()
        .filter(|s| is_task_heading(&s.title))
        .flat_map(|s| s.bullets.iter().map(|b| from_bullet(b)))
        .collect();
    if raw.is_empty() {
        raw = sections
            .iter()
            .filter(|s| !is_dod_heading(&s.title) && !is_validation_heading(&s.title))
            .flat_map(|s| s.bullets.iter().map(|b| from_bullet(b)))
            .collect();
    }
    if raw.is_empty() {
        // 箇条書きが 1 つも無い SPEC でも、見出しをタスクにして進める。
        // **このときだけ本文がある** — 見出しの下の地の文がその節の中身。
        raw = sections
            .iter()
            .filter(|s| s.level >= 2 && !s.title.is_empty())
            .filter(|s| !is_dod_heading(&s.title) && !is_validation_heading(&s.title))
            .map(|s| TaskSeed {
                title: s.title.clone(),
                body: s
                    .prose
                    .iter()
                    .chain(s.bullets.iter())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
            })
            .collect();
    }
    if raw.is_empty() {
        raw = vec![from_bullet(title)];
    }
    raw
}

/// **この指示は「仕様書に書き換える」段を通したほうがよいか。**
///
/// 判定は「実装タスクが 2 件に分かれないこと」— 分かれない指示は、
/// 何体エージェントを立てても**1 体しか働かない**。実機の
/// 「かっこいい３DのWebページを作って」がまさにこれで、実装 1 件 + 統合 1 件
/// にしかならず、起動した 2 体目は最後まで仕事ゼロだった。
///
/// 文字数では測らない。長くても箇条書きの無い散文は 1 件にしかならないし、
/// 短くても箇条書きが 3 つあれば 3 件に分かれる。**計画と同じ読み取りで
/// 数える**のが唯一ずれない物差しになる。
pub fn needs_spec_rewrite(spec: &str) -> bool {
    let sections = parse_sections(spec);
    let title = sections
        .iter()
        .find(|s| !s.title.is_empty())
        .map(|s| s.title.clone())
        .unwrap_or_default();
    implementation_titles(&sections, &title).len() < MIN_PARALLEL_TASKS
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

    /// **SPEC が役割を名乗っていれば、同じ役割の骨組みを積まない。**
    ///
    /// 実機の Run (6 レーン) で、SPEC には既に「サイト構成を決める」
    /// 「技術構成を決める」「動作確認手順」「統合と公開手順」があったのに、
    /// その上へ `分割` / `設計` / `テスト` / `最終統合` がもう一度積まれて
    /// 8 本の SPEC が **14 本**になった。重複した 4 本は全実装の完了に
    /// 依存するので、そのまま*待ち*になり、25 分走って**1 度も動かなかった**。
    ///
    /// 原因は [`super::super::plan_schema::role_of`] が
    /// **`<役割>:` の名乗りしか読まない**こと。SPEC を書くのは
    /// [`super::super::spec_writer`] = こちらの製品なので、名乗らせる側を
    /// 直した。ここはその取り決めが効いていることを固定する。
    #[test]
    fn 役割を名乗ったspecには骨組みを積まない() {
        use super::super::model::TeamRole as R;
        const BODY: &str = "\
# ランディングページ

## タスク
- {P}サイト構成を決める (files: docs/PLAN.md)
- {A}技術構成を決める (files: docs/ARCHITECTURE.md)
- {I}ページ本体のマークアップ (files: index.html)
- {I}スタイルの実装 (files: assets/css/style.css)
- {T}動作確認手順 (files: docs/TEST.md)
- {R}レビュー記録 (files: docs/REVIEW.md)
- {G}統合と公開手順 (files: README.md)

## 完了条件
- index.html を開いてコンソールにエラーが出ない

## 検証
- `cargo test`
";
        let roles = vec![
            R::Planner,
            R::Architect,
            R::Implementer,
            R::Tester,
            R::Reviewer,
            R::Integrator,
        ];
        let named = BODY
            .replace("{P}", "planner: ")
            .replace("{A}", "architect: ")
            .replace("{I}", "implementer: ")
            .replace("{T}", "tester: ")
            .replace("{R}", "reviewer: ")
            .replace("{G}", "integrator: ");
        let bare = BODY
            .replace("{P}", "")
            .replace("{A}", "")
            .replace("{I}", "")
            .replace("{T}", "")
            .replace("{R}", "")
            .replace("{G}", "");

        let with_roles = |spec: &str| {
            let mut i = input(spec);
            i.roles = roles.clone();
            StaticPlanner.plan(i).expect("計画できる")
        };
        let named_plan = with_roles(&named);
        let bare_plan = with_roles(&bare);

        // 名乗っていれば SPEC の 7 本のまま (骨組みは 1 本も積まれない)。
        assert_eq!(
            named_plan.tasks.len(),
            7,
            "名乗っているのに骨組みが積まれた: {:?}",
            named_plan.tasks.iter().map(|t| &t.title).collect::<Vec<_>>()
        );
        // 名乗りが無ければ従来どおり骨組みが要る (この検査が空回りしない証明)。
        assert!(
            bare_plan.tasks.len() > named_plan.tasks.len(),
            "名乗りの有無で計画が変わらない (検査が空回りしている)"
        );

        // **役割も正しく付く。** 名乗りが無いと全部 implementer になり、
        // レビュー担当を立てても実装担当がレビューを持つ。
        let roles_of = |p: &super::super::plan_schema::TeamPlan| {
            p.tasks.iter().map(|t| t.role).collect::<Vec<_>>()
        };
        for want in [R::Planner, R::Architect, R::Tester, R::Reviewer, R::Integrator] {
            assert!(
                roles_of(&named_plan).contains(&want),
                "{:?} のタスクが 1 本も無い",
                want
            );
        }
        // **名乗りが無ければ、その役割のタスクは 1 本も無い。**
        // ここが両方 true だと、上の検査は名乗りと無関係に通ってしまう。
        assert!(
            !roles_of(&bare_plan).contains(&R::Reviewer),
            "名乗りが無いのにレビュー担当が付いた (検査が名乗りを見ていない)"
        );
    }
    ///
    /// 空のパスは「どのリポジトリでもない」なので、検証の自動決定は
    /// 必ず断られる (`detect` が相対パスを cwd 基準で解決してしまう事故を
    /// 防ぐため、空は明示的に弾いてある)。だからここを使うテストの SPEC は
    /// 検証を自分で書く。自動決定そのものは `検証を自動決定する` 群が見る。
    pub(super) fn input(spec: &str) -> PlanInput {
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
        // **分割済みの SPEC では実装を待たせない** (待たせると 1 体しか
        // 動かない)。設計の成果は統合が待つので取りこぼさない。
        // 直列に繋ぐのは分割されていない SPEC のときだけ —
        // それは `一行のspecでは計画から順に繋ぐ` が見ている。
        let integ = p
            .tasks
            .iter()
            .find(|t| t.role == R::Integrator)
            .expect("統合タスクが立つ");
        assert!(
            integ.dependencies.contains(&design.id),
            "統合が設計を待っていない"
        );

        // レビューを外すと QA レーンが消える
        let mut no_qa = input(SPEC);
        no_qa.roles = vec![R::Implementer];
        let p2 = StaticPlanner.plan(no_qa).unwrap();
        let lanes: Vec<&str> = p2.teams.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(lanes, vec!["implementation", "integration"]);
    }

    /// **一行のゴールでも、役割ごとのチームになる。**
    ///
    /// 実機で「かっこいい３DのWebページを作って」と 1 行だけ入れた Run は、
    /// 実装 1 件 + 統合 1 件の**2 タスク**にしかならず、しかも実装が
    /// Team Lead へ渡ったので、起動した Agent 1 は最後まで仕事ゼロだった
    /// (`state.json` の実物で確認)。既定の役割を 5 つにしたので、同じ
    /// 1 行から設計・実装・テスト・統合が立つ。
    #[test]
    fn 一行のゴールでも役割ごとのタスクが立つ() {
        use super::super::model::TeamRole as R;
        let mut inp = input("かっこいい３DのWebページを作って");
        inp.roles = super::super::panel::NewRunForm::default().roles;
        assert_eq!(inp.roles.len(), 6, "既定は選べる 6 つ全部");
        let p = StaticPlanner.plan(inp).unwrap();
        for want in [
            R::Planner,
            R::Architect,
            R::Implementer,
            R::Tester,
            R::Integrator,
        ] {
            assert!(
                p.tasks.iter().any(|t| t.role == want),
                "{want:?} の仕事が無い — 担当を立てても仕事が渡らない"
            );
        }
        // 編成もその役割どおりになる (計画 → 編成が繋がっていること)。
        // **レビュー担当はタスクを持たない**ので、レーンの頭として数える
        // (数えないとレビューを頼む相手が 1 体も居なくなる)。
        let roster = super::super::runtime::roster_roles(&p.tasks, &p.teams, 6);
        assert_eq!(
            roster,
            vec![
                R::Planner,
                R::Architect,
                R::Implementer,
                R::Tester,
                R::Reviewer,
                R::Integrator
            ]
        );
    }

    /// **分割済みの SPEC は、設計確定後に実装が並列で動ける。**
    #[test]
    fn 分割済みのspecは設計確定後に実装が並列で動ける() {
        use super::super::model::TeamRole as R;
        let mut inp = input(SPEC);
        inp.roles = super::super::panel::NewRunForm::default().roles;
        let p = StaticPlanner.plan(inp).unwrap();
        let impls: Vec<&super::super::model::TeamTask> =
            p.tasks.iter().filter(|t| t.role == R::Implementer).collect();
        assert!(impls.len() >= MIN_PARALLEL_TASKS, "SPEC が分割されていない");
        let plan = p.tasks.iter().find(|t| t.role == R::Planner).expect("Planner");
        let design = p.tasks.iter().find(|t| t.role == R::Architect).expect("Architect");
        assert!(plan.dependencies.is_empty());
        assert!(design.dependencies.contains(&plan.id));
        for task in &impls {
            assert!(task.dependencies.contains(&design.id));
            for peer in &impls {
                assert!(
                    task.id == peer.id || !task.dependencies.contains(&peer.id),
                    "実装 #{} が同段の #{} を待っている",
                    task.id,
                    peer.id
                );
            }
        }
        // **統合は全部を待つ。** 待たないと、まだ書かれていない設計を
        // 前提に締めることになる。
        let integ = p.tasks.iter().find(|t| t.role == R::Integrator).unwrap();
        for t in p.tasks.iter().filter(|t| t.role != R::Integrator) {
            assert!(
                integ.dependencies.contains(&t.id),
                "統合が #{} を待っていない",
                t.id
            );
        }
        // 並列実装数 + レビュー余力だけを起動し、仕事の無い担当を増やさない。
        assert_eq!(
            super::super::scheduler::desired_sessions(&p.tasks, 4),
            (impls.len() + 1).min(4),
            "実作業数と起動数が一致しない"
        );
    }

    /// **分割されていない SPEC では、従来どおり直列に繋ぐ。**
    /// そこでは分割そのものが仕事なので、待たせるのが正しい。
    #[test]
    fn 一行のspecでは計画から順に繋ぐ() {
        use super::super::model::TeamRole as R;
        let mut inp = input("かっこいいHPを作る");
        inp.roles = super::super::panel::NewRunForm::default().roles;
        let p = StaticPlanner.plan(inp).unwrap();
        let plan = p.tasks.iter().find(|t| t.role == R::Planner).unwrap();
        let design = p.tasks.iter().find(|t| t.role == R::Architect).unwrap();
        let imp = p.tasks.iter().find(|t| t.role == R::Implementer).unwrap();
        assert!(design.dependencies.contains(&plan.id), "設計が計画を待たない");
        assert!(imp.dependencies.contains(&design.id), "実装が設計を待たない");
    }

    /// **テスト担当を選んだら、テスト担当の仕事が立つ。**
    ///
    /// 以前は Tester を選ぶと QA のレーンが空で立つだけで、テスト用の
    /// タスクは 1 件も作られなかった。選べるのに何も変わらないなら、
    /// その選択肢は嘘になる。
    #[test]
    fn テスト担当を選ぶとテストのタスクが立つ() {
        use super::super::model::TeamRole as R;
        let mut no_test = input(SPEC);
        no_test.roles = vec![R::Implementer, R::Reviewer];
        let p = StaticPlanner.plan(no_test).unwrap();
        assert!(p.tasks.iter().all(|t| t.role != R::Tester));

        let mut with_test = input(SPEC);
        with_test.roles = vec![R::Implementer, R::Tester, R::Reviewer];
        let p = StaticPlanner.plan(with_test).unwrap();
        let test = p
            .tasks
            .iter()
            .find(|t| t.role == R::Tester)
            .expect("テストのタスクが立つ");
        // 実装が終わってから走る (先に走っても対象が無い)。
        for imp in p.tasks.iter().filter(|t| t.role == R::Implementer) {
            assert!(test.dependencies.contains(&imp.id), "実装に依存していない");
        }
        // 統合はテストの完了も待つ (待たないとテスト前に締める)。
        let integ = p
            .tasks
            .iter()
            .find(|t| t.role == R::Integrator)
            .expect("統合のタスクが立つ");
        assert!(
            integ.dependencies.contains(&test.id),
            "統合がテストを待っていない"
        );
    }


    /// **`(files: …)` の札を剥がして読む。** spec_writer が書かせる形そのもの。
    /// 札を語として数えると丸ごと捨てて、glob (`assets/vendor/**`) が落ちる。
    #[test]
    fn filesの札つきでも担当ファイルを読む() {
        let (t, f) = split_files(
            "implementer: ページを作る (files: index.html assets/css/style.css assets/vendor/**)",
        );
        assert_eq!(t, "implementer: ページを作る");
        assert_eq!(
            f,
            vec![
                "index.html".to_string(),
                "assets/css/style.css".to_string(),
                "assets/vendor/**".to_string()
            ]
        );
        let (_, f) = split_files("設計 (ファイル: docs/PLAN.md, docs/ARCH.md)");
        assert_eq!(f, vec!["docs/PLAN.md".to_string(), "docs/ARCH.md".to_string()]);
        // 札だけで中身が無いなら、ファイルは無い。
        let (_, f) = split_files("x (files: )");
        assert!(f.is_empty());
    }

    #[test]
    fn ファイル指定でない括弧は表題に残す() {
        let (t, f) = split_files("ログイン API を実装する (重要)");
        assert_eq!(t, "ログイン API を実装する (重要)");
        assert!(f.is_empty());
    }
}

#[cfg(test)]
mod no_double_planning_tests {
    use super::tests::input;
    use super::*;
    use crate::features::team::imp::model::TeamRole as R;

    /// 実機の SPEC。**見出しがそのまま役割の分担になっている** —
    /// 仕様を書く担当が「誰が何をするか」まで書いたときの形。
    const SPEC: &str = "\
# かっこいい 3D の Zaivern ホームページ

## planner: 依頼の中身を実装前に文章で固める
Zaivern が何なのかを決め、ページの構成を決める。

## architect: ファイル構成と 3D の実現方式を確定する
3D ライブラリを使うか素の WebGL かを選ぶ。

## implementer(markup): ページの HTML を書く
3D キャンバスの置き場所を決めて index.html を書く。

## implementer(style): スタイルを書く
配色・タイポグラフィ・余白。

## tester: 実際にブラウザで開いて確認する
デスクトップ幅とモバイル幅の表示を見る。

## integrator: 成果物を 1 つのサイトとして繋ぐ
開き方とファイル構成をまとめる。
";

    /// **同じ役割の仕事を二重に置かない。**
    ///
    /// 実機では 13 本のうち 4 本 (`分割と受入基準` / `設計` / `テスト` /
    /// `最終統合`) が丸ごと重複し、**1 度も動かないまま 1 時間**が過ぎた。
    /// 害は本数だけではない — 骨組みの「テスト」は全実装の完了に依存し、
    /// 「最終統合」は全部に依存するので、重複したぶんがそのまま*待ち*になる。
    #[test]
    fn specが役割で割れているなら骨組みを重ねない() {
        let mut inp = input(SPEC);
        inp.roles = vec![
            R::Planner,
            R::Architect,
            R::Implementer,
            R::Tester,
            R::Reviewer,
            R::Integrator,
        ];
        let p = StaticPlanner.plan(inp).expect("計画できるべき");
        let titles: Vec<&str> = p.tasks.iter().map(|t| t.title.as_str()).collect();
        for dup in [
            "の分割と受入基準を確定する",
            "の設計をまとめる",
            "のテストを書いて通す",
            "最終統合と全体検証",
        ] {
            assert!(
                !titles.iter().any(|t| t.contains(dup)),
                "SPEC に担当が居るのに骨組みを重ねた: {dup} / {titles:?}"
            );
        }
        // **SPEC の担当はそのまま役割として立つ** (全部 implementer に
        // 潰れると、テストにもレビューにも実装担当の指示文が飛ぶ)。
        for want in [R::Planner, R::Architect, R::Tester, R::Integrator] {
            assert!(
                p.tasks.iter().any(|t| t.role == want),
                "{want:?} の担当が居ない: {:?}",
                p.tasks.iter().map(|t| t.role).collect::<Vec<_>>()
            );
        }
        // 役割ごとの節を骨組みで重複させなくても、確定順は失わない。
        let planner = p.tasks.iter().find(|t| t.role == R::Planner).expect("Planner");
        let architect = p
            .tasks
            .iter()
            .find(|t| t.role == R::Architect)
            .expect("Architect");
        assert!(planner.dependencies.is_empty());
        assert!(architect.dependencies.contains(&planner.id));
        let impls: Vec<_> = p
            .tasks
            .iter()
            .filter(|t| t.role == R::Implementer)
            .collect();
        for task in &impls {
            assert!(task.dependencies.contains(&architect.id));
            for peer in &impls {
                assert!(task.id == peer.id || !task.dependencies.contains(&peer.id));
            }
        }
        for t in p.tasks.iter().filter(|t| t.role == R::Tester) {
            for implementation in &impls {
                assert!(
                    t.dependencies.contains(&implementation.id),
                    "検証担当 #{} が実装 #{} を待っていない (何も無いのに配られる)",
                    t.id,
                    implementation.id
                );
            }
        }
    }

    /// **役割を名乗らない SPEC では、これまでどおり骨組みを置く。**
    /// 重複を消す話であって、段取りを無くす話ではない。
    #[test]
    fn 役割で割れていないspecには骨組みを置く() {
        let mut inp = input("# 認証機能\n\n## ログイン API\n書く。\n\n## ログアウト API\n書く。\n");
        inp.roles = vec![R::Planner, R::Architect, R::Implementer, R::Tester];
        let p = StaticPlanner.plan(inp).expect("計画できるべき");
        for want in [R::Planner, R::Architect, R::Tester, R::Integrator] {
            assert!(
                p.tasks.iter().any(|t| t.role == want),
                "{want:?} の骨組みが消えた"
            );
        }
    }
}

// ══════════════════════════════════════════════════════════════════════
//  担当ファイルは本文にも書かれる
//
//  実機の Run では 7 タスク全部が `files: (未申告)` だった。SPEC は
//  担当ファイルを**見出しの括弧ではなく説明の側**に書いていたのに、
//  読み取りが見出ししか見ていなかった。害は 2 つ — リースが 1 本も
//  張れないので 6 体が同じファイルを同時に触りうること、変更の帰属が
//  できず「報告と実測が食い違います」の誤った却下が 6 件出たこと。
// ══════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod files_from_prose_tests {
    use super::tests::input;
    use super::*;

    /// 実機の SPEC。**見出しが役割の分担で、担当ファイルは本文の側**にある。
    ///
    /// `implementer(markup)` と `implementer(style)` の 2 節は実機の写し。
    /// 残りは同じ Run の見出しに、拾ってはいけないもの (CDN の URL・
    /// 版番号 `r159`・`3D` という語) を実機どおりの書き方で足してある。
    const SPEC: &str = "\
# かっこいい 3D の Zaivern ホームページ

## planner: 依頼の中身を実装前に文章で固める
Zaivern が何なのかを決め、ページの構成を docs/brief.md にまとめる。

## architect: ファイル構成と 3D の実現方式を確定する
3D ライブラリは CDN (https://unpkg.com/three@0.159.0/build/three.module.js) の r159 を読み込む。

## implementer(markup): ページの HTML を書く
3D キャンバスの置き場所とセクション構成を決めて index.html を書く。

## implementer(style): スタイルを書く
配色・タイポグラフィ・余白を css/style.css に書く。

## implementer(script): 3D シーンを書く
js/scene.js にシーン・カメラ・ライトを組む。

## tester: 実際にブラウザで開いて確認する
デスクトップ幅とモバイル幅の表示を見る。
";

    /// 見出しに `needle` を含むタスクの担当ファイル。
    fn files_of(spec: &str, needle: &str) -> Vec<String> {
        let plan = StaticPlanner.compose(&input(spec)).expect("計画できるべき");
        let titles: Vec<&str> = plan.tasks.iter().map(|t| t.title.as_str()).collect();
        plan.tasks
            .iter()
            .find(|t| t.title.contains(needle))
            .unwrap_or_else(|| panic!("{needle} のタスクが無い: {titles:?}"))
            .files
            .clone()
    }

    /// **本文にしか書かれていない担当ファイルを拾う。**
    ///
    /// これが空に戻ると、実機で起きた「7 タスク全部が未申告」に戻る。
    #[test]
    fn 本文に書かれた担当ファイルを拾う() {
        assert_eq!(files_of(SPEC, "HTML を書く"), vec!["index.html".to_string()]);
        assert_eq!(
            files_of(SPEC, "スタイルを書く"),
            vec!["css/style.css".to_string()]
        );
        assert_eq!(
            files_of(SPEC, "シーンを書く"),
            vec!["js/scene.js".to_string()]
        );
        assert_eq!(
            files_of(SPEC, "文章で固める"),
            vec!["docs/brief.md".to_string()]
        );
    }

    /// **URL・版番号・ただの語は担当ファイルではない。**
    #[test]
    fn urlと版番号と語は拾わない() {
        assert!(
            files_of(SPEC, "実現方式").is_empty(),
            "URL か版番号を担当ファイルにした: {:?}",
            files_of(SPEC, "実現方式")
        );
    }

    /// **拾えなかった節は空のまま。** 推測で埋めない。
    #[test]
    fn 拾えなかった節は空のまま() {
        assert!(files_of(SPEC, "ブラウザで開いて").is_empty());
    }

    /// **でっち上げない。** 申告したファイルは必ず SPEC の文面に現れる。
    #[test]
    fn 文面に無いファイルを作らない() {
        let plan = StaticPlanner.compose(&input(SPEC)).expect("計画できるべき");
        for t in &plan.tasks {
            for f in &t.files {
                assert!(
                    SPEC.contains(f.as_str()),
                    "SPEC に無いファイルを申告した: {f} ({})",
                    t.title
                );
            }
        }
    }

    /// **見出しに指定があるときは、これまでどおりそちらを使う。**
    /// 本文からの拾い上げは*見つからなかったときだけ*の後詰めである。
    #[test]
    fn 見出しの指定はこれまでどおり優先する() {
        let spec = "\
# 認証機能

## 要件
- ログイン API を実装する (src/auth/login.rs)
- トークン更新 API を実装する (src/auth/refresh.rs)

## 検証
- cargo test auth
";
        assert_eq!(
            files_of(spec, "ログイン API"),
            vec!["src/auth/login.rs".to_string()]
        );
    }

    /// 拾う / 拾わないを表で固定する (純関数なので入力を直に置ける)。
    ///
    /// **日本語は分かち書きしない。** 助詞が直に付いた `index.htmlを書く` を
    /// 落とすと、実機の SPEC の半分が未申告のまま残る。
    #[test]
    fn 拾うものと拾わないものを表で固定する() {
        let table: &[(&str, &[&str])] = &[
            ("3D キャンバスの置き場所とセクション構成を決めて index.html を書く。", &["index.html"]),
            ("配色・タイポグラフィ・余白を css/style.css に書く。", &["css/style.css"]),
            ("js/scene.js にシーン・カメラ・ライトを組む。", &["js/scene.js"]),
            ("ページの構成を docs/brief.md にまとめる。", &["docs/brief.md"]),
            // 助詞が直に付く / 括弧や強調に包まれる
            ("index.htmlを書く", &["index.html"]),
            ("**index.html** を書く", &["index.html"]),
            ("`docs/brief.md`にまとめる", &["docs/brief.md"]),
            // 拾ってはいけないもの
            ("https://unpkg.com/three@0.159.0/build/three.module.js から読み込む", &[]),
            ("https://example.com/assets/app.js を参照", &[]),
            ("three の r159 を使う", &[]),
            ("3D キャンバスを置く", &[]),
            ("バージョンは 1.2.3 に上げる", &[]),
            // 拡張子が無いものは推測になるので採らない
            ("src/auth のあたりを直す", &[]),
            ("特にファイルの指定は無い", &[]),
        ];
        for (text, want) in table {
            let got = files_in_text(text);
            let want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
            assert_eq!(got, want, "{text}");
        }
    }
}

#[cfg(test)]
mod scope_and_order_tests {
    use super::tests::input;
    use super::*;
    use crate::features::team::imp::model::TeamRole as R;

    /// 実機の形。ライブラリ名と、書く側・読む側の両方に出るファイル名を含む。
    const SPEC: &str = "\
# かっこいい 3D の Zaivern ホームページ

## architect: ファイル構成と 3D の実現方式を確定する
Three.js r159 をローカル同梱し、vendor/three.module.js から読み込む。docs/architecture.md に書く。

## implementer(markup): ページの HTML を書く
index.html を書く。Three.js は main.js から使う。

## implementer(style): スタイルを書く
css/style.css に書く。index.html のクラス名に合わせる。

## implementer(3d): 3D シーンを実装する
Three.js でシーンを組み、js/scene.js に書く。

## tester: 実際にブラウザで開いて確認する
index.html を開いて確認し、結果を docs/test.md に書く。

## integrator: 成果物を 1 つのサイトとして繋ぐ
README.md にまとめる。
";

    fn plan_of(spec: &str) -> Vec<crate::features::team::imp::model::TeamTask> {
        let mut inp = input(spec);
        inp.roles = vec![R::Planner, R::Architect, R::Implementer, R::Tester, R::Reviewer, R::Integrator];
        StaticPlanner.plan(inp).expect("計画できるべき").tasks
    }

    /// **2 つ以上の節に出てくる名前を担当ファイルにしない。**
    ///
    /// 散文から拾うと、書く側だけでなく**読む側の節にも同じ名前が出る**。
    /// 全部を担当にすると重なり判定が働き、実測で 5 本中 3 本が直列化した
    /// (`three.js` が 3 節、`index.html` が 3 節)。
    /// 「待っている役割が多すぎる」の再発そのもの。
    #[test]
    fn 節をまたぐ名前は誰の担当にもしない() {
        let tasks = plan_of(SPEC);
        let mut owner: std::collections::BTreeMap<&str, Vec<u64>> = Default::default();
        for t in &tasks {
            for f in &t.files {
                owner.entry(f.as_str()).or_default().push(t.id);
            }
        }
        for (f, ids) in &owner {
            assert_eq!(ids.len(), 1, "{f} を {ids:?} が同時に持っている (直列化する)");
        }
        // **その節にしか出てこないものは、ちゃんと担当になる。**
        let own: Vec<&str> = owner.keys().copied().collect();
        for want in ["css/style.css", "js/scene.js", "docs/test.md"] {
            assert!(own.contains(&want), "{want} が誰の担当にもなっていない: {own:?}");
        }
        // ライブラリ名は落ちる (3 節に出るので)。
        assert!(!own.contains(&"three.js"), "ライブラリ名を担当にした: {own:?}");
    }

    /// **整合担当は他の全部を待つ。**
    ///
    /// 実測で、依存を持たない `integrator:` の節が即座に配られ、
    /// 「他が未完了」という当然の理由で `blocked` → 人の判断待ちになった。
    /// 待たせれば担当の席が空くので、そのぶん他へ回せる。
    #[test]
    fn 整合担当は他を待つ() {
        let tasks = plan_of(SPEC);
        let integ = tasks
            .iter()
            .find(|t| t.role == R::Integrator)
            .expect("整合担当が居る");
        for t in &tasks {
            if t.id == integ.id || t.role == R::Integrator {
                continue;
            }
            assert!(
                integ.dependencies.contains(&t.id),
                "整合担当が #{} を待っていない",
                t.id
            );
        }
        // 同じ段の実装同士は待たせない。設計の確定だけを待ち、そこからは
        // 並列に進めることで、安全性のために必要な直列化以上は増やさない。
        let implementers: Vec<_> = tasks
            .iter()
            .filter(|t| t.role == R::Implementer)
            .collect();
        for task in &implementers {
            for peer in &implementers {
                if task.id != peer.id {
                    assert!(
                        !task.dependencies.contains(&peer.id),
                        "実装 #{} が同段の実装 #{} を待っている",
                        task.id,
                        peer.id
                    );
                }
            }
        }
    }

    #[test]
    fn 分割済みspecでも設計確定前に実装を先行させない() {
        let tasks = plan_of(SPEC);
        let planner = tasks
            .iter()
            .find(|t| t.role == R::Planner)
            .expect("Planner");
        let architect = tasks
            .iter()
            .find(|t| t.role == R::Architect)
            .expect("Architect");
        let implementers: Vec<_> = tasks
            .iter()
            .filter(|t| t.role == R::Implementer)
            .collect();
        assert!(!implementers.is_empty(), "前提: 実装タスクが無い");
        assert!(planner.dependencies.is_empty(), "Planner に先行依存がある");
        assert!(
            architect.dependencies.contains(&planner.id),
            "Architect が Planner の確定を待たない"
        );
        for task in implementers {
            assert!(
                task.dependencies.contains(&architect.id),
                "実装 #{} が設計確定前に先行する",
                task.id
            );
        }
    }

    #[test]
    fn spec側のintegratorも自動生成されたtesterを待つ() {
        let spec = "# Site\n\n## implementer(markup): HTMLを作る\nindex.html\n\n\
                    ## implementer(style): CSSを作る\nstyle.css\n\n\
                    ## integrator: 全体を統合する\nREADME.md\n";
        let tasks = plan_of(spec);
        let tester = tasks
            .iter()
            .find(|t| t.role == R::Tester)
            .expect("自動生成Tester");
        let integrator = tasks
            .iter()
            .find(|t| t.role == R::Integrator)
            .expect("SPEC側Integrator");
        assert!(
            integrator.dependencies.contains(&tester.id),
            "Integrator が最終テストより先に走る"
        );
        assert!(
            !tester.dependencies.contains(&integrator.id),
            "Integrator と Tester が互いを待つ循環になっている"
        );
    }
}

/// テストが `split_files` を外から呼ぶための口 (実装は 1 つのまま)。
#[cfg(test)]
pub(super) mod tests_hook {
    pub fn split_files_for_test(title: &str) -> (String, Vec<String>) {
        super::split_files(title)
    }
}
