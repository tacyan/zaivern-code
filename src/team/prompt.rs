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

/// 必須契約の中で、仲間の**表示一覧だけ**に使ってよい上限。
///
/// Agent は最大数が増えても、MSG/EVENT/完了報告の書式を押し出しては
/// いけない。ID の途中で切ると実在しない宛先を教えることになるため、
/// 一覧は完全な行だけをこの予算へ収める。
const TEAMMATES_LIST_MAX_BYTES: usize = 1_536;

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
    /// この Run の ID。提出の包みに書かせて、**別 Run 宛ての取り違えを断る**
    /// ための材料 (`outbox::judge` が照合する)。
    pub run_id: &'a str,
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

/// 可変の本文だけを上限で切り、必須契約は必ず末尾へ残す。
///
/// 完成済みの文字列を先頭から単純に切ると、後ろに置いた完了報告・伝言・
/// サブエージェント報告の形式から順に消える。すると長いタスクほど正式な
/// 報告手段を失い、終わっていても Runtime は完了を受け取れない。
///
/// `required_tail` は固定の契約なので切らない。契約だけで上限を超えるのは
/// プロンプト設計そのものの不整合であり、不完全な指示を黙って渡すより
/// その場で検出する。
fn cap(mut body: String, required_tail: String) -> String {
    if body.len() + required_tail.len() <= PROMPT_MAX_BYTES {
        body.push_str(&required_tail);
        return body;
    }

    assert!(
        required_tail.len() <= PROMPT_MAX_BYTES,
        "必須の報告契約だけでプロンプト上限を超えています"
    );

    const NOTICE: &str = "\n…(可変の指示本文が長いため切り詰めました)\n\n";
    let notice = if required_tail.len() + NOTICE.len() <= PROMPT_MAX_BYTES {
        NOTICE
    } else {
        ""
    };
    let mut cut = PROMPT_MAX_BYTES - required_tail.len() - notice.len();
    cut = cut.min(body.len());
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    body.truncate(cut);
    body.push_str(notice);
    body.push_str(&required_tail);
    body
}

/// **置き場への提出の作法。4 種類 (完了報告・レビュー・伝言・出来事) 共通。**
///
/// 画面へ出すだけでは届かない。Claude Code v2 のような TUI は改行ではなく
/// カーソル移動で描くので、画面のグリッドでは行が潰れて**構造的に**
/// 取りこぼす。完了報告だけをファイルにしても、レビューを落とせばタスクは
/// `Reviewing` のまま止まる — だから 4 種類とも同じ道で出させる。
///
/// 名前と形の取り決めは [`super::outbox`] が持つ 1 か所から引く。ここで
/// 別に綴ると、教えた名前を読む側が受け付けない食い違いが黙って起きる。
fn posix_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn powershell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn outbox_section(agent_id: &str, outbox: &std::path::Path, run_id: &str) -> String {
    if outbox.as_os_str().is_empty() {
        return String::new();
    }
    let dir = outbox.display();
    let tmp = super::outbox::tmp_name(agent_id, "<一意な値>");
    let fin = super::outbox::final_name(agent_id, "<一意な値>");
    let ex_tmp = super::outbox::tmp_name(agent_id, "1712345678");
    let ex_fin = super::outbox::final_name(agent_id, "1712345678");
    let ex_tmp_path = outbox.join(&ex_tmp).display().to_string();
    let ex_fin_path = outbox.join(&ex_fin).display().to_string();
    let posix_tmp = posix_single_quote(&ex_tmp_path);
    let posix_fin = posix_single_quote(&ex_fin_path);
    let powershell_tmp = powershell_single_quote(&ex_tmp_path);
    let powershell_fin = powershell_single_quote(&ex_fin_path);
    format!(
        "\n## 提出のしかた (**これが正式な経路**)\n\
         報告・判定・伝言・出来事は、下の JSON を**ファイルとして提出**してください。\n\
         画面にも出してよいですが、画面は人が読むための控えです。\n\n\
         提出先フォルダ: `{dir}` (無ければ作る)\n\
         書きかけを読まれないための手順です。**必ずこの順で**:\n\
         1. 一時ファイル `{dir}/{tmp}` へ JSON 全体を書き切る\n\
         \x20  (`<一意な値>` は時刻や乱数など毎回違う値。例: `{ex_tmp}`)\n\
         2. 書き終えてから、**同じフォルダの中で** `.tmp` を外した名前 `{dir}/{fin}` へ改名する\n\
         \x20  macOS / Linux: `mv {posix_tmp} {posix_fin}`\n\
         \x20  Windows (PowerShell): `Move-Item {powershell_tmp} {powershell_fin}`\n\
         3. `.json` へ直接は書かない。`.tmp` のままのファイルは提出になりません\n\n\
         中身は**この包み**にしてください。`payload` には下の各節が示す JSON を\n\
         そのまま入れます。\n\n\
         ```json\n\
         {{\"kind\": \"result\", \"run_id\": \"{run_id}\", \"agent_id\": \"{agent_id}\", \"payload\": {{ … }}}}\n\
         ```\n\n\
         `kind` は `result` (完了報告) / `review` (レビュー判定) / \
         `message` (仲間への伝言) / `event` (サブエージェントの出来事)。\n\
         `agent_id` は**あなた自身**で、ファイル名の担当と一致していること。\n\
         1 通ごとに別のファイルにしてください。\n"
    )
}

/// レビュー判定を置き場へ出させる 1 行 (置き場が無ければ空)。
///
/// **レビューを画面依存のままにしない。** 落とすと、実装が終わったタスクが
/// `Reviewing` のまま永久に止まる (完了報告だけを置き場へ移しても、
/// ここが残っていれば同じ形で詰まる)。
fn review_submit(outbox: &std::path::Path) -> String {
    if outbox.as_os_str().is_empty() {
        return String::new();
    }
    "**上の「提出のしかた」で `kind` を `review` にして提出してください** \
     (これが正式な提出です)。下の形は `payload` の中身です。\n\n"
        .to_string()
}

/// 完了報告のひな型。**全役割で同じ 1 本**を使う。
fn result_format(task_id: u64, agent_id: &str, outbox: &std::path::Path) -> String {
    // **ファイルへ書かせるのが本線。** 画面へ出すだけだと、カーソル移動で
    // 描く CLI (Claude Code v2) では行が潰れて届かない。画面にも出させるのは
    // 人が読むためで、こちらは控え。
    let file = if outbox.as_os_str().is_empty() {
        String::new()
    } else {
        "**上の「提出のしかた」で `kind` を `result` にして提出してください** \
         (これが正式な提出です)。下の形は `payload` の中身であり、\
         画面へ出すときの形でもあります。\n\n"
            .to_string()
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
fn subagents_section(agent_id: &str, outbox: &std::path::Path) -> String {
    let submit = if outbox.as_os_str().is_empty() {
        String::new()
    } else {
        "**上の「提出のしかた」で `kind` を `event` にして提出してください。**\n\
         (画面へ出すだけだと、描き方によっては届きません。)\n\n"
            .to_string()
    };
    format!(
        "\n## 中で誰かに手伝わせたとき\n\
         あなたが内部でサブエージェントを使ったら、**始めたときと終えたとき**に\n\
         次を出してください (Zaivern の盤面へ、あなたの下にぶら下がって出ます)。\n\n\
         {submit}\
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
fn teammates_section(mates: &[(String, String)], outbox: &std::path::Path) -> String {
    if mates.is_empty() {
        return String::new();
    }
    let submit = if outbox.as_os_str().is_empty() {
        String::new()
    } else {
        "**上の「提出のしかた」で `kind` を `message` にして提出してください。**\n\
         (画面へ出すだけだと、描き方によっては届きません。)\n\n"
            .to_string()
    };
    const LINE_DECORATION_BYTES: usize = "* `` — \n".len();
    const OMITTED: &str = "* …(仲間の一覧が長いため一部省略。全員への宛先 `all` は使えます)\n";
    let full_len = mates.iter().fold(0usize, |total, (id, role)| {
        total
            .saturating_add(LINE_DECORATION_BYTES)
            .saturating_add(id.len())
            .saturating_add(role.len())
    });
    let omitted = full_len > TEAMMATES_LIST_MAX_BYTES;
    let budget = if omitted {
        TEAMMATES_LIST_MAX_BYTES.saturating_sub(OMITTED.len())
    } else {
        TEAMMATES_LIST_MAX_BYTES
    };
    let mut list = String::with_capacity(full_len.min(TEAMMATES_LIST_MAX_BYTES));
    for (id, role) in mates {
        let line_len = LINE_DECORATION_BYTES + id.len() + role.len();
        // 宛先 ID を途中で切らない。収まらない 1 行は丸ごと省く。
        if list.len() + line_len <= budget {
            list.push_str("* `");
            list.push_str(id);
            list.push_str("` — ");
            list.push_str(role);
            list.push('\n');
        }
    }
    if omitted {
        list.push_str(OMITTED);
    }
    format!(
        "\n## チームの仲間\n{list}\n\
         区切りが付いたときや、相手が待っていることが分かったときは、\
         次の形で**その相手へ直接伝えてください** (Zaivern が相手の端末へ届けます)。\n\n\
         {submit}{howto}",
        howto = super::result_parser::message_howto("<上の ID か役割、全員なら all>"),
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
    let mut required_tail = String::new();
    required_tail.push_str(&format!(
        "\n## 体制\n  - あなたの ID: {}\n  - 親エージェント: {}\n  - ワークスペースルート: {}\n",
        b.agent_id,
        b.parent_id.unwrap_or("(なし)"),
        b.workspace_root
    ));
    required_tail.push_str("\n## 禁止事項\n");
    required_tail.push_str(
        "  - git push / PR 作成 / merge / deploy / release は行わない\n\
         \x20 - 権限昇格 (sudo 等) を行わない\n\
         \x20 - ワークスペース外へ書き込まない\n\
         \x20 - 破壊的な削除 (rm -rf 等) を行わない\n",
    );
    required_tail.push_str(&outbox_section(b.agent_id, &b.outbox, b.run_id));
    required_tail.push_str("\n## 完了報告\n");
    required_tail.push_str(&result_format(t.id, b.agent_id, &b.outbox));
    required_tail.push_str(&teammates_section(&b.teammates, &b.outbox));
    required_tail.push_str(&subagents_section(b.agent_id, &b.outbox));
    cap(s, required_tail)
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
    let mut required_tail = String::new();
    required_tail.push_str("\n## 確認する観点\n");
    required_tail.push_str(
        "  - 仕様への適合 (受入基準を満たしているか)\n\
         \x20 - バグ (境界値・異常系・競合)\n\
         \x20 - テスト不足\n\
         \x20 - セキュリティ (入力検証・秘密情報の漏れ)\n\
         \x20 - 破壊的変更 (既存の振る舞いを壊していないか)\n\
         \x20 - 担当外ファイルの変更\n",
    );
    required_tail.push_str(&outbox_section(b.agent_id, &b.outbox, b.run_id));
    required_tail.push_str(&format!(
        "\n## 判定の出し方\n{submit}次の形式を**そのまま**出力してください。\n\n\
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
        submit = review_submit(&b.outbox),
        open = super::reviewer::REVIEW_OPEN,
        close = super::reviewer::REVIEW_CLOSE,
        id = target.id,
    ));
    // **レビューこそ伝える相手が要る。** 指摘を書いても、直す本人へ
    // 届かなければ盤面に残るだけになる。
    required_tail.push_str(&teammates_section(&b.teammates, &b.outbox));
    required_tail.push_str(&subagents_section(b.agent_id, &b.outbox));
    cap(s, required_tail)
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
    let mut required_tail = String::new();
    required_tail.push_str("\n## 禁止事項\n");
    required_tail.push_str(
        "  - git push / PR 作成 / merge / deploy / release は**行わない**\n\
         \x20 - 本番環境・課金・credential に触れない\n",
    );
    required_tail.push_str(&outbox_section(b.agent_id, &b.outbox, b.run_id));
    required_tail.push_str("\n## 完了報告\n");
    required_tail.push_str(&result_format(b.task.id, b.agent_id, &b.outbox));
    required_tail.push_str(&teammates_section(&b.teammates, &b.outbox));
    required_tail.push_str(&subagents_section(b.agent_id, &b.outbox));
    cap(s, required_tail)
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
            run_id: "run-1712345678-1-0",
            teammates: vec![("reviewer-1".into(), "Reviewer".into())],
        }
    }

    /// **提出は「一時ファイルへ書いてから改名」。完了報告を出す役割の指示文に載る。**
    ///
    /// 読む側 (`panel::drain_outbox`) は `.json` だけを見る。指示文が
    /// 「`.json` へ直接書け」と教えると、書いている途中を読まれて報告が
    /// 半分になる (以前はそう教えていた)。名前は `outbox` の 1 か所から
    /// 引くので、ここで教えた名前は必ず読む側の照合を通る。
    #[test]
    fn 提出は一時ファイルへ書いてから改名する手順で教える() {
        let g = goal();
        let mut t = task(1, "a", &[]);
        t.assigned_agent = Some(super::super::model::AgentId::new("impl-1"));
        let b = brief(&g, &t);
        let dir = b.outbox.display().to_string();
        let tmp = super::super::outbox::tmp_name("impl-1", "<一意な値>");
        let fin = super::super::outbox::final_name("impl-1", "<一意な値>");
        // 完了報告 (`[ZAI-TEAM-RESULT]`) を出すのは実装と統合。レビューの
        // 判定は `[ZAI-TEAM-REVIEW]` で、置き場は使わない。
        for (name, text) in [
            ("実装", implementer(&b)),
            ("統合", integrator(&b, std::slice::from_ref(&t))),
        ] {
            // 一時ファイル → 改名、の順で両方の名前が出る (正式な名前は一時
            // ファイルの名前の接頭辞なので、閉じる ` まで含めて探す)
            let at_tmp = text.find(&format!("{dir}/{tmp}`"));
            let at_fin = text.find(&format!("{dir}/{fin}`"));
            assert!(at_tmp.is_some(), "{name}担当の指示文に一時ファイルの名前が無い");
            assert!(at_fin.is_some(), "{name}担当の指示文に正式な名前が無い");
            assert!(at_tmp < at_fin, "{name}担当: 改名先が一時ファイルより先に出ている");
            // 改名の手段が OS ごとに 1 つずつ
            assert!(text.contains("mv '"), "{name}担当: unix の改名手順が無い");
            assert!(text.contains("Move-Item"), "{name}担当: Windows の改名手順が無い");
            // **`.json` へ直接書けとは教えない** (旧: `<dir>/impl-1.json`)
            assert!(
                !text.contains(&format!("{dir}/impl-1.json")),
                "{name}担当: `.json` へ直接書く旧手順が残っている"
            );
            // 教えた例の名前は、読む側の照合を通る (取り決めが 1 か所)
            let example = super::super::outbox::final_name("impl-1", "1712345678");
            assert!(text.contains(&example), "{name}担当: 例が無い");
            let stem = std::path::Path::new(&example)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap();
            let ids = [super::super::model::AgentId::new("impl-1")];
            assert_eq!(
                super::super::outbox::candidates(stem, &ids).len(),
                1,
                "{name}担当: 教えた名前を読む側が受け付けない"
            );
        }
    }

    /// outbox の絶対パスは利用者が決める。空白や Unicode は引用だけで扱えるが、
    /// シングルクォートをそのまま単一引用符の内側へ差し込むと、POSIX shell と
    /// PowerShell のどちらでもそこで引用が終わり、例示した改名コマンドが壊れる。
    #[test]
    fn 提出先パスをposixとpowershellの規則で安全に引用する() {
        assert_eq!(posix_single_quote("a'b"), "'a'\\''b'");
        assert_eq!(powershell_single_quote("a'b"), "'a''b'");
        let g = goal();
        let mut t = task(1, "a", &[]);
        t.assigned_agent = Some(super::super::model::AgentId::new("impl-1"));
        let mut b = brief(&g, &t);
        b.outbox = std::path::PathBuf::from("/tmp/Team's 日本語 outbox");

        let text = implementer(&b);
        let tmp = super::super::outbox::tmp_name("impl-1", "1712345678");
        let fin = super::super::outbox::final_name("impl-1", "1712345678");
        let tmp_path = b.outbox.join(tmp).display().to_string();
        let fin_path = b.outbox.join(fin).display().to_string();
        assert!(
            text.contains(&format!(
                "mv {} {}",
                posix_single_quote(&tmp_path),
                posix_single_quote(&fin_path)
            )),
            "POSIX shell の単一引用符として安全にエスケープされていない"
        );
        assert!(
            text.contains(&format!(
                "Move-Item {} {}",
                powershell_single_quote(&tmp_path),
                powershell_single_quote(&fin_path)
            )),
            "PowerShell の単一引用符として安全にエスケープされていない"
        );
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
                assert!(text.contains(kind), "{name}担当の指示文に {kind} が無い");
                assert!(
                    super::super::result_parser::EVENT_KINDS.contains(&kind),
                    "{kind} は受け付けない語なのに教えている"
                );
            }
            // 親は必ず自分 (`parent_id` を取り違えると木が繋がらない)。
            assert!(
                text.contains("impl-1"),
                "{name}担当の指示文に自分の ID が無い"
            );
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
        assert!(s.len() <= PROMPT_MAX_BYTES, "{}", s.len());
        assert!(s.contains("切り詰めました"));
    }

    /// Goal や description が長くても、先頭から完成品を切って契約を
    /// 消してはいけない。全 role は `for_task` の三つの経路へ分類されるので、
    /// `TeamRole::ALL` を通して完了・伝言・子エージェント報告を固定する。
    #[test]
    fn 長い可変本文でも全役割の必須契約は末尾に残る() {
        let mut g = goal();
        g.title = "長いゴール界".repeat(PROMPT_MAX_BYTES);
        g.definition_of_done = vec!["長い完了条件界".repeat(PROMPT_MAX_BYTES)];

        for role in TeamRole::ALL {
            let mut target = task(1, "target", &[]);
            target.title = "長いレビュー対象界".repeat(PROMPT_MAX_BYTES);
            target.description = "長い対象説明界".repeat(PROMPT_MAX_BYTES);
            target.acceptance_criteria = vec!["長い受入基準界".repeat(PROMPT_MAX_BYTES)];
            target.last_summary = "長い実装報告界".repeat(PROMPT_MAX_BYTES);

            let mut assigned = task(2, "assigned", &[]);
            assigned.role = role;
            assigned.review_of = Some(target.id);
            assigned.title = "長い担当名界".repeat(PROMPT_MAX_BYTES);
            assigned.description = "長い担当説明界".repeat(PROMPT_MAX_BYTES);
            assigned.context = vec!["長い引き継ぎ界".repeat(PROMPT_MAX_BYTES)];

            let all = vec![target, assigned.clone()];
            let text = for_task(&brief(&g, &assigned), &all);
            let name = role.key();

            assert!(
                text.len() <= PROMPT_MAX_BYTES,
                "{name} の指示が上限を超えた: {} bytes",
                text.len()
            );
            let notice = text
                .find("切り詰めました")
                .unwrap_or_else(|| panic!("{name} の長い可変本文が切り詰められていない"));
            let completion = if super::super::roles::is_review_role(role) {
                assert!(text.contains(super::super::reviewer::REVIEW_CLOSE));
                text.find(super::super::reviewer::REVIEW_OPEN)
            } else {
                assert!(text.contains(super::super::result_parser::RESULT_CLOSE));
                text.find(super::super::result_parser::RESULT_OPEN)
            }
            .unwrap_or_else(|| panic!("{name} の完了報告契約が消えた"));
            let message = text
                .find(super::super::result_parser::MSG_OPEN)
                .unwrap_or_else(|| panic!("{name} の伝言契約が消えた"));
            let event = text
                .find(super::super::result_parser::EVENT_OPEN)
                .unwrap_or_else(|| panic!("{name} のサブエージェント報告契約が消えた"));

            assert!(text.contains(super::super::result_parser::MSG_CLOSE));
            assert!(text.contains(super::super::result_parser::EVENT_CLOSE));
            assert!(text.contains("sub_agent_started"));
            assert!(text.contains("sub_agent_completed"));
            assert!(
                notice < completion && completion < message && message < event,
                "{name} の必須契約が切詰め通知より後ろへ順番どおり残っていない"
            );
        }
    }

    /// 137 体を一度に見せても、可変の仲間一覧が必須契約を 8KB の外へ
    /// 押し出さない。表示する ID は完全な行だけにし、MSG 自体は残す。
    #[test]
    fn 百三十七体の仲間がいても必須契約は上限内に残る() {
        let g = goal();
        let mut assigned = task(2, "assigned", &[]);
        let target = task(1, "target", &[]);
        let teammates: Vec<(String, String)> = (0..137)
            .map(|i| (format!("agent-{i}"), "Implementer".to_string()))
            .collect();

        for role in TeamRole::ALL {
            assigned.role = role;
            assigned.review_of = Some(target.id);
            let all = vec![target.clone(), assigned.clone()];
            let mut b = brief(&g, &assigned);
            b.teammates = teammates.clone();
            let text = for_task(&b, &all);
            let name = role.key();

            assert!(
                text.len() <= PROMPT_MAX_BYTES,
                "{name} の137体プロンプトが上限を超えた: {} bytes",
                text.len()
            );
            assert!(text.contains("仲間の一覧が長いため一部省略"));
            assert!(text.contains(super::super::result_parser::MSG_OPEN));
            assert!(text.contains(super::super::result_parser::MSG_CLOSE));
            assert!(text.contains(super::super::result_parser::EVENT_OPEN));
            assert!(text.contains(super::super::result_parser::EVENT_CLOSE));
            if super::super::roles::is_review_role(role) {
                assert!(text.contains(super::super::reviewer::REVIEW_OPEN));
                assert!(text.contains(super::super::reviewer::REVIEW_CLOSE));
            } else {
                assert!(text.contains(super::super::result_parser::RESULT_OPEN));
                assert!(text.contains(super::super::result_parser::RESULT_CLOSE));
            }
        }
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
