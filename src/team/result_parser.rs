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
/// **エージェント同士のやり取り**の開始・終了マーカー。
pub const MSG_OPEN: &str = "[ZAI-TEAM-MSG]";
pub const MSG_CLOSE: &str = "[/ZAI-TEAM-MSG]";

/// 1 通のメッセージに書ける本文の上限 (文字)。
///
/// **上限が無いと、相手の端末へ丸ごと流し込める。** 長文はそのまま
/// 相手の作業を押し流すので、伝言は伝言の長さに留める。
pub const MSG_MAX_CHARS: usize = 800;

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
    /// 担当外のファイルを変更している (**実測**)。
    OutOfScopeFiles(Vec<String>),
    /// 自己申告で担当外のファイルを挙げている。
    ///
    /// 実測とは分けて出す — 直し方が違う (申告を直すのか、変更を戻すのか)。
    OutOfScopeReported(Vec<String>),
    /// **実測できなかった。** 担当範囲を守ったと言える根拠が無い。
    EvidenceUnavailable(String),
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
                format!(
                    "担当外のファイルが実際に変更されています: {}",
                    f.join(", ")
                )
            }
            RejectReason::OutOfScopeReported(f) => {
                format!("担当外のファイルを変更したと報告しています: {}", f.join(", "))
            }
            RejectReason::EvidenceUnavailable(w) => {
                format!(
                    "変更されたファイルを実測できないので完了にできません: {w}"
                )
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

/// **Zaivern 自身が測った**変更ファイルの証跡。
///
/// これを [`accept`] へ渡すのが要点。渡さずにエージェントの
/// `changed_files` だけで照合すると、**申告し忘れ・意図的な省略と
/// 「本当に触っていない」が区別できない**ので、担当外を書き換えた
/// タスクが素通りする (しかも台帳には「担当内だけ」と残る)。
///
/// 測るのは [`super::changeset`]。この型は測り方を知らない — 判定は
/// 純関数のままにして、テストから両方の筋書きを作れるようにする。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileEvidence {
    /// 測れた。**このタスクに帰属する**変更のパス (正規化済み)。
    Measured {
        /// 自分の担当範囲の中で実際に変わったもの。
        mine: Vec<String>,
        /// 自分のものと言わざるを得ないのに担当範囲の外にあるもの。
        out_of_scope: Vec<String>,
    },
    /// 測れなかった (理由つき)。**完了は通さない。**
    ///
    /// 「測れるはずなのに失敗した」場合。git はあるのに壊れている、
    /// 変更が多すぎる、など — 直せば測れるので、人へ渡す。
    Unavailable(String),
    /// **そもそも測る手立てが無い** (理由つき)。**完了は通す。**
    ///
    /// Git 管理下でないフォルダがこれ。直しようが無いので、ここで
    /// 止めると**そのフォルダでは 1 件も完了できない**
    /// (実機で 7 体が並列で働いているのに 1 件も終わらなかった)。
    ///
    /// **通すが、隠さない。** 「担当内だけを変更した」とは言えない状態
    /// なので、盤面が「実測なし」を出す (検証なしで進む Run と同じ扱い)。
    Unmeasurable(String),
    /// このタスクに担当範囲が宣言されていない。
    ///
    /// 範囲が無ければ「範囲外」も無い。**測った事実は残すが、
    /// 範囲の照合はできない**ことを型で言い切る (黙って空の
    /// `Measured` にすると「担当内だけだった」と読める)。
    NoScope { measured: Vec<String> },
}

impl FileEvidence {
    /// 実測できた変更 (表示・台帳用)。
    pub fn measured_paths(&self) -> &[String] {
        match self {
            FileEvidence::Measured { mine, .. } => mine,
            FileEvidence::NoScope { measured } => measured,
            FileEvidence::Unavailable(_) | FileEvidence::Unmeasurable(_) => &[],
        }
    }
}

/// 受理された報告。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedResult {
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub status: ReportedStatus,
    pub summary: String,
    /// **実測した**変更ファイル。台帳と画面はこちらを「変更したファイル」
    /// として扱う。
    pub changed_files: Vec<String>,
    /// エージェントの**自己申告**。証跡ではなく参考情報。
    ///
    /// 実測と食い違ったら、その食い違い自体が読み手への情報になる
    /// ([`Self::report_mismatch`])。
    pub reported_files: Vec<String>,
    pub validation: Vec<ValidationRun>,
    pub blockers: Vec<String>,
}

impl AcceptedResult {
    /// 自己申告と実測の食い違い (`申告し忘れ`, `申告だけあって実体が無い`)。
    ///
    /// **どちらも黙って捨てない。** 前者は「何を変えたか把握していない」
    /// の印で、後者は「やったつもりで何も変わっていない」の印。
    pub fn report_mismatch(&self) -> (Vec<String>, Vec<String>) {
        let missing: Vec<String> = self
            .changed_files
            .iter()
            .filter(|m| !self.reported_files.iter().any(|r| r == *m))
            .cloned()
            .collect();
        let phantom: Vec<String> = self
            .reported_files
            .iter()
            .filter(|r| !self.changed_files.iter().any(|m| m == *r))
            .cloned()
            .collect();
        (missing, phantom)
    }
}

/// **文字列の中の生の制御文字を、JSON として読める形へ直す。**
///
/// エージェントは本文の中で**改行をそのまま**書いてくる。JSON の仕様では
/// 文字列の中に生の制御文字を置けないので、`serde_json` は
/// 「control character found while parsing a string」で断る。
///
/// 実機では**伝言 14 通が全部これで捨てられていた** — 仕組みは動いているのに
/// 1 通も届かなかった。形式を守れと言い続けるより、こちらが読めるように
/// するほうが速い (相手は毎回違うモデルで、こちらの都合は知らない)。
///
/// **文字列の中だけ**を直す。構造 (波括弧やカンマの間の改行) は JSON として
/// 正しいので触らない。
pub fn escape_raw_controls(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 16);
    let mut in_str = false;
    let mut esc = false;
    for c in src.chars() {
        if esc {
            out.push(c);
            esc = false;
            continue;
        }
        match c {
            '\\' if in_str => {
                out.push(c);
                esc = true;
            }
            '"' => {
                in_str = !in_str;
                out.push(c);
            }
            '\n' if in_str => out.push_str("\\n"),
            '\r' if in_str => out.push_str("\\r"),
            '\t' if in_str => out.push_str("\\t"),
            c if in_str && (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            _ => out.push(c),
        }
    }
    out
}

/// **手書きの JSON を、意味を変えずに直す。ここが唯一の直し場。**
///
/// 報告・レビュー・出来事・伝言の**すべて**がここを通る
/// ([`parse_lenient`])。種類ごとに別々の直し方を持つと、片方だけが
/// 直る = 「実装は届くのにレビューは落ちる」という説明できない差が出る。
///
/// 実測 (Team Run 1 本) で捨てられていたのは 3 通り。どれも**中身は正しく、
/// 綴りだけが JSON になっていない**:
///
/// 1. **文字列の中の生の `"`** — いちばん多い。実物:
///    `"command": "test -s a.md && test "$(rg -c '^x' a.md)" -eq 8"`
///    シェルのコマンドを書けばまず入る
/// 2. **鍵の間のカンマ抜け** — `{"to": "a" "text": "b"}`
/// 3. **末尾カンマ** — `{"text": "x",}`
///
/// 捨てると、**正しく働いた担当が「報告していない」ことになって止まる**。
/// 実機では 1 本の Run で 4 回続けて捨てていた。
///
/// 直すのは綴りだけで、**値は 1 文字も変えない**。閉じ引用符かどうかは
/// 「その後に何が来るか」で決める — JSON で文字列の直後に来てよいのは
/// `,` `}` `]` `:` と、次の鍵 (`"…":`) だけである。
pub fn repair_json(src: &str) -> String {
    let ch: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len() + 16);
    let mut i = 0usize;
    let mut in_str = false;
    while i < ch.len() {
        let c = ch[i];
        if !in_str {
            if c == '"' {
                in_str = true;
                out.push(c);
                i += 1;
                continue;
            }
            // 3) 末尾カンマ — `,` の後に `}` / `]` しか無ければ落とす。
            if c == ',' && next_is_close(&ch, i + 1) {
                i += 1;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        // 文字列の中。
        if c == '\\' {
            out.push(c);
            if let Some(&n) = ch.get(i + 1) {
                out.push(n);
            }
            i += 2;
            continue;
        }
        if c != '"' {
            out.push(c);
            i += 1;
            continue;
        }
        match closer_kind(&ch, i + 1) {
            // 閉じ引用符。
            Closer::Plain => {
                in_str = false;
                out.push(c);
            }
            // 2) 閉じ引用符だが、次の鍵との間にカンマが無い。
            Closer::NeedsComma => {
                in_str = false;
                out.push(c);
                out.push(',');
            }
            // 1) 本文の中の `"`。エスケープして文字列を続ける。
            Closer::No => out.push_str("\\\""),
        }
        i += 1;
    }
    out
}

/// `"` の後ろが何であるか。
enum Closer {
    /// 文字列を閉じてよい (`,` `}` `]` `:` か、入力の終わり)。
    Plain,
    /// 文字列を閉じてよいが、次の鍵との間にカンマが要る。
    NeedsComma,
    /// 閉じない (本文の中の `"`)。
    No,
}

/// `from` から空白を飛ばして、`"` が閉じ引用符かを見る。
fn closer_kind(ch: &[char], from: usize) -> Closer {
    let Some(j) = skip_ws(ch, from) else {
        return Closer::Plain; // 入力の終わり
    };
    match ch[j] {
        ',' | '}' | ']' | ':' => Closer::Plain,
        // 次が `"…":` なら、鍵の始まりなのでカンマが抜けている。
        '"' if is_key_at(ch, j) => Closer::NeedsComma,
        _ => Closer::No,
    }
}

/// `,` の後ろが `}` / `]` か (= 末尾カンマ)。
fn next_is_close(ch: &[char], from: usize) -> bool {
    skip_ws(ch, from).is_some_and(|j| ch[j] == '}' || ch[j] == ']')
}

/// `at` の `"` から始まる文字列の直後が `:` か (= 鍵)。
fn is_key_at(ch: &[char], at: usize) -> bool {
    let mut i = at + 1;
    while i < ch.len() {
        match ch[i] {
            '\\' => i += 2,
            '"' => return skip_ws(ch, i + 1).is_some_and(|j| ch[j] == ':'),
            _ => i += 1,
        }
    }
    false
}

/// 空白でない最初の位置。
fn skip_ws(ch: &[char], from: usize) -> Option<usize> {
    (from..ch.len()).find(|&i| !ch[i].is_whitespace())
}

/// **まだ読める形になっていない塊か。**
///
/// 画面は書いている途中でも見える。1 tick が描画の途中に当たると、
/// 開きマーカーと閉じマーカーの間に**中身の断片**しか無いことがある
/// (実機で `"task_id"` だけの塊を拾い、`invalid type: string "task_id"`
/// として報告を断っていた)。
///
/// 断ると 2 つ困る。書いた本人には落ち度が無いのに却下が記録され、
/// **人には「エージェントが壊れた報告を出した」ように見える**。
/// 次の tick には全部揃うので、ここは黙って見送るのが正しい。
///
/// 判定は形だけで見る — JSON オブジェクトは `{` で始まる。
pub fn looks_incomplete(body: &str) -> bool {
    !body.trim_start().starts_with('{')
}

/// **ブロック本文を読む。報告もレビューも出来事も伝言も、ここを通る。**
///
/// 1. まず素直に読む (正しい JSON はこれまでどおり `serde` が読む)
/// 2. 読めなければ [`repair_json`] で綴りだけ直して、もう一度読む
///
/// **エラーは 1 回目のものを返す。** 直した後のエラーを返すと、こちらが
/// 手を入れた文字列についての苦情になって、元の原因が見えなくなる。
pub fn parse_lenient<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, String> {
    let src = escape_raw_controls(body);
    match serde_json::from_str(&src) {
        Ok(v) => Ok(v),
        Err(first) => {
            // **直したものは、元と同じ制御文字の扱いで読み直す。**
            let fixed = escape_raw_controls(&repair_json(&src));
            serde_json::from_str(&fixed).map_err(|_| first.to_string())
        }
    }
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


// ── 自分が送った指示のエコー ─────────────────────────────────────────

/// 拾った塊が、**こちらが送った指示のひな型がそのまま画面に出ているだけ**か。
///
/// ## なぜ要るか (実測)
///
/// 指示は PTY へ打ち込むので、エージェントの TUI は**それをそのまま画面へ
/// 描き返す**。指示には報告のひな型がマーカーごと載っているので、
/// [`extract_blocks`] は **自分が送った文面を相手の報告として拾う**。
///
/// 0.23.0 の実機では、Team Run を開始した直後に必ず
/// `報告の JSON を読めません: invalid type: string "task_id" …` が出ていた —
/// 端末が枠を描き直している途中の、まだ `{` が無い状態を拾っていた。
/// **落ちるほうがまだ軽い**: 全部描き終わってから拾うと、ひな型は
/// `"status": "completed"` を持つ**正しい JSON**なので、1 文字も作業して
/// いないのに完了報告として通ってしまう。
///
/// ## 判定を「一致」ではなく「部分」にする理由
///
/// 端末は行を詰め物で埋め、折り返し、描き直しの途中を見せる。実測の本文は
/// 先頭の `{` を失っていた。だから空白を 1 個へ畳んだうえで
/// **ひな型の部分列なら echo** とみなす。本物の報告はひな型と違う
/// `summary` / `changed_files` を持つので、部分列にはならない
/// (ひな型を 1 文字も埋めずに送り返してきた場合は echo 扱いで捨てるが、
///  それは報告として受け取ってはいけないものなので正しい)。
pub fn is_prompt_echo(body: &str, sent: &str, open: &str, close: &str) -> bool {
    let b = squeeze_ws(body);
    if b.is_empty() {
        return true;
    }
    extract_blocks(sent, open, close)
        .iter()
        .any(|t| squeeze_ws(t).contains(&b))
}

/// 連続する空白を 1 個へ畳む (端末の詰め物・折り返しを無視するため)。
fn squeeze_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut sp = false;
    for c in s.chars() {
        if c.is_whitespace() {
            sp = true;
            continue;
        }
        if sp && !out.is_empty() {
            out.push(' ');
        }
        sp = false;
        out.push(c);
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
    let doc: ResultDoc = parse_lenient(body).map_err(RejectReason::BadJson)?;
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
pub fn accept(
    doc: ResultDoc,
    task: &TeamTask,
    evidence: &FileEvidence,
) -> Result<AcceptedResult, RejectReason> {
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
            changed_files: evidence.measured_paths().to_vec(),
            reported_files: changed,
            summary: super::model::clamp_text(&doc.summary),
            validation,
            blockers: doc.blockers.clone(),
        });
    }

    // 1) 担当外のファイルを触っていないか。
    //
    //    **根拠は実測**。自己申告 (`doc.changed_files`) は、書き忘れても
    //    意図的に省いても同じ「空の配列」になるので、これを唯一の根拠に
    //    すると担当外の変更が素通りする。
    match evidence {
        // 測れていない = 「担当範囲を守った」と言える根拠が無い。
        // **通さない** (人へ渡す)。
        FileEvidence::Unavailable(why) => {
            return Err(RejectReason::EvidenceUnavailable(why.clone()));
        }
        // **測る手立てが無いなら、実測は求めない。**
        // ここで止めると、Git 管理下でないフォルダでは 1 件も完了できない。
        // 通すかわりに「実測なし」を盤面が出す (隠さない)。
        FileEvidence::Unmeasurable(_) => {}
        FileEvidence::Measured { out_of_scope, .. } if !out_of_scope.is_empty() => {
            return Err(RejectReason::OutOfScopeFiles(out_of_scope.clone()));
        }
        _ => {}
    }
    // 自己申告のほうも見る。**実測とは別に咎める** — 直し方が違う
    // (申告を直すのか、変更を戻すのか)。担当範囲が空のタスクは
    // 照合しない (何を触ってよいかこちらが言っていない)。
    if !task.files.is_empty() {
        let bad: Vec<String> = changed
            .iter()
            .filter(|f| !task.files.iter().any(|p| crate::lease::overlaps(p, f)))
            .cloned()
            .collect();
        if !bad.is_empty() {
            return Err(RejectReason::OutOfScopeReported(bad));
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
        // **台帳へ載るのは実測のほう。** 自己申告を載せると、後から
        // 見た人が「これが実際に変わったファイルだ」と読んでしまう。
        changed_files: evidence.measured_paths().to_vec(),
        reported_files: changed,
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

/// エージェントが他のエージェントへ送る 1 通。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageDoc {
    /// 宛先のエージェント ID (`implementer-1` 等) か役割 (`reviewer`)、
    /// または `all` (チーム全員)。
    pub to: String,
    /// 本文。
    pub text: String,
}

/// メッセージを断った理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageReject {
    BadJson(String),
    /// 宛先が空。
    NoTarget,
    /// 宛先が居ない。**捏造を通さない** (居ない相手に送ったことにしない)。
    UnknownTarget(String),
    /// 宛先が**送り主自身**を指している (自分の ID か、自分の役割)。
    ///
    /// **`UnknownTarget` と混ぜてはいけない。** 相手は居るのだから
    /// 「居ません」は嘘で、そう言われたエージェントは綴りを疑って
    /// 同じ宛先を書き直す (実機で 2 回繰り返した: 役割 `tester` の担当が
    /// `"to": "tester"` と書き、`伝言の宛先 tester は居ません` と返っていた)。
    SelfTarget(String),
    /// 宛先は `all` だが、**自分以外の担当が 1 人も居ない**。
    ///
    /// 居ないのは相手ではなく「あなた以外の担当」なので、
    /// `UnknownTarget("all")` と言うと `all` の綴りを疑わせてしまう。
    NoOtherAgents,
    /// 本文が空。
    Empty,
}

impl MessageReject {
    pub fn detail(&self) -> String {
        match self {
            MessageReject::BadJson(e) => format!("伝言の JSON を読めません: {e}"),
            MessageReject::NoTarget => "伝言の宛先 (to) が空です".to_string(),
            MessageReject::UnknownTarget(t) => {
                format!("伝言の宛先 `{t}` は居ません")
            }
            MessageReject::SelfTarget(t) => format!(
                "伝言の宛先 `{t}` はあなた自身です。自分宛ての伝言は誰にも届きません — \
                 宛先には**相手**の ID か役割を書いてください (全員なら all)"
            ),
            MessageReject::NoOtherAgents => "宛先 `all` は「あなた以外の全員」です。\
                 いまチームで動いているのはあなただけなので、受け取る担当が 1 人も居ません"
                .to_string(),
            MessageReject::Empty => "伝言の本文が空です".to_string(),
        }
    }
}

/// **エージェントが手で書く JSON は、素直には読めない。**
///
/// 実測 (Team Run 1 本・19 通) で 5 通が読めず、内訳は 4 通りだった:
///
/// | 実際に来たもの | serde の言い分 |
/// |---|---|
/// | `"text": "彼は "yes" と言った"` | `expected ',' or '}'` |
/// | `"to": "a" "text": "b"` (カンマ抜け) | `expected ',' or '}'` |
/// | `"text": "x",}` (末尾カンマ) | `trailing comma` |
/// | `"text": "C:\path"` (Windows パス) | `invalid escape` |
///
/// どれも**人間が読めば意味は明らか**なのに、1 通まるごと捨てていた。
/// 伝言が落ちると相手は待ち続けるので、落とす代償が大きい。
///
/// そこで JSON の文法には頼らず、`to` と `text` の値を直接取り出す。
/// 鍵は 2 つしかないので、これで曖昧さは出ない:
///
/// * `to` — 短い識別子。**最初の**閉じ引用符まで
/// * `text` — 自由文。**最後の**閉じ引用符まで (中の `"` を巻き込む)
///
/// **`serde` を置き換えない。** 正しい JSON は今までどおり `serde` が読む
/// ([`check_message`] が先に試す)。ここは読めなかったときの受け皿で、
/// 読めた振りをしない — 鍵が見つからなければ `None` を返して断りに戻す。
fn lenient_message(s: &str) -> Option<MessageDoc> {
    let to_at = key_value_start(s, "to")?;
    let text_at = key_value_start(s, "text")?;
    // `to` は最初の閉じ引用符まで (識別子に `"` は入らない)。
    let to_end = closing_quote(s, to_at, false)?;
    // `text` は最後の閉じ引用符まで。ただし `to` が後ろにあるなら、
    // その手前で止める (`to` の値の引用符を巻き込まないため)。
    //
    // **括弧の外まで飲み込まない。** 塊の中に後書き (`}` のあとの一行など)
    // が混じることがあり、そこに `"` があると本文が伸びてしまう。
    let mut limit = s.rfind('}').unwrap_or(s.len());
    if to_at > text_at {
        limit = limit.min(key_pos(s, "to").unwrap_or(s.len()));
    }
    let limit = limit.max(text_at);
    let text_end = closing_quote(&s[..limit], text_at, true)?;
    Some(MessageDoc {
        to: unescape_lenient(&s[to_at..to_end]),
        text: unescape_lenient(&s[text_at..text_end]),
    })
}

/// `"<key>"` という**鍵**の開始位置。値ではない。
fn key_pos(s: &str, key: &str) -> Option<usize> {
    let pat = format!("\"{key}\"");
    let mut from = 0usize;
    while let Some(i) = s[from..].find(&pat) {
        let at = from + i;
        // 鍵の後ろは `:` (空白は挟んでよい)。値の中の同じ綴りを拾わない。
        let rest = s[at + pat.len()..].trim_start();
        if rest.starts_with(':') {
            return Some(at);
        }
        from = at + pat.len();
    }
    None
}

/// `"<key>": "` の**値の中身**が始まる位置。
fn key_value_start(s: &str, key: &str) -> Option<usize> {
    let at = key_pos(s, key)?;
    let after = at + key.len() + 2;
    let rest = &s[after..];
    let colon = rest.find(':')?;
    let tail = &rest[colon + 1..];
    let quote = tail.len() - tail.trim_start().len();
    if !tail[quote..].starts_with('"') {
        return None;
    }
    Some(after + colon + 1 + quote + 1)
}

/// `from` から見て、**エスケープされていない** `"` の位置。
///
/// `last` なら最後のもの、そうでなければ最初のもの。
fn closing_quote(s: &str, from: usize, last: bool) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut found = None;
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => {
                found = Some(i);
                if !last {
                    return found;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    found
}

/// JSON のエスケープを解く。**知らない綴りはそのまま残す。**
///
/// `C:\path` の `\p` を「不正」として 1 通捨てるより、`\p` のまま
/// 届けるほうが利用者の役に立つ。
fn unescape_lenient(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hex: String = it.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) => out.push(ch),
                    // 読めなければ**元の綴りを残す** (黙って消さない)。
                    None => {
                        out.push('\\');
                        out.push('u');
                        out.push_str(&hex);
                    }
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// **伝言 1 通を読む。ここが伝言の読み方の唯一の置き場。**
///
/// Team Run と通常タブで**同じ 1 つを使う** — 2 つ持つと、片方だけが
/// 綴り間違いを拾えるようになって「Team では届くのに通常タブでは届かない」
/// という説明できない差が出る。番人は
/// `cli::tests::伝言の読み手はひとつだけ`。
pub fn read_message(body: &str) -> Result<MessageDoc, MessageReject> {
    // **正しい JSON はこれまでどおり `serde` が読む。**
    // 読めなかったときだけ、鍵を直接拾う受け皿へ落ちる
    // (`lenient_message` の表に、実際に来た 4 通りが載っている)。
    // 1) `parse_lenient` (素直に読む → 綴りを直して読む) は報告と共通。
    // 2) それでも駄目なら、伝言だけの受け皿 (鍵を 2 つ直接拾う) へ落ちる。
    let mut doc: MessageDoc = match parse_lenient(body) {
        Ok(d) => d,
        Err(e) => {
            lenient_message(&escape_raw_controls(body)).ok_or(MessageReject::BadJson(e))?
        }
    };
    // **整形もここで済ませる。** 呼ぶ側で `trim` と上限を書くと、
    // 片方だけ上限が違う伝言が届く。
    doc.to = doc.to.trim().to_string();
    doc.text = doc.text.trim().chars().take(MSG_MAX_CHARS).collect();
    Ok(doc)
}

/// **伝言の作法の文面。ここが唯一の置き場。**
///
/// Team の指示文 (`prompt::teammates_section`) と、通常タブへ教える文面
/// (`agent_talk::how_to`) が同じものを使う。2 つ書くと、片方だけに
/// 「エスケープの書き方」が載っている状態になって、そちらのエージェントだけ
/// 伝言を落とす。
///
/// `target_hint` は宛先の書き方 (Team は ID / 役割、通常タブはタブの名前)。
pub fn message_howto(target_hint: &str) -> String {
    format!(
        "{MSG_OPEN}\n\
         {{\"to\": \"{target_hint}\", \"text\": \"伝えたいことを 1〜3 行で\"}}\n\
         {MSG_CLOSE}\n\n\
         * 伝えるのは**相手の仕事が変わるとき**だけ。実況中継はしない\n\
         * 本文は {MSG_MAX_CHARS} 文字まで。長い成果物はファイルに書いて、場所だけ伝える\n\
         * 相手が居ない宛先を書かない (届かず、こちらに断りが記録されます)\n\
         * 宛先は**相手**を書く。自分の ID や自分の役割を書いても誰にも届かない\n\
         * 本文の `\"` は `\\\"`、`\\` は `\\\\` と書く \
         (Windows のパスは区切りを 2 つ重ねる)。書けていなくても読み取りますが、\
         `\\t` などは制御文字として読まれます\n"
    )
}

/// 伝言を読んで、宛先を**実在の担当**へ解決する。
///
/// `known` は `(エージェント ID, 役割キー)` の一覧。
/// **表に無い宛先は断る** — 居ない相手へ送ったことにすると、盤面には
/// 「伝えた」と出るのに誰も受け取っていない、という嘘になる。
///
/// `all` は自分以外の全員。宛先に自分を含めない (自分への伝言は
/// 端末をもう一度自分で読むだけで、何も起きない)。
pub fn check_message(
    body: &str,
    known: &[(AgentId, String)],
    from: &AgentId,
) -> Result<(Vec<AgentId>, String), MessageReject> {
    let doc = read_message(body)?;
    let to = doc.to.as_str();
    if to.is_empty() {
        return Err(MessageReject::NoTarget);
    }
    let text = doc.text;
    if text.is_empty() {
        return Err(MessageReject::Empty);
    }
    let others = || known.iter().filter(|(id, _)| id != from);
    let all = to.eq_ignore_ascii_case("all");
    let hits = |id: &AgentId, role: &str| id.0 == to || role == to;
    let targets: Vec<AgentId> = if all {
        others().map(|(id, _)| id.clone()).collect()
    } else {
        others()
            .filter(|(id, role)| hits(id, role))
            .map(|(id, _)| id.clone())
            .collect()
    };
    if targets.is_empty() {
        // **「居ません」は最後の枝。** 宛先が自分自身のときにそう言うと、
        // 相手は居るのに居ないと言われたことになり、綴りを疑って同じ
        // 間違いを繰り返す (実機で 2 回起きた)。何が起きたかを言う。
        if all {
            return Err(MessageReject::NoOtherAgents);
        }
        let is_self = from.0 == to
            || known
                .iter()
                .any(|(id, role)| id == from && hits(id, role.as_str()));
        if is_self {
            return Err(MessageReject::SelfTarget(to.to_string()));
        }
        // 表に無い宛先は今までどおり断る (捏造を通さない)。
        return Err(MessageReject::UnknownTarget(to.to_string()));
    }
    Ok((targets, text))
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
    /// **ひな型のまま**の名前 (`<子の名前>` など)。実在の担当にしない。
    PlaceholderAgent(String),
}

impl EventReject {
    pub fn detail(&self) -> String {
        match self {
            EventReject::BadJson(e) => format!("イベントの JSON を読めません: {e}"),
            EventReject::PlaceholderAgent(id) => format!(
                "エージェント ID「{id}」はひな型のままです。実際に使った名前を書いてください"
            ),
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
    let doc: EventDoc = parse_lenient(body).map_err(EventReject::BadJson)?;
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
        // **ひな型のまま送ってきたものを実在の担当にしない。**
        //
        // 指示文は `"agent_id": "<子の名前>"` という穴埋めを見せる。そのまま
        // 出してくるエージェントが実際に居て、盤面に `<子の名前>` という
        // 担当が並んだ (実機で観測)。山括弧は名前に使わないので、これで見分く。
        let id = doc.agent_id.trim();
        if id.starts_with('<') || id.ends_with('>') {
            return Err(EventReject::PlaceholderAgent(id.to_string()));
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

    /// 「担当内だけを変更した」と**実測できた**証跡。
    fn clean() -> FileEvidence {
        FileEvidence::Measured {
            mine: vec!["src/auth.rs".to_string()],
            out_of_scope: Vec::new(),
        }
    }

    /// 担当外まで変更したと**実測できた**証跡。
    fn dirty(out: &[&str]) -> FileEvidence {
        FileEvidence::Measured {
            mine: vec!["src/auth.rs".to_string()],
            out_of_scope: out.iter().map(|s| s.to_string()).collect(),
        }
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
        let acc = accept(doc, &assigned(), &clean()).expect("受理されるべき");
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
            accept(doc, &assigned(), &clean()),
            Err(RejectReason::TaskMismatch { got: 99, want: 12 })
        );
    }

    #[test]
    fn エージェントid不一致を拒否する() {
        let mut doc = parse_result(GOOD).unwrap();
        doc.agent_id = "someone-else".into();
        assert!(matches!(
            accept(doc, &assigned(), &clean()),
            Err(RejectReason::AgentMismatch { .. })
        ));
    }

    #[test]
    fn 担当外ファイルの変更を拒否する() {
        // 自己申告のほうに担当外が出ていたら、それも咎める
        // (**実測とは別の理由**として出す — 直し方が違う)。
        let mut doc = parse_result(GOOD).unwrap();
        doc.changed_files.push("src/other.rs".into());
        assert_eq!(
            accept(doc, &assigned(), &clean()),
            Err(RejectReason::OutOfScopeReported(vec!["src/other.rs".into()]))
        );
    }

    // ── 実測を根拠にする ─────────────────────────────────────────────

    #[test]
    fn 自己申告が空でも実測が担当外なら拒否する() {
        // **これが中核。** 申告し忘れても、意図的に省いても、こちらから
        // 見れば同じ「空の配列」になる。自己申告を唯一の根拠にすると、
        // 担当外を書き換えたタスクが素通りする。
        let mut doc = parse_result(GOOD).unwrap();
        doc.changed_files.clear();
        assert_eq!(
            accept(doc, &assigned(), &dirty(&["src/secret.rs"])),
            Err(RejectReason::OutOfScopeFiles(vec!["src/secret.rs".into()])),
            "申告さえしなければ担当外の変更が通ってしまう"
        );
    }

    #[test]
    fn 自己申告から担当外を省いても実測で拒否する() {
        // 担当内のファイルだけを申告し、担当外の変更は黙っている形。
        let doc = parse_result(GOOD).unwrap();
        assert_eq!(doc.changed_files, vec!["src/auth.rs".to_string()]);
        assert_eq!(
            accept(doc, &assigned(), &dirty(&["../outside.rs", "docs/x.md"])),
            Err(RejectReason::OutOfScopeFiles(vec![
                "../outside.rs".into(),
                "docs/x.md".into()
            ]))
        );
    }

    #[test]
    fn 実測が担当内なら通り台帳には実測が載る() {
        let mut doc = parse_result(GOOD).unwrap();
        // 申告し忘れがあっても、実測が担当内なら完了そのものは通す。
        doc.changed_files.clear();
        let acc = accept(doc, &assigned(), &clean()).expect("受理されるべき");
        assert_eq!(
            acc.changed_files,
            vec!["src/auth.rs".to_string()],
            "台帳へ載るのは実測のほう"
        );
        assert!(acc.reported_files.is_empty(), "自己申告は自己申告として残す");
        // 食い違いが見える形で残る。
        let (missing, phantom) = acc.report_mismatch();
        assert_eq!(missing, vec!["src/auth.rs".to_string()]);
        assert!(phantom.is_empty());
    }

    #[test]
    fn 申告だけあって実体が無いことも見える() {
        // 「やったつもりで何も変わっていない」の印。
        let doc = parse_result(GOOD).unwrap();
        let acc = accept(
            doc,
            &assigned(),
            &FileEvidence::Measured {
                mine: Vec::new(),
                out_of_scope: Vec::new(),
            },
        )
        .expect("受理されるべき");
        let (missing, phantom) = acc.report_mismatch();
        assert!(missing.is_empty());
        assert_eq!(phantom, vec!["src/auth.rs".to_string()]);
    }

    #[test]
    fn 実測できないなら完了にしない() {
        // **保証を偽らない。** 測れないのに通すと、台帳には「担当内だけ」
        // と残る。人へ渡すのが正しい。
        let doc = parse_result(GOOD).unwrap();
        let got = accept(
            doc,
            &assigned(),
            &FileEvidence::Unavailable("git 管理下ではありません".into()),
        );
        assert_eq!(
            got,
            Err(RejectReason::EvidenceUnavailable(
                "git 管理下ではありません".into()
            ))
        );
        assert!(
            got.unwrap_err().detail().contains("git 管理下"),
            "人へ理由が伝わらない"
        );
    }

    #[test]
    fn 実測できなくても完了以外はそのまま受け取る() {
        // 「進められない」「失敗した」は、実測できるかどうかとは無関係。
        // ここまで止めると、詰まったタスクが誰にも見えなくなる。
        for st in ["blocked", "failed"] {
            let mut doc = parse_result(GOOD).unwrap();
            doc.status = st.into();
            let acc = accept(doc, &assigned(), &FileEvidence::Unavailable("x".into()))
                .unwrap_or_else(|e| panic!("{st} を止めた: {e:?}"));
            assert_ne!(acc.status, ReportedStatus::Completed);
        }
    }

    #[test]
    fn 担当範囲が無いタスクは範囲外を作らない() {
        // 何を触ってよいかこちらが言っていないのに咎めるのは筋が通らない。
        // ただし**測った事実は残す**。
        let mut t = assigned();
        t.files.clear();
        let doc = parse_result(GOOD).unwrap();
        let acc = accept(
            doc,
            &t,
            &FileEvidence::NoScope {
                measured: vec!["src/anything.rs".to_string()],
            },
        )
        .expect("受理されるべき");
        assert_eq!(acc.changed_files, vec!["src/anything.rs".to_string()]);
    }

    #[test]
    fn 自己申告を書き換えるだけでは通らない() {
        // **申告を「正直」にしても、実測が担当外なら通らない。**
        // ここが通ってしまうと、結局は自己申告で結果が決まっている。
        for reported in [
            vec![],
            vec!["src/auth.rs".to_string()],
            vec!["src/auth.rs".to_string(), "src/other.rs".to_string()],
        ] {
            let mut doc = parse_result(GOOD).unwrap();
            doc.changed_files = reported.clone();
            let got = accept(doc, &assigned(), &dirty(&["src/other.rs"]));
            assert!(
                matches!(
                    got,
                    Err(RejectReason::OutOfScopeFiles(_))
                        | Err(RejectReason::OutOfScopeReported(_))
                ),
                "申告を {reported:?} にしただけで通った: {got:?}"
            );
        }
    }

    #[test]
    fn 検証未実行を拒否する() {
        let mut doc = parse_result(GOOD).unwrap();
        doc.validation.clear();
        assert_eq!(
            accept(doc, &assigned(), &clean()),
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
            accept(doc, &assigned(), &clean()),
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
            accept(doc, &assigned(), &clean()),
            Err(RejectReason::BlockersRemain(_))
        ));
    }

    #[test]
    fn 未知のstatusを拒否する() {
        let mut doc = parse_result(GOOD).unwrap();
        doc.status = "almost".into();
        assert!(matches!(
            accept(doc, &assigned(), &clean()),
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

    // ── 自分が送った指示のエコー ─────────────────────────────────────

    /// 送った指示の**実物**を組み立てる (ひな型は `prompt.rs` の 1 か所だけ)。
    fn sent_instruction_sample() -> String {
        let goal = super::super::testkit::goal();
        let mut t = task(1, "web3d", &[]);
        t.title = "かっこいい３DのWebページを作って".to_string();
        t.assigned_agent = Some(AgentId::new("team-lead"));
        let brief = super::super::prompt::Brief {
            goal: &goal,
            task: &t,
            agent_id: "team-lead",
            parent_id: None,
            workspace_root: "<ワークスペースルート>",
            upstream: Vec::new(),
            forbidden_files: Vec::new(),
            outbox: std::path::PathBuf::new(),
            teammates: Vec::new(),
        };
        super::super::prompt::for_task(&brief, std::slice::from_ref(&t))
    }

    /// **実機で実際に出た壊れ方をそのまま固定する。**
    ///
    /// 0.23.0 の Team Run は、開始直後に必ず
    /// `報告の JSON を読めません: invalid type: string "task_id" …` を出していた。
    /// 正体は「端末が指示の枠を描き直している途中」— 先頭の `{` がまだ無い
    /// 状態を、相手の報告として拾っていた。
    /// **測る手立てが無いフォルダでも完了できる。**
    ///
    /// 実機で `~/dev/Sharp` (Git 管理下でない) を相手にしたとき、7 体が
    /// 並列で働いているのに**どの完了報告も却下**され、1 件も終わらなかった:
    /// 「変更されたファイルを実測できないので完了にできません」。
    ///
    /// 直しようが無い理由で止め続けるのは、そのフォルダでこの機能を
    /// 使えなくするのと同じ。**通すが、盤面が「実測なし」を出す**。
    #[test]
    fn 測る手立てが無くても完了できる() {
        let task = assigned();
        let doc: ResultDoc = serde_json::from_str(GOOD).unwrap();
        // 測れるはずなのに失敗した → **通さない** (直せば測れるので人へ渡す)。
        let broken = FileEvidence::Unavailable("git が壊れています".into());
        assert!(matches!(
            accept(doc.clone(), &task, &broken),
            Err(RejectReason::EvidenceUnavailable(_))
        ));
        // そもそも測る手立てが無い → **通す**。
        let none = FileEvidence::Unmeasurable("Git 管理下ではありません".into());
        let ok = accept(doc, &task, &none).expect("完了できる");
        assert_eq!(ok.task_id, task.id);
        // **測れていないことは隠さない。** 実測は空のまま残る
        // (自己申告を実測に格上げしない)。
        assert!(
            ok.changed_files.is_empty(),
            "測っていないのに実測として載せている"
        );
        assert!(!ok.reported_files.is_empty(), "自己申告は残る");
    }

    #[test]
    fn 描き直し途中のエコーを報告として拾わない() {
        let sent = sent_instruction_sample();
        // 実機のログから起こした本文 (先頭の `{` が無い)。
        let body = "\"task_id\": 1,    \"agent_id\": \"team-lead\",    \
                    \"status\": \"completed\",    \"summary\": \"何をしたかの 1 行\"";
        assert!(
            parse_result(body).is_err(),
            "この本文は JSON として読めない (だから rejected が出ていた)"
        );
        assert!(
            is_prompt_echo(body, &sent, RESULT_OPEN, RESULT_CLOSE),
            "送った指示のひな型の一部なので、報告として扱ってはいけない"
        );
    }

    /// **全部描き終わったエコーは、落ちるより悪い。**
    ///
    /// ひな型は `"status": "completed"` を持つ正しい JSON なので、素直に
    /// 拾うと「1 文字も作業していないのに完了報告が届いた」ことになる。
    #[test]
    fn 描き終わったエコーは正しいjsonなので必ず弾く() {
        let sent = sent_instruction_sample();
        let body = extract_blocks(&sent, RESULT_OPEN, RESULT_CLOSE)
            .into_iter()
            .next()
            .expect("指示には報告のひな型が載っている");
        let doc = parse_result(&body).expect("ひな型はそれ自体が正しい JSON である");
        assert_eq!(doc.status, "completed", "だから黙って完了になり得た");
        assert!(is_prompt_echo(&body, &sent, RESULT_OPEN, RESULT_CLOSE));
    }

    /// **本物の報告は素通しする。** echo 判定が全部を飲み込んだら、
    /// 今度は永久に完了しなくなる (直したつもりで別の壊し方になる)。
    #[test]
    fn 本物の報告はエコーとみなさない() {
        let sent = sent_instruction_sample();
        let body = "{\"task_id\": 1, \"agent_id\": \"team-lead\", \
                    \"status\": \"completed\", \"summary\": \"index.html に three.js の球体を置いた\", \
                    \"changed_files\": [\"index.html\"], \"validation\": [], \"blockers\": []}";
        assert!(parse_result(body).is_ok());
        assert!(
            !is_prompt_echo(body, &sent, RESULT_OPEN, RESULT_CLOSE),
            "中身がひな型と違うのだから echo ではない"
        );
    }

    /// **本文に生の改行が入っていても伝言は届く。**
    ///
    /// 実機で伝言 14 通が全部「control character found while parsing a
    /// string」で捨てられていた。仕組みは動いているのに 1 通も届いて
    /// いなかった — エージェントは本文の改行をそのまま書いてくる。
    #[test]
    fn 本文に生の改行があっても伝言は届く() {
        let known: Vec<(AgentId, String)> = vec![
            (AgentId::new("a"), "implementer".into()),
            (AgentId::new("b"), "reviewer".into()),
        ];
        // 生の改行入り (これが実機で来ていた形)。
        let body = "{\"to\": \"b\", \"text\": \"設計が終わった。\n次は実装に入って\"}";
        assert!(
            serde_json::from_str::<MessageDoc>(body).is_err(),
            "素の serde_json はこれを読めない (だから捨てられていた)"
        );
        let (to, text) = check_message(body, &known, &AgentId::new("a")).expect("届く");
        assert_eq!(to, vec![AgentId::new("b")]);
        assert!(text.contains("次は実装に入って"), "本文が欠けている: {text:?}");
        assert!(text.contains('\n'), "改行が失われている: {text:?}");
    }

    /// **直すのは文字列の中だけ。** 構造の改行は JSON として正しいので触らない。
    #[test]
    fn 直すのは文字列の中だけ() {
        let pretty = "{\n  \"to\": \"b\",\n  \"text\": \"ok\"\n}";
        assert_eq!(escape_raw_controls(pretty), pretty, "構造を書き換えている");
        // 文字列の中のタブは逃がす。
        let raw = "{\"text\": \"a\tb\"}";
        assert!(escape_raw_controls(raw).contains("\\t"));
        // 既にエスケープ済みのものを二重にしない。
        let done = "{\"text\": \"a\\nb\"}";
        assert_eq!(escape_raw_controls(done), done, "二重にエスケープしている");
    }

}


#[cfg(test)]
mod lenient_message_tests {
    use super::*;

    fn known() -> Vec<(AgentId, String)> {
        vec![
            (AgentId("reviewer-1".into()), "reviewer".into()),
            (AgentId("impl-1".into()), "implementer".into()),
        ]
    }

    fn read(body: &str) -> Result<(Vec<AgentId>, String), MessageReject> {
        check_message(body, &known(), &AgentId("impl-1".into()))
    }

    /// **実測で落ちていた 4 通りが、全部届く。**
    ///
    /// Team Run 1 本 (19 通) のうち 5 通がこれで捨てられていた。
    /// 伝言が落ちると相手は待ち続けるので、落とす代償が大きい。
    #[test]
    fn 手書きのjsonの綴り間違いでも届く() {
        // 1) 本文に生の `"` (いちばん多い)
        let (to, text) = read(r#"{"to": "reviewer-1", "text": "彼は "yes" と言った"}"#)
            .expect("生の引用符で落ちた");
        assert_eq!(to, vec![AgentId("reviewer-1".into())]);
        assert_eq!(text, r#"彼は "yes" と言った"#);
        // 2) 鍵の間のカンマ抜け
        let (_, text) =
            read(r#"{"to": "reviewer-1" "text": "カンマが無い"}"#).expect("カンマ抜けで落ちた");
        assert_eq!(text, "カンマが無い");
        // 3) 末尾カンマ
        let (_, text) = read(r#"{"to": "reviewer-1", "text": "末尾カンマ",}"#)
            .expect("末尾カンマで落ちた");
        assert_eq!(text, "末尾カンマ");
        // 4) Windows パス (`\U` / `\m` は JSON の不正エスケープ)
        let (_, text) =
            read(r#"{"to": "reviewer-1", "text": "C:\Users\me を見て"}"#).expect("パスで落ちた");
        assert_eq!(text, r"C:\Users\me を見て", "知らない綴りを消してしまった");
    }

    /// **区別が付かないものは、付かないと認める。**
    ///
    /// `\t` / `\n` は JSON の正当なエスケープなので、`C:\temp` と
    /// 「タブ + emp」を**入力からは見分けられない** (serde も同じ)。
    /// ここは正しい JSON と同じ読み方に揃える — 1 通まるごと捨てるよりは、
    /// 一部が化けても届いたほうが相手の仕事が進む。
    ///
    /// 直したいなら**入力の側**を変えるしかない (指示文で「パスは
    /// `\\` で書く」と教える)。読み手側では決められない。
    #[test]
    fn 正当なエスケープと紛れるパスは化ける() {
        let (_, text) =
            read(r#"{"to": "reviewer-1", "text": "C:\temp の "log" を見て"}"#).unwrap();
        assert_eq!(text, "C:\temp の \"log\" を見て".replace("\\t", "\t"));
        assert!(text.contains('\t'), "タブとして読まれていない");
        assert!(text.contains(r#""log""#), "伝言そのものは届いている");
    }

    /// **正しい JSON の読み方は 1 ミリも変えない。**
    ///
    /// 受け皿は「読めなかったとき」だけ通る。ここが変わると、いままで
    /// 届いていた 14 通の意味が黙って変わる。
    #[test]
    fn 正しいjsonはこれまでどおり() {
        let (to, text) = read(r#"{"to": "all", "text": "改行\nと \"引用符\" と \\ "}"#).unwrap();
        assert_eq!(to, vec![AgentId("reviewer-1".into())], "all は自分を除く");
        assert_eq!(text, "改行\nと \"引用符\" と \\");
    }

    /// **読めた振りをしない。** 鍵が無いものは今までどおり断る。
    /// 拾えなかったものを黙って空の伝言にすると、盤面には「伝えた」と
    /// 出るのに中身が無い、という嘘になる。
    #[test]
    fn 鍵が無いものは断る() {
        assert!(matches!(
            read(r#"{"dest": "reviewer-1", "body": "鍵の名前が違う"}"#),
            Err(MessageReject::BadJson(_))
        ));
        assert!(matches!(read("ただの文章です"), Err(MessageReject::BadJson(_))));
        // 宛先が空 / 本文が空は、受け皿を通っても断る。
        assert!(matches!(
            read(r#"{"to": "", "text": "宛先が空"}"#),
            Err(MessageReject::NoTarget)
        ));
        assert!(matches!(
            read(r#"{"to": "reviewer-1", "text": "  "}"#),
            Err(MessageReject::Empty)
        ));
        // 居ない相手は捏造しない。
        assert!(matches!(
            read(r#"{"to": "居ない人", "text": "やあ "君" "}"#),
            Err(MessageReject::UnknownTarget(_))
        ));
    }

    /// **実機で 2 回記録された却下そのもの。**
    ///
    /// `actor=agent-4 : 伝言の宛先 \`tester\` は居ません` — ところが
    /// agent-4 の役割がまさに `tester` だった。相手は居るのに「居ません」と
    /// 言われたので、綴りを疑って同じ宛先を書き直し、また断られた。
    ///
    /// 断る判断は変えない (自分宛ての伝言は誰にも届かない) が、
    /// **理由は実態に合わせる**。
    #[test]
    fn 自分の役割を宛先に書いたら居ないとは言わない() {
        let team = vec![
            (AgentId("agent-3".into()), "implementer".to_string()),
            (AgentId("agent-4".into()), "tester".to_string()),
        ];
        let me = AgentId("agent-4".into());
        let body = r#"{"to": "tester", "text": "テストを流します"}"#;
        let err = check_message(body, &team, &me).expect_err("自分宛ては届かない");
        assert_eq!(
            err,
            MessageReject::SelfTarget("tester".into()),
            "自分自身なのに UnknownTarget と言っている"
        );
        let d = err.detail();
        assert!(
            !d.contains("居ません"),
            "相手は居るのに「居ません」と言った: {d}"
        );
        assert!(d.contains("あなた自身"), "何が起きたか言えていない: {d}");
        assert!(d.contains("相手"), "どうすればよいか言えていない: {d}");

        // ID を書いた場合も同じ (綴り間違いではない、と伝わること)。
        let err = check_message(r#"{"to": "agent-4", "text": "自分へ"}"#, &team, &me)
            .expect_err("自分宛ては届かない");
        assert_eq!(err, MessageReject::SelfTarget("agent-4".into()));

        // **本物の宛先はこれまでどおり届く。**
        let (targets, _) =
            check_message(r#"{"to": "implementer", "text": "見て"}"#, &team, &me).unwrap();
        assert_eq!(targets, vec![AgentId("agent-3".into())]);
    }

    /// **`all` で誰も居ないのは、宛先の綴りの問題ではない。**
    ///
    /// 居ないのは相手ではなく「あなた以外の担当」なので、
    /// `all は居ません` と言うと `all` を疑わせてしまう。
    #[test]
    fn 自分しか居ないときのallは宛先のせいにしない() {
        let alone = vec![(AgentId("agent-4".into()), "tester".to_string())];
        let me = AgentId("agent-4".into());
        let err = check_message(r#"{"to": "all", "text": "みんなへ"}"#, &alone, &me)
            .expect_err("自分しか居ないので届かない");
        assert_eq!(err, MessageReject::NoOtherAgents);
        let d = err.detail();
        assert!(d.contains("あなただけ"), "実態を言えていない: {d}");

        // 相手が 1 人でも居れば、all はこれまでどおり届く。
        let team = vec![
            (AgentId("agent-3".into()), "implementer".to_string()),
            (AgentId("agent-4".into()), "tester".to_string()),
        ];
        let (targets, _) =
            check_message(r#"{"to": "all", "text": "みんなへ"}"#, &team, &me).unwrap();
        assert_eq!(targets, vec![AgentId("agent-3".into())]);
    }

    /// **捏造した宛先は今までどおり断る。**
    /// 自分自身の枝を足したせいで、表に無い宛先まで通ってはいけない。
    #[test]
    fn 表に無い宛先はこれまでどおり断る() {
        let team = vec![
            (AgentId("agent-3".into()), "implementer".to_string()),
            (AgentId("agent-4".into()), "tester".to_string()),
        ];
        let me = AgentId("agent-4".into());
        for to in ["designer", "agent-9", "tester-2", "TESTER"] {
            let body = format!(r#"{{"to": "{to}", "text": "やあ"}}"#);
            assert_eq!(
                check_message(&body, &team, &me).expect_err("居ない相手へ届けた"),
                MessageReject::UnknownTarget(to.to_string()),
                "宛先 {to}"
            );
        }
    }

    /// **鍵の順番が逆でも読める。** `text` を「最後の引用符まで」と読むので、
    /// 後ろに `to` があるとその値まで飲み込みかねない。
    #[test]
    fn 鍵の順番が逆でも本文を飲み込まない() {
        let (to, text) =
            read(r#"{"text": "先に本文 "引用" あり", "to": "reviewer-1"}"#).expect("読めない");
        assert_eq!(to, vec![AgentId("reviewer-1".into())]);
        assert_eq!(text, r#"先に本文 "引用" あり"#);
    }

    /// **括弧の外の文字を本文へ引き込まない。**
    ///
    /// 画面から切り出した塊には、`}` のあとに別の行が混じることがある。
    /// 「最後の引用符まで」を素直にやると、そこまで本文になる。
    #[test]
    fn 括弧の外の後書きを飲み込まない() {
        let (_, text) = read(
            "{\"to\": \"reviewer-1\", \"text\": \"本文 \"引用\" あり\"}\n書き終わり \"余談\"",
        )
        .expect("読めない");
        assert_eq!(text, "本文 \"引用\" あり");
    }

    /// **本文の中に鍵と同じ綴りがあっても惑わされない。**
    #[test]
    fn 本文の中の鍵らしい綴りを拾わない() {
        let (_, text) = read(r#"{"to": "reviewer-1", "text": "\"to\": を説明する"}"#).unwrap();
        assert_eq!(text, r#""to": を説明する"#);
    }
}


#[cfg(test)]
mod repair_tests {
    use super::*;

    /// **実機で捨てられていた報告が、そのまま通る。**
    ///
    /// これは作り物ではなく、Team Run で agent-1 (Planner) が実際に出した
    /// 報告 (`~/.zaivern/term_logs` の生ログから起こしたもの)。台帳には
    /// `報告の JSON を読めません` として 4 回残っていた。**中身は正しく、
    /// シェルのコマンドに `"` が入っていただけ**で、正しく働いた担当が
    /// 「報告していない」ことになって止まっていた。
    const REAL: &str = r#"{
  "task_id": 4,
  "agent_id": "agent-1",
  "status": "completed",
  "summary": "3D方式、ローカル依存、ファイル責務を architecture.md に確定した",
  "changed_files": ["docs/architecture.md"],
  "validation": [
    {
      "command": "test -s docs/architecture.md && test "$(rg -c '^x' docs/architecture.md)" -eq 8",
      "exit_code": 0
    }
  ],
  "blockers": []
}"#;

    #[test]
    fn 実機で捨てられた報告が通る() {
        // 直す前は serde が断っていたことを、まず確かめる
        // (直っていない入力でテストしても何も守れない)。
        assert!(
            serde_json::from_str::<ResultDoc>(&escape_raw_controls(REAL)).is_err(),
            "この入力は素の serde でも通る (再現していない)"
        );
        let d = parse_result(REAL).expect("実機の報告が読めない");
        assert_eq!(d.task_id, 4);
        assert_eq!(d.status, "completed");
        assert_eq!(d.changed_files, vec!["docs/architecture.md".to_string()]);
        // **コマンドは 1 文字も変えない。** 直すのは綴りだけ。
        assert_eq!(
            d.validation[0].command,
            r#"test -s docs/architecture.md && test "$(rg -c '^x' docs/architecture.md)" -eq 8"#
        );
        assert_eq!(d.validation[0].exit_code, 0);
    }

    /// **直すのは 3 通りだけ。それぞれ単独で効く。**
    #[test]
    fn 綴りの直しは三通り() {
        // 1) 本文の中の生の `"`
        let v: serde_json::Value =
            parse_lenient(r#"{"a": "彼は "yes" と言った"}"#).expect("生の引用符");
        assert_eq!(v["a"], r#"彼は "yes" と言った"#);
        // 2) 鍵の間のカンマ抜け
        let v: serde_json::Value = parse_lenient(r#"{"a": "x" "b": "y"}"#).expect("カンマ抜け");
        assert_eq!(v["a"], "x");
        assert_eq!(v["b"], "y");
        // 3) 末尾カンマ (オブジェクトも配列も)
        let v: serde_json::Value =
            parse_lenient(r#"{"a": ["x","y",], "b": 1,}"#).expect("末尾カンマ");
        assert_eq!(v["a"][1], "y");
        assert_eq!(v["b"], 1);
    }

    /// **正しい JSON は 1 文字も変えない。**
    ///
    /// 直し場が正しい入力に手を出すと、いままで通っていた報告の意味が
    /// 黙って変わる。ここが**いちばん壊してはいけない**性質。
    #[test]
    fn 正しいjsonには手を出さない() {
        for good in [
            r#"{"a":"x","b":[1,2],"c":{"d":null}}"#,
            r#"{"a":"エス\"ケープ\\済み","b":"改行\nタブ\t"}"#,
            r#"{"a":"記号 , } ] : を含む","b":"末尾が記号,"}"#,
            r#"[{"a":1},{"a":2}]"#,
            r#"{"empty":"","arr":[],"obj":{}}"#,
        ] {
            let want: serde_json::Value = serde_json::from_str(good).expect("元が正しい");
            let got: serde_json::Value = parse_lenient(good).expect("直し場が壊した");
            assert_eq!(got, want, "正しい JSON を書き換えた: {good}");
            assert_eq!(repair_json(good), good, "文字列そのものを変えた: {good}");
        }
    }

    /// **読めた振りをしない。** 直しても意味が取れないものは断る。
    #[test]
    fn 直しても駄目なものは断る() {
        assert!(parse_lenient::<serde_json::Value>("ただの文章").is_err());
        assert!(parse_lenient::<serde_json::Value>("{壊れて").is_err());
        // **エラーは 1 回目のもの** (直した後の文字列への苦情を返さない)。
        let e = parse_lenient::<ResultDoc>("{}").unwrap_err();
        assert!(e.contains("task_id"), "元の原因が見えない: {e}");
    }

    /// **報告・レビュー・出来事・伝言が同じ直し場を通る。**
    ///
    /// 種類ごとに直し方が違うと「実装は届くのにレビューは落ちる」という
    /// 説明できない差が出る。
    #[test]
    fn 四種類とも同じ直し場を通る() {
        let src = include_str!("result_parser.rs").replace("\r\n", "\n");
        let rev = include_str!("reviewer.rs").replace("\r\n", "\n");
        for (name, body) in [
            ("報告", fn_body(&src, "pub fn parse_result")),
            ("出来事", fn_body(&src, "pub fn parse_event")),
            ("伝言", fn_body(&src, "pub fn read_message")),
            ("レビュー", fn_body(&rev, "pub fn parse_review")),
        ] {
            assert!(
                body.contains("parse_lenient"),
                "{name} が直し場を通っていない"
            );
            assert!(
                !body.contains("serde_json::from_str"),
                "{name} が自前で読んでいる"
            );
        }
    }

    /// 関数 1 本の中身だけを返す (範囲を広げると隣の関数を拾って空回りする)。
    fn fn_body<'a>(src: &'a str, sig: &str) -> &'a str {
        let at = src.find(sig).unwrap_or_else(|| panic!("{sig} が無い"));
        let rest = &src[at..];
        let end = rest.find("\n}\n").map(|i| i + 2).unwrap_or(rest.len());
        &rest[..end]
    }
}
