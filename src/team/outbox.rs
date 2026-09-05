//! **報告置き場 (outbox) の取り決め。** 書く側 (指示文 = `prompt.rs`) と
//! 読む側 (`panel.rs`) が**同じ 1 か所**を使う。ここが 2 か所になると、
//! 指示文が教えた名前を読む側が受け付けない、という食い違いが黙って起きる。
//!
//! ## なぜファイルか
//!
//! 画面から読むのをやめてここから読む。Claude Code v2 は報告を改行ではなく
//! カーソル移動で描くので、画面のグリッドでは行が潰れて**構造的に**
//! 取りこぼす (実測)。置き場を `ZAIVERN_HOME` の下に置くのは、ワークスペースへ
//! 置くと `changeset` が「担当外を変更した」と測って報告ごと却下されるため。
//!
//! ## 取り決め (書きかけを読まないために)
//!
//! 1. エージェントは `<agent-id>-<一意な値>.json.tmp` へ JSON **全体**を書き切る
//! 2. 書き終えてから**同じフォルダの中で** `.tmp` を外した名前へ改名する。
//!    同じファイルシステム上の rename は原子的なので、読む側が「半分だけ
//!    書かれたファイル」を見ることは無い
//! 3. 読む側は `.json` だけを見る ([`list_reports`])。`.json.tmp` も
//!    `rejected/` の中身も見ない
//! 4. **読んで・解析して・配送できたときだけ消す。** 先に消すと、読めなかった
//!    報告は二度と戻らない (以前は読む前に消していた)
//! 5. 読めない (書きかけ・壊れた) ファイルは残して次の tick で読み直す。
//!    [`MAX_ATTEMPTS`] 回で諦めて `rejected/` へ隔離し、理由を Run の記録へ残す
//!    ([`Ledger`] が回数を数える)
//! 6. 担当の照合は**ファイル名と本文の両方**で行う ([`judge`])。ファイル名の
//!    境界は `-` なので `agent-1` は `agent-10-…` に当たらない
//!    ([`stem_matches`])。本文の `agent_id` がファイル名の担当と食い違う
//!    報告は配送しない
//!
//! ## 4 種類とも同じ道を通る
//!
//! 完了報告だけを置き場へ移しても**根本解決にならない**。レビュー・伝言・
//! 出来事も同じ TUI の描画に晒されていて、取りこぼすと同じ形で止まる —
//! とくにレビューを落とすと、実装が終わったタスクが `Reviewing` のまま
//! 永久に動かない。だから 4 種類 ([`Kind`]) とも置き場を正規の経路にする。
//!
//! 種別はエンベロープで名乗る (**これが正しい書き方**):
//!
//! ```json
//! {"kind":"review","run_id":"run-…","agent_id":"reviewer-1","payload":{ … }}
//! ```
//!
//! * `kind` — `result` / `review` / `message` / `event`
//! * `run_id` — 必須。**その Run のものだけ**受ける
//! * `agent_id` — **送り主**。ファイル名の担当と一致すること
//! * `payload` — 中身。画面の囲みに入っていたものと**同じ JSON**
//!
//! 素の JSON (エンベロープ無し) も受ける。完了報告は前の版でそう教えていた
//! ので、受けないと移行の瞬間に報告が全部隔離される。種別は形から決める
//! ([`infer_kind`])。**送り主が本文から分かるのは `result` と `event` だけ**
//! (`review` / `message` の JSON に送り主の欄は無い) なので、素で書かれた
//! その 2 種はファイル名から一意に決まるときだけ受ける。
//!
//! 解析器も状態も増やさない。ここがするのは**種別を決めて中身を取り出す**
//! ことだけで、受理の判断は今までどおり Runtime 1 か所
//! (`take_result` / `take_review` / `take_message` / `take_event`)。
//! 画面から読む経路も残っているが、同じ塊は Runtime の
//! `take_unseen` が塊の指紋で落とすので**二重には入らない**。
//!
//! ## 置き場は Run ごと
//!
//! `<state_dir>/outbox/<run_id>/`。**同じ ID の担当は毎 Run に居る**
//! (`team-lead` など) ので、Run をまたいで 1 つの表に混ぜると後の Run の
//! セッションが前の Run のものを上書きし、報告が別の Run へ流れる。
//! 読む側は Run ごとに独立した表で配送先を引く。
//!
//! Run を閉じるときは置き場ごと消す ([`run_dir`] が「消してよい場所」を
//! 構造で保証する — `run_id` が空・`..`・区切り文字入りなら置き場そのものを
//! 作らない)。

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::Deserialize;

use super::model::AgentId;
use super::result_parser as rp;

/// 置き場の親フォルダ名 (`<state_dir>/outbox/<run_id>/`)。
pub const DIR_NAME: &str = "outbox";

/// 正式な報告ファイルの拡張子。
pub const FINAL_EXT: &str = "json";

/// 書きかけの一時ファイルの末尾。`.json` ではないので読む側は見ない。
pub const TMP_SUFFIX: &str = ".json.tmp";

/// 取り込めなかった報告を移す先 (置き場の中の 1 段下)。
pub const REJECTED_DIR: &str = "rejected";
/// 読み始める前に原本から分離した報告。クラッシュ後も再走査する。
pub const PROCESSING_DIR: &str = "processing";
/// 1 Runから一度に列挙する報告数。異常投入でUI tickを占有させない。
pub const REPORT_LIST_MAX: usize = 256;
/// Retry台帳の上限。超過分は保持せず直ちに隔離判定へ送る。
pub const LEDGER_MAX: usize = 1_024;
/// 1領域で1 tickに確認するdirectory entry数。skip台帳が上限まで埋まっても、
/// その後ろの正常候補を少なくとも`REPORT_LIST_MAX`件選べる大きさにする。
const REPORT_SCAN_MAX: usize = REPORT_LIST_MAX + LEDGER_MAX;

/// 読めないファイルを読み直す回数の上限。
///
/// 走査は [`super::panel::SCAN_INTERVAL`] (400ms) ごとなので、20 回 ≒ 8 秒。
/// 取り決めどおり rename で公開されたファイルは 1 回で読めるので、ここまで
/// 読めないのは「直接 `.json` へ書いている途中」か「壊れている」のどちらか。
/// 上限が無いと壊れた 1 個が永久に残り、毎 tick 同じ失敗を出す。
pub const MAX_ATTEMPTS: u32 = 20;

/// `run_id` の長さの上限 (置き場の名前にするので、ファイル名の制限より内側)。
const RUN_ID_MAX_LEN: usize = 128;

/// 正式な報告ファイルの名前 (`<agent-id>-<一意な値>.json`)。
pub fn final_name(agent_id: &str, unique: &str) -> String {
    format!("{agent_id}-{unique}.{FINAL_EXT}")
}

/// 一時ファイルの名前 (`<agent-id>-<一意な値>.json.tmp`)。
pub fn tmp_name(agent_id: &str, unique: &str) -> String {
    format!("{agent_id}-{unique}{TMP_SUFFIX}")
}

/// **置き場の名前にしてよい `run_id` か。**
///
/// 消す側 ([`run_dir`]) が `<state_dir>/outbox/` の 1 段下しか触らないための
/// 関門。空・`.` 始まり (`.` / `..` / 隠しファイル)・区切り文字や `:`
/// (Windows のドライブ) を含むものは通さない。`new_run_id` が作る
/// `run-<秒>-<pid>-<番号>` は必ず通る。
pub fn valid_run_id(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id.len() <= RUN_ID_MAX_LEN
        && !run_id.starts_with('.')
        && run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// `<base>/<name>` — ただし `name` が 1 段の安全な名前のときだけ。
///
/// 戻りが `Some` なら、その親は必ず `base` で、末尾の 1 段は `name` そのもの
/// (join が別の場所へ跳ぶ余地を、名前の検査と結果の検査の両方で塞ぐ)。
pub fn safe_child(base: &Path, name: &str) -> Option<PathBuf> {
    if !valid_run_id(name) {
        return None;
    }
    let dir = base.join(name);
    let anchored = dir.parent() == Some(base)
        && dir.file_name().and_then(|n| n.to_str()) == Some(name)
        && dir.starts_with(base);
    anchored.then_some(dir)
}

/// この Run の置き場 (`<state_dir>/outbox/<run_id>/`)。
///
/// `run_id` が置き場の名前として安全でなければ `None` — 置き場を**作らない**
/// (画面から読む経路だけになる) し、閉じるときも**消さない**。
pub fn run_dir(state_dir: &Path, run_id: &str) -> Option<PathBuf> {
    safe_child(&state_dir.join(DIR_NAME), run_id)
}

/// このRunのoutboxを、安全な親から1段ずつ作る。
/// `state/outbox` がsymlinkへ差し替えられていれば、外部へ報告を書かせない。
pub fn prepare_run_dir(state_dir: &Path, run_id: &str) -> Result<PathBuf, String> {
    let dir = run_dir(state_dir, run_id)
        .ok_or_else(|| format!("run_id {run_id:?} はoutboxの名前にできません"))?;
    if let Some(parent) = state_dir.parent() {
        super::persistence::ensure_plain_dir_created(parent).map_err(|e| e.detail())?;
    }
    super::persistence::ensure_plain_dir_created(state_dir).map_err(|e| e.detail())?;
    let base = state_dir.join(DIR_NAME);
    super::persistence::ensure_plain_dir_created(&base).map_err(|e| e.detail())?;
    super::persistence::ensure_plain_dir_created(&dir).map_err(|e| e.detail())?;
    Ok(dir)
}

/// ファイル名 (拡張子なし) が担当 `id` のものか。**境界を見る。**
///
/// `id` そのもの、または `id` の直後に `-` が来るものだけ。
/// `agent-1` は `agent-10-x` に当たらない (直後が `0`)。
pub fn stem_matches(stem: &str, id: &str) -> bool {
    !id.is_empty()
        && (stem == id
            || stem
                .strip_prefix(id)
                .is_some_and(|rest| rest.starts_with('-')))
}

/// ファイル名から担当の候補を全部引く (`a` と `a-b` のように、境界を
/// 見ても 2 つ当たることはある。決めるのは [`judge`] で、本文と突き合わせる)。
pub fn candidates(stem: &str, ids: &[AgentId]) -> Vec<AgentId> {
    ids.iter()
        .filter(|id| stem_matches(stem, id.as_str()))
        .cloned()
        .collect()
}

/// 置き場で運ぶ 4 種類。**画面の囲みと 1 対 1** に対応する。
///
/// 対応を 1 か所に閉じておかないと、書く側 (指示文) と読む側 (配送) が
/// 別の綴りを使い、種別が合わないまま黙って隔離される。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// 完了報告 (`[ZAI-TEAM-RESULT]`)。
    Result,
    /// レビュー結果 (`[ZAI-TEAM-REVIEW]`)。
    Review,
    /// エージェント間の伝言 (`[ZAI-TEAM-MSG]`)。
    Message,
    /// サブエージェントの出来事 (`[ZAI-TEAM-EVENT]`)。
    Event,
}

impl Kind {
    /// エンベロープの `kind` に書く綴り。
    pub fn key(self) -> &'static str {
        match self {
            Kind::Result => "result",
            Kind::Review => "review",
            Kind::Message => "message",
            Kind::Event => "event",
        }
    }

    /// 綴りから引く (知らない語は `None` = 隔離)。
    pub fn from_key(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "result" => Some(Kind::Result),
            "review" => Some(Kind::Review),
            "message" | "msg" => Some(Kind::Message),
            "event" => Some(Kind::Event),
            _ => None,
        }
    }

    /// 全種別 (指示文と番人が舐める)。
    pub const ALL: &'static [Kind] = &[Kind::Result, Kind::Review, Kind::Message, Kind::Event];
}

/// 1 つの報告ファイルをどう扱うか。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// この担当の報告として配送してよい。`body` は囲みへ入れる中身。
    Deliver {
        agent: AgentId,
        kind: Kind,
        body: String,
    },
    /// いまは取り込めない (書きかけ・読めない)。残して次の tick で読み直す。
    Retry(String),
    /// 取り込まない。隔離して理由を残す。`agent` は記録の宛先 (分かれば)。
    Reject { agent: Option<AgentId>, why: String },
}

/// 報告ファイルを bounded read した結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    /// 上限内の UTF-8 本文。
    Body(String),
    /// 消失・権限など、一時的かもしれない読み取り失敗。
    Retry(String),
    /// サイズ・種類・文字コードが取り決め違反。再読せず隔離する。
    Reject(String),
}

fn report_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn open_report(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // 一覧取得後に symlink / FIFO へ差し替えられても、リンク先を開かず
        // FIFO の相手も待たない。open 後にも通常ファイルかを検査する。
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: symlink/junction のリンク先を開かない。
        opts.custom_flags(0x0020_0000);
    }
    opts.open(path)
}

fn read_report_after(path: &Path, after_metadata: impl FnOnce()) -> ReadOutcome {
    let name = report_name(path);
    let before = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) => return ReadOutcome::Retry(format!("{name} の metadata を読めません: {e}")),
    };
    if !before.file_type().is_file() {
        return ReadOutcome::Reject(format!(
            "{name} は通常ファイルではないため読みません (上限 {} バイト)",
            rp::BLOCK_MAX_BYTES
        ));
    }
    if before.len() > rp::BLOCK_MAX_BYTES as u64 {
        return ReadOutcome::Reject(format!(
            "{name} は {} バイトで上限 {} バイトを超えています",
            before.len(),
            rp::BLOCK_MAX_BYTES
        ));
    }

    // テストでは metadata の直後に内容を増やし、TOCTOU 時も bounded read
    // で止まることを実ファイルで確かめる。
    after_metadata();

    let file = match open_report(path) {
        Ok(file) => file,
        Err(e) => return ReadOutcome::Retry(format!("{name} を開けません: {e}")),
    };
    let opened = match file.metadata() {
        Ok(meta) => meta,
        Err(e) => return ReadOutcome::Retry(format!("{name} の metadata を読めません: {e}")),
    };
    if !opened.file_type().is_file() {
        return ReadOutcome::Reject(format!(
            "{name} は通常ファイルではないため読みません (上限 {} バイト)",
            rp::BLOCK_MAX_BYTES
        ));
    }
    if opened.len() > rp::BLOCK_MAX_BYTES as u64 {
        return ReadOutcome::Reject(format!(
            "{name} は {} バイトで上限 {} バイトを超えています",
            opened.len(),
            rp::BLOCK_MAX_BYTES
        ));
    }

    let mut bytes = Vec::with_capacity(opened.len().min(rp::BLOCK_MAX_BYTES as u64) as usize);
    let mut bounded = file.take(rp::BLOCK_MAX_BYTES as u64 + 1);
    if let Err(e) = bounded.read_to_end(&mut bytes) {
        return ReadOutcome::Retry(format!("{name} を読めません: {e}"));
    }
    if bytes.len() > rp::BLOCK_MAX_BYTES {
        return ReadOutcome::Reject(format!(
            "{name} は読み込み中に上限 {} バイトを超えました (少なくとも {} バイト)",
            rp::BLOCK_MAX_BYTES,
            bytes.len()
        ));
    }
    match String::from_utf8(bytes) {
        Ok(body) => ReadOutcome::Body(body),
        Err(e) => ReadOutcome::Reject(format!(
            "{name} は UTF-8 ではありません ({} バイト、上限 {} バイト): {e}",
            opened.len(),
            rp::BLOCK_MAX_BYTES
        )),
    }
}

/// metadata で事前検査し、さらに上限 + 1 バイトで止めて報告を読む。
pub fn read_report(path: &Path) -> ReadOutcome {
    read_report_after(path, || {})
}

/// 素の JSON の**形**から種別を決める。エンベロープが無いときだけ使う。
///
/// 見分けは「その種別にしかない欄」で行う。順序が意味を持つ — `event` の
/// JSON も `kind` を持つ (`sub_agent_started` 等) ので、エンベロープの
/// `kind` と取り違えないよう**エンベロープは `payload` の有無で決める**
/// ([`judge`])。
pub fn infer_kind(v: &serde_json::Value) -> Option<Kind> {
    let mut found = Vec::with_capacity(2);
    let has = |k: &str| v.get(k).is_some();
    if has("verdict") {
        found.push(Kind::Review);
    }
    if has("to") && has("text") {
        found.push(Kind::Message);
    }
    // 出来事の `kind` は表にある語だけ (`result` 等と紛れない)。
    if let Some(k) = v.get("kind").and_then(|k| k.as_str()) {
        if rp::EVENT_KINDS.contains(&k.trim()) {
            found.push(Kind::Event);
        }
    }
    if has("task_id") && has("status") {
        found.push(Kind::Result);
    }
    (found.len() == 1).then(|| found[0])
}

struct StrictEnvelope {
    kind: String,
    run_id: String,
    agent_id: String,
    payload: serde_json::Value,
}

impl<'de> Deserialize<'de> for StrictEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct EnvelopeVisitor;
        impl<'de> Visitor<'de> for EnvelopeVisitor {
            type Value = StrictEnvelope;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a Team outbox envelope object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut kind = None;
                let mut run_id = None;
                let mut agent_id = None;
                let mut payload = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "kind" if kind.is_none() => kind = Some(map.next_value::<String>()?),
                        "run_id" if run_id.is_none() => {
                            run_id = Some(map.next_value::<String>()?)
                        }
                        "agent_id" if agent_id.is_none() => {
                            agent_id = Some(map.next_value::<String>()?)
                        }
                        "payload" if payload.is_none() => {
                            payload = Some(map.next_value::<serde_json::Value>()?)
                        }
                        "kind" | "run_id" | "agent_id" | "payload" => {
                            return Err(serde::de::Error::custom(format!(
                                "duplicate top-level field {key:?}"
                            )));
                        }
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(StrictEnvelope {
                    kind: kind.ok_or_else(|| serde::de::Error::missing_field("kind"))?,
                    run_id: run_id.ok_or_else(|| serde::de::Error::missing_field("run_id"))?,
                    agent_id: agent_id
                        .ok_or_else(|| serde::de::Error::missing_field("agent_id"))?,
                    payload: payload.ok_or_else(|| serde::de::Error::missing_field("payload"))?,
                })
            }
        }
        deserializer.deserialize_map(EnvelopeVisitor)
    }
}

/// その種別の素の JSON から**送り主**を読む。
///
/// * `result` — `agent_id` が送り主
/// * `event` — `parent_id` が送り主 (`agent_id` は*報告された子*なので、
///   ここを送り主として照合すると必ず食い違う)
/// * `review` / `message` — 送り主の欄が無い (エンベロープでしか名乗れない)
fn bare_sender(kind: Kind, v: &serde_json::Value) -> Option<String> {
    let field = match kind {
        Kind::Result => "agent_id",
        Kind::Event => "parent_id",
        Kind::Review | Kind::Message => return None,
    };
    v.get(field)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// **ファイル名と本文の両方で担当を決める。** 純関数。
///
/// * JSON として完結していなければ [`Verdict::Retry`] (書きかけは必ずここで
///   止まる — 途中で切れた JSON が正しい JSON になることは無い)
/// * 本文に `agent_id` が無い / ファイル名がどの担当にも当たらない /
///   本文の `agent_id` がファイル名の担当と食い違う → [`Verdict::Reject`]
/// * それ以外 → [`Verdict::Deliver`]
///
/// 読み方は Runtime と同じ [`rp::parse_lenient`] (綴りの手直しまで同じ)。
/// ここで読めたものは Runtime でも読める。
pub fn judge(stem: &str, body: &str, ids: &[AgentId], run_id: &str) -> Verdict {
    if body.len() > rp::BLOCK_MAX_BYTES {
        // 囲みに入れて渡しても `extract_blocks` が黙って落とす大きさ。
        // 配送して消すと「届いたのに何も起きない」になるので、ここで断る。
        return Verdict::Reject {
            agent: None,
            why: format!(
                "報告が大きすぎます ({} バイト。上限 {} バイト)",
                body.len(),
                rp::BLOCK_MAX_BYTES
            ),
        };
    }
    let value: serde_json::Value = match rp::parse_lenient(body) {
        Ok(v) => v,
        Err(e) => return Verdict::Retry(format!("JSON として読めません: {e}")),
    };
    let named = |claimed: &str| ids.iter().find(|id| id.as_str() == claimed).cloned();

    // **エンベロープかどうかは `payload` の有無で決める。**
    // 出来事の JSON も `kind` を持つので、`kind` だけでは見分けられない。
    let (kind, sender, payload) = match value.get("payload") {
        Some(_) => {
            // IdentityをValueから拾わない。重複キー・非文字列・欠落を区別して
            // fail-closedにするため、トップレベルを一度だけ厳密に読む。
            let envelope: StrictEnvelope = match serde_json::from_str(body) {
                Ok(envelope) => envelope,
                Err(e) => {
                    return Verdict::Reject {
                        agent: None,
                        why: format!("エンベロープを厳密に読めません: {e}"),
                    }
                }
            };
            let Some(kind) = Kind::from_key(&envelope.kind) else {
                return Verdict::Reject {
                    agent: None,
                    why: format!(
                        "知らない kind です: {} (使えるのは {})",
                        envelope.kind,
                        Kind::ALL
                            .iter()
                            .map(|x| x.key())
                            .collect::<Vec<_>>()
                            .join(" / ")
                    ),
                };
            };
            let claimed_run = envelope.run_id.trim();
            if claimed_run.is_empty() || claimed_run != run_id {
                return Verdict::Reject {
                    agent: None,
                    why: format!(
                        "別の Run 宛てです (本文 {claimed_run:?} / この置き場 {run_id})"
                    ),
                };
            }
            let sender = (!envelope.agent_id.trim().is_empty())
                .then(|| envelope.agent_id.trim().to_string());
            let Some(sender) = sender else {
                return Verdict::Reject {
                    agent: None,
                    why: "エンベロープに agent_id (送り主) がありません".to_string(),
                };
            };
            // 中身は**そのまま**渡す (画面の囲みに入っていたものと同じ形)。
            let text = match serde_json::to_string(&envelope.payload) {
                Ok(t) => t,
                Err(e) => {
                    return Verdict::Reject {
                        agent: named(&sender),
                        why: format!("payload を JSON にできません: {e}"),
                    }
                }
            };
            (kind, Some(sender), text)
        }
        None => {
            let Some(kind) = infer_kind(&value) else {
                return Verdict::Reject {
                    agent: None,
                    why: "種別を決められません (kind と payload のエンベロープで書いてください)"
                        .to_string(),
                };
            };
            (kind, bare_sender(kind, &value), body.trim().to_string())
        }
    };

    let cands = candidates(stem, ids);
    if cands.is_empty() {
        return Verdict::Reject {
            agent: sender.as_deref().and_then(named),
            why: format!("ファイル名 `{stem}` はこの Run のどの担当にも一致しません"),
        };
    }
    // 送り主が本文から分かるなら、ファイル名と突き合わせる。
    if let Some(claimed) = sender {
        return match cands.iter().find(|c| c.as_str() == claimed) {
            Some(agent) => Verdict::Deliver {
                agent: agent.clone(),
                kind,
                body: payload,
            },
            None => Verdict::Reject {
                agent: named(&claimed),
                why: format!(
                    "ファイル名の担当 ({}) と本文の送り主 ({claimed}) が一致しません",
                    cands
                        .iter()
                        .map(|c| c.as_str())
                        .collect::<Vec<_>>()
                        .join(" / ")
                ),
            },
        };
    }
    // 本文に送り主が無い素の JSON (`review` / `message`)。ファイル名から
    // **一意に決まるときだけ**受ける。2 人に当たるなら決め手が無い。
    match cands.len() {
        1 => Verdict::Deliver {
            agent: cands[0].clone(),
            kind,
            body: payload,
        },
        _ => Verdict::Reject {
            agent: None,
            why: format!(
                "送り主を決められません (ファイル名が {} に当たり、本文に送り主がありません。\
                 kind と agent_id のエンベロープで書いてください)",
                cands
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(" / ")
            ),
        },
    }
}

/// 正式な報告ファイルだけを並べる (`.json`・通常ファイル・名前順)。
///
/// `.json.tmp` (拡張子が `tmp`) と `rejected/` (ディレクトリ) は
/// 構造的に外れる。
#[cfg(test)]
pub fn list_reports(dir: &Path) -> Vec<PathBuf> {
    list_reports_skipping(dir, &HashSet::new())
}

/// この起動で再処理しない報告を、件数上限を適用する前に除いて並べる。
pub fn list_reports_skipping(dir: &Path, skip: &HashSet<PathBuf>) -> Vec<PathBuf> {
    // Run の置き場自体が symlink / junction へ差し替えられても、リンク先の
    // ファイルを報告として読まない。metadata は必ずリンク非追従で見る。
    let Some(parent) = dir.parent() else {
        return Vec::new();
    };
    if !plain_directory(parent) {
        return Vec::new();
    }
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return Vec::new();
    };
    if !meta.file_type().is_dir() {
        return Vec::new();
    }
    let mut files = Vec::new();
    let mut root_budget = REPORT_SCAN_MAX;
    regular_reports_in(dir, skip, &mut root_budget, &mut files);
    let processing = dir.join(PROCESSING_DIR);
    if std::fs::symlink_metadata(&processing).is_ok_and(|m| m.file_type().is_dir()) {
        let mut slots = Vec::new();
        let mut processing_budget = REPORT_SCAN_MAX;
        if let Ok(rd) = std::fs::read_dir(&processing) {
            for slot in rd.filter_map(|e| e.ok()).take(REPORT_SCAN_MAX) {
                let Ok(kind) = slot.file_type() else { continue };
                if kind.is_file() {
                    let path = slot.path();
                    if path.extension().is_some_and(|x| x == FINAL_EXT)
                        && !skip.contains(&path)
                    {
                        files.push(path);
                    }
                } else if kind.is_dir() {
                    slots.push(slot.path());
                }
            }
        }
        slots.sort();
        for slot in slots {
            if processing_budget == 0 {
                break;
            }
            regular_reports_in(
                &slot,
                skip,
                &mut processing_budget,
                &mut files,
            );
        }
    }
    files.sort();
    files.dedup();
    files.truncate(REPORT_LIST_MAX);
    files
}

fn plain_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}

fn regular_reports_in(
    dir: &Path,
    skip: &HashSet<PathBuf>,
    budget: &mut usize,
    files: &mut Vec<PathBuf>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.filter_map(|entry| entry.ok()) {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == FINAL_EXT) && !skip.contains(&path) {
            files.push(path);
        }
    }
}

/// 原本を読み始める前に同じfilesystem内のprocessingへ原子的に移す。
/// 原本の名前へ次の報告が置かれても、後始末で新しい報告を消さない。
pub fn claim_report(file: &Path) -> std::io::Result<PathBuf> {
    let Some(dir) = file.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "報告の親フォルダがありません",
        ));
    };
    if dir.file_name().is_some_and(|n| n == PROCESSING_DIR)
        || dir
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == PROCESSING_DIR)
    {
        return Ok(file.to_path_buf());
    }
    let processing = dir.join(PROCESSING_DIR);
    ensure_plain_dir(&processing)?;
    let name = file.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "報告の名前がありません")
    })?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for n in 0..32u8 {
        let slot = processing.join(format!("{}-{stamp}-{n}", std::process::id()));
        match std::fs::create_dir(&slot) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
        let dest = slot.join(name);
        match std::fs::rename(file, &dest) {
            Ok(()) => return Ok(dest),
            Err(e) => {
                let _ = std::fs::remove_dir(&slot);
                return Err(e);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "processing内に安全な確保名を作れません",
    ))
}

/// 受理済み報告を片付ける唯一の口。テストでは「状態反映後に削除だけ失敗」
/// をOS権限に依存せず再現する。
pub fn remove_report(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if fault_inject::take_remove_failure() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "(テスト) 受理済み報告を削除できません",
        ));
    }
    std::fs::remove_file(path)?;
    remove_empty_claim_slot(path);
    Ok(())
}

#[cfg(test)]
pub mod fault_inject {
    use std::cell::Cell;

    thread_local! {
        static FAIL_REMOVE_ONCE: Cell<bool> = const { Cell::new(false) };
    }

    pub fn fail_remove_once() {
        FAIL_REMOVE_ONCE.with(|flag| flag.set(true));
    }

    pub(super) fn take_remove_failure() -> bool {
        FAIL_REMOVE_ONCE.with(|flag| flag.replace(false))
    }
}

/// 隔離の結果。**取り込めなかった報告を消すことは無い。**
///
/// 却下された報告は「エージェントが何を書いたか」の唯一の証拠なので、消すと
/// 直しようが無くなる (人も、書いた本人も、何が悪かったのか確かめられない)。
/// 移せないときも消さず、**読み直しの対象から外れる名前へ変える**だけにする。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposal {
    /// `rejected/` へ移した。
    Moved(PathBuf),
    /// `rejected/` へ移せなかったので、その場で拡張子を外した
    /// (`…json` → `…json.rejected`)。[`list_reports`] には出ない。
    Renamed(PathBuf),
    /// どちらもできなかった。**ファイルはそのまま残る** — 呼び出し側が
    /// 読み直しの対象から外し、理由を人へ出す。
    Kept(String),
}

fn move_no_replace(file: &Path, preferred: PathBuf) -> std::io::Result<PathBuf> {
    let Some(parent) = preferred.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "隔離先の親がありません",
        ));
    };
    let base = preferred
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "report".to_string());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for n in 0..256u16 {
        let candidate = if n == 0 {
            preferred.clone()
        } else {
            parent.join(format!(
                "{base}.rejected-{}-{stamp}-{n}",
                std::process::id()
            ))
        };
        match std::fs::hard_link(file, &candidate) {
            Ok(()) => match std::fs::remove_file(file) {
                Ok(()) => return Ok(candidate),
                Err(e) => {
                    let _ = std::fs::remove_file(&candidate);
                    return Err(e);
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "隔離先の衝突上限に達しました",
    ))
}

fn ensure_plain_dir(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => return Ok(()),
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "隔離先が通常ディレクトリではありません",
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::create_dir(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "作成した隔離先が通常ディレクトリではありません",
        ))
    }
}

/// 取り込めなかった報告を読み直しの対象から外す。**消さない。**
///
/// 1. `rejected/` へ移す (人が中身を見て直せる場所)
/// 2. 作れなければ、その場で `.rejected` を付けて拡張子を外す
/// 3. どちらも駄目なら、そのまま残して理由を返す
///
/// どの段でも `remove_file` は呼ばない。「移せないから消す」は、いちばん
/// 調べたいものをいちばん失いやすい形になる。
pub fn quarantine(file: &Path) -> Disposal {
    let Some(dir) = report_dir(file) else {
        return Disposal::Kept("親フォルダがありません".to_string());
    };
    let Some(name) = file.file_name() else {
        return Disposal::Kept("ファイル名がありません".to_string());
    };
    let pen = dir.join(REJECTED_DIR);
    let moved = ensure_plain_dir(&pen).and_then(|()| move_no_replace(file, pen.join(name)));
    match moved {
        Ok(dest) => {
            remove_empty_claim_slot(file);
            Disposal::Moved(dest)
        }
        Err(e) => {
            // 隔離先へ移せない (権限・別ボリューム等)。**消さずに**、
            // Run直下で読み直しの対象から外す。
            let aside = dir.join(format!("{}.{}", name.to_string_lossy(), REJECTED_DIR));
            match move_no_replace(file, aside) {
                Ok(aside) => {
                    remove_empty_claim_slot(file);
                    Disposal::Renamed(aside)
                }
                Err(e2) => Disposal::Kept(format!(
                    "{REJECTED_DIR}/ へ移せず ({e})、名前も変えられません ({e2})"
                )),
            }
        }
    }
}

fn report_dir(file: &Path) -> Option<&Path> {
    let parent = file.parent()?;
    if parent.file_name().is_some_and(|n| n == PROCESSING_DIR) {
        return parent.parent();
    }
    if parent
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == PROCESSING_DIR)
    {
        return parent.parent()?.parent();
    }
    Some(parent)
}

fn remove_empty_claim_slot(file: &Path) {
    let Some(parent) = file.parent() else { return };
    if parent
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == PROCESSING_DIR)
    {
        let _ = std::fs::remove_dir(parent);
    }
}

/// 1 ファイルの覚え書き。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Entry {
    /// 読み直した回数。
    attempts: u32,
    /// 理由を記録へ出したか (同じファイルで毎 tick 出さない)。
    announced: bool,
}

/// **読めなかった報告の台帳** (保存しない — 再起動したら数え直せばよい)。
///
/// 置き場ごとに持たず、パスで引く。閉じた Run のぶんは [`Ledger::prune_missing`]
/// が「もう無いファイル」として落とす。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    entries: HashMap<PathBuf, Entry>,
}

impl Ledger {
    /// 読み直しを 1 回数え、通算を返す。
    pub fn bump(&mut self, file: &Path) -> u32 {
        if !self.entries.contains_key(file) && self.entries.len() >= LEDGER_MAX {
            return MAX_ATTEMPTS;
        }
        let e = self.entries.entry(file.to_path_buf()).or_default();
        e.attempts = e.attempts.saturating_add(1);
        e.attempts
    }

    /// このファイルについて**初めて**理由を出すときだけ `true`。
    pub fn announce_once(&mut self, file: &Path) -> bool {
        if !self.entries.contains_key(file) && self.entries.len() >= LEDGER_MAX {
            return true;
        }
        let e = self.entries.entry(file.to_path_buf()).or_default();
        let first = !e.announced;
        e.announced = true;
        first
    }

    /// 片付いたファイルを忘れる。
    pub fn forget(&mut self, file: &Path) {
        self.entries.remove(file);
    }

    /// もう無いファイルの覚え書きを落とす (消えた・隔離した・Run を閉じた)。
    pub fn prune_missing(&mut self) {
        self.entries.retain(|p, _| p.exists());
    }

    /// 覚えているファイル (テストが「片付いたら忘れる」を見る)。
    #[cfg(test)]
    pub fn tracked(&self) -> std::collections::HashSet<PathBuf> {
        self.entries.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<AgentId> {
        list.iter().map(|s| AgentId::new(*s)).collect()
    }

    /// この置き場の Run。
    const RUN: &str = "run-1712345678-1-0";

    fn deliver(agent: &str, kind: Kind, body: &str) -> Verdict {
        Verdict::Deliver {
            agent: AgentId::new(agent),
            kind,
            body: body.trim().to_string(),
        }
    }

    fn envelope(kind: &str, run: &str, agent: &str, payload: &str) -> String {
        format!(r#"{{"kind":"{kind}","run_id":"{run}","agent_id":"{agent}","payload":{payload}}}"#)
    }

    fn report(agent: &str) -> String {
        format!(
            "{{\"task_id\": 1, \"agent_id\": \"{agent}\", \"status\": \"completed\", \
             \"summary\": \"x\", \"changed_files\": [], \"validation\": [], \"blockers\": []}}"
        )
    }

    /// **`agent-1` / `agent-10` / `agent-100` を同時に置いても取り違えない。**
    ///
    /// 以前は `stem.starts_with(id)` だったので `agent-10-report` が `agent-1`
    /// にも当たり、しかも HashMap の走査順で結果が変わっていた。
    #[test]
    fn 担当idの前方一致は境界で切る() {
        let all = ids(&["agent-1", "agent-10", "agent-100"]);
        let table: &[(&str, &[&str])] = &[
            ("agent-1", &["agent-1"]),
            ("agent-10", &["agent-10"]),
            ("agent-100", &["agent-100"]),
            ("agent-1-report", &["agent-1"]),
            ("agent-10-report", &["agent-10"]),
            ("agent-100-1712345678", &["agent-100"]),
            // 境界が `-` でないものは誰にも当たらない
            ("agent-1x", &[]),
            ("agent-1_report", &[]),
            ("agent-", &[]),
            ("agent", &[]),
            ("", &[]),
        ];
        for (stem, want) in table {
            let got: Vec<String> = candidates(stem, &all).into_iter().map(|a| a.0).collect();
            let want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
            assert_eq!(got, want, "stem={stem:?}");
        }
        // 走査順に依らない: 逆順で渡しても同じ 1 つに決まる
        let rev: Vec<AgentId> = all.iter().rev().cloned().collect();
        for stem in ["agent-1-r", "agent-10-r", "agent-100-r"] {
            assert_eq!(candidates(stem, &all), candidates(stem, &rev), "stem={stem}");
        }
    }

    /// **本文の送り主とファイル名の担当を突き合わせる。**
    #[test]
    fn 本文とファイル名の担当が食い違う報告は配送しない() {
        let all = ids(&["agent-1", "agent-10", "agent-100"]);
        // 一致 → 配送
        assert_eq!(
            judge("agent-10-abc", &report("agent-10"), &all, RUN),
            deliver("agent-10", Kind::Result, &report("agent-10"))
        );
        assert_eq!(
            judge("agent-1", &report("agent-1"), &all, RUN),
            deliver("agent-1", Kind::Result, &report("agent-1"))
        );
        // ファイル名は agent-10、本文は agent-1 → 却下 (どちらへも配らない)
        match judge("agent-10-abc", &report("agent-1"), &all, RUN) {
            Verdict::Reject { agent, why } => {
                assert_eq!(agent, Some(AgentId::new("agent-1")), "記録の宛先");
                assert!(why.contains("agent-10") && why.contains("agent-1"), "{why}");
            }
            other => panic!("配送してしまった: {other:?}"),
        }
        // ファイル名が誰にも当たらない → 却下
        assert!(matches!(
            judge("stranger-1", &report("agent-1"), &all, RUN),
            Verdict::Reject { .. }
        ));
        // 種別を決められない JSON → 却下
        assert!(matches!(
            judge("agent-1-x", "{\"hello\": 1}", &all, RUN),
            Verdict::Reject { .. }
        ));
    }

    /// **4 種類とも、エンベロープで種別と送り主が決まる。**
    ///
    /// 完了報告だけを置き場へ移しても根本解決にならない (レビューを落とすと
    /// タスクが `Reviewing` のまま止まる)。ここが 4 種類とも通ることを固定する。
    #[test]
    fn エンベロープは四種類とも種別と送り主で配送先が決まる() {
        let all = ids(&["impl-1", "reviewer-1"]);
        let table: &[(Kind, &str)] = &[
            (Kind::Result, r#"{"task_id":1,"agent_id":"impl-1","status":"completed"}"#),
            (Kind::Review, r#"{"task_id":1,"verdict":"APPROVE","findings":[]}"#),
            (Kind::Message, r#"{"to":"impl-1","text":"できました"}"#),
            (
                Kind::Event,
                r#"{"kind":"sub_agent_started","agent_id":"child-1","parent_id":"impl-1"}"#,
            ),
        ];
        for (kind, payload) in table {
            let env = envelope(kind.key(), RUN, "impl-1", payload);
            match judge("impl-1-9", &env, &all, RUN) {
                Verdict::Deliver { agent, kind: k, body } => {
                    assert_eq!(agent, AgentId::new("impl-1"), "{kind:?}");
                    assert_eq!(k, *kind, "{kind:?} の種別を取り違えた");
                    // 中身は囲みへ入れる JSON そのもの (包みは剥がす)。
                    let got: serde_json::Value = serde_json::from_str(&body).expect("payload");
                    let want: serde_json::Value = serde_json::from_str(payload).unwrap();
                    assert_eq!(got, want, "{kind:?} の中身が変わった");
                }
                other => panic!("{kind:?} を配送しなかった: {other:?}"),
            }
        }
        // 綴りの揺れ (大小・msg) は受ける。知らない綴りは隔離。
        for k in ["Result", "REVIEW", "msg", "event"] {
            let env = envelope(k, RUN, "impl-1", r#"{"task_id":1,"verdict":"APPROVE"}"#);
            assert!(
                matches!(judge("impl-1-9", &env, &all, RUN), Verdict::Deliver { .. }),
                "{k} を受けなかった"
            );
        }
        let env = envelope("hack", RUN, "impl-1", "{}");
        assert!(matches!(
            judge("impl-1-9", &env, &all, RUN),
            Verdict::Reject { .. }
        ));
        // payload はあるが kind が無い → 隔離
        assert!(matches!(
            judge("impl-1-9", r#"{"agent_id":"impl-1","payload":{}}"#, &all, RUN),
            Verdict::Reject { .. }
        ));
        // 送り主が違う → 配送しない
        let env = envelope("review", RUN, "reviewer-1", r#"{"task_id":1,"verdict":"APPROVE"}"#);
        assert!(matches!(
            judge("impl-1-9", &env, &all, RUN),
            Verdict::Reject { .. }
        ));
    }

    /// **別 Run 宛てとrun_id欠落のエンベロープを受け取らない。**
    #[test]
    fn 別のrun宛ての報告は配送しない() {
        let all = ids(&["impl-1"]);
        let payload = r#"{"task_id":1,"verdict":"APPROVE","findings":[]}"#;
        let env = envelope("review", "run-OTHER", "impl-1", payload);
        match judge("impl-1-9", &env, &all, RUN) {
            Verdict::Reject { why, .. } => assert!(why.contains("run-OTHER"), "{why}"),
            other => panic!("別 Run 宛てを配送した: {other:?}"),
        }
        // エンベロープではrun_idが必須。素のJSONだけが旧形式互換。
        let env = format!(
            r#"{{"kind":"review","agent_id":"impl-1","payload":{payload}}}"#
        );
        assert!(matches!(
            judge("impl-1-9", &env, &all, RUN),
            Verdict::Reject { .. }
        ));
    }

    #[test]
    fn エンベロープの重複identityと曖昧な素jsonは配送しない() {
        let all = ids(&["impl-1"]);
        let duplicate = format!(
            r#"{{"kind":"message","run_id":"{RUN}","run_id":"other","agent_id":"impl-1","payload":{{"to":"all","text":"x"}}}}"#
        );
        assert!(matches!(
            judge("impl-1-x", &duplicate, &all, RUN),
            Verdict::Reject { .. }
        ));
        for body in [
            format!(
                r#"{{"kind":"message","run_id":"{RUN}","agent_id":"impl-1","agent_id":"other","payload":{{"to":"all","text":"x"}}}}"#
            ),
            r#"{"kind":"message","run_id":1,"agent_id":"impl-1","payload":{"to":"all","text":"x"}}"#
                .to_string(),
        ] {
            assert!(matches!(
                judge("impl-1-x", &body, &all, RUN),
                Verdict::Reject { .. }
            ));
        }
        let ambiguous =
            r#"{"task_id":1,"status":"completed","verdict":"APPROVE","agent_id":"impl-1"}"#;
        assert!(matches!(
            judge("impl-1-x", ambiguous, &all, RUN),
            Verdict::Reject { .. }
        ));
    }

    #[test]
    fn claim後に同名報告が来ても後始末で新しい報告を消さない() {
        let dir = crate::test_util::unique_temp_dir("zai-team-outbox", "claim-replace");
        std::fs::create_dir_all(&dir).unwrap();
        let original = dir.join("impl-1-same.json");
        std::fs::write(&original, "first").unwrap();
        let claimed = claim_report(&original).expect("原本を確保できる");
        assert!(!original.exists());
        assert_eq!(std::fs::read_to_string(&claimed).unwrap(), "first");

        std::fs::write(&original, "second").unwrap();
        remove_report(&claimed).unwrap();
        assert_eq!(
            std::fs::read_to_string(&original).unwrap(),
            "second",
            "確保後に届いた別報告を削除した"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **素の JSON も受ける** (前の版の完了報告がそう書かれている)。
    ///
    /// 送り主が本文から分かるのは `result` (`agent_id`) と `event`
    /// (`parent_id`) だけ。出来事の `agent_id` は*報告された子*なので、
    /// そこを送り主として照合すると必ず食い違う。
    #[test]
    fn 素のjsonは形から種別を決め送り主を正しく読む() {
        let all = ids(&["impl-1", "child-1"]);
        // result — agent_id が送り主
        assert_eq!(
            judge("impl-1-1", &report("impl-1"), &all, RUN),
            deliver("impl-1", Kind::Result, &report("impl-1"))
        );
        // event — parent_id が送り主 (agent_id は子)
        let ev = r#"{"kind":"sub_agent_started","agent_id":"child-1","parent_id":"impl-1"}"#;
        assert_eq!(
            judge("impl-1-2", ev, &all, RUN),
            deliver("impl-1", Kind::Event, ev)
        );
        // 子の名前をファイル名にしたら食い違う (送り主は親)
        assert!(matches!(
            judge("child-1-2", ev, &all, RUN),
            Verdict::Reject { .. }
        ));
        // review / message — 送り主の欄が無いのでファイル名で決める
        let rv = r#"{"task_id":1,"verdict":"APPROVE","findings":[]}"#;
        assert_eq!(
            judge("impl-1-3", rv, &all, RUN),
            deliver("impl-1", Kind::Review, rv)
        );
        let ms = r#"{"to":"child-1","text":"やあ"}"#;
        assert_eq!(
            judge("impl-1-4", ms, &all, RUN),
            deliver("impl-1", Kind::Message, ms)
        );
        // 種別の見分けは表の語だけ。知らない kind の素 JSON は隔離。
        assert!(matches!(
            judge("impl-1-5", r#"{"kind":"hack_the_planet","parent_id":"impl-1"}"#, &all, RUN),
            Verdict::Reject { .. }
        ));
    }

    /// **`a` と `a-b` のように 2 つ当たるときは、本文の送り主で決める。**
    /// 送り主が無い種別 (`review` / `message`) は決め手が無いので隔離する。
    #[test]
    fn 候補が二つあるときは本文の送り主で決める() {
        let all = ids(&["a", "a-b"]);
        assert_eq!(candidates("a-b-x", &all).len(), 2, "前提: 2 つ当たる");
        assert_eq!(
            judge("a-b-x", &report("a-b"), &all, RUN),
            deliver("a-b", Kind::Result, &report("a-b"))
        );
        assert_eq!(
            judge("a-b-x", &report("a"), &all, RUN),
            deliver("a", Kind::Result, &report("a"))
        );
        assert!(matches!(
            judge("a-b-x", &report("c"), &all, RUN),
            Verdict::Reject { .. }
        ));
        // 送り主の欄が無い素の review は、候補が 2 つなら決められない
        let rv = r#"{"task_id":1,"verdict":"APPROVE","findings":[]}"#;
        match judge("a-b-x", rv, &all, RUN) {
            Verdict::Reject { why, .. } => assert!(why.contains("送り主"), "{why}"),
            other => panic!("当てずっぽうで配送した: {other:?}"),
        }
        // エンベロープなら名乗れるので決まる (中身は同じ JSON)
        let env = envelope("review", RUN, "a-b", rv);
        match judge("a-b-x", &env, &all, RUN) {
            Verdict::Deliver { agent, kind, body } => {
                assert_eq!(agent, AgentId::new("a-b"));
                assert_eq!(kind, Kind::Review);
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&body).unwrap(),
                    serde_json::from_str::<serde_json::Value>(rv).unwrap()
                );
            }
            other => panic!("名乗ったのに配送しない: {other:?}"),
        }
    }

    /// **書きかけは Retry、大きすぎは Reject。** 途中で切れた JSON が
    /// 正しい JSON になることは無いので、書きかけは必ずここで止まる。
    #[test]
    fn 書きかけのjsonは読み直しに回す() {
        let all = ids(&["agent-1"]);
        let full = report("agent-1");
        for cut in [0, 1, 10, full.len() / 2, full.len() - 1] {
            let partial = &full[..cut];
            assert!(
                matches!(judge("agent-1-x", partial, &all, RUN), Verdict::Retry(_)),
                "cut={cut} で Retry にならない: {partial:?}"
            );
        }
        assert!(matches!(
            judge("agent-1-x", &full, &all, RUN),
            Verdict::Deliver { .. }
        ));
        let huge = format!(
            "{{\"agent_id\":\"agent-1\",\"summary\":\"{}\"}}",
            "x".repeat(rp::BLOCK_MAX_BYTES)
        );
        assert!(matches!(
            judge("agent-1-x", &huge, &all, RUN),
            Verdict::Reject { .. }
        ));
    }

    /// **消してよい場所は `<state_dir>/outbox/<run_id>` の 1 段下だけ。**
    #[test]
    fn 置き場の名前にできないrun_idは断る() {
        let root = std::env::temp_dir().join("zv-outbox-safety");
        let base = root.join(DIR_NAME);
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "a/b",
            "a\\b",
            "../x",
            "/abs",
            "C:x",
            "run 1",
            "run\u{0}1",
            "日本語",
        ] {
            assert!(!valid_run_id(bad), "{bad:?} を通した");
            assert_eq!(run_dir(&root, bad), None, "{bad:?} で置き場を作った");
        }
        let long = "r".repeat(RUN_ID_MAX_LEN + 1);
        assert_eq!(run_dir(&root, &long), None, "長すぎる run_id を通した");
        let longest = "x".repeat(RUN_ID_MAX_LEN);
        for good in ["run-1756000000-123-0", "abc", "a.b-c_d", longest.as_str()] {
            let dir = run_dir(&root, good).unwrap_or_else(|| panic!("{good:?} を断った"));
            assert_eq!(dir.parent(), Some(base.as_path()), "{good:?} の親が置き場でない");
            assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some(good));
        }
        // `new_run_id` が作るものは必ず通る
        assert!(valid_run_id(&super::super::runtime::new_run_id()));
    }

    /// **`.json` だけを読む。** `.json.tmp` と `rejected/` の中は見ない。
    #[test]
    fn 一時ファイルと隔離先は一覧に出ない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "list");
        std::fs::write(dir.join(tmp_name("agent-1", "1")), "{").unwrap();
        std::fs::write(dir.join(final_name("agent-1", "2")), "{}").unwrap();
        std::fs::write(dir.join("agent-1.json"), "{}").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::fs::create_dir_all(dir.join(REJECTED_DIR)).unwrap();
        std::fs::write(dir.join(REJECTED_DIR).join(final_name("agent-1", "3")), "{}").unwrap();
        // 拡張子だけ `.json` のディレクトリも報告ではない
        std::fs::create_dir_all(dir.join("dir.json")).unwrap();
        let names: Vec<String> = list_reports(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["agent-1-2.json".to_string(), "agent-1.json".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn skip上限回帰(list_root: &Path, report_dir: &Path) {
        let mut all = Vec::new();
        for i in 0..=REPORT_LIST_MAX {
            let path = report_dir.join(final_name("agent-1", &format!("{i:04}")));
            std::fs::write(&path, "{}").unwrap();
            all.push(path);
        }
        let first = list_reports(list_root);
        assert_eq!(first.len(), REPORT_LIST_MAX, "前提: 列挙上限まで選ばれる");
        let skip: HashSet<PathBuf> = first.into_iter().collect();
        let omitted = all
            .into_iter()
            .find(|path| !skip.contains(path))
            .expect("上限の後ろに1件ある");
        let got = list_reports_skipping(list_root, &skip);
        assert_eq!(got, vec![omitted], "skipの後ろの正常報告が飢餓した");
    }

    #[test]
    fn root直下は列挙上限より前にskipを除く() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "skip-before-limit-root");
        skip上限回帰(&dir, &dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn processing配下も列挙上限より前にskipを除く() {
        let dir = crate::test_util::unique_temp_dir(
            "zaivern-outbox",
            "skip-before-limit-processing",
        );
        let slot = dir.join(PROCESSING_DIR).join("slot");
        std::fs::create_dir_all(&slot).unwrap();
        skip上限回帰(&dir, &slot);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **symlink は報告として読まない。**
    ///
    /// `.json` の名前を付けた symlink を置かれても、読めば置き場の外を読み、
    /// 消せば置き場の外を消すことになる。`DirEntry::file_type` は symlink を
    /// 辿らないので、通常ファイルだけを通す形で構造的に外れる。
    #[cfg(unix)]
    #[test]
    fn symlinkは報告として読まない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "symlink");
        let outside = dir.join("secret.txt");
        std::fs::write(&outside, "外のファイル").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(final_name("agent-1", "1"))).unwrap();
        std::fs::write(dir.join(final_name("agent-1", "2")), "{}").unwrap();
        let names: Vec<String> = list_reports(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["agent-1-2.json".to_string()],
            "symlink を報告として読んだ"
        );
        assert!(outside.exists(), "symlink の先を消した");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 隔離は同じ置き場の 1 段下へ。移した後は一覧から消える。
    #[test]
    fn 隔離したファイルは一覧から消える() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "quarantine");
        let f = dir.join(final_name("agent-1", "9"));
        std::fs::write(&f, "{broken").unwrap();
        assert_eq!(list_reports(&dir).len(), 1);
        let dest = match quarantine(&f) {
            Disposal::Moved(d) => d,
            other => panic!("移せる場所なのに移さなかった: {other:?}"),
        };
        assert_eq!(dest.parent(), Some(dir.join(REJECTED_DIR).as_path()));
        assert!(dest.exists() && !f.exists());
        assert!(list_reports(&dir).is_empty(), "隔離したものがまだ一覧に出る");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 台帳は回数を数え消えたファイルを忘れる() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "ledger");
        let f = dir.join("agent-1-1.json");
        std::fs::write(&f, "{").unwrap();
        let mut l = Ledger::default();
        assert_eq!(l.bump(&f), 1);
        assert_eq!(l.bump(&f), 2);
        assert!(l.announce_once(&f), "初回は出す");
        assert!(!l.announce_once(&f), "2 回目は出さない");
        l.prune_missing();
        assert_eq!(l.tracked().len(), 1, "まだ有るファイルを忘れた");
        std::fs::remove_file(&f).unwrap();
        l.prune_missing();
        assert!(l.tracked().is_empty(), "消えたファイルを覚えたまま");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 指示文が教える名前を、読む側がそのまま受け付ける (取り決めが 1 か所)。
    #[test]
    fn 指示文の名前は読む側の照合を通る() {
        let all = ids(&["impl-1", "impl-10"]);
        let fin = final_name("impl-1", "1712345678");
        let stem = Path::new(&fin)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap();
        assert_eq!(candidates(stem, &all), ids(&["impl-1"]));
        let tmp = tmp_name("impl-1", "1712345678");
        assert!(tmp.ends_with(TMP_SUFFIX));
        assert_eq!(
            Path::new(&tmp).extension().and_then(|x| x.to_str()),
            Some("tmp"),
            "一時ファイルの拡張子が json になっている (読まれてしまう)"
        );
        assert_eq!(&tmp[..tmp.len() - 4], &fin, "`.tmp` を外すと正式な名前になる");
    }

    #[test]
    fn 上限ちょうどは読み上限プラス一は本文を渡さず隔離判定になる() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "bounded-size");
        let exact = dir.join(final_name("agent-1", "exact"));
        let over = dir.join(final_name("agent-1", "over"));
        std::fs::write(&exact, vec![b'a'; rp::BLOCK_MAX_BYTES]).unwrap();
        std::fs::write(&over, vec![b'b'; rp::BLOCK_MAX_BYTES + 1]).unwrap();

        match read_report(&exact) {
            ReadOutcome::Body(body) => assert_eq!(body.len(), rp::BLOCK_MAX_BYTES),
            other => panic!("上限ちょうどを読まなかった: {other:?}"),
        }
        match read_report(&over) {
            ReadOutcome::Reject(why) => {
                assert!(why.contains("agent-1-over.json"), "ファイル名が無い: {why}");
                assert!(why.contains(&(rp::BLOCK_MAX_BYTES + 1).to_string()), "実サイズが無い: {why}");
                assert!(why.contains(&rp::BLOCK_MAX_BYTES.to_string()), "上限が無い: {why}");
            }
            other => panic!("上限超過の本文を渡した: {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn metadata確認後に増えても上限プラス一で読み止める() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "bounded-growth");
        let file = dir.join(final_name("agent-1", "grow"));
        std::fs::write(&file, b"{}").unwrap();
        let grow = file.clone();
        let got = read_report_after(&file, move || {
            std::fs::write(&grow, vec![b'x'; rp::BLOCK_MAX_BYTES + 100]).unwrap();
        });
        match got {
            ReadOutcome::Reject(why) => {
                assert!(why.contains("agent-1-grow.json"), "{why}");
                assert!(why.contains("上限"), "{why}");
            }
            other => panic!("差し替え後の巨大本文を渡した: {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 非utf8は再試行せず隔離判定になる() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "non-utf8");
        let file = dir.join(final_name("agent-1", "bad"));
        std::fs::write(&file, [0xff, 0xfe, 0xfd]).unwrap();
        assert!(matches!(read_report(&file), ReadOutcome::Reject(why) if why.contains("UTF-8")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn 置き場自体がsymlinkならリンク先を走査しない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "dir-symlink");
        let outside = crate::test_util::unique_temp_dir("zaivern-outbox", "outside-dir");
        let report = outside.join(final_name("agent-1", "outside"));
        std::fs::write(&report, "外の証拠").unwrap();
        let linked = dir.join("run-id");
        std::os::unix::fs::symlink(&outside, &linked).unwrap();

        assert!(list_reports(&linked).is_empty(), "symlink先の報告を列挙した");
        assert_eq!(std::fs::read_to_string(&report).unwrap(), "外の証拠");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[cfg(unix)]
    #[test]
    fn 置き場の親がsymlinkでもリンク先を走査しない() {
        let state = crate::test_util::unique_temp_dir("zaivern-outbox", "parent-symlink");
        let outside = crate::test_util::unique_temp_dir("zaivern-outbox", "outside-parent");
        let run = outside.join("run-id");
        std::fs::create_dir_all(&run).unwrap();
        let report = run.join(final_name("agent-1", "outside"));
        std::fs::write(&report, "外の証拠").unwrap();
        std::os::unix::fs::symlink(&outside, state.join(DIR_NAME)).unwrap();

        let linked_run = state.join(DIR_NAME).join("run-id");
        assert!(
            prepare_run_dir(&state, "run-id").is_err(),
            "symlinkの親へ正式な投函先を作った"
        );
        assert!(
            list_reports(&linked_run).is_empty(),
            "symlinkの親を経由して報告を列挙した"
        );
        assert_eq!(std::fs::read_to_string(&report).unwrap(), "外の証拠");
        std::fs::remove_dir_all(&state).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[cfg(unix)]
    #[test]
    fn readerもsymlinkと特殊ファイルを読まない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "special");
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        let link = dir.join(final_name("agent-1", "link"));
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(matches!(read_report(&link), ReadOutcome::Reject(_)));

        use std::os::unix::ffi::OsStrExt;
        let fifo = dir.join(final_name("agent-1", "fifo"));
        let raw = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `raw` は NUL 終端済みで呼び出し中も生存し、mode は通常の
        // POSIX permission bits。作成先はこのテストだけの一時ディレクトリ。
        let rc = unsafe { libc::mkfifo(raw.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
        assert!(matches!(read_report(&fifo), ReadOutcome::Reject(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn metadata確認後にsymlinkへ差し替えられてもリンク先を読まない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "symlink-swap");
        let file = dir.join(final_name("agent-1", "swap"));
        let outside = dir.join("outside.txt");
        std::fs::write(&file, "{}").unwrap();
        std::fs::write(&outside, "リンク先の秘密").unwrap();
        let swap = file.clone();
        let got = read_report_after(&file, move || {
            std::fs::remove_file(&swap).unwrap();
            std::os::unix::fs::symlink(&outside, &swap).unwrap();
        });
        assert!(
            !matches!(got, ReadOutcome::Body(_)),
            "metadata後に差し替えたsymlinkのリンク先を読んだ"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 隔離先に同名があっても既存の証拠を上書きしない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "quarantine-collision");
        let rejected = dir.join(REJECTED_DIR);
        std::fs::create_dir_all(&rejected).unwrap();
        let file = dir.join(final_name("agent-1", "same"));
        let occupied = rejected.join(file.file_name().unwrap());
        std::fs::write(&occupied, "先の証拠").unwrap();
        std::fs::write(&file, "後の証拠").unwrap();

        let moved = match quarantine(&file) {
            Disposal::Moved(path) => path,
            other => panic!("隔離できなかった: {other:?}"),
        };
        assert_ne!(moved, occupied, "同名の証拠を上書きした");
        assert_eq!(std::fs::read_to_string(&occupied).unwrap(), "先の証拠");
        assert_eq!(std::fs::read_to_string(&moved).unwrap(), "後の証拠");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 隔離フォールバックも同名の証拠を上書きしない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "aside-collision");
        // ディレクトリ作成を失敗させ、同じ場所での拡張子変更へ進ませる。
        std::fs::write(dir.join(REJECTED_DIR), "not a directory").unwrap();
        let file = dir.join(final_name("agent-1", "same"));
        let occupied = file.with_extension(format!("{FINAL_EXT}.{REJECTED_DIR}"));
        std::fs::write(&occupied, "先の証拠").unwrap();
        std::fs::write(&file, "後の証拠").unwrap();

        let renamed = match quarantine(&file) {
            Disposal::Renamed(path) => path,
            other => panic!("その場で退避できなかった: {other:?}"),
        };
        assert_ne!(renamed, occupied, "フォールバック先の証拠を上書きした");
        assert_eq!(std::fs::read_to_string(&occupied).unwrap(), "先の証拠");
        assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "後の証拠");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn 隔離先のsymlinkを辿って外へ証拠を動かさない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "rejected-symlink");
        let outside = crate::test_util::unique_temp_dir("zaivern-outbox", "outside");
        std::os::unix::fs::symlink(&outside, dir.join(REJECTED_DIR)).unwrap();
        let file = dir.join(final_name("agent-1", "same"));
        std::fs::write(&file, "証拠").unwrap();

        let renamed = match quarantine(&file) {
            Disposal::Renamed(path) => path,
            other => panic!("外を指す隔離先から同じ場所へ退避しなかった: {other:?}"),
        };
        assert!(renamed.starts_with(&dir));
        assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "証拠");
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[cfg(unix)]
    #[test]
    fn 隔離先のdangling_symlinkも既存の証拠として上書きしない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "dangling-collision");
        let rejected = dir.join(REJECTED_DIR);
        std::fs::create_dir_all(&rejected).unwrap();
        let file = dir.join(final_name("agent-1", "same"));
        let occupied = rejected.join(file.file_name().unwrap());
        std::os::unix::fs::symlink(dir.join("missing"), &occupied).unwrap();
        std::fs::write(&file, "後の証拠").unwrap();

        let moved = match quarantine(&file) {
            Disposal::Moved(path) => path,
            other => panic!("隔離できなかった: {other:?}"),
        };
        assert_ne!(moved, occupied, "dangling symlink を上書きした");
        assert!(std::fs::symlink_metadata(&occupied).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&moved).unwrap(), "後の証拠");
        std::fs::remove_dir_all(&dir).ok();
    }
}
