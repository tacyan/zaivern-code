//! 調停レイヤ (coordinator) を実際に動かす層 — UI と、橋渡しの手順。
//!
//! `coordinator.rs` は「何をしてよいか」の規則だけを持つ純粋な部品で、
//! 誰もそれを叩かなければ何も起きない。ここがその叩き手にあたる。
//!
//! ## 置き場所の方針
//!
//! `app.rs` を太らせないため、**判断も描画もすべてこのモジュールに置く**。
//! `app.rs` 側に残すのは「状態を 1 つ持つ」「1 か所から呼ぶ」だけ。
//! そのため、このモジュールは `ZaivernApp` の中身に一切触らない。
//! 必要なものは [`SessionRow`] という要約に写して受け取り、
//! やってほしいことは [`Effects`] として返す。副作用は呼び出し側が実行する。
//!
//! ## 安全の要
//!
//! 再割り当ての順序は **停止提案 → 承認ゲート → kill → プロセス消滅の確認 →
//! `confirm_stopped` → `redispatch`** で、途中を飛ばさない。
//! 前任者が止まったと確認できていないタスクを新しい担当へ渡すと、
//! 2 人が同じファイルを同時に編集して成果物が壊れる。
//! `coordinator` 側はそれを [`coordinator::AssignRefusal::PreviousHolderNotStopped`]
//! で断るので、ここは**その拒否を回避せず、順序を守って進めるだけ**。

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align2, RichText};

use crate::coordinator::{
    self, AgentMessage, AssignRefusal, Endpoint, MsgKind, ReassignReason, SendOutcome, SessionId,
    SessionInfo, SessionState, Task, TaskId, TaskState,
};
use crate::theme::Theme;

// ── 上限 ─────────────────────────────────────────────────────────────

/// 1 セッションが発信マーカーを出してよい窓の長さ。
pub const OUTBOUND_WINDOW: Duration = Duration::from_secs(20);

/// その窓の中で許す発信数。暴走したエージェントに受信箱を埋めさせない。
pub const OUTBOUND_PER_WINDOW: u32 = 4;

/// 「この行はもう見た」の記憶数 (セッションごと)。画面は繰り返し同じ行を映すので、
/// これが無いと 1 行の発信が毎フレーム再送されてしまう。
const SEEN_LINES_CAP: usize = 400;

/// 画面を走査する間隔。UI スレッドで舐めるので毎フレームはやらない。
const SCAN_INTERVAL: Duration = Duration::from_millis(400);

/// 再割り当てに失敗したタスクを、次に試すまでの待ち時間。
const REDISPATCH_BACKOFF: Duration = Duration::from_secs(5);

// ── app 側から受け取る要約 ───────────────────────────────────────────

/// 生きているセッション 1 つ分の要約。`terminal::Session` には依存しない。
#[derive(Clone, Debug)]
pub struct SessionRow {
    pub id: SessionId,
    pub title: String,
    pub running: bool,
    /// 調停レイヤから見た状態 (`app::coordinator_state` の結果)。
    pub state: SessionState,
}

impl SessionRow {
    /// 割り当て候補としての姿。能力はセッション名を 1 つの能力として申告する
    /// (「backend」という名前のセッションは `backend` ができる、という素朴な規約)。
    fn as_info(&self) -> SessionInfo {
        let cap = self.title.to_lowercase();
        SessionInfo::new(self.id, self.state, &[cap.as_str()])
    }
}

// ── バスの様子 ───────────────────────────────────────────────────────

/// 受信箱と破棄の要約。**抑制されたメッセージを人の目に見せる**ためにある。
#[derive(Clone, Debug, Default)]
pub struct BusStatus {
    /// まだ配達されていない通数の合計。
    pub queued: usize,
    /// 溢れて捨てた累計。
    pub overflowed: u32,
    /// 破棄の累計。
    pub drops: u32,
    /// 直近の破棄の説明 (日本語)。
    pub last_drop: Option<String>,
}

/// バスの様子を集める。
pub fn bus_status(co: &coordinator::Coordinator, rows: &[SessionRow]) -> BusStatus {
    let mut st = BusStatus {
        drops: co.total_drops(),
        ..BusStatus::default()
    };
    for r in rows {
        if let Some(mb) = co.mailbox(r.id) {
            st.queued += mb.len();
            st.overflowed = st.overflowed.saturating_add(mb.dropped_oldest());
        }
    }
    st.last_drop = co.drop_log().last().map(|d| {
        format!(
            "#{} {} → {}: {}",
            d.msg_id,
            describe_endpoint(rows, d.from),
            describe_endpoint(rows, d.to),
            d.reason.label()
        )
    });
    st
}

/// 候補一覧を作る。
fn candidates(rows: &[SessionRow]) -> Vec<SessionInfo> {
    rows.iter().filter(|r| r.running).map(|r| r.as_info()).collect()
}

// ── 呼び出し側にやってもらう副作用 ───────────────────────────────────

/// このモジュールが返す「やってほしいこと」。`app.rs` がそのまま実行する。
#[derive(Default, Debug)]
pub struct Effects {
    /// トースト行。`bool` は成功表示か (false は警告色)。
    pub toasts: Vec<(String, bool)>,
    /// PTY へ直接書く文字列 (セッション ID, 本文)。
    pub writes: Vec<(SessionId, String)>,
}

impl Effects {
    fn ok(&mut self, s: impl Into<String>) {
        self.toasts.push((s.into(), true));
    }
    fn warn(&mut self, s: impl Into<String>) {
        self.toasts.push((s.into(), false));
    }
    fn merge(&mut self, other: Effects) {
        self.toasts.extend(other.toasts);
        self.writes.extend(other.writes);
    }
}

// ── フォームの状態 ───────────────────────────────────────────────────

/// タスクの担当先。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TaskTarget {
    /// 能力と空き具合から調停レイヤに選ばせる。
    #[default]
    Auto,
    /// このセッションに任せる。
    Session(SessionId),
}

/// メッセージの宛先。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MsgTarget {
    /// 全セッションへ (送信元は除く)。
    #[default]
    Broadcast,
    Session(SessionId),
}

/// UI から出てくる要求。描画中は実行できないので、いったんこの形で取り出す。
#[derive(Clone, Debug)]
pub enum OrchAction {
    CreateTask {
        title: String,
        description: String,
        caps: Vec<String>,
        target: TaskTarget,
    },
    SendMessage {
        to: MsgTarget,
        kind: MsgKind,
        body: String,
    },
    /// 人手が要る状態から、もう一度自動割り当てを試す。
    Retry(TaskId),
    MarkDone(TaskId),
    MarkFailed(TaskId),
}

/// 調停レイヤ UI と発信取り込みの状態。`ZaivernApp` が 1 つ持つ。
pub struct OrchState {
    // タスク作成フォーム
    pub form_open: bool,
    pub title: String,
    pub description: String,
    pub caps: String,
    pub target: TaskTarget,

    // メッセージ送信フォーム
    pub msg_open: bool,
    pub msg_body: String,
    pub msg_target: MsgTarget,
    pub msg_kind: MsgKind,

    /// セッションごとの「もう見た行」。順序付きで上限あり。
    seen_order: HashMap<SessionId, VecDeque<u64>>,
    seen_set: HashMap<SessionId, HashSet<u64>>,
    /// セッションごとの発信時刻の窓。
    out_win: HashMap<SessionId, VecDeque<Instant>>,
    /// 次に画面を走査してよい時刻。
    next_scan: Option<Instant>,
    /// 再割り当てを次に試してよい時刻 (タスクごと)。
    retry_at: HashMap<TaskId, Instant>,
    /// 「候補がいない」を人へ上げ済みのタスク。何度も上げない。
    escalated: HashSet<TaskId>,

    /// 宛先不明で捨てた発信の数 (検証用)。
    pub unknown_target_drops: u32,
    /// 上限超過で捨てた発信の数 (検証用)。
    pub rate_capped_drops: u32,
}

impl Default for OrchState {
    fn default() -> Self {
        Self {
            form_open: false,
            title: String::new(),
            description: String::new(),
            caps: String::new(),
            target: TaskTarget::default(),
            msg_open: false,
            msg_body: String::new(),
            msg_target: MsgTarget::default(),
            msg_kind: MsgKind::Request,
            seen_order: HashMap::new(),
            seen_set: HashMap::new(),
            out_win: HashMap::new(),
            next_scan: None,
            retry_at: HashMap::new(),
            escalated: HashSet::new(),
            unknown_target_drops: 0,
            rate_capped_drops: 0,
        }
    }
}

impl OrchState {
    /// タスク作成フォームを開く (コマンドパレットから)。
    pub fn open_task_form(&mut self) {
        self.form_open = true;
    }

    /// メッセージ送信フォームを開く (コマンドパレットから)。
    pub fn open_msg_form(&mut self) {
        self.msg_open = true;
    }

    /// セッションが消えたら、その記憶も捨てる (無制限に溜めない)。
    pub fn forget(&mut self, id: SessionId) {
        self.seen_order.remove(&id);
        self.seen_set.remove(&id);
        self.out_win.remove(&id);
    }

    /// この行はもう処理済みか。未処理なら記録して `false` を返す。
    fn already_seen(&mut self, id: SessionId, hash: u64) -> bool {
        let set = self.seen_set.entry(id).or_default();
        if !set.insert(hash) {
            return true;
        }
        let order = self.seen_order.entry(id).or_default();
        order.push_back(hash);
        while order.len() > SEEN_LINES_CAP {
            if let Some(old) = order.pop_front() {
                set.remove(&old);
            }
        }
        false
    }

    /// 発信の本数制限。まだ出してよければ記録して `true`。
    fn allow_outbound(&mut self, id: SessionId, now: Instant) -> bool {
        let w = self.out_win.entry(id).or_default();
        while let Some(&front) = w.front() {
            if now.checked_duration_since(front).is_some_and(|d| d > OUTBOUND_WINDOW) {
                w.pop_front();
            } else {
                break;
            }
        }
        if w.len() as u32 >= OUTBOUND_PER_WINDOW {
            return false;
        }
        w.push_back(now);
        true
    }
}

// ── 承認モード ───────────────────────────────────────────────────────

/// 設定文字列 → 調停レイヤの承認モード。
///
/// 未知の値は必ず `Ask` に倒す。停止はやり直しが効かないので、
/// 読めない設定を「自動でよい」と解釈してはいけない。
pub fn permission_mode(s: &str) -> coordinator::PermissionMode {
    match s {
        "auto" => coordinator::PermissionMode::Auto,
        "agent" => coordinator::PermissionMode::Agent,
        _ => coordinator::PermissionMode::Ask,
    }
}

// ── 1: タスクの作成と割り当て ────────────────────────────────────────

/// UI の要求を実行する。
pub fn apply_action(
    co: &mut coordinator::Coordinator,
    rows: &[SessionRow],
    act: OrchAction,
    now: Instant,
) -> Effects {
    let mut eff = Effects::default();
    match act {
        OrchAction::CreateTask {
            title,
            description,
            caps,
            target,
        } => {
            let caps_ref: Vec<&str> = caps.iter().map(|s| s.as_str()).collect();
            let tid = co.add_task(title.clone(), description, &caps_ref, now);

            // 指名なら候補をその 1 つに絞る。自動なら生きている全員。
            let cands: Vec<SessionInfo> = match target {
                TaskTarget::Auto => candidates(rows),
                TaskTarget::Session(sid) => rows
                    .iter()
                    .filter(|r| r.id == sid && r.running)
                    .map(|r| r.as_info())
                    .collect(),
            };

            match co.try_assign(tid, &cands, now) {
                Ok(sid) => {
                    let name = row_label(rows, sid);
                    // 本文はバスへ積む。相手が「注入して安全な状態」になってから届く。
                    match co.queue_handoff(tid, sid, now) {
                        SendOutcome::Dropped { reason } => {
                            eff.warn(format!(
                                "タスク #{tid} は {name} に割り当てましたが、本文を送れませんでした: {}",
                                reason.label()
                            ));
                        }
                        _ => {
                            eff.ok(format!(
                                "📋 タスク #{tid}「{title}」を {name} に割り当てました (相手が待機状態になり次第、本文を送ります)"
                            ));
                        }
                    }
                }
                Err(r) => eff.warn(format!("📋 タスク #{tid} を割り当てられません: {}", r.label())),
            }
        }

        OrchAction::SendMessage { to, kind, body } => {
            let dest = match to {
                MsgTarget::Broadcast => Endpoint::Broadcast,
                MsgTarget::Session(id) => Endpoint::Session(id),
            };
            let out = co.enqueue(AgentMessage::new(Endpoint::User, dest, kind, body).at(now));
            eff.merge(report_send(&out, &describe_target(rows, to)));
        }

        OrchAction::Retry(tid) => {
            let cands = candidates(rows);
            match co.redispatch(tid, &cands, ReassignReason::Manual, now) {
                Ok(sid) => eff.ok(format!(
                    "📋 タスク #{tid} を {} へ渡し直しました",
                    row_label(rows, sid)
                )),
                Err(r) => eff.warn(format!("📋 タスク #{tid} の再割り当てを断りました: {}", r.label())),
            }
        }

        OrchAction::MarkDone(tid) => {
            co.note_done(tid, now);
            eff.ok(format!("✅ タスク #{tid} を完了にしました"));
        }

        OrchAction::MarkFailed(tid) => {
            let holder = co.task(tid).and_then(|t| t.assigned).unwrap_or(0);
            co.note_failed(tid, holder, "ユーザーが失敗と判断した", now);
            eff.warn(format!("⚠ タスク #{tid} を失敗にしました"));
        }
    }
    eff
}

/// 送信結果を必ず言葉にする。黙って消えるのが一番まずい。
fn report_send(out: &SendOutcome, dest: &str) -> Effects {
    let mut eff = Effects::default();
    match out {
        SendOutcome::Queued { id } => eff.ok(format!("📮 #{id} を {dest} へ送りました")),
        SendOutcome::Broadcast { id, delivered_to } => {
            if *delivered_to == 0 {
                eff.warn(format!("📮 #{id} は届け先がいませんでした"));
            } else {
                eff.ok(format!("📮 #{id} を {delivered_to} 件へ一斉送信しました"));
            }
        }
        SendOutcome::Dropped { reason } => {
            eff.warn(format!("📮 {dest} への送信は届きません: {}", reason.label()))
        }
    }
    eff
}

fn row_label(rows: &[SessionRow], id: SessionId) -> String {
    rows.iter()
        .find(|r| r.id == id)
        .map(|r| format!("{} (#{id})", r.title))
        .unwrap_or_else(|| format!("session:{id}"))
}

fn describe_target(rows: &[SessionRow], to: MsgTarget) -> String {
    match to {
        MsgTarget::Broadcast => "全エージェント".into(),
        MsgTarget::Session(id) => row_label(rows, id),
    }
}

// ── 2: 停滞・死亡からの再割り当て ────────────────────────────────────

/// **前任者の停止が確認できたタスクだけ**を、次の担当へ渡す。
///
/// ここへ来る前に `propose_stop` → `gate_for` → kill → プロセス消滅の確認 →
/// `confirm_stopped` が終わっている。`previous_stopped()` を自分で確かめてから
/// `redispatch` するのは早すぎる呼び出しを避けるためで、
/// もし漏れても `coordinator` 側が拒否する (二重の守り)。
pub fn redispatch_ready(
    st: &mut OrchState,
    co: &mut coordinator::Coordinator,
    rows: &[SessionRow],
    now: Instant,
) -> Effects {
    let mut eff = Effects::default();

    // 渡し直す対象を先に洗い出す (借用を分けるため一度 Vec に落とす)。
    let targets: Vec<(TaskId, SessionId, TaskState, u8)> = co
        .tasks()
        .iter()
        .filter(|t| matches!(t.state, TaskState::Stalled | TaskState::Failed))
        .filter(|t| t.previous_stopped())
        .filter_map(|t| t.assigned.map(|s| (t.id, s, t.state, t.attempts)))
        .collect();
    if targets.is_empty() {
        return eff;
    }

    let cands = candidates(rows);
    for (tid, prev, state, attempts) in targets {
        if st.retry_at.get(&tid).is_some_and(|&t| now < t) {
            continue;
        }
        st.retry_at.insert(tid, now + REDISPATCH_BACKOFF);

        // 前任がまだ画面上に生きているなら、渡さない。
        // (`confirm_stopped` 済みでもここで念を入れる)
        if rows.iter().any(|r| r.id == prev && r.running) {
            continue;
        }

        // 次の担当が「何をどこまでやったか」を継げるように書き残す。
        // これをしないと、渡された側は最初からやり直す。
        let why = match state {
            TaskState::Stalled => "無反応になった",
            _ => "作業を続けられなくなった",
        };
        co.add_context(
            tid,
            format!("session:{prev} が{why}ため引き継ぎ ({attempts} 回目の担当)"),
            now,
        );

        let reason = if rows.iter().any(|r| r.id == prev) {
            match state {
                TaskState::Stalled => ReassignReason::Stalled,
                _ => ReassignReason::Failed,
            }
        } else {
            ReassignReason::SessionDied
        };

        match co.redispatch(tid, &cands, reason, now) {
            Ok(sid) => {
                st.escalated.remove(&tid);
                eff.warn(format!(
                    "🔁 タスク #{tid} を {} へ引き継ぎました ({})",
                    row_label(rows, sid),
                    reason.label()
                ));
            }
            Err(AssignRefusal::PreviousHolderNotStopped { previous }) => {
                // 順序が崩れている。回避せず、そのまま見せて止まる。
                eff.warn(format!(
                    "🛑 タスク #{tid} は引き渡しません: 前任 session:{previous} の停止が未確認です"
                ));
            }
            Err(AssignRefusal::NoEligibleCandidate) => {
                // 空きが出れば直るので、人へ上げるのは 1 回だけ。
                if st.escalated.insert(tid) {
                    // このタスクで既に失敗した相手は候補から外れている。
                    // なぜ空振りしたのかが分かるよう、その顔ぶれも添える。
                    let excluded: Vec<String> = co
                        .task(tid)
                        .map(|t| {
                            rows.iter()
                                .filter(|r| t.has_failed(r.id))
                                .map(|r| r.title.clone())
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut body =
                        format!("タスク #{tid} を引き継げるエージェントがいません。");
                    if !excluded.is_empty() {
                        body.push_str(&format!(
                            " (このタスクで既に失敗: {})",
                            excluded.join(", ")
                        ));
                    }
                    body.push_str(&format!(
                        " 再試行の上限は {} 回です。",
                        co.limits().max_attempts
                    ));
                    co.escalate(body, now);
                }
            }
            Err(_) => {
                // AttemptsExhausted は coordinator 側が NeedsUser にして人へ上げる。
            }
        }
    }
    eff
}

/// 配達が済んだセッションが担当しているタスクを「着手した」ことにする。
/// 引き継ぎ本文が実際に相手の入力へ入った瞬間が、走り出した瞬間。
pub fn note_delivered(co: &mut coordinator::Coordinator, delivered_to: &[SessionId], now: Instant) {
    if delivered_to.is_empty() {
        return;
    }
    let ids: Vec<TaskId> = co
        .tasks()
        .iter()
        .filter(|t| t.state == TaskState::Assigned)
        .filter(|t| t.assigned.is_some_and(|s| delivered_to.contains(&s)))
        .map(|t| t.id)
        .collect();
    for tid in ids {
        co.note_running(tid, now);
    }
}

// ── 3(b): 画面から発信マーカーを拾う ─────────────────────────────────

/// 走査してよい時刻か。UI スレッドを塞がないよう間引く。
pub fn scan_due(st: &mut OrchState, now: Instant) -> bool {
    if st.next_scan.is_some_and(|t| now < t) {
        return false;
    }
    st.next_scan = Some(now + SCAN_INTERVAL);
    true
}

/// 1 セッション分の画面テキストから発信マーカーを取り込む。
///
/// - 初回はいまの画面を「見た」ことにするだけで、1 通も出さない
///   (起動前から残っている行で暴発させない)。
/// - 同じ行は二度処理しない。
/// - 宛先が引けない / 本数を超えた場合は **積まずに理由を残す**。
pub fn scan_outbound(
    st: &mut OrchState,
    co: &mut coordinator::Coordinator,
    from: SessionId,
    screen: &str,
    rows: &[SessionRow],
    now: Instant,
) -> Effects {
    let mut eff = Effects::default();
    let primed = st.seen_set.contains_key(&from);

    for line in screen.lines() {
        let h = line_hash(line);
        if st.already_seen(from, h) {
            continue;
        }
        if !primed {
            // 初回は記録だけ。
            continue;
        }
        let Some((target, body)) = coordinator::parse_outbound(line) else {
            continue;
        };

        let Some(dest) = resolve_target(&target, rows, from) else {
            st.unknown_target_drops += 1;
            eff.warn(format!(
                "📮 {} の発信を破棄: 宛先「{target}」が見つかりません",
                row_label(rows, from)
            ));
            continue;
        };

        if !st.allow_outbound(from, now) {
            st.rate_capped_drops += 1;
            eff.warn(format!(
                "📮 {} の発信を抑制: {OUTBOUND_WINDOW:?} あたり {OUTBOUND_PER_WINDOW} 通の上限に達しました",
                row_label(rows, from)
            ));
            continue;
        }

        let msg = AgentMessage::new(Endpoint::Session(from), dest, MsgKind::Request, body).at(now);

        // 宛先が既に死んでいるなら、握り潰さず人へ回す。
        let dead = matches!(dest, Endpoint::Session(id)
            if rows.iter().any(|r| r.id == id && !r.running));
        let out = if dead {
            co.forward(msg, Endpoint::User, now)
        } else {
            co.enqueue(msg)
        };
        eff.merge(report_send(&out, &describe_endpoint(rows, dest)));
    }
    eff
}

/// 宛先ラベルを解決する。`ALL` は一斉送信、それ以外はセッション名の一致。
/// 引けなければ `None` (= 積まない)。
fn resolve_target(target: &str, rows: &[SessionRow], from: SessionId) -> Option<Endpoint> {
    if target.eq_ignore_ascii_case(coordinator::OUTBOUND_ALL) {
        return Some(Endpoint::Broadcast);
    }
    // セッション ID の直指定も許す。
    if let Some(id) = target.strip_prefix('#').and_then(|s| s.parse::<u64>().ok()) {
        return rows.iter().find(|r| r.id == id).map(|r| Endpoint::Session(r.id));
    }
    // 名前の完全一致を優先し、無ければ前方一致。どちらも自分自身は除く。
    let exact = rows
        .iter()
        .find(|r| r.id != from && r.title.eq_ignore_ascii_case(target));
    let pick = exact.or_else(|| {
        let mut it = rows.iter().filter(|r| {
            r.id != from && r.title.to_lowercase().starts_with(&target.to_lowercase())
        });
        // 前方一致が 2 つ以上あるなら曖昧なので選ばない。
        let first = it.next()?;
        if it.next().is_some() {
            None
        } else {
            Some(first)
        }
    });
    pick.map(|r| Endpoint::Session(r.id))
}

fn describe_endpoint(rows: &[SessionRow], e: Endpoint) -> String {
    match e {
        Endpoint::Broadcast => "全エージェント".into(),
        Endpoint::Session(id) => row_label(rows, id),
        Endpoint::Supervisor => "監視役".into(),
        Endpoint::User => "あなた".into(),
    }
}

/// 行の同一性判定に使う軽いハッシュ (FNV-1a)。
fn line_hash(line: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in line.trim_end().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ── UI ───────────────────────────────────────────────────────────────

/// 状態に応じた表示色と札。`NeedsUser` だけは目立たせる。
fn state_badge(state: TaskState, theme: &Theme) -> (&'static str, egui::Color32) {
    match state {
        TaskState::Pending => ("待機", theme.text_dim),
        TaskState::Assigned => ("割当済", theme.accent),
        TaskState::Running => ("作業中", theme.ok),
        TaskState::Stalled => ("停滞", theme.warn),
        TaskState::Failed => ("失敗", theme.warn),
        TaskState::Done => ("完了", theme.ok),
        TaskState::NeedsUser => ("⚠ 人手が必要", theme.err),
    }
}

/// Cockpit に差し込む調停セクション。押されたボタンを [`OrchAction`] で返す。
pub fn cockpit_section(
    st: &mut OrchState,
    ui: &mut egui::Ui,
    theme: &Theme,
    tasks: &[Task],
    rows: &[SessionRow],
    bus: &BusStatus,
) -> Vec<OrchAction> {
    let mut acts: Vec<OrchAction> = Vec::new();

    ui.horizontal(|ui| {
        ui.label(
            RichText::new("📋 タスクとメッセージ")
                .strong()
                .color(theme.accent),
        );
        let open = tasks.iter().filter(|t| !t.state.is_terminal()).count();
        let stuck = tasks
            .iter()
            .filter(|t| t.state == TaskState::NeedsUser)
            .count();
        ui.label(RichText::new(format!("未完了 {open} 件")).color(theme.text_dim));
        if stuck > 0 {
            ui.label(
                RichText::new(format!("⚠ 人手待ち {stuck} 件"))
                    .strong()
                    .color(theme.err),
            );
        }
        // 配達待ちと、抑制されて消えた分。黙って消えたように見せない。
        if bus.queued > 0 {
            ui.label(
                RichText::new(format!("📮 配達待ち {}", bus.queued)).color(theme.text_dim),
            )
            .on_hover_text("相手が待機状態になってから届きます");
        }
        if bus.drops > 0 || bus.overflowed > 0 {
            let hint = bus
                .last_drop
                .clone()
                .unwrap_or_else(|| "直近の破棄はありません".into());
            ui.label(
                RichText::new(format!("🗑 破棄 {} / 溢れ {}", bus.drops, bus.overflowed))
                    .color(theme.warn),
            )
            .on_hover_text(format!("直近: {hint}"));
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("📮 メッセージ").clicked() {
                st.msg_open = true;
            }
            if ui.button("＋ タスク").clicked() {
                st.form_open = true;
            }
        });
    });

    if !tasks.is_empty() {
        ui.add_space(4.0);
        task_list(st, ui, theme, tasks, rows, &mut acts);
    }
    ui.add_space(6.0);
    acts
}

fn task_list(
    _st: &mut OrchState,
    ui: &mut egui::Ui,
    theme: &Theme,
    tasks: &[Task],
    rows: &[SessionRow],
    acts: &mut Vec<OrchAction>,
) {
    egui::ScrollArea::vertical()
        .id_salt("orch-task-list")
        .max_height(150.0)
        .show(ui, |ui| {
            egui::Grid::new("orch-tasks")
                .num_columns(6)
                .striped(true)
                .spacing([10.0, 4.0])
                .show(ui, |ui| {
                    for h in ["ID", "タイトル", "状態", "担当", "試行", ""] {
                        ui.label(RichText::new(h).small().color(theme.text_dim));
                    }
                    ui.end_row();

                    for t in tasks {
                        let (badge, color) = state_badge(t.state, theme);
                        let needs_user = t.state == TaskState::NeedsUser;

                        ui.label(RichText::new(format!("#{}", t.id)).color(theme.text_dim));

                        let title = RichText::new(&t.title).color(if needs_user {
                            theme.err
                        } else {
                            theme.text
                        });
                        // 引き継ぎ材料も一緒に見せる。次の担当が何を継いだかが分かる。
                        let mut hover = t.description.clone();
                        if !t.context().is_empty() {
                            hover.push_str("\n\nこれまでの経過:\n");
                            for c in t.context() {
                                hover.push_str(&format!("・{c}\n"));
                            }
                        }
                        if t.history_dropped() > 0 {
                            hover
                                .push_str(&format!("\n(古い履歴 {} 件は省略)", t.history_dropped()));
                        }
                        ui.label(if needs_user { title.strong() } else { title })
                            .on_hover_text(hover.trim_end());

                        let mut badge_txt = RichText::new(badge).color(color);
                        if needs_user {
                            badge_txt = badge_txt.strong();
                        }
                        ui.label(badge_txt);

                        let who = match t.assigned {
                            Some(id) => row_label(rows, id),
                            None => "—".into(),
                        };
                        ui.label(RichText::new(who).color(theme.text_dim));
                        ui.label(RichText::new(t.attempts.to_string()).color(theme.text_dim));

                        ui.horizontal(|ui| {
                            if !t.state.is_terminal() && ui.small_button("完了").clicked() {
                                acts.push(OrchAction::MarkDone(t.id));
                            }
                            if !t.state.is_terminal() && ui.small_button("失敗").clicked() {
                                acts.push(OrchAction::MarkFailed(t.id));
                            }
                            if needs_user
                                && ui
                                    .small_button(RichText::new("↻ 再割当").color(theme.err))
                                    .on_hover_text(
                                        "再試行の上限に達しています。人が担当を決め直してください",
                                    )
                                    .clicked()
                            {
                                acts.push(OrchAction::Retry(t.id));
                            }
                        });
                        ui.end_row();
                    }
                });
        });
}

/// タスク作成ウィンドウ。開いていないときは何もしない。
pub fn task_form_ui(
    st: &mut OrchState,
    ctx: &egui::Context,
    theme: &Theme,
    rows: &[SessionRow],
) -> Vec<OrchAction> {
    if !st.form_open {
        return Vec::new();
    }
    let mut acts = Vec::new();
    let mut close = false;
    let mut open = true;

    egui::Window::new("📋 新しいタスク")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_min_width(420.0);

            ui.label(RichText::new("タイトル").small().color(theme.text_dim));
            ui.add(
                egui::TextEdit::singleline(&mut st.title)
                    .desired_width(f32::INFINITY)
                    .hint_text("例: ログイン画面のテストを直す"),
            );

            ui.add_space(6.0);
            ui.label(RichText::new("内容").small().color(theme.text_dim));
            ui.add(
                egui::TextEdit::multiline(&mut st.description)
                    .desired_width(f32::INFINITY)
                    .desired_rows(4)
                    .hint_text("担当するエージェントへそのまま送られます"),
            );

            ui.add_space(6.0);
            ui.label(
                RichText::new("必要な能力 (任意・カンマ区切り)")
                    .small()
                    .color(theme.text_dim),
            );
            ui.add(
                egui::TextEdit::singleline(&mut st.caps)
                    .desired_width(f32::INFINITY)
                    .hint_text("backend, test"),
            );

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("担当").small().color(theme.text_dim));
                let current = match st.target {
                    TaskTarget::Auto => "自動割り当て".to_string(),
                    TaskTarget::Session(id) => row_label(rows, id),
                };
                egui::ComboBox::from_id_salt("orch-task-target")
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut st.target, TaskTarget::Auto, "自動割り当て");
                        for r in rows.iter().filter(|r| r.running) {
                            ui.selectable_value(
                                &mut st.target,
                                TaskTarget::Session(r.id),
                                format!("{} (#{})", r.title, r.id),
                            );
                        }
                    });
            });

            if rows.iter().all(|r| !r.running) {
                ui.label(
                    RichText::new("稼働中のエージェントがいないため、いま割り当てはできません")
                        .small()
                        .color(theme.warn),
                );
            }

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let ready = !st.title.trim().is_empty();
                if ui
                    .add_enabled(ready, egui::Button::new("▶ 作成して割り当て"))
                    .clicked()
                {
                    acts.push(OrchAction::CreateTask {
                        title: st.title.trim().to_string(),
                        description: st.description.trim().to_string(),
                        caps: st
                            .caps
                            .split(',')
                            .map(|s| s.trim().to_lowercase())
                            .filter(|s| !s.is_empty())
                            .collect(),
                        target: st.target,
                    });
                    close = true;
                }
                if ui.button("キャンセル").clicked() {
                    close = true;
                }
            });
        });

    if close || !open {
        st.form_open = false;
        st.title.clear();
        st.description.clear();
        st.caps.clear();
        st.target = TaskTarget::Auto;
    }
    acts
}

/// 手動メッセージ送信ウィンドウ。
pub fn message_form_ui(
    st: &mut OrchState,
    ctx: &egui::Context,
    theme: &Theme,
    rows: &[SessionRow],
) -> Vec<OrchAction> {
    if !st.msg_open {
        return Vec::new();
    }
    let mut acts = Vec::new();
    let mut close = false;
    let mut open = true;

    egui::Window::new("📮 エージェントへ送信")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_min_width(420.0);

            ui.horizontal(|ui| {
                ui.label(RichText::new("宛先").small().color(theme.text_dim));
                let current = describe_target(rows, st.msg_target);
                egui::ComboBox::from_id_salt("orch-msg-target")
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut st.msg_target,
                            MsgTarget::Broadcast,
                            "全エージェント (一斉送信)",
                        );
                        for r in rows.iter().filter(|r| r.running) {
                            ui.selectable_value(
                                &mut st.msg_target,
                                MsgTarget::Session(r.id),
                                format!("{} (#{})", r.title, r.id),
                            );
                        }
                    });

                ui.label(RichText::new("種別").small().color(theme.text_dim));
                egui::ComboBox::from_id_salt("orch-msg-kind")
                    .selected_text(st.msg_kind.label())
                    .show_ui(ui, |ui| {
                        for k in [
                            MsgKind::Request,
                            MsgKind::Reply,
                            MsgKind::Status,
                            MsgKind::Question,
                            MsgKind::Handoff,
                        ] {
                            ui.selectable_value(&mut st.msg_kind, k, k.label());
                        }
                    });
            });

            ui.add_space(6.0);
            ui.add(
                egui::TextEdit::multiline(&mut st.msg_body)
                    .desired_width(f32::INFINITY)
                    .desired_rows(4)
                    .hint_text("相手が待機状態になったときに届きます"),
            );

            ui.label(
                RichText::new(
                    "作業中のエージェントには割り込みません。安全な状態になるまで待ってから届きます",
                )
                .small()
                .color(theme.text_dim),
            );

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let ready = !st.msg_body.trim().is_empty();
                if ui.add_enabled(ready, egui::Button::new("📮 送信")).clicked() {
                    acts.push(OrchAction::SendMessage {
                        to: st.msg_target,
                        kind: st.msg_kind,
                        body: st.msg_body.trim().to_string(),
                    });
                    close = true;
                }
                if ui.button("閉じる").clicked() {
                    close = true;
                }
            });
        });

    if close || !open {
        st.msg_open = false;
        st.msg_body.clear();
    }
    acts
}

// ── テスト ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::Coordinator;

    fn t0() -> Instant {
        Instant::now() - Duration::from_secs(3600)
    }

    fn row(id: u64, title: &str, running: bool, state: SessionState) -> SessionRow {
        SessionRow {
            id,
            title: title.into(),
            running,
            state,
        }
    }

    fn live(id: u64, title: &str) -> SessionRow {
        row(id, title, true, SessionState::Idle)
    }

    // ── 承認モードのゲート ───────────────────────────────────────

    /// `Ask` でのセッション停止は、絶対にユーザー確認を要求する。
    #[test]
    fn stop_under_ask_requires_confirmation() {
        use coordinator::ProposalGate as G;
        assert_eq!(
            coordinator::gate_for(permission_mode("ask")),
            G::NeedsUserConfirm
        );
        // 設定が壊れていても自動にはしない。
        assert_eq!(
            coordinator::gate_for(permission_mode("")),
            G::NeedsUserConfirm
        );
        assert_eq!(
            coordinator::gate_for(permission_mode("なにこれ")),
            G::NeedsUserConfirm
        );
        assert_eq!(
            coordinator::gate_for(permission_mode("agent")),
            G::NeedsUserConfirm
        );
        assert_eq!(coordinator::gate_for(permission_mode("auto")), G::AutoApproved);
    }

    // ── タスク作成 ───────────────────────────────────────────────

    #[test]
    fn creating_a_task_assigns_and_queues_the_body() {
        let mut co = Coordinator::new();
        co.register_session(1);
        let rows = vec![live(1, "backend")];
        let eff = apply_action(
            &mut co,
            &rows,
            OrchAction::CreateTask {
                title: "テストを直す".into(),
                description: "cargo test が赤い".into(),
                caps: vec![],
                target: TaskTarget::Auto,
            },
            t0(),
        );
        assert!(eff.toasts.iter().any(|(_, ok)| *ok), "成功が伝わる: {eff:?}");
        assert_eq!(co.tasks().len(), 1);
        assert_eq!(co.tasks()[0].assigned, Some(1));
        // 本文はバスに積まれ、相手が安全になってから届く。
        assert_eq!(co.mailbox(1).map(|m| m.len()), Some(1));
    }

    /// 候補がいなければ、黙って何もしないのではなく理由を出す。
    #[test]
    fn refusal_is_surfaced_in_japanese() {
        let mut co = Coordinator::new();
        let eff = apply_action(
            &mut co,
            &[],
            OrchAction::CreateTask {
                title: "誰もいない".into(),
                description: String::new(),
                caps: vec![],
                target: TaskTarget::Auto,
            },
            t0(),
        );
        let warned: Vec<&String> = eff
            .toasts
            .iter()
            .filter(|(_, ok)| !*ok)
            .map(|(s, _)| s)
            .collect();
        assert_eq!(warned.len(), 1, "{eff:?}");
        assert!(warned[0].contains("割り当て可能なセッションがいない"), "{warned:?}");
    }

    // ── 再割り当ての順序 ─────────────────────────────────────────

    /// **前任者の停止が未確認のうちは渡さない**。ここが崩れると
    /// 2 人が同じファイルを同時に触って成果物が壊れる。
    #[test]
    fn redispatch_is_refused_while_previous_holder_is_not_stopped() {
        let mut co = Coordinator::new();
        co.register_session(1);
        co.register_session(2);
        let now = t0();
        let tid = co.add_task("引き継ぎ", "", &[], now);
        co.try_assign(tid, &[SessionInfo::new(1, SessionState::Idle, &[])], now)
            .unwrap();
        // 停滞。ただし前任はまだ動いている。
        co.note_stalled(1, now);
        assert!(!co.task(tid).unwrap().previous_stopped());

        let cands = [
            SessionInfo::new(1, SessionState::Stalled, &[]),
            SessionInfo::new(2, SessionState::Idle, &[]),
        ];
        let err = co
            .redispatch(tid, &cands, ReassignReason::Stalled, now)
            .unwrap_err();
        assert_eq!(err, AssignRefusal::PreviousHolderNotStopped { previous: 1 });
        assert_eq!(co.task(tid).unwrap().assigned, Some(1), "担当は変わらない");
    }

    /// **プロセスが消えていても、`confirm_stopped` を通していなければ渡さない**。
    /// 「死んだように見える」と「止まったと確認した」は別物で、
    /// ここを同一視すると前任が生き返ったときに二重編集になる。
    #[test]
    fn dead_looking_process_alone_does_not_authorize_handover() {
        let mut co = Coordinator::new();
        co.register_session(1);
        co.register_session(2);
        let mut st = OrchState::default();
        let now = t0();

        let tid = co.add_task("引き継ぎ", "", &[], now);
        co.try_assign(tid, &[SessionInfo::new(1, SessionState::Idle, &[])], now)
            .unwrap();
        co.note_stalled(1, now);

        // 画面上は死んでいる。しかし confirm_stopped はまだ呼んでいない。
        let rows = vec![row(1, "a", false, SessionState::Exited), live(2, "b")];
        assert!(!co.task(tid).unwrap().previous_stopped());

        redispatch_ready(&mut st, &mut co, &rows, now);
        assert_eq!(
            co.task(tid).unwrap().assigned,
            Some(1),
            "停止を確認する前に引き渡した"
        );

        // 直接叩いても、coordinator 側が同じ理由で断る (二重の守り)。
        let err = co
            .redispatch(tid, &candidates(&rows), ReassignReason::Stalled, now)
            .unwrap_err();
        assert_eq!(err, AssignRefusal::PreviousHolderNotStopped { previous: 1 });
    }

    /// バスの様子は、抑制されて消えた分まで数える。
    #[test]
    fn bus_status_counts_queued_and_drops() {
        let mut co = Coordinator::new();
        co.register_session(1);
        let rows = vec![live(1, "a")];
        let now = t0();

        co.enqueue(
            AgentMessage::new(Endpoint::User, Endpoint::Session(1), MsgKind::Status, "1 通目")
                .at(now),
        );
        // 宛先不明は破棄される。
        co.enqueue(
            AgentMessage::new(Endpoint::User, Endpoint::Session(9), MsgKind::Status, "行方不明")
                .at(now),
        );

        let bus = bus_status(&co, &rows);
        assert_eq!(bus.queued, 1);
        assert_eq!(bus.drops, 1);
        let last = bus.last_drop.expect("直近の破棄が見える");
        assert!(last.contains("宛先セッションが存在しない"), "{last}");
    }

    /// 停止を確認してからは渡る。文脈も一緒に運ばれる。
    #[test]
    fn redispatch_proceeds_after_stop_is_confirmed() {
        let mut co = Coordinator::new();
        co.register_session(1);
        co.register_session(2);
        let mut st = OrchState::default();
        let now = t0();

        let tid = co.add_task("引き継ぎ", "内容", &[], now);
        co.try_assign(tid, &[SessionInfo::new(1, SessionState::Idle, &[])], now)
            .unwrap();
        co.note_stalled(1, now);

        // 前任が生きているうちは、橋渡しは 1 通も動かさない。
        let rows_alive = vec![live(1, "a"), live(2, "b")];
        let eff = redispatch_ready(&mut st, &mut co, &rows_alive, now);
        assert!(eff.toasts.is_empty());
        assert_eq!(co.task(tid).unwrap().assigned, Some(1));

        // kill → プロセス消滅 → confirm_stopped、の順を踏む。
        assert!(co.confirm_stopped(tid, now));
        let rows_dead = vec![row(1, "a", false, SessionState::Exited), live(2, "b")];
        st.retry_at.clear();
        let eff = redispatch_ready(&mut st, &mut co, &rows_dead, now);

        assert_eq!(co.task(tid).unwrap().assigned, Some(2), "{eff:?}");
        assert!(
            !co.task(tid).unwrap().context().is_empty(),
            "引き継ぎ材料が空だと、渡された側は最初からやり直す"
        );
        let brief = co.handoff_brief(tid).unwrap();
        assert!(brief.contains("引き継ぎ"), "{brief}");
    }

    /// 上限まで失敗したら、ぐるぐる回さずに `NeedsUser` で止まって人を呼ぶ。
    #[test]
    fn exhausted_attempts_reach_needs_user_instead_of_looping() {
        let mut co = Coordinator::new();
        let mut st = OrchState::default();
        let now = t0();
        let max = co.limits().max_attempts;

        let mut rows: Vec<SessionRow> = Vec::new();
        for i in 1..=(max as u64 + 2) {
            co.register_session(i);
            rows.push(live(i, &format!("agent{i}")));
        }

        let tid = co.add_task("何度も落ちる", "", &[], now);
        co.try_assign(tid, &candidates(&rows), now).unwrap();

        // 「停滞 → 停止確認 → 渡し直し」を上限まで繰り返す。
        for _ in 0..(max as usize + 2) {
            let holder = co.task(tid).unwrap().assigned.unwrap();
            co.note_stalled(holder, now);
            co.confirm_stopped(tid, now);
            for r in rows.iter_mut().filter(|r| r.id == holder) {
                r.running = false;
                r.state = SessionState::Exited;
            }
            st.retry_at.clear();
            redispatch_ready(&mut st, &mut co, &rows, now);
            if co.task(tid).unwrap().state == TaskState::NeedsUser {
                break;
            }
        }

        let t = co.task(tid).unwrap();
        assert_eq!(t.state, TaskState::NeedsUser, "履歴: {:?}", t.history);
        assert!(t.attempts <= max, "上限を超えて試行している: {}", t.attempts);
        // 人が気づけるよう、ユーザーの受信箱へ上がっている。
        assert!(!co.user_inbox().is_empty());
    }

    /// 終わったタスク (完了 / 人手待ち) は、自動で渡し直さない。
    #[test]
    fn terminal_tasks_are_not_auto_redispatched() {
        let mut co = Coordinator::new();
        co.register_session(1);
        let now = t0();
        let tid = co.add_task("止まったまま", "", &[], now);
        co.try_assign(tid, &[SessionInfo::new(1, SessionState::Idle, &[])], now)
            .unwrap();
        co.note_stalled(1, now);
        co.confirm_stopped(tid, now);
        co.note_done(tid, now);
        assert!(co.task(tid).unwrap().state.is_terminal());

        let mut st = OrchState::default();
        let rows = vec![row(1, "a", false, SessionState::Exited)];
        let eff = redispatch_ready(&mut st, &mut co, &rows, now);
        assert!(eff.toasts.is_empty(), "終わったタスクを触らない: {eff:?}");
        assert_eq!(co.task(tid).unwrap().assigned, Some(1));
    }

    // ── 発信マーカーの取り込み ───────────────────────────────────

    /// 初回の走査はいまの画面を覚えるだけ。残っている行で暴発させない。
    #[test]
    fn first_scan_only_primes() {
        let mut co = Coordinator::new();
        co.register_session(1);
        co.register_session(2);
        let mut st = OrchState::default();
        let rows = vec![live(1, "a"), live(2, "b")];
        scan_outbound(&mut st, &mut co, 1, "[ZAI-TO:b] 前からある行\n", &rows, t0());
        assert_eq!(co.mailbox(2).map(|m| m.len()), Some(0));
    }

    #[test]
    fn outbound_marker_reaches_the_bus() {
        let mut co = Coordinator::new();
        co.register_session(1);
        co.register_session(2);
        let mut st = OrchState::default();
        let rows = vec![live(1, "a"), live(2, "b")];
        let now = t0();

        scan_outbound(&mut st, &mut co, 1, "起動しました\n", &rows, now);
        let eff = scan_outbound(&mut st, &mut co, 1, "[ZAI-TO:b] 手伝って\n", &rows, now);

        assert_eq!(co.mailbox(2).map(|m| m.len()), Some(1), "{eff:?}");
        let body = co.mailbox(2).unwrap().iter().next().unwrap().body.clone();
        assert_eq!(body, "手伝って");
    }

    /// **注入した行のこだまから新しいメッセージを作らない**。
    /// ここが崩れると 送る → 映る → また送る の無限ループになる。
    #[test]
    fn echo_of_injected_line_creates_no_new_message() {
        let mut co = Coordinator::new();
        co.register_session(1);
        co.register_session(2);
        let mut st = OrchState::default();
        let rows = vec![live(1, "a"), live(2, "b")];
        let now = t0();

        scan_outbound(&mut st, &mut co, 1, "起動\n", &rows, now);
        scan_outbound(&mut st, &mut co, 2, "起動\n", &rows, now);

        // 1 → 2 の発信を 1 通だけ出す。
        scan_outbound(&mut st, &mut co, 1, "[ZAI-TO:b] 折り返して\n", &rows, now);
        let d = co.take_deliverable(&[(2, SessionState::Idle)]);
        assert_eq!(d.len(), 1);
        let injected = d[0].text.trim_end_matches('\r').to_string();

        // 注入行が 2 の画面に映る。これを何周回しても、新しい発信は生まれない。
        for _ in 0..5 {
            let screen = format!("{injected}\n");
            scan_outbound(&mut st, &mut co, 2, &screen, &rows, now);
        }
        assert_eq!(
            co.mailbox(1).map(|m| m.len()),
            Some(0),
            "こだまが新しいメッセージになった (無限ループ)"
        );
        assert_eq!(st.unknown_target_drops, 0);
    }

    /// 同じ行が画面に残り続けても、二度は送らない。
    #[test]
    fn repeated_screen_line_is_sent_once() {
        let mut co = Coordinator::new();
        co.register_session(1);
        co.register_session(2);
        let mut st = OrchState::default();
        let rows = vec![live(1, "a"), live(2, "b")];
        let now = t0();
        scan_outbound(&mut st, &mut co, 1, "起動\n", &rows, now);
        for _ in 0..10 {
            scan_outbound(&mut st, &mut co, 1, "[ZAI-TO:b] 一度だけ\n", &rows, now);
        }
        assert_eq!(co.mailbox(2).map(|m| m.len()), Some(1));
    }

    /// 宛先が引けない発信は積まず、理由を残す。
    #[test]
    fn unknown_target_is_refused_with_a_reason() {
        let mut co = Coordinator::new();
        co.register_session(1);
        co.register_session(2);
        let mut st = OrchState::default();
        let rows = vec![live(1, "a"), live(2, "b")];
        let now = t0();

        scan_outbound(&mut st, &mut co, 1, "起動\n", &rows, now);
        let eff = scan_outbound(&mut st, &mut co, 1, "[ZAI-TO:いない人] やあ\n", &rows, now);

        assert_eq!(st.unknown_target_drops, 1);
        assert_eq!(co.mailbox(2).map(|m| m.len()), Some(0));
        assert!(
            eff.toasts.iter().any(|(s, ok)| !*ok && s.contains("いない人")),
            "理由が見えない: {eff:?}"
        );
    }

    /// 前方一致が複数あるなら、勝手に選ばずに断る。
    #[test]
    fn ambiguous_target_is_refused() {
        let mut co = Coordinator::new();
        co.register_session(1);
        co.register_session(2);
        co.register_session(3);
        let mut st = OrchState::default();
        let rows = vec![live(1, "a"), live(2, "backend"), live(3, "backup")];
        let now = t0();
        scan_outbound(&mut st, &mut co, 1, "起動\n", &rows, now);
        scan_outbound(&mut st, &mut co, 1, "[ZAI-TO:back] どっち?\n", &rows, now);
        assert_eq!(st.unknown_target_drops, 1);
        assert_eq!(co.mailbox(2).map(|m| m.len()), Some(0));
        assert_eq!(co.mailbox(3).map(|m| m.len()), Some(0));
    }

    /// 1 セッションが窓あたりに出せる本数には上限がある。
    #[test]
    fn outbound_rate_cap_engages() {
        // バス側のレート制限に先に当たると何を測っているか分からなくなるので緩める。
        let mut co = Coordinator::with_limits(coordinator::Limits {
            pair_limit: 1000,
            global_limit: 1000,
            pingpong_limit: 1000,
            ..coordinator::Limits::default()
        });
        co.register_session(1);
        co.register_session(2);
        let mut st = OrchState::default();
        let rows = vec![live(1, "a"), live(2, "b")];
        let now = t0();

        scan_outbound(&mut st, &mut co, 1, "起動\n", &rows, now);
        let n = OUTBOUND_PER_WINDOW + 3;
        for i in 0..n {
            let screen = format!("[ZAI-TO:b] 連投 {i}\n");
            scan_outbound(&mut st, &mut co, 1, &screen, &rows, now);
        }
        assert_eq!(
            co.mailbox(2).map(|m| m.len()),
            Some(OUTBOUND_PER_WINDOW as usize),
            "上限が効いていない"
        );
        assert_eq!(st.rate_capped_drops, 3);

        // 窓が過ぎればまた出せる。
        let later = now + OUTBOUND_WINDOW + Duration::from_secs(1);
        scan_outbound(&mut st, &mut co, 1, "[ZAI-TO:b] 窓明け\n", &rows, later);
        assert_eq!(
            co.mailbox(2).map(|m| m.len()),
            Some(OUTBOUND_PER_WINDOW as usize + 1)
        );
    }

    /// 死んだ相手宛の発信は握り潰さず、人へ回す。
    #[test]
    fn message_to_a_dead_session_goes_to_the_user() {
        let mut co = Coordinator::new();
        co.register_session(1);
        co.register_session(2);
        let mut st = OrchState::default();
        let rows = vec![live(1, "a"), row(2, "b", false, SessionState::Exited)];
        let now = t0();
        scan_outbound(&mut st, &mut co, 1, "起動\n", &rows, now);
        scan_outbound(&mut st, &mut co, 1, "[ZAI-TO:b] 届く?\n", &rows, now);
        assert!(!co.user_inbox().is_empty(), "人へ回っていない");
    }

    /// 自分自身を宛先にはできない (バスも自分宛は捨てる)。
    #[test]
    fn self_addressed_marker_is_not_resolved() {
        let rows = vec![live(1, "a"), live(2, "b")];
        assert_eq!(resolve_target("a", &rows, 1), None);
        assert_eq!(resolve_target("b", &rows, 1), Some(Endpoint::Session(2)));
        assert_eq!(resolve_target("ALL", &rows, 1), Some(Endpoint::Broadcast));
        assert_eq!(resolve_target("all", &rows, 1), Some(Endpoint::Broadcast));
        assert_eq!(resolve_target("#2", &rows, 1), Some(Endpoint::Session(2)));
        assert_eq!(resolve_target("#9", &rows, 1), None);
    }

    // ── 手動送信 ─────────────────────────────────────────────────

    #[test]
    fn manual_send_reports_the_outcome() {
        let mut co = Coordinator::new();
        co.register_session(1);
        let rows = vec![live(1, "a")];
        let eff = apply_action(
            &mut co,
            &rows,
            OrchAction::SendMessage {
                to: MsgTarget::Session(1),
                kind: MsgKind::Request,
                body: "やってほしいこと".into(),
            },
            t0(),
        );
        assert!(eff.toasts.iter().any(|(_, ok)| *ok), "{eff:?}");
        assert_eq!(co.mailbox(1).map(|m| m.len()), Some(1));
    }

    /// 捨てられたら、必ず理由まで見せる。黙って消えるのが一番まずい。
    #[test]
    fn dropped_manual_send_shows_the_reason() {
        let mut co = Coordinator::new();
        let rows: Vec<SessionRow> = vec![];
        let eff = apply_action(
            &mut co,
            &rows,
            OrchAction::SendMessage {
                to: MsgTarget::Session(42),
                kind: MsgKind::Request,
                body: "宛先がいない".into(),
            },
            t0(),
        );
        let warned: Vec<&String> = eff
            .toasts
            .iter()
            .filter(|(_, ok)| !*ok)
            .map(|(s, _)| s)
            .collect();
        assert_eq!(warned.len(), 1, "{eff:?}");
        assert!(warned[0].contains("宛先セッションが存在しない"), "{warned:?}");
    }

    // ── 記憶量 ───────────────────────────────────────────────────

    /// 画面を延々流しても、覚える行は上限で頭打ちになる。
    #[test]
    fn seen_lines_are_bounded() {
        let mut st = OrchState::default();
        for i in 0..(SEEN_LINES_CAP * 3) {
            st.already_seen(1, i as u64);
        }
        assert!(st.seen_order[&1].len() <= SEEN_LINES_CAP);
        assert!(st.seen_set[&1].len() <= SEEN_LINES_CAP);
    }

    #[test]
    fn forgetting_a_session_clears_its_memory() {
        let mut st = OrchState::default();
        st.already_seen(7, 1);
        st.allow_outbound(7, t0());
        st.forget(7);
        assert!(!st.seen_set.contains_key(&7));
        assert!(!st.out_win.contains_key(&7));
    }

    /// 着手の記録は、実際に本文が届いたセッションの分だけ立つ。
    #[test]
    fn note_delivered_marks_only_the_receiving_holder() {
        let mut co = Coordinator::new();
        co.register_session(1);
        co.register_session(2);
        let now = t0();
        let a = co.add_task("A", "", &[], now);
        let b = co.add_task("B", "", &[], now);
        co.try_assign(a, &[SessionInfo::new(1, SessionState::Idle, &[])], now)
            .unwrap();
        co.try_assign(b, &[SessionInfo::new(2, SessionState::Idle, &[])], now)
            .unwrap();

        note_delivered(&mut co, &[1], now);
        assert_eq!(co.task(a).unwrap().state, TaskState::Running);
        assert_eq!(co.task(b).unwrap().state, TaskState::Assigned);
    }
}
