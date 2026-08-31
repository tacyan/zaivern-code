//! エージェントへ渡す指示文の組み立て (純関数)。
//!
//! ## 何を必ず入れるか
//!
//! 指示から 1 つでも欠けると、エージェントは欠けた項目を**自分で決める**。
//! 「編集禁止範囲」が抜ければ担当外を触り、「完了報告フォーマット」が
//! 抜ければ自然言語で「終わりました」と言う — どちらも後で拒否することに
//! なり、往復が 1 回増える。だから [`implementer`] は必須項目を
//! 網羅し、[`tests`] がその網羅を固定する。
//!
//! ## ワークスペース境界
//!
//! 指示文に絶対パスを焼き込まない。**ワークスペースルートは 1 行だけ**
//! 示し、それ以外はすべて相対で書く (どのマシンでも同じ文面になる)。

use super::model::{TeamGoal, TeamRole, TeamTask};
use super::validation_command::ValidationCommand;

/// 指示文の上限 (バイト)。長い指示は途中で切られて意味が壊れるので、
/// **こちらで切ってから渡す**。
pub const PROMPT_MAX_BYTES: usize = 8_000;

/// 指示に添える材料。
#[derive(Clone, Debug)]
pub struct Brief<'a> {
    pub goal: &'a TeamGoal,
    pub task: &'a TeamTask,
    /// 自分のエージェント ID (報告に必ず載せてもらう)。
    pub agent_id: &'a str,
    /// 親エージェント (居れば)。
    pub parent_id: Option<&'a str>,
    /// ワークスペースルート (表示用の 1 行)。
    pub workspace_root: &'a str,
    /// 依存タスクの結果 (要約)。
    pub upstream: Vec<String>,
    /// このタスクが**触ってはいけない**ファイル (他タスクの担当範囲)。
    pub forbidden_files: Vec<String>,
    /// 報告を書き出すフォルダ (**画面ではなくここから読む**)。
    pub outbox: std::path::PathBuf,
    /// **同じチームの顔ぶれ** `(ID, 役割の表示名)`。
    ///
    /// 誰が居るか分からなければ伝言のしようがない (宛先を捏造するだけ)。
    pub teammates: Vec<(String, String)>,
}

/// 検証コマンドを**見出しの一覧**にする (指示文へ載せるため)。
///
/// 実行はここを通らない — 実行は構造化した形のまま実行器へ渡る。
fn command_labels(cmds: &[ValidationCommand]) -> Vec<String> {
    cmds.iter().map(|c| c.display()).collect()
}

fn bullets(items: &[String]) -> String {
    if items.is_empty() {
        return "  (なし)\n".to_string();
    }
    items
        .iter()
        .map(|s| format!("  - {s}\n"))
        .collect::<String>()
}

/// 上限で切る。切ったことが分かるようにする。
fn cap(s: String) -> String {
    if s.len() <= PROMPT_MAX_BYTES {
        return s;
    }
    let mut cut = PROMPT_MAX_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n…(指示が長いため切り詰めました)\n", &s[..cut])
}

/// 完了報告のひな型。**全役割で同じ 1 本**を使う。
fn result_format(task_id: u64, agent_id: &str, outbox: &std::path::Path) -> String {
    // **ファイルへ書かせるのが本線。** 画面へ出すだけだと、カーソル移動で
    // 描く CLI (Claude Code v2) では行が潰れて届かない。画面にも出させるのは
    // 人が読むためで、こちらは控え。
    let file = if outbox.as_os_str().is_empty() {
        String::new()
    } else {
        format!(
            "**まず次のファイルへ同じ JSON を書いてください** (これが正式な提出です)。\n\
             `{}/{agent_id}.json`\n\
             フォルダが無ければ作ってください。書けたら、下の形で画面にも出してください\n\
             (画面のほうは人が読むための控えです)。\n\n",
            outbox.display()
        )
    };
    format!(
        "{file}作業が終わったら、次の形式を**そのまま**出力してください (前後に説明を書いてよい)。\n\
         この形式以外での完了報告は受け付けません。\n\n\
         {open}\n\
         {{\n\
         \x20 \"task_id\": {task_id},\n\
         \x20 \"agent_id\": \"{agent_id}\",\n\
         \x20 \"status\": \"completed\",\n\
         \x20 \"summary\": \"何をしたかの 1 行\",\n\
         \x20 \"changed_files\": [\"変更したファイル\"],\n\
         \x20 \"validation\": [{{\"command\": \"実行した検証コマンド\", \"exit_code\": 0}}],\n\
         \x20 \"blockers\": []\n\
         }}\n\
         {close}\n\n\
         * 検証コマンドを実際に実行し、その終了コードを正直に書くこと\n\
         * 担当外のファイルを変更しないこと (変更すると報告が却下されます)\n\
         * 進められない場合は status を \"blocked\" にし、blockers に理由を書くこと\n",
        open = super::result_parser::RESULT_OPEN,
        close = super::result_parser::RESULT_CLOSE,
    )
}

/// **自分が中で使ったサブエージェントの知らせ方。**
///
/// Zaivern は `[ZAI-TEAM-EVENT]` を読んで盤面へ子として並べる仕組みを
/// 持っているのに、**指示文がそれを一言も伝えていなかった**ので、誰も
/// 報告せず、盤面には一度も現れなかった (作ってあるのに繋がっていない)。
///
/// 出させるのは**始まりと終わり**だけ。実況中継させると、盤面が流れて
/// 「いま誰が何をしているか」が読めなくなる。
fn subagents_section(agent_id: &str) -> String {
    format!(
        "\n## 中で誰かに手伝わせたとき\n\
         あなたが内部でサブエージェントを使ったら、**始めたときと終えたとき**に\n\
         次を出してください (Zaivern の盤面へ、あなたの下にぶら下がって出ます)。\n\n\
         {open}\n\
         {{\"kind\": \"sub_agent_started\", \"agent_id\": \"<子の名前>\", \
         \"parent_id\": \"{agent_id}\", \"role\": \"implementer\", \
         \"action\": \"何をさせるか 1 行\"}}\n\
         {close}\n\n\
         終えたら同じ形で `\"kind\": \"sub_agent_completed\"` を出してください\n\
         (失敗なら `sub_agent_failed`、詰まったなら `sub_agent_blocked`)。\n\
         * 実況中継はしない。始まりと終わりだけ\n\
         * `parent_id` は必ず `{agent_id}` (あなた自身)\n",
        open = super::result_parser::EVENT_OPEN,
        close = super::result_parser::EVENT_CLOSE,
    )
}

/// **チームの顔ぶれと、仲間への伝言の作法。**
///
/// 伝言できることを指示文に書かなければ、エージェントは一生使わない
/// (機能があっても到達経路が無いのと同じ)。
fn teammates_section(mates: &[(String, String)]) -> String {
    if mates.is_empty() {
        return String::new();
    }
    let list: String = mates
        .iter()
        .map(|(id, role)| format!("* `{id}` — {role}\n"))
        .collect();
    format!(
        "\n## チームの仲間\n{list}\n\
         区切りが付いたときや、相手が待っていることが分かったときは、\
         次の形で**その相手へ直接伝えてください** (Zaivern が相手の端末へ届けます)。\n\n\
         {open}\n\
         {{\"to\": \"<上の ID か役割、全員なら all>\", \
         \"text\": \"ここまで出来た / 次にこれをする、を 1〜3 行で\"}}\n\
         {close}\n\n\
         * 伝えるのは**相手の仕事が変わるとき**だけ。実況中継はしない\n\
         * 本文は {max} 文字まで。長い成果物はファイルに書いて、場所だけ伝える\n\
         * 相手が居ない ID を書かない (届かず、こちらに断りが記録されます)\n",
        open = super::result_parser::MSG_OPEN,
        close = super::result_parser::MSG_CLOSE,
        max = super::result_parser::MSG_MAX_CHARS,
    )
}

/// 実装担当への指示。
pub fn implementer(b: &Brief<'_>) -> String {
    let t = b.task;
    let mut s = String::new();
    s.push_str(
        "あなたは Zaivern の AI 開発チームの一員です。以下の指示だけに従って作業してください。\n\n",
    );
    s.push_str(&format!("## Goal\n{}\n\n", b.goal.title));
    s.push_str("## Definition of Done (Goal 全体)\n");
    s.push_str(&bullets(&b.goal.definition_of_done));
    s.push_str(&format!("\n## あなたの担当タスク\n#{} {}\n", t.id, t.title));
    if !t.description.is_empty() {
        s.push_str(&format!("\n{}\n", t.description));
    }
    s.push_str("\n## 受入基準 (すべて満たすこと)\n");
    s.push_str(&bullets(&t.acceptance_criteria));
    // **コードを書かない役割には編集範囲を出さない。** 出すと「触ってよい」
    // と読まれる (レビュアーに変更させないための線)。
    if !super::roles::writes_code(t.role) {
        s.push_str("\n## このタスクではコードを変更しません\n");
        s.push_str("  - 読んで判断するだけです\n");
    }
    s.push_str("\n## 編集してよいファイル\n");
    if t.files.is_empty() {
        s.push_str("  (指定なし。ただしワークスペースの外へは絶対に書かないこと)\n");
    } else {
        s.push_str(&bullets(&t.files));
    }
    s.push_str("\n## 編集してはいけない範囲\n");
    if b.forbidden_files.is_empty() {
        s.push_str("  - ワークスペースの外 (絶対パス・`..` での脱出)\n");
    } else {
        s.push_str("  - ワークスペースの外 (絶対パス・`..` での脱出)\n");
        s.push_str(&bullets(&b.forbidden_files));
        s.push_str("    (上記は他の担当が同時に編集しています)\n");
    }
    s.push_str("\n## 依存タスクの結果\n");
    s.push_str(&bullets(&b.upstream));
    if !t.context.is_empty() {
        s.push_str("\n## 引き継ぎ・レビュー指摘\n");
        s.push_str(&bullets(&t.context));
    }
    s.push_str("\n## 実行する検証コマンド\n");
    s.push_str(&bullets(&command_labels(&t.validation_commands)));
    s.push_str(&format!(
        "\n## 体制\n  - あなたの ID: {}\n  - 親エージェント: {}\n  - ワークスペースルート: {}\n",
        b.agent_id,
        b.parent_id.unwrap_or("(なし)"),
        b.workspace_root
    ));
    s.push_str("\n## 禁止事項\n");
    s.push_str(
        "  - git push / PR 作成 / merge / deploy / release は行わない\n\
         \x20 - 権限昇格 (sudo 等) を行わない\n\
         \x20 - ワークスペース外へ書き込まない\n\
         \x20 - 破壊的な削除 (rm -rf 等) を行わない\n",
    );
    s.push_str("\n## 完了報告\n");
    s.push_str(&result_format(t.id, b.agent_id, &b.outbox));
    s.push_str(&teammates_section(&b.teammates));
    s.push_str(&subagents_section(b.agent_id));
    cap(s)
}

/// レビュー担当への指示。**原則としてコードを変更させない。**
pub fn reviewer(b: &Brief<'_>, target: &TeamTask) -> String {
    let mut s = String::new();
    s.push_str("あなたは Zaivern の AI 開発チームのレビュー担当です。\n");
    s.push_str("**コードを変更してはいけません。** 読んで判定するだけです。\n\n");
    s.push_str(&format!("## Goal\n{}\n\n", b.goal.title));
    s.push_str(&format!(
        "## レビュー対象\n#{} {}\n\n{}\n",
        target.id, target.title, target.description
    ));
    s.push_str("\n## 受入基準 (これを満たしているか)\n");
    s.push_str(&bullets(&target.acceptance_criteria));
    // **Zaivern が実測したもの**を渡す (`TeamTask::changed_files`)。
    // 自己申告を渡すと、書き忘れたファイルはレビューの対象にすら
    // ならない — レビュアーは「書いていないもの」を見られない。
    s.push_str("\n## 変更されたファイル (Zaivern が実測)\n");
    s.push_str(&bullets(&target.changed_files));
    s.push_str("\n## 実装担当の報告\n");
    s.push_str(&format!(
        "  {}\n",
        if target.last_summary.is_empty() {
            "(要約なし)"
        } else {
            target.last_summary.as_str()
        }
    ));
    s.push_str("\n## 確認する観点\n");
    s.push_str(
        "  - 仕様への適合 (受入基準を満たしているか)\n\
         \x20 - バグ (境界値・異常系・競合)\n\
         \x20 - テスト不足\n\
         \x20 - セキュリティ (入力検証・秘密情報の漏れ)\n\
         \x20 - 破壊的変更 (既存の振る舞いを壊していないか)\n\
         \x20 - 担当外ファイルの変更\n",
    );
    s.push_str(&format!(
        "\n## 判定の出し方\n次の形式を**そのまま**出力してください。\n\n\
         {open}\n\
         {{\n\
         \x20 \"task_id\": {id},\n\
         \x20 \"verdict\": \"APPROVE\",\n\
         \x20 \"findings\": [],\n\
         \x20 \"summary\": \"判断の 1 行\"\n\
         }}\n\
         {close}\n\n\
         * 指摘があるときは verdict を \"REQUEST_CHANGES\" にし、findings に\n\
         \x20 **具体的な指摘**を 1 件 1 行で書くこと (空では受け付けません)\n\
         * コードは変更しないこと\n",
        open = super::reviewer::REVIEW_OPEN,
        close = super::reviewer::REVIEW_CLOSE,
        id = target.id,
    ));
    // **レビューこそ伝える相手が要る。** 指摘を書いても、直す本人へ
    // 届かなければ盤面に残るだけになる。
    s.push_str(&teammates_section(&b.teammates));
    s.push_str(&subagents_section(b.agent_id));
    cap(s)
}

/// 統合担当への指示。
pub fn integrator(b: &Brief<'_>, all: &[TeamTask]) -> String {
    let mut s = String::new();
    s.push_str("あなたは Zaivern の AI 開発チームの統合担当です。\n\n");
    s.push_str(&format!("## Goal\n{}\n\n", b.goal.title));
    s.push_str("## Definition of Done\n");
    s.push_str(&bullets(&b.goal.definition_of_done));
    s.push_str("\n## 全タスクの状態\n");
    let list: Vec<String> = all
        .iter()
        .map(|t| format!("#{} {} — {}", t.id, t.title, t.state.key()))
        .collect();
    s.push_str(&bullets(&list));
    s.push_str("\n## やること\n");
    s.push_str(
        "  1. 全タスクが完了しているか確認する\n\
         \x20 2. 整形・ビルド・lint・テストを実行する\n\
         \x20 3. 未解決のレビュー指摘が無いか確認する\n\
         \x20 4. 失敗したら、原因のタスク番号を blockers に書いて報告する\n",
    );
    s.push_str("\n## 実行する検証コマンド\n");
    s.push_str(&bullets(&command_labels(&b.task.validation_commands)));
    s.push_str("\n## 禁止事項\n");
    s.push_str(
        "  - git push / PR 作成 / merge / deploy / release は**行わない**\n\
         \x20 - 本番環境・課金・credential に触れない\n",
    );
    s.push_str("\n## 完了報告\n");
    s.push_str(&result_format(b.task.id, b.agent_id, &b.outbox));
    s.push_str(&teammates_section(&b.teammates));
    s.push_str(&subagents_section(b.agent_id));
    cap(s)
}

/// 役割に応じた指示文を作る。
///
/// 役割の分類は [`super::roles`] が持つ 1 本を通す — ここで `match` を
/// 書き直すと、スケジューラ側の「実装担当とレビュアーを分ける」判断と
/// ずれた瞬間に**レビュアーへ実装の指示が飛ぶ**。
pub fn for_task(b: &Brief<'_>, all: &[TeamTask]) -> String {
    if super::roles::is_review_role(b.task.role) {
        let target = b
            .task
            .review_of
            .and_then(|id| all.iter().find(|t| t.id == id))
            .unwrap_or(b.task);
        return reviewer(b, target);
    }
    if b.task.role == TeamRole::Integrator {
        return integrator(b, all);
    }
    implementer(b)
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{goal, task};
    use super::*;

    fn brief<'a>(g: &'a TeamGoal, t: &'a TeamTask) -> Brief<'a> {
        Brief {
            goal: g,
            task: t,
            agent_id: "impl-1",
            parent_id: Some("team-lead"),
            workspace_root: "<ワークスペース>",
            upstream: vec!["#1 の成果: API の骨格".into()],
            forbidden_files: vec!["src/other/**".into()],
            outbox: std::path::PathBuf::from("/tmp/zv-outbox"),
            teammates: vec![("reviewer-1".into(), "Reviewer".into())],
        }
    }

    /// **サブエージェントの知らせ方を、指示文が必ず伝える。**
    ///
    /// `[ZAI-TEAM-EVENT]` を読んで盤面へ子として並べる仕組みは前からあったのに、
    /// **指示文がそれを一言も伝えていなかった**ので誰も報告せず、盤面には
    /// 一度も現れなかった (作ってあるのに繋がっていない)。
    #[test]
    fn どの役割にもサブエージェントの知らせ方が載る() {
        let g = goal();
        let mut t = task(1, "a", &[]);
        t.assigned_agent = Some(super::super::model::AgentId::new("impl-1"));
        let b = brief(&g, &t);
        for (name, text) in [
            ("実装", implementer(&b)),
            ("レビュー", reviewer(&b, &t)),
            ("統合", integrator(&b, std::slice::from_ref(&t))),
        ] {
            assert!(
                text.contains(super::super::result_parser::EVENT_OPEN),
                "{name}担当の指示文にサブエージェントの知らせ方が無い"
            );
            // **表に有る語だけを教える** (捏造した種別は `check_event` が断る)。
            for kind in ["sub_agent_started", "sub_agent_completed"] {
                assert!(
                    text.contains(kind),
                    "{name}担当の指示文に {kind} が無い"
                );
                assert!(
                    super::super::result_parser::EVENT_KINDS.contains(&kind),
                    "{kind} は受け付けない語なのに教えている"
                );
            }
            // 親は必ず自分 (`parent_id` を取り違えると木が繋がらない)。
            assert!(text.contains("impl-1"), "{name}担当の指示文に自分の ID が無い");
        }
    }

    /// **どの役割の指示文にも、仲間の一覧と伝言の作法が載る。**
    ///
    /// 載っていない役割は**一生伝言を使わない** (機能があっても到達経路が
    /// 無いのと同じ)。実際にレビュー担当だけ抜けていた — いちばん伝える
    /// 必要がある役割なのに。
    #[test]
    fn どの役割にも伝言の作法が載る() {
        let g = goal();
        let mut t = task(1, "a", &[]);
        t.assigned_agent = Some(super::super::model::AgentId::new("impl-1"));
        let mut b = brief(&g, &t);
        b.teammates = vec![
            ("agent-2".into(), "Reviewer".into()),
            ("agent-3".into(), "Tester".into()),
        ];
        for (name, text) in [
            ("実装", implementer(&b)),
            ("レビュー", reviewer(&b, &t)),
            ("統合", integrator(&b, std::slice::from_ref(&t))),
        ] {
            assert!(
                text.contains(super::super::result_parser::MSG_OPEN),
                "{name}担当の指示文に伝言の作法が無い"
            );
            assert!(
                text.contains("agent-2") && text.contains("Reviewer"),
                "{name}担当の指示文に仲間の一覧が無い"
            );
        }
        // 仲間が居なければ**出さない** (宛先の無い作法は書かせない)。
        b.teammates.clear();
        assert!(!implementer(&b).contains(super::super::result_parser::MSG_OPEN));
    }

    #[test]
    fn 実装指示に必須項目がすべて入る() {
        let g = goal();
        let mut t = task(12, "auth", &[]);
        t.files = vec!["src/auth.rs".into()];
        t.context = vec!["レビュー指摘 1: 境界値".into()];
        let s = implementer(&brief(&g, &t));
        for needle in [
            "## Goal",
            "## Definition of Done",
            "## あなたの担当タスク",
            "## 受入基準",
            "## 編集してよいファイル",
            "## 編集してはいけない範囲",
            "## 依存タスクの結果",
            "## 実行する検証コマンド",
            "## 体制",
            "## 禁止事項",
            "## 完了報告",
            "src/auth.rs",
            "src/other/**",
            "レビュー指摘 1: 境界値",
            "team-lead",
            "impl-1",
            super::super::result_parser::RESULT_OPEN,
        ] {
            assert!(s.contains(needle), "指示に「{needle}」が無い");
        }
    }

    #[test]
    fn レビュー指示はコード変更を禁じる() {
        let g = goal();
        let mut target = task(1, "impl", &[]);
        target.changed_files = vec!["src/a.rs".into()];
        target.last_summary = "実装した".into();
        let mut rev = task(2, "rev", &[]);
        rev.role = TeamRole::Reviewer;
        rev.review_of = Some(1);
        let s = reviewer(&brief(&g, &rev), &target);
        assert!(s.contains("コードを変更してはいけません"));
        assert!(s.contains("APPROVE"));
        assert!(s.contains("REQUEST_CHANGES"));
        assert!(s.contains(super::super::reviewer::REVIEW_OPEN));
        assert!(s.contains("src/a.rs"));
        // レビュー対象のタスク ID が載る (自分のではなく)
        assert!(s.contains("\"task_id\": 1"), "{s}");
    }

    #[test]
    fn 統合指示はpushを禁じる() {
        let g = goal();
        let mut t = task(3, "int", &[]);
        t.role = TeamRole::Integrator;
        let all = vec![task(1, "a", &[]), t.clone()];
        let s = integrator(&brief(&g, &t), &all);
        assert!(s.contains("git push"));
        assert!(s.contains("行わない"));
        assert!(s.contains("#1 a"));
    }

    #[test]
    fn 役割で指示が切り替わる() {
        let g = goal();
        let mut rev = task(2, "rev", &[]);
        rev.role = TeamRole::Reviewer;
        rev.review_of = Some(1);
        let all = vec![task(1, "impl", &[]), rev.clone()];
        let s = for_task(&brief(&g, &rev), &all);
        assert!(s.contains("レビュー担当"));
        let imp = task(1, "impl", &[]);
        let s2 = for_task(&brief(&g, &imp), &all);
        assert!(s2.contains("## 完了報告"));
        assert!(!s2.contains("レビュー担当"));
    }

    #[test]
    fn 指示は上限で切られる() {
        let g = goal();
        let mut t = task(1, "a", &[]);
        t.description = "あ".repeat(PROMPT_MAX_BYTES);
        let s = implementer(&brief(&g, &t));
        assert!(s.len() <= PROMPT_MAX_BYTES + 64, "{}", s.len());
        assert!(s.contains("切り詰めました"));
    }

    #[test]
    fn 絶対パスを焼き込まない() {
        let g = goal();
        let t = task(1, "a", &[]);
        let s = implementer(&brief(&g, &t));
        assert!(!s.contains("/Users/"), "絶対パスが入っている");
        assert!(!s.contains("C:\\"), "絶対パスが入っている");
    }
}
