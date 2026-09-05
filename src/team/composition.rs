//! 🧭 **編成の推奨** — 依頼の形から「何体・どの役割」を決める。
//!
//! ## なぜ要るか (実測)
//!
//! New Team Run の既定は **6 役割・4 体**で、依頼が何であれ同じだった。
//! 「かっこいい HP を作る」にこの既定を当てた Run は、SPEC 8 本のうち
//! 5 本が文書 (`PLAN.md` / `ARCHITECTURE.md` / `TEST.md` / `REVIEW.md` /
//! `README.md`) になり、計画は 14 本へ膨らみ、**25 分走って完了 0 件**、
//! 出来たページは読み込みエラーのままだった。同じ依頼を 1 体に普通に頼めば
//! 10 分で動くものが出る。**並列は速さを買う道具であって、品質を買う
//! 道具ではない** — 分けるほど繋ぎ目が増え、繋ぎ目の品質は誰の担当でも
//! なくなる。
//!
//! そして体を 1 つ増やすたびに**トークンも 1 体ぶん増える**。並列で
//! 速くならない仕事に体を立てるのは、遅くて高い。
//!
//! ## 判断の表
//!
//! | 依頼の形 | 体 | 役割 | 根拠 |
//! |---|---|---|---|
//! | [`WorkShape::SingleArtifact`] — 1 枚の HP / LP / 画面 / ロゴ / 記事 | 2 | 実装 + テスト | まとまりが品質。1 体が通しで作り、1 体が実際に開いて確かめる |
//! | [`WorkShape::WideIndependent`] — N 個の独立した単位 (エンドポイント・翻訳・移行・モジュール別のテスト) | N (上限まで) | 実装 + レビュー + 統合 | 各自が自分で検証できるので並列が丸ごと速さになる |
//! | [`WorkShape::FeatureInRepo`] — 既存リポジトリへの機能追加 | 2〜4 | (設計) + 実装 + テスト + レビュー | 接点 (型・API・ファイル) を先に固定しないと繋ぎ目で壊れる |
//! | [`WorkShape::Research`] — 調査・比較・検討 | 1 | 計画 | コードを書かない仕事に実装担当は要らない |
//!
//! ## ここに置くもの / 置かないもの
//!
//! * 置く: 依頼文と作業場の**観測結果**から編成を決める**純関数** ([`recommend`])
//!   と、その観測 ([`probe_workspace`] — I/O はここだけ)
//! * 置かない: 画面の描画・フォームへの反映 (`organization_board` /
//!   `app::team_glue` が持つ)。判断の表を 2 か所に持つと、片方だけ直した日に
//!   「画面のおすすめ」と「実際に使う編成」が食い違う
//!
//! ## 推定であることを隠さない
//!
//! 依頼文の分類は語の一致で行う。CLAUDE.md の「画面テキストの部分一致で
//! 状態を判定しない」は*エージェントの状態*の話で、ここは*人の書いた
//! 依頼*を読んで**人に提案する**層。外れることはあるので、(1) 理由を
//! 必ず画面に出し、(2) 人が手で変えた編成は上書きしない
//! (`NewRunForm::composition_touched`)。

use std::path::Path;

use super::model::TeamRole;

/// 依頼の形。編成はここから決まる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkShape {
    /// 1 枚の成果物 (HP / LP / 画面 / ロゴ / 記事)。まとまりが品質。
    SingleArtifact,
    /// 独立した単位が N 個 (エンドポイント / 翻訳 / 移行 / モジュール別のテスト)。
    WideIndependent,
    /// 既存リポジトリへの機能追加。接点の固定が要る。
    FeatureInRepo,
    /// 調査・比較・検討。コードを書かない。
    Research,
}

impl WorkShape {
    /// 一覧 (作法が全部の形で空でないことをテストが網羅する)。
    #[cfg(test)]
    pub const ALL: [WorkShape; 4] = [
        WorkShape::SingleArtifact,
        WorkShape::WideIndependent,
        WorkShape::FeatureInRepo,
        WorkShape::Research,
    ];
}

/// 作業場の観測結果。**I/O は [`probe_workspace`] だけ**が行う。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceProbe {
    /// `Cargo.toml` / `package.json` / `go.mod` / `pyproject.toml` /
    /// `src/` のような「既にコードがある」目印が 1 つでもあるか。
    pub has_repo_markers: bool,
    /// 隠しファイルを除いて、何かファイルがあるか (空フォルダなら false)。
    pub has_files: bool,
    /// どの作業場を見た結果か (フォルダを切り替えたら取り直す)。
    pub path: String,
}

/// 「既にコードがある」目印。**存在だけ**を見る (中身は解釈しない)。
const REPO_MARKERS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "requirements.txt",
    "pom.xml",
    "build.gradle",
    "Gemfile",
    "composer.json",
    "src",
    "lib",
    "app",
];

/// 作業場を観測する。**深くは歩かない** — 目印の存在と「空かどうか」だけ。
pub fn probe_workspace(ws: &Path) -> WorkspaceProbe {
    let path = ws.display().to_string();
    // 空のパスは「どの作業場でもない」。`Path::new("").join(x)` は cwd
    // 相対になるので、プロセスの作業ディレクトリを観測してしまう。
    if ws.as_os_str().is_empty() {
        return WorkspaceProbe {
            path,
            ..Default::default()
        };
    }
    let has_repo_markers = REPO_MARKERS.iter().any(|m| ws.join(m).exists());
    let has_files = std::fs::read_dir(ws)
        .map(|rd| {
            rd.flatten()
                .any(|e| !e.file_name().to_string_lossy().starts_with('.'))
        })
        .unwrap_or(false);
    WorkspaceProbe {
        has_repo_markers,
        has_files,
        path,
    }
}

/// 推奨した理由。画面には i18n を通して出す (`organization_board::reason_label`)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// 1 枚のものは分けるほど繋ぎ目が増える。
    SingleArtifact,
    /// 独立した単位が N 個ある。
    WideUnits,
    /// 既存のコードに合わせる (接点の固定が要る)。
    ExistingRepo,
    /// 空の作業場 (ゼロから作る)。
    EmptyWorkspace,
    /// 調査は実装担当が要らない。
    Research,
    /// 体を増やすほどトークンも増える。
    TokenCost,
}

/// 推奨した編成。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recommendation {
    pub shape: WorkShape,
    /// 同時に動かす体の数 (1〜`max_agents`)。
    pub agents: usize,
    /// チームに置く役割 (フォームの「エージェントプリセット」と同じ集合)。
    pub roles: Vec<TeamRole>,
    /// 数えた独立単位 (WideIndependent のとき、体の数の根拠)。
    pub units: usize,
    pub reasons: Vec<Reason>,
    /// レビュー専任のレーンを立てるか。1 枚の成果物ではテスト担当が
    /// その役を兼ねる (別レーンにすると往復が 1 回増える)。
    pub review_required: bool,
    /// 仕上げる時間の目安 (分)。1 枚の成果物は [`SINGLE_ARTIFACT_BUDGET_MIN`] —
    /// それを超えるなら、分け方か依頼の大きさが間違っている。
    pub time_budget_min: Option<u32>,
}

/// 1 枚の成果物を仕上げる時間の目安 (分)。
///
/// 「10 分で高品質な HP が出ないならチームの意味が無い」が利用者の物差し。
/// 6 体・14 本で 90 分かけて完了 0 件だった Run の対極として、
/// **2 体・2 本・10 分**をここで数にしておく。
pub const SINGLE_ARTIFACT_BUDGET_MIN: u32 = 10;

/// 1 枚の成果物を**強く**指す語。**先頭一致ではなく含有**で見る (「HPを」「LP作って」)。
///
/// 既存リポジトリの中でこれらが出たら、それでも 1 枚の成果物。
const ARTIFACT_WORDS: &[&str] = &[
    "hp",
    "ホームページ",
    "ランディング",
    "lp",
    "ウェブページ",
    "webページ",
    "web ページ",
    "サイト",
    "ロゴ",
    "バナー",
    "記事",
    "ポスター",
    "スライド",
    "プレゼン",
    "ポートフォリオ",
    "landing",
    "homepage",
    "website",
    "web page",
    "webpage",
    "logo",
    "banner",
    "poster",
    "slide deck",
    "portfolio",
];

/// 1 枚の成果物を**弱く**指す語。空の作業場でだけ効く —
/// 既存リポジトリの「設定画面にフォントサイズを足す」は機能追加であって、
/// 画面を 1 枚作る依頼ではない。
const WEAK_ARTIFACT_WORDS: &[&str] = &["ページ", "画面", "ui", "デザイン", "mockup", "page", "screen"];

/// 独立した単位が並ぶことを示す語。
const WIDE_WORDS: &[&str] = &[
    "移行",
    "翻訳",
    "一括",
    "全ファイル",
    "全モジュール",
    "それぞれ",
    "ごとに",
    "エンドポイント",
    "多言語",
    "各画面",
    "各ページ",
    "各モジュール",
    "各エンドポイント",
    "テストを足す",
    "テストを追加",
    "テストを書く",
    "migrate",
    "migration",
    "translate",
    "localize",
    "i18n",
    "endpoints",
    "for each",
    "every module",
    "all modules",
    "all files",
    "add tests",
];

/// 調査・検討を示す語。
const RESEARCH_WORDS: &[&str] = &[
    "調査",
    "調べて",
    "比較",
    "検討",
    "評価して",
    "洗い出",
    "分析して",
    "research",
    "investigate",
    "compare",
    "evaluate",
    "survey",
    "analyze",
];

/// 実装を示す語 (調査の語と同居したら実装のほうを採る)。
const BUILD_WORDS: &[&str] = &[
    "作る",
    "作って",
    "実装",
    "追加",
    "直す",
    "修正",
    "build",
    "implement",
    "create",
    "make",
    "add",
    "fix",
];

fn contains_any(text: &str, words: &[&str]) -> bool {
    words.iter().any(|w| text.contains(w))
}

/// 依頼の中の**独立した単位**を数える (純関数)。
///
/// 2 つの物差しの大きいほう:
/// * 箇条書きの行数 (`- ` / `* ` / `1.`)
/// * 「12 個」「6 言語」「30 ファイル」のような数え上げ
///
/// 見出しの下の説明文は数えない。数えるのは「これが 1 単位」と読める行だけ。
///
/// **SPEC の形 (`## タスク` の見出しがある) なら、その節の箇条書きだけ数える。**
/// 完了条件の箇条書きまで数えると、実装 2 本の SPEC が「7 単位」になって
/// 7 体立ててしまう (書き換えた SPEC を計画へ渡すときに実際に踏む)。
pub fn count_units(brief: &str) -> usize {
    let task_heading = |l: &str| {
        let t = l.trim_start_matches('#').trim().to_lowercase();
        t.contains("タスク") || t.contains("task") || t.contains("todo") || t.contains("やること")
    };
    let has_task_heading = brief
        .lines()
        .any(|l| l.trim_start().starts_with('#') && task_heading(l));
    let mut in_task = !has_task_heading;
    let bullets = brief
        .lines()
        .map(str::trim_start)
        .filter(|l| {
            if has_task_heading && l.starts_with('#') {
                in_task = task_heading(l);
                return false;
            }
            if !in_task {
                return false;
            }
            l.starts_with("- ")
                || l.starts_with("* ")
                || l.starts_with("・")
                || l
                    .split_once(['.', ')', '、'])
                    .is_some_and(|(n, rest)| {
                        !n.is_empty()
                            && n.chars().all(|c| c.is_ascii_digit())
                            && !rest.is_empty()
                    })
        })
        .count();
    let counted = counted_number(brief);
    bullets.max(counted)
}

/// 「12 個」「6 言語」「30 ファイル」「5 endpoints」のような数え上げの最大値。
///
/// 単位の語を伴わない数字 (「2026 年」「3D」「v2」) は数えない —
/// **数字があるだけで並列にしない**。
const COUNT_UNITS: &[&str] = &[
    "個",
    "本",
    "件",
    "つ",
    "枚",
    "言語",
    "画面",
    "ページ",
    "ファイル",
    "モジュール",
    "エンドポイント",
    "テーブル",
    "コマンド",
    "関数",
    "機能",
    "endpoints",
    "endpoint",
    "modules",
    "module",
    "files",
    "file",
    "pages",
    "page",
    "languages",
    "language",
    "screens",
    "screen",
    "tables",
    "table",
    "functions",
    "function",
    "features",
    "feature",
    "items",
    "item",
];

fn counted_number(brief: &str) -> usize {
    let lower = brief.to_lowercase();
    let bytes = lower.as_bytes();
    let mut best = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let Ok(n) = lower[start..i].parse::<usize>() else {
            continue;
        };
        // 数字の直後 (空白を挟んでもよい) に単位の語があるか。
        let rest = lower[i..].trim_start();
        // 「2 つの」「3 個の」のように短い単位を優先して見る。
        if COUNT_UNITS.iter().any(|u| rest.starts_with(u)) {
            best = best.max(n);
        }
    }
    // 4 桁以上は年やポート番号なので数えない。
    if best >= 1000 {
        0
    } else {
        best
    }
}

/// 依頼の形を決める (純関数)。
pub fn classify(brief: &str, probe: &WorkspaceProbe) -> (WorkShape, usize) {
    let text = brief.to_lowercase();
    let units = count_units(brief);
    let artifact = contains_any(&text, ARTIFACT_WORDS)
        || (!probe.has_repo_markers && contains_any(&text, WEAK_ARTIFACT_WORDS));
    let wide = contains_any(&text, WIDE_WORDS);
    let research = contains_any(&text, RESEARCH_WORDS) && !contains_any(&text, BUILD_WORDS);

    if research {
        return (WorkShape::Research, units);
    }
    // 独立した単位が 3 つ以上並ぶなら、1 枚の語があっても「複数枚」。
    // (「5 ページのサイト」は 5 単位)
    if units >= 3 || wide {
        return (WorkShape::WideIndependent, units.max(3));
    }
    if artifact {
        return (WorkShape::SingleArtifact, units);
    }
    if probe.has_repo_markers {
        return (WorkShape::FeatureInRepo, units);
    }
    // 空の作業場に「作って」と言われた = 1 枚の成果物として扱う。
    (WorkShape::SingleArtifact, units)
}

/// 編成を推奨する (純関数)。`max_agents` はフォームのスライダの上限。
pub fn recommend(brief: &str, probe: &WorkspaceProbe, max_agents: usize) -> Recommendation {
    use TeamRole as R;
    let max = max_agents.max(1);
    let (shape, units) = classify(brief, probe);
    let mut reasons = Vec::new();
    let (agents, roles) = match shape {
        WorkShape::SingleArtifact => {
            reasons.push(Reason::SingleArtifact);
            if !probe.has_files {
                reasons.push(Reason::EmptyWorkspace);
            }
            (2, vec![R::Implementer, R::Tester])
        }
        WorkShape::WideIndependent => {
            reasons.push(Reason::WideUnits);
            let n = units.max(3).min(max);
            (n, vec![R::Implementer, R::Reviewer, R::Integrator])
        }
        WorkShape::FeatureInRepo => {
            reasons.push(Reason::ExistingRepo);
            // 2 単位以上なら接点を先に固定する担当を置く。
            let mut roles = vec![R::Implementer, R::Tester, R::Reviewer];
            let n = if units >= 2 {
                roles.insert(0, R::Architect);
                units.max(3).min(4).min(max)
            } else {
                2.min(max)
            };
            (n, roles)
        }
        WorkShape::Research => {
            reasons.push(Reason::Research);
            (1, vec![R::Planner])
        }
    };
    reasons.push(Reason::TokenCost);
    let (review_required, time_budget_min) = match shape {
        WorkShape::SingleArtifact => (false, Some(SINGLE_ARTIFACT_BUDGET_MIN)),
        WorkShape::Research => (false, None),
        WorkShape::WideIndependent | WorkShape::FeatureInRepo => (true, None),
    };
    Recommendation {
        shape,
        agents: agents.clamp(1, max),
        roles,
        units,
        reasons,
        review_required,
        time_budget_min,
    }
}


/// 1 枚の成果物の SPEC を**こちらで書く** (純関数)。
///
/// 書き換えの段は headless のエージェントに最大 5 分待つ。1 枚の成果物の
/// SPEC は毎回同じ形 (実装 1 本 + 検証 1 本) なので待つ理由が無い —
/// 10 分の予算のうち 5 分を「仕様書を書いてもらう」ことに使うのは
/// 本末転倒。返した文面は従来どおり人が確認してから採用する。
///
/// 形の決まり (計画がそのまま読める):
/// * 箇条書きの先頭に役割の名乗り (`implementer:` / `tester:`)
/// * 担当ファイルは**行末の** `(files: …)` 1 つだけ。それより後ろに
///   括弧を置かない (`planner::split_files` は最後の括弧を見る)
/// * 完了条件は**測れる形** — 何が在るか・何が出ないか・どの幅か
pub fn spec_template(goal: &str, brief: &str, rec: &Recommendation) -> Option<String> {
    if rec.shape != WorkShape::SingleArtifact {
        return None;
    }
    let budget = rec.time_budget_min.unwrap_or(SINGLE_ARTIFACT_BUDGET_MIN);
    let goal = goal.trim();
    let brief = brief.trim();
    let title = if goal.is_empty() {
        brief.lines().next().unwrap_or("Web ページ").trim()
    } else {
        goal
    };
    // 依頼文の改行は 1 行に畳む (箇条書きの行にすると計画が別タスクに割る)。
    let ask: String = if brief.is_empty() { title } else { brief }
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!(
        "# {title}\n\
         \n\
         ## タスク\n\
         - implementer: 「{ask}」を 1 枚のページとして**通しで**作る。HTML・CSS・JS を全部このタスクが持ち、\
         見た目のまとまり — 配色・余白・文字の大きさ・動き — を 1 人で決める。\
         **{budget} 分で仕上げる**: 凝る前に動くものを出し、残りの時間で磨く。\
         文言はプレースホルダを使わず、依頼から読み取れる実際の内容で書く。\
         外部ライブラリは CDN の URL で読むか、`assets/vendor/` に自分で置く — \
         読み込むと書いたのに置かないファイルを 1 つも残さない。\
         375px 幅と 1280px 幅の両方で崩れないこと。\
         できたら `zai team check` を自分でも走らせてから完了報告する \
         (files: index.html assets/css/style.css assets/js/main.js assets/vendor/**)\n\
         - tester: 実装担当が「できた」と言ったら**実際に開いて確かめる**。`zai team check` で \
         読み込みエラーとブラウザのコンソールエラーを見て、`zai team shot` で 375px と 1280px の \
         画像を撮って崩れ・読めない文字・埋もれた文字を見る。直すべき点は具体的な箇所と直し方を \
         伝言で実装担当へ返し、直ったのをもう一度開いて確認してから完了にする。\
         手順書・レビュー記録・README は書かない — 直すのはページであって文書ではない\n\
         \n\
         ## 完了条件\n\
         - `index.html` を開くと、見出し・説明・行動を促すボタン・フッターが実際の文言で表示される\n\
         - `index.html` が読み込むローカルファイルがすべて実在する (`zai team check` が緑)\n\
         - ブラウザのコンソールにエラーが 0 件\n\
         - 375px 幅と 1280px 幅の両方で横スクロールが出ず、文字が背景に埋もれず読める\n\
         - 開始から {budget} 分以内に上の条件を満たしている\n"
    ))
}

/// 書き換え依頼文へ載せる**形ごとの作法** (純関数)。
///
/// [`super::spec_writer::build_prompt`] がこれをそのまま貼る。編成を
/// 決めた層と作法を書く層が別々に判断すると、「2 体と言ったのに 8 本に
/// 割る」ような食い違いが出る。
pub fn spec_guidance(shape: WorkShape) -> &'static str {
    match shape {
        WorkShape::SingleArtifact => {
            "* **成果物は 1 枚。実装は 1 本のタスクにまとめる** — HTML / CSS / JS の\
             ように 1 つのものを成す部品は**同じ担当が通しで作る**。分けると\
             繋ぎ目 (読み込み順・命名・配色) が誰の担当でもなくなる\n\
             * テスト担当のタスクは「**実際に開いて確かめ、崩れは伝言で実装担当へ\
             返す**」。確認手順書は作らない (手順書はページを直さない)\n\
             * 品質の物差しを完了条件に**具体的に**書く: 何を読み込むか・\
             どの幅で崩れないか・コンソールにエラーが無いか"
        }
        WorkShape::WideIndependent => {
            "* **単位ごとに 1 本**。各タスクは他のタスクの完成を待たずに\
             始められ、**自分だけで正しさを確かめられる**形にする\
             (検証コマンド、または具体的な確認手順を完了条件に書く)\n\
             * 単位をまたぐ共通の型・命名・ファイル配置があるなら、それを\
             **最初の 1 本**として先に固定する (全員がそれに従う)"
        }
        WorkShape::FeatureInRepo => {
            "* **既存の構造に合わせる**。新しい流儀を持ち込まない\n\
             * 接点 (型・API・ファイル配置) を先に固定するタスクを 1 本置き、\
             実装はそれに従う。接点が決まる前に並列で書き始めると繋ぎ目で壊れる\n\
             * 既存のテストを壊さないことを完了条件に含める"
        }
        WorkShape::Research => {
            "* **コードは書かない**。結論と根拠 (出典・実測) を 1 枚にまとめる\n\
             * 比較なら観点を先に列挙し、観点ごとに埋める"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TeamRole as R;

    fn empty() -> WorkspaceProbe {
        WorkspaceProbe::default()
    }

    fn repo() -> WorkspaceProbe {
        WorkspaceProbe {
            has_repo_markers: true,
            has_files: true,
            path: "repo".into(),
        }
    }

    /// **実機の依頼そのもの。** 6 役割・4 体ではなく 2 体になる。
    #[test]
    fn かっこいいhpは2体で実装とテスト() {
        for brief in [
            "かっこいいHPを作る",
            "かっこいい３DのWebページを作って",
            "会社のランディングページ",
            "Build a landing page for my app",
            "ポートフォリオサイトを作りたい",
        ] {
            let r = recommend(brief, &empty(), 16);
            assert_eq!(r.shape, WorkShape::SingleArtifact, "{brief}");
            assert_eq!(r.agents, 2, "{brief}");
            assert_eq!(r.roles, vec![R::Implementer, R::Tester], "{brief}");
            assert!(r.reasons.contains(&Reason::SingleArtifact), "{brief}");
            assert!(r.reasons.contains(&Reason::TokenCost), "理由に費用が無い");
        }
    }

    /// 独立した単位が並ぶ依頼は、単位の数だけ体を立てる。
    #[test]
    fn 独立した単位は数だけ並列にする() {
        let r = recommend("12 個のエンドポイントを実装する", &repo(), 16);
        assert_eq!(r.shape, WorkShape::WideIndependent);
        assert_eq!(r.agents, 12);
        assert_eq!(r.units, 12);
        assert_eq!(r.roles, vec![R::Implementer, R::Reviewer, R::Integrator]);

        // 上限で止まる。
        let r = recommend("30 ファイルを移行する", &repo(), 8);
        assert_eq!(r.agents, 8);

        // 語だけでも並列 (数が無ければ 3 から)。
        let r = recommend("全モジュールにテストを足す", &repo(), 16);
        assert_eq!(r.shape, WorkShape::WideIndependent);
        assert_eq!(r.agents, 3);

        // 英語。
        let r = recommend("Translate the UI into 6 languages", &repo(), 16);
        assert_eq!(r.shape, WorkShape::WideIndependent);
        assert_eq!(r.agents, 6);
    }

    /// 箇条書きの行数も単位として数える。
    #[test]
    fn 箇条書きは単位として数える() {
        let brief = "認証を作る\n- ログイン\n- ログアウト\n- パスワード再設定\n- 2 段階認証";
        assert_eq!(count_units(brief), 4);
        let r = recommend(brief, &repo(), 16);
        assert_eq!(r.shape, WorkShape::WideIndependent);
        assert_eq!(r.agents, 4);
    }

    /// **数字があるだけでは並列にしない。** 「3D」「2026 年」は単位ではない。
    #[test]
    fn 単位を伴わない数字は数えない() {
        assert_eq!(count_units("かっこいい3DのHP"), 0);
        assert_eq!(count_units("2026 年版のサイト"), 0);
        assert_eq!(count_units("v2 に上げる"), 0);
        assert_eq!(count_units("ポート 8899 を使う"), 0);
        assert_eq!(count_units("5 ページのサイト"), 5);
        assert_eq!(count_units("3 endpoints"), 3);
    }

    /// 既存リポジトリへの機能追加は、接点を固定する担当を置く。
    #[test]
    fn 既存リポジトリの機能追加は設計を先に置く() {
        // 1 単位: 小さく。
        let r = recommend("設定画面にフォントサイズを足す", &repo(), 16);
        assert_eq!(r.shape, WorkShape::FeatureInRepo);
        assert_eq!(r.agents, 2);
        assert!(!r.roles.contains(&R::Architect));
        assert!(r.reasons.contains(&Reason::ExistingRepo));

        // 2 単位: 設計を先頭に。
        let r = recommend("- 設定モデルに font_size を足す\n- 設定画面に UI を足す", &repo(), 16);
        assert_eq!(r.shape, WorkShape::FeatureInRepo);
        assert_eq!(r.roles[0], R::Architect);
        assert!(r.agents >= 3);
    }

    /// 調査は実装担当が要らない。
    #[test]
    fn 調査は1体で計画担当() {
        let r = recommend("競合 3 社の料金体系を調査して比較する", &repo(), 16);
        assert_eq!(r.shape, WorkShape::Research);
        assert_eq!(r.agents, 1);
        assert_eq!(r.roles, vec![R::Planner]);
        // 「調査して実装する」は実装。
        let r = recommend("ライブラリを調査して、選んだものでログイン画面を作る", &empty(), 16);
        assert_ne!(r.shape, WorkShape::Research);
    }

    /// 空の作業場に「作って」は 1 枚の成果物。
    #[test]
    fn 空の作業場は1枚の成果物として扱う() {
        let r = recommend("家計簿アプリを作って", &empty(), 16);
        assert_eq!(r.shape, WorkShape::SingleArtifact);
        assert!(r.reasons.contains(&Reason::EmptyWorkspace));
    }

    /// 体の数は必ず 1〜上限に収まる。
    #[test]
    fn 体の数は上限に収まる() {
        for brief in ["HP", "12 個の API", "調査", "機能を足す", ""] {
            for max in [1, 2, 4, 16] {
                let r = recommend(brief, &repo(), max);
                assert!((1..=max).contains(&r.agents), "{brief} / max={max} → {}", r.agents);
            }
        }
    }

    /// **形ごとの作法は必ず何か言う。** 空の作法を貼ると、書き換え依頼文に
    /// 空行だけが残って「形を伝えた」ことにならない。
    #[test]
    fn 形ごとの作法は空でない() {
        for s in WorkShape::ALL {
            assert!(!spec_guidance(s).trim().is_empty(), "{:?}", s);
            assert!(spec_guidance(s).starts_with("* "), "箇条書きで始める: {:?}", s);
        }
        // 1 枚の成果物は「1 本にまとめる」と言い切る。
        assert!(spec_guidance(WorkShape::SingleArtifact).contains("1 本のタスクにまとめる"));
    }


    /// **1 枚の成果物はレビュー専任を立てず、10 分の予算を持つ。**
    #[test]
    fn 一枚の成果物はレビュー無しで10分() {
        let r = recommend("かっこいいHPを作る", &empty(), 16);
        assert!(!r.review_required, "テスト担当が兼ねるのに別レーンを立てた");
        assert_eq!(r.time_budget_min, Some(SINGLE_ARTIFACT_BUDGET_MIN));
        let r = recommend("12 個のエンドポイントを実装する", &repo(), 16);
        assert!(r.review_required, "独立した単位にレビューが無い");
        assert_eq!(r.time_budget_min, None);
    }

    /// **雛形は計画がそのまま読める。** 実装 1 本 + 検証 1 本に割れ、
    /// 役割が付き、実装は Web の 3 ファイルと vendor を持つ。
    #[test]
    fn 一枚の成果物の雛形は計画がそのまま読める() {
        use super::super::planner;
        let rec = recommend("かっこいいHPを作る", &empty(), 16);
        let spec = spec_template("かっこいい HP", "かっこいいHPを作る", &rec).expect("雛形が出る");
        assert!(!planner::needs_spec_rewrite(&spec), "雛形なのに書き換えが要ると言う");
        let sections = planner::parse_sections(&spec);
        let seeds = planner::implementation_seeds(&sections, "かっこいい HP");
        assert_eq!(seeds.len(), 2, "実装 1 本 + 検証 1 本でない: {seeds:?}");
        assert!(seeds[0].title.starts_with("implementer: "), "{}", seeds[0].title);
        assert!(seeds[1].title.starts_with("tester: "), "{}", seeds[1].title);
        // 担当ファイルは行末の (files: …) だけ。
        let mut i = super::super::planner::tests_hook::split_files_for_test(&seeds[0].title);
        i.1.sort();
        assert_eq!(
            i.1,
            vec![
                "assets/css/style.css".to_string(),
                "assets/js/main.js".to_string(),
                "assets/vendor/**".to_string(),
                "index.html".to_string(),
            ]
        );
        assert!(spec.contains("10 分"), "時間の予算が載っていない");
        // 完了条件は測れる形 (何が在るか・何が出ないか・どの幅か)。
        for must in ["zai team check", "コンソール", "375px", "1280px"] {
            assert!(spec.contains(must), "完了条件に {must} が無い");
        }
        // 雛形そのものを計画へ渡しても、体の数は 2 のまま (完了条件の
        // 箇条書きを単位として数えない)。
        let again = recommend(&spec, &empty(), 16);
        assert_eq!(again.shape, WorkShape::SingleArtifact, "雛形を読ませたら形が変わった");
        assert_eq!(again.agents, 2);
    }

    /// 1 枚の成果物でなければ雛形は出さない (書き換えはエージェントに頼む)。
    #[test]
    fn 一枚の成果物でなければ雛形を出さない() {
        for brief in ["12 個のエンドポイントを実装する", "競合を調査して比較する"] {
            let rec = recommend(brief, &repo(), 16);
            assert!(spec_template("x", brief, &rec).is_none(), "{brief}");
        }
    }


    /// **確かめる担当は、確かめるものができてから配る。** 雛形を計画へ
    /// 通すと、`tester:` は `implementer:` に依存する (同時に配られて
    /// 「まだ無い」と待ちに入り、停滞として人へ上げられた実測がある)。
    #[test]
    fn 雛形の検証担当は実装を待つ() {
        use super::super::planner::{PlanInput, StaticPlanner, TeamPlanner};
        let rec = recommend("かっこいいHPを作る", &empty(), 16);
        let spec = spec_template("HP", "かっこいいHPを作る", &rec).expect("雛形");
        let plan = StaticPlanner
            .plan(PlanInput {
                spec,
                source: "SPEC.md".into(),
                agent_count: 2,
                review_required: false,
                workspace_root: std::path::PathBuf::new(),
                roles: rec.roles.clone(),
            })
            .expect("計画できる");
        let implementer = plan
            .tasks
            .iter()
            .find(|t| t.role == TeamRole::Implementer)
            .expect("実装");
        let tester = plan
            .tasks
            .iter()
            .find(|t| t.role == TeamRole::Tester)
            .expect("検証");
        assert!(
            tester.dependencies.contains(&implementer.id),
            "検証担当が実装を待っていない: {:?}",
            tester.dependencies
        );
        assert!(implementer.dependencies.is_empty(), "実装が何かを待っている");
    }

    /// 観測は実ファイルで往復する。**空フォルダと目印付きを見分ける。**
    #[test]
    fn 作業場の観測は空と目印を見分ける() {
        let dir = crate::test_util::unique_temp_dir("zaivern", "composition");
        let p = probe_workspace(&dir);
        assert!(!p.has_files, "空フォルダなのに何かある");
        assert!(!p.has_repo_markers);

        std::fs::write(dir.join(".DS_Store"), "").unwrap();
        let p = probe_workspace(&dir);
        assert!(!p.has_files, "隠しファイルを数えた");

        std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        let p = probe_workspace(&dir);
        assert!(p.has_files);
        assert!(p.has_repo_markers);
        assert_eq!(p.path, dir.display().to_string());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 空のパスは cwd を観測しない。
    #[test]
    fn 空のパスは何も観測しない() {
        let p = probe_workspace(Path::new(""));
        assert_eq!(p, WorkspaceProbe::default());
    }
}
