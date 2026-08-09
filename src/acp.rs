//! ACP (Agent Client Protocol) クライアント — **構造化プロトコル**でエージェントを駆動する。
//!
//! CLAUDE.md の設計原則 4 は「エージェントの状態を画面 (ピクセル) から推測しない。
//! 構造化プロトコル > ベンダー提供フック > 状態ファイル > 画面スクレイプ」と定める。
//! 既存の PTY 経路 (`terminal.rs` + `agents.rs`) は最下段の**画面スクレイプ**で、
//! ベンダーが CLI の出力を変えるたびに壊れる。このモジュールは**最上段**を実装する。
//!
//! # ワイヤ仕様 (実測で確定済み)
//!
//! - JSON-RPC 2.0 / UTF-8 / **改行 (`\n`) 区切り**。1 メッセージに改行を含めない。
//! - クライアントがエージェントを**子プロセス**として起動し、stdin へ書き stdout を読む。
//!   stderr はログ (ACP ではない)。**専用スレッドで吸い続ける** — 溜めるとパイプ
//!   バッファが埋まってエージェントが書き込みでブロックする。
//! - **双方向**。エージェントは同じパイプでクライアントへ**リクエストを投げ返す**
//!   (`session/request_permission` / `fs/read_text_file` / `fs/write_text_file`)。
//!   だからリーダースレッドは「リクエスト / レスポンス / 通知」の 3 系統へ振り分ける
//!   ([`classify`])。
//! - JSON のキーは camelCase、判別子の値は snake_case、パスは絶対、行番号は 1 始まり。
//! - `protocolVersion` は**ただの整数**。実在するエージェントは全部 `1`
//!   ([`PROTOCOL_VERSION`])。`2` はドラフトなので折衝で降りる。
//!
//! # 実測から得た「仕様書に書いていない」注意点 (全部このコードに反映済み)
//!
//! 1. `tool_call.title` は総称 (`"Terminal"`) で先に届き、あとから
//!    `tool_call_update` が正しい値 (`"echo hi"`) へ**訂正する**。だから行は
//!    upsert ([`Turn::apply`]) で、first-write-wins にしない。
//! 2. `rawInput` は `{}` で始まり、複数回の更新で少しずつ埋まる。
//! 3. `usage_update` はツール 2 回のターンで**約 7 回**飛ぶ。合体しないと
//!    設計原則 3 (アイドルのコストはゼロ) が即死するので [`Coalescer`] を挟む。
//! 4. チャンクは極小 (`"I"`, `"'ll run"`)。レイアウト前に文字列へ溜める。
//! 5. プロンプトのレスポンスに非標準の `usage` が同梱されて届く。
//!    **`#[serde(deny_unknown_fields)]` は使わない** (未知フィールドは黙って捨てる)。
//! 6. 権限要求の `options` は **`reject_once` が先頭**で届く。素朴に `options[0]`
//!    を選ぶと編集を拒否してしまう。選択は必ず [`PermissionOptionKind`] で照合する
//!    ([`pick_option`])。
//!
//! # このモジュールが守る約束
//!
//! - **UI スレッドを絶対にブロックしない。** パースはリーダースレッド、書き込みは
//!   ライタースレッド。UI が触るのは `mpsc` の受け口だけ ([`AcpClient::pump`])。
//! - **ACP は任意・ベストエフォート。** 起動できない相手は**起動前に**検出して
//!   理由を出す ([`AcpEntry::resolve`])。既存の PTY 経路には一切手を入れない
//!   (設計原則 5「ハンドラは 1 面、トランスポートは多数」)。
//! - **承認は新しい UI を作らない。** `session/request_permission` は
//!   [`crate::agents::approvals::ApprovalQueue`] へ積み、既存のネイティブ承認
//!   パネル・ポリシー・監査ログへそのまま乗る。
//! - **`fs/*` はワークスペースのルート配下へサンドボックスする** ([`resolve_in_roots`])。
//!   絶対パスでないもの、`..` で外へ出るもの、シンボリックリンクで外を指すものは拒否。
//! - **終了はプロセスツリーごと** ([`crate::procx::kill_tree`])。エージェントは
//!   自分の MCP 子プロセスを起こすので、直接の子だけ殺すと孫が残る。

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agents::approvals::{self, ApprovalQueue, ReplyAction, Verdict};
use crate::i18n::{tr, trf};
use crate::panels::space;
use crate::theme::Theme;

// ═══════════════════════════════════════════════════════════════════════
//  0. 定数
// ═══════════════════════════════════════════════════════════════════════

/// ワイヤのプロトコル版。**安定は 1 のみ** (2 はドラフト)。
pub const PROTOCOL_VERSION: u32 = 1;

/// レジストリの所在。**取得は任意** — 既定カタログだけでオフラインでも動く。
/// (UI のホバーで出典として見せる。ハードコードした一覧の出どころを隠さない)
pub const REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// JSON-RPC の標準エラー。
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;
/// 不正なパラメータ (サンドボックス違反もここへ落とす)。
pub const ERR_INVALID_PARAMS: i64 = -32602;
/// 内部エラー (I/O 失敗など)。
pub const ERR_INTERNAL: i64 = -32603;
/// ACP 固有: リクエストがキャンセルされた。
pub const ERR_REQUEST_CANCELLED: i64 = -32800;
/// ACP 固有: 認証が必要。
pub const ERR_AUTH_REQUIRED: i64 = -32000;
/// ACP 固有: リソースが見つからない。
pub const ERR_RESOURCE_NOT_FOUND: i64 = -32002;

/// stderr の控えを何行まで持つか (UI のログ欄)。
const STDERR_TAIL: usize = 200;
/// エージェントのメッセージを何文字まで持つか (1 ターン分)。
const MESSAGE_CAP: usize = 200_000;
/// `usage_update` の再描画間引き。実測で 1 ターンに約 7 回飛ぶ。
const USAGE_REPAINT_MS: u64 = 500;
/// メッセージチャンクの再描画間引き (約 30fps)。チャンクは 1 文字単位で届く。
const CHUNK_REPAINT_MS: u64 = 33;

// ═══════════════════════════════════════════════════════════════════════
//  1. エージェントカタログ (**データは src/agents.rs**)
// ═══════════════════════════════════════════════════════════════════════

/// ACP で駆動できるエージェント 1 件の起動情報。
///
/// レジストリ (`REGISTRY_URL`) の `distribution` をそのまま持てる形にしてある。
/// **コマンド文字列はここではなく [`crate::agents::ACP_CATALOG`] にデータとして置く**
/// (CLAUDE.md のハードコーディング禁止: エージェント固有値はカタログへ)。
pub struct AcpEntry {
    /// レジストリの `id`。
    pub id: &'static str,
    /// UI 表示名。
    pub label: &'static str,
    /// UI アイコン。
    pub icon: &'static str,
    /// ローカルに入っていれば**そちらを優先**する実行ファイル名。無ければ ""。
    pub local_bin: &'static str,
    /// `local_bin` に渡す引数。
    pub local_args: &'static [&'static str],
    /// npx 配布のパッケージ名。無ければ ""。
    pub npx_package: &'static str,
    /// レジストリが固定しているバージョン。空なら最新。
    pub npx_version: &'static str,
    /// パッケージに渡す引数。
    pub npx_args: &'static [&'static str],
    /// UI に出す日本語の注意書き。無ければ ""。
    pub note: &'static str,
}

/// 実際に起動するコマンド。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    /// 実行ファイルの**絶対パス** (PATH 解決済み)。
    pub program: PathBuf,
    pub args: Vec<String>,
    /// npx 経由か (初回は取得に時間がかかることを UI が伝えるため)。
    pub via_npx: bool,
}

impl AcpEntry {
    /// `package@version` (バージョン指定が無ければパッケージ名だけ)。
    pub fn package_spec(&self) -> String {
        if self.npx_version.is_empty() {
            self.npx_package.to_string()
        } else {
            format!("{}@{}", self.npx_package, self.npx_version)
        }
    }

    /// 起動コマンドを決める。**起動してハングするのではなく、事前に検出する。**
    ///
    /// ローカル実行ファイル → npx の順。どちらも無ければ**理由つきで**失敗する。
    pub fn resolve(&self) -> Result<Launch, String> {
        if !self.local_bin.is_empty() {
            if let Some(p) = crate::shellenv::which(self.local_bin) {
                return Ok(Launch {
                    program: p,
                    args: self.local_args.iter().map(|s| s.to_string()).collect(),
                    via_npx: false,
                });
            }
        }
        if self.npx_package.is_empty() {
            return Err(trf(
                "{bin} が見つかりません",
                &[("bin", self.local_bin.to_string())],
            ));
        }
        let Some(npx) = crate::shellenv::which("npx") else {
            return Err(tr("npx (Node.js) が見つかりません — Node.js を入れるか、対応 CLI を直接インストールしてください"));
        };
        let mut args = vec!["-y".to_string(), self.package_spec()];
        args.extend(self.npx_args.iter().map(|s| s.to_string()));
        Ok(Launch {
            program: npx,
            args,
            via_npx: true,
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  2. パッチ値 — 「省略 ≠ null ≠ 値」
// ═══════════════════════════════════════════════════════════════════════

/// 3 状態を区別するパッチ値。
///
/// v1 でも `tool_call_update` は**触ったフィールドだけ**を送ってくるので、
/// 素の `Option<T>` だと「省略」と「null にした」が区別できない。v2 は
/// パッチ意味論を明文化するので、いま分けておけば書き直しにならない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Patch<T> {
    /// キーそのものが無い = 触っていない
    Absent,
    /// 明示的な `null` = 消した
    Null,
    /// 値が入った
    Set(T),
}

impl<T> Default for Patch<T> {
    fn default() -> Self {
        Patch::Absent
    }
}

impl<T> Patch<T> {
    /// 触っていなければ何もしない。`null` なら消す。値なら入れる。
    pub fn apply(self, dst: &mut Option<T>) {
        match self {
            Patch::Absent => {}
            Patch::Null => *dst = None,
            Patch::Set(v) => *dst = Some(v),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Patch<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // フィールドに `#[serde(default)]` を付けてあるので、キーが無いときは
        // ここへ来ない (= `Absent`)。来たなら null か値のどちらか。
        Ok(match Option::<T>::deserialize(d)? {
            Some(v) => Patch::Set(v),
            None => Patch::Null,
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  3. 列挙値 (判別子は snake_case。**未知の値で落ちない**)
// ═══════════════════════════════════════════════════════════════════════

/// 文字列 → 列挙のゆるい復元を作る。未知の値は既定へ丸める
/// (エージェントが新しい種別を足しても、こちらは動き続ける)。
macro_rules! wire_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $wire:literal),+ $(,)? } default $def:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
        pub enum $name { $($variant),+ }

        impl $name {
            /// ワイヤ表記 (snake_case)。
            pub fn as_wire(self) -> &'static str {
                match self { $($name::$variant => $wire),+ }
            }
            /// ワイヤ表記から復元する。未知は既定値。
            pub fn from_wire(s: &str) -> Self {
                match s { $($wire => $name::$variant,)+ _ => $name::$def }
            }
        }

        impl Default for $name {
            fn default() -> Self { $name::$def }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Ok($name::from_wire(&String::deserialize(d)?))
            }
        }
    };
}

wire_enum! {
    /// ツールの種別。UI のアイコンと承認種別への写像に使う。
    ToolKind {
        Read => "read",
        Edit => "edit",
        Delete => "delete",
        Move => "move",
        Search => "search",
        Execute => "execute",
        Think => "think",
        Fetch => "fetch",
        SwitchMode => "switch_mode",
        Other => "other",
    } default Other
}

wire_enum! {
    /// ツール呼び出しの状態。
    ToolCallStatus {
        Pending => "pending",
        InProgress => "in_progress",
        Completed => "completed",
        Failed => "failed",
    } default Pending
}

wire_enum! {
    /// 計画 (plan) 1 項目の状態。
    PlanEntryStatus {
        Pending => "pending",
        InProgress => "in_progress",
        Completed => "completed",
    } default Pending
}

wire_enum! {
    /// 計画 1 項目の優先度。
    PlanEntryPriority {
        High => "high",
        Medium => "medium",
        Low => "low",
    } default Medium
}

wire_enum! {
    /// 権限選択肢の種別。**選択は必ずこれで照合する** (インデックス禁止)。
    PermissionOptionKind {
        AllowOnce => "allow_once",
        AllowAlways => "allow_always",
        RejectOnce => "reject_once",
        RejectAlways => "reject_always",
    } default RejectOnce
}

wire_enum! {
    /// ターンの終わり方。
    StopReason {
        EndTurn => "end_turn",
        MaxTokens => "max_tokens",
        MaxTurnRequests => "max_turn_requests",
        Refusal => "refusal",
        Cancelled => "cancelled",
    } default EndTurn
}

impl ToolKind {
    /// UI アイコン。
    pub fn icon(self) -> &'static str {
        match self {
            ToolKind::Read => "👁",
            ToolKind::Edit => "✏",
            ToolKind::Delete => "🗑",
            ToolKind::Move => "📦",
            ToolKind::Search => "🔍",
            ToolKind::Execute => "⌘",
            ToolKind::Think => "💭",
            ToolKind::Fetch => "🌐",
            ToolKind::SwitchMode => "🔀",
            ToolKind::Other => "🔧",
        }
    }
}

impl ToolCallStatus {
    /// UI アイコン。
    pub fn icon(self) -> &'static str {
        match self {
            ToolCallStatus::Pending => "○",
            ToolCallStatus::InProgress => "◐",
            ToolCallStatus::Completed => "●",
            ToolCallStatus::Failed => "✗",
        }
    }

    /// UI ラベル (原文は日本語。`tr()` を通して使う)。
    pub fn label(self) -> &'static str {
        match self {
            ToolCallStatus::Pending => "待機",
            ToolCallStatus::InProgress => "実行中",
            ToolCallStatus::Completed => "完了",
            ToolCallStatus::Failed => "失敗",
        }
    }
}

impl PlanEntryStatus {
    /// UI アイコン。
    pub fn icon(self) -> &'static str {
        match self {
            PlanEntryStatus::Pending => "☐",
            PlanEntryStatus::InProgress => "▶",
            PlanEntryStatus::Completed => "☑",
        }
    }
}

impl StopReason {
    /// UI ラベル (原文は日本語)。
    pub fn label(self) -> &'static str {
        match self {
            StopReason::EndTurn => "ターン終了",
            StopReason::MaxTokens => "トークン上限で打ち切り",
            StopReason::MaxTurnRequests => "リクエスト数上限で打ち切り",
            StopReason::Refusal => "エージェントが拒否",
            StopReason::Cancelled => "キャンセル",
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  4. ワイヤ型 (**未知フィールドは黙って捨てる**)
// ═══════════════════════════════════════════════════════════════════════

/// コンテンツブロック。種別を増やされても壊れないよう、必要な値だけ拾う。
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
}

impl ContentBlock {
    /// 画面へ出せる文字列。テキスト以外は種別を括弧書きで示す。
    pub fn display_text(&self) -> String {
        if !self.text.is_empty() {
            return self.text.clone();
        }
        match (
            self.kind.as_str(),
            self.path.as_deref(),
            self.uri.as_deref(),
        ) {
            ("", None, None) => String::new(),
            (k, Some(p), _) => format!("[{k}: {p}]"),
            (k, None, Some(u)) => format!("[{k}: {u}]"),
            (k, None, None) => format!("[{k}]"),
        }
    }
}

/// ツール呼び出しが触っている場所。**エディタの追従はこれが駆動する。**
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ToolCallLocation {
    #[serde(default)]
    pub path: String,
    /// 1 始まり。
    #[serde(default)]
    pub line: Option<u32>,
}

/// ツール呼び出しに添えられる内容。
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ToolCallContent {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub content: Option<ContentBlock>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(rename = "oldText", default)]
    pub old_text: Option<String>,
    #[serde(rename = "newText", default)]
    pub new_text: Option<String>,
    #[serde(rename = "terminalId", default)]
    pub terminal_id: Option<String>,
}

impl ToolCallContent {
    /// 人が読む 1 行。`diff` は行数、`terminal` は ID、`content` は本文。
    pub fn summary(&self) -> String {
        match self.kind.as_str() {
            "diff" => {
                let old_n = self.old_text.as_deref().map(count_lines).unwrap_or(0);
                let new_n = self.new_text.as_deref().map(count_lines).unwrap_or(0);
                format!(
                    "diff {} (-{old_n}/+{new_n})",
                    self.path.clone().unwrap_or_default()
                )
            }
            "terminal" => format!("terminal {}", self.terminal_id.clone().unwrap_or_default()),
            other => {
                let body = self
                    .content
                    .as_ref()
                    .map(|c| c.display_text())
                    .unwrap_or_default();
                if body.is_empty() {
                    other.to_string()
                } else {
                    format!("{other}: {body}")
                }
            }
        }
    }
}

/// 行数 (末尾に改行が無くても最後の行を数える)。
fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.lines().count()
    }
}

/// `tool_call` / `tool_call_update` の中身 (**両方とも同じ形。更新は部分適用**)。
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ToolCallPatch {
    #[serde(rename = "toolCallId", default)]
    pub tool_call_id: String,
    #[serde(default)]
    pub title: Patch<String>,
    #[serde(default)]
    pub kind: Patch<ToolKind>,
    #[serde(default)]
    pub status: Patch<ToolCallStatus>,
    #[serde(default)]
    pub content: Patch<Vec<ToolCallContent>>,
    #[serde(default)]
    pub locations: Patch<Vec<ToolCallLocation>>,
    #[serde(rename = "rawInput", default)]
    pub raw_input: Patch<Value>,
    #[serde(rename = "rawOutput", default)]
    pub raw_output: Patch<Value>,
}

/// 計画 (plan) の 1 項目。
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct PlanEntry {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub status: PlanEntryStatus,
    #[serde(default)]
    pub priority: PlanEntryPriority,
}

/// 使用量。
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub used: u64,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub cost: Option<Cost>,
}

/// 課金額。
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Cost {
    #[serde(default)]
    pub amount: f64,
    #[serde(default)]
    pub currency: String,
}

/// スラッシュコマンド 1 件。
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct AvailableCommand {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// `session/update` の中身 — **v1 の全 11 種**。
///
/// 封筒は `{"sessionId":…, "update":{"sessionUpdate":"<種別>", …平坦化}}`。
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    UserMessageChunk {
        #[serde(default)]
        content: ContentBlock,
    },
    AgentMessageChunk {
        #[serde(default)]
        content: ContentBlock,
    },
    AgentThoughtChunk {
        #[serde(default)]
        content: ContentBlock,
    },
    ToolCall(ToolCallPatch),
    ToolCallUpdate(ToolCallPatch),
    Plan {
        #[serde(default)]
        entries: Vec<PlanEntry>,
    },
    AvailableCommandsUpdate {
        #[serde(rename = "availableCommands", default)]
        available_commands: Vec<AvailableCommand>,
    },
    CurrentModeUpdate {
        #[serde(rename = "currentModeId", default)]
        current_mode_id: String,
    },
    ConfigOptionUpdate {
        #[serde(rename = "configOptions", default)]
        config_options: Value,
    },
    SessionInfoUpdate {
        #[serde(default)]
        title: Option<String>,
        #[serde(rename = "updatedAt", default)]
        updated_at: Option<String>,
    },
    UsageUpdate(Usage),
    /// 知らない種別。**捨てずに握り潰す** (未知で落ちないための受け皿)。
    #[serde(other)]
    Unknown,
}

impl SessionUpdate {
    /// 再描画の間引き区分。**`usage_update` だけ特別扱いする** (1 ターン約 7 回)。
    pub fn repaint_class(&self) -> RepaintClass {
        match self {
            SessionUpdate::UsageUpdate(_) => RepaintClass::Usage,
            SessionUpdate::UserMessageChunk { .. }
            | SessionUpdate::AgentMessageChunk { .. }
            | SessionUpdate::AgentThoughtChunk { .. } => RepaintClass::Chunk,
            SessionUpdate::Unknown => RepaintClass::Silent,
            _ => RepaintClass::Immediate,
        }
    }
}

/// 再描画をどれだけ間引くか。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RepaintClass {
    /// すぐ描く (ツール状態・計画など、見逃すと困るもの)
    Immediate,
    /// 約 30fps へ間引く (1 文字ずつ届くチャンク)
    Chunk,
    /// 0.5 秒へ間引く (使用量)
    Usage,
    /// 描かない (未知の種別)
    Silent,
}

impl RepaintClass {
    /// 間引き間隔。
    pub fn interval(self) -> Duration {
        match self {
            RepaintClass::Immediate => Duration::ZERO,
            RepaintClass::Chunk => Duration::from_millis(CHUNK_REPAINT_MS),
            RepaintClass::Usage => Duration::from_millis(USAGE_REPAINT_MS),
            RepaintClass::Silent => Duration::MAX,
        }
    }
}

/// エージェントの自己申告。
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct AgentInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub version: String,
}

impl AgentInfo {
    /// 画面に出す 1 行 (`title` があればそちらを優先し、無ければパッケージ名)。
    pub fn display_line(&self) -> String {
        let name = match self.title.as_deref() {
            Some(t) if !t.is_empty() => t,
            _ => self.name.as_str(),
        };
        if self.version.is_empty() {
            name.to_string()
        } else {
            format!("{name} {}", self.version)
        }
    }
}

/// エージェントの能力。**省略されているものは「非対応」**。
///
/// v1 は能力マーカーがブール値 (`image: true`) と空オブジェクト (`list: {}`) で
/// 混在しているので、`sessionCapabilities` は**キーの有無**だけを見る。
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AgentCapabilities {
    #[serde(rename = "loadSession", default)]
    pub load_session: bool,
    #[serde(rename = "promptCapabilities", default)]
    pub prompt_capabilities: Value,
    #[serde(rename = "sessionCapabilities", default)]
    pub session_capabilities: Value,
}

impl AgentCapabilities {
    /// `sessionCapabilities.<key>` を広告しているか (値の型は問わない)。
    pub fn session_supports(&self, key: &str) -> bool {
        self.session_capabilities
            .get(key)
            .is_some_and(|v| !v.is_null() && v != &json!(false))
    }

    /// `promptCapabilities.image` — 画像を渡せるか。
    pub fn supports_image(&self) -> bool {
        self.prompt_capabilities
            .get("image")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

/// `initialize` の結果。
#[derive(Clone, Debug, Default, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion", default)]
    pub protocol_version: u32,
    #[serde(rename = "agentCapabilities", default)]
    pub agent_capabilities: AgentCapabilities,
    #[serde(rename = "agentInfo", default)]
    pub agent_info: Option<AgentInfo>,
    #[serde(rename = "authMethods", default)]
    pub auth_methods: Vec<Value>,
    #[serde(rename = "_meta", default)]
    pub meta: Value,
}

impl InitializeResult {
    /// `_meta.steering.supported` — 仕様書に無いが Claude / Codex のアダプタが
    /// 両方とも実装している拡張。走行中のターンへ割り込める。
    pub fn steering_supported(&self) -> bool {
        self.meta
            .get("steering")
            .and_then(|s| s.get("supported"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

/// 権限選択肢 1 件。
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct PermissionOption {
    #[serde(rename = "optionId", default)]
    pub option_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: PermissionOptionKind,
}

/// `session/request_permission` のパラメータ。
#[derive(Clone, Debug, Default, Deserialize)]
pub struct PermissionParams {
    #[serde(rename = "sessionId", default)]
    pub session_id: String,
    #[serde(rename = "toolCall", default)]
    pub tool_call: ToolCallPatch,
    #[serde(default)]
    pub options: Vec<PermissionOption>,
}

/// JSON-RPC のエラー本体。
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct RpcError {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub message: String,
}

impl RpcError {
    /// 人が読める 1 行 (`-32000` などの ACP 固有コードは日本語で補う)。
    pub fn describe(&self) -> String {
        let hint = match self.code {
            ERR_AUTH_REQUIRED => tr("認証が必要です"),
            ERR_RESOURCE_NOT_FOUND => tr("リソースが見つかりません"),
            ERR_REQUEST_CANCELLED => tr("キャンセルされました"),
            ERR_METHOD_NOT_FOUND => tr("メソッドがありません"),
            _ => String::new(),
        };
        if hint.is_empty() {
            format!("[{}] {}", self.code, self.message)
        } else {
            format!("[{}] {} — {}", self.code, hint, self.message)
        }
    }
}

/// **`reject_once` が先頭で届く**ので、インデックスではなく種別で選ぶ。
///
/// `allow = true` なら `allow_once` → `allow_always` の順、
/// `false` なら `reject_once` → `reject_always` の順に探す。
/// 望む向きの選択肢が 1 つも無ければ `None` (勝手に逆を選ばない)。
pub fn pick_option(options: &[PermissionOption], allow: bool) -> Option<&PermissionOption> {
    let order: [PermissionOptionKind; 2] = if allow {
        [
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ]
    } else {
        [
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ]
    };
    order
        .iter()
        .find_map(|want| options.iter().find(|o| o.kind == *want))
}

// ═══════════════════════════════════════════════════════════════════════
//  5. JSON-RPC のフレーミング
// ═══════════════════════════════════════════════════════════════════════

/// 1 行に収まる JSON へ直す。**`serde_json::to_string` は生の改行を出さない**
/// (制御文字は必ずエスケープされる) ので、この 1 関数だけで
/// 「メッセージに改行を含めない」という規約を守れる。
pub fn encode_line(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string())
}

/// リクエスト。
pub fn rpc_request(id: i64, method: &str, params: Value) -> String {
    encode_line(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
}

/// 通知 (`id` を持たない)。
pub fn rpc_notify(method: &str, params: Value) -> String {
    encode_line(&json!({"jsonrpc":"2.0","method":method,"params":params}))
}

/// 成功レスポンス。`id` は受け取ったものをそのまま返す (数値とは限らない)。
pub fn rpc_result(id: &Value, result: Value) -> String {
    encode_line(&json!({"jsonrpc":"2.0","id":id,"result":result}))
}

/// エラーレスポンス。
pub fn rpc_error(id: &Value, code: i64, message: &str) -> String {
    encode_line(&json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
}

/// 受信した 1 行の分類。**リーダースレッドはこの 3 系統へ振り分ける。**
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    /// エージェント → クライアントのリクエスト (返事が要る)
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// 自分が投げたリクエストへの返事
    Response {
        id: i64,
        result: Result<Value, RpcError>,
    },
    /// 通知 (返事は要らない)
    Notification { method: String, params: Value },
    /// JSON として読めない / 形が違う
    Malformed(String),
}

/// 1 行を分類する (**純関数**)。
pub fn classify(line: &str) -> Incoming {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Incoming::Malformed(String::new());
    }
    let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
        return Incoming::Malformed(trimmed.to_string());
    };
    let method = v.get("method").and_then(Value::as_str);
    let id = v.get("id").cloned();
    match (method, id) {
        (Some(m), Some(id)) if !id.is_null() => Incoming::Request {
            id,
            method: m.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        },
        (Some(m), _) => Incoming::Notification {
            method: m.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        },
        (None, Some(id)) => {
            // 相関できない id (文字列など) は自分が投げたものではない。
            let Some(n) = id.as_i64() else {
                return Incoming::Malformed(trimmed.to_string());
            };
            if let Some(e) = v.get("error") {
                let err = serde_json::from_value::<RpcError>(e.clone()).unwrap_or_default();
                Incoming::Response {
                    id: n,
                    result: Err(err),
                }
            } else {
                Incoming::Response {
                    id: n,
                    result: Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                }
            }
        }
        (None, None) => Incoming::Malformed(trimmed.to_string()),
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  6. クライアント側のファイルシステム (**ワークスペースへサンドボックス**)
// ═══════════════════════════════════════════════════════════════════════

/// サンドボックスが拒否した理由。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsDenied {
    /// 絶対パスでない (ACP は絶対パスを要求する)
    NotAbsolute,
    /// ワークスペースのルートから出ている
    Outside,
    /// ルートが 1 つも無い (フォルダを開いていない)
    NoRoots,
}

impl FsDenied {
    /// 日本語の理由 (原文。`tr()` を通して使う)。
    pub fn label(self) -> &'static str {
        match self {
            FsDenied::NotAbsolute => "絶対パスではありません",
            FsDenied::Outside => "ワークスペースの外です",
            FsDenied::NoRoots => "フォルダを開いていません",
        }
    }
}

/// `.` と `..` をファイルシステムに触らずに畳む (**純関数**)。
///
/// `..` はルートより上には行かない。Windows のプレフィックス (`C:`) は残す。
pub fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut popped: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if popped.pop().is_none() {
                    // ルートより上へは行かせない (捨てる)
                }
            }
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::Normal(_) => popped.push(c),
        }
    }
    for c in popped {
        out.push(c.as_os_str());
    }
    out
}

/// 実在する最も深い祖先だけ canonicalize して、残りを繋ぎ直す。
///
/// これでシンボリックリンク経由の脱出を捕まえられる (存在しないファイルを
/// 書くときも、親ディレクトリのリンクは解決される)。
fn canonical_or_nearest(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return crate::pathx::plain(c);
    }
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cur = p;
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            rest.push(name);
        }
        if let Ok(c) = parent.canonicalize() {
            let mut out = crate::pathx::plain(c);
            for name in rest.iter().rev() {
                out.push(name);
            }
            return out;
        }
        cur = parent;
    }
    p.to_path_buf()
}

/// ワークスペースのルート配下へ閉じ込める。**外へ出るパスは拒否**。
///
/// 返すのは正規化済みの絶対パス。`roots` は呼び出し側で canonicalize 済みの
/// ものを渡す ([`FsHost::set_roots`] がやる)。
pub fn resolve_in_roots(roots: &[PathBuf], raw: &str) -> Result<PathBuf, FsDenied> {
    let p = Path::new(raw);
    if !p.is_absolute() {
        return Err(FsDenied::NotAbsolute);
    }
    if roots.is_empty() {
        return Err(FsDenied::NoRoots);
    }
    let norm = lexical_normalize(p);
    let real = canonical_or_nearest(&norm);
    if roots.iter().any(|r| real.starts_with(r)) {
        Ok(real)
    } else {
        Err(FsDenied::Outside)
    }
}

/// クライアント側 `fs/*` の実装。**リーダースレッドから直接呼ばれる**
/// (UI のフレームを待たせない)。
pub struct FsHost {
    inner: Mutex<FsHostState>,
}

#[derive(Default)]
struct FsHostState {
    /// canonicalize 済みのワークスペースルート。
    roots: Vec<PathBuf>,
    /// **まだ保存していないエディタバッファ**。ここが ACP の存在意義の 1 つで、
    /// ディスクではなく「いま画面に見えている内容」をエージェントへ返せる。
    unsaved: HashMap<PathBuf, String>,
}

impl Default for FsHost {
    fn default() -> Self {
        FsHost {
            inner: Mutex::new(FsHostState::default()),
        }
    }
}

impl FsHost {
    /// ワークスペースのルートを差し替える。
    pub fn set_roots(&self, roots: &[PathBuf]) {
        let canon: Vec<PathBuf> = roots.iter().map(|r| crate::pathx::canonical(r)).collect();
        if let Ok(mut g) = self.inner.lock() {
            g.roots = canon;
        }
    }

    /// 未保存バッファの一覧を丸ごと差し替える (保存されたものは自然に消える)。
    pub fn replace_unsaved(&self, items: Vec<(PathBuf, String)>) {
        let map: HashMap<PathBuf, String> = items
            .into_iter()
            .map(|(p, t)| (crate::pathx::canonical(&p), t))
            .collect();
        if let Ok(mut g) = self.inner.lock() {
            g.unsaved = map;
        }
    }

    /// 公開をやめる (接続が 0 本になったとき)。
    pub fn clear_unsaved(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.unsaved.clear();
        }
    }

    /// いま保持している未保存バッファの数 (UI の表示用)。
    pub fn unsaved_len(&self) -> usize {
        self.inner.lock().map(|g| g.unsaved.len()).unwrap_or(0)
    }

    /// `fs/read_text_file`。`line` は 1 始まり、`limit` は行数。
    fn read_text_file(&self, params: &Value) -> Result<Value, (i64, String)> {
        let raw = params.get("path").and_then(Value::as_str).unwrap_or("");
        let (roots, unsaved) = {
            let g = self
                .inner
                .lock()
                .map_err(|_| (ERR_INTERNAL, tr("内部状態を取得できません")))?;
            (g.roots.clone(), g.unsaved.clone())
        };
        let path = resolve_in_roots(&roots, raw).map_err(|d| {
            (
                ERR_INVALID_PARAMS,
                trf(
                    "読み取りを拒否しました ({why}): {path}",
                    &[("why", tr(d.label())), ("path", raw.to_string())],
                ),
            )
        })?;
        // **未保存のエディタバッファがあればそちらを返す。**
        let text = match unsaved.get(&path) {
            Some(t) => t.clone(),
            None => std::fs::read_to_string(&path).map_err(|e| {
                (
                    ERR_RESOURCE_NOT_FOUND,
                    trf(
                        "読み取れません: {path} ({e})",
                        &[("path", raw.to_string()), ("e", e.to_string())],
                    ),
                )
            })?,
        };
        let line = params.get("line").and_then(Value::as_u64);
        let limit = params.get("limit").and_then(Value::as_u64);
        Ok(json!({ "content": slice_lines(&text, line, limit) }))
    }

    /// `fs/write_text_file`。**ファイルが無ければ作る** (親ディレクトリごと)。
    fn write_text_file(&self, params: &Value) -> Result<Value, (i64, String)> {
        let raw = params.get("path").and_then(Value::as_str).unwrap_or("");
        let content = params
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let roots = {
            let g = self
                .inner
                .lock()
                .map_err(|_| (ERR_INTERNAL, tr("内部状態を取得できません")))?;
            g.roots.clone()
        };
        let path = resolve_in_roots(&roots, raw).map_err(|d| {
            (
                ERR_INVALID_PARAMS,
                trf(
                    "書き込みを拒否しました ({why}): {path}",
                    &[("why", tr(d.label())), ("path", raw.to_string())],
                ),
            )
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                (
                    ERR_INTERNAL,
                    trf("親フォルダを作れません: {e}", &[("e", e.to_string())]),
                )
            })?;
        }
        std::fs::write(&path, content).map_err(|e| {
            (
                ERR_INTERNAL,
                trf(
                    "書き込めません: {path} ({e})",
                    &[("path", raw.to_string()), ("e", e.to_string())],
                ),
            )
        })?;
        Ok(Value::Null)
    }

    /// リーダースレッドから呼ぶ入口。**このモジュールが答えられるものだけ**扱う。
    fn handle(&self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "fs/read_text_file" => self.read_text_file(params),
            "fs/write_text_file" => self.write_text_file(params),
            _ => Err((
                ERR_METHOD_NOT_FOUND,
                trf(
                    "このクライアントは {m} を実装していません",
                    &[("m", method.to_string())],
                ),
            )),
        }
    }
}

/// `line` (1 始まり) と `limit` (行数) を当てる (**純関数**)。
pub fn slice_lines(text: &str, line: Option<u64>, limit: Option<u64>) -> String {
    if line.is_none() && limit.is_none() {
        return text.to_string();
    }
    let start = line.unwrap_or(1).max(1) as usize - 1;
    let mut it = text.lines().skip(start);
    let picked: Vec<&str> = match limit {
        Some(n) => it.by_ref().take(n as usize).collect(),
        None => it.by_ref().collect(),
    };
    picked.join("\n")
}

// ═══════════════════════════════════════════════════════════════════════
//  7. 再描画の間引き
// ═══════════════════════════════════════════════════════════════════════

/// 再描画の合体器。**設計原則 3「アイドル時のコストはゼロ」の実装**。
///
/// `usage_update` は 1 ターンに約 7 回、チャンクは 1 文字ずつ飛んでくる。
/// そのたびに `request_repaint` すると、エージェントが喋っている間は
/// フレームを回しっぱなしになる。
#[derive(Debug, Default)]
pub struct Coalescer {
    last: HashMap<RepaintClassKey, Instant>,
}

/// [`Coalescer`] のキー (HashMap に入れるため `RepaintClass` を写す)。
type RepaintClassKey = u8;

impl Coalescer {
    /// いま描いてよいか。`true` を返したときだけ時刻を進める。
    pub fn allow(&mut self, class: RepaintClass, now: Instant) -> bool {
        if class == RepaintClass::Silent {
            return false;
        }
        let key = class as RepaintClassKey;
        let interval = class.interval();
        if interval.is_zero() {
            self.last.insert(key, now);
            return true;
        }
        match self.last.get(&key) {
            Some(prev) if now.duration_since(*prev) < interval => false,
            _ => {
                self.last.insert(key, now);
                true
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  8. トランスポート
// ═══════════════════════════════════════════════════════════════════════

/// リーダースレッドが UI へ渡すもの。
///
/// 大きいものは `Box` に入れる (variant の大きさを揃える)。
#[derive(Debug)]
pub enum AcpEvent {
    /// `session/update` 通知
    Update {
        session: String,
        update: Box<SessionUpdate>,
    },
    /// 自分が投げたリクエストへの返事
    Response {
        id: i64,
        result: Box<Result<Value, RpcError>>,
    },
    /// 権限要求 (**UI = 承認キューが答える**)
    Permission {
        req_id: Value,
        params: Box<PermissionParams>,
    },
    /// エージェントの stderr 1 行
    Stderr(String),
    /// stdout が閉じた (プロセス終了)
    Closed,
    /// 読み飛ばした行など、記録だけ残したいもの
    Note(String),
}

/// 1 行ずつ読んで振り分ける本体。**テストは `Cursor` を渡して呼べる**
/// (実エージェントのバイナリを要らなくするため、ここを `BufRead` で切ってある)。
fn read_loop<R: BufRead>(
    reader: R,
    tx: mpsc::Sender<AcpEvent>,
    out: mpsc::Sender<String>,
    host: Arc<FsHost>,
    ctx: Option<egui::Context>,
) {
    let mut co = Coalescer::default();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let mut class = RepaintClass::Immediate;
        match classify(&line) {
            Incoming::Notification { method, params } if method == "session/update" => {
                let session = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let raw = params.get("update").cloned().unwrap_or(Value::Null);
                let update: SessionUpdate =
                    serde_json::from_value(raw).unwrap_or(SessionUpdate::Unknown);
                class = update.repaint_class();
                if tx
                    .send(AcpEvent::Update {
                        session,
                        update: Box::new(update),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Incoming::Notification { method, .. } => {
                // `$/cancel_request` などは受け取るだけ。こちらに長時間走る
                // クライアント側処理は無いので、部分結果もエラーも返さない。
                class = RepaintClass::Silent;
                let _ = tx.send(AcpEvent::Note(format!("notify {method}")));
            }
            Incoming::Response { id, result } => {
                if tx
                    .send(AcpEvent::Response {
                        id,
                        result: Box::new(result),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Incoming::Request { id, method, params } => {
                if method == "session/request_permission" {
                    let p: PermissionParams = serde_json::from_value(params).unwrap_or_default();
                    if tx
                        .send(AcpEvent::Permission {
                            req_id: id,
                            params: Box::new(p),
                        })
                        .is_err()
                    {
                        break;
                    }
                } else {
                    // fs/* はここで即答する (UI のフレームを待たせない)。
                    let reply = match host.handle(&method, &params) {
                        Ok(v) => rpc_result(&id, v),
                        Err((code, msg)) => rpc_error(&id, code, &msg),
                    };
                    if out.send(reply).is_err() {
                        break;
                    }
                    class = RepaintClass::Chunk;
                }
            }
            Incoming::Malformed(raw) => {
                class = RepaintClass::Silent;
                if !raw.is_empty() {
                    let _ = tx.send(AcpEvent::Note(format!("非 JSON-RPC 行: {raw}")));
                }
            }
        }
        if let Some(c) = &ctx {
            if co.allow(class, Instant::now()) {
                crate::perf::repaint(c, "acp");
            }
        }
    }
    let _ = tx.send(AcpEvent::Closed);
    if let Some(c) = &ctx {
        crate::perf::repaint(c, "acp");
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  9. ターンの状態 (**upsert で持つ**)
// ═══════════════════════════════════════════════════════════════════════

/// 1 件のツール呼び出し。`tool_call_update` で**後から書き換わる**前提。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolCallRow {
    pub id: String,
    /// 総称で届き、あとから訂正される。
    pub title: Option<String>,
    pub kind: Option<ToolKind>,
    pub status: Option<ToolCallStatus>,
    pub locations: Option<Vec<ToolCallLocation>>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<Value>,
    pub content: Option<Vec<ToolCallContent>>,
}

impl ToolCallRow {
    /// 画面に出す見出し。訂正前でも空にはしない。
    pub fn display_title(&self) -> String {
        match self.title.as_deref() {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => self.id.clone(),
        }
    }

    /// ホバーに出す詳細 (**行に収まらないものは全部ここ**)。
    pub fn detail_lines(&self) -> Vec<String> {
        let mut out = vec![self.display_title()];
        if let Some(k) = self.kind {
            out.push(format!("kind: {}", k.as_wire()));
        }
        if let Some(locs) = &self.locations {
            for l in locs.iter().take(8) {
                out.push(match l.line {
                    Some(n) => format!("{}:{}", l.path, n),
                    None => l.path.clone(),
                });
            }
        }
        if let Some(v) = &self.raw_input {
            let s = raw_input_summary(v);
            if !s.is_empty() {
                out.push(format!("in: {s}"));
            }
        }
        if let Some(v) = &self.raw_output {
            let s = approvals::trim_cap(&v.to_string(), 200);
            if s != "null" && s != "{}" {
                out.push(format!("out: {s}"));
            }
        }
        if let Some(cs) = &self.content {
            for c in cs.iter().take(4) {
                out.push(c.summary());
            }
        }
        out
    }

    /// 触っている場所の 1 行 (`path:line`)。無ければ空。
    pub fn location_line(&self) -> String {
        let Some(locs) = &self.locations else {
            return String::new();
        };
        let Some(first) = locs.first() else {
            return String::new();
        };
        match first.line {
            Some(n) => format!("{}:{}", first.path, n),
            None => first.path.clone(),
        }
    }
}

/// いま走っているターンの構造化された姿。
#[derive(Clone, Debug, Default)]
pub struct Turn {
    /// 計画。`plan` 更新は**毎回まるごと置き換え**。
    pub plan: Vec<PlanEntry>,
    /// ツール呼び出し (届いた順)。
    pub tools: Vec<ToolCallRow>,
    /// エージェントの本文 (チャンクを連結したもの)。
    pub message: String,
    /// 思考 (チャンクを連結したもの)。
    pub thought: String,
    /// 使用量 (最後の 1 件だけ持つ = 合体)。
    pub usage: Option<Usage>,
    /// セッションの自動タイトル。
    pub title: Option<String>,
    /// 現在のモード ID。
    pub mode: Option<String>,
    /// 使えるスラッシュコマンド。
    pub commands: Vec<AvailableCommand>,
    /// 直前のターンの終わり方。
    pub stop: Option<StopReason>,
}

impl Turn {
    /// 新しいプロンプトを投げるときに、ターン固有の状態だけ捨てる。
    /// (モード・コマンド一覧・タイトルはセッションの持ち物なので残す)
    pub fn begin(&mut self) {
        self.plan.clear();
        self.tools.clear();
        self.message.clear();
        self.thought.clear();
        self.stop = None;
    }

    /// 1 件の更新を畳み込む。**部分更新は upsert**。
    pub fn apply(&mut self, u: SessionUpdate) {
        match u {
            SessionUpdate::AgentMessageChunk { content } => {
                push_capped(&mut self.message, &content.display_text());
            }
            SessionUpdate::AgentThoughtChunk { content } => {
                push_capped(&mut self.thought, &content.display_text());
            }
            SessionUpdate::UserMessageChunk { .. } => {}
            SessionUpdate::ToolCall(p) | SessionUpdate::ToolCallUpdate(p) => self.upsert_tool(p),
            SessionUpdate::Plan { entries } => self.plan = entries,
            SessionUpdate::AvailableCommandsUpdate { available_commands } => {
                self.commands = available_commands;
            }
            SessionUpdate::CurrentModeUpdate { current_mode_id } => {
                self.mode = Some(current_mode_id);
            }
            SessionUpdate::ConfigOptionUpdate { .. } => {}
            SessionUpdate::SessionInfoUpdate { title, .. } => {
                if let Some(t) = title {
                    self.title = Some(t);
                }
            }
            SessionUpdate::UsageUpdate(u) => self.usage = Some(u),
            SessionUpdate::Unknown => {}
        }
    }

    /// ツール行を upsert する。**first-write-wins にしない** (title は訂正される)。
    fn upsert_tool(&mut self, p: ToolCallPatch) {
        let id = p.tool_call_id.clone();
        let row = match self.tools.iter_mut().find(|r| r.id == id) {
            Some(r) => r,
            None => {
                self.tools.push(ToolCallRow {
                    id: id.clone(),
                    ..Default::default()
                });
                self.tools.last_mut().expect("たった今 push した")
            }
        };
        p.title.apply(&mut row.title);
        p.kind.apply(&mut row.kind);
        p.status.apply(&mut row.status);
        p.locations.apply(&mut row.locations);
        p.content.apply(&mut row.content);
        p.raw_output.apply(&mut row.raw_output);
        // rawInput は `{}` から少しずつ埋まる。空オブジェクトで上書きしない。
        match p.raw_input {
            Patch::Set(v) if v.as_object().is_some_and(|o| o.is_empty()) => {
                row.raw_input.get_or_insert(v);
            }
            other => other.apply(&mut row.raw_input),
        }
    }
}

/// 上限つきで文字列を足す (先頭から捨てる)。
fn push_capped(dst: &mut String, add: &str) {
    dst.push_str(add);
    if dst.len() > MESSAGE_CAP {
        let cut = dst.len() - MESSAGE_CAP;
        // 文字境界まで進めてから捨てる
        let mut i = cut;
        while i < dst.len() && !dst.is_char_boundary(i) {
            i += 1;
        }
        dst.drain(..i);
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  10. 承認キューへの橋渡し
// ═══════════════════════════════════════════════════════════════════════

/// 権限要求から、承認キューへ渡す**根拠テキスト**を組む (**純関数**)。
///
/// 既存の分類器 (`approvals::classify_detail`) はエージェントの画面テキストを
/// 読む前提なので、そこへ渡せる形へ翻訳する。**構造化された `kind` を先頭に
/// 置き、実際のコマンド / パスを続ける**ので、`rm -rf` や `sudo` のような
/// より危険な語があればそちらが優先される (分類表は危険側が先)。
pub fn permission_prompt_text(tool: &ToolCallRow) -> String {
    let kind = tool.kind.unwrap_or_default();
    let head = match kind {
        ToolKind::Read => "Read file",
        ToolKind::Edit => "Edit file",
        ToolKind::Delete => "Delete file",
        ToolKind::Move => "Edit file (move)",
        ToolKind::Execute => "Run command",
        ToolKind::Fetch => "WebFetch",
        ToolKind::Search | ToolKind::Think | ToolKind::SwitchMode | ToolKind::Other => "",
    };
    let mut parts: Vec<String> = Vec::new();
    if !head.is_empty() {
        parts.push(head.to_string());
    }
    parts.push(tool.display_title());
    let loc = tool.location_line();
    if !loc.is_empty() {
        parts.push(loc);
    }
    if let Some(v) = &tool.raw_input {
        let raw = raw_input_summary(v);
        if !raw.is_empty() {
            parts.push(raw);
        }
    }
    parts.join(" / ")
}

/// `rawInput` から「人が読む 1 行」を作る (**純関数**)。
///
/// コマンド系のキーを優先して拾い、無ければ短い JSON を出す。
pub fn raw_input_summary(v: &Value) -> String {
    for key in ["command", "cmd", "file_path", "path", "url", "pattern"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    let s = v.to_string();
    if s == "{}" || s == "null" {
        String::new()
    } else {
        approvals::trim_cap(&s, 200)
    }
}

/// 保留中の権限要求 (承認キューの ID と結びつける)。
struct PendingPerm {
    /// 承認キューが払い出した ID。
    approval_id: u64,
    /// エージェントのリクエスト ID (返事に使う)。
    req_id: Value,
    options: Vec<PermissionOption>,
}

// ═══════════════════════════════════════════════════════════════════════
//  11. クライアント
// ═══════════════════════════════════════════════════════════════════════

/// いま接続がどの段にいるか。**UI にそのまま出す** (設計原則 4)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// initialize を投げた
    Initializing,
    /// session/new を投げた
    CreatingSession,
    /// プロンプト待ち
    Idle,
    /// ターン実行中
    Running,
    /// 失敗した (理由つき)。**PTY へ降格してよい**
    Failed(String),
    /// エージェントが終了した
    Ended,
}

impl Phase {
    /// UI ラベル (原文は日本語)。
    pub fn label(&self) -> String {
        match self {
            Phase::Initializing => tr("ハンドシェイク中"),
            Phase::CreatingSession => tr("セッション作成中"),
            Phase::Idle => tr("待機中"),
            Phase::Running => tr("実行中"),
            Phase::Failed(why) => trf("失敗: {why}", &[("why", why.clone())]),
            Phase::Ended => tr("終了"),
        }
    }

    /// もう何も送れない状態か。
    pub fn is_dead(&self) -> bool {
        matches!(self, Phase::Failed(_) | Phase::Ended)
    }
}

/// 送ったリクエストの種別 (返事の相関に使う)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Call {
    Initialize,
    NewSession,
    Prompt,
    Steering,
}

/// ACP 接続 1 本 = エージェント 1 体。
pub struct AcpClient {
    /// 承認キューで使う疑似セッション ID (PTY のセッション ID とは別空間)。
    pub id: u64,
    /// カタログの ID。
    pub entry_id: &'static str,
    /// UI 表示名。
    pub label: String,
    /// アイコン。
    pub icon: &'static str,
    /// 監査ログに出す実行ファイル名。
    pub bin: String,
    /// 作業フォルダ。
    pub cwd: PathBuf,
    /// npx 経由で起動したか (初回が遅い理由を UI が説明できるように)。
    pub via_npx: bool,

    child: Option<std::process::Child>,
    /// 生きている間だけ持つ PID。wait 済みなら `None` (再利用 PID を撃たない)。
    pid: Option<u32>,
    tx_out: mpsc::Sender<String>,
    rx: mpsc::Receiver<AcpEvent>,

    next_id: i64,
    inflight: HashMap<i64, Call>,
    perms: Vec<PendingPerm>,

    pub phase: Phase,
    pub info: Option<AgentInfo>,
    pub caps: AgentCapabilities,
    /// `_meta.steering.supported`。**走行中のターンへ割り込める**。
    pub steering: bool,
    pub session: Option<String>,
    pub turn: Turn,
    /// stderr の末尾 (上限つき)。
    pub log: VecDeque<String>,
    /// UI の入力欄 (プロンプト)。
    pub prompt_draft: String,
    /// UI の入力欄 (割り込み)。
    pub steer_draft: String,
}

impl AcpClient {
    /// 子プロセスを起こして `initialize` まで投げる。**ここでブロックしない。**
    pub fn start(
        entry: &'static AcpEntry,
        id: u64,
        cwd: PathBuf,
        host: Arc<FsHost>,
        ctx: Option<egui::Context>,
    ) -> Result<AcpClient, String> {
        let launch = entry.resolve()?;
        let dir = crate::pathx::launch_dir(&cwd);
        let mut cmd = crate::procx::hidden_command(&launch.program);
        cmd.args(&launch.args)
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // **独立したプロセスグループで起こす。** エージェントは自分の MCP 子
        // プロセスを起こすので、片付けはグループごと (`procx::kill_tree`)。
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            // SAFETY: fork と exec の間で呼ぶのは async-signal-safe な setsid だけ。
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            /// コンソール窓を出さない (procx と同じ値)。
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            /// 新しいプロセスグループ。taskkill /T が木を辿る足場になる。
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        }
        let mut child = cmd.spawn().map_err(|e| {
            trf(
                "{prog} を起動できません: {e}",
                &[
                    ("prog", launch.program.display().to_string()),
                    ("e", e.to_string()),
                ],
            )
        })?;
        let pid = child.id();
        let stdin = child.stdin.take().ok_or_else(|| tr("stdin を開けません"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| tr("stdout を開けません"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| tr("stderr を開けません"))?;

        let (tx_ev, rx) = mpsc::channel::<AcpEvent>();
        let (tx_out, rx_out) = mpsc::channel::<String>();

        // ライタースレッド: UI が `send` してもここで詰まらない。
        std::thread::Builder::new()
            .name("acp-write".into())
            .spawn(move || {
                let mut w = stdin;
                while let Ok(line) = rx_out.recv() {
                    if w.write_all(line.as_bytes()).is_err() || w.write_all(b"\n").is_err() {
                        break;
                    }
                    if w.flush().is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;

        // リーダースレッド: パースは全部ここ。UI スレッドではやらない。
        {
            let tx_ev = tx_ev.clone();
            let out = tx_out.clone();
            let host = host.clone();
            let ctx2 = ctx.clone();
            std::thread::Builder::new()
                .name("acp-read".into())
                .spawn(move || read_loop(BufReader::new(stdout), tx_ev, out, host, ctx2))
                .map_err(|e| e.to_string())?;
        }

        // stderr 専用スレッド: **吸い続けないとパイプが埋まってエージェントが止まる。**
        {
            let tx_ev = tx_ev.clone();
            std::thread::Builder::new()
                .name("acp-err".into())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines() {
                        let Ok(l) = line else { break };
                        if tx_ev.send(AcpEvent::Stderr(l)).is_err() {
                            break;
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
        }

        let mut c = AcpClient {
            id,
            entry_id: entry.id,
            label: entry.label.to_string(),
            icon: entry.icon,
            bin: launch
                .program
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| entry.id.to_string()),
            cwd: dir,
            via_npx: launch.via_npx,
            child: Some(child),
            pid: Some(pid),
            tx_out,
            rx,
            next_id: 0,
            inflight: HashMap::new(),
            perms: Vec::new(),
            phase: Phase::Initializing,
            info: None,
            caps: AgentCapabilities::default(),
            steering: false,
            session: None,
            turn: Turn::default(),
            log: VecDeque::new(),
            prompt_draft: String::new(),
            steer_draft: String::new(),
        };
        c.request(
            Call::Initialize,
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {
                    // terminal/* は未実装。**実装していない能力は広告しない。**
                    "fs": {"readTextFile": true, "writeTextFile": true},
                    "terminal": false
                },
                "clientInfo": {
                    "name": "zaivern-code",
                    "title": "Zaivern Code",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        );
        Ok(c)
    }

    /// リクエストを 1 本投げる (返事は `pump` で拾う)。
    fn request(&mut self, call: Call, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.inflight.insert(id, call);
        let _ = self.tx_out.send(rpc_request(id, method, params));
        id
    }

    /// 通知を 1 本投げる。
    fn notify(&self, method: &str, params: Value) {
        let _ = self.tx_out.send(rpc_notify(method, params));
    }

    /// プロンプトを送れるか。
    pub fn can_prompt(&self) -> bool {
        self.session.is_some() && self.phase == Phase::Idle
    }

    /// 割り込み (steering) を送れるか。**走行中のターンへ差し込める**。
    pub fn can_steer(&self) -> bool {
        self.steering && self.session.is_some() && !self.phase.is_dead()
    }

    /// プロンプトを 1 本送る。
    pub fn prompt(&mut self, text: &str) -> bool {
        let Some(sid) = self.session.clone() else {
            return false;
        };
        if text.trim().is_empty() || self.phase != Phase::Idle {
            return false;
        }
        self.turn.begin();
        self.phase = Phase::Running;
        self.request(
            Call::Prompt,
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type":"text","text": text}]}),
        );
        true
    }

    /// **走行中のターンへ割り込む** (`_session/steering`)。
    ///
    /// Zed 自身は「外部エージェントのターン境界を検出できない」として
    /// 外部エージェントには出していない拡張だが、Claude / Codex のアダプタは
    /// 両方とも `_meta.steering.supported` を広告している。ターンを殺さずに
    /// 「そのファイルじゃない」と言えるのは、並列コックピットでは決定的。
    pub fn steer(&mut self, text: &str) -> bool {
        let Some(sid) = self.session.clone() else {
            return false;
        };
        if text.trim().is_empty() || !self.can_steer() {
            return false;
        }
        self.request(
            Call::Steering,
            "_session/steering",
            json!({
                "sessionId": sid,
                "prompt": [{"type":"text","text": text}],
                // ターンが走っていなければ通常のプロンプトへ降格させる。
                "_meta": {"steering": {"idleBehavior": "promptRequired"}}
            }),
        );
        true
    }

    /// ターンをキャンセルする。
    ///
    /// **保留中の権限要求は全部 `cancelled` で返す**のが仕様上の義務。
    pub fn cancel(&mut self, queue: &mut ApprovalQueue) {
        let Some(sid) = self.session.clone() else {
            return;
        };
        self.notify("session/cancel", json!({"sessionId": sid}));
        for p in std::mem::take(&mut self.perms) {
            let _ = self.tx_out.send(rpc_result(
                &p.req_id,
                json!({"outcome":{"outcome":"cancelled"}}),
            ));
        }
        // 承認キューにも残さない (答えたのに待ち行列に残るのは嘘)。
        queue.forget_session(self.id);
    }

    /// 保留中の権限要求に**種別で**答える。app.rs の承認パネルから呼ばれる。
    ///
    /// 同じセッションへ複数の返事が来たときは古い順に消化する
    /// (`ApprovalQueue::apply` はまとめ処理で同じ ID を複数回返し得る)。
    pub fn answer_permission(&mut self, action: ReplyAction) -> bool {
        if self.perms.is_empty() {
            return false;
        }
        let allow = match action {
            ReplyAction::Approve => true,
            ReplyAction::Deny => false,
            ReplyAction::None => return false,
        };
        let p = self.perms.remove(0);
        let msg = match pick_option(&p.options, allow) {
            Some(opt) => {
                self.log.push_back(format!(
                    "permission #{} -> {} ({})",
                    p.approval_id,
                    opt.option_id,
                    opt.kind.as_wire()
                ));
                rpc_result(
                    &p.req_id,
                    json!({"outcome":{"outcome":"selected","optionId":opt.option_id}}),
                )
            }
            // 望む向きの選択肢が無い = 答えようがない。キャンセルで返す
            // (勝手に逆を選ばない)。
            None => {
                self.log
                    .push_back(format!("permission #{} -> cancelled", p.approval_id));
                rpc_result(&p.req_id, json!({"outcome":{"outcome":"cancelled"}}))
            }
        };
        while self.log.len() > STDERR_TAIL {
            self.log.pop_front();
        }
        self.tx_out.send(msg).is_ok()
    }

    /// この接続が承認パネルへ積んでいる要求の ID (古い順)。
    ///
    /// パネルへ「そちらで捌ける」と案内するために出す (**0 件なら何も描かない**)。
    pub fn pending_permission_ids(&self) -> Vec<u64> {
        self.perms
            .iter()
            .map(|p| p.approval_id)
            .filter(|id| *id > 0)
            .collect()
    }

    /// 受信キューを空にして状態へ畳み込む。**UI スレッドで呼ぶが、ここでは
    /// I/O もパースもしない** (パースはリーダースレッド済み)。
    ///
    /// 返すのはトースト用の `(本文, 成功か)`。
    pub fn pump(&mut self, queue: &mut ApprovalQueue) -> Vec<(String, bool)> {
        let mut toasts = Vec::new();
        loop {
            let ev = match self.rx.try_recv() {
                Ok(e) => e,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if !self.phase.is_dead() {
                        self.phase = Phase::Ended;
                    }
                    break;
                }
            };
            match ev {
                AcpEvent::Update { session, update } => {
                    // 1 接続は複数セッションを運べる。**いま見ているセッション
                    // 以外の更新を混ぜない** (混ぜると別の会話の計画が出る)。
                    if self.session.as_deref().is_some_and(|s| s != session) {
                        self.log
                            .push_back(format!("other session update: {session}"));
                        continue;
                    }
                    self.turn.apply(*update);
                }
                AcpEvent::Response { id, result } => {
                    if let Some(t) = self.on_response(id, *result) {
                        toasts.push(t);
                    }
                }
                AcpEvent::Permission { req_id, params } => {
                    self.on_permission(req_id, *params, queue, &mut toasts);
                }
                AcpEvent::Stderr(l) => {
                    self.log.push_back(l);
                    while self.log.len() > STDERR_TAIL {
                        self.log.pop_front();
                    }
                }
                AcpEvent::Note(l) => {
                    self.log.push_back(l);
                    while self.log.len() > STDERR_TAIL {
                        self.log.pop_front();
                    }
                }
                AcpEvent::Closed => {
                    if !self.phase.is_dead() {
                        self.phase = Phase::Ended;
                        toasts.push((
                            trf(
                                "🛰 {label} の ACP 接続が終了しました",
                                &[("label", self.label.clone())],
                            ),
                            false,
                        ));
                    }
                    // 答えられないまま残った要求は掃除する。
                    self.perms.clear();
                    queue.forget_session(self.id);
                }
            }
        }
        toasts
    }

    /// 自分が投げたリクエストへの返事を処理する。
    fn on_response(&mut self, id: i64, result: Result<Value, RpcError>) -> Option<(String, bool)> {
        let call = self.inflight.remove(&id)?;
        match (call, result) {
            (Call::Initialize, Ok(v)) => {
                let init: InitializeResult = serde_json::from_value(v).unwrap_or_default();
                if init.protocol_version != PROTOCOL_VERSION {
                    // 折衝: こちらが話せるのは v1 だけ。**降りられないなら降格する。**
                    self.phase = Phase::Failed(trf(
                        "プロトコル版 {v} は未対応 (このクライアントは v{ours})",
                        &[
                            ("v", init.protocol_version.to_string()),
                            ("ours", PROTOCOL_VERSION.to_string()),
                        ],
                    ));
                    return Some((
                        trf(
                            "🛰 {label}: ACP のバージョン折衝に失敗しました — PTY で起動してください",
                            &[("label", self.label.clone())],
                        ),
                        false,
                    ));
                }
                self.steering = init.steering_supported();
                self.info = init.agent_info.clone();
                self.caps = init.agent_capabilities.clone();
                if !init.auth_methods.is_empty() {
                    // 認証が要る相手は `session/new` が -32000 で落ちうる。
                    // 黙って失敗させず、何が要るかをログへ残す。
                    self.log.push_back(trf(
                        "認証方式 {n} 件が提示されました (未対応: 先に CLI 側でログインしてください)",
                        &[("n", init.auth_methods.len().to_string())],
                    ));
                }
                self.phase = Phase::CreatingSession;
                let cwd = self.cwd.display().to_string();
                self.request(
                    Call::NewSession,
                    "session/new",
                    // `mcpServers` は v1 では**必須** (空配列でよい)。
                    json!({"cwd": cwd, "mcpServers": []}),
                );
                None
            }
            (Call::NewSession, Ok(v)) => {
                // Codex は非標準の `models` を混ぜてくる。トップレベルの未知
                // フィールドは黙って捨てる。
                let sid = v
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if sid.is_empty() {
                    self.phase = Phase::Failed(tr("sessionId が返りませんでした"));
                    return Some((
                        trf(
                            "🛰 {label}: セッションを作れませんでした",
                            &[("label", self.label.clone())],
                        ),
                        false,
                    ));
                }
                self.session = Some(sid);
                self.phase = Phase::Idle;
                Some((
                    trf(
                        "🛰 {label} に ACP で接続しました ({ver})",
                        &[
                            ("label", self.label.clone()),
                            (
                                "ver",
                                self.info
                                    .as_ref()
                                    .map(|i| i.version.clone())
                                    .unwrap_or_default(),
                            ),
                        ],
                    ),
                    true,
                ))
            }
            (Call::Prompt, Ok(v)) => {
                // 実測では非標準の `usage` が同梱されて届く。stopReason だけ読む。
                let stop = v
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .map(StopReason::from_wire);
                self.turn.stop = stop;
                self.phase = Phase::Idle;
                if let Some(s) = stop {
                    self.log.push_back(format!("stopReason={}", s.as_wire()));
                }
                stop.map(|s| {
                    (
                        trf(
                            "🛰 {label}: {why}",
                            &[("label", self.label.clone()), ("why", tr(s.label()))],
                        ),
                        s == StopReason::EndTurn,
                    )
                })
            }
            (Call::Steering, Ok(_)) => Some((
                trf(
                    "🛰 {label} へ割り込みを届けました",
                    &[("label", self.label.clone())],
                ),
                true,
            )),
            (call, Err(e)) => {
                let msg = e.describe();
                self.log.push_back(msg.clone());
                match call {
                    Call::Initialize | Call::NewSession => {
                        self.phase = Phase::Failed(msg.clone());
                        Some((
                            trf(
                                "🛰 {label}: ACP を開始できません ({e}) — PTY で起動してください",
                                &[("label", self.label.clone()), ("e", msg)],
                            ),
                            false,
                        ))
                    }
                    Call::Prompt => {
                        self.phase = Phase::Idle;
                        Some((trf("🛰 プロンプトが失敗しました: {e}", &[("e", msg)]), false))
                    }
                    Call::Steering => {
                        // 割り込みが通らないだけ。ターンは殺さない。
                        self.steering = false;
                        Some((
                            trf("🛰 割り込みは受け付けられませんでした: {e}", &[("e", msg)]),
                            false,
                        ))
                    }
                }
            }
        }
    }

    /// 権限要求を**既存のネイティブ承認キューへ載せる**。
    fn on_permission(
        &mut self,
        req_id: Value,
        params: PermissionParams,
        queue: &mut ApprovalQueue,
        toasts: &mut Vec<(String, bool)>,
    ) {
        if self
            .session
            .as_deref()
            .is_some_and(|s| s != params.session_id)
        {
            self.log
                .push_back(format!("other session permission: {}", params.session_id));
        }
        let mut row = ToolCallRow {
            id: params.tool_call.tool_call_id.clone(),
            ..Default::default()
        };
        let p = params.tool_call.clone();
        p.title.apply(&mut row.title);
        p.kind.apply(&mut row.kind);
        p.status.apply(&mut row.status);
        p.locations.apply(&mut row.locations);
        p.raw_input.apply(&mut row.raw_input);
        let text = permission_prompt_text(&row);
        // 指紋はリクエスト ID から作る。**同じ内容でも重複扱いにしない**
        // (重複で握り潰すとエージェントが永久に待つ)。
        let sig = fingerprint(&req_id.to_string());
        match queue.intake(self.id, Some(&self.bin), &text, sig) {
            Verdict::Queued { id } => {
                self.perms.push(PendingPerm {
                    approval_id: id,
                    req_id,
                    options: params.options,
                });
                toasts.push((
                    trf(
                        "🛡 {label} が承認を求めています",
                        &[("label", self.label.clone())],
                    ),
                    false,
                ));
            }
            Verdict::Decided { reply, note, .. } => {
                // ポリシーが即断した。その場で答える。
                self.perms.push(PendingPerm {
                    approval_id: 0,
                    req_id,
                    options: params.options,
                });
                self.answer_permission(reply);
                toasts.push((tr(note), reply == ReplyAction::Approve));
            }
            Verdict::Duplicate => {
                // 指紋がリクエスト ID 由来なので通常は起きない。起きたら
                // 答えないと相手が止まるので、安全側 (拒否) で必ず返す。
                self.perms.push(PendingPerm {
                    approval_id: 0,
                    req_id,
                    options: params.options,
                });
                self.answer_permission(ReplyAction::Deny);
            }
        }
    }

    /// 明示的に止める。**プロセスツリーごと** (孫の MCP まで)。
    pub fn stop(&mut self) {
        if let Some(pid) = self.pid.take() {
            crate::procx::kill_tree(pid);
        }
        if let Some(mut ch) = self.child.take() {
            let _ = ch.wait();
        }
        if !self.phase.is_dead() {
            self.phase = Phase::Ended;
        }
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 文字列の安定した指紋 (FNV-1a 64)。承認キューの重複判定キーに使う。
fn fingerprint(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ═══════════════════════════════════════════════════════════════════════
//  12. 「いまどの段にいるか」 (設計原則 4 が明示的に要求している表示)
// ═══════════════════════════════════════════════════════════════════════

/// エージェントの状態をどこから知っているか。**上ほど壊れにくい。**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rung {
    /// 構造化プロトコル (ACP) — 最上段
    StructuredProtocol,
    /// 画面スクレイプ (PTY のテキスト解析) — 最下段
    ScreenScrape,
}

impl Rung {
    /// UI ラベル (原文は日本語)。
    pub fn label(self) -> &'static str {
        match self {
            Rung::StructuredProtocol => "構造化プロトコル (ACP)",
            Rung::ScreenScrape => "画面スクレイプ (PTY)",
        }
    }

    /// UI アイコン。
    pub fn icon(self) -> &'static str {
        match self {
            Rung::StructuredProtocol => "🛰",
            Rung::ScreenScrape => "🖥",
        }
    }

    /// 説明 (ホバー)。
    pub fn hint(self) -> &'static str {
        match self {
            Rung::StructuredProtocol => {
                "エージェントの状態を JSON-RPC で受け取っています。CLI の出力書式が変わっても壊れません。"
            }
            Rung::ScreenScrape => {
                "端末の文字列から状態を推測しています。CLI の出力書式が変わると壊れます。"
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  13. レイアウト (純関数。テーブルテストで固定する)
// ═══════════════════════════════════════════════════════════════════════

/// 列の間隔。
const GAP: f32 = 8.0;
/// 状態アイコン列の幅。
const STATUS_W: f32 = 22.0;
/// 種別アイコン列の幅。
const KIND_W: f32 = 22.0;
/// 場所列の幅 (`path:line`)。
const LOC_W: f32 = 180.0;
/// 見出し列の下限。ここを割るなら他の列を落とす。
const TITLE_MIN_W: f32 = 120.0;
/// 場所列を出せる行幅の下限。
const LOC_MIN_ROW_W: f32 = 420.0;

/// ツール呼び出し 1 行の列幅。**幅 0 の列は描かない。**
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ToolRow {
    pub status_w: f32,
    pub kind_w: f32,
    pub title_w: f32,
    pub loc_w: f32,
}

impl ToolRow {
    /// 描く列の合計 (列間の間隔込み)。**必ず可用幅以下**。
    pub fn total(&self) -> f32 {
        let cols = [self.status_w, self.kind_w, self.title_w, self.loc_w];
        let n = cols.iter().filter(|w| **w > 0.0).count();
        if n == 0 {
            return 0.0;
        }
        cols.iter().sum::<f32>() + GAP * (n as f32 - 1.0)
    }
}

/// ツール行の列幅を決める (**純関数**)。
///
/// 優先順は **状態 > 見出し > 種別 > 場所**。状態は「今どうなっているか」で
/// 最後まで落とさない。場所は狭いところで真っ先に消す (見出しに path が
/// 入っていることが多いため)。
pub fn tool_row_layout(avail_w: f32) -> ToolRow {
    let avail = if avail_w.is_finite() {
        avail_w.max(0.0)
    } else {
        0.0
    };
    let status_w = STATUS_W.min(avail);
    let mut rest = (avail - status_w - GAP).max(0.0);
    let mut kind_w = 0.0;
    let mut loc_w = 0.0;
    if rest >= TITLE_MIN_W + KIND_W + GAP {
        kind_w = KIND_W;
        rest -= KIND_W + GAP;
    }
    if avail >= LOC_MIN_ROW_W && rest >= TITLE_MIN_W + LOC_W + GAP {
        loc_w = LOC_W;
        rest -= LOC_W + GAP;
    }
    ToolRow {
        status_w,
        kind_w,
        title_w: rest,
        loc_w,
    }
}

/// 空状態カードの最大幅。
const EMPTY_CARD_MAX_W: f32 = 460.0;
/// 空状態カードの高さ。
const EMPTY_CARD_H: f32 = 160.0;

/// 空状態カードの矩形 (**純関数**)。常に `avail` の中央 1 枚で、必ず収まる。
pub fn empty_card(avail: egui::Rect) -> egui::Rect {
    let aw = avail.width().max(0.0);
    let ah = avail.height().max(0.0);
    let w = (aw - space::LG * 2.0).clamp(0.0, EMPTY_CARD_MAX_W).min(aw);
    let h = EMPTY_CARD_H.min(ah);
    let x = avail.left() + (aw - w) * 0.5;
    let y = avail.top() + (ah - h) * 0.5;
    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))
}

// ═══════════════════════════════════════════════════════════════════════
//  14. マネージャ + UI
// ═══════════════════════════════════════════════════════════════════════

/// パネルが app へ返す要求 (I/O は描画の外でやる)。
#[derive(Clone, PartialEq, Eq, Debug)]
enum AcpAction {
    Start(&'static str),
    Prompt(usize),
    Steer(usize),
    Cancel(usize),
    Close(usize),
}

/// ACP 接続をまとめて持つ。`ZaivernApp` が 1 個持つ。
pub struct AcpManager {
    clients: Vec<AcpClient>,
    next_id: u64,
    /// パネルを出しているか。
    pub open: bool,
    /// 選択中の接続。
    sel: usize,
    host: Arc<FsHost>,
    /// 未保存バッファの署名 `(パス, 版数)`。**変わらない限り本文を読まない。**
    unsaved_sig: Vec<(PathBuf, u64)>,
}

/// 承認キュー用の疑似セッション ID の起点。
///
/// PTY のセッション ID (`AgentManager::next_id` は 1 から連番) と**空間を分ける**。
/// 分けておかないと、承認キューの返事が別のセッションへ届く。
pub const ACP_SESSION_ID_BASE: u64 = 1 << 48;

impl Default for AcpManager {
    fn default() -> Self {
        AcpManager {
            clients: Vec::new(),
            next_id: ACP_SESSION_ID_BASE,
            open: false,
            sel: 0,
            host: Arc::new(FsHost::default()),
            unsaved_sig: Vec::new(),
        }
    }
}

impl AcpManager {
    /// 接続が 1 本も無いか (**0 本なら UI も同期も 1 ピクセルも動かない**)。
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// **未保存のエディタバッファ**を ACP へ公開する。
    ///
    /// `fs/read_text_file` がディスクではなく「いま画面に見えている内容」を
    /// 返せるようになる — エディタを持っているクライアントだけが出せる価値。
    ///
    /// `items` は遅延イテレータで渡すこと。**署名 (パス, 版数) が変わらない
    /// フレームでは本文を 1 バイトも読まない** (設計原則 3)。
    pub fn sync_unsaved<'a>(&mut self, items: impl Iterator<Item = (&'a Path, u64, &'a str)>) {
        if self.clients.is_empty() {
            // 接続が無いなら公開の意味が無い。溜まっていたら捨てる。
            if !self.unsaved_sig.is_empty() {
                self.unsaved_sig.clear();
                self.host.clear_unsaved();
            }
            return;
        }
        // ここでは **&str のまま**集める (本文のコピーはまだしない)。
        let pending: Vec<(&Path, u64, &str)> = items.collect();
        let sig: Vec<(PathBuf, u64)> = pending
            .iter()
            .map(|(p, rev, _)| (p.to_path_buf(), *rev))
            .collect();
        if sig == self.unsaved_sig {
            return;
        }
        self.unsaved_sig = sig;
        self.host.replace_unsaved(
            pending
                .into_iter()
                .map(|(p, _, t)| (p.to_path_buf(), t.to_string()))
                .collect(),
        );
    }

    /// 承認パネルからの返事を ACP へ流す。
    ///
    /// `session_id` が ACP のものでなければ `false` (呼び出し側は PTY 経路へ落とす)。
    pub fn reply(&mut self, session_id: u64, action: ReplyAction) -> bool {
        let Some(c) = self.clients.iter_mut().find(|c| c.id == session_id) else {
            return false;
        };
        c.answer_permission(action)
    }

    /// 1 フレーム分: 受信を畳み込み、開いていればパネルを描く。
    ///
    /// 接続が 0 本ならほぼ何もしない (アイドルのコストはゼロ)。
    pub fn frame(
        &mut self,
        ctx: &egui::Context,
        theme: &Theme,
        queue: &mut ApprovalQueue,
        roots: &[PathBuf],
        cwd: &Path,
    ) -> Vec<(String, bool)> {
        let mut toasts = Vec::new();
        if !self.is_empty() {
            self.host.set_roots(roots);
            for c in &mut self.clients {
                toasts.extend(c.pump(queue));
            }
        }
        if !self.open {
            return toasts;
        }
        let mut open = self.open;
        let mut actions: Vec<AcpAction> = Vec::new();
        {
            let clients = &mut self.clients;
            let sel = &mut self.sel;
            let host = &self.host;
            egui::Window::new(tr("🛰 ACP エージェント"))
                .open(&mut open)
                .default_width(560.0)
                .default_height(460.0)
                .resizable(true)
                .show(ctx, |ui| {
                    actions = panel_ui(ui, theme, clients, sel, host);
                });
        }
        self.open = open;
        for a in actions {
            match a {
                AcpAction::Start(id) => {
                    if let Some(t) = self.start(id, cwd.to_path_buf(), Some(ctx.clone())) {
                        toasts.push(t);
                    }
                }
                AcpAction::Prompt(i) => {
                    if let Some(c) = self.clients.get_mut(i) {
                        let text = std::mem::take(&mut c.prompt_draft);
                        if !c.prompt(&text) {
                            c.prompt_draft = text;
                        }
                    }
                }
                AcpAction::Steer(i) => {
                    if let Some(c) = self.clients.get_mut(i) {
                        let text = std::mem::take(&mut c.steer_draft);
                        if !c.steer(&text) {
                            c.steer_draft = text;
                        }
                    }
                }
                AcpAction::Cancel(i) => {
                    if let Some(c) = self.clients.get_mut(i) {
                        c.cancel(queue);
                    }
                }
                AcpAction::Close(i) => {
                    if i < self.clients.len() {
                        let mut c = self.clients.remove(i);
                        c.cancel(queue);
                        c.stop();
                        queue.forget_session(c.id);
                        self.sel = self.sel.min(self.clients.len().saturating_sub(1));
                    }
                }
            }
        }
        toasts
    }

    /// カタログ ID から 1 本起こす。失敗したら**理由をトーストで返す**
    /// (死んだペインをユーザーに残さない)。
    pub fn start(
        &mut self,
        entry_id: &str,
        cwd: PathBuf,
        ctx: Option<egui::Context>,
    ) -> Option<(String, bool)> {
        let entry = crate::agents::ACP_CATALOG
            .iter()
            .find(|e| e.id == entry_id)?;
        let id = self.next_id;
        self.next_id += 1;
        match AcpClient::start(entry, id, cwd, self.host.clone(), ctx) {
            Ok(c) => {
                let via = c.via_npx;
                self.clients.push(c);
                self.sel = self.clients.len() - 1;
                self.open = true;
                Some((
                    if via {
                        trf(
                            "🛰 {label} を npx 経由で起動しています (初回は取得に時間がかかります)",
                            &[("label", entry.label.to_string())],
                        )
                    } else {
                        trf(
                            "🛰 {label} を ACP で起動しました",
                            &[("label", entry.label.to_string())],
                        )
                    },
                    true,
                ))
            }
            Err(e) => Some((
                trf(
                    "🛰 {label} を ACP で起動できません: {e} — 従来どおり PTY で起動してください",
                    &[("label", entry.label.to_string()), ("e", e)],
                ),
                false,
            )),
        }
    }
}

/// パネル本体。**状態は借り物で、ここでは I/O も spawn もしない。**
fn panel_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    clients: &mut [AcpClient],
    sel: &mut usize,
    host: &FsHost,
) -> Vec<AcpAction> {
    let mut acts: Vec<AcpAction> = Vec::new();

    // ── いまどの段にいるか (設計原則 4) ──
    let rung = if clients.is_empty() {
        Rung::ScreenScrape
    } else {
        Rung::StructuredProtocol
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("{} {}", rung.icon(), tr(rung.label())))
                .size(12.0)
                .strong()
                .color(if rung == Rung::StructuredProtocol {
                    theme.ok
                } else {
                    theme.text_dim
                }),
        )
        .on_hover_text(tr(rung.hint()));
        let n = host.unsaved_len();
        if n > 0 {
            ui.label(
                RichText::new(trf(
                    "未保存バッファ {n} 件を共有中",
                    &[("n", n.to_string())],
                ))
                .size(11.0)
                .color(theme.text_dim),
            )
            .on_hover_text(tr(
                "fs/read_text_file にディスクではなく編集中の内容を返します",
            ));
        }
    });
    ui.separator();

    if clients.is_empty() {
        empty_state(ui, theme, &mut acts);
        return acts;
    }

    // ── 接続の選択 ──
    if clients.len() > 1 {
        ui.horizontal_wrapped(|ui| {
            for (i, c) in clients.iter().enumerate() {
                let on = *sel == i;
                if ui
                    .selectable_label(on, format!("{} {}", c.icon, c.label))
                    .clicked()
                {
                    *sel = i;
                }
            }
        });
    }
    let idx = (*sel).min(clients.len() - 1);
    *sel = idx;
    let c = &mut clients[idx];

    // ── 見出し: 相手 / 段階 / 能力 ──
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("{} {}", c.icon, c.label))
                .strong()
                .color(theme.text),
        )
        .on_hover_text(trf("カタログ ID: {id}", &[("id", c.entry_id.to_string())]));
        if let Some(i) = &c.info {
            ui.label(
                RichText::new(i.display_line())
                    .size(11.0)
                    .color(theme.text_dim),
            );
        }
        let ph = c.phase.clone();
        ui.label(RichText::new(ph.label()).size(11.0).color(match &ph {
            Phase::Failed(_) => theme.err,
            Phase::Running => theme.accent,
            Phase::Ended => theme.text_dim,
            _ => theme.ok,
        }));
        if c.steering {
            ui.label(
                RichText::new(tr("⚡ 割り込み対応"))
                    .size(11.0)
                    .color(theme.ok),
            )
            .on_hover_text(tr(
                "_session/steering — 走行中のターンを殺さずに訂正を差し込めます",
            ));
        }
        if c.caps.session_supports("resume") {
            ui.label(
                RichText::new(tr("↻ 再開対応"))
                    .size(11.0)
                    .color(theme.text_dim),
            );
        }
        if c.caps.load_session {
            ui.label(
                RichText::new(tr("📂 読込対応"))
                    .size(11.0)
                    .color(theme.text_dim),
            );
        }
        if c.caps.supports_image() {
            ui.label(
                RichText::new(tr("🖼 画像対応"))
                    .size(11.0)
                    .color(theme.text_dim),
            );
        }
    });
    if let Some(t) = &c.turn.title {
        ui.label(RichText::new(t.clone()).size(11.5).color(theme.text));
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(c.cwd.display().to_string())
                .size(10.5)
                .color(theme.text_dim),
        );
        if let Some(m) = &c.turn.mode {
            ui.label(
                RichText::new(trf("モード: {m}", &[("m", m.clone())]))
                    .size(10.5)
                    .color(theme.text_dim),
            );
        }
    });
    // ── 承認待ち (0 件なら 1 ピクセルも描かない) ──
    let waiting = c.pending_permission_ids();
    if !waiting.is_empty() {
        let ids = waiting
            .iter()
            .map(|i| format!("#{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        ui.label(
            RichText::new(trf(
                "🛡 承認待ち {ids} — 承認キューで捌けます",
                &[("ids", ids)],
            ))
            .size(11.0)
            .color(theme.warn),
        );
    }

    // ── 操作列 (狭いときは折り返す) ──
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(c.phase == Phase::Running, egui::Button::new(tr("■ 中止")))
            .on_hover_text(tr(
                "session/cancel を送り、保留中の承認は cancelled で返します",
            ))
            .clicked()
        {
            acts.push(AcpAction::Cancel(idx));
        }
        if ui.button(tr("✕ 切断")).clicked() {
            acts.push(AcpAction::Close(idx));
        }
    });
    ui.separator();

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(260.0)
        .show(ui, |ui| {
            // ── 計画 / TODO (空なら見出しごと出さない) ──
            if !c.turn.plan.is_empty() {
                ui.label(
                    RichText::new(tr("📋 計画"))
                        .size(12.0)
                        .strong()
                        .color(theme.text),
                );
                for e in &c.turn.plan {
                    let col = match e.status {
                        PlanEntryStatus::Completed => theme.text_dim,
                        PlanEntryStatus::InProgress => theme.accent,
                        PlanEntryStatus::Pending => theme.text,
                    };
                    ui.label(
                        RichText::new(format!("{} {}", e.status.icon(), e.content))
                            .size(11.5)
                            .color(col),
                    )
                    .on_hover_text(format!(
                        "{}\n{} / {}",
                        e.content,
                        e.status.as_wire(),
                        e.priority.as_wire()
                    ));
                }
                ui.add_space(space::XS);
            }

            // ── ツール呼び出し ──
            if !c.turn.tools.is_empty() {
                ui.label(
                    RichText::new(tr("🔧 ツール呼び出し"))
                        .size(12.0)
                        .strong()
                        .color(theme.text),
                );
                let l = tool_row_layout(ui.available_width());
                for t in &c.turn.tools {
                    tool_row(ui, theme, t, &l);
                }
                ui.add_space(space::XS);
            }

            // ── 本文 ──
            if !c.turn.message.is_empty() {
                ui.label(
                    RichText::new(tr("💬 応答"))
                        .size(12.0)
                        .strong()
                        .color(theme.text),
                );
                ui.label(
                    RichText::new(c.turn.message.clone())
                        .size(11.5)
                        .color(theme.text),
                );
                ui.add_space(space::XS);
            }

            // ── 思考 (畳んでおく。開くのは明示操作) ──
            if !c.turn.thought.is_empty() {
                ui.push_id("acp-thought", |ui| {
                    egui::CollapsingHeader::new(tr("💭 思考"))
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(c.turn.thought.clone())
                                    .size(11.0)
                                    .color(theme.text_dim),
                            );
                        });
                });
            }

            // ── 使えるスラッシュコマンド ──
            if !c.turn.commands.is_empty() {
                ui.push_id("acp-cmds", |ui| {
                    egui::CollapsingHeader::new(trf(
                        "⌗ コマンド {n} 件",
                        &[("n", c.turn.commands.len().to_string())],
                    ))
                    .default_open(false)
                    .show(ui, |ui| {
                        for cmd in &c.turn.commands {
                            ui.label(RichText::new(cmd.name.clone()).size(11.0).color(theme.text))
                                .on_hover_text(cmd.description.clone());
                        }
                    });
                });
            }

            // ── stderr (エージェントのログ。畳んでおく) ──
            if !c.log.is_empty() {
                ui.push_id("acp-log", |ui| {
                    egui::CollapsingHeader::new(trf(
                        "🪵 ログ {n} 行",
                        &[("n", c.log.len().to_string())],
                    ))
                    .default_open(false)
                    .show(ui, |ui| {
                        for l in c.log.iter().rev().take(40) {
                            ui.label(RichText::new(l.clone()).size(10.5).color(theme.text_dim));
                        }
                    });
                });
            }
        });

    // ── 使用量 / 終了理由 ──
    ui.horizontal_wrapped(|ui| {
        if let Some(u) = &c.turn.usage {
            let mut line = trf(
                "🔢 {used} / {size}",
                &[("used", u.used.to_string()), ("size", u.size.to_string())],
            );
            if let Some(cost) = &u.cost {
                line.push_str(&format!(" · {:.4} {}", cost.amount, cost.currency));
            }
            ui.label(RichText::new(line).size(11.0).color(theme.text_dim));
        }
        if let Some(s) = c.turn.stop {
            ui.label(
                RichText::new(tr(s.label()))
                    .size(11.0)
                    .color(theme.text_dim),
            );
        }
    });

    // ── プロンプト ──
    ui.separator();
    ui.horizontal(|ui| {
        let w = (ui.available_width() - 110.0).max(80.0);
        ui.add_sized(
            [w, 22.0],
            egui::TextEdit::singleline(&mut c.prompt_draft)
                .hint_text(tr("プロンプト (ACP で送る)")),
        );
        if ui
            .add_enabled(c.can_prompt(), egui::Button::new(tr("送信")))
            .clicked()
        {
            acts.push(AcpAction::Prompt(idx));
        }
    });
    // ── 割り込み (走行中のターンへ差し込む) ──
    if c.steering {
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 110.0).max(80.0);
            ui.add_sized(
                [w, 22.0],
                egui::TextEdit::singleline(&mut c.steer_draft)
                    .hint_text(tr("割り込み (ターンを殺さずに訂正)")),
            );
            if ui
                .add_enabled(c.can_steer(), egui::Button::new(tr("割り込み")))
                .on_hover_text(tr(
                    "_session/steering — 走行中のターンへメッセージを注入します",
                ))
                .clicked()
            {
                acts.push(AcpAction::Steer(idx));
            }
        });
    }
    acts
}

/// ツール呼び出し 1 行。列は必ず可用幅に収まる ([`ToolRow::total`] の不変条件)。
fn tool_row(ui: &mut egui::Ui, theme: &Theme, t: &ToolCallRow, l: &ToolRow) {
    let status = t.status.unwrap_or_default();
    let col = match status {
        ToolCallStatus::Failed => theme.err,
        ToolCallStatus::Completed => theme.ok,
        ToolCallStatus::InProgress => theme.accent,
        ToolCallStatus::Pending => theme.text_dim,
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = GAP;
        ui.set_width(l.total().min(ui.available_width()));
        ui.add_sized(
            [l.status_w, 18.0],
            egui::Label::new(RichText::new(status.icon()).size(11.5).color(col)),
        )
        .on_hover_text(format!("{} ({})", tr(status.label()), status.as_wire()));
        if l.kind_w > 0.0 {
            let k = t.kind.unwrap_or_default();
            ui.add_sized(
                [l.kind_w, 18.0],
                egui::Label::new(RichText::new(k.icon()).size(11.5)),
            )
            .on_hover_text(k.as_wire());
        }
        let title = t.display_title();
        ui.add_sized(
            [l.title_w, 18.0],
            egui::Label::new(
                RichText::new(crate::mcp::ellipsize(&title, 120))
                    .size(11.5)
                    .color(theme.text),
            )
            .truncate(),
        )
        .on_hover_text(t.detail_lines().join("\n"));
        if l.loc_w > 0.0 {
            let loc = t.location_line();
            if !loc.is_empty() {
                ui.add_sized(
                    [l.loc_w, 18.0],
                    egui::Label::new(
                        RichText::new(crate::mcp::ellipsize(&loc, 60))
                            .size(10.5)
                            .color(theme.text_dim),
                    )
                    .truncate(),
                )
                .on_hover_text(loc);
            }
        }
    });
}

/// 空状態。**可用領域の中央に 1 枚**のカードで、起動できる相手だけ出す。
fn empty_state(ui: &mut egui::Ui, theme: &Theme, acts: &mut Vec<AcpAction>) {
    let avail = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    let card = empty_card(avail);
    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(card), |ui| {
        egui::Frame::none()
            .fill(theme.panel_alt)
            .stroke(egui::Stroke::new(1.0_f32, theme.border))
            .rounding(egui::Rounding::same(10.0))
            .inner_margin(egui::Margin::same(space::MD))
            .show(ui, |ui| {
                ui.set_width((card.width() - space::MD * 2.0).max(0.0));
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(tr("🛰 ACP エージェントに接続していません"))
                            .size(14.0)
                            .color(theme.text),
                    );
                    ui.label(
                        RichText::new(tr(
                            "構造化プロトコルで駆動すると、CLI の出力書式が変わっても状態が壊れません",
                        ))
                        .size(11.0)
                        .color(theme.text_dim),
                    )
                    .on_hover_text(trf(
                        "一覧の出典: {url}",
                        &[("url", REGISTRY_URL.to_string())],
                    ));
                    ui.add_space(space::SM);
                    ui.horizontal_wrapped(|ui| {
                        for e in crate::agents::ACP_CATALOG {
                            catalog_button(ui, e, acts);
                        }
                    });
                });
            });
    });
}

/// カタログ 1 件のボタン。**起動できないものは押せない形で理由ごと出す**
/// (起動してハングさせない)。
fn catalog_button(ui: &mut egui::Ui, e: &'static AcpEntry, acts: &mut Vec<AcpAction>) {
    let text = format!("{} {}", e.icon, e.label);
    match e.resolve() {
        Ok(l) => {
            let mut hint = trf(
                "{label} を ACP で起動する\n{cmd}",
                &[
                    ("label", e.label.to_string()),
                    (
                        "cmd",
                        format!("{} {}", l.program.display(), l.args.join(" ")),
                    ),
                ],
            );
            if !e.note.is_empty() {
                hint.push('\n');
                hint.push_str(&tr(e.note));
            }
            if ui.button(text).on_hover_text(hint).clicked() {
                acts.push(AcpAction::Start(e.id));
            }
        }
        Err(why) => {
            ui.add_enabled(false, egui::Button::new(text))
                .on_disabled_hover_text(why);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  テスト
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── フレーミングと振り分け ──────────────────────────────────

    #[test]
    fn 送信するJSONに生の改行が入らない() {
        // 改行を含む本文でも、1 メッセージ = 1 行の規約は破られない。
        let line = rpc_request(1, "session/prompt", json!({"text":"a\nb\r\nc"}));
        assert!(!line.contains('\n'), "生の改行が混ざった: {line}");
        assert!(line.contains("\\n"), "エスケープされていない: {line}");
    }

    #[test]
    fn 受信行はリクエスト_レスポンス_通知の3系統へ振り分く() {
        let table: &[(&str, &str)] = &[
            // 実物の形をそのまま使う
            (
                r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1}}"#,
                "response",
            ),
            (
                r#"{"jsonrpc":"2.0","id":5,"method":"session/request_permission","params":{}}"#,
                "request",
            ),
            (
                r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s"}}"#,
                "notification",
            ),
            (
                r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"nope"}}"#,
                "response",
            ),
            ("これは JSON ではない", "malformed"),
            ("", "malformed"),
            (r#"{"jsonrpc":"2.0"}"#, "malformed"),
        ];
        for (line, want) in table {
            let got = match classify(line) {
                Incoming::Request { .. } => "request",
                Incoming::Response { .. } => "response",
                Incoming::Notification { .. } => "notification",
                Incoming::Malformed(_) => "malformed",
            };
            assert_eq!(&got, want, "{line}");
        }
    }

    #[test]
    fn エラーレスポンスはコードとメッセージを保つ() {
        let Incoming::Response { id, result } =
            classify(r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"auth"}}"#)
        else {
            panic!("レスポンスとして読めない");
        };
        assert_eq!(id, 3);
        let e = result.expect_err("エラーのはず");
        assert_eq!(e.code, ERR_AUTH_REQUIRED);
        assert!(e.describe().contains("認証が必要"), "{}", e.describe());
    }

    // ── パッチ意味論 (省略 ≠ null ≠ 値) ──────────────────────────

    #[test]
    fn パッチは省略とnullと値を区別する() {
        #[derive(Deserialize)]
        struct T {
            #[serde(default)]
            a: Patch<String>,
        }
        let absent: T = serde_json::from_str("{}").expect("省略");
        assert_eq!(absent.a, Patch::Absent);
        let null: T = serde_json::from_str(r#"{"a":null}"#).expect("null");
        assert_eq!(null.a, Patch::Null);
        let set: T = serde_json::from_str(r#"{"a":"x"}"#).expect("値");
        assert_eq!(set.a, Patch::Set("x".to_string()));

        // 適用の効き方
        let mut dst = Some("old".to_string());
        Patch::<String>::Absent.apply(&mut dst);
        assert_eq!(dst.as_deref(), Some("old"), "省略は触らない");
        Patch::Set("new".to_string()).apply(&mut dst);
        assert_eq!(dst.as_deref(), Some("new"));
        Patch::<String>::Null.apply(&mut dst);
        assert_eq!(dst, None, "null は消す");
    }

    // ── session/update の全 11 種 ───────────────────────────────

    fn parse_update(raw: &str) -> SessionUpdate {
        serde_json::from_str(raw).unwrap_or_else(|e| panic!("{raw}\n{e}"))
    }

    #[test]
    fn v1の11種類のsession_updateを全部読める() {
        let table: &[(&str, &str)] = &[
            (
                r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hi"}}"#,
                "user",
            ),
            (
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"I"}}"#,
                "agent",
            ),
            (
                r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hmm"}}"#,
                "thought",
            ),
            (
                r#"{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Terminal","kind":"execute","status":"pending","rawInput":{}}"#,
                "tool_call",
            ),
            (
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"t1","title":"echo hi","status":"in_progress"}"#,
                "tool_call_update",
            ),
            (
                r#"{"sessionUpdate":"plan","entries":[{"content":"調べる","status":"in_progress","priority":"high"}]}"#,
                "plan",
            ),
            (
                r#"{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"/init","description":"d"}]}"#,
                "commands",
            ),
            (
                r#"{"sessionUpdate":"current_mode_update","currentModeId":"default"}"#,
                "mode",
            ),
            (
                r#"{"sessionUpdate":"config_option_update","configOptions":[]}"#,
                "config",
            ),
            (
                r#"{"sessionUpdate":"session_info_update","title":"直したい","updatedAt":"2026-08-09T00:00:00Z"}"#,
                "info",
            ),
            (
                r#"{"sessionUpdate":"usage_update","used":1200,"size":200000,"cost":{"amount":0.01,"currency":"USD"}}"#,
                "usage",
            ),
        ];
        for (raw, tag) in table {
            let u = parse_update(raw);
            assert!(
                !matches!(u, SessionUpdate::Unknown),
                "{tag} が Unknown へ落ちた: {raw}"
            );
        }
    }

    #[test]
    fn 知らない種別はUnknownになって落ちない() {
        let u = parse_update(r#"{"sessionUpdate":"future_thing","whatever":1}"#);
        assert!(matches!(u, SessionUpdate::Unknown));
        // 未知の余分なフィールドがあっても既知の種別は読める
        let u =
            parse_update(r#"{"sessionUpdate":"plan","entries":[],"_meta":{"x":1},"未知":"値"}"#);
        assert!(matches!(u, SessionUpdate::Plan { .. }));
    }

    #[test]
    fn 列挙値はワイヤ表記と往復し未知は既定へ落ちる() {
        for k in [
            ToolKind::Read,
            ToolKind::Edit,
            ToolKind::Delete,
            ToolKind::Move,
            ToolKind::Search,
            ToolKind::Execute,
            ToolKind::Think,
            ToolKind::Fetch,
            ToolKind::SwitchMode,
            ToolKind::Other,
        ] {
            assert_eq!(ToolKind::from_wire(k.as_wire()), k);
        }
        assert_eq!(ToolKind::from_wire("未来の種別"), ToolKind::Other);
        assert_eq!(ToolCallStatus::from_wire("failed"), ToolCallStatus::Failed);
        assert_eq!(StopReason::from_wire("cancelled"), StopReason::Cancelled);
        assert_eq!(StopReason::from_wire("なにこれ"), StopReason::EndTurn);
        assert_eq!(
            PermissionOptionKind::from_wire("allow_always"),
            PermissionOptionKind::AllowAlways
        );
    }

    // ── 実測ターンの再現 ─────────────────────────────────────────

    #[test]
    fn toolcallのタイトルは後から訂正されrawInputは少しずつ埋まる() {
        let mut turn = Turn::default();
        // 実測: 総称のタイトル + 空の rawInput で先に届く
        turn.apply(parse_update(
            r#"{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Terminal","kind":"execute","status":"pending","rawInput":{}}"#,
        ));
        assert_eq!(turn.tools.len(), 1);
        assert_eq!(turn.tools[0].display_title(), "Terminal");
        // 引数のストリーミングが終わってから訂正が来る
        turn.apply(parse_update(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"t1","title":"echo hi","status":"in_progress","rawInput":{"command":"echo hi"}}"#,
        ));
        assert_eq!(turn.tools.len(), 1, "行が増えてはいけない (upsert)");
        assert_eq!(
            turn.tools[0].display_title(),
            "echo hi",
            "訂正が効いていない"
        );
        assert_eq!(
            turn.tools[0].status,
            Some(ToolCallStatus::InProgress),
            "状態遷移が反映されていない"
        );
        assert_eq!(
            turn.tools[0]
                .raw_input
                .as_ref()
                .and_then(|v| v.get("command"))
                .and_then(Value::as_str),
            Some("echo hi")
        );
        // 空の rawInput が後から来ても、埋まった値を消さない
        turn.apply(parse_update(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"t1","rawInput":{}}"#,
        ));
        assert_eq!(
            turn.tools[0]
                .raw_input
                .as_ref()
                .and_then(|v| v.get("command"))
                .and_then(Value::as_str),
            Some("echo hi"),
            "空オブジェクトで上書きしてしまった"
        );
        // 触っていないフィールドは残る
        assert_eq!(turn.tools[0].kind, Some(ToolKind::Execute));
    }

    #[test]
    fn チャンクは連結され計画は丸ごと置き換わる() {
        let mut turn = Turn::default();
        for c in ["I", "'ll run", " it"] {
            turn.apply(parse_update(&format!(
                r#"{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{c}"}}}}"#
            )));
        }
        assert_eq!(turn.message, "I'll run it");
        turn.apply(parse_update(
            r#"{"sessionUpdate":"plan","entries":[{"content":"A","status":"pending","priority":"high"},{"content":"B","status":"pending","priority":"low"}]}"#,
        ));
        assert_eq!(turn.plan.len(), 2);
        turn.apply(parse_update(
            r#"{"sessionUpdate":"plan","entries":[{"content":"C","status":"completed","priority":"medium"}]}"#,
        ));
        assert_eq!(turn.plan.len(), 1, "plan は毎回まるごと置き換え");
        assert_eq!(turn.plan[0].content, "C");
    }

    #[test]
    fn usage_updateは合体され再描画を撃ち続けない() {
        let mut co = Coalescer::default();
        let t0 = Instant::now();
        // 実測: ツール 2 回のターンで約 7 回飛ぶ
        assert!(co.allow(RepaintClass::Usage, t0), "1 回目は描く");
        for i in 1..7 {
            let t = t0 + Duration::from_millis(10 * i);
            assert!(
                !co.allow(RepaintClass::Usage, t),
                "{i} 回目まで間引かれるはず"
            );
        }
        assert!(
            co.allow(RepaintClass::Usage, t0 + Duration::from_millis(600)),
            "間隔を過ぎたら描く"
        );
        // ツール状態は間引かない (見逃すと困る)
        assert!(co.allow(RepaintClass::Immediate, t0));
        assert!(co.allow(RepaintClass::Immediate, t0));
        // 未知の種別は 1 フレームも起こさない
        assert!(!co.allow(RepaintClass::Silent, t0));
    }

    #[test]
    fn 更新の種別ごとに再描画の間引きが決まる() {
        let usage = parse_update(r#"{"sessionUpdate":"usage_update","used":1,"size":2}"#);
        assert_eq!(usage.repaint_class(), RepaintClass::Usage);
        let chunk = parse_update(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"a"}}"#,
        );
        assert_eq!(chunk.repaint_class(), RepaintClass::Chunk);
        let tool = parse_update(r#"{"sessionUpdate":"tool_call","toolCallId":"t"}"#);
        assert_eq!(tool.repaint_class(), RepaintClass::Immediate);
        assert_eq!(
            parse_update(r#"{"sessionUpdate":"???"}"#).repaint_class(),
            RepaintClass::Silent
        );
    }

    // ── 権限要求 (最重要の罠) ────────────────────────────────────

    /// claude-agent-acp の実測。**`reject_once` が先頭**。
    fn real_options() -> Vec<PermissionOption> {
        serde_json::from_str(
            r#"[{"optionId":"reject","name":"Deny","kind":"reject_once"},
                {"optionId":"allow","name":"Allow Once","kind":"allow_once"},
                {"optionId":"allow_always","name":"Always Allow","kind":"allow_always"}]"#,
        )
        .expect("実測の選択肢")
    }

    #[test]
    fn 権限の選択はインデックスではなく種別で行う() {
        let opts = real_options();
        // 素朴な options[0] は reject。ここを踏むと編集を拒否してしまう。
        assert_eq!(opts[0].kind, PermissionOptionKind::RejectOnce);
        let allow = pick_option(&opts, true).expect("許可の選択肢がある");
        assert_eq!(allow.option_id, "allow");
        let deny = pick_option(&opts, false).expect("拒否の選択肢がある");
        assert_eq!(deny.option_id, "reject");
        // allow_once が無ければ allow_always へ落ちる
        let only_always: Vec<PermissionOption> = opts
            .iter()
            .filter(|o| o.kind != PermissionOptionKind::AllowOnce)
            .cloned()
            .collect();
        assert_eq!(
            pick_option(&only_always, true).map(|o| o.option_id.as_str()),
            Some("allow_always")
        );
        // 望む向きが 1 つも無ければ None (勝手に逆を選ばない)
        let deny_only: Vec<PermissionOption> = opts
            .iter()
            .filter(|o| o.kind == PermissionOptionKind::RejectOnce)
            .cloned()
            .collect();
        assert!(pick_option(&deny_only, true).is_none());
    }

    #[test]
    fn 権限要求は既存の承認種別へ翻訳される() {
        let table: &[(&str, approvals::ApprovalKind)] = &[
            // 構造化された kind が土台。危険な語があればそちらが勝つ。
            (
                r#"{"toolCallId":"t","title":"Terminal","kind":"execute","rawInput":{"command":"echo hi"}}"#,
                approvals::ApprovalKind::ShellCommand,
            ),
            (
                r#"{"toolCallId":"t","title":"Terminal","kind":"execute","rawInput":{"command":"rm -rf build"}}"#,
                approvals::ApprovalKind::FileDelete,
            ),
            (
                r#"{"toolCallId":"t","title":"Terminal","kind":"execute","rawInput":{"command":"npm install left-pad"}}"#,
                approvals::ApprovalKind::PackageInstall,
            ),
            (
                r#"{"toolCallId":"t","title":"Terminal","kind":"execute","rawInput":{"command":"sudo rm /etc/hosts"}}"#,
                approvals::ApprovalKind::Privilege,
            ),
            (
                r#"{"toolCallId":"t","title":"src/app.rs","kind":"edit","locations":[{"path":"/w/src/app.rs","line":3}]}"#,
                approvals::ApprovalKind::FileWrite,
            ),
            (
                r#"{"toolCallId":"t","title":"README.md","kind":"read"}"#,
                approvals::ApprovalKind::FileRead,
            ),
            (
                r#"{"toolCallId":"t","title":"https://example.com","kind":"fetch"}"#,
                approvals::ApprovalKind::NetworkAccess,
            ),
        ];
        for (raw, want) in table {
            let p: ToolCallPatch = serde_json::from_str(raw).expect("ツール呼び出し");
            let mut row = ToolCallRow {
                id: p.tool_call_id.clone(),
                ..Default::default()
            };
            p.title.clone().apply(&mut row.title);
            p.kind.clone().apply(&mut row.kind);
            p.locations.clone().apply(&mut row.locations);
            p.raw_input.clone().apply(&mut row.raw_input);
            let text = permission_prompt_text(&row);
            let got = approvals::classify(&text, Some("claude"));
            assert_eq!(&got, want, "{text}");
        }
    }

    #[test]
    fn 権限昇格は自動承認できない種別へ落ちる() {
        let p: ToolCallPatch = serde_json::from_str(
            r#"{"toolCallId":"t","title":"Terminal","kind":"execute","rawInput":{"command":"sudo installer"}}"#,
        )
        .expect("ツール呼び出し");
        let mut row = ToolCallRow::default();
        p.title.clone().apply(&mut row.title);
        p.kind.clone().apply(&mut row.kind);
        p.raw_input.clone().apply(&mut row.raw_input);
        let kind = approvals::classify(&permission_prompt_text(&row), None);
        assert_eq!(kind, approvals::ApprovalKind::Privilege);
        assert!(
            !kind.auto_approvable(),
            "権限昇格が自動承認可能になっている"
        );
    }

    #[test]
    fn 権限の返し方は仕様どおりの封筒になる() {
        let opts = real_options();
        let allow = pick_option(&opts, true).expect("許可");
        let line = rpc_result(
            &json!(5),
            json!({"outcome":{"outcome":"selected","optionId":allow.option_id}}),
        );
        let v: Value = serde_json::from_str(&line).expect("JSON");
        assert_eq!(v["id"], json!(5));
        assert_eq!(v["result"]["outcome"]["outcome"], json!("selected"));
        assert_eq!(v["result"]["outcome"]["optionId"], json!("allow"));
        let cancelled = rpc_result(&json!(5), json!({"outcome":{"outcome":"cancelled"}}));
        let v: Value = serde_json::from_str(&cancelled).expect("JSON");
        assert_eq!(v["result"]["outcome"]["outcome"], json!("cancelled"));
    }

    // ── ハンドシェイク ────────────────────────────────────────────

    /// claude-agent-acp 0.66.0 の**実測レスポンス**。
    const REAL_INIT: &str = r#"{
      "protocolVersion":1,
      "agentCapabilities":{
        "promptCapabilities":{"image":true,"embeddedContext":true},
        "mcpCapabilities":{"http":true,"sse":true},
        "loadSession":true,
        "sessionCapabilities":{"additionalDirectories":{},"close":{},"delete":{},"fork":{},"list":{},"resume":{}}},
      "agentInfo":{"name":"@agentclientprotocol/claude-agent-acp","title":"Claude Agent","version":"0.66.0"},
      "authMethods":[],
      "_meta":{"steering":{"supported":true},
               "goal":{"version":1,"controlMethod":"_session/goal","actions":["set","clear"]}}}"#;

    #[test]
    fn 実測のinitializeレスポンスを読み切れる() {
        let init: InitializeResult = serde_json::from_str(REAL_INIT).expect("実測レスポンス");
        assert_eq!(init.protocol_version, PROTOCOL_VERSION);
        assert!(init.agent_capabilities.load_session);
        assert!(init.agent_capabilities.session_supports("resume"));
        assert!(init.agent_capabilities.session_supports("list"));
        assert!(
            !init.agent_capabilities.session_supports("なにか"),
            "広告されていないものを対応と誤認している"
        );
        assert_eq!(
            init.agent_info.as_ref().map(|i| i.version.as_str()),
            Some("0.66.0")
        );
        assert!(init.steering_supported(), "steering を見落としている");
    }

    #[test]
    fn gemini風の最小レスポンスでも壊れない() {
        // Gemini CLI は sessionCapabilities を一切広告しない。
        let init: InitializeResult = serde_json::from_str(
            r#"{"protocolVersion":1,"agentCapabilities":{"promptCapabilities":{"image":true}}}"#,
        )
        .expect("最小レスポンス");
        assert!(!init.agent_capabilities.session_supports("list"));
        assert!(!init.steering_supported());
        assert!(init.agent_info.is_none());
    }

    #[test]
    fn プロンプト応答の非標準フィールドは黙って捨てる() {
        // 実測: 非標準の `usage` が同梱されて届いた。
        let v: Value =
            serde_json::from_str(r#"{"stopReason":"end_turn","usage":{"in":1,"out":2}}"#)
                .expect("JSON");
        let stop = v
            .get("stopReason")
            .and_then(Value::as_str)
            .map(StopReason::from_wire);
        assert_eq!(stop, Some(StopReason::EndTurn));
        // session/new に Codex が混ぜてくる非標準の models も同様
        let v: Value = serde_json::from_str(r#"{"sessionId":"sess_1","models":[{"id":"gpt"}]}"#)
            .expect("JSON");
        assert_eq!(v.get("sessionId").and_then(Value::as_str), Some("sess_1"));
    }

    #[test]
    fn initializeで送る封筒が仕様どおり() {
        let line = rpc_request(
            0,
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {"fs":{"readTextFile":true,"writeTextFile":true},"terminal":false},
                "clientInfo": {"name":"zaivern-code","title":"Zaivern Code","version":"0.0.0"}
            }),
        );
        let v: Value = serde_json::from_str(&line).expect("JSON");
        assert_eq!(v["jsonrpc"], json!("2.0"));
        assert_eq!(v["method"], json!("initialize"));
        assert_eq!(v["params"]["protocolVersion"], json!(1));
        assert_eq!(
            v["params"]["clientCapabilities"]["fs"]["readTextFile"],
            json!(true)
        );
        // **実装していない能力は広告しない。**
        assert_eq!(v["params"]["clientCapabilities"]["terminal"], json!(false));
    }

    // ── fs/* のサンドボックス ────────────────────────────────────

    #[test]
    fn 相対パスとワークスペース外は拒否する() {
        let root = crate::test_util::unique_temp_dir("zaivern", "acp-fs");
        let roots = vec![crate::pathx::canonical(&root)];
        // 相対パスは仕様違反 (ACP は絶対パス)
        assert_eq!(
            resolve_in_roots(&roots, "src/app.rs"),
            Err(FsDenied::NotAbsolute)
        );
        // 素直に外を指す
        let outside =
            crate::pathx::canonical(&std::env::temp_dir()).join("zaivern-acp-outside.txt");
        assert_eq!(
            resolve_in_roots(&roots, &outside.display().to_string()),
            Err(FsDenied::Outside)
        );
        // `..` で外へ出る
        let escape = root.join("..").join("zaivern-acp-escape.txt");
        assert_eq!(
            resolve_in_roots(&roots, &escape.display().to_string()),
            Err(FsDenied::Outside)
        );
        // ルートが無ければ何も許さない
        assert_eq!(
            resolve_in_roots(&[], &root.join("a.txt").display().to_string()),
            Err(FsDenied::NoRoots)
        );
        // 中は通る (まだ存在しないファイルも)
        let inside = root.join("sub").join("new.txt");
        assert!(resolve_in_roots(&roots, &inside.display().to_string()).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn パスの正規化はルートより上へ行かない() {
        let p = lexical_normalize(Path::new("/a/b/../../../../c"));
        assert_eq!(p, PathBuf::from("/c"));
        assert_eq!(
            lexical_normalize(Path::new("/a/./b/")),
            PathBuf::from("/a/b")
        );
    }

    #[test]
    fn 未保存バッファがディスクより優先される() {
        let root = crate::test_util::unique_temp_dir("zaivern", "acp-unsaved");
        let file = root.join("a.txt");
        std::fs::write(&file, "ディスクの内容").expect("書ける");
        let host = FsHost::default();
        host.set_roots(&[root.clone()]);
        let params = json!({"sessionId":"s","path": file.display().to_string()});
        let got = host.read_text_file(&params).expect("読める");
        assert_eq!(got["content"], json!("ディスクの内容"));
        // 編集中の内容を publish すると、そちらが返る
        host.replace_unsaved(vec![(file.clone(), "編集中の内容".to_string())]);
        assert_eq!(host.unsaved_len(), 1);
        let got = host.read_text_file(&params).expect("読める");
        assert_eq!(got["content"], json!("編集中の内容"));
        // 保存したら一覧から消える
        host.replace_unsaved(Vec::new());
        assert_eq!(host.unsaved_len(), 0);
        let got = host.read_text_file(&params).expect("読める");
        assert_eq!(got["content"], json!("ディスクの内容"));
        host.clear_unsaved();
        assert_eq!(host.unsaved_len(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 書き込みは無いファイルを作り外は拒否する() {
        let root = crate::test_util::unique_temp_dir("zaivern", "acp-write");
        let host = FsHost::default();
        host.set_roots(&[root.clone()]);
        let target = root.join("deep").join("b.txt");
        let ok = host.write_text_file(&json!({
            "sessionId":"s","path": target.display().to_string(),"content":"あ"
        }));
        assert!(ok.is_ok(), "{ok:?}");
        assert_eq!(std::fs::read_to_string(&target).expect("読める"), "あ");
        // 外は拒否 (エラーコードは -32602)
        let outside = crate::pathx::canonical(&std::env::temp_dir()).join("zaivern-acp-nope.txt");
        let err = host
            .write_text_file(&json!({
                "sessionId":"s","path": outside.display().to_string(),"content":"x"
            }))
            .expect_err("拒否される");
        assert_eq!(err.0, ERR_INVALID_PARAMS);
        assert!(!outside.exists(), "拒否したのに書かれている");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 行指定の読み取りは1始まりで効く() {
        let text = "1行目\n2行目\n3行目\n4行目";
        assert_eq!(slice_lines(text, None, None), text);
        assert_eq!(slice_lines(text, Some(2), Some(2)), "2行目\n3行目");
        assert_eq!(slice_lines(text, Some(1), Some(1)), "1行目");
        // 0 を渡されても 1 行目として扱う (1 始まりの規約を破らない)
        assert_eq!(slice_lines(text, Some(0), Some(1)), "1行目");
        // 行数を超えても空文字で返る (panic しない)
        assert_eq!(slice_lines(text, Some(99), Some(2)), "");
    }

    #[test]
    fn 知らないクライアントメソッドは_32601で返す() {
        let host = FsHost::default();
        let err = host
            .handle("terminal/create", &json!({}))
            .expect_err("未実装");
        assert_eq!(err.0, ERR_METHOD_NOT_FOUND);
    }

    // ── リーダースレッドの振り分け (実プロセス不要) ────────────────

    #[test]
    fn 捕獲したワイヤを流すと1本のパイプから3系統が出てくる() {
        let root = crate::test_util::unique_temp_dir("zaivern", "acp-loop");
        let file = root.join("read-me.txt");
        std::fs::write(&file, "中身").expect("書ける");
        let host = Arc::new(FsHost::default());
        host.set_roots(&[root.clone()]);

        // 実測ターンの縮約 (initialize の返事 → 更新 → fs 読み取り要求 →
        // 権限要求 → プロンプトの返事)。
        let transcript = format!(
            concat!(
                r#"{{"jsonrpc":"2.0","id":0,"result":{{"protocolVersion":1}}}}"#,
                "\n",
                r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"O"}}}}}}}}"#,
                "\n",
                r#"{{"jsonrpc":"2.0","id":9,"method":"fs/read_text_file","params":{{"sessionId":"s1","path":"{path}"}}}}"#,
                "\n",
                r#"{{"jsonrpc":"2.0","id":5,"method":"session/request_permission","params":{{"sessionId":"s1","toolCall":{{"toolCallId":"t1","title":"Terminal","kind":"execute"}},"options":[{{"optionId":"reject","name":"Deny","kind":"reject_once"}},{{"optionId":"allow","name":"Allow","kind":"allow_once"}}]}}}}"#,
                "\n",
                "これは ACP ではないゴミ行\n",
                r#"{{"jsonrpc":"2.0","id":2,"result":{{"stopReason":"end_turn","usage":{{"in":1}}}}}}"#,
                "\n"
            ),
            path = file.display().to_string().replace('\\', "\\\\")
        );

        let (tx_ev, rx_ev) = mpsc::channel();
        let (tx_out, rx_out) = mpsc::channel();
        read_loop(
            std::io::Cursor::new(transcript.into_bytes()),
            tx_ev,
            tx_out,
            host,
            None,
        );

        let mut responses = 0;
        let mut updates = 0;
        let mut perms = 0;
        let mut notes = 0;
        let mut closed = false;
        while let Ok(ev) = rx_ev.try_recv() {
            match ev {
                AcpEvent::Response { .. } => responses += 1,
                AcpEvent::Update { .. } => updates += 1,
                AcpEvent::Permission { params, .. } => {
                    perms += 1;
                    // **先頭は reject。ここで種別照合が効いていることを確かめる。**
                    assert_eq!(params.options[0].kind, PermissionOptionKind::RejectOnce);
                    assert_eq!(
                        pick_option(&params.options, true).map(|o| o.option_id.as_str()),
                        Some("allow")
                    );
                }
                AcpEvent::Note(_) => notes += 1,
                AcpEvent::Closed => closed = true,
                AcpEvent::Stderr(_) => {}
            }
        }
        assert_eq!(responses, 2, "レスポンスが 2 本出ていない");
        assert_eq!(updates, 1);
        assert_eq!(perms, 1);
        assert_eq!(notes, 1, "ゴミ行は記録だけ残す");
        assert!(closed, "終端で Closed を出していない");

        // fs/read_text_file には**その場で**答えている (UI を待たない)
        let reply = rx_out.try_recv().expect("fs の返事が出ている");
        let v: Value = serde_json::from_str(&reply).expect("JSON");
        assert_eq!(v["id"], json!(9));
        assert_eq!(v["result"]["content"], json!("中身"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ワークスペース外の読み取り要求にはエラーで答える() {
        let root = crate::test_util::unique_temp_dir("zaivern", "acp-deny");
        let host = Arc::new(FsHost::default());
        host.set_roots(&[root.clone()]);
        let outside = crate::pathx::canonical(&std::env::temp_dir())
            .join("zaivern-acp-outside-read.txt")
            .display()
            .to_string()
            .replace('\\', "\\\\");
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":7,"method":"fs/read_text_file","params":{{"sessionId":"s","path":"{outside}"}}}}"#
        );
        let (tx_ev, _rx_ev) = mpsc::channel();
        let (tx_out, rx_out) = mpsc::channel();
        read_loop(
            std::io::Cursor::new(line.into_bytes()),
            tx_ev,
            tx_out,
            host,
            None,
        );
        let reply = rx_out.try_recv().expect("返事が出ている");
        let v: Value = serde_json::from_str(&reply).expect("JSON");
        assert_eq!(v["id"], json!(7));
        assert_eq!(v["error"]["code"], json!(ERR_INVALID_PARAMS));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── カタログ ────────────────────────────────────────────────

    #[test]
    fn acpカタログはデータとして揃っている() {
        let cat = crate::agents::ACP_CATALOG;
        assert!(cat.len() >= 4, "登録が少なすぎる: {}", cat.len());
        let mut ids: Vec<&str> = cat.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "id が重複している");
        for e in cat {
            assert!(!e.label.is_empty(), "{} にラベルが無い", e.id);
            assert!(!e.icon.is_empty(), "{} にアイコンが無い", e.id);
            assert!(
                !e.npx_package.is_empty() || !e.local_bin.is_empty(),
                "{} に起動手段が無い",
                e.id
            );
            // 実行ファイル名だけを持ち、パスを直書きしていない
            assert!(
                !e.local_bin.contains('/') && !e.local_bin.contains('\\'),
                "{} がパスを直書きしている",
                e.id
            );
        }
    }

    #[test]
    fn パッケージ指定はバージョンを固定できる() {
        let e = crate::agents::ACP_CATALOG
            .iter()
            .find(|e| e.id == "claude-acp")
            .expect("claude-acp がある");
        assert_eq!(
            e.package_spec(),
            format!("{}@{}", e.npx_package, e.npx_version)
        );
        // deprecated な旧名を使っていない
        for e in crate::agents::ACP_CATALOG {
            assert!(
                !e.npx_package.contains("@zed-industries/"),
                "{} が deprecated な旧パッケージ名を使っている",
                e.id
            );
        }
    }

    // ── レイアウト ───────────────────────────────────────────────

    #[test]
    fn ツール行はどの幅でも可用幅に収まる() {
        for w in [
            0.0_f32, 60.0, 120.0, 200.0, 300.0, 419.0, 420.0, 900.0, 1200.0,
        ] {
            let l = tool_row_layout(w);
            assert!(
                l.total() <= w + 0.01,
                "幅 {w} で列がはみ出した: {l:?} total={}",
                l.total()
            );
            assert!(l.title_w >= 0.0, "幅 {w} で負の列幅: {l:?}");
            // 狭いところでは場所列から落ちる
            if w < LOC_MIN_ROW_W {
                assert_eq!(l.loc_w, 0.0, "幅 {w} で場所列が残っている");
            }
        }
        // 十分広ければ全列出る
        let wide = tool_row_layout(900.0);
        assert!(wide.kind_w > 0.0 && wide.loc_w > 0.0);
        assert!(wide.title_w >= TITLE_MIN_W);
    }

    #[test]
    fn 空状態カードは可用領域の中央に必ず収まる() {
        for (w, h) in [
            (900.0_f32, 700.0_f32),
            (1200.0, 300.0),
            (320.0, 200.0),
            (100.0, 60.0),
        ] {
            let avail = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(w, h));
            let card = empty_card(avail);
            assert!(
                avail.contains_rect(card),
                "{w}x{h} でカードがはみ出した: {card:?} avail={avail:?}"
            );
            // 中央
            assert!((card.center().x - avail.center().x).abs() < 0.01);
            assert!((card.center().y - avail.center().y).abs() < 0.01);
        }
    }

    // ── 段の表示 (設計原則 4) ─────────────────────────────────────

    #[test]
    fn 段の表示は構造化プロトコルと画面スクレイプを区別する() {
        assert_ne!(Rung::StructuredProtocol.label(), Rung::ScreenScrape.label());
        assert!(Rung::StructuredProtocol.label().contains("ACP"));
        assert!(Rung::ScreenScrape.label().contains("PTY"));
        assert!(!Rung::StructuredProtocol.hint().is_empty());
    }

    // ── 疑似セッション ID ────────────────────────────────────────

    #[test]
    fn acpのセッションIDはPTYと空間が重ならない() {
        let m = AcpManager::default();
        assert!(
            m.next_id >= ACP_SESSION_ID_BASE,
            "PTY の連番 (1 から) と衝突する"
        );
        assert!(m.is_empty(), "起動直後は 0 本");
    }

    #[test]
    fn 接続が無ければ未保存の同期は何もしない() {
        let mut m = AcpManager::default();
        let p = PathBuf::from(if cfg!(windows) {
            r"C:\w\a.rs"
        } else {
            "/w/a.rs"
        });
        m.sync_unsaved(std::iter::once((p.as_path(), 1_u64, "本文")));
        assert_eq!(m.host.unsaved_len(), 0, "接続が無いのに公開している");
    }

    #[test]
    fn 接続が無ければ承認の返事は素通りする() {
        let mut m = AcpManager::default();
        assert!(
            !m.reply(1, ReplyAction::Approve),
            "PTY のセッション ID を横取りしてはいけない"
        );
    }

    // ── 表示文字列 ───────────────────────────────────────────────

    #[test]
    fn 画面に出す打鍵表記をベタ書きしていない() {
        // このモジュールはショートカットを持たない。持ったら keybinds 経由に
        // すること (config.toml で再割り当てされた瞬間に嘘になるため)。
        let src = include_str!("acp.rs").replace("\r\n", "\n");
        for pat in ["⌘", "Ctrl+", "⌃", "⌥"] {
            assert!(
                !src.contains(pat),
                "打鍵表記 {pat} をベタ書きしている — keybinds::format_shortcut を使うこと"
            );
        }
    }
}
