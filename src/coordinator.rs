//! エージェント調停レイヤ — セッション間の連絡と、停滞タスクの再割り当て。
//!
//! ## 何を解決するか
//!
//! 1. **連絡** — 走っている CLI エージェント同士 / 監督役 / ユーザーの間で
//!    メッセージをやり取りする。ただし CLI エージェントへの入り口は PTY の
//!    標準入力しか無く、**生成中に書き込むと入力が壊れる**。だからメッセージは
//!    必ずキューに積み、相手が「注入して安全な状態」のときだけ配達する。
//! 2. **再割り当て** — タスク担当が固まった / 死んだときに別のエージェントへ
//!    引き継ぐ。ただし**前任者が確実に停止したと確認できるまで引き渡さない**。
//!    2 つのエージェントが同じファイルを同時に編集すると成果物が壊れるため。
//!
//! ## 設計の方針
//!
//! - このモジュールは **他モジュールへ一切依存しない**(`use crate::…` が無い)。
//!   セッションの状態や承認モードは呼び出し側が自前の型へ変換して渡す。
//!   監督レイヤ(supervisor)とも型を共有しないので、どちらが先に出来ても壊れない。
//! - **スレッドを使わない**。全て同期的な純メモリ操作で、1 フレーム分の呼び出しは
//!   セッション数・キュー長に対して線形かつ上限付き。UI スレッドを塞がない。
//! - **メモリは全て有界**。リングバッファ・窓の刈り取り・履歴の上限を徹底する。
//! - **黙って捨てない**。落としたメッセージは必ず理由付きで記録・計数する。

// 公開 API 一式を先に用意し、app.rs 側の配線は後から行うため、
// 未使用の警告を抑える(keybinds.rs / editor_ops.rs と同じ扱い)。
#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

// ── 上限値(既定) ───────────────────────────────────────────────────────

/// 転送の最大ホップ数。これを超えたメッセージは捨てる(ループ止め)。
pub const DEFAULT_MAX_HOPS: u8 = 4;
/// レート制限の窓幅。
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(10);
/// 同一 (送信元, 宛先) ペアが窓内に送れる本数。
pub const DEFAULT_PAIR_LIMIT: u32 = 8;
/// 全体で窓内に送れる本数。
pub const DEFAULT_GLOBAL_LIMIT: u32 = 40;
/// ブロードキャストが窓内に送れる本数(直接送信よりきつく絞る)。
pub const DEFAULT_BROADCAST_LIMIT: u32 = 2;
/// ピンポン判定の窓幅。
pub const DEFAULT_PINGPONG_WINDOW: Duration = Duration::from_secs(15);
/// ピンポン判定のしきい値(2 者間の往復本数の合計)。
pub const DEFAULT_PINGPONG_LIMIT: u32 = 6;
/// メールボックス 1 個あたりの保持本数(超えたら古いものから捨てる)。
pub const DEFAULT_MAILBOX_CAP: usize = 64;
/// 破棄ログの保持件数。
pub const DEFAULT_DROP_LOG_CAP: usize = 128;
/// タスク再試行の既定上限。使い切ったら NeedsUser。
pub const DEFAULT_MAX_ATTEMPTS: u8 = 3;
/// タスク履歴の保持件数。
pub const HISTORY_CAP: usize = 64;
/// 引き継ぎコンテキストの保持件数。
pub const CONTEXT_CAP: usize = 32;
/// 引き継ぎコンテキスト 1 件の最大文字数。
pub const CONTEXT_ITEM_MAX: usize = 500;
/// PTY へ注入する本文の最大文字数。
pub const INJECT_BODY_MAX: usize = 600;
/// 追跡するペア数の上限(これを超えたら空の窓を掃除する)。
const PAIR_TRACK_CAP: usize = 256;

/// PTY へ注入したメッセージに付ける目印。
///
/// 端末を人間が見たときに「これは自分が打ったのではない」と一目で分かるようにする。
pub const INJECT_PREFIX: &str = "[ZAI-AGENT]";

// ── 宛先とメッセージ ──────────────────────────────────────────────────

/// セッション識別子。`terminal::Session::id` と同じ値を渡す想定。
pub type SessionId = u64;
/// タスク識別子。
pub type TaskId = u64;

/// メッセージの送信元 / 宛先。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Endpoint {
    /// 実行中の CLI エージェントセッション。
    Session(SessionId),
    /// 監督レイヤ(異常検知など)。
    Supervisor,
    /// 人間のユーザー。ここへ届いたものは UI で必ず見せる。
    User,
    /// 全セッション宛。
    Broadcast,
}

/// メッセージの種別。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgKind {
    /// 依頼。
    Request,
    /// 返答。
    Reply,
    /// タスクの引き継ぎ。
    Handoff,
    /// 状況報告。
    Status,
    /// 質問。
    Question,
    /// 人間へのエスカレーション。
    Escalation,
}

impl MsgKind {
    /// 端末へ注入するときの日本語ラベル。
    pub fn label(self) -> &'static str {
        match self {
            MsgKind::Request => "依頼",
            MsgKind::Reply => "返答",
            MsgKind::Handoff => "引き継ぎ",
            MsgKind::Status => "状況",
            MsgKind::Question => "質問",
            MsgKind::Escalation => "エスカレーション",
        }
    }
}

/// エージェント間メッセージ 1 通。
#[derive(Clone, Debug)]
pub struct AgentMessage {
    /// 連番 ID。`Coordinator::enqueue` が採番する(投入前は 0)。
    pub id: u64,
    pub from: Endpoint,
    pub to: Endpoint,
    pub kind: MsgKind,
    pub body: String,
    /// 生成時刻。レート制限の窓もこの時刻を基準に判定するため、
    /// テストからは任意の時刻を差し込める。
    pub at: Instant,
    /// 転送するたびに 1 増える。`max_hops` を超えたら捨てる。
    pub hops: u8,
}

impl AgentMessage {
    /// 「いま」の時刻でメッセージを作る。
    pub fn new(from: Endpoint, to: Endpoint, kind: MsgKind, body: impl Into<String>) -> Self {
        Self {
            id: 0,
            from,
            to,
            kind,
            body: body.into(),
            at: Instant::now(),
            hops: 0,
        }
    }

    /// 時刻を差し替える(テストと、まとめ処理で時刻を揃えたいとき用)。
    pub fn at(mut self, at: Instant) -> Self {
        self.at = at;
        self
    }
}

// ── セッション状態と「注入して安全か」の判定 ─────────────────────────────

/// 調停レイヤから見たセッションの状態。
///
/// `terminal::Session` の生の状態からの変換は呼び出し側の責任。
/// 判断がつかないときは必ず `Unknown` を渡すこと(既定で配達しない)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// プロンプトで待機中。注入して安全。
    Idle,
    /// 入力待ち(プロンプトが出て人のターンになっている)。注入して安全。
    AwaitingInput,
    /// 生成中 / 作業中。**注入すると入力が壊れる**。
    Working,
    /// 承認プロンプト待ち。**絶対に注入しない**
    /// (本文がそのまま承認の返事として解釈されてしまう)。
    WaitingApproval,
    /// 無反応。内部状態が読めないので注入しない。
    Stalled,
    /// 終了済み。
    Exited,
    /// 不明。既定で注入しない。
    Unknown,
}

/// 注入して安全な状態かどうか。
///
/// 安全な集合は `Idle` と `AwaitingInput` の 2 つだけ。それ以外は全て不可で、
/// 特に `WaitingApproval` と `Unknown` は明示的に不可とする。
pub fn deliverable(state: SessionState) -> bool {
    match state {
        SessionState::Idle | SessionState::AwaitingInput => true,
        SessionState::Working
        | SessionState::WaitingApproval
        | SessionState::Stalled
        | SessionState::Exited
        | SessionState::Unknown => false,
    }
}

/// タスクを割り当ててよい状態か(忙しくても割り当て自体は可能)。
fn assignable(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Idle | SessionState::AwaitingInput | SessionState::Working
    )
}

/// 空いている(= 忙しくない)なら 0、忙しいなら 1。割り当ての優先順位に使う。
fn busy_rank(state: SessionState) -> u8 {
    match state {
        SessionState::Idle | SessionState::AwaitingInput => 0,
        _ => 1,
    }
}

// ── 承認モード ───────────────────────────────────────────────────────

/// 承認モード。`agents::Approval` と 1:1 で対応する写し。
///
/// 依存を断つためにこちら側で持つ。変換は呼び出し側で行う
/// (`Approval::Ask => PermissionMode::Ask` など)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionMode {
    Ask,
    Auto,
    Agent,
}

/// 破壊的な操作の提案。実行の可否は承認モードのゲートを通す。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Proposal {
    /// セッションを停止する。作業中の内容を捨てる可能性があるため破壊的。
    StopSession {
        session: SessionId,
        task: TaskId,
        /// 日本語の理由(UI にそのまま出せる)。
        reason: String,
    },
}

/// 提案をどう扱うか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposalGate {
    /// そのまま実行してよい。
    AutoApproved,
    /// ユーザーの明示的な確認が要る。
    NeedsUserConfirm,
}

/// 承認モードから提案の扱いを決める。
///
/// セッション停止は作業中の成果を捨てうるので、自動で通すのは `Auto` のときだけ。
/// `Agent`(プリセット任せ)は調停レイヤ側の意味が定義できないため、
/// 安全側に倒してユーザー確認を要求する。
pub fn gate_for(mode: PermissionMode) -> ProposalGate {
    match mode {
        PermissionMode::Auto => ProposalGate::AutoApproved,
        PermissionMode::Ask | PermissionMode::Agent => ProposalGate::NeedsUserConfirm,
    }
}

// ── 破棄理由 ─────────────────────────────────────────────────────────

/// メッセージを捨てた理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// ホップ数超過(転送ループ)。
    HopLimit { hops: u8 },
    /// (送信元, 宛先) ペアのレート制限。
    RateLimitPair,
    /// 全体のレート制限。
    RateLimitGlobal,
    /// ブロードキャストのレート制限。
    RateLimitBroadcast,
    /// ピンポン(2 者間の往復)を検出して抑制。
    PingPong,
    /// メールボックス溢れ(古いものを押し出した)。
    MailboxOverflow,
    /// 宛先セッションが登録されていない。
    UnknownTarget,
    /// 自分宛(送信元と宛先が同じ)。
    SelfAddressed,
}

/// 破棄理由の種別だけを取り出したもの(計数用のキー)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DropKind {
    HopLimit,
    RateLimitPair,
    RateLimitGlobal,
    RateLimitBroadcast,
    PingPong,
    MailboxOverflow,
    UnknownTarget,
    SelfAddressed,
}

impl DropReason {
    pub fn kind(self) -> DropKind {
        match self {
            DropReason::HopLimit { .. } => DropKind::HopLimit,
            DropReason::RateLimitPair => DropKind::RateLimitPair,
            DropReason::RateLimitGlobal => DropKind::RateLimitGlobal,
            DropReason::RateLimitBroadcast => DropKind::RateLimitBroadcast,
            DropReason::PingPong => DropKind::PingPong,
            DropReason::MailboxOverflow => DropKind::MailboxOverflow,
            DropReason::UnknownTarget => DropKind::UnknownTarget,
            DropReason::SelfAddressed => DropKind::SelfAddressed,
        }
    }

    /// UI に出す日本語の説明。
    pub fn label(self) -> String {
        match self {
            DropReason::HopLimit { hops } => format!("転送回数の上限超過 ({hops} ホップ)"),
            DropReason::RateLimitPair => "同一相手への送信が多すぎる".into(),
            DropReason::RateLimitGlobal => "全体の送信量が多すぎる".into(),
            DropReason::RateLimitBroadcast => "一斉送信が多すぎる".into(),
            DropReason::PingPong => "2 者間の往復を検出したため抑制".into(),
            DropReason::MailboxOverflow => "受信箱が満杯のため古いものを破棄".into(),
            DropReason::UnknownTarget => "宛先セッションが存在しない".into(),
            DropReason::SelfAddressed => "自分宛のため破棄".into(),
        }
    }
}

/// 破棄の記録 1 件。
#[derive(Clone, Debug)]
pub struct DropRecord {
    pub at: Instant,
    pub msg_id: u64,
    pub from: Endpoint,
    pub to: Endpoint,
    pub reason: DropReason,
}

/// 送信の結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// 受信箱に積んだ(まだ配達はしていない)。
    Queued { id: u64 },
    /// 一斉送信で n 個の受信箱へ積んだ。
    Broadcast { id: u64, delivered_to: usize },
    /// 捨てた。理由付き。
    Dropped { reason: DropReason },
}

// ── メールボックス(有界リングバッファ) ────────────────────────────────

/// セッション 1 つ分の受信箱。上限に達したら**古いものから捨てる**。
#[derive(Debug)]
pub struct Mailbox {
    queue: VecDeque<AgentMessage>,
    cap: usize,
    /// 溢れて捨てた累計本数。
    dropped_oldest: u32,
    /// 配達済みの累計本数。
    delivered: u32,
}

impl Mailbox {
    fn new(cap: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            cap: cap.max(1),
            dropped_oldest: 0,
            delivered: 0,
        }
    }

    /// 末尾へ積む。溢れたら押し出された 1 通を返す。
    fn push(&mut self, msg: AgentMessage) -> Option<AgentMessage> {
        let evicted = if self.queue.len() >= self.cap {
            self.dropped_oldest = self.dropped_oldest.saturating_add(1);
            self.queue.pop_front()
        } else {
            None
        };
        self.queue.push_back(msg);
        evicted
    }

    fn pop(&mut self) -> Option<AgentMessage> {
        let m = self.queue.pop_front();
        if m.is_some() {
            self.delivered = self.delivered.saturating_add(1);
        }
        m
    }

    /// 溜まっている本数。
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// 溢れて捨てた累計本数。
    pub fn dropped_oldest(&self) -> u32 {
        self.dropped_oldest
    }

    /// 配達した累計本数。
    pub fn delivered(&self) -> u32 {
        self.delivered
    }

    /// 中身を覗く(UI 表示用)。
    pub fn iter(&self) -> impl Iterator<Item = &AgentMessage> {
        self.queue.iter()
    }
}

// ── 配達 ─────────────────────────────────────────────────────────────

/// 1 通分の配達指示。呼び出し側が `Session::send_text(&text)` へ流す。
#[derive(Clone, Debug)]
pub struct Delivery {
    pub session: SessionId,
    pub msg_id: u64,
    /// PTY へそのまま書き込む文字列(末尾に確定用の CR を含む)。
    pub text: String,
}

/// 本文を 1 行へ潰し、制御文字を除いて長さを切り詰める。
///
/// CLI エージェントの入力は「1 行 + Enter」で 1 ターン。本文中に改行があると
/// 途中で送信されてしまうため、改行は区切り記号へ置き換える。
fn sanitize_body(body: &str) -> String {
    let mut out = String::with_capacity(body.len().min(INJECT_BODY_MAX) + 8);
    let mut pending_break = false;
    for ch in body.chars() {
        if out.chars().count() >= INJECT_BODY_MAX {
            out.push('…');
            break;
        }
        if ch == '\n' || ch == '\r' {
            pending_break = true;
            continue;
        }
        if ch.is_control() {
            continue;
        }
        if pending_break {
            if !out.is_empty() {
                out.push_str(" / ");
            }
            pending_break = false;
        }
        out.push(ch);
    }
    out
}

/// 送信元を人が読める短い表記にする。
fn endpoint_label(e: Endpoint) -> String {
    match e {
        Endpoint::Session(id) => format!("session:{id}"),
        Endpoint::Supervisor => "supervisor".into(),
        Endpoint::User => "user".into(),
        Endpoint::Broadcast => "broadcast".into(),
    }
}

/// PTY へ注入する 1 行を組み立てる。
///
/// 先頭に [`INJECT_PREFIX`] を置くので、端末を見ている人間には
/// 「機械が入れた行」だと分かる。末尾の `\r` で 1 ターンとして確定させる。
pub fn format_injection(msg: &AgentMessage) -> String {
    format!(
        "{} #{} {}から({}): {}\r",
        INJECT_PREFIX,
        msg.id,
        endpoint_label(msg.from),
        msg.kind.label(),
        sanitize_body(&msg.body)
    )
}

// ── 発信マーカー ─────────────────────────────────────────────────────

/// エージェントが「別のエージェントへ送りたい」ときに **自分で書く** 行の接頭辞。
///
/// 受信側の [`INJECT_PREFIX`] と対になる。書式は
/// `[ZAI-TO:<宛先>] <本文>` の 1 行で、`<宛先>` はセッション名か
/// [`OUTBOUND_ALL`]。LLM に解釈させず、この決め打ちの形だけを見る。
pub const OUTBOUND_PREFIX: &str = "[ZAI-TO:";

/// 一斉送信を表す宛先ラベル。
pub const OUTBOUND_ALL: &str = "ALL";

/// 宛先ラベルの最大文字数。これを超える行は形式不正として捨てる。
const OUTBOUND_TARGET_MAX: usize = 64;

/// 発信マーカー 1 行を解析して `(宛先ラベル, 本文)` を返す **純関数**。
///
/// 解析しないもの(すべて `None`):
///
/// - **行頭以外**にマーカーがある行。プロンプトや引用の中の文字列で
///   誤爆させないため、位置をずらす救済は一切しない。
/// - [`INJECT_PREFIX`] を含む行。注入した行がそのまま画面に出た「こだま」を
///   発信と読むと、送る → 映る → また送る の**無限ループ**になる。
/// - 宛先が空 / `]` が閉じていない / 本文が空。
pub fn parse_outbound(line: &str) -> Option<(String, String)> {
    // 端末は行末を空白で埋める。末尾だけは落とすが、行頭は 1 文字もずらさない。
    let line = line.trim_end_matches([' ', '\t', '\r', '\n']);

    // 注入行のこだま除け。これが唯一のループ止めなので、順序を入れ替えないこと。
    if line.contains(INJECT_PREFIX) {
        return None;
    }

    let rest = line.strip_prefix(OUTBOUND_PREFIX)?;
    let close = rest.find(']')?;
    let target = rest[..close].trim();
    if target.is_empty() || target.chars().count() > OUTBOUND_TARGET_MAX {
        return None;
    }
    // `]` は ASCII なので close + 1 は必ず文字境界。
    let body = sanitize_body(rest[close + 1..].trim());
    if body.is_empty() {
        return None;
    }
    Some((target.to_string(), body))
}

// ── タスク ───────────────────────────────────────────────────────────

/// タスクの状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    /// 未割り当て。
    Pending,
    /// 割り当て済み(まだ動き出していない)。
    Assigned,
    /// 実行中。
    Running,
    /// 停滞。
    Stalled,
    /// 失敗。
    Failed,
    /// 完了。
    Done,
    /// 人手が要る(再試行の上限に達した等)。
    NeedsUser,
}

impl TaskState {
    /// これ以上動かす必要が無い終端状態か。
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskState::Done | TaskState::NeedsUser)
    }
}

/// 再割り当ての理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReassignReason {
    /// 担当が停滞した。
    Stalled,
    /// 担当プロセスが落ちた。
    SessionDied,
    /// 担当が失敗を報告した。
    Failed,
    /// 人手による指示。
    Manual,
}

impl ReassignReason {
    pub fn label(self) -> &'static str {
        match self {
            ReassignReason::Stalled => "停滞",
            ReassignReason::SessionDied => "セッション消滅",
            ReassignReason::Failed => "失敗",
            ReassignReason::Manual => "手動",
        }
    }
}

/// タスクに起きた出来事。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskEvent {
    Created,
    Assigned(SessionId),
    Started(SessionId),
    Stalled(SessionId),
    Failed {
        session: SessionId,
        reason: String,
    },
    Reassigned {
        from: Option<SessionId>,
        to: SessionId,
        reason: ReassignReason,
    },
    /// 引き渡しを拒否した(前任者の停止が未確認 など)。
    HandoverRefused(AssignRefusal),
    /// 前任者の停止を確認した。
    PreviousStopped(SessionId),
    /// 引き継ぎ資料を渡した。
    ContextCarried(usize),
    Completed(SessionId),
    /// 人間へ上げた。
    EscalatedToUser(String),
}

/// 割り当てを断った理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignRefusal {
    /// そんなタスクは無い。
    NoSuchTask,
    /// もう終わっている。
    TaskFinished,
    /// **前任者の停止が確認できていない**。
    /// 同じファイルを 2 人が同時に触ると成果物が壊れるため、引き渡さない。
    PreviousHolderNotStopped { previous: SessionId },
    /// 再試行の上限に達した(タスクは NeedsUser になる)。
    AttemptsExhausted { attempts: u8 },
    /// 条件を満たす候補がいない。
    NoEligibleCandidate,
}

impl AssignRefusal {
    /// UI に出す日本語の説明。
    pub fn label(self) -> String {
        match self {
            AssignRefusal::NoSuchTask => "該当タスクが無い".into(),
            AssignRefusal::TaskFinished => "タスクは既に終了している".into(),
            AssignRefusal::PreviousHolderNotStopped { previous } => {
                format!("前任 session:{previous} の停止が未確認のため引き渡さない")
            }
            AssignRefusal::AttemptsExhausted { attempts } => {
                format!("再試行の上限に到達 ({attempts} 回) — 人手が必要")
            }
            AssignRefusal::NoEligibleCandidate => "割り当て可能なセッションがいない".into(),
        }
    }
}

/// 割り当て候補のセッション情報。
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub id: SessionId,
    pub state: SessionState,
    /// 申告している能力(`required_caps` と突き合わせる)。
    pub caps: Vec<String>,
}

impl SessionInfo {
    pub fn new(id: SessionId, state: SessionState, caps: &[&str]) -> Self {
        Self {
            id,
            state,
            caps: caps.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// 作業タスク 1 件。
#[derive(Clone, Debug)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub description: String,
    pub assigned: Option<SessionId>,
    pub state: TaskState,
    pub attempts: u8,
    /// 出来事の履歴(上限 [`HISTORY_CAP`]、古いものから捨てる)。
    pub history: Vec<(Instant, TaskEvent)>,
    pub required_caps: Vec<String>,

    /// このタスクで失敗した / 停滞したセッション。**二度と割り当てない**。
    failed_by: HashSet<SessionId>,
    /// 前任者の停止が確認できているか。引き渡しの前提条件。
    prev_holder_stopped: bool,
    /// 次の担当へ引き継ぐ材料(上限 [`CONTEXT_CAP`])。
    context: Vec<String>,
    /// 履歴が溢れて捨てた件数。
    history_dropped: u32,
}

impl Task {
    fn record(&mut self, at: Instant, ev: TaskEvent) {
        if self.history.len() >= HISTORY_CAP {
            self.history.remove(0);
            self.history_dropped = self.history_dropped.saturating_add(1);
        }
        self.history.push((at, ev));
    }

    /// このタスクで失敗済みのセッションか。
    pub fn has_failed(&self, s: SessionId) -> bool {
        self.failed_by.contains(&s)
    }

    /// 前任者の停止が確認済みか。
    pub fn previous_stopped(&self) -> bool {
        self.prev_holder_stopped
    }

    /// 引き継ぎ材料。
    pub fn context(&self) -> &[String] {
        &self.context
    }

    /// 履歴が溢れて捨てた件数。
    pub fn history_dropped(&self) -> u32 {
        self.history_dropped
    }
}

// ── 上限設定 ─────────────────────────────────────────────────────────

/// 調停レイヤの上限設定。既定値は各定数を参照。
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_hops: u8,
    pub window: Duration,
    pub pair_limit: u32,
    pub global_limit: u32,
    pub broadcast_limit: u32,
    pub pingpong_window: Duration,
    pub pingpong_limit: u32,
    pub mailbox_cap: usize,
    pub drop_log_cap: usize,
    pub max_attempts: u8,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_hops: DEFAULT_MAX_HOPS,
            window: DEFAULT_WINDOW,
            pair_limit: DEFAULT_PAIR_LIMIT,
            global_limit: DEFAULT_GLOBAL_LIMIT,
            broadcast_limit: DEFAULT_BROADCAST_LIMIT,
            pingpong_window: DEFAULT_PINGPONG_WINDOW,
            pingpong_limit: DEFAULT_PINGPONG_LIMIT,
            mailbox_cap: DEFAULT_MAILBOX_CAP,
            drop_log_cap: DEFAULT_DROP_LOG_CAP,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }
}

/// 時刻の窓。古いものを刈り取って本数を数えるだけの小さな器。
#[derive(Debug, Default)]
struct Window {
    stamps: VecDeque<Instant>,
}

impl Window {
    /// `now - width` より古い記録を落とす。
    fn prune(&mut self, now: Instant, width: Duration) {
        while let Some(&front) = self.stamps.front() {
            // now より未来の記録は残す(テストで時刻を巻き戻した場合の保険)。
            if now.checked_duration_since(front).is_some_and(|d| d > width) {
                self.stamps.pop_front();
            } else {
                break;
            }
        }
    }

    fn len(&self) -> u32 {
        self.stamps.len() as u32
    }

    fn push(&mut self, now: Instant) {
        self.stamps.push_back(now);
    }
}

/// 2 者間のペアキー(向きを無視する)。
fn unordered(a: Endpoint, b: Endpoint) -> (Endpoint, Endpoint) {
    // Endpoint に順序が無いので、判別用の数値で正規化する。
    let rank = |e: Endpoint| match e {
        Endpoint::Session(id) => (0u8, id),
        Endpoint::Supervisor => (1, 0),
        Endpoint::User => (2, 0),
        Endpoint::Broadcast => (3, 0),
    };
    if rank(a) <= rank(b) {
        (a, b)
    } else {
        (b, a)
    }
}

// ── 本体 ─────────────────────────────────────────────────────────────

/// エージェント調停レイヤ。
///
/// app.rs が 1 つだけ持ち、毎フレーム [`Coordinator::take_deliverable`] を呼ぶ。
pub struct Coordinator {
    limits: Limits,
    next_msg_id: u64,
    next_task_id: TaskId,

    /// セッションごとの受信箱。ここに存在する = 登録済みセッション。
    mailboxes: HashMap<SessionId, Mailbox>,
    supervisor_inbox: Mailbox,
    user_inbox: Mailbox,

    /// 破棄ログ(有界リング)。
    drop_log: VecDeque<DropRecord>,
    /// 理由ごとの破棄累計。
    drop_counts: HashMap<DropKind, u32>,

    /// (送信元, 宛先) ごとの送信時刻窓。
    pair_windows: HashMap<(Endpoint, Endpoint), Window>,
    /// 向き無視ペアごとの往復時刻窓(ピンポン判定用)。
    pingpong_windows: HashMap<(Endpoint, Endpoint), Window>,
    /// 一度エスカレーション済みのペア(何度も人を呼ばない)。
    pingpong_escalated: HashSet<(Endpoint, Endpoint)>,
    global_window: Window,
    broadcast_window: Window,

    tasks: Vec<Task>,
    /// 直近の割り当て拒否理由。
    last_refusal: Option<AssignRefusal>,
}

impl Default for Coordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl Coordinator {
    pub fn new() -> Self {
        Self::with_limits(Limits::default())
    }

    pub fn with_limits(limits: Limits) -> Self {
        Self {
            next_msg_id: 1,
            next_task_id: 1,
            mailboxes: HashMap::new(),
            supervisor_inbox: Mailbox::new(limits.mailbox_cap),
            user_inbox: Mailbox::new(limits.mailbox_cap),
            drop_log: VecDeque::new(),
            drop_counts: HashMap::new(),
            pair_windows: HashMap::new(),
            pingpong_windows: HashMap::new(),
            pingpong_escalated: HashSet::new(),
            global_window: Window::default(),
            broadcast_window: Window::default(),
            tasks: Vec::new(),
            last_refusal: None,
            limits,
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    // ── セッション登録 ──────────────────────────────────────────────

    /// セッションを登録する(受信箱を用意する)。既存なら何もしない。
    pub fn register_session(&mut self, id: SessionId) {
        let cap = self.limits.mailbox_cap;
        self.mailboxes.entry(id).or_insert_with(|| Mailbox::new(cap));
    }

    /// セッションを外す。溜まっていたメッセージは失われるので、
    /// 事前に [`Coordinator::mailbox`] で中身を UI へ出しておくとよい。
    pub fn unregister_session(&mut self, id: SessionId) {
        self.mailboxes.remove(&id);
    }

    /// 登録済みセッションの受信箱。
    pub fn mailbox(&self, id: SessionId) -> Option<&Mailbox> {
        self.mailboxes.get(&id)
    }

    /// 監督レイヤ宛の受信箱。
    pub fn supervisor_inbox(&self) -> &Mailbox {
        &self.supervisor_inbox
    }

    /// ユーザー宛の受信箱(UI に必ず出す)。
    pub fn user_inbox(&self) -> &Mailbox {
        &self.user_inbox
    }

    /// ユーザー宛メッセージを取り出す(取り出したら消える)。
    pub fn take_user_messages(&mut self) -> Vec<AgentMessage> {
        let mut out = Vec::new();
        while let Some(m) = self.user_inbox.pop() {
            out.push(m);
        }
        out
    }

    /// 監督レイヤ宛メッセージを取り出す(取り出したら消える)。
    pub fn take_supervisor_messages(&mut self) -> Vec<AgentMessage> {
        let mut out = Vec::new();
        while let Some(m) = self.supervisor_inbox.pop() {
            out.push(m);
        }
        out
    }

    // ── 破棄ログ ────────────────────────────────────────────────────

    fn record_drop(&mut self, msg: &AgentMessage, reason: DropReason) {
        if self.drop_log.len() >= self.limits.drop_log_cap {
            self.drop_log.pop_front();
        }
        self.drop_log.push_back(DropRecord {
            at: msg.at,
            msg_id: msg.id,
            from: msg.from,
            to: msg.to,
            reason,
        });
        *self.drop_counts.entry(reason.kind()).or_insert(0) += 1;
    }

    /// 破棄ログ(新しいものが後ろ)。
    pub fn drop_log(&self) -> impl Iterator<Item = &DropRecord> {
        self.drop_log.iter()
    }

    /// 理由ごとの破棄累計。
    pub fn drop_count(&self, kind: DropKind) -> u32 {
        self.drop_counts.get(&kind).copied().unwrap_or(0)
    }

    /// 破棄の総数。
    pub fn total_drops(&self) -> u32 {
        self.drop_counts.values().sum()
    }

    // ── 送信 ────────────────────────────────────────────────────────

    /// メッセージを受信箱へ積む。**この時点では PTY へ書かない**。
    ///
    /// 判定の順番は次のとおり。どれかに引っかかったら理由付きで捨てる。
    /// 1. 自分宛
    /// 2. ホップ数超過
    /// 3. ピンポン検出(検出時はユーザーへエスカレーション)
    /// 4. 一斉送信のレート制限 / ペアのレート制限
    /// 5. 全体のレート制限
    /// 6. 宛先の存在確認
    pub fn enqueue(&mut self, mut msg: AgentMessage) -> SendOutcome {
        let now = msg.at;
        if msg.id == 0 {
            msg.id = self.next_msg_id;
            self.next_msg_id += 1;
        }

        // 1) 自分宛は無意味なので捨てる。
        if msg.from == msg.to {
            let r = DropReason::SelfAddressed;
            self.record_drop(&msg, r);
            return SendOutcome::Dropped { reason: r };
        }

        // 2) 転送ループ止め。
        if msg.hops > self.limits.max_hops {
            let r = DropReason::HopLimit { hops: msg.hops };
            self.record_drop(&msg, r);
            return SendOutcome::Dropped { reason: r };
        }

        // 3) ピンポン検出。ユーザー宛/エスカレーションは対象外
        //    (人を呼ぶ経路まで止めてしまうと異常が見えなくなる)。
        if msg.to != Endpoint::User && msg.kind != MsgKind::Escalation {
            if let Some(r) = self.check_pingpong(&msg, now) {
                self.record_drop(&msg, r);
                return SendOutcome::Dropped { reason: r };
            }
        }

        // 4) レート制限。一斉送信は直接送信よりきつく絞る。
        if msg.to == Endpoint::Broadcast {
            self.broadcast_window.prune(now, self.limits.window);
            if self.broadcast_window.len() >= self.limits.broadcast_limit {
                let r = DropReason::RateLimitBroadcast;
                self.record_drop(&msg, r);
                return SendOutcome::Dropped { reason: r };
            }
        } else if msg.to != Endpoint::User && msg.kind != MsgKind::Escalation {
            let key = (msg.from, msg.to);
            self.prune_pair_tracking();
            let w = self.pair_windows.entry(key).or_default();
            w.prune(now, self.limits.window);
            if w.len() >= self.limits.pair_limit {
                let r = DropReason::RateLimitPair;
                self.record_drop(&msg, r);
                return SendOutcome::Dropped { reason: r };
            }
        }

        // 5) 全体のレート制限(ユーザー宛は人が見る唯一の窓口なので免除)。
        if msg.to != Endpoint::User {
            self.global_window.prune(now, self.limits.window);
            if self.global_window.len() >= self.limits.global_limit {
                let r = DropReason::RateLimitGlobal;
                self.record_drop(&msg, r);
                return SendOutcome::Dropped { reason: r };
            }
        }

        // 6) 宛先の存在確認と投函。
        match msg.to {
            Endpoint::Broadcast => {
                let ids: Vec<SessionId> = {
                    let mut v: Vec<SessionId> = self.mailboxes.keys().copied().collect();
                    v.sort_unstable(); // 決定的に配る
                    v
                };
                let mut n = 0usize;
                for id in ids {
                    if Endpoint::Session(id) == msg.from {
                        continue; // 送信元自身へは返さない
                    }
                    let mut copy = msg.clone();
                    copy.to = Endpoint::Session(id);
                    let evicted = self
                        .mailboxes
                        .get_mut(&id)
                        .and_then(|mb| mb.push(copy));
                    if let Some(old) = evicted {
                        self.record_drop(&old, DropReason::MailboxOverflow);
                    }
                    n += 1;
                }
                self.broadcast_window.push(now);
                self.global_window.push(now);
                self.note_pair(&msg, now);
                SendOutcome::Broadcast {
                    id: msg.id,
                    delivered_to: n,
                }
            }
            Endpoint::Session(id) => {
                if !self.mailboxes.contains_key(&id) {
                    let r = DropReason::UnknownTarget;
                    self.record_drop(&msg, r);
                    return SendOutcome::Dropped { reason: r };
                }
                let mid = msg.id;
                self.global_window.push(now);
                self.note_pair(&msg, now);
                let evicted = self.mailboxes.get_mut(&id).and_then(|mb| mb.push(msg));
                if let Some(old) = evicted {
                    self.record_drop(&old, DropReason::MailboxOverflow);
                }
                SendOutcome::Queued { id: mid }
            }
            Endpoint::Supervisor => {
                let mid = msg.id;
                self.global_window.push(now);
                self.note_pair(&msg, now);
                if let Some(old) = self.supervisor_inbox.push(msg) {
                    self.record_drop(&old, DropReason::MailboxOverflow);
                }
                SendOutcome::Queued { id: mid }
            }
            Endpoint::User => {
                let mid = msg.id;
                if let Some(old) = self.user_inbox.push(msg) {
                    self.record_drop(&old, DropReason::MailboxOverflow);
                }
                SendOutcome::Queued { id: mid }
            }
        }
    }

    /// 送信を記録する(レート制限とピンポン判定の窓へ 1 本足す)。
    fn note_pair(&mut self, msg: &AgentMessage, now: Instant) {
        if msg.to != Endpoint::Broadcast {
            self.pair_windows
                .entry((msg.from, msg.to))
                .or_default()
                .push(now);
        }
        if msg.to != Endpoint::User && msg.kind != MsgKind::Escalation {
            self.pingpong_windows
                .entry(unordered(msg.from, msg.to))
                .or_default()
                .push(now);
        }
    }

    /// ピンポン(2 者が窓内で往復しすぎ)を判定する。
    ///
    /// 検出したら抑制し、ユーザーへエスカレーションする(同じペアで 1 回だけ)。
    fn check_pingpong(&mut self, msg: &AgentMessage, now: Instant) -> Option<DropReason> {
        let key = unordered(msg.from, msg.to);
        let width = self.limits.pingpong_window;
        let limit = self.limits.pingpong_limit;
        let w = self.pingpong_windows.entry(key).or_default();
        w.prune(now, width);
        if w.len() < limit {
            return None;
        }
        if self.pingpong_escalated.insert(key) {
            let body = format!(
                "{} と {} の間で往復が {} 回を超えました。以降のやり取りを抑制しています。",
                endpoint_label(key.0),
                endpoint_label(key.1),
                limit
            );
            let esc = AgentMessage {
                id: self.next_msg_id,
                from: Endpoint::Supervisor,
                to: Endpoint::User,
                kind: MsgKind::Escalation,
                body,
                at: now,
                hops: 0,
            };
            self.next_msg_id += 1;
            // 人を呼ぶ経路は制限を通さず直接積む(再帰しない)。
            if let Some(old) = self.user_inbox.push(esc) {
                self.record_drop(&old, DropReason::MailboxOverflow);
            }
        }
        Some(DropReason::PingPong)
    }

    /// 追跡テーブルが増えすぎたら空になった窓を掃除する(メモリ有界化)。
    fn prune_pair_tracking(&mut self) {
        if self.pair_windows.len() > PAIR_TRACK_CAP {
            self.pair_windows.retain(|_, w| w.len() > 0);
        }
        if self.pingpong_windows.len() > PAIR_TRACK_CAP {
            self.pingpong_windows.retain(|_, w| w.len() > 0);
        }
    }

    /// メッセージを別の宛先へ転送する。ホップ数が 1 増える。
    pub fn forward(&mut self, mut msg: AgentMessage, to: Endpoint, now: Instant) -> SendOutcome {
        msg.hops = msg.hops.saturating_add(1);
        msg.from = msg.to;
        msg.to = to;
        msg.at = now;
        msg.id = 0; // 転送は新しい ID を振る
        self.enqueue(msg)
    }

    // ── 配達 ────────────────────────────────────────────────────────

    /// 注入して安全なセッションへ、**1 セッションにつき 1 通だけ**配達する。
    ///
    /// 1 通ずつにするのは、プロンプトへ連続で流し込んで入力を壊さないため。
    /// 続きは次のフレームで配られる。
    ///
    /// `states` は呼び出し側が毎フレーム組み立てる (セッション ID, 状態) の一覧。
    /// ここに載っていないセッションへは配達しない(状態不明 = 配達しない)。
    pub fn take_deliverable(&mut self, states: &[(SessionId, SessionState)]) -> Vec<Delivery> {
        let mut out = Vec::new();
        for &(id, st) in states {
            if !deliverable(st) {
                continue;
            }
            let Some(mb) = self.mailboxes.get_mut(&id) else {
                continue;
            };
            if let Some(msg) = mb.pop() {
                out.push(Delivery {
                    session: id,
                    msg_id: msg.id,
                    text: format_injection(&msg),
                });
            }
        }
        out
    }

    // ── タスク ──────────────────────────────────────────────────────

    /// タスクを登録する。
    pub fn add_task(
        &mut self,
        title: impl Into<String>,
        description: impl Into<String>,
        required_caps: &[&str],
        now: Instant,
    ) -> TaskId {
        let id = self.next_task_id;
        self.next_task_id += 1;
        let mut t = Task {
            id,
            title: title.into(),
            description: description.into(),
            assigned: None,
            state: TaskState::Pending,
            attempts: 0,
            history: Vec::new(),
            required_caps: required_caps.iter().map(|s| s.to_string()).collect(),
            failed_by: HashSet::new(),
            // まだ誰も持っていないので「前任者は停止済み」と見なす。
            prev_holder_stopped: true,
            context: Vec::new(),
            history_dropped: 0,
        };
        t.record(now, TaskEvent::Created);
        self.tasks.push(t);
        id
    }

    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    fn task_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// 直近の割り当て拒否理由。
    pub fn last_refusal(&self) -> Option<AssignRefusal> {
        self.last_refusal
    }

    /// 引き継ぎ材料を足す(担当が変わっても失われない)。
    pub fn add_context(&mut self, task_id: TaskId, item: impl Into<String>, now: Instant) {
        let Some(t) = self.task_mut(task_id) else {
            return;
        };
        let mut s: String = item.into();
        if s.chars().count() > CONTEXT_ITEM_MAX {
            s = s.chars().take(CONTEXT_ITEM_MAX).collect::<String>() + "…";
        }
        if t.context.len() >= CONTEXT_CAP {
            t.context.remove(0);
        }
        t.context.push(s);
        let n = t.context.len();
        t.record(now, TaskEvent::ContextCarried(n));
    }

    /// 次の担当へ渡す引き継ぎ文。これを [`MsgKind::Handoff`] で送る。
    pub fn handoff_brief(&self, task_id: TaskId) -> Option<String> {
        let t = self.task(task_id)?;
        let mut s = format!(
            "タスク #{} 「{}」を引き継ぎます。{}",
            t.id, t.title, t.description
        );
        if t.attempts > 0 {
            s.push_str(&format!(" (これまでの試行 {} 回)", t.attempts));
        }
        if !t.context.is_empty() {
            s.push_str(" これまでの経過: ");
            s.push_str(&t.context.join(" / "));
        }
        Some(s)
    }

    // ── 割り当て ────────────────────────────────────────────────────

    /// タスクを候補の中から 1 つのセッションへ割り当てる。
    ///
    /// 断った理由は [`Coordinator::last_refusal`] とタスク履歴に残る。
    /// 理由まで受け取りたいときは [`Coordinator::try_assign`] を使う。
    pub fn assign(&mut self, task_id: TaskId, candidates: &[SessionInfo]) -> Option<SessionId> {
        self.try_assign(task_id, candidates, Instant::now()).ok()
    }

    /// 割り当ての本体。断った理由を型で返す。
    ///
    /// 方針:
    /// - 空いているセッションを、忙しいセッションより優先する
    /// - `required_caps` に多く合致するセッションを優先する
    /// - **このタスクで失敗したセッションへは二度と割り当てない**
    /// - `max_attempts` を使い切ったら `NeedsUser` にして、それ以上回さない
    /// - **前任者の停止が未確認なら引き渡さない**(同時編集による破壊を防ぐ)
    /// - 同点なら ID の小さい方(決定的)
    pub fn try_assign(
        &mut self,
        task_id: TaskId,
        candidates: &[SessionInfo],
        now: Instant,
    ) -> Result<SessionId, AssignRefusal> {
        let max_attempts = self.limits.max_attempts;

        // ── 事前条件の確認 ──
        let (refusal, needs_user) = {
            let Some(t) = self.tasks.iter().find(|t| t.id == task_id) else {
                self.last_refusal = Some(AssignRefusal::NoSuchTask);
                return Err(AssignRefusal::NoSuchTask);
            };
            if t.state.is_terminal() {
                (Some(AssignRefusal::TaskFinished), false)
            } else if t.attempts >= max_attempts {
                (
                    Some(AssignRefusal::AttemptsExhausted {
                        attempts: t.attempts,
                    }),
                    true,
                )
            } else if let Some(prev) = t.assigned {
                if !t.prev_holder_stopped {
                    (
                        Some(AssignRefusal::PreviousHolderNotStopped { previous: prev }),
                        false,
                    )
                } else {
                    (None, false)
                }
            } else {
                (None, false)
            }
        };

        if let Some(r) = refusal {
            if needs_user {
                // 無限に回さない。人を呼んで終わりにする。
                let title = self
                    .task(task_id)
                    .map(|t| t.title.clone())
                    .unwrap_or_default();
                if let Some(t) = self.task_mut(task_id) {
                    t.state = TaskState::NeedsUser;
                    t.record(now, TaskEvent::HandoverRefused(r));
                    t.record(
                        now,
                        TaskEvent::EscalatedToUser(
                            "再試行の上限に達したため人手が必要です".into(),
                        ),
                    );
                }
                self.escalate(
                    format!(
                        "タスク #{task_id}「{title}」は再試行の上限に達しました。担当を人が決めてください。"
                    ),
                    now,
                );
            } else if let Some(t) = self.task_mut(task_id) {
                t.record(now, TaskEvent::HandoverRefused(r));
            }
            self.last_refusal = Some(r);
            return Err(r);
        }

        // ── 候補の選定(純粋な方針・決定的) ──
        let (required, failed, previous) = {
            let t = self
                .tasks
                .iter()
                .find(|t| t.id == task_id)
                .expect("事前条件で存在は確認済み");
            (t.required_caps.clone(), t.failed_by.clone(), t.assigned)
        };

        let pick = candidates
            .iter()
            .filter(|c| assignable(c.state))
            .filter(|c| !failed.contains(&c.id))
            .min_by_key(|c| {
                let matched = required.iter().filter(|r| c.caps.contains(r)).count();
                // 能力の合致が多い順 → 空いている順 → ID の小さい順
                (std::cmp::Reverse(matched), busy_rank(c.state), c.id)
            })
            .map(|c| c.id);

        let Some(chosen) = pick else {
            self.last_refusal = Some(AssignRefusal::NoEligibleCandidate);
            if let Some(t) = self.task_mut(task_id) {
                t.record(
                    now,
                    TaskEvent::HandoverRefused(AssignRefusal::NoEligibleCandidate),
                );
            }
            return Err(AssignRefusal::NoEligibleCandidate);
        };

        // ── 確定 ──
        let reason = previous.map(|_| ReassignReason::Manual);
        if let Some(t) = self.task_mut(task_id) {
            t.assigned = Some(chosen);
            t.state = TaskState::Assigned;
            t.attempts = t.attempts.saturating_add(1);
            // 新しい担当が動き出す = また「停止未確認」に戻る。
            t.prev_holder_stopped = false;
            if previous.is_some() {
                t.record(
                    now,
                    TaskEvent::Reassigned {
                        from: previous,
                        to: chosen,
                        reason: reason.unwrap_or(ReassignReason::Manual),
                    },
                );
            }
            t.record(now, TaskEvent::Assigned(chosen));
        }
        self.last_refusal = None;
        Ok(chosen)
    }

    /// 引き継ぎ文をメッセージとして新担当の受信箱へ積む。
    ///
    /// 割り当ての直後に呼ぶ想定。前任者の作業内容を持ち越すことで、
    /// 新担当がゼロからやり直さずに済む。
    pub fn queue_handoff(&mut self, task_id: TaskId, to: SessionId, now: Instant) -> SendOutcome {
        let body = self
            .handoff_brief(task_id)
            .unwrap_or_else(|| format!("タスク #{task_id} を引き継ぎます。"));
        self.enqueue(
            AgentMessage::new(
                Endpoint::Supervisor,
                Endpoint::Session(to),
                MsgKind::Handoff,
                body,
            )
            .at(now),
        )
    }

    // ── 状態通知(監督レイヤから呼ばれる) ────────────────────────────

    /// タスクが動き出した。
    pub fn note_running(&mut self, task_id: TaskId, now: Instant) {
        if let Some(t) = self.task_mut(task_id) {
            if let Some(s) = t.assigned {
                t.state = TaskState::Running;
                t.record(now, TaskEvent::Started(s));
            }
        }
    }

    /// タスクが完了した。
    pub fn note_done(&mut self, task_id: TaskId, now: Instant) {
        if let Some(t) = self.task_mut(task_id) {
            let s = t.assigned.unwrap_or(0);
            t.state = TaskState::Done;
            t.record(now, TaskEvent::Completed(s));
        }
    }

    /// セッションが停滞した。担当中のタスクを `Stalled` にする。
    ///
    /// **停止は確認していない**ので、この時点では引き渡せない。
    /// 引き渡すには [`Coordinator::propose_stop`] → 承認 → 実際に停止 →
    /// [`Coordinator::confirm_stopped`] の順を踏む必要がある。
    pub fn note_stalled(&mut self, session: SessionId, now: Instant) {
        for t in self.tasks.iter_mut() {
            if t.assigned == Some(session) && !t.state.is_terminal() {
                t.state = TaskState::Stalled;
                // 一度停滞した相手へは戻さない。
                t.failed_by.insert(session);
                t.record(now, TaskEvent::Stalled(session));
            }
        }
    }

    /// セッションが失敗を報告した。停止確認は別途必要。
    pub fn note_failed(
        &mut self,
        task_id: TaskId,
        session: SessionId,
        reason: impl Into<String>,
        now: Instant,
    ) {
        if let Some(t) = self.task_mut(task_id) {
            t.state = TaskState::Failed;
            t.failed_by.insert(session);
            t.record(
                now,
                TaskEvent::Failed {
                    session,
                    reason: reason.into(),
                },
            );
        }
    }

    /// セッションのプロセスが消えた(終了 / クラッシュ)。
    ///
    /// プロセスが無い = **停止は確認済み**なので、そのまま引き渡してよい。
    pub fn note_exited(&mut self, session: SessionId, now: Instant) {
        for t in self.tasks.iter_mut() {
            if t.assigned == Some(session) && !t.state.is_terminal() {
                t.state = TaskState::Failed;
                t.failed_by.insert(session);
                t.prev_holder_stopped = true;
                t.record(
                    now,
                    TaskEvent::Failed {
                        session,
                        reason: "セッションが終了した".into(),
                    },
                );
                t.record(now, TaskEvent::PreviousStopped(session));
            }
        }
        self.unregister_session(session);
    }

    /// 前任者を止める提案を作る。既に停止確認済み / 担当不在なら None。
    ///
    /// 返った提案は [`gate_for`] で承認モードのゲートを通してから実行する。
    pub fn propose_stop(&self, task_id: TaskId) -> Option<Proposal> {
        let t = self.task(task_id)?;
        let s = t.assigned?;
        if t.prev_holder_stopped {
            return None;
        }
        Some(Proposal::StopSession {
            session: s,
            task: task_id,
            reason: format!(
                "タスク #{} 「{}」を別のエージェントへ引き継ぐため、前任 session:{} を停止します(作業中の内容が失われる可能性があります)",
                t.id, t.title, s
            ),
        })
    }

    /// 前任者が確実に止まったことを記録する。これで初めて引き渡せる。
    ///
    /// 呼び出し側は、実際に PTY の子プロセスが終了したことを
    /// (`Session::running() == false` などで)確認してから呼ぶこと。
    pub fn confirm_stopped(&mut self, task_id: TaskId, now: Instant) -> bool {
        let Some(t) = self.task_mut(task_id) else {
            return false;
        };
        let Some(s) = t.assigned else {
            return false;
        };
        t.prev_holder_stopped = true;
        t.record(now, TaskEvent::PreviousStopped(s));
        true
    }

    /// 停滞 / 死亡を受けての再割り当て。理由が履歴に残る。
    ///
    /// 前提条件(前任者の停止確認・失敗済みの除外・試行上限)は
    /// [`Coordinator::try_assign`] が全て見る。
    pub fn redispatch(
        &mut self,
        task_id: TaskId,
        candidates: &[SessionInfo],
        reason: ReassignReason,
        now: Instant,
    ) -> Result<SessionId, AssignRefusal> {
        let previous = self.task(task_id).and_then(|t| t.assigned);
        let chosen = self.try_assign(task_id, candidates, now)?;
        if let Some(t) = self.task_mut(task_id) {
            // try_assign が積んだ汎用の記録を、具体的な理由で上書き補足する。
            t.record(
                now,
                TaskEvent::Reassigned {
                    from: previous,
                    to: chosen,
                    reason,
                },
            );
        }
        // 引き継ぎ材料を新担当へ渡す。
        self.queue_handoff(task_id, chosen, now);
        Ok(chosen)
    }

    /// ユーザーへ上げる。レート制限を通さずに直接積む(人を呼ぶ経路は塞がない)。
    pub fn escalate(&mut self, body: impl Into<String>, now: Instant) -> u64 {
        let id = self.next_msg_id;
        self.next_msg_id += 1;
        let msg = AgentMessage {
            id,
            from: Endpoint::Supervisor,
            to: Endpoint::User,
            kind: MsgKind::Escalation,
            body: body.into(),
            at: now,
            hops: 0,
        };
        if let Some(old) = self.user_inbox.push(msg) {
            self.record_drop(&old, DropReason::MailboxOverflow);
        }
        id
    }
}

// ── テスト ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn msg(from: SessionId, to: SessionId, at: Instant) -> AgentMessage {
        AgentMessage::new(
            Endpoint::Session(from),
            Endpoint::Session(to),
            MsgKind::Request,
            "テスト本文",
        )
        .at(at)
    }

    // ── 配達の安全性 ────────────────────────────────────────────────

    /// 安全な状態の集合が明示的であること。
    #[test]
    fn deliverable_safe_set_is_explicit() {
        assert!(deliverable(SessionState::Idle));
        assert!(deliverable(SessionState::AwaitingInput));
        assert!(!deliverable(SessionState::Working));
        assert!(!deliverable(SessionState::WaitingApproval));
        assert!(!deliverable(SessionState::Stalled));
        assert!(!deliverable(SessionState::Exited));
        // 状態が分からないときは既定で配達しない。
        assert!(!deliverable(SessionState::Unknown));
    }

    /// 生成中のセッションへは配達しない(PTY へ書くと入力が壊れる)。
    #[test]
    fn no_delivery_while_working() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        let now = t0();
        assert!(matches!(
            c.enqueue(msg(1, 2, now)),
            SendOutcome::Queued { .. }
        ));

        let out = c.take_deliverable(&[(2, SessionState::Working)]);
        assert!(out.is_empty(), "作業中に配達してはいけない");
        // メッセージは消えずに残っている。
        assert_eq!(c.mailbox(2).unwrap().len(), 1);
    }

    /// 承認待ちへは絶対に配達しない。
    /// 本文がそのまま承認の返事として解釈されてしまうため。
    #[test]
    fn no_delivery_while_waiting_approval() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        let now = t0();
        c.enqueue(msg(1, 2, now));

        let out = c.take_deliverable(&[(2, SessionState::WaitingApproval)]);
        assert!(out.is_empty(), "承認待ちに配達してはいけない");
        assert_eq!(c.mailbox(2).unwrap().len(), 1);
        assert_eq!(c.mailbox(2).unwrap().delivered(), 0);
    }

    /// 状態が一覧に無い(= 不明な)セッションへは配達しない。
    #[test]
    fn no_delivery_when_state_unknown() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        c.enqueue(msg(1, 2, t0()));

        assert!(c.take_deliverable(&[]).is_empty());
        assert!(c
            .take_deliverable(&[(2, SessionState::Unknown)])
            .is_empty());
        assert_eq!(c.mailbox(2).unwrap().len(), 1);
    }

    /// 待機中なら配達される。注入行には目印が付く。
    #[test]
    fn delivers_when_idle() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        c.enqueue(msg(1, 2, t0()));

        let out = c.take_deliverable(&[(2, SessionState::Idle)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session, 2);
        assert!(out[0].text.starts_with(INJECT_PREFIX), "機械注入の目印が要る");
        assert!(out[0].text.ends_with('\r'), "1 ターンとして確定させる");
        assert!(out[0].text.contains("session:1"));
        assert_eq!(c.mailbox(2).unwrap().len(), 0);
        assert_eq!(c.mailbox(2).unwrap().delivered(), 1);
    }

    /// 1 フレームで 1 セッションにつき 1 通だけ配る(連打で入力を壊さない)。
    #[test]
    fn delivers_one_per_session_per_call() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        let now = t0();
        c.enqueue(msg(1, 2, now));
        c.enqueue(msg(1, 2, now));

        let out = c.take_deliverable(&[(2, SessionState::Idle)]);
        assert_eq!(out.len(), 1);
        assert_eq!(c.mailbox(2).unwrap().len(), 1);
    }

    /// 注入本文の改行と制御文字は潰す(途中で送信されてしまうのを防ぐ)。
    #[test]
    fn injection_body_is_single_line() {
        let m = AgentMessage::new(
            Endpoint::Supervisor,
            Endpoint::Session(1),
            MsgKind::Status,
            "一行目\n二行目\r\n三行目\x07",
        );
        let text = format_injection(&m);
        assert_eq!(text.matches('\r').count(), 1, "末尾の CR 以外に CR は無い");
        assert!(!text.contains('\n'));
        assert!(text.contains("一行目 / 二行目 / 三行目"));
    }

    // ── ループとストーム抑制 ─────────────────────────────────────────

    /// ホップ上限を超えた転送は捨て、理由を記録する。
    #[test]
    fn hop_limit_drops_forwarded_message_with_reason() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        c.register_session(3);
        let now = t0();

        let mut m = msg(1, 2, now);
        m.hops = c.limits().max_hops; // 次の転送で上限超過
        let out = c.forward(m, Endpoint::Session(3), now);

        match out {
            SendOutcome::Dropped {
                reason: DropReason::HopLimit { hops },
            } => assert_eq!(hops, DEFAULT_MAX_HOPS + 1),
            other => panic!("ホップ上限で捨てるはず: {other:?}"),
        }
        assert_eq!(c.drop_count(DropKind::HopLimit), 1);
        let rec = c.drop_log().last().expect("破棄ログが残る");
        assert_eq!(rec.reason.kind(), DropKind::HopLimit);
        assert!(!rec.reason.label().is_empty());
        // 宛先には届いていない。
        assert_eq!(c.mailbox(3).unwrap().len(), 0);
    }

    /// 上限内の転送は通る(上限そのものが誤爆していないことの確認)。
    #[test]
    fn forward_below_hop_limit_passes() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        c.register_session(3);
        let now = t0();
        let m = msg(1, 2, now); // hops = 0 → 転送で 1
        assert!(matches!(
            c.forward(m, Endpoint::Session(3), now),
            SendOutcome::Queued { .. }
        ));
        assert_eq!(c.drop_count(DropKind::HopLimit), 0);
    }

    /// ピンポンを検出したら抑制し、ユーザーへエスカレーションする。
    #[test]
    fn pingpong_suppressed_and_escalated_to_user() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        let base = t0();

        // 交互に往復させる。しきい値は「向き無視の合計本数」。
        let limit = c.limits().pingpong_limit;
        for i in 0..limit {
            let at = base + Duration::from_millis(100 * i as u64);
            let out = if i % 2 == 0 {
                c.enqueue(msg(1, 2, at))
            } else {
                c.enqueue(msg(2, 1, at))
            };
            assert!(
                matches!(out, SendOutcome::Queued { .. }),
                "{i} 本目までは通るはず"
            );
        }
        assert_eq!(c.drop_count(DropKind::PingPong), 0);

        // しきい値到達後の 1 本は抑制される。
        let at = base + Duration::from_millis(100 * limit as u64);
        let out = c.enqueue(msg(1, 2, at));
        assert_eq!(
            out,
            SendOutcome::Dropped {
                reason: DropReason::PingPong
            }
        );
        assert_eq!(c.drop_count(DropKind::PingPong), 1);

        // ユーザーへ 1 通だけエスカレーションされている。
        let esc = c.take_user_messages();
        assert_eq!(esc.len(), 1, "人へ上げるのはペアにつき 1 回");
        assert_eq!(esc[0].to, Endpoint::User);
        assert_eq!(esc[0].kind, MsgKind::Escalation);
        assert!(esc[0].body.contains("session:1"));
        assert!(esc[0].body.contains("session:2"));
    }

    /// 同一ペアのレート制限。抑制されるだけでなく、件数が数えられている。
    #[test]
    fn pair_rate_limit_suppresses_and_counts() {
        // ピンポン判定と混ざらないよう、片方向だけへ送る構成にする。
        let limits = Limits {
            pingpong_limit: 1_000,
            ..Limits::default()
        };
        let mut c = Coordinator::with_limits(limits);
        c.register_session(1);
        c.register_session(2);
        let base = t0();
        let pair_limit = c.limits().pair_limit;

        for i in 0..pair_limit {
            let at = base + Duration::from_millis(10 * i as u64);
            assert!(matches!(
                c.enqueue(msg(1, 2, at)),
                SendOutcome::Queued { .. }
            ));
        }
        // 超過分は 3 本まとめて捨てられ、3 と数えられる。
        for i in 0..3u32 {
            let at = base + Duration::from_millis(1000 + i as u64);
            assert_eq!(
                c.enqueue(msg(1, 2, at)),
                SendOutcome::Dropped {
                    reason: DropReason::RateLimitPair
                }
            );
        }
        assert_eq!(c.drop_count(DropKind::RateLimitPair), 3, "捨てた数を数える");
        assert_eq!(c.total_drops(), 3);
        assert_eq!(c.drop_log().count(), 3);

        // 窓が過ぎればまた通る。
        let later = base + DEFAULT_WINDOW + Duration::from_secs(1);
        assert!(matches!(
            c.enqueue(msg(1, 2, later)),
            SendOutcome::Queued { .. }
        ));
    }

    /// 一斉送信は直接送信よりきつく絞る。
    #[test]
    fn broadcast_is_limited_harder_than_direct() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        c.register_session(3);
        let base = t0();
        assert!(c.limits().broadcast_limit < c.limits().pair_limit);

        for i in 0..c.limits().broadcast_limit {
            let at = base + Duration::from_millis(10 * i as u64);
            let out = c.enqueue(
                AgentMessage::new(
                    Endpoint::Session(1),
                    Endpoint::Broadcast,
                    MsgKind::Status,
                    "全員へ",
                )
                .at(at),
            );
            // 送信元 1 を除く 2 つの受信箱へ入る。
            assert_eq!(
                out,
                SendOutcome::Broadcast {
                    id: i as u64 + 1,
                    delivered_to: 2
                }
            );
        }
        let at = base + Duration::from_millis(500);
        assert_eq!(
            c.enqueue(
                AgentMessage::new(
                    Endpoint::Session(1),
                    Endpoint::Broadcast,
                    MsgKind::Status,
                    "全員へ",
                )
                .at(at)
            ),
            SendOutcome::Dropped {
                reason: DropReason::RateLimitBroadcast
            }
        );
        assert_eq!(c.drop_count(DropKind::RateLimitBroadcast), 1);
    }

    /// 全体のレート制限も数えられる。
    #[test]
    fn global_rate_limit_counts() {
        let limits = Limits {
            global_limit: 3,
            pair_limit: 1_000,
            pingpong_limit: 1_000,
            ..Limits::default()
        };
        let mut c = Coordinator::with_limits(limits);
        c.register_session(1);
        c.register_session(2);
        let base = t0();
        for i in 0..3 {
            let at = base + Duration::from_millis(i);
            assert!(matches!(
                c.enqueue(msg(1, 2, at)),
                SendOutcome::Queued { .. }
            ));
        }
        assert_eq!(
            c.enqueue(msg(1, 2, base + Duration::from_millis(10))),
            SendOutcome::Dropped {
                reason: DropReason::RateLimitGlobal
            }
        );
        assert_eq!(c.drop_count(DropKind::RateLimitGlobal), 1);
    }

    /// 存在しないセッション宛は理由付きで捨てる(黙って消さない)。
    #[test]
    fn unknown_target_is_recorded() {
        let mut c = Coordinator::new();
        c.register_session(1);
        assert_eq!(
            c.enqueue(msg(1, 99, t0())),
            SendOutcome::Dropped {
                reason: DropReason::UnknownTarget
            }
        );
        assert_eq!(c.drop_count(DropKind::UnknownTarget), 1);
    }

    // ── メールボックス ──────────────────────────────────────────────

    /// 受信箱は上限を超えたら古いものから捨て、その件数を報告する。
    #[test]
    fn mailbox_ring_drops_oldest_and_counts() {
        let limits = Limits {
            mailbox_cap: 4,
            pair_limit: 1_000,
            global_limit: 1_000,
            pingpong_limit: 1_000,
            ..Limits::default()
        };
        let mut c = Coordinator::with_limits(limits);
        c.register_session(1);
        c.register_session(2);
        let base = t0();

        let mut ids = Vec::new();
        for i in 0..7u64 {
            let at = base + Duration::from_millis(i);
            match c.enqueue(msg(1, 2, at)) {
                SendOutcome::Queued { id } => ids.push(id),
                other => panic!("積まれるはず: {other:?}"),
            }
        }

        let mb = c.mailbox(2).unwrap();
        assert_eq!(mb.len(), 4, "上限を超えて伸びない");
        assert_eq!(mb.dropped_oldest(), 3, "溢れて捨てた件数を数える");
        // 残っているのは新しい 4 通(古い 3 通が押し出された)。
        let remaining: Vec<u64> = mb.iter().map(|m| m.id).collect();
        assert_eq!(remaining, ids[3..].to_vec());
        // 押し出しも破棄ログに理由付きで残る。
        assert_eq!(c.drop_count(DropKind::MailboxOverflow), 3);
    }

    // ── 割り当て ────────────────────────────────────────────────────

    fn cands() -> Vec<SessionInfo> {
        vec![
            SessionInfo::new(1, SessionState::Idle, &["rust"]),
            SessionInfo::new(2, SessionState::Working, &["rust", "test"]),
            SessionInfo::new(3, SessionState::Idle, &["docs"]),
        ]
    }

    /// 能力が合致し、かつ空いているセッションを選ぶ。
    #[test]
    fn assign_prefers_capable_then_idle() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("実装", "rust を書く", &["rust"], now);
        // 1 と 2 が rust 持ち。合致数は同じ 1 なので、空いている 1 が勝つ。
        assert_eq!(c.try_assign(t, &cands(), now), Ok(1));
    }

    /// 能力の合致数が多い方を、忙しくても優先する。
    #[test]
    fn assign_prefers_more_capability_matches() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("実装", "rust とテスト", &["rust", "test"], now);
        // 2 は 2 つ合致(作業中)、1 は 1 つ合致(空き)→ 合致数が優先。
        assert_eq!(c.try_assign(t, &cands(), now), Ok(2));
    }

    /// 同じ入力なら必ず同じ結果になる(決定的)。
    #[test]
    fn assign_is_deterministic() {
        let now = t0();
        // 完全に横並びの候補。並び順を変えても結果が変わらないこと。
        let a = vec![
            SessionInfo::new(7, SessionState::Idle, &["rust"]),
            SessionInfo::new(3, SessionState::Idle, &["rust"]),
            SessionInfo::new(5, SessionState::Idle, &["rust"]),
        ];
        let mut b = a.clone();
        b.reverse();

        for _ in 0..10 {
            let mut c1 = Coordinator::new();
            let t1 = c1.add_task("T", "d", &["rust"], now);
            let mut c2 = Coordinator::new();
            let t2 = c2.add_task("T", "d", &["rust"], now);
            // 同点なら ID の小さい方。
            assert_eq!(c1.try_assign(t1, &a, now), Ok(3));
            assert_eq!(c2.try_assign(t2, &b, now), Ok(3));
        }
    }

    /// 終了済み / 停滞中のセッションへは割り当てない。
    #[test]
    fn assign_skips_unusable_sessions() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("T", "d", &[], now);
        let list = vec![
            SessionInfo::new(1, SessionState::Exited, &[]),
            SessionInfo::new(2, SessionState::Stalled, &[]),
            SessionInfo::new(3, SessionState::Unknown, &[]),
            SessionInfo::new(4, SessionState::Idle, &[]),
        ];
        assert_eq!(c.try_assign(t, &list, now), Ok(4));
    }

    /// そのタスクで一度失敗したセッションへは、二度と割り当てない。
    #[test]
    fn assign_never_returns_previously_failed_session() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("T", "d", &["rust"], now);

        // 1 に割り当てて失敗させる。
        assert_eq!(c.try_assign(t, &cands(), now), Ok(1));
        c.note_failed(t, 1, "ビルドが通らない", now);
        c.confirm_stopped(t, now);
        assert!(c.task(t).unwrap().has_failed(1));

        // 1 は候補に残っているが選ばれない。
        let next = c.try_assign(t, &cands(), now).expect("2 が選ばれる");
        assert_ne!(next, 1, "失敗したセッションへ戻してはいけない");
        assert_eq!(next, 2);

        // 2 も失敗 → 残るのは 3 のみ。
        c.note_failed(t, 2, "タイムアウト", now);
        c.confirm_stopped(t, now);
        let third = c.try_assign(t, &cands(), now).expect("3 が選ばれる");
        assert_eq!(third, 3);
    }

    /// **前任者の停止が未確認なら引き渡さない**(同時編集で成果物が壊れる)。
    #[test]
    fn assign_refuses_handover_when_previous_not_stopped() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("T", "d", &[], now);
        assert_eq!(c.try_assign(t, &cands(), now), Ok(1));

        // 停滞しただけ。プロセスはまだ生きているかもしれない。
        c.note_stalled(1, now);
        assert!(!c.task(t).unwrap().previous_stopped());

        let refusal = c.try_assign(t, &cands(), now).unwrap_err();
        assert_eq!(
            refusal,
            AssignRefusal::PreviousHolderNotStopped { previous: 1 }
        );
        assert_eq!(c.last_refusal(), Some(refusal));
        // 担当は変わっていない。
        assert_eq!(c.task(t).unwrap().assigned, Some(1));
        // 拒否の事実が履歴に残る。
        assert!(c
            .task(t)
            .unwrap()
            .history
            .iter()
            .any(|(_, e)| matches!(e, TaskEvent::HandoverRefused(_))));

        // 停止を提案 → 承認モードのゲートを通す。
        let p = c.propose_stop(t).expect("停止提案が出る");
        match &p {
            Proposal::StopSession { session, task, .. } => {
                assert_eq!(*session, 1);
                assert_eq!(*task, t);
            }
        }
        // 破壊的操作なので Ask / Agent では人の確認が要る。
        assert_eq!(
            gate_for(PermissionMode::Ask),
            ProposalGate::NeedsUserConfirm
        );
        assert_eq!(
            gate_for(PermissionMode::Agent),
            ProposalGate::NeedsUserConfirm
        );
        assert_eq!(gate_for(PermissionMode::Auto), ProposalGate::AutoApproved);

        // 実際に停止を確認して初めて引き渡せる。
        // 能力指定が無いので、作業中の 2 ではなく空いている 3 が選ばれる。
        assert!(c.confirm_stopped(t, now));
        assert_eq!(c.try_assign(t, &cands(), now), Ok(3));
    }

    /// プロセスが消えた場合は停止確認済みとして扱い、すぐ引き渡せる。
    #[test]
    fn dead_session_counts_as_confirmed_stopped() {
        let mut c = Coordinator::new();
        let now = t0();
        c.register_session(1);
        let t = c.add_task("T", "d", &[], now);
        assert_eq!(c.try_assign(t, &cands(), now), Ok(1));

        c.note_exited(1, now);
        assert!(c.task(t).unwrap().previous_stopped());
        assert_eq!(c.task(t).unwrap().state, TaskState::Failed);
        // 停止提案は不要。
        assert!(c.propose_stop(t).is_none());
        // 能力指定が無いので空いている 3 が選ばれる(2 は作業中)。
        assert_eq!(c.try_assign(t, &cands(), now), Ok(3));
    }

    /// 再試行の上限に達したら NeedsUser にして、それ以上回さない。
    #[test]
    fn max_attempts_exhaustion_yields_needs_user() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("T", "d", &[], now);
        let list = vec![
            SessionInfo::new(1, SessionState::Idle, &[]),
            SessionInfo::new(2, SessionState::Idle, &[]),
            SessionInfo::new(3, SessionState::Idle, &[]),
            SessionInfo::new(4, SessionState::Idle, &[]),
        ];

        // 既定の上限は 3 回。
        assert_eq!(c.limits().max_attempts, DEFAULT_MAX_ATTEMPTS);
        for expected in 1..=DEFAULT_MAX_ATTEMPTS {
            let s = c.try_assign(t, &list, now).expect("上限までは割り当てられる");
            assert_eq!(s, expected as u64);
            c.note_failed(t, s, "失敗", now);
            c.confirm_stopped(t, now);
        }
        assert_eq!(c.task(t).unwrap().attempts, DEFAULT_MAX_ATTEMPTS);

        // 4 回目は拒否され、タスクは人手待ちになる。
        let err = c.try_assign(t, &list, now).unwrap_err();
        assert_eq!(
            err,
            AssignRefusal::AttemptsExhausted {
                attempts: DEFAULT_MAX_ATTEMPTS
            }
        );
        assert_eq!(c.task(t).unwrap().state, TaskState::NeedsUser);

        // ユーザーへ上がっている。
        let esc = c.take_user_messages();
        assert_eq!(esc.len(), 1);
        assert_eq!(esc[0].kind, MsgKind::Escalation);

        // 何度呼んでも回り続けない(状態は NeedsUser のまま)。
        for _ in 0..5 {
            assert!(c.try_assign(t, &list, now).is_err());
        }
        assert_eq!(c.task(t).unwrap().state, TaskState::NeedsUser);
        assert_eq!(c.task(t).unwrap().attempts, DEFAULT_MAX_ATTEMPTS);
    }

    /// 候補がいなければ理由付きで断る(誰かに無理やり押し付けない)。
    #[test]
    fn assign_refuses_when_no_candidate() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("T", "d", &[], now);
        assert_eq!(
            c.try_assign(t, &[], now),
            Err(AssignRefusal::NoEligibleCandidate)
        );
        assert_eq!(c.last_refusal(), Some(AssignRefusal::NoEligibleCandidate));
    }

    /// 再割り当ての理由と、引き継ぎ材料の持ち越しが記録される。
    #[test]
    fn redispatch_records_reason_and_carries_context() {
        let mut c = Coordinator::new();
        let now = t0();
        c.register_session(1);
        c.register_session(2);
        c.register_session(3);
        let t = c.add_task("移植", "パーサを移植する", &[], now);
        assert_eq!(c.try_assign(t, &cands(), now), Ok(1));

        // 前任が積み上げた成果。
        c.add_context(t, "lexer.rs は移植済み", now);
        c.add_context(t, "parser.rs は途中(式まで)", now);

        c.note_exited(1, now); // 落ちた → 停止確認済み
        // 能力指定が無いので、空いている 3 が新担当になる(2 は作業中)。
        let next = c
            .redispatch(t, &cands(), ReassignReason::SessionDied, now)
            .expect("3 へ引き継ぐ");
        assert_eq!(next, 3);

        // 理由付きで履歴に残る。
        let task = c.task(t).unwrap();
        assert!(task.history.iter().any(|(_, e)| matches!(
            e,
            TaskEvent::Reassigned {
                reason: ReassignReason::SessionDied,
                to: 3,
                ..
            }
        )));

        // 引き継ぎ文が新担当の受信箱に積まれ、経過が入っている。
        let mb = c.mailbox(3).unwrap();
        assert_eq!(mb.len(), 1);
        let handoff = mb.iter().next().unwrap();
        assert_eq!(handoff.kind, MsgKind::Handoff);
        assert!(handoff.body.contains("lexer.rs は移植済み"));
        assert!(handoff.body.contains("parser.rs は途中"));

        // 待機中になれば配達される。
        let out = c.take_deliverable(&[(3, SessionState::Idle)]);
        assert_eq!(out.len(), 1);
        assert!(out[0].text.contains("引き継ぎ"));
    }

    /// 履歴とコンテキストは無限に伸びない。
    #[test]
    fn task_history_and_context_are_bounded() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("T", "d", &[], now);
        for i in 0..(CONTEXT_CAP + HISTORY_CAP + 20) {
            c.add_context(t, format!("経過 {i}"), now);
        }
        let task = c.task(t).unwrap();
        assert_eq!(task.context().len(), CONTEXT_CAP);
        assert_eq!(task.history.len(), HISTORY_CAP);
        assert!(task.history_dropped() > 0);
    }

    /// 自分宛は捨てる。
    #[test]
    fn self_addressed_is_dropped() {
        let mut c = Coordinator::new();
        c.register_session(1);
        assert_eq!(
            c.enqueue(msg(1, 1, t0())),
            SendOutcome::Dropped {
                reason: DropReason::SelfAddressed
            }
        );
        assert_eq!(c.drop_count(DropKind::SelfAddressed), 1);
    }

    /// ユーザーへのエスカレーションはレート制限で塞がない。
    #[test]
    fn escalation_to_user_is_never_rate_limited() {
        let limits = Limits {
            global_limit: 1,
            pair_limit: 1,
            ..Limits::default()
        };
        let mut c = Coordinator::with_limits(limits);
        let now = t0();
        for i in 0..10 {
            c.escalate(format!("異常 {i}"), now);
        }
        // 受信箱の上限までは残る。捨てるとしても理由付き。
        assert!(!c.user_inbox().is_empty());
        assert_eq!(c.take_user_messages().len(), 10);
    }

    // ── 発信マーカーの解析 ───────────────────────────────────────────

    #[test]
    fn outbound_marker_parses_target_and_body() {
        let (to, body) = parse_outbound("[ZAI-TO:backend] テストを直してほしい").unwrap();
        assert_eq!(to, "backend");
        assert_eq!(body, "テストを直してほしい");
    }

    #[test]
    fn outbound_marker_accepts_all() {
        let (to, body) = parse_outbound("[ZAI-TO:ALL] 全員へ連絡").unwrap();
        assert_eq!(to, OUTBOUND_ALL);
        assert_eq!(body, "全員へ連絡");
    }

    /// 端末は行末を空白で埋めるので、そこだけは許す。
    #[test]
    fn outbound_marker_tolerates_trailing_padding() {
        let (to, body) = parse_outbound("[ZAI-TO:a] やあ   \r").unwrap();
        assert_eq!(to, "a");
        assert_eq!(body, "やあ");
    }

    /// **行頭でしか拾わない**。引用・プロンプト内の文字列で誤爆させない。
    #[test]
    fn outbound_marker_is_line_start_only() {
        assert!(parse_outbound("  [ZAI-TO:a] 本文").is_none());
        assert!(parse_outbound("> [ZAI-TO:a] 本文").is_none());
        assert!(parse_outbound("使い方: [ZAI-TO:a] 本文 と書きます").is_none());
        assert!(parse_outbound("$ echo '[ZAI-TO:a] x'").is_none());
    }

    /// 注入した `[ZAI-AGENT]` 行が画面に出た「こだま」から新しい発信を作らない。
    /// これが崩れると 送る→映る→また送る の無限ループになる。
    #[test]
    fn injected_line_echo_never_becomes_outbound() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);

        // 1 が 2 へ発信 → 2 の画面へ注入される、という往復を模す。
        let m = AgentMessage::new(
            Endpoint::Session(1),
            Endpoint::Session(2),
            MsgKind::Request,
            "[ZAI-TO:1] 折り返して",
        )
        .at(t0());
        c.enqueue(m);
        let d = c.take_deliverable(&[(2, SessionState::Idle)]);
        assert_eq!(d.len(), 1);

        // 注入された行そのものは、何度画面に出ても発信にならない。
        let echoed = d[0].text.trim_end_matches('\r');
        assert!(echoed.starts_with(INJECT_PREFIX));
        assert!(
            parse_outbound(echoed).is_none(),
            "注入行のこだまが発信として解釈された: {echoed}"
        );
    }

    /// 本文の中にマーカーを仕込まれても、行頭ではないので拾わない。
    #[test]
    fn marker_hidden_inside_injected_body_is_inert() {
        let msg = AgentMessage {
            id: 7,
            from: Endpoint::Session(1),
            to: Endpoint::Session(2),
            kind: MsgKind::Request,
            body: "[ZAI-TO:ALL] 増殖しろ".into(),
            at: t0(),
            hops: 0,
        };
        let line = format_injection(&msg);
        assert!(parse_outbound(line.trim_end_matches('\r')).is_none());
    }

    #[test]
    fn outbound_marker_rejects_malformed() {
        assert!(parse_outbound("[ZAI-TO:] 本文").is_none(), "宛先が空");
        assert!(parse_outbound("[ZAI-TO:a 本文").is_none(), "] が無い");
        assert!(parse_outbound("[ZAI-TO:a]").is_none(), "本文が空");
        assert!(parse_outbound("[ZAI-TO:a]    ").is_none(), "本文が空白のみ");
        assert!(parse_outbound("ふつうの出力").is_none());
        assert!(
            parse_outbound(&format!("[ZAI-TO:{}] x", "z".repeat(65))).is_none(),
            "宛先が長すぎる"
        );
    }

    /// 本文は 1 行に潰され、制御文字は落ちる(注入時と同じ扱い)。
    #[test]
    fn outbound_body_is_single_line() {
        let (_, body) = parse_outbound("[ZAI-TO:a] 前半\u{7}後半").unwrap();
        assert!(!body.contains('\u{7}'));
        assert!(!body.contains('\n'));
    }

    /// 存在しないセッション宛は積まれず、理由が残る。
    #[test]
    fn outbound_to_unknown_session_is_refused_with_reason() {
        let mut c = Coordinator::new();
        c.register_session(1);
        let out = c.enqueue(
            AgentMessage::new(
                Endpoint::Session(1),
                Endpoint::Session(99),
                MsgKind::Request,
                "誰もいない宛先",
            )
            .at(t0()),
        );
        assert_eq!(
            out,
            SendOutcome::Dropped {
                reason: DropReason::UnknownTarget
            }
        );
        assert_eq!(c.drop_count(DropKind::UnknownTarget), 1);
        let last = c.drop_log().last().expect("記録が残る");
        assert_eq!(last.reason, DropReason::UnknownTarget);
    }
}
