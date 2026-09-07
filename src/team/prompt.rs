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

/// 指示文の目安 (バイト)。可変本文を切り詰め、報告契約は長いパスでも保持する。
pub const PROMPT_MAX_BYTES: usize = 8_000;

// 作業用パスはツール実行・内部報告に必要。配布物には持ち込ませない。
// 工程を増やさず、実装担当と統合担当の両方へ同じ納品条件を渡す。
const DELIVERY_QUALITY: &str = "\n納品品質: 依頼の用途・利用者・成果物数を守り、そのまま使う本体を作る。販売用なら対象顧客・解決する具体的な課題・収録内容・使い方・必要環境・制約を購入者向けに明記する。説明やJSONだけで実体を代用しない。スキルは選択されたエージェントに対応する形式で、SKILL.mdのname・descriptionと発動条件・入力・判断基準・具体的手順・出力形式・不足入力時の対応を備え、導入方法と呼出例、編集用テンプレート、完成見本を同梱する。見本は架空例と明示した具体的入力から作り、SVGやHTML/CSSなど依頼に合う形式の実ファイルを保存し、同じ入力と出力を利用例で結び付ける。例の必須値は埋め、テンプレートの差替箇所とは区別する。未提供の実績・権利・販売条件・売上見込みは捏造しない。\n配布物のパス: 本文・コード・設定・リンクには配布物内の相対パスを使い、参照元から実在する同梱先へ繋ぐ。個人名入り絶対パス、作業フォルダ名、一時領域、隔離先、内部報告先、実行IDを転記しない。導入説明は利用者が選ぶ保存先を基準に書く。利用者固有の外部パスが必須なら設定入力として分離する。この制約は配布物用であり、ツールのcwdと内部報告のchanged_filesには指定された実パスを使う。\n";

/// 必須契約の中で、仲間の**表示一覧だけ**に使ってよい上限。
///
/// Agent は最大数が増えても、MSG/EVENT/完了報告の書式を押し出しては
/// いけない。ID の途中で切ると実在しない宛先を教えることになるため、
/// 一覧は完全な行だけをこの予算へ収める。
const TEAMMATES_LIST_MAX_BYTES: usize = 256;

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
/// `required_tail` は報告契約なので切らない。保存先パス等により契約だけで
/// 目安の上限を超える場合も、契約を丸ごと残して進行を止めない。
fn cap(mut body: String, required_tail: String) -> String {
    if body.len() + required_tail.len() <= PROMPT_MAX_BYTES {
        body.push_str(&required_tail);
        return body;
    }

    // 保存先パスは利用者の環境で長くなる。目安の8KBを超えた契約も
    // 切断・panicさせず残す。詳細仕様はexecution-contextから取得できる。
    if required_tail.len() > PROMPT_MAX_BYTES {
        return required_tail;
    }

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

/// workspace と報告置き場の境界。**outbox を使う全役割で同じ文面**
/// を必須契約に入れる。役割ごとに書くと、レビュー担当だけ例外が
/// 欠ける、といった差が必ず生まれる。
fn filesystem_boundary(outbox: &std::path::Path) -> String {
    if outbox.as_os_str().is_empty() {
        return "  - workspace 外パスへの書込みは禁止\n".to_string();
    }
    format!(
        "  - workspace 外への一般的な書込みは禁止\n\
         \x20 - **Zaivern が指定したこの Run 専用 outbox だけが例外**: `{}`\n\
         \x20 - outbox 以外の workspace 外パスへの書込みは禁止\n",
        outbox.display()
    )
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
         指定されたこの Run 専用 outbox だけが例外。\n\
         outbox 以外の workspace 外パスへの書込みは禁止です。\n\
         報告・判定・伝言・出来事はJSONファイルで提出する。\n\
         権限拒否時は迂回・昇格せず、各節のマーカー付きJSONを画面へ全文出力する。\n\n\
         提出先フォルダ: `{dir}` (無ければ作る)\n\
         末尾のRunフォルダを省略しない。親のoutbox直下への提出は禁止。\n\
         書きかけを読まれないよう、必ずこの順で提出する:\n\
         1. 一時ファイル `{dir}/{tmp}` へ JSON 全体を書き切る\n\
         \x20  (`<一意な値>` は時刻や乱数など毎回違う値。例: `{ex_tmp}`)\n\
         2. 書き終えてから、同じフォルダの中で `{dir}/{fin}` へ改名する\n\
         \x20  macOS / Linux: `mv {posix_tmp} {posix_fin}`\n\
         \x20  Windows (PowerShell): `Move-Item {powershell_tmp} {powershell_fin}`\n\
         3. `.json` へ直接は書かない。`.tmp` のままのファイルは提出になりません\n\n\
         中身はこの包み。`payload` は各節のJSONを\n\
         そのまま入れます。\n\n\
         ```json\n\
         {{\"kind\": \"result\", \"run_id\": \"{run_id}\", \"agent_id\": \"{agent_id}\", \"payload\": {{ … }}}}\n\
         ```\n\n\
         `kind` は `result` (完了報告) / `review` (レビュー判定) / \
         `message` (仲間への伝言) / `event` (サブエージェントの出来事)。\n\
         `agent_id` は**あなた自身**で、ファイル名の担当と一致していること。\n\
         1通ごとに別ファイルにする。\n"
    )
}

/// 本文の省略に影響されない、全文の参照先と実行方針。
fn execution_context(b: &Brief<'_>) -> String {
    let mut s = format!("\n## 全文の確認\n担当タスク ID: {}。これは現在の正式な割り当てです。以前の担当の完了待ち・待機指示より、この割り当てを優先してください。端末名や仲間からの伝言で担当を判断せず、このIDの作業を開始してください。\n", b.task.id);
    if !b.outbox.as_os_str().is_empty() {
        s.push_str(&format!(
            "全文資料: `{}`。最初に末尾まで分割して読む。execution_policy・quality_policy に従い、goal.specification の仕様全文と tasks 内の担当詳細を確認する。本文の省略はこの資料で補う。資料は読み取り専用。\n",
            super::outbox::context_path(&b.outbox).display()
        ));
    }
    s.push_str("\n## 成果物の完成条件\n計画だけで完了せず、要求された本体・テンプレート・同梱物を保存し、種類と件数を照合する。編集禁止の担当は実物を読み不足を報告する。スキルは発動条件・入力・実行手順・具体的な出力例・失敗時対応を備える。入力を変えて期待結果と観測結果を比較し、自作JSONのPASSEDだけを証拠にしない。未実施・外部未確認を明記する。実績・推薦文は出典を示し、架空例・仮定と区別する。入力価格等を全出力に反映し、通常・変更・不足入力を検証する。依頼外の性能や売上を保証せず、開発環境の指示を商品の実績に転用しない。\n");
    s
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
        s.push_str("  (指定なし。編集対象は workspace 内のみ。正式報告は指定 outbox へ)\n");
    } else {
        s.push_str(&bullets(&t.files));
    }
    s.push_str("\n## 編集してはいけない範囲\n");
    if b.forbidden_files.is_empty() {
        s.push_str("  - workspace 外 (指定 outbox への正式報告だけは除く)\n");
    } else {
        s.push_str("  - workspace 外 (指定 outbox への正式報告だけは除く)\n");
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
    let mut required_tail = execution_context(b);
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
         \x20 - 破壊的な削除 (rm -rf 等) を行わない\n",
    );
    required_tail.push_str(&filesystem_boundary(&b.outbox));
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
    let mut required_tail = execution_context(b);
    required_tail.push_str(&format!("\nレビュー作業IDは #{}、判定対象IDは #{}。報告はRESULTでなくREVIEWのみ。JSONのtask_idは対象ID {}を使う。\n", b.task.id, target.id, target.id));
    required_tail.push_str("\n## 確認する観点\n");
    required_tail.push_str(
        "  - 仕様への適合 (受入基準を満たしているか)\n\
         \x20 - バグ (境界値・異常系・競合)\n\
         \x20 - テスト不足\n\
         \x20 - セキュリティ (入力検証・秘密情報の漏れ)\n\
         \x20 - 破壊的変更 (既存の振る舞いを壊していないか)\n\
         \x20 - 担当外ファイルの変更\n",
    );
    required_tail.push_str("\n## 禁止事項\n");
    required_tail.push_str(&filesystem_boundary(&b.outbox));
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
    if super::reviewer::requires_content_review(b.goal) {
        required_tail.push_str("\n内容レビュー必須: requirements=要件と出力の全件照合、truthfulness=実績の出典または架空例の明示、deliverables=約束した同梱物の実在と件数、reproducibility=入力変更・不足入力の実行と期待/観測の比較、scope=依頼範囲と未検証事項。担当成果物に各観点を適用し、対象外なら具体的理由を記録。自己申告PASSEDだけでは承認しない。不足はREQUEST_CHANGESのfindingsで担当へ返す。APPROVEでは上のJSONにquality_checks配列を追加し、各観点を一件ずつ報告する。各要素の形式: {\"criterion\":\"requirements\",\"source_path\":\"skills/example/SKILL.md\",\"excerpt\":\"実ファイルからの具体的な原文引用\",\"expected\":\"仕様の要求\",\"actual\":\"検証手順と観測結果\"}。パスは作業フォルダ内の相対パス。引用は12バイト以上で実物と一致させる。再現性はテスト名でなく実入力・本体・観測出力の証跡を引用。\n");
    }
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
        .filter(|t| t.id != b.task.id)
        .map(|t| format!("#{} {} — {}", t.id, t.title, t.state.key()))
        .collect();
    s.push_str(&bullets(&list));
    s.push_str("\n## やること\n");
    s.push_str(
        "  1. 自分以外の前提タスクの成果物を開き、仕様全文の各要件を実物と照合する\n\
         \x20 2. 成果物に適した検証を実行する。文書・スキルには不要なビルドを要求しない\n\
         \x20 3. 未解決のレビュー指摘が無いか確認する\n\
         \x20 4. 不足は担当範囲で修正し、範囲外なら原因のタスク番号と不足を blockers に書いて報告する\n",
    );
    s.push_str("\n## 実行する検証コマンド\n");
    s.push_str(&bullets(&command_labels(&b.task.validation_commands)));
    let mut required_tail = execution_context(b);
    required_tail.push_str("\n統合担当自身は現在実行中なので、自分の完了や進捗100％を待たない。既に完了した担当への伝言だけで終了せず、この担当タスクIDの正式完了報告を必ず提出する。\n");
    required_tail.push_str("\n## 禁止事項\n");
    required_tail.push_str(
        "  - git push / PR 作成 / merge / deploy / release は**行わない**\n\
         \x20 - 本番環境・課金・credential に触れない\n",
    );
    required_tail.push_str(&filesystem_boundary(&b.outbox));
    required_tail.push_str(&outbox_section(b.agent_id, &b.outbox, b.run_id));
    required_tail.push_str("\n## 完了報告\n");
    required_tail.push_str(&result_format(b.task.id, b.agent_id, &b.outbox));
    required_tail.push_str(&teammates_section(&b.teammates, &b.outbox));
    required_tail.push_str(&subagents_section(b.agent_id, &b.outbox));
    cap(s, required_tail)
}

/// レビュー対象の有無で報告形式を決め、通常タスクは役割に応じた指示を作る。
/// 役割名だけで REVIEW を要求すると、対象の無い報告を受信側が拒否して停止する。
pub fn for_task(b: &Brief<'_>, all: &[TeamTask]) -> String {
    if super::planner::implementation_only(&b.goal.specification) {
        let body = format!(
            "依頼:\n{}\n",
            b.goal
                .specification
                .strip_prefix(super::planner::IMPLEMENTATION_ONLY)
                .unwrap_or(&b.goal.specification)
        );
        let assignment = format!(
            "作業フォルダ: {}\n担当 #{}: {}\n編集範囲:\n{}\n編集禁止:\n{}\n",
            b.workspace_root,
            b.task.id,
            format!("{}\n{}", b.task.title, b.task.description),
            bullets(&b.task.files),
            bullets(&b.forbidden_files)
        );
        let isolation = match super::task_workspace::execution(std::path::Path::new(b.workspace_root), &b.task.files) {
            Ok(Some((workspace, prefix, git))) => format!("\n実装用cwd: {}。{}。各ツールはこのディレクトリをcwdとして実行する。changed_filesは元フォルダ基準で {prefix}/ を先頭につける。変更・削除したファイルを要約に列挙し、作業用コピー全体を成果物として申告しない。\n", workspace.display(), if git { "隔離Gitワークツリーで直接実装する" } else { "Gitなしの独立コピーで直接実装する。Gitコマンドは不要" }),
            _ => String::new(),
        };
        let assignment = format!("{assignment}{isolation}{DELIVERY_QUALITY}");
        let report = serde_json::json!({"task_id": b.task.id, "agent_id": b.agent_id, "status": "completed", "summary": "作成した成果物の短い説明", "changed_files": [], "validation": [], "blockers": []});
        let handoff = b.upstream.join("\n");
        let task_scopes = all
            .iter()
            .map(|task| format!("#{} {}: {}", task.id, task.title, task.files.join(", ")))
            .collect::<Vec<_>>()
            .join("\n");
        let assignment = format!(
            "{assignment}\n他担当の編集範囲（読み取り可）:\n{task_scopes}\n引継ぎ:\n{handoff}\n"
        );
        let tail = format!("\n{assignment}\n実装を直ちに開始する。計画書・テスト・レビュー・検証記録を追加しない。テストやブラウザ確認を開始条件にしない。通常の仕様の不足は合理的に補って進め、相談や途中報告だけで停止しない。HTMLの依頼ならHTML本体を保存する。JSONの完了報告は成果物の代わりにならない。指定フォルダと担当範囲で実装し、成果物を保存した後にだけcompletedを報告する。保存できなかった場合はfailedと理由を報告する。未実施のテストを成功と書かない。\n全文は {} の goal.specification、担当詳細は tasks を読む（本文が省略された場合も要件を落とさない）。\n{}\n{}\n最後に {}\n{}\n{} を出力する。changed_filesには実際に保存した相対パスを列挙する。validationは空配列。outboxへ書ける場合はkind=resultで同じ報告を保存する。書込みが拒否された場合は再試行で停止せず、上の端末出力で提出する。\n", super::outbox::context_path(&b.outbox).display(), filesystem_boundary(&b.outbox), outbox_section(b.agent_id, &b.outbox, b.run_id), super::result_parser::RESULT_OPEN, report, super::result_parser::RESULT_CLOSE);
        return cap(body, tail);
    }
    if super::roles::is_review_task(b.task) {
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

    #[test]
    fn 長い保存先を含む必須契約で停止せず完了報告の形式を残す() {
        let required = format!(
            "{}\n正式完了報告",
            "保存先の長いパス/".repeat(PROMPT_MAX_BYTES / 10)
        );
        assert!(required.len() > PROMPT_MAX_BYTES);
        assert_eq!(cap("省略可能な説明".into(), required.clone()), required);
    }

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

    #[test]
    fn 統合は自分の完了を待たず実物を検証して報告する() {
        let g = goal();
        let mut t = task(3, "integrate", &[1]);
        t.role = TeamRole::Integrator;
        let text = for_task(&brief(&g, &t), &[task(1, "artifact", &[]), t.clone()]);
        assert!(!text.contains("#3 integrate —"));
        assert!(text.contains("自分の完了や進捗100％を待たない"));
        assert!(text.contains("自作JSONのPASSED"));
        assert!(text.contains("具体的な出力例"));
        assert!(text.contains("正式完了報告を必ず提出"));
    }

    #[test]
    fn 検証資料の作成と対象付きレビューの報告形式を分ける() {
        let g = goal();
        let target = task(1, "implementation", &[]);
        let mut assigned = task(2, "validation-checklist", &[1]);
        assigned.role = TeamRole::Tester;
        assigned.files = vec!["references/validation_checklist.md".to_string()];
        let text = for_task(&brief(&g, &assigned), &[target.clone(), assigned.clone()]);
        assert!(text.contains("references/validation_checklist.md"));
        assert!(text.contains(super::super::result_parser::RESULT_OPEN));
        assert!(!text.contains(super::super::reviewer::REVIEW_OPEN));
        assert!(!text.contains("このタスクではコードを変更しません"));
        assert!(!text.contains("コードを変更してはいけません"));

        assigned.review_of = Some(target.id);
        let text = for_task(&brief(&g, &assigned), &[target.clone(), assigned.clone()]);
        assert_eq!(text, reviewer(&brief(&g, &assigned), &target));
        assert!(text.contains(super::super::reviewer::REVIEW_OPEN));
        assert!(!text.contains(super::super::result_parser::RESULT_OPEN));

        assigned.role = TeamRole::Reviewer;
        assigned.review_of = None;
        let text = for_task(&brief(&g, &assigned), &[target, assigned.clone()]);
        assert!(text.contains("このタスクではコードを変更しません"));
        assert!(text.contains(super::super::result_parser::RESULT_OPEN));
        assert!(!text.contains(super::super::reviewer::REVIEW_OPEN));
    }

    #[test]
    fn 長文でも全文資料と継続方針と報告経路が残る() {
        let mut g = goal();
        g.title = "長い要件".repeat(8000);
        let t = task(1, "制作", &[]);
        let b = brief(&g, &t);
        for p in [
            implementer(&b),
            reviewer(&b, &t),
            integrator(&b, std::slice::from_ref(&t)),
        ] {
            assert!(p.len() <= PROMPT_MAX_BYTES);
            assert!(p.contains("/tmp/zv-outbox/execution-context.md"));
            assert!(p.contains("担当タスク ID: 1"));
            assert!(p.contains("execution_policy"));
            assert!(p.contains("権限拒否時は迂回・昇格せず"));
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
        // 完了報告を出す実装・統合の双方で同じ原子的提出契約を使う。
        for (name, text) in [
            ("実装", implementer(&b)),
            ("統合", integrator(&b, std::slice::from_ref(&t))),
        ] {
            // 一時ファイル → 改名、の順で両方の名前が出る (正式な名前は一時
            // ファイルの名前の接頭辞なので、閉じる ` まで含めて探す)
            let at_tmp = text.find(&format!("{dir}/{tmp}`"));
            let at_fin = text.find(&format!("{dir}/{fin}`"));
            assert!(
                at_tmp.is_some(),
                "{name}担当の指示文に一時ファイルの名前が無い"
            );
            assert!(at_fin.is_some(), "{name}担当の指示文に正式な名前が無い");
            assert!(
                at_tmp < at_fin,
                "{name}担当: 改名先が一時ファイルより先に出ている"
            );
            // 改名の手段が OS ごとに 1 つずつ
            assert!(text.contains("mv '"), "{name}担当: unix の改名手順が無い");
            assert!(
                text.contains("Move-Item"),
                "{name}担当: Windows の改名手順が無い"
            );
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

        let tmp = super::super::outbox::tmp_name("impl-1", "1712345678");
        let fin = super::super::outbox::final_name("impl-1", "1712345678");
        let tmp_path = b.outbox.join(tmp).display().to_string();
        let fin_path = b.outbox.join(fin).display().to_string();
        for role in TeamRole::ALL {
            let mut assigned = t.clone();
            assigned.role = role;
            assigned.review_of = Some(t.id);
            let all = vec![t.clone(), assigned.clone()];
            let mut role_brief = b.clone();
            role_brief.task = &assigned;
            let text = for_task(&role_brief, &all);
            assert!(
                text.contains(&format!(
                    "mv {} {}",
                    posix_single_quote(&tmp_path),
                    posix_single_quote(&fin_path)
                )),
                "{}: POSIX shell の単一引用符として安全でない",
                role.key()
            );
            assert!(
                text.contains(&format!(
                    "Move-Item {} {}",
                    powershell_single_quote(&tmp_path),
                    powershell_single_quote(&fin_path)
                )),
                "{}: PowerShell の単一引用符として安全でない",
                role.key()
            );
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
        g.specification = "SKILL.mdを作成".into();
        g.definition_of_done = vec!["長い完了条件界".repeat(PROMPT_MAX_BYTES)];

        for role in TeamRole::ALL {
            let mut target = task(1, "target", &[]);
            target.title = "長いレビュー対象界".repeat(PROMPT_MAX_BYTES);
            target.description = "長い対象説明界".repeat(PROMPT_MAX_BYTES);
            target.acceptance_criteria = vec!["長い受入基準界".repeat(PROMPT_MAX_BYTES)];
            target.last_summary = "長い実装報告界".repeat(PROMPT_MAX_BYTES);

            let mut assigned = task(2, "assigned", &[]);
            assigned.role = role;
            assigned.review_of = (role == TeamRole::Reviewer).then_some(target.id);
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
            let completion = if super::super::roles::is_review_task(&assigned) {
                assert!(text.contains(super::super::reviewer::REVIEW_CLOSE));
                for criterion in super::super::reviewer::QUALITY_CRITERIA {
                    assert!(text.contains(criterion));
                }
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

    /// outbox は workspace 外にあるが、Zaivern がこの Run に限って
    /// 指定した報告先である。「workspace 外へ書くな」と「そこへ
    /// 報告を書け」を同時に渡すと、安全側の Agent は報告を出さず
    /// Run が永久に止まる。生成後の最終プロンプト全体を、全役割で
    /// 検査する。
    #[test]
    fn 全役割の最終プロンプトはoutboxだけを外部書込みの例外にする() {
        let mut g = goal();
        g.title = "長いゴール".repeat(PROMPT_MAX_BYTES);
        g.definition_of_done = vec!["長い完了条件".repeat(PROMPT_MAX_BYTES)];

        for role in TeamRole::ALL {
            let mut target = task(1, "target", &[]);
            target.description = "長いレビュー対象".repeat(PROMPT_MAX_BYTES);
            let mut assigned = task(2, "assigned", &[]);
            assigned.role = role;
            assigned.review_of = (role == TeamRole::Reviewer).then_some(target.id);
            assigned.description = "長い担当説明".repeat(PROMPT_MAX_BYTES);
            let all = vec![target, assigned.clone()];
            let b = brief(&g, &assigned);
            let text = for_task(&b, &all);
            let name = role.key();

            assert!(text.len() <= PROMPT_MAX_BYTES, "{name}: 8KB 上限超過");
            assert!(
                !text.contains("ワークスペース外へ書き込まない"),
                "{name}: outbox と矛盾する無条件禁止が残っている"
            );
            assert!(
                text.contains("指定されたこの Run 専用 outbox だけが例外"),
                "{name}: outbox だけが例外と明記されていない"
            );
            assert!(
                text.contains("outbox 以外の workspace 外パスへの書込みは禁止"),
                "{name}: 例外が外部全体へ広がっている"
            );
            for contract in [
                ".json.tmp",
                "同じフォルダの中で",
                "\"run_id\": \"run-1712345678-1-0\"",
                "\"agent_id\": \"impl-1\"",
                "`result` (完了報告)",
                "`review` (レビュー判定)",
                "`message` (仲間への伝言)",
                "`event` (サブエージェントの出来事)",
            ] {
                assert!(
                    text.contains(contract),
                    "{name}: 必須契約 {contract:?} が欠落"
                );
            }
        }
    }

    /// 137 体を一度に見せても、可変の仲間一覧が必須契約を 8KB の外へ
    /// 押し出さない。表示する ID は完全な行だけにし、MSG 自体は残す。
    #[test]
    fn 百三十七体の仲間がいても必須契約は上限内に残る() {
        let mut g = goal();
        g.specification = "SKILL.mdを作成".into();
        let mut assigned = task(2, "assigned", &[]);
        let target = task(1, "target", &[]);
        let teammates: Vec<(String, String)> = (0..137)
            .map(|i| (format!("agent-{i}"), "Implementer".to_string()))
            .collect();

        for role in TeamRole::ALL {
            assigned.role = role;
            assigned.review_of = (role == TeamRole::Reviewer).then_some(target.id);
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
            if super::super::roles::is_review_task(&assigned) {
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
