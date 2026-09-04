//! Team 画面の状態と、GUI から Runtime を動かす橋。
//!
//! ## なぜ `ZaivernApp` のフィールドにしないのか
//!
//! `ZaivernApp` の構造体と初期化は**全ブランチが取り合う共有面**で、欄を 1 つ
//! 足すだけで並列の機能ブランチが同時に壊れる (CLAUDE.md の実測)。Team の
//! 状態は UI スレッドからしか触らないので、ここに `thread_local!` で持つと
//! 共有面を 1 バイトも増やさずに済む。
//!
//! 設計原則 1 (「ターミナルのモデルはウィンドウより長生きさせる」) とも
//! 揃う — Runtime は UI の破棄を生き延びる場所に居て、UI はその純粋な
//! ビューになる。
//!
//! ## 描画中に副作用を起こさない
//!
//! [`TeamPanel::pump`] は**描画の外**で呼ぶ。描画側 ([`super::organization_board`])
//! は [`TeamSnapshot`](super::view_model::TeamSnapshot) を読んで
//! [`BoardAction`] を返すだけで、プロセス起動もファイル書き込みもしない。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::model::*;
use super::gitinit;
use super::outbox::{self, ReadOutcome, Verdict};
use super::persistence::{self, LoadOutcome};
use super::planner::{PlanInput, StaticPlanner, TeamPlanner};
use super::runtime::{Observation, RunOptions, RunOwner, TeamAction, TeamEffect, TeamRuntime};
use super::view_model::{self, TeamSnapshot};

/// 画面を走査する間隔。**毎フレームは舐めない** (UI スレッドで走るので、
/// 64 体ぶんの画面を 60fps で解析すると確実にフレームが落ちる)。
pub const SCAN_INTERVAL: Duration = Duration::from_millis(400);

/// 起動要求 (`zai team run` の投函) を見に行く間隔。
///
/// **毎フレーム `stat` を撃たない。** 画面が動いている間は 60fps で
/// 呼ばれるので、1 フレームに 1 回のシステムコールでも積み上がる
/// (設計原則 3: アイドル時のコストはゼロ)。人が `zai team run` を打って
/// から 1 秒以内に反応すれば、待たされたとは感じない。
pub const LAUNCH_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 1 回の走査で読む画面の行数。
pub const SCAN_ROWS: usize = 200;
/// 1 行あたりの文字数上限。
pub const SCAN_COLS: usize = 400;

/// app 側から渡す 1 セッションの観測材料。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInput {
    pub id: SessionId,
    pub title: String,
    pub provider: String,
    pub state: crate::coordinator::SessionState,
    /// 画面末尾のテキスト (行ごと)。**全履歴を渡さないこと。**
    pub tail: Vec<String>,
}

/// Team 画面のタブ。**同時に 2 つは描かない** (中央ビューと同じ理由で
/// 独立した bool を持たない)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BoardTab {
    #[default]
    Organization,
    Tasks,
    Terminals,
    Timeline,
}

impl BoardTab {
    pub fn key(self) -> &'static str {
        match self {
            BoardTab::Organization => "organization",
            BoardTab::Tasks => "tasks",
            BoardTab::Terminals => "terminals",
            BoardTab::Timeline => "timeline",
        }
    }
    pub const ALL: [BoardTab; 4] = [
        BoardTab::Organization,
        BoardTab::Tasks,
        BoardTab::Terminals,
        BoardTab::Timeline,
    ];
}

/// 画面が Runtime に返す要求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoardAction {
    Close,
    SwitchTab(BoardTab),
    Start,
    Pause,
    Resume,
    Stop,
    Approve(EventId),
    Reject(EventId),
    Retry(TaskId),
    Reassign(TaskId),
    /// エージェントを選ぶ (Inspector を開く)。
    Select(AgentId),
    /// タスクへ追加の指示を足す (Inspector の Edit Instruction)。
    AddContext {
        task: TaskId,
        text: String,
    },
    /// 指示パネル (Inspector) を開く。**相手は開いてから選べる。**
    OpenInstruct,
    /// **選んだエージェントへ、その場で指示を送る。**
    ///
    /// `AddContext` は「次に配るときの文脈」を足すだけで、いま動いている
    /// 端末には 1 バイトも届かない。途中で口を出すにはこちらを使う。
    InstructAgent {
        agent: AgentId,
        text: String,
    },
    SelectTask(TaskId),
    /// 実際の端末を開く。**`ManagedSession` のときだけ**。
    OpenTerminal(SessionId),
    /// **その場で出力を開く / 閉じる。** 画面を切り替えずに中身を見る。
    ToggleAgentOutput(AgentId),
    /// New Team Run のフォームを開く。
    OpenNewRun,
    /// フォームの内容で計画する。
    PlanFromForm,
    /// **画面に出す Run を切り替える** (複数同時に走っているとき)。
    SelectRun(usize),
    /// **Run を 1 本閉じる** (止めて、記憶からも保存からも外す)。
    CloseRun(usize),
    /// dirty な Run の成果物を残して管理だけを閉じる。
    CloseRunKeep,
    /// dirty な Run の成果物を明示的に破棄して閉じる。
    CloseRunDiscard,
    /// Run を閉じる確認を取り消す。状態やプロセスには触れない。
    CloseRunCancel,
    /// 停止・削除に失敗した Close を再試行する。
    RetryCloseRun,
    /// **このワークスペースを Git 管理下にする** (実測の基準点を作る)。
    InitGit,
    /// **短い指示を仕様書へ書き換えてもらう。** 計画の手前の段。
    DraftSpec,
    /// 書き換えた下書きを採用して、そのまま計画へ進む。
    AcceptDraft,
    /// 下書きを捨てる (元の指示のまま進む / やり直す)。
    DiscardDraft,
    /// 未完了 Run の扱い。
    ResumeRun,
    DiscardRun,
    OpenReadOnly,
}

/// 仕様書への書き換えの進み具合。
///
/// **チャネルは持たない。** 描画側は状態だけを見る (受け口は
/// [`TeamPanel`] が持ち、毎フレーム移し替える)。持たせると、
/// 画面を描くたびに受信を試すことになって真実の在り処が 2 つになる。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum DraftState {
    #[default]
    Idle,
    /// 依頼中。`agent` は表示用の名前。
    Running { agent: String },
    /// 書き換わった。**まだ採用していない** — 人の確認を待つ。
    Ready { agent: String, text: String },
    /// 失敗した。理由はそのまま人へ出す。
    Failed { why: String },
}

/// Run ごとの保存先を束ねるフォルダ名 (`<workspace>/runs/<run_id>/`)。
pub const RUNS_DIR: &str = "runs";

/// 1 回の tick で見る報告ファイルの数の上限 (**全 Run 合わせて**)。
///
/// 上限が無いと、暴走したエージェントが吐いた数千個のファイルを
/// 1 フレームで読み切ろうとして画面が止まる。読み直し (書きかけ・壊れた
/// ファイル) もこの数に入る — 数えないと、壊れた山の向こうの正しい報告が
/// 永久に順番待ちになる。置き場の取り決めそのものは [`outbox`]。
pub const OUTBOX_PER_TICK: usize = 32;

/// 復元時に列挙するRun保存ディレクトリの上限。
/// 壊れた状態ルートで起動時のI/Oとメモリを無制限にしない。
const RESTORE_SCAN_MAX: usize = 256;

/// 同じ元 workspace で同時に走らせてよい Run の本数。
/// すべての Run は `run_workspace` で別々の git worktree へ隔離する。
pub const MAX_CONCURRENT_RUNS: usize = 4;

/// New Team Run フォームの入力。
#[derive(Clone, Debug, PartialEq)]
pub struct NewRunForm {
    pub open: bool,
    pub goal_name: String,
    pub spec_path: String,
    pub spec_text: String,
    /// SPEC をファイルから読むか、直接入力か。
    pub from_file: bool,
    pub agents: usize,
    pub max_attempts: u8,
    pub review_required: bool,
    /// エージェントプリセット — チームに置く役割。
    ///
    /// **既定は選べる 6 つ全部** (計画・設計・実装・テスト・レビュー・統合)。
    /// 以前の既定は実装 + レビューの 2 つだったが、それだと編成が
    /// 「実装 1 体 + 統合 1 体」にしかならず、**チームとして分担している
    /// ようには一度も動かなかった**。選べる役割は全部既定で入れて、
    /// 要らないものを外してもらう向きにする。
    ///
    /// **選択が計画を変えるのは 計画 / 設計 / テスト / レビュー の 4 つ。**
    /// 実装と統合は選択に関わらず必ず作られる (実装しない開発も、統合しない
    /// 完了も無いため)。外せてしまうのに外れないのは正直ではないが、
    /// **この版では外せるように見えるだけ**である — 直すなら「常時オン」として
    /// 描くほうで、既定を減らす方向ではない。
    /// 効くほうの 4 つは `planner::tests::選んだ役割は必ず計画を変える` が見張る。
    pub roles: Vec<TeamRole>,
    /// **どのエージェントで動かすか** (プリセット名の一覧)。空なら「おまかせ」。
    ///
    /// 空 = この PC に入っている CLI を役割ごとに配る。
    /// 1 つ = 全員がそれ。複数 = **選んだものの中だけ**で配る。
    /// 「揃えたい」も「この 2 つで混ぜたい」も、ここ 1 か所で決まる。
    pub agent_presets: Vec<String>,
    /// 承認モード (`ask` / `auto` / `agent`)。既存の承認モードと同じ綴り。
    pub approval_mode: String,
    /// コスト上限 (USD)。0 なら上限なし。
    pub cost_limit: f32,
    /// 直近のエラー文面。
    pub error: String,
    /// 仕様書への書き換えの進み具合。
    pub draft: DraftState,
    /// **人が体の数か役割を手で変えたか。**
    ///
    /// 偽のあいだは、計画・書き換えのたびに
    /// [`super::composition::recommend`] の編成を当てる (依頼の形に合わせて
    /// 2 体になったり 12 体になったりする)。真になったら二度と上書きしない —
    /// 人の判断をおすすめで潰さない。
    pub composition_touched: bool,
    /// 作業場の観測結果 (おすすめの根拠。フォルダを切り替えたら取り直す)。
    pub probe: super::composition::WorkspaceProbe,
}

/// フォームのスライダの上限 = おすすめが出せる体の数の上限。
/// **2 か所に数を書かない** — スライダとおすすめが別の上限を持つと、
/// 「おすすめは 20 体なのにスライダは 16 まで」のような嘘が出る。
pub const FORM_MAX_AGENTS: usize = 16;

impl Default for NewRunForm {
    fn default() -> Self {
        Self {
            open: false,
            goal_name: String::new(),
            spec_path: "SPEC.md".to_string(),
            spec_text: String::new(),
            from_file: true,
            // 仕様の初期値
            agents: 4,
            max_attempts: 3,
            review_required: true,
            agent_presets: Vec::new(),
            roles: vec![
                TeamRole::Planner,
                TeamRole::Architect,
                TeamRole::Implementer,
                TeamRole::Tester,
                TeamRole::Reviewer,
                TeamRole::Integrator,
            ],
            approval_mode: "ask".to_string(),
            cost_limit: 0.0,
            error: String::new(),
            draft: DraftState::Idle,
            composition_touched: false,
            probe: super::composition::WorkspaceProbe::default(),
        }
    }
}

/// 未完了 Run が見つかったときの選択。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RestorePrompt {
    #[default]
    None,
    /// 未完了 Run がある。Resume / Open Read Only / Discard を出す。
    Found,
    /// Discard の確認中。
    ConfirmDiscard,
}

/// Run を閉じる確認・進行状態。UI はこの値だけを描き、副作用を起こさない。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ClosePrompt {
    #[default]
    None,
    Confirm {
        run_id: String,
        artifact_path: String,
    },
    Stopping {
        run_id: String,
        artifact_path: String,
    },
    Failed {
        run_id: String,
        artifact_path: String,
        policy: persistence::ClosePolicy,
        why: String,
    },
}

#[derive(Clone)]
struct PendingClose {
    owner: RunOwner,
    policy: persistence::ClosePolicy,
    artifact_path: String,
    started: Instant,
}

struct StopJob {
    owner: RunOwner,
    key: String,
    handle: crate::terminal::ReapHandle,
}

/// 裏で走っている検証 1 件。**受け口と一緒に「何の実行か」を持つ。**
///
/// 送り手が消えたときに失敗として戻すには、タスク・実行 ID・コマンドが
/// 要る。受け口だけを配列で持っていた頃は、切断を握り潰す以外に手が無く、
/// `validation.running` が true のまま残っていた。
/// 実行器の時限を過ぎてから、こちら側が見切るまでの余白 (秒)。
pub const WATCHDOG_SLACK_SECS: u64 = 60;

pub struct ValidationJob {
    /// **持ち主。** 戻ってきた結果を、いまの Run のものだけに限る。
    ///
    /// 実行 ID にも `run_id` が入っているので二重の守りだが、外向きの
    /// 4 つの口と同じ形にしておく — 片方だけ文字列の一致に頼っていると、
    /// 書式を変えた日に静かに守りが消える。
    pub owner: RunOwner,
    pub task: TaskId,
    /// `run_id:task:attempt:generation`。結果の突き合わせに使う。
    pub execution: String,
    /// 走らせているコマンド (切断時に失敗として戻すため)。
    pub commands: Vec<String>,
    /// 開始時刻 (Unix 秒)。見張りの基準になる。
    pub started_at: u64,
    /// 実行器へ渡した時間切れ (秒)。
    ///
    /// 実行器が自分で打ち切れなかったとき (worker がブロックしたまま何も
    /// 送らない) の**最後の砦**。これを持たないと「切断も来ない・結果も
    /// 来ない」経路だけが永久に残る。
    pub timeout_secs: u64,
    /// 停止の札。立てると実行器がプロセスツリーごと終了させる。
    pub cancel: super::launch::CancelFlag,
    /// いま走っている子の PID (0 = 走っていない)。
    ///
    /// **札だけでは足りない場面がある。** アプリを閉じるときは worker ごと
    /// 消えるので、誰も木を落とさない (子は自分のプロセスグループを持つので
    /// 親と一緒には死なない)。閉じる側がここを見て自分で落とす。
    pub pid: super::launch::PidSlot,
    /// 結果の受け口 (実行 ID, タスク, 実測)。
    pub rx: std::sync::mpsc::Receiver<(String, TaskId, Vec<ValidationRun>)>,
}

/// いま面倒を見ているものの数 (workspace を切り替えてよいかの判断)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LiveWork {
    /// セッションが結び付いているエージェント。
    pub agents: usize,
    /// 走っている検証。
    pub validations: usize,
    /// まだ実行していない Effect。
    pub effects: usize,
}

impl LiveWork {
    /// 放置すると孤児になるものがあるか。
    pub fn is_busy(&self) -> bool {
        self.agents > 0 || self.validations > 0 || self.effects > 0
    }

    /// 断る理由 (そのまま画面に出す)。
    pub fn why_blocked(&self) -> String {
        crate::i18n::trf(
            "team.err.workspace_busy",
            &[
                ("agents", self.agents.to_string()),
                ("validations", self.validations.to_string()),
            ],
        )
    }
}

/// Team 画面の状態。
pub struct TeamPanel {
    pub open: bool,
    pub tab: BoardTab,
    pub form: NewRunForm,
    /// 仕様書の書き換えの受け口。**フォームには持たせない**
    /// (描画のたびに受信を試す形にすると真実の在り処が 2 つになる)。
    draft_rx: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    pub selected_agent: Option<AgentId>,
    pub selected_task: Option<TaskId>,
    pub inspector_open: bool,
    /// 端末タブで**その場で開いている**担当 (`None` なら全部畳んだ状態)。
    pub expanded_output: Option<AgentId>,
    /// **このワークスペースは Git 管理下でない。**
    ///
    /// 実測 (`changeset`) は git を使うので、Git が無いと**どの完了報告も
    /// 受理できない**。実機では 7 体が並列で働いているのに、報告が全部
    /// 「変更されたファイルを実測できないので完了にできません」で却下され、
    /// 1 件も終わらなかった。**走らせてから気付かせない。**
    pub needs_git: bool,
    /// Inspector の「追加の指示」入力欄。
    pub inspector_note: String,
    pub restore: RestorePrompt,
    /// Close の確認・停止待ち・失敗。確認前には Run を一切変更しない。
    pub close_prompt: ClosePrompt,
    /// 読み取り専用で開いているか (復元して眺めるだけ)。
    pub read_only: bool,
    /// 直近の説明・エラー (画面の帯に出す)。
    pub notice: String,
    /// 保持している Run。同じ元 workspace につき最大4本で、各Runの実行先は
    /// 専用git worktreeへ分離する。
    ///
    /// **画面が触るのは `active` の1本だけ**なので、既存の操作は
    /// [`TeamPanel::rt`] / [`TeamPanel::rt_mut`] を通してそのまま動く。
    /// 内部に複数本ある場合も、進行と副作用は持ち主を照合して全本へ回す。
    runs: Vec<TeamRuntime>,
    /// 画面に出している Run の位置 (`runs` の添字)。
    active: usize,
    snapshot: Option<TeamSnapshot>,
    /// スナップショットを作り直す必要があるか。
    /// **60fps で全件を作り直さない**ための印。
    dirty: bool,
    /// 実行側へ渡す仕事。**冪等キーを必ず添える** — 実行できたら
    /// [`TeamRuntime::note_effect_done`] へ、失敗したら
    /// [`TeamRuntime::note_effect_failed`] へ返すため。返さない限り
    /// Runtime は「済んだ」と見なさないので、途中で落ちても失われない。
    /// **持ち主 (`RunOwner`) を必ず添える。** 実行の直前にもう一度
    /// 突き合わせ、いまの Run のものでなければ実行しない — workspace を
    /// 切り替えた瞬間に前の Run の仕事が新しいフォルダで動くのを、
    /// キューを空にする偶然ではなく**構造で**防ぐ。
    pending_launches: Vec<(RunOwner, String, super::runtime::AgentLaunchSpec)>,
    /// 実行を頼んだ検証。
    pending_validations: Vec<(RunOwner, String, super::runtime::ValidationSpec)>,
    /// 停止を頼んだセッション。
    pending_stops: Vec<(RunOwner, String, SessionId)>,
    /// セッションのプロセスツリーとPTYが実際に畳まれるのを待つ札。
    stop_jobs: Vec<StopJob>,
    /// 明示的な選択を受けた Run だけが停止・後始末へ進む。
    pending_close: Option<PendingClose>,
    /// 送るべき指示 (持ち主, 冪等キー, セッション ID, 本文)。
    pending_instructions: Vec<(RunOwner, String, (TaskId, SessionId, String))>,
    /// **人が出した指示** (持ち主, 冪等キー, 宛先エージェント, セッション, 本文)。
    ///
    /// Runtime が配る指示と**別の列**にする。混ぜると `take_instructions`
    /// の受け手が「宛先タスクがある」前提で書けなくなる。
    pending_manual: Vec<(RunOwner, String, (AgentId, SessionId, String))>,
    /// 別の Run のものとして捨てた Effect の数 (診断とテストが見る)。
    dropped_effects: usize,
    /// 保存が要るか。
    needs_save: bool,
    workspace: PathBuf,
    /// 状態の置き場の**根**。既定は `~/.zaivern`。
    ///
    /// テストはここを一時ディレクトリへ向ける (`ZAIVERN_HOME` を差し替えると
    /// 並列に走る他のテストへ漏れるので使わない)。
    home: PathBuf,
    /// 次に走査してよい時刻。**`Instant` は永続化しない。**
    next_scan: Option<Instant>,
    /// 次に起動要求を見に行ってよい時刻。
    next_launch_poll: Option<Instant>,
    /// 裏で走らせている検証の受け口。
    ///
    /// **UI スレッドでブロッキング I/O をしない**ので、`try_recv` で
    /// 拾える形にして持つ。
    /// 走らせている検証。**受け口だけを持たない。**
    ///
    /// 送り手が消えたとき (worker の panic) に、どのタスクの・どの実行の・
    /// 何のコマンドが失われたのかが分からないと、`validation.running` を
    /// 下ろすことも失敗として記録することもできない。
    validation_jobs: Vec<ValidationJob>,
    /// **読めなかった報告ファイルの台帳** (回数と、理由を出したか)。
    ///
    /// 保存しない — 再起動したら数え直せばよい。Run をまたいでパスで
    /// 引くので、閉じた Run のぶんは「もう無いファイル」として落ちる。
    outbox_ledger: outbox::Ledger,
    /// **この起動では読み直さない報告ファイル。**
    ///
    /// 隔離も改名もできなかったものが入る。消さずに残すので、そのままだと
    /// 毎 tick 同じ却下を積む。保存しないのは、次の起動では権限が戻って
    /// いるかもしれないから (永久に諦めない)。
    outbox_skip: HashSet<PathBuf>,
    /// 次の tick をどの Run から始めるか。添字ではなく安定した `run_id` を
    /// 覚えるので、Run の追加・削除・並び替えで別物を指さない。
    outbox_run_cursor: Option<String>,
    /// Run ごとの最後に見たファイル。毎回ファイル名順の先頭から始めると、
    /// 壊れた先頭ファイルより後ろが永久に読まれないため、次から再開する。
    outbox_file_cursors: HashMap<String, PathBuf>,
}

impl Default for TeamPanel {
    fn default() -> Self {
        Self {
            open: false,
            tab: BoardTab::default(),
            draft_rx: None,
            expanded_output: None,
            needs_git: false,
            form: NewRunForm::default(),
            selected_agent: None,
            selected_task: None,
            inspector_open: false,
            inspector_note: String::new(),
            restore: RestorePrompt::None,
            close_prompt: ClosePrompt::None,
            read_only: false,
            notice: String::new(),
            runs: Vec::new(),
            active: 0,
            snapshot: None,
            dirty: true,
            pending_launches: Vec::new(),
            pending_validations: Vec::new(),
            pending_stops: Vec::new(),
            stop_jobs: Vec::new(),
            pending_close: None,
            pending_instructions: Vec::new(),
            pending_manual: Vec::new(),
            needs_save: false,
            workspace: PathBuf::new(),
            home: persistence::default_home(),
            next_scan: None,
            next_launch_poll: None,
            validation_jobs: Vec::new(),
            dropped_effects: 0,
            outbox_ledger: outbox::Ledger::default(),
            outbox_skip: HashSet::new(),
            outbox_run_cursor: None,
            outbox_file_cursors: HashMap::new(),
        }
    }
}

impl TeamPanel {
    pub fn has_run(&self) -> bool {
        self.rt().is_some()
    }

    /// いまの Goal の状態 (画面と操作の判断に使う唯一の入口)。
    pub fn goal_status(&self) -> Option<GoalStatus> {
        self.rt().map(|r| r.goal().status)
    }

    /// 画面が読むスナップショット。無ければ `None`。
    pub fn snapshot(&self) -> Option<&TeamSnapshot> {
        self.snapshot.as_ref()
    }

    /// 状態の置き場 (`<根>/team/<ワークスペースキー>/`)。
    /// いま面倒を見ている workspace。**計画も SPEC の解決もここを基準にする。**
    ///
    /// 画面の「いまのフォルダ」(`agent_cwd`) と食い違うことがある — 実行中の
    /// Run があると切り替えを断るため。基準を 2 つ持つと、Run の workspace と
    /// 違う場所の SPEC を読む・違う場所でエージェントを起こす、が起きる。
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn state_dir(&self) -> PathBuf {
        persistence::team_dir_in(&self.home, &self.workspace)
    }

    /// **いま面倒を見ているものがあるか。**
    ///
    /// あるうちは workspace を切り替えない — 切り替えると Runtime への参照が
    /// 消えて、**画面から消えたのに裏で動き続けるエージェントと検証**が残る
    /// (誰も結果を受け取らず、誰も止められない)。
    ///
    /// **`runtime.is_some()` では広すぎる。** 計画しただけ・停止し終えた
    /// Run で永久に切り替え不能になる。見るのは「放置すると孤児になるもの」:
    ///
    /// * セッションが結び付いているエージェント
    /// * 走っている検証
    /// * まだ実行していない Effect
    pub fn live_work(&self) -> LiveWork {
        LiveWork {
            // エージェントは Runtime が知っている。
            // **全部の Run を数える。** 画面に出していない Run の
            // エージェントも実在するので、数えないと「居ないことになる」。
            agents: self
                .runs
                .iter()
                .map(|rt| {
                    rt.agents()
                        .iter()
                        .filter(|a| a.kind == AgentKind::ManagedSession && a.session_id.is_some())
                        .count()
                })
                .sum(),
            // **検証と未実行の仕事は画面側の持ち物。** Runtime が無い
            // ときに 0 と答えると、`discard_run` の直後 (Runtime は捨てた
            // が子プロセスはまだ畳んでいる) に「空いている」と嘘をつく。
            validations: self.validation_jobs.len(),
            effects: self.pending_launches.len()
                + self.pending_instructions.len()
                + self.pending_manual.len()
                + self.pending_stops.len()
                + self.pending_validations.len()
                + self.stop_jobs.len()
                + usize::from(self.pending_close.is_some()),
        }
    }

    /// ワークスペースを設定し、保存済みの Run があれば知らせる。
    ///
    /// **実行中のものがあれば切り替えない。** 断った理由をそのまま返す
    /// (黙って無視すると、利用者は「押したのに何も起きない」を見る)。
    pub fn attach_workspace(&mut self, ws: &Path) -> Result<(), String> {
        if self.workspace == ws {
            return Ok(());
        }
        let live = self.live_work();
        if live.is_busy() {
            let why = live.why_blocked();
            self.notice = why.clone();
            return Err(why);
        }
        // ここへ来るのは「面倒を見ているものが無い」ときだけ。それでも
        // **捨てる前に後始末をする** — 走り出しかけた検証と、書き残しの
        // 保存を置き去りにしない。
        self.cancel_all_validations();
        self.validation_jobs.clear();
        self.pending_launches.clear();
        self.pending_instructions.clear();
        self.pending_manual.clear();
        self.pending_stops.clear();
        self.stop_jobs.clear();
        self.pending_close = None;
        self.pending_validations.clear();
        self.save_if_needed();
        self.workspace = ws.to_path_buf();
        self.runs.clear();
        self.active = 0;
        self.snapshot = None;
        self.read_only = false;
        // **復元できるものがあるときだけ案内する。** 根の控えの有無 (`has_run`)
        // だけで見ると、閉じた Run の控えが残っているだけで「保存がある」と
        // 案内し、押した先で復元経路が墓標で断る (押せるのに何も起きない)。
        self.restore = if persistence::has_restorable_run(&self.state_dir()) {
            RestorePrompt::Found
        } else {
            RestorePrompt::None
        };
        self.close_prompt = ClosePrompt::None;
        Ok(())
    }

    /// **いまの設定をフォームの初期値にする** (既存設定は書き換えない)。
    ///
    /// フォームの既定は `ask` / `0` で、`0` は「上限なし」を意味する。
    /// 読まずに計画へ流すと、`agent` / `25` で使っている人の環境で
    /// フォームを開いただけで承認モードが下がり、上限が外れる。
    ///
    /// **開いている間は上書きしない。** 人が選び直した値を、次のフレームの
    /// 読み込みで元へ戻してしまう。
    pub fn seed_guardrails(&mut self, approval_mode: &str, cost_limit: f32) {
        if self.form.open {
            return;
        }
        self.form.approval_mode = approval_mode.to_string();
        self.form.cost_limit = cost_limit.max(0.0);
    }

    /// いまの Run にだけ効く締め具合 (Run が無ければ `None`)。
    #[cfg(test)]
    pub fn run_guardrails(&self) -> Option<RunGuardrails> {
        self.rt().map(|rt| rt.run().guardrails.clone())
    }

    /// 指定した Run にだけ効く締め具合。
    pub fn run_guardrails_for(&self, owner: &RunOwner) -> Option<RunGuardrails> {
        let pos = self.run_pos_of_owner(owner)?;
        Some(self.runs[pos].run().guardrails.clone())
    }

    // ── 仕様書への書き換え ───────────────────────────────────────────

    /// 書き換えを頼んだ (受け口を預かる)。
    ///
    /// **重ねて始めない。** 走っている最中にもう一度押されても、
    /// 先に頼んだほうの結果を待つ (2 本目を起こすと 2 通の下書きが返り、
    /// どちらを採ったのか誰にも分からなくなる)。
    pub fn begin_draft(
        &mut self,
        agent: &str,
        rx: std::sync::mpsc::Receiver<Result<String, String>>,
    ) {
        if matches!(self.form.draft, DraftState::Running { .. }) {
            return;
        }
        self.form.draft = DraftState::Running {
            agent: agent.to_string(),
        };
        self.draft_rx = Some(rx);
    }

    /// 走っている書き換えがあるか (毎フレームの再描画要求に使う)。
    pub fn drafting(&self) -> bool {
        matches!(self.form.draft, DraftState::Running { .. })
    }

    /// 受け口を覗いて、届いていればフォームへ移す。**待たない。**
    pub fn poll_draft(&mut self) {
        let Some(rx) = self.draft_rx.as_ref() else {
            return;
        };
        let got = match rx.try_recv() {
            Ok(v) => v,
            // 送り手が消えた = 作業スレッドが落ちた。黙って待ち続けない。
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err(crate::i18n::tr("team.draft.lost"))
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
        };
        self.draft_rx = None;
        let agent = match &self.form.draft {
            DraftState::Running { agent } => agent.clone(),
            _ => String::new(),
        };
        self.form.draft = match got {
            // **受け取る前に確かめる。** 1 件にしかならない下書きを通すと、
            // 確認まで出しておいて結果は元と同じになる。
            Ok(text) => match super::spec_writer::accept(&text) {
                Ok(()) => DraftState::Ready { agent, text },
                Err(why) => DraftState::Failed { why },
            },
            Err(why) => DraftState::Failed { why },
        };
    }

    /// 下書きを採用する (SPEC は直接入力へ移す)。**採用は人が決める。**
    pub fn accept_draft(&mut self) {
        if let DraftState::Ready { text, .. } = std::mem::take(&mut self.form.draft) {
            self.form.spec_text = text;
            self.form.from_file = false;
            self.form.error.clear();
        }
    }

    /// 下書きを捨てる (元の指示のまま進む / やり直す)。
    pub fn discard_draft(&mut self) {
        self.form.draft = DraftState::Idle;
        self.draft_rx = None;
    }

    /// 計画を作って Runtime を立てる (まだ開始はしない)。
    ///
    /// `roles` はフォームの「エージェントプリセット」、`title_override` は
    /// 「Goal 名」。**どちらも実際に計画へ効く** — 選べるのに何も変わらない
    /// 入力欄を残さない。
    pub fn plan_with(
        &mut self,
        spec_text: &str,
        source: &str,
        opts: RunOptions,
        roles: Vec<TeamRole>,
        title_override: &str,
    ) -> Result<(), String> {
        // 上限とIDの所有権は、計画を組み立てる前に確定する。同じrun_idを
        // 受け入れるとworktree・outbox・保存が同じ所有者になり、Run間分離が
        // 構造から崩れる。
        self.refuse_if_busy()?;
        if !outbox::valid_run_id(&opts.run_id) {
            return Err(format!(
                "run_id {:?} は Team Run の識別子に使えません",
                opts.run_id
            ));
        }
        let state = self.state_dir();
        let duplicate = self.runs.iter().any(|r| r.run().run_id == opts.run_id)
            || persistence::is_closed(&state, &opts.run_id)
            || persistence::run_dir_in(&state, &opts.run_id).is_some_and(|p| p.exists())
            || super::run_workspace::expected_root(&self.home, &self.workspace, &opts.run_id)
                .is_ok_and(|p| std::fs::symlink_metadata(p).is_ok());
        if duplicate {
            return Err(format!(
                "run_id {:?} は既存または後始末待ちの Team Run が使用中です",
                opts.run_id
            ));
        }
        let plan = StaticPlanner
            .plan(PlanInput {
                spec: spec_text.to_string(),
                source: source.to_string(),
                agent_count: opts.agent_count,
                review_required: opts.review_required,
                workspace_root: self.workspace.clone(),
                roles,
            })
            .map_err(|e| e.detail())?;
        // **配る前に計画そのものを検証する。**
        let issues = super::graph::validate_plan(&plan.tasks, &plan.goal.definition_of_done);
        // 検証コマンドが 1 本も無い計画か (`plan` はこの後 Runtime へ渡すので先に見る)。
        let no_validation = plan.tasks.iter().all(|t| t.validation_commands.is_empty());
        if !issues.is_empty() {
            return Err(issues
                .iter()
                .map(|i| i.detail())
                .collect::<Vec<_>>()
                .join("\n"));
        }
        // **動いているチームを別の計画で潰さない。** 置き換えると、走って
        // いる検証と起動済みのエージェントの面倒を見る相手が消える
        // (結果は `run_id` 違いで捨てられ、プロセスだけが残る)。
        let ws = self.workspace.clone();
        let mut rt = TeamRuntime::from_plan(plan, ws, opts);
        let t = title_override.trim();
        if !t.is_empty() {
            rt.rename_goal(t);
        }
        let dir = self.outbox_dir(&rt.run().run_id);
        rt.set_outbox(dir);
        // **走らせる前に確かめる。** 基準点が無いと実測できず、どの完了報告も
        // 受理できない (エージェントは働くのに 1 件も終わらない)。
        // 見るのは `.git` の有無ではなく**使える基準点があるか** — HEAD の
        // 無いリポジトリを「準備完了」と読むと、そのまま Run が走ってしまう。
        self.refresh_git_readiness();
        self.runs.push(rt);
        self.active = self.runs.len() - 1;
        self.read_only = false;
        self.restore = RestorePrompt::None;
        // **「検証なし」は帯で言わない。** 盤面のヘッダに「⚠ 検証なし」の札が
        // 常時出ていて、ホバーで理由まで読める。帯にも同じことを長い文章で
        // 出していたので、開くたびに 1 行まるごと占領して**同じことを 2 回**
        // 言っていた (CLAUDE.md「増やす前に減らせないかを考える」)。
        //
        // 札のほうを残すのは、状態から導くので**消えない**から。帯は次の
        // 通知で上書きされて消えるので、そもそも常設の警告には向かない。
        let _ = no_validation;
        self.dirty = true;
        self.needs_save = true;
        Ok(())
    }

    /// 面倒を見ているものがあるなら断る (理由をそのまま返す)。
    fn refuse_if_busy(&mut self) -> Result<(), String> {
        // 既存 Run を置き換えず、5本目は計画を作る前に断る。
        if self.runs.len() < MAX_CONCURRENT_RUNS {
            return Ok(());
        }
        // 翻訳カタログがまだ初期化されていない起動早期でも、上限数だけは
        // 必ず分かるようにする。キー文字列だけで断るのは製品エラーではない。
        let translated = crate::i18n::trf(
            "team.err.too_many_runs",
            &[("n", MAX_CONCURRENT_RUNS.to_string())],
        );
        let why = format!("{translated} (最大 {MAX_CONCURRENT_RUNS} Run)");
        self.notice = why.clone();
        Err(why)
    }

    /// 既定のプリセットで計画する (テストの足場。製品の入口は CLI 経由も
    /// GUI 経由も [`Self::plan_with`] 1 本 — 投函の Run にも編成を当てるため)。
    #[cfg(test)]
    pub fn plan(&mut self, spec_text: &str, source: &str, opts: RunOptions) -> Result<(), String> {
        self.plan_with(spec_text, source, opts, Vec::new(), "")
    }

    /// 保存された Run を復元する。
    pub fn restore_run(&mut self, read_only: bool) -> Result<(), String> {
        let root = self.state_dir();
        // **閉じた Run は復元しない。** 墓標 ([`persistence::mark_closed`]) は
        // 削除の前に書かれるので、削除に失敗した Run もここで断れる。
        // 片付けはここで試し直し、済んだら墓標を掃く。
        self.sweep_closed_runs(&root);
        // **Run ごとの置き場を先に全部読む。** 1 本だけ読むと、
        // 同時に走らせていた残りが再起動で消える。
        let mut loaded = 0usize;
        // 既に持っている Run と二重に持たない (復元は追加であって置き換えではない)。
        let mut seen: HashSet<String> = self.runs.iter().map(|r| r.run().run_id.clone()).collect();
        let mut skipped: Vec<String> = Vec::new();
        let mut capped = 0usize;
        if let Ok(rd) = std::fs::read_dir(root.join(RUNS_DIR)) {
            let mut entries = rd.filter_map(|e| e.ok());
            let mut dirs: Vec<std::path::PathBuf> = entries
                .by_ref()
                .take(RESTORE_SCAN_MAX.saturating_add(1))
                .map(|e| e.path())
                .collect();
            if dirs.len() > RESTORE_SCAN_MAX {
                dirs.truncate(RESTORE_SCAN_MAX);
                skipped.push(format!(
                    "Run保存の走査上限{RESTORE_SCAN_MAX}件を超えました"
                ));
            }
            dirs.sort();
            for d in dirs {
                let name = d
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                // フォルダ名が保存の名前として安全でないなら**中を読まない**
                // (読んだ `run_id` をパスに使う経路が後ろにある)。
                if !outbox::valid_run_id(&name) {
                    skipped.push(format!("{name:?} (安全でない名前)"));
                    continue;
                }
                if persistence::is_closed(&root, &name) {
                    continue;
                }
                let LoadOutcome::Loaded(s) = persistence::load(&d) else {
                    continue;
                };
                // **保存の中の `run_id` はフォルダ名と一致していなければならない。**
                // ずれていたら、置き場を移された・手で書き換えられたのどちらか。
                // その `run_id` で保存し直すと別の場所へ書くので、復元しない。
                if s.run.run_id != name {
                    skipped.push(format!("{name:?} (中身の run_id が {:?})", s.run.run_id));
                    continue;
                }
                if !seen.insert(name.clone()) {
                    continue;
                }
                // **同時に走らせられる本数を超えて復元しない。** 上限は
                // 資源の約束 ([`MAX_CONCURRENT_RUNS`]) なので、復元でも守る。
                // 超えたぶんは保存のまま残す (消さない)。
                if self.runs.len() >= MAX_CONCURRENT_RUNS {
                    capped += 1;
                    continue;
                }
                let outbox_dir = match outbox::prepare_run_dir(&root, &name) {
                    Ok(dir) => dir,
                    Err(why) => {
                        skipped.push(format!("{name:?} ({why})"));
                        continue;
                    }
                };
                let mut rt = match self.restore_saved(*s, &d, read_only) {
                    Ok(rt) => rt,
                    Err(why) => {
                        skipped.push(format!("{name:?} ({why})"));
                        continue;
                    }
                };
                rt.set_outbox(outbox_dir);
                self.runs.push(rt);
                loaded += 1;
            }
        }
        // **どちらも黙らない。** 片方で上書きすると、読まなかった保存が
        // あることを利用者が知る手立てが消える。
        let mut notes: Vec<String> = Vec::new();
        if !skipped.is_empty() {
            notes.push(crate::i18n::trf(
                "team.notice.restore_skipped",
                &[
                    ("n", skipped.len().to_string()),
                    ("why", skipped.join(" / ")),
                ],
            ));
        }
        if capped > 0 {
            notes.push(crate::i18n::trf(
                "team.notice.restore_capped",
                &[
                    ("n", loaded.to_string()),
                    ("rest", capped.to_string()),
                    ("max", MAX_CONCURRENT_RUNS.to_string()),
                ],
            ));
        }
        if !notes.is_empty() {
            self.notice = notes.join(" / ");
        }
        if loaded > 0 {
            self.active = 0;
            self.read_only = read_only;
            self.restore = RestorePrompt::None;
            // **Run が生きている間は `needs_git` が実態を映していること。**
            // 復元でここを飛ばすと、基準点の無いフォルダで「準備完了」の
            // まま走り出す (計画の入口と同じ関門をここにも置く)。
            self.refresh_git_readiness();
            self.dirty = true;
            return Ok(());
        }
        if !self.runs.is_empty() {
            // 何も足せなかったが、持っている Run はある (上限か重複)。
            self.restore = RestorePrompt::None;
            return Ok(());
        }
        let dir = root;
        match persistence::load(&dir) {
            LoadOutcome::Loaded(s) => {
                let id = s.run.run_id.clone();
                // 根の控えも同じ関門: 閉じた Run と安全でない名前は復元しない。
                if !outbox::valid_run_id(&id) || persistence::is_closed(&dir, &id) {
                    self.restore = RestorePrompt::None;
                    return Err("保存された Team Run がありません".to_string());
                }
                let outbox_dir = outbox::prepare_run_dir(&dir, &id)?;
                let mut rt = self.restore_saved(*s, &dir, read_only)?;
                rt.set_outbox(outbox_dir);
                self.runs.push(rt);
                self.active = self.runs.len() - 1;
                self.read_only = read_only;
                self.restore = RestorePrompt::None;
                self.refresh_git_readiness();
                self.dirty = true;
                Ok(())
            }
            LoadOutcome::Empty => Err("保存された Team Run がありません".to_string()),
            LoadOutcome::Corrupt { backed_up, reason } => {
                Err(format!("{reason}\n退避しました: {}", backed_up.join(", ")))
            }
            LoadOutcome::Newer { found } => Err(format!(
                "保存された状態の版 ({found}) が新しすぎます。Zaivern を更新してください。"
            )),
        }
    }

    /// 保存された Run の元 workspace / 実行 workspace 対応を検査する。
    /// 実行中だった旧形式に worktree 記録が無ければ、復元前に専用
    /// worktree を作り、対応の保存が成功してから Runtime を返す。
    fn restore_saved(
        &self,
        mut saved: persistence::Saved,
        save_dir: &Path,
        read_only: bool,
    ) -> Result<TeamRuntime, String> {
        let source = self
            .workspace
            .canonicalize()
            .map(crate::pathx::plain)
            .map_err(|e| format!("元 workspace を確認できません: {e}"))?;
        let recorded = PathBuf::from(&saved.run.workspace)
            .canonicalize()
            .map(crate::pathx::plain)
            .map_err(|e| format!("保存された元 workspace を確認できません: {e}"))?;
        if source != recorded {
            return Err("保存された Run は別の workspace のものです".to_string());
        }

        let execution = if let Some(run_workspace) = saved.run.run_workspace.as_ref() {
            super::run_workspace::restore(
                &self.home,
                &source,
                &saved.run.run_id,
                run_workspace,
            )?
        } else if read_only || saved.goal.status == GoalStatus::Ready {
            // まだ開始していない計画プレビューは、Start 時に worktree を作る。
            source.clone()
        } else {
            let run_workspace =
                super::run_workspace::create(&self.home, &source, &saved.run.run_id)?;
            saved.run.workspace = run_workspace.source_workspace.clone();
            saved.run.run_workspace = Some(run_workspace.clone());
            if let Err(e) = persistence::save(save_dir, &saved) {
                let cleanup = super::run_workspace::remove_clean(
                    &self.home,
                    &source,
                    &saved.run.run_id,
                    &run_workspace,
                )
                .err()
                .map(|why| format!(" / worktree の後始末も失敗: {why}"))
                .unwrap_or_default();
                return Err(format!("{}{cleanup}", e.detail()));
            }
            PathBuf::from(&run_workspace.execution_workspace)
        };
        Ok(TeamRuntime::restore_in(saved, source, execution))
    }

    /// 保存された Run を消す (**確認済みの呼び出しだけ**)。
    ///
    /// **走っている検証を置き去りにしない。** 記録だけ消して `cargo test` が
    /// 走り続けると、誰も結果を受け取らないプロセスがリポジトリを触り続ける。
    pub fn discard_run(&mut self) -> Result<usize, String> {
        // **閉じるのと同じ不変条件: 捨てる Run の担当へ停止が届く。**
        // 記録だけ消して担当を残すと、盤面から消えたのに端末では動き続ける
        // (誰も結果を受け取らないまま、リポジトリを触り続ける)。
        // 効果を積むのは Run を手放す**前** — `absorb_for` は持ち主で引くので、
        // 手放した後だと ACK の返し先が無くなる。
        for i in 0..self.runs.len() {
            let effects = self.runs[i].close();
            let owner = self.runs[i].owner();
            self.absorb_for(owner, effects);
        }
        self.cancel_all_validations();
        let dir = self.state_dir();
        // **消す前に、持っている Run 全部に墓標を書く。** 下の削除が途中で
        // 失敗しても、次の起動で復活しない (閉じる経路と同じ不変条件)。
        let runs: Vec<(String, Option<super::run_workspace::RunWorkspace>)> = self
            .runs
            .iter()
            .map(|r| (r.run().run_id.clone(), r.run().run_workspace.clone()))
            .collect();
        let ids: Vec<String> = runs.iter().map(|(id, _)| id.clone()).collect();
        for id in &ids {
            if let Err(e) = persistence::mark_closed(&dir, id) {
                let why = crate::i18n::trf(
                    "team.notice.run_state_cleanup_failed",
                    &[("run", id.clone()), ("e", e.detail())],
                );
                self.notice = why.clone();
                return Err(why);
            }
        }
        // 保存された任意のパスは削除対象にしない。元 workspace・run_id から
        // 決定パスを再計算し、同じ repository の登録済み worktree と確認できた
        // Run だけを外す。失敗時は Runtime と保存を残すので、利用者が同じ
        // 破棄操作を安全に再試行できる。
        for (id, saved) in &runs {
            let discovered;
            let worktree = match saved.as_ref() {
                Some(w) => Some(w),
                None => {
                    discovered = super::run_workspace::discover(&self.home, &self.workspace, id)
                        .map_err(|e| {
                            format!("Run {id} の専用 worktree を安全に確認できません: {e}")
                        })?;
                    discovered.as_ref()
                }
            };
            if let Some(worktree) = worktree {
                if let Err(e) =
                    super::run_workspace::remove_discarded(
                        &self.home,
                        &self.workspace,
                        id,
                        worktree,
                    )
                {
                    let why = format!("Run {id} の専用 worktree を削除できません: {e}");
                    self.notice = why.clone();
                    return Err(why);
                }
            }
        }
        let n = persistence::reset(&dir).map_err(|e| e.detail())?;
        // **Run ごとの置き場も消す。** 残すと、次に開いたときに
        // 消したはずの Run が戻ってくる。報告置き場も同じ — 全 Run を捨てる
        // ので `outbox/` ごと消してよい (Run 単位の関門は要らない)。
        // 失敗は握り潰さない: 残った保存は墓標が復元を止めるが、残っている
        // こと自体は人に見せる。
        let runs_gone = persistence::remove_dir_checked(&dir.join(RUNS_DIR));
        let outbox_gone = persistence::remove_dir_checked(&dir.join(outbox::DIR_NAME));
        self.outbox_ledger.prune_missing();
        self.runs.clear();
        self.active = 0;
        self.snapshot = None;
        self.restore = RestorePrompt::None;
        match (runs_gone, outbox_gone) {
            (Ok(()), Ok(())) => {
                for id in &ids {
                    let _ = persistence::unmark_closed(&dir, id);
                }
                Ok(n)
            }
            (Err(e), _) => Err(crate::i18n::trf(
                "team.notice.run_state_cleanup_failed",
                &[("run", ids.join(", ")), ("e", e)],
            )),
            (Ok(()), Err(e)) => Err(crate::i18n::trf(
                "team.notice.outbox_cleanup_failed",
                &[("run", ids.join(", ")), ("e", e)],
            )),
        }
    }

    /// 人の操作を Runtime へ渡す。
    pub fn act(&mut self, action: TeamAction) {
        if self.read_only {
            self.notice = "読み取り専用で開いています (操作できません)".to_string();
            return;
        }
        if matches!(&action, TeamAction::Start) {
            if let Err(why) = self.prepare_active_workspace() {
                self.notice = why;
                self.refresh_git_readiness();
                self.dirty = true;
                return;
            }
        }
        let Some(rt) = self.rt_mut() else {
            return;
        };
        let effects = rt.apply_action(action);
        self.absorb(effects);
        self.dirty = true;
    }

    /// active Run の専用 worktree を作り、対応を保存してから開始可能にする。
    /// worktree 作成後の保存に失敗した場合は Runtime を共有 workspace へ
    /// 向けず、そのまま Ready で止める。
    fn prepare_active_workspace(&mut self) -> Result<(), String> {
        let pos = self.active;
        let Some(rt) = self.runs.get(pos) else {
            return Err("Team Run がありません".to_string());
        };
        if let Some(saved) = rt.run().run_workspace.as_ref() {
            let execution = super::run_workspace::restore(
                &self.home,
                &self.workspace,
                &rt.run().run_id,
                saved,
            )?;
            if execution != rt.workspace() {
                return Err("Run の実行 workspace が保存された対応と違います".to_string());
            }
            return Ok(());
        }

        let id = rt.run().run_id.clone();
        let outbox_dir = outbox::prepare_run_dir(&self.state_dir(), &id)?;
        let run_workspace = super::run_workspace::create(&self.home, &self.workspace, &id)?;
        // まず対応を含む完全なスナップショットを書く。これが成功するまで
        // Runtime の cwd は切り替えないので、途中失敗で共有 workspace を走らない。
        let mut saved = self.runs[pos].to_saved();
        saved.run.workspace = run_workspace.source_workspace.clone();
        saved.run.run_workspace = Some(run_workspace.clone());
        let dir = persistence::run_dir_in(&self.state_dir(), &id)
            .ok_or_else(|| format!("Run {id:?} の保存先を作れません"))?;
        if let Err(e) = persistence::save(&dir, &saved) {
            let cleanup = super::run_workspace::remove_clean(
                &self.home,
                &self.workspace,
                &id,
                &run_workspace,
            )
            .err()
            .map(|why| format!(" / worktree の後始末も失敗: {why}"))
            .unwrap_or_default();
            return Err(format!("{}{cleanup}", e.detail()));
        }
        self.runs[pos].set_run_workspace(run_workspace);
        self.runs[pos].set_outbox(outbox_dir);
        self.needs_save = true;
        Ok(())
    }

    /// 走査してよい時刻か。**毎フレームは走らせない。**
    pub fn scan_due(&mut self, now: Instant) -> bool {
        match self.next_scan {
            Some(t) if now < t => false,
            _ => {
                self.next_scan = Some(now + SCAN_INTERVAL);
                true
            }
        }
    }

    /// 起動要求を見に行ってよい時刻か。**毎フレームは撃たない。**
    pub fn launch_poll_due(&mut self, now: Instant) -> bool {
        match self.next_launch_poll {
            Some(t) if now < t => false,
            _ => {
                self.next_launch_poll = Some(now + LAUNCH_POLL_INTERVAL);
                true
            }
        }
    }

    /// この Run の報告置き場 (`<state_dir>/outbox/<run_id>/`)。
    ///
    /// `run_id` が置き場の名前として安全でなければ**空** — 置き場を持たない
    /// Run になる (画面から読む経路だけ)。作らないものは閉じるときも消さない
    /// ので、作る側と消す側が同じ関門 ([`outbox::run_dir`]) を通る。
    fn outbox_dir(&self, run_id: &str) -> PathBuf {
        outbox::run_dir(&self.state_dir(), run_id).unwrap_or_default()
    }

    /// **置き場に届いた報告を読み、取り込めたファイルだけ消す。**
    ///
    /// 戻りは「どのセッションの画面テキストへ足すか」。画面から読む経路と
    /// 同じ入口へ流すので、受理・却下の判断は 1 か所のまま
    /// (第 2 の取り込み経路を作らない)。
    ///
    /// * **Run ごとに独立した表で配送先を引く。** 全 Run を 1 つの表に
    ///   混ぜると、同じ ID の担当 (`team-lead` は毎 Run に居る) が後の Run の
    ///   セッションで上書きされ、前の Run の報告が別の Run へ流れる
    /// * **読んで・解析して・配送できたときだけ消す。** 以前は読む前に消して
    ///   いたので、書きかけを読んだ瞬間にその報告は永久に失われていた
    /// * 読めないファイルは残して次の tick で読み直す。上限
    ///   ([`outbox::MAX_ATTEMPTS`]) で `rejected/` へ隔離し、理由を Run の
    ///   記録へ残す (壊れた 1 個が残り続けて毎 tick 同じ失敗を出さない)
    /// * 担当はファイル名と本文の両方で決める ([`outbox::judge`])。
    ///   `agent-1` が `agent-10-…` を拾うことは無い
    /// * 1 tick に見る数は全 Run 合わせて [`OUTBOX_PER_TICK`]
    /// * **セッションを見ない。** 担当 ID だけで Runtime へ渡すので、
    ///   結び付く前・観測に載らない tick・プロセスが終わった直後に書かれた
    ///   報告も落ちない (画面経由はそこで必ず落ちる)
    /// * 消すのは [`TeamRuntime::accept_outbox`] が受理を返した後だけ。
    ///   途中で落ちてもファイルは残り、次の tick で読み直す
    fn drain_outbox(&mut self, now: u64) -> usize {
        if self.read_only || self.runs.is_empty() {
            return 0;
        }
        let mut budget = OUTBOX_PER_TICK;
        let mut taken = 0usize;

        let closing = self.pending_close.as_ref().map(|p| p.owner.run_id.as_str());
        let live: HashSet<String> = self
            .runs
            .iter()
            .filter(|rt| Some(rt.run().run_id.as_str()) != closing)
            .map(|rt| rt.run().run_id.clone())
            .collect();
        self.outbox_file_cursors.retain(|run, _| live.contains(run));
        if self
            .outbox_run_cursor
            .as_ref()
            .is_some_and(|run| !live.contains(run))
        {
            self.outbox_run_cursor = None;
        }

        struct LaneQueue {
            run: usize,
            run_id: String,
            files: VecDeque<PathBuf>,
        }

        let mut order: Vec<usize> = (0..self.runs.len())
            .filter(|&i| Some(self.runs[i].run().run_id.as_str()) != closing)
            .collect();
        if let Some(cursor) = self.outbox_run_cursor.as_deref() {
            if let Some(pos) = order
                .iter()
                .position(|&i| self.runs[i].run().run_id == cursor)
            {
                let next = (pos + 1) % order.len();
                order.rotate_left(next);
            }
        }
        let mut lanes = Vec::with_capacity(order.len());
        for run in order {
            let dir = self.runs[run].outbox().to_path_buf();
            if dir.as_os_str().is_empty() {
                continue;
            }
            let run_id = self.runs[run].run().run_id.clone();
            let mut files = outbox::list_reports(&dir);
            files.retain(|f| !self.outbox_skip.contains(f));
            if let Some(cursor) = self.outbox_file_cursors.get(&run_id) {
                let next = files.partition_point(|f| f <= cursor);
                if !files.is_empty() {
                    let len = files.len();
                    files.rotate_left(next % len);
                }
            }
            lanes.push(LaneQueue {
                run,
                run_id,
                files: files.into(),
            });
        }

        // 1 周につき各 Run から最大 1 ファイル。全体上限は維持したまま、
        // 壊れた Run や大量投入の Run が後続を独占できない。
        while budget > 0 {
            let mut progressed = false;
            for lane in &mut lanes {
                if budget == 0 {
                    break;
                }
                let Some(file) = lane.files.pop_front() else {
                    continue;
                };
                progressed = true;
                budget -= 1;
                self.outbox_run_cursor = Some(lane.run_id.clone());
                self.outbox_file_cursors
                    .insert(lane.run_id.clone(), file.clone());
                if self.process_outbox_file(lane.run, &lane.run_id, &file, now) {
                    taken += 1;
                }
            }
            if !progressed {
                break;
            }
        }
        self.outbox_ledger.prune_missing();
        taken
    }

    /// 1 ファイルだけを処理する。公平な選択と内容の検証を分け、異常な
    /// 1 ファイルが他 Run の順番まで巻き込まない。
    fn process_outbox_file(&mut self, run: usize, run_id: &str, file: &Path, now: u64) -> bool {
        let file = match outbox::claim_report(file) {
            Ok(file) => file,
            Err(e) => {
                self.outbox_retry(run, file, format!("報告を安全に確保できません: {e}"));
                return false;
            }
        };
        let ids: Vec<AgentId> = self.runs[run]
            .agents()
            .iter()
            .map(|a| a.id.clone())
            .collect();
        let stem = file
            .file_stem()
            .and_then(|x| x.to_str())
            .unwrap_or_default()
            .to_string();
        let verdict = match outbox::read_report(&file) {
            ReadOutcome::Body(body) => outbox::judge(&stem, &body, &ids, run_id),
            ReadOutcome::Retry(why) => Verdict::Retry(why),
            ReadOutcome::Reject(why) => Verdict::Reject { agent: None, why },
        };
        match verdict {
            Verdict::Deliver { agent, kind, body } => {
                match self.runs[run].accept_outbox(&agent, kind, &body, now) {
                    Ok(outcome) => match self.persist_accepted_outbox(run) {
                        Err(e) => self.outbox_retry(
                            run,
                            &file,
                            format!("受理した状態を保存できません: {e}"),
                        ),
                        Ok(()) => match outbox::remove_report(&file) {
                        Ok(()) => {
                            self.outbox_ledger.forget(&file);
                            return outcome == super::runtime::AcceptOutcome::Applied;
                        }
                        Err(e) => self.outbox_retry(
                            run,
                            &file,
                            format!("受理したが消せません: {e}"),
                        ),
                    },
                    },
                    Err(why) => self.outbox_quarantine(run, &file, Some(agent), why),
                }
            }
            Verdict::Retry(why) => self.outbox_retry(run, &file, why),
            Verdict::Reject { agent, why } => self.outbox_quarantine(run, &file, agent, why),
        }
        false
    }

    fn persist_accepted_outbox(&mut self, run: usize) -> Result<(), String> {
        let rt = self
            .runs
            .get(run)
            .ok_or_else(|| "受理したRunが見つかりません".to_string())?;
        let root = self.state_dir();
        let dir = persistence::run_dir_in(&root, &rt.run().run_id)
            .ok_or_else(|| "受理したRunの安全な保存先を作れません".to_string())?;
        persistence::save(&dir, &rt.to_saved()).map_err(|e| e.detail())?;
        if run == 0 {
            persistence::save(&root, &rt.to_saved()).map_err(|e| e.detail())?;
        }
        Ok(())
    }

    /// 読めなかった報告を数え、上限に達したら隔離する。
    fn outbox_retry(&mut self, run: usize, file: &Path, why: String) {
        let n = self.outbox_ledger.bump(file);
        if n < outbox::MAX_ATTEMPTS {
            return;
        }
        self.outbox_quarantine(
            run,
            file,
            None,
            format!("{n} 回読んでも取り込めませんでした — {why}"),
        );
    }

    /// 取り込めない報告を `rejected/` へ移し、理由を Run の記録へ残す。
    ///
    /// 理由は**ファイルごとに 1 回**だけ出す (隔離に失敗して残ったファイルを
    /// 毎 tick 言い直さない)。
    fn outbox_quarantine(&mut self, run: usize, file: &Path, agent: Option<AgentId>, why: String) {
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let disposal = outbox::quarantine(file);
        let announce = self.outbox_ledger.announce_once(file);
        let where_ = match &disposal {
            outbox::Disposal::Moved(dest) => {
                self.outbox_ledger.forget(file);
                format!("{} へ隔離しました", dest.display())
            }
            outbox::Disposal::Renamed(dest) => {
                self.outbox_ledger.forget(file);
                format!("{} へ名前を変えました (隔離先を作れませんでした)", dest.display())
            }
            // **消さない。** 中身は「何を書いたのか」の唯一の証拠なので、
            // 動かせないときはその場に残す。ただし読み直しは止める —
            // 止めないと毎 tick 同じ却下を積む。
            outbox::Disposal::Kept(why) => {
                if self.outbox_skip.len() >= outbox::LEDGER_MAX {
                    if let Some(old) = self.outbox_skip.iter().next().cloned() {
                        self.outbox_skip.remove(&old);
                    }
                }
                self.outbox_skip.insert(file.to_path_buf());
                format!("そのまま残しました (この起動では読み直しません): {why}")
            }
        };
        if !announce {
            return;
        }
        if let Some(rt) = self.runs.get_mut(run) {
            rt.note_outbox_rejected(
                agent,
                format!("報告ファイル {name} を取り込めません: {why} ({where_})"),
            );
        }
    }

    /// app から観測を受け取って 1 tick 進める。**描画の外で呼ぶこと。**
    pub fn pump_sessions(&mut self, rows: Vec<SessionInput>, now: u64) {
        if self.rt().is_none() || self.read_only {
            return;
        }
        // **置き場を先に読む。** 画面より前に取り込むので、同じ報告が
        // 両方にあっても効くのは置き場のほう (画面側は `take_unseen` が落とす)。
        // 画面はカーソル移動で描く CLI で構造的に取りこぼすので、こちらが正規。
        self.drain_outbox(now);
        let sessions = rows
            .into_iter()
            .map(|r| {
                // **画面をそのまま渡す。**
                //
                // 以前はここで「前回以降に増えた行だけ」に絞っていたが、
                // それでは**報告そのものが分断される**。指示のエコーで
                // `[ZAI-TEAM-RESULT]` や `"blockers": []` は既に「見た行」に
                // なっているので、本物の報告が来ても開始マーカーごと消えて
                // 解析器に届かない (実機で、完了報告を出しているのに却下も
                // 受理も記録されないまま止まっていた)。
                //
                // 重複は**意味の単位 (塊)** で Runtime が落とす
                // (`TeamRuntime::take_unseen`)。走査量は `SCAN_MAX_BYTES` が
                // 抑えるので、毎フレーム全履歴を舐めることにはならない。
                //
                // **画面は互換のための控え。** 正規の経路は置き場
                // ([`Self::drain_outbox`]) で、そちらを先に取り込む。
                let text = r.tail.join("\n");
                super::runtime::SessionObs {
                    id: r.id,
                    title: r.title,
                    provider: r.provider,
                    state: r.state,
                    text,
                }
            })
            .collect();
        self.pump(Observation { now, sessions });
    }

    /// 1 tick 進める。**描画の外で呼ぶこと。**
    pub fn pump(&mut self, obs: Observation) {
        if self.read_only {
            return;
        }
        // **全部の Run を進める。** 画面に出していない Run も走っている。
        // 進めないと、そちらの担当は起動したまま何も配られない。
        let active = self.active;
        let closing = self.pending_close.as_ref().map(|p| p.owner.clone());
        let mut active_snapshot_changed = false;
        let batches: Vec<(RunOwner, Vec<TeamEffect>)> = self
            .runs
            .iter_mut()
            .enumerate()
            .filter(|(_, rt)| closing.as_ref().is_none_or(|owner| rt.owner() != *owner))
            .map(|(index, rt)| {
                let before = rt.snapshot_generation();
                let owner = rt.owner();
                let effects = rt.tick(&obs);
                if index == active && rt.snapshot_generation() != before {
                    active_snapshot_changed = true;
                }
                (owner, effects)
            })
            .collect();
        // 観測だけの tick は Effect を返さない。世代差を見ないと、端末の
        // preview・状態・ReportedSubAgent が変わっても組織図が古いまま固まる。
        // inactive Run は表示していないので、切替時の dirty に任せる。
        if active_snapshot_changed {
            self.dirty = true;
        }
        for (owner, effects) in batches {
            self.absorb_for(owner, effects);
        }
    }

    /// 画面に出している Run の副作用を取り込む。
    fn absorb(&mut self, effects: Vec<TeamEffect>) {
        // **発行した瞬間の持ち主を焼き付ける。** あとから「いまの Runtime」を
        // 見て決めると、切り替わった後の値になってしまう。
        let Some(owner) = self.rt().map(|rt| rt.owner()) else {
            return;
        };
        self.absorb_for(owner, effects);
    }

    /// 持ち主を指定して取り込む (Run が複数あるときはこちら)。
    fn absorb_for(&mut self, owner: RunOwner, effects: Vec<TeamEffect>) {
        for e in effects {
            let key = e.key();
            match e {
                TeamEffect::StartAgent(s) => self.pending_launches.push((owner.clone(), key, s)),
                TeamEffect::SendInstruction {
                    task,
                    session,
                    text,
                    ..
                } => self
                    .pending_instructions
                    .push((owner.clone(), key, (task, session, text))),
                TeamEffect::SendManualInstruction {
                    agent,
                    session,
                    text,
                    ..
                } => self
                    .pending_manual
                    .push((owner.clone(), key, (agent, session, text))),
                TeamEffect::StopAgent(s) => self.pending_stops.push((owner.clone(), key, s)),
                TeamEffect::RunValidation(v) => {
                    self.pending_validations.push((owner.clone(), key, v))
                }
                TeamEffect::RequestHumanApproval(_) => {
                    // 判断は Runtime が保持していて、画面が Mission Panel で出す。
                    // ここで別の入れ物へ写すと第 2 の真実になる。
                    // **画面に出た時点で仕事は済んでいる**ので、そのまま成功を返す。
                    self.ack_done(&owner, &key);
                }
                TeamEffect::CancelValidation {
                    execution, task, ..
                } => {
                    // 相手が居なくても目的は果たされている (走っていない)。
                    let _ = (self.cancel_validation(&execution), task);
                    self.ack_done(&owner, &key);
                }
                TeamEffect::PersistState => self.needs_save = true,
            }
            self.dirty = true;
        }
    }

    /// Effect の実行が成功したと Runtime へ返す。
    /// **その Run の** Effect を成功として返す。
    ///
    /// 冪等キーは Run をまたいで重なりうる — `instr:<task>:<agent>:…` の
    /// タスク番号もエージェント名も Run ごとに 1 から数え直すので、同じ
    /// SPEC を 2 本走らせると**綴りまで一致する**。鍵だけで探す
    pub fn ack_done(&mut self, owner: &RunOwner, key: &str) {
        if let Some(pos) = self.run_pos_of_owner(owner) {
            let rt = &mut self.runs[pos];
            rt.note_effect_done(key);
        }
        self.needs_save = true;
        self.dirty = true;
    }

    /// **その Run の** Effect を失敗として返す。
    pub fn ack_failed(&mut self, owner: &RunOwner, key: &str) {
        if let Some(pos) = self.run_pos_of_owner(owner) {
            let rt = &mut self.runs[pos];
            rt.note_effect_failed(key);
        }
        self.needs_save = true;
        self.dirty = true;
    }

    /// **人が出した指示が、配送に届く前に落ちた**と Runtime へ返す
    /// (コスト上限で止まった / 送信キューへ積めなかった)。
    ///
    /// `ack_failed` だけで済ませると、記録には「送信キューへ追加しました」
    /// が残ったまま結末が 1 件も無い状態になる。**撃ち直さない**のは
    /// `note_delivery` の失敗と同じ理由 (人の発話を自動で再送しない)。
    ///
    /// **出した Run へ返す。** 画面に出している Run へ返すと、2 本目の
    /// チームの指示が落ちたときに 1 本目の台帳へ書かれる (`note_delivery`
    /// と同じ理由)。
    pub fn note_manual_failed(&mut self, owner: &RunOwner, key: &str, why: &str) {
        if let Some(pos) = self.run_pos_of_owner(owner) {
            let rt = &mut self.runs[pos];
            rt.note_manual_delivery(key, false, why);
        }
        self.needs_save = true;
        self.dirty = true;
    }

    /// **指示が方針で止められた**と Runtime へ返す (撃ち直さない)。
    pub fn note_instruction_blocked(&mut self, owner: &RunOwner, task: TaskId, why: &str) {
        if let Some(pos) = self.run_pos_of_owner(owner) {
            let rt = &mut self.runs[pos];
            rt.note_instruction_blocked(task, why);
        }
        self.needs_save = true;
        self.dirty = true;
    }

    /// 画面に出している Run。**無ければ `None`。**
    pub(super) fn rt(&self) -> Option<&TeamRuntime> {
        self.runs.get(self.active)
    }

    /// 指定した Run で使うと決めたエージェント。
    pub fn pinned_agents_for(&self, owner: &RunOwner) -> Vec<String> {
        self.run_pos_of_owner(owner)
            .map(|pos| self.runs[pos].run().agent_presets.clone())
            .unwrap_or_default()
    }

    /// 画面に出している Run (書き換え用)。
    pub(super) fn rt_mut(&mut self) -> Option<&mut TeamRuntime> {
        self.runs.get_mut(self.active)
    }

    /// 走っている Run の一覧 `(表題, 進行中か)`。画面の切り替えに使う。
    pub fn run_tabs(&self) -> Vec<(String, bool)> {
        self.runs
            .iter()
            .map(|r| (r.goal().title.clone(), !r.goal().status.is_terminal()))
            .collect()
    }

    /// 画面に出す Run を選ぶ。**範囲外は無視する** (押せない位置を作らない)。
    pub fn select_run(&mut self, i: usize) {
        if i < self.runs.len() && i != self.active {
            self.active = i;
            self.selected_agent = None;
            self.expanded_output = None;
            self.dirty = true;
        }
    }

    /// いま画面に出している Run の位置。
    pub fn active_run(&self) -> usize {
        self.active
    }

    /// **このワークスペースで Team Run の実測ができるようにする。**
    ///
    /// 実測 (`changeset`) は `git status` の「HEAD と同じか」をそのまま使う
    /// ので、**コミットが 1 つも無いと基準点が成立しない**。だから見るのは
    /// 「Git があるか」ではなく [`gitinit::GitState`] で、`init` 済み・HEAD
    /// 無しは**準備完了にしない**。前の版はここを `.git` の有無だけで見て
    /// いたので、コミットに失敗した後にもう一度押すと「準備完了」と表示し、
    /// 基準点が無いまま Run が走っていた。
    ///
    /// **人が押したときだけ**走らせる (利用者のフォルダを黙って変える操作)。
    /// 利用者の index にも作業ツリーにも触らない — 詳しくは [`gitinit`]。
    pub fn init_git(&mut self) -> Result<(), String> {
        let ws = self.workspace.clone();
        if ws.as_os_str().is_empty() || !ws.is_dir() {
            return Err(crate::i18n::tr("team.git.no_workspace"));
        }
        match gitinit::prepare(&ws) {
            Ok(_) => {
                self.refresh_git_readiness();
                self.dirty = true;
                Ok(())
            }
            Err(why) => {
                // **失敗したら準備完了にしない。** 次に押せば続きから作る。
                self.refresh_git_readiness();
                self.dirty = true;
                self.notice = why.clone();
                Err(why)
            }
        }
    }

    /// **実測に使える基準点があるか**を見直す (`needs_git` の唯一の決め所)。
    ///
    /// `git::discover_toplevel` の有無で決めない — それでは HEAD の無い
    /// リポジトリを「準備完了」と読む。
    fn refresh_git_readiness(&mut self) {
        self.needs_git = !matches!(
            gitinit::plan_for(&gitinit::probe(&self.workspace)),
            gitinit::GitPlan::Ready
        );
    }

    /// Run を閉じる要求。dirtyなら確認を出すだけで、停止も削除もしない。
    pub fn close_run(&mut self, i: usize) -> Option<String> {
        if i >= self.runs.len() {
            return None;
        }
        if self.pending_close.is_some()
            || matches!(self.close_prompt, ClosePrompt::Stopping { .. })
        {
            self.notice = crate::i18n::tr("team.close.already_running");
            return None;
        }
        let id = self.runs[i].run().run_id.clone();
        let artifact_path = self.runs[i].workspace().display().to_string();
        if let Some(saved) = self.runs[i].run().run_workspace.as_ref() {
            match super::run_workspace::change_state(
                &self.home,
                &self.workspace,
                &id,
                saved,
            ) {
                Ok(super::run_workspace::ChangeState::Dirty) => {
                    self.close_prompt = ClosePrompt::Confirm {
                        run_id: id.clone(),
                        artifact_path,
                    };
                    return Some(id);
                }
                Ok(super::run_workspace::ChangeState::Clean) => {}
                Err(why) => {
                    self.close_prompt = ClosePrompt::Failed {
                        run_id: id.clone(),
                        artifact_path,
                        policy: persistence::ClosePolicy::CleanOnly,
                        why: why.clone(),
                    };
                    self.notice = why;
                    return Some(id);
                }
            }
        }
        self.begin_close(&id, persistence::ClosePolicy::CleanOnly);
        Some(id)
    }

    /// dirty確認で選んだRunの成果物を残し、管理だけを閉じる。
    pub fn close_run_keep(&mut self) {
        if let ClosePrompt::Confirm { run_id, .. } = self.close_prompt.clone() {
            self.begin_close(&run_id, persistence::ClosePolicy::Keep);
        }
    }

    /// dirty確認で選んだRunだけを、明示承認として強制削除する。
    pub fn close_run_discard(&mut self) {
        if let ClosePrompt::Confirm { run_id, .. } = self.close_prompt.clone() {
            self.begin_close(&run_id, persistence::ClosePolicy::Discard);
        }
    }

    /// 確認の取消。確認前は何も変更していないので、表示を消すだけでよい。
    pub fn close_run_cancel(&mut self) {
        if matches!(self.close_prompt, ClosePrompt::Confirm { .. }) {
            self.close_prompt = ClosePrompt::None;
        }
    }

    /// 失敗したCloseを同じ方針で再試行する。成果物破棄の承認は保持する。
    pub fn retry_close_run(&mut self) {
        let ClosePrompt::Failed { run_id, policy, .. } = self.close_prompt.clone() else {
            return;
        };
        if let Some(pending) = self.pending_close.as_mut() {
            if pending.owner.run_id == run_id {
                pending.started = Instant::now();
                self.close_prompt = ClosePrompt::Stopping {
                    run_id,
                    artifact_path: pending.artifact_path.clone(),
                };
                self.progress_close();
                return;
            }
        }
        self.begin_close(&run_id, policy);
    }

    fn begin_close(&mut self, id: &str, policy: persistence::ClosePolicy) {
        let Some(pos) = self.runs.iter().position(|r| r.run().run_id == id) else {
            return;
        };
        let owner = self.runs[pos].owner();
        let artifact_path = self.runs[pos].workspace().display().to_string();

        // 停止後に新しい仕事を起動・送信しない。同じownerだけを対象にし、
        // 別Runのキューはそのまま継続する。
        let queued_before = self.pending_launches.len()
            + self.pending_instructions.len()
            + self.pending_manual.len()
            + self.pending_validations.len();
        self.pending_launches.retain(|(o, _, _)| o != &owner);
        self.pending_instructions.retain(|(o, _, _)| o != &owner);
        self.pending_manual.retain(|(o, _, _)| o != &owner);
        self.pending_validations.retain(|(o, _, _)| o != &owner);
        let queued_after = self.pending_launches.len()
            + self.pending_instructions.len()
            + self.pending_manual.len()
            + self.pending_validations.len();
        self.dropped_effects = self
            .dropped_effects
            .saturating_add(queued_before.saturating_sub(queued_after));
        let effects = self.runs[pos].close();
        self.cancel_validations_for_owner(&owner);
        self.absorb_for(owner.clone(), effects);
        self.needs_save = true;

        // 停止済みRuntimeを先に保存する。保存できない状態で墓標や成果物だけを
        // 変更すると、再試行に必要な対応情報を失う。
        if let Err(why) = self.save_all_state() {
            self.close_prompt = ClosePrompt::Failed {
                run_id: id.to_string(),
                artifact_path,
                policy,
                why: why.clone(),
            };
            self.notice = why;
            return;
        }
        let state = self.state_dir();
        if let Err(e) = persistence::mark_close_state(
            &state,
            id,
            policy,
            persistence::ClosePhase::Stopping,
            &artifact_path,
        ) {
            let why = e.detail();
            self.close_prompt = ClosePrompt::Failed {
                run_id: id.to_string(),
                artifact_path,
                policy,
                why: why.clone(),
            };
            self.notice = why;
            return;
        }
        self.pending_close = Some(PendingClose {
            owner,
            policy,
            artifact_path: artifact_path.clone(),
            started: Instant::now(),
        });
        self.close_prompt = ClosePrompt::Stopping {
            run_id: id.to_string(),
            artifact_path,
        };
        self.progress_close();
    }

    /// 実行側がセッション終了の追跡札を返す入口。
    pub fn watch_stop(
        &mut self,
        owner: RunOwner,
        key: String,
        handle: Option<crate::terminal::ReapHandle>,
    ) {
        if let Some(handle) = handle {
            self.stop_jobs.push(StopJob { owner, key, handle });
        } else {
            self.ack_done(&owner, &key);
        }
    }

    /// セッションとvalidationの終了を確認してからだけ後始末へ進む。
    pub fn progress_close(&mut self) {
        let mut waiting = Vec::new();
        let mut completed = Vec::new();
        for job in std::mem::take(&mut self.stop_jobs) {
            if job.handle.is_finished() {
                completed.push((job.owner, job.key));
            } else {
                waiting.push(job);
            }
        }
        self.stop_jobs = waiting;
        for (owner, key) in completed {
            self.ack_done(&owner, &key);
        }

        // cleanup/停止失敗後は、利用者がRetryを選ぶまで勝手に再実行しない。
        // 特に明示破棄の方針を保持したまま毎tick進めると、診断を読む前に
        // 成果物が消える可能性がある。
        if matches!(self.close_prompt, ClosePrompt::Failed { .. }) {
            return;
        }

        let Some(pending) = self.pending_close.clone() else {
            return;
        };
        let owner = &pending.owner;
        let busy = self.pending_stops.iter().any(|(o, _, _)| o == owner)
            || self.stop_jobs.iter().any(|j| &j.owner == owner)
            || self.validation_jobs.iter().any(|j| &j.owner == owner)
            || self.pending_validations.iter().any(|(o, _, _)| o == owner);
        if busy {
            if pending.started.elapsed() > Duration::from_secs(30) {
                let why = crate::i18n::tr("team.close.stop_timeout");
                self.close_prompt = ClosePrompt::Failed {
                    run_id: owner.run_id.clone(),
                    artifact_path: pending.artifact_path,
                    policy: pending.policy,
                    why: why.clone(),
                };
                self.notice = why;
            }
            return;
        }
        self.finish_close(pending);
    }

    fn finish_close(&mut self, pending: PendingClose) {
        let id = pending.owner.run_id.as_str();
        let Some(pos) = self.run_pos_of_owner(&pending.owner) else {
            self.pending_close = None;
            return;
        };
        let saved = self.runs[pos].run().run_workspace.clone();

        // cleanで開始した場合も削除直前に再実測する。ここで増えた成果物は
        // 新しい確認なしには消さない。
        if pending.policy == persistence::ClosePolicy::CleanOnly {
            if let Some(worktree) = saved.as_ref() {
                match super::run_workspace::change_state(
                    &self.home,
                    &self.workspace,
                    id,
                    worktree,
                ) {
                    Ok(super::run_workspace::ChangeState::Dirty) => {
                        if let Err(e) = persistence::mark_close_state(
                            &self.state_dir(),
                            id,
                            persistence::ClosePolicy::CleanOnly,
                            persistence::ClosePhase::Stopping,
                            &pending.artifact_path,
                        ) {
                            self.close_failed(&pending, e.detail());
                            return;
                        }
                        self.pending_close = None;
                        self.close_prompt = ClosePrompt::Confirm {
                            run_id: id.to_string(),
                            artifact_path: pending.artifact_path,
                        };
                        return;
                    }
                    Ok(super::run_workspace::ChangeState::Clean) => {}
                    Err(why) => {
                        self.close_failed(&pending, why);
                        return;
                    }
                }
            }
        }

        if let Err(e) = persistence::mark_close_state(
            &self.state_dir(),
            id,
            pending.policy,
            persistence::ClosePhase::Cleanup,
            &pending.artifact_path,
        ) {
            self.close_failed(&pending, e.detail());
            return;
        }

        if pending.policy != persistence::ClosePolicy::Keep {
            if let Err(why) = self.cleanup_closed_run(id, saved.as_ref(), pending.policy) {
                self.close_failed(&pending, why);
                return;
            }
        }
        self.remove_run_from_memory(&pending.owner);
        self.pending_close = None;
        if pending.policy == persistence::ClosePolicy::Keep {
            self.notice = crate::i18n::trf(
                "team.close.kept_notice",
                &[("path", pending.artifact_path.clone())],
            );
        } else {
            if let Err(e) = persistence::unmark_closed(&self.state_dir(), id) {
                self.notice = e.detail();
            } else {
                self.notice = crate::i18n::tr("team.notice.run_closed");
            }
        }
        self.close_prompt = ClosePrompt::None;
        self.needs_save = true;
        self.save_if_needed();
    }

    fn close_failed(&mut self, pending: &PendingClose, why: String) {
        self.close_prompt = ClosePrompt::Failed {
            run_id: pending.owner.run_id.clone(),
            artifact_path: pending.artifact_path.clone(),
            policy: pending.policy,
            why: why.clone(),
        };
        self.notice = why;
    }

    fn remove_run_from_memory(&mut self, owner: &RunOwner) {
        let Some(i) = self.run_pos_of_owner(owner) else {
            return;
        };
        self.runs.remove(i);
        if self.runs.is_empty() {
            self.active = 0;
            self.snapshot = None;
        } else if i < self.active {
            self.active -= 1;
        } else if self.active >= self.runs.len() {
            self.active = self.runs.len() - 1;
        }
        self.selected_agent = None;
        self.expanded_output = None;
        self.dirty = true;
    }

    /// 墓標がCleanupで、方針が確定したRunだけを片付ける。
    fn cleanup_closed_run(
        &mut self,
        id: &str,
        run_workspace: Option<&super::run_workspace::RunWorkspace>,
        policy: persistence::ClosePolicy,
    ) -> Result<(), String> {
        let state = self.state_dir();
        let discovered;
        let worktree = match run_workspace {
            Some(w) => Some(w),
            None => match super::run_workspace::discover(&self.home, &self.workspace, id) {
                Ok(found) => {
                    discovered = found;
                    discovered.as_ref()
                }
                Err(e) => {
                    return Err(format!(
                        "Run {id} の専用 worktree を安全に確認できません: {e}"
                    ));
                }
            },
        };
        if let Some(worktree) = worktree {
            let result = match policy {
                persistence::ClosePolicy::Discard => super::run_workspace::remove_discarded(
                    &self.home,
                    &self.workspace,
                    id,
                    worktree,
                ),
                persistence::ClosePolicy::CleanOnly => super::run_workspace::remove_clean(
                    &self.home,
                    &self.workspace,
                    id,
                    worktree,
                ),
                persistence::ClosePolicy::Keep => return Ok(()),
            };
            result.map_err(|e| format!("Run {id} の専用 worktree を削除できません: {e}"))?;
        }

        let mut errors = Vec::new();
        let mut all_gone = true;
        if let Some(dir) = persistence::run_dir_in(&state, id) {
            if let Err(e) = persistence::remove_dir_checked(&dir) {
                all_gone = false;
                errors.push(e);
            }
        }
        if let Some(dir) = outbox::run_dir(&state, id) {
            if let Err(e) = persistence::remove_dir_checked(&dir) {
                all_gone = false;
                errors.push(e);
            }
        }
        // **根の控えが閉じた Run のものなら消す。** 控えは「いちばん古い 1 本」の
        // 写しなので、その Run を閉じたのに残すと復元経路が拾い直す
        // (墓標が断るが、案内だけ出て何も起きない状態になる)。
        if persistence::root_run_id(&state).as_deref() == Some(id) {
            if let Err(e) = persistence::reset(&state) {
                all_gone = false;
                errors.push(e.detail());
            }
        }
        if !errors.is_empty() {
            return Err(crate::i18n::trf(
                "team.notice.run_state_cleanup_failed",
                &[("run", id.to_string()), ("e", errors.join(" / "))],
            ));
        }
        self.outbox_ledger.prune_missing();
        debug_assert!(all_gone);
        Ok(())
    }

    /// **墓標のある Run の片付けを試し直し、済んだ墓標を掃く** (復元の前に呼ぶ)。
    ///
    /// 閉じたときに消せなかった保存・置き場は、ここでもう一度消す。まだ
    /// 消せなければ墓標を残す (復元はしない)。何も残っていなければ墓標だけが
    /// 残っているので、それを掃く。失敗しても復元は続ける — 掃除の失敗で
    /// 生きている Run の復元を止めない。
    fn sweep_closed_runs(&mut self, root: &Path) {
        debug_assert_eq!(root, self.state_dir());
        for id in persistence::closed_run_ids(root) {
            let Some(record) = persistence::close_record(root, &id) else {
                self.notice = format!("Run {id} のClose記録が壊れているため自動削除しません");
                continue;
            };
            if record.phase != persistence::ClosePhase::Cleanup
                || record.policy == persistence::ClosePolicy::Keep
            {
                continue;
            }
            if self.cleanup_closed_run(&id, None, record.policy).is_ok() {
                let _ = persistence::unmark_closed(root, &id);
            }
        }
    }

    /// いまの Run の持ち主。Run が無ければ `None`。
    #[cfg(test)]
    pub fn owner(&self) -> Option<RunOwner> {
        self.rt().map(|rt| rt.owner())
    }

    /// **いまの Run のものだけを渡す。** 持ち主が違うものは捨てる
    /// (別の workspace / 別の Run で実行させない)。捨てた件数を数える。
    fn mine<T>(&mut self, q: Vec<(RunOwner, String, T)>) -> Vec<(RunOwner, String, T)> {
        // **走っている Run のどれかのものなら実行する。**
        // 「いまの Run」だけで見ると、画面に出していない Run の仕事が
        // 全部捨てられる (2 本目のチームが 1 つも動かない)。
        let live: Vec<RunOwner> = self.runs.iter().map(|r| r.owner()).collect();
        let mut out = Vec::with_capacity(q.len());
        let mut dropped = 0usize;
        for (owner, key, v) in q {
            if live.contains(&owner) {
                // **ここで owner を落とさない。** key は Run ごとに採番し直すので、
                // 同じ SPEC の並列 Run では綴りが一致する。
                out.push((owner, key, v));
            } else {
                dropped += 1;
            }
        }
        if dropped > 0 {
            // **黙って捨てない。** 画面には「前の Run の仕事を実行しなかった」
            // ことが出る (何も起きないまま消えると、利用者は理由を追えない)。
            self.dropped_effects = self.dropped_effects.saturating_add(dropped);
            self.notice =
                crate::i18n::trf("team.notice.dropped_effects", &[("n", dropped.to_string())]);
        }
        out
    }

    /// 別の Run のものとして捨てた Effect の数。
    ///
    /// 画面へは `notice` で出る (上の `mine`)。この数を読むのはテストだけ。
    #[cfg(test)]
    pub fn dropped_effects(&self) -> usize {
        self.dropped_effects
    }

    /// 起動してほしいエージェント (冪等キー付き。取り出したら消える)。
    pub fn take_launches(&mut self) -> Vec<(RunOwner, String, super::runtime::AgentLaunchSpec)> {
        let q = std::mem::take(&mut self.pending_launches);
        self.mine(q)
    }
    /// 送ってほしい指示 (冪等キー付き。取り出したら消える)。
    ///
    /// **宛先のタスクも一緒に渡す。** 実行側がセッションから引き直すと、
    /// 間に 1 tick 入っただけで別のタスクを指す。
    pub fn take_instructions(&mut self) -> Vec<(RunOwner, String, TaskId, SessionId, String)> {
        let q = std::mem::take(&mut self.pending_instructions);
        self.mine(q)
            .into_iter()
            .map(|(owner, k, (task, s, t))| (owner, k, task, s, t))
            .collect()
    }
    /// **人が出した指示** (冪等キー付き。取り出したら消える)。
    ///
    /// 宛先はタスクではなく**エージェント**。タスクを持たない相手
    /// (Team Lead など) へも送れる。
    pub fn take_manual_instructions(
        &mut self,
    ) -> Vec<(RunOwner, String, AgentId, SessionId, String)> {
        let q = std::mem::take(&mut self.pending_manual);
        self.mine(q)
            .into_iter()
            .map(|(owner, k, (a, s, t))| (owner, k, a, s, t))
            .collect()
    }

    /// 止めてほしいセッション (冪等キー付き。取り出したら消える)。
    ///
    /// **停止だけは生存 Run で選り分けない** ([`Self::mine`] を通さない)。
    /// 閉じた Run の担当を止める命令は、Run を記憶から外した**後**に実行側が
    /// 取り出す ([`Self::close_run`] の順序: 止める → 外す → 取り出す)。
    /// ここで `mine` を通すと、閉じた Run の停止は「死んだ Run の仕事」として
    /// 捨てられ、担当のプロセスだけが残る (実際に残っていた)。
    ///
    /// 通してよい理由 — 停止は具体的な `SessionId` を名指しする:
    /// * 結び付けは Run ごとなので、別の Run のセッションが載ることは無い
    /// * 相手が既に居なければ実行側は何もしない (二重実行は無害)
    /// * 消えた Run への ACK は [`Self::ack_done`] が黙って落とす
    /// * workspace を切り替える経路と終了は、取り出す前に列を空にする
    ///   ([`Self::attach_workspace`] / [`Self::shutdown`])
    pub fn take_stops(&mut self) -> Vec<(RunOwner, String, SessionId)> {
        std::mem::take(&mut self.pending_stops)
    }

    /// 走らせてほしい検証 (冪等キー付き。取り出したら消える)。
    pub fn take_validations(&mut self) -> Vec<(RunOwner, String, super::runtime::ValidationSpec)> {
        let q = std::mem::take(&mut self.pending_validations);
        self.mine(q)
    }

    /// 起動したセッションを結び付ける。
    ///
    /// `identity` は**再起動をまたぐ目印** (実行側が決める安定した文字列)。
    /// 覚えておかないと、次の起動で同じ logical agent を 2 体起こす。
    pub fn bind_session(
        &mut self,
        owner: &RunOwner,
        agent: &AgentId,
        session: SessionId,
        identity: Option<String>,
    ) {
        // **取り出した起動要求の owner へ結び付ける。** `agent-1` も
        // `start:agent-1` も Run ごとに同じなので、名前や key から引き直さない。
        if let Some(pos) = self.run_pos_of_owner(owner) {
            let rt = &mut self.runs[pos];
            rt.bind_session(agent, session, identity);
        }
        self.dirty = true;
        self.needs_save = true;
    }

    /// **指示が実際に届いたか**を実行側から受け取る (`submit` の終わり方)。
    ///
    /// 届いたなら冪等キーを完了にする (もう出し直さない)。届かなかったなら
    /// 完了にせず、担当を解いて配り直せる形へ戻す。**積めた時点で完了に
    /// してしまうと、相手が消えた場合に指示が消えたまま Runtime だけが
    /// 「送った」と信じ続ける。**
    ///
    /// 戻りは「届かなかったタスク」(画面へ理由を出すため)。
    pub fn note_delivery(&mut self, tag: &str, delivered: bool) -> Option<TaskId> {
        let (run, key) = tag.split_once('|')?;
        // **出した Run へ返す。「いま画面に出している Run」ではない。**
        //
        // 実行するのは [`TeamPanel::mine`] = **走っている全 Run** の仕事なので、
        // 結末も同じ範囲で受け取れなければ辻褄が合わない。ここを `owner()`
        // (画面に出している 1 本) で照合していたので、Run が 2 本あるときと、
        // 配達中に人がタブを切り替えたときに**結末が丸ごと消えて**いた
        // (台帳に 1 行も残らず、担当は `running` のまま放置される)。
        //
        // 死んだ Run の配達はどの Run にも一致しないので、従来どおり効かない
        // (前の Run の結末が、同じ番号の別のタスクを完了にしてしまわない)。
        let pos = self.run_pos_of(run)?;
        // **人が出した指示は、タスクの機構へ流さない。** 宛先タスクが無い
        // ことがあるし、あっても「いま待っている指示」ではないので
        // `note_instruction_undelivered` の照合が必ず外れ、届かなかった
        // 事実が「古い配達」として毎回捨てられる。
        if key.starts_with("manual:") {
            // **結末を監査へ残す。** 発行時の記録は「送信キューへ追加した」
            // までなので、ここを素の ack で済ませると delivered と failed が
            // 記録の上で見分けられない。
            self.runs[pos].note_manual_delivery(key, delivered, "宛先の端末が応答しませんでした");
            if !delivered {
                self.notice = crate::i18n::tr("team.err.manual_undelivered");
            }
            self.needs_save = true;
            self.dirty = true;
            return None;
        }
        let task = self.instruction_task_of(key)?;
        if delivered {
            let owner = self.runs[pos].owner();
            self.ack_done(&owner, key);
            return None;
        }
        let owner = self.runs[pos].owner();
        self.ack_failed(&owner, key);
        self.runs[pos].note_instruction_undelivered(task, key, "宛先の端末が応答しませんでした");
        self.needs_save = true;
        self.dirty = true;
        Some(task)
    }

    /// **目印の run_id から Run の位置を引く。**
    ///
    /// 生きている Run だけが一致する (閉じた Run の配達は何も起こさない)。
    fn run_pos_of(&self, run_id: &str) -> Option<usize> {
        self.runs.iter().position(|r| r.run().run_id == run_id)
    }

    /// 構造化された持ち主から Run の位置を引く。
    fn run_pos_of_owner(&self, owner: &RunOwner) -> Option<usize> {
        self.runs.iter().position(|r| r.owner() == *owner)
    }

    /// 配達の結末を受け取るための目印 (`<run_id>|<冪等キー>`)。
    ///
    /// Run を添えるのは、積んだ仕事が Run の切り替えでは消えないから
    /// (前の Run の配達が、同じ番号の別のタスクを完了にしてしまう)。
    /// Run が無ければ `None` = そもそも配達の結末を受け取らない。
    ///
    /// **添えるのは「その鍵を出した Run」であって、画面に出している Run
    /// ではない。** 取り出し口 ([`TeamPanel::mine`]) は走っている全 Run の
    /// 仕事を渡すので、`owner()` で目印を作ると **2 本目の Run の配達に
    /// 1 本目の名札が付く**。名札が違えば結末は捨てられ、担当は `running`
    /// のまま残る (実機で 6 体中 2 体が 28 分放置された形)。
    pub fn delivery_tag(&self, owner: &RunOwner, key: &str) -> Option<String> {
        self.run_pos_of_owner(owner)?;
        Some(format!("{}|{key}", owner.run_id))
    }

    /// 冪等キーからタスク番号を読む (`instr:<task>:<agent>:<attempt>:<seq>`)。
    ///
    /// **鍵の綴りを 2 か所で組み立てない。** 発行側は
    /// [`super::runtime`] の 1 か所だけで、ここは読むだけ。読めない綴り
    /// (Team のものではない目印) は `None` になり、何も起きない。
    fn instruction_task_of(&self, key: &str) -> Option<TaskId> {
        let rest = key.strip_prefix("instr:")?;
        let (task, _) = rest.split_once(':')?;
        task.parse().ok()
    }

    /// いま担当へ結び付いているセッション。
    ///
    /// 実行側が「このセッションはもう別の担当のものか」を見るために使う
    /// (**第 2 のセッション台帳を作らない** — 真実は Runtime の
    /// エージェント一覧 1 か所)。
    pub fn bound_sessions(&self) -> HashSet<SessionId> {
        self.runs
            .iter()
            .flat_map(|rt| rt.agents().iter().filter_map(|a| a.session_id))
            .collect()
    }

    /// 起動に失敗した。
    pub fn note_launch_failed(&mut self, owner: &RunOwner, agent: &AgentId, why: &str) {
        if let Some(pos) = self.run_pos_of_owner(owner) {
            let rt = &mut self.runs[pos];
            rt.note_launch_failed(agent, why);
        }
        self.dirty = true;
    }

    /// 検証結果を戻す。
    /// **実行 ID を添えて**実測を戻す (古い実行の結果を採らないため)。
    pub fn note_validation_for(
        &mut self,
        owner: &RunOwner,
        execution: &str,
        task: TaskId,
        runs: Vec<ValidationRun>,
    ) {
        if let Some(pos) = self.run_pos_of_owner(owner) {
            let rt = &mut self.runs[pos];
            rt.note_validation_for(execution, task, runs);
        }
        self.needs_save = true;
        self.dirty = true;
    }

    /// 裏で走らせた検証を預かる。
    pub fn watch_validation(&mut self, job: ValidationJob) {
        self.validation_jobs.push(job);
    }

    /// 走っている検証の数 (UI とテストが見る)。
    pub fn running_validations(&self) -> usize {
        self.validation_jobs.len()
    }

    /// 指定した実行を止めるよう札を立てる。**プロセスは実行器が落とす。**
    ///
    /// 戻り値は「止める相手が居たか」。世代がずれていれば既に別の実行なので
    /// 何もしない (古い停止要求で新しい検証を殺さない)。
    pub fn cancel_validation(&mut self, execution: &str) -> bool {
        let mut hit = false;
        for j in &self.validation_jobs {
            if j.execution == execution {
                j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                hit = true;
            }
        }
        hit
    }

    /// 走っている検証を全部止める (Run を閉じる / 破棄するとき)。戻りは件数。
    ///
    /// **札を立てるだけ。** worker が次の刻みで木を落として `Cancelled` を
    /// 戻すので、結果を受け取る口はそのまま残す (捨てると Runtime が
    /// 永久に待つ)。アプリを閉じるときだけは待てないので
    /// [`Self::stop_all_validations_now`] を使う。
    pub fn cancel_all_validations(&mut self) -> usize {
        for j in &self.validation_jobs {
            j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.running_validations()
    }

    /// 1 Run に属する検証だけを即時停止する。受け口は終了確認まで残す。
    fn cancel_validations_for_owner(&mut self, owner: &RunOwner) -> usize {
        let mut stopped = 0usize;
        for job in &self.validation_jobs {
            if &job.owner != owner {
                continue;
            }
            job.cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let pid = job.pid.load(std::sync::atomic::Ordering::Relaxed);
            if pid != 0 {
                crate::procx::kill_tree(pid);
            }
            stopped += 1;
        }
        stopped
    }

    /// **アプリを閉じるときの後始末。**
    ///
    /// この状態は `thread_local!` に居るので、`ZaivernApp` より長生きする。
    /// 生き残った Runtime を次のアプリが拾うと、**もう居ないセッションへ
    /// 結び付いたまま**の状態を新しい画面が見ることになる。保存してから
    /// 手放し、次回は保存経路 (`restore`) から入り直す — そこで結び付きは
    /// 必ず外れる。走っている検証はその場で落とす (札だけでは死なない)。
    ///
    /// **報告置き場 (`outbox/<run_id>/`) は消さない。** Run は保存して次回
    /// 復元するので、まだ読んでいない報告を消すと復元後に届かない。
    /// 置き場を消すのは Run を閉じる ([`Self::close_run`]) か捨てる
    /// ([`Self::discard_run`]) ときだけ。
    pub fn shutdown(&mut self) -> usize {
        let killed = self.stop_all_validations_now();
        self.pending_launches.clear();
        self.pending_instructions.clear();
        self.pending_manual.clear();
        self.pending_stops.clear();
        self.pending_validations.clear();
        self.save_if_needed();
        self.runs.clear();
        self.active = 0;
        self.snapshot = None;
        self.restore = RestorePrompt::None;
        killed
    }

    /// **新しいアプリ文脈がこのスレッドに現れた**ときの引き継ぎ拒否。
    ///
    /// 状態は `thread_local!` に居るので `ZaivernApp` より長生きする。
    /// [`Self::shutdown`] は終わる側が呼ぶが、**呼ばれないまま次のアプリが
    /// 立つ経路がある** (前のアプリが落ちた / 同じスレッドで 2 つ目が
    /// 立った)。そのとき生き残った Runtime をそのまま拾うと、新しい画面が
    /// **もう自分のものではないセッションへ結び付いた Run** を操作できて
    /// しまう (起動済みエージェントも走っている検証も、前のアプリのもの)。
    ///
    /// なので**暗黙の引き継ぎを構造で断つ**: 閉じるときと同じ後始末をして
    /// 手放し、`workspace` も空へ戻す。新しいアプリは必ず
    /// `attach_workspace` → 保存経路 (`restore`) から入り直すので、
    /// 復元した Run はそのアプリのセッションへ結び直される。
    ///
    /// 戻りは「前のアプリの状態が残っていたか」。**普通の起動では false**
    /// (何も無いところから始まるので、1 命令も走らない)。
    pub fn adopt_new_app_context(&mut self) -> bool {
        if self.rt().is_none()
            && self.workspace.as_os_str().is_empty()
            && self.validation_jobs.is_empty()
        {
            return false;
        }
        // 走っている検証は落とす。**渡さないなら面倒を見る相手が居ない**
        // ので、札を立てるだけでは孤児のプロセスが残る。
        self.shutdown();
        // **`workspace` も空へ戻す。** 残すと、同じフォルダを開いた次の
        // アプリの `attach_workspace` が「同じだから何もしない」で早々に
        // 返り、保存済み Run の案内 (`RestorePrompt`) すら出ない。
        self.workspace = PathBuf::new();
        self.read_only = false;
        self.notice.clear();
        self.dropped_effects = 0;
        self.next_scan = None;
        self.next_launch_poll = None;
        self.dirty = true;
        true
    }

    /// **その場でプロセスツリーごと落とす** (アプリの終了時)。
    ///
    /// 札を立てて worker に任せる余裕が無いときに使う。戻りは落とした件数。
    /// 既存の [`crate::procx::kill_tree`] を使う — Team 専用の第 2 の
    /// プロセス管理は作らない。
    pub fn stop_all_validations_now(&mut self) -> usize {
        let mut killed = 0usize;
        for j in std::mem::take(&mut self.validation_jobs) {
            j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            let pid = j.pid.load(std::sync::atomic::Ordering::Relaxed);
            if pid != 0 {
                crate::procx::kill_tree(pid);
                killed += 1;
            }
        }
        killed
    }

    /// 終わった検証を取り込む。**待たない** (`try_recv`)。
    ///
    /// **送り手が消えた場合を握り潰さない。** worker が panic すると結果は
    /// 永久に来ないので、受け口を捨てるだけでは `validation.running` が
    /// true のまま残り、そのタスクは二度と進まない。
    pub fn collect_validations(&mut self) {
        if self.validation_jobs.is_empty() {
            return;
        }
        let now = super::model::now_secs();
        let mut still = Vec::new();
        let live: Vec<RunOwner> = self.runs.iter().map(TeamRuntime::owner).collect();
        let mut done: Vec<(RunOwner, String, TaskId, Vec<ValidationRun>)> = Vec::new();
        for job in std::mem::take(&mut self.validation_jobs) {
            // **最後の砦。** 実行器が自分で打ち切れず、切断も起きない
            // (worker がブロックしたまま) 経路をここで決着させる。
            // 猶予は実行器の時限 + コマンド数ぶん + 余白。
            let limit = job
                .timeout_secs
                .saturating_mul(job.commands.len().max(1) as u64)
                .saturating_add(WATCHDOG_SLACK_SECS);
            if now.saturating_sub(job.started_at) > limit {
                let runs = job
                    .commands
                    .iter()
                    .map(|c| ValidationRun::new(c, 124, super::model::ValidationOutcome::TimedOut))
                    .collect();
                job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                done.push((job.owner.clone(), job.execution.clone(), job.task, runs));
                continue;
            }
            // **閉じられた Run のものだけを止める。** 画面に出していないだけの
            // 生きた Run は、そのまま走らせて持ち主へ結果を返す。
            if !live.contains(&job.owner) {
                job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                self.dropped_effects = self.dropped_effects.saturating_add(1);
                continue;
            }
            match job.rx.try_recv() {
                Ok((execution, task, runs)) => {
                    done.push((job.owner.clone(), execution, task, runs))
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => still.push(job),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // 結果は来ない。**失敗として戻す** — 待ち続けない。
                    let runs = job
                        .commands
                        .iter()
                        .map(|c| {
                            ValidationRun::new(
                                c,
                                125,
                                super::model::ValidationOutcome::RunnerDisconnected,
                            )
                        })
                        .collect();
                    done.push((job.owner.clone(), job.execution.clone(), job.task, runs));
                }
            }
        }
        self.validation_jobs = still;
        for (owner, execution, task, runs) in done {
            if let Some(pos) = self.run_pos_of_owner(&owner) {
                let rt = &mut self.runs[pos];
                rt.note_validation_for(&execution, task, runs);
            }
            self.needs_save = true;
            self.dirty = true;
        }
    }

    /// 読み取り専用で開いているか。
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// 保存が要るなら保存する。**要らないときは 1 バイトも書かない。**
    pub fn save_if_needed(&mut self) {
        if !self.needs_save || self.read_only {
            return;
        }
        if self.runs.is_empty() {
            self.needs_save = false;
            return;
        }
        match self.save_all_state() {
            Ok(()) => self.needs_save = false,
            Err(e) => {
                // 次のtickで必ず再試行する。失敗時に先にfalseへ落とすと、
                // 永続化されていない状態を成功扱いして以後保存しなくなる。
                self.needs_save = true;
                self.notice = e;
            }
        }
    }

    fn save_all_state(&self) -> Result<(), String> {
        // **全部の Run を保存する。** 画面に出していない Run も走っている
        // ので、保存しないと再起動でそれだけが消える。
        //
        // 置き場は Run ごと (`runs/<run_id>/`)。1 か所に上書きすると、
        // 2 本目が 1 本目を消す。
        let root = self.state_dir();
        for rt in &self.runs {
            // **`run_id` をそのまま `join` しない。** 復元した保存の `run_id` は
            // 利用者のファイルから来るので、`..` や区切り文字が入っていれば
            // `runs/` の外へ書くことになる。安全でない名前の Run は保存しない
            // (`restore_run` は読まないので、ここへ来るのは計画の入口から
            // 変な ID を渡されたときだけ)。
            let Some(dir) = persistence::run_dir_in(&root, &rt.run().run_id) else {
                return Err(format!(
                    "Team Run {:?} は保存できません (保存の名前にできない ID)",
                    rt.run().run_id
                ));
            };
            persistence::save(&dir, &rt.to_saved()).map_err(|e| e.detail())?;
        }
        // Runごとの正本が全部成功した後だけ互換ミラーを書く。途中失敗時に
        // 古い正本へ新しいミラーを被せない。
        if let Some(rt) = self.runs.first() {
            persistence::save(&root, &rt.to_saved()).map_err(|e| e.detail())?;
        }
        Ok(())
    }

    /// スナップショットを作り直す (**変わったときだけ**)。
    pub fn refresh_snapshot(&mut self, now: u64) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.snapshot = self.rt().map(|rt| view_model::snapshot(rt, now));
    }

    /// 次のフレームでスナップショットを作り直す。
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }
}

thread_local! {
    /// UI スレッドが持つ Team の状態。
    ///
    /// **プロセス共通の `static` にしない。** テストは自分の
    /// [`TeamRuntime`] を作るので、ここへ触るのは GUI だけになる
    /// (共通にすると同時に走る他のテストの操作が混ざる)。
    static PANEL: RefCell<TeamPanel> = RefCell::new(TeamPanel::default());
}

/// UI スレッドの Team 状態へ触る。
pub fn with_panel<R>(f: impl FnOnce(&mut TeamPanel) -> R) -> R {
    PANEL.with(|p| f(&mut p.borrow_mut()))
}

/// **アプリが立ち上がったことを Team 状態へ知らせる** (`ZaivernApp::new`)。
///
/// この状態はアプリより長生きするので、前のアプリの Run が残っていることが
/// ありうる。[`TeamPanel::adopt_new_app_context`] が暗黙の引き継ぎを断つ。
/// 戻りは「残っていたものを手放したか」。
pub fn begin_app_context() -> bool {
    with_panel(|p| p.adopt_new_app_context())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str) -> PathBuf {
        crate::test_util::unique_temp_dir("zaivern-team-panel", name)
    }

    /// **実 `~/.zaivern` に 1 バイトも触らない**パネルを作る。
    ///
    /// 置き場の根を一時ディレクトリへ向ける。`ZAIVERN_HOME` を差し替える手も
    /// あるが、環境変数は並列に走る他のテストへ漏れるので採らない。
    fn panel_at(dir: &Path) -> TeamPanel {
        std::fs::create_dir_all(dir).expect("テスト workspace を作れる");
        gitinit::prepare(dir).expect("製品の Start 経路に必要な HEAD 付き git repo");
        let mut p = TeamPanel::default();
        p.home = dir.join(".zaivern-test-home");
        p.attach_workspace(dir)
            .expect("新しい画面は必ず attach できる");
        p
    }

    /// 製品の Run 開始経路で worktree 隔離を検査するための最小 repo。
    fn git_repo(name: &str) -> Option<PathBuf> {
        let dir = ws(name);
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(dir.join("seed.txt"), "seed\n").ok()?;
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"])
            || !git(&["config", "user.email", "team-test@example.invalid"])
            || !git(&["config", "user.name", "Team Test"])
            || !git(&["add", "seed.txt"])
            || !git(&["commit", "-q", "-m", "seed"])
        {
            std::fs::remove_dir_all(&dir).ok();
            return None;
        }
        Some(dir)
    }

    /// GUI/CLI と同じ製品経路で Run を追加する。
    fn add_product_run(p: &mut TeamPanel) -> Result<(), String> {
        p.plan_with(SPEC, "SPEC.md", RunOptions::default(), Vec::new(), "")
    }

    /// **検証を明示する SPEC。**
    ///
    /// ここで見たいのは画面の筋道であって、検証コマンドの自動決定ではない。
    /// 自動決定そのものは `validation_defaults` と `planner` の番人が見る。
    const SPEC: &str = "# 認証\n## 要件\n- A を作る (src/a.rs)\n## 検証\n- cargo test\n";

    #[test]
    fn 計画してから開始する() {
        let dir = ws("plan");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        assert!(p.has_run());
        // **計画しただけでは起動要求は出ない。**
        assert!(p.take_launches().is_empty());
        assert_eq!(p.goal_status().unwrap(), GoalStatus::Ready);
        p.act(TeamAction::Start);
        assert_eq!(p.goal_status().unwrap(), GoalStatus::Running);
        // 置き場もワークスペースの下なので、これ 1 行で全部片付く
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn フォームの入力はすべて計画に効く() {
        use super::super::model::TeamRole as R;
        let dir = ws("form-effect");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan_with(
            SPEC,
            "SPEC.md",
            RunOptions {
                agent_count: 2,
                max_attempts: 5,
                review_required: false,
                ..RunOptions::default()
            },
            vec![R::Architect, R::Implementer],
            "私が付けた名前",
        )
        .unwrap();
        let rt = p.rt().unwrap();
        // Goal 名が効く
        assert_eq!(rt.goal().title, "私が付けた名前");
        // 役割の選択が効く (設計レーンが立つ)
        assert!(rt.teams().iter().any(|t| t.id.as_str() == "architecture"));
        // レビューを外したので QA レーンは立たない
        assert!(!rt.teams().iter().any(|t| t.id.as_str() == "qa"));
        // 最大試行回数と最大エージェント数が効く
        assert_eq!(rt.run().max_attempts, 5);
        assert_eq!(rt.run().agent_count, 2);
        assert!(!rt.run().review_required);
        // 置き場もワークスペースの下なので、これ 1 行で全部片付く
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 起動要求が出るところまで進めた画面。
    fn started_panel(dir: &Path) -> TeamPanel {
        std::fs::create_dir_all(dir).unwrap();
        let mut p = panel_at(dir);
        add_product_run(&mut p).unwrap();
        p.act(TeamAction::Start);
        p.pump(super::super::runtime::Observation {
            now: 1,
            sessions: Vec::new(),
        });
        p
    }

    fn finish_close_for_test(p: &mut TeamPanel) -> Vec<(RunOwner, String, SessionId)> {
        let stops = p.take_stops();
        for (owner, key, _) in stops.iter().cloned() {
            p.watch_stop(owner, key, None);
        }
        p.collect_validations();
        p.progress_close();
        stops
    }

    #[test]
    fn 起動要求はrunのworkspaceを運ぶ() {
        // **Runtime が決めた実行先を、実行側が取り直さないための材料。**
        // 要求そのものに workspace が載っていなければ、app は
        // 「いまの画面のフォルダ」を見るしかなくなる。
        let dir = ws("launch-ws");
        let mut p = started_panel(&dir);
        let launches = p.take_launches();
        assert!(!launches.is_empty(), "起動要求が出ていない");
        for (_, _, spec) in &launches {
            assert_eq!(
                spec.workspace_root,
                p.owner().expect("active Run").workspace,
                "起動要求が Run の workspace を運んでいない"
            );
            assert_ne!(spec.workspace_root, p.workspace, "元 workspace を共有した");
            // 飾りのフィールドを残さない (役割と名前は指示文と端末名に出る)。
            assert!(!spec.name.trim().is_empty(), "名前が空");
            assert!(!spec.team_id.0.trim().is_empty(), "所属チームが空");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 実行中は別のworkspaceへ切り替えない() {
        // **画面から消えたのに裏で動き続ける**エージェントと検証を作らない。
        let a = ws("switch-a");
        let b = ws("switch-b");
        std::fs::create_dir_all(&b).unwrap();
        let mut p = started_panel(&a);
        // 起動要求が残っている = まだ面倒を見ている
        assert!(p.live_work().is_busy(), "実行中と見なされていない");
        let before = p.workspace.clone();
        let err = p
            .attach_workspace(&b)
            .expect_err("切り替えを許してしまった");
        assert!(!err.trim().is_empty(), "断った理由が空");
        assert_eq!(p.workspace, before, "workspace が変わってしまった");
        assert!(p.has_run(), "Runtime を捨ててしまった");
        assert!(!p.take_launches().is_empty(), "抱えていた仕事まで消えた");
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn 走っている検証があるうちは切り替えない() {
        let a = ws("switch-val-a");
        let b = ws("switch-val-b");
        std::fs::create_dir_all(&b).unwrap();
        let mut p = started_panel(&a);
        // 仕事を全部片付けてから、検証だけを走らせる。
        let _ = p.take_launches();
        let (_tx, rx) = std::sync::mpsc::channel();
        p.watch_validation(ValidationJob {
            owner: p.owner().expect("Run がある"),
            task: 1,
            execution: "x".into(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: super::super::launch::new_cancel_flag(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });
        assert_eq!(p.live_work().validations, 1);
        assert!(p.attach_workspace(&b).is_err(), "検証を孤児にした");
        assert_eq!(p.running_validations(), 1, "検証の管理を手放した");
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn 面倒を見るものが無ければ切り替えられる() {
        // **`runtime.is_some()` で永久に断らない。** 計画しただけの Run は
        // 誰も動かしていないので、切り替えてよい。
        let a = ws("switch-idle-a");
        let b = ws("switch-idle-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let mut p = panel_at(&a);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        assert!(!p.live_work().is_busy(), "計画しただけで実行中と見なした");
        p.attach_workspace(&b).expect("切り替えられるべき");
        assert_eq!(p.workspace, b);
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn 同一workspaceは四本まで計画でき五本目を開始前に断る() {
        let dir = ws("four-run-limit");
        let mut p = panel_at(&dir);
        for i in 0..MAX_CONCURRENT_RUNS {
            p.plan(SPEC, "SPEC.md", RunOptions::default())
                .unwrap_or_else(|e| panic!("{} 本目を計画できない: {e}", i + 1));
        }
        let before: Vec<String> = p.runs.iter().map(|r| r.run().run_id.clone()).collect();
        let err = p
            .plan(SPEC, "SPEC.md", RunOptions::default())
            .expect_err("5本目を作ってしまった");
        assert!(!err.trim().is_empty(), "拒否理由が空");
        assert_eq!(p.notice, err, "利用者へ拒否理由を表示していない");
        assert_eq!(MAX_CONCURRENT_RUNS, 4, "製品上限が4本ではない");
        assert_eq!(p.runs.len(), MAX_CONCURRENT_RUNS);
        assert_eq!(
            p.runs
                .iter()
                .map(|r| r.run().run_id.clone())
                .collect::<Vec<_>>(),
            before,
            "拒否で既存 Run を置き換えた"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 内部フィクスチャで `runs` を書き換えず、GUI/CLI が共通で使う
    /// `plan` → `Start` だけで4本を開始できることを要求する。
    #[test]
    fn 製品経路から四本を別worktreeで開始し五本目を拒否する() {
        let Some(dir) = git_repo("product-four-runs") else {
            println!("[skip] git を使えません");
            return;
        };
        // 開始前からある未コミット変更を、worktree 作成で触らない。
        std::fs::write(dir.join("user-change.txt"), "keep me\n").unwrap();
        let mut p = panel_at(&dir);
        let mut owners = Vec::new();
        for i in 0..4 {
            add_product_run(&mut p)
                .unwrap_or_else(|e| panic!("{} 本目の計画を作れない: {e}", i + 1));
            p.act(TeamAction::Start);
            assert_eq!(p.goal_status(), Some(GoalStatus::Running));
            owners.push(p.owner().expect("開始した Run の持ち主"));
        }
        assert_eq!(MAX_CONCURRENT_RUNS, 4);
        assert_eq!(p.runs.len(), 4);
        let workspaces: std::collections::HashSet<PathBuf> =
            owners.iter().map(|o| o.workspace.clone()).collect();
        assert_eq!(workspaces.len(), 4, "Run 間で実行 workspace が共有された");
        assert!(
            workspaces.iter().all(|w| w != &dir && w.is_dir()),
            "専用 worktree ではない: {workspaces:?}"
        );
        let before: Vec<String> = owners.iter().map(|o| o.run_id.clone()).collect();
        let err = p
            .plan(SPEC, "SPEC.md", RunOptions::default())
            .expect_err("5 本目を開始できてしまった");
        assert!(err.contains('4'), "上限が分かる拒否理由でない: {err}");
        assert_eq!(
            p.runs
                .iter()
                .map(|r| r.run().run_id.clone())
                .collect::<Vec<_>>(),
            before,
            "5 本目の拒否で既存 Run が壊れた"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("user-change.txt")).unwrap(),
            "keep me\n",
            "元 workspace の未コミット変更が壊れた"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 非git_workspaceは共有実行へfallbackせず開始前に拒否する() {
        let dir = ws("start-non-git");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = TeamPanel::default();
        p.home = dir.join(".zaivern-test-home");
        p.attach_workspace(&dir).expect("attach");
        add_product_run(&mut p).expect("計画までは作れる");
        let source = p.owner().expect("Run").workspace;
        p.act(TeamAction::Start);
        assert_eq!(p.goal_status(), Some(GoalStatus::Ready), "共有workspaceで開始した");
        assert_eq!(p.owner().expect("Run").workspace, source);
        assert!(p.runs[0].run().run_workspace.is_none());
        assert!(p.take_launches().is_empty(), "拒否したのに担当を起動した");
        assert!(!p.notice.trim().is_empty(), "拒否理由を表示していない");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 二本目のworktree対応保存失敗は一本目を壊さない() {
        let Some(dir) = git_repo("worktree-save-failure") else {
            println!("[skip] git を使えません");
            return;
        };
        let mut p = panel_at(&dir);
        add_product_run(&mut p).expect("A計画");
        p.act(TeamAction::Start);
        let a = p.owner().expect("A");
        assert!(a.workspace.is_dir());
        add_product_run(&mut p).expect("B計画");
        let b_id = p.owner().expect("B").run_id;
        persistence::fault_inject::fail_at(persistence::SavePhase::TmpWritten);
        p.act(TeamAction::Start);
        persistence::fault_inject::clear();
        assert_eq!(p.goal_status(), Some(GoalStatus::Ready), "保存前にBを開始した");
        assert!(p.runs[p.active].run().run_workspace.is_none());
        assert!(a.workspace.is_dir(), "失敗でAのworktreeを消した");
        let b_root = super::super::run_workspace::expected_root(&p.home, &dir, &b_id)
            .expect("Bの決定パス");
        assert!(!b_root.exists(), "保存に失敗したBのworktreeを残した");
        assert_eq!(p.runs.len(), 2, "失敗で既存Runを消した");
        let pos_a = p.run_pos_of_owner(&a).expect("A");
        p.close_run(pos_a);
        p.close_run(0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 作成済み未保存のworktreeをstartが再利用して既存runを壊さない() {
        let Some(dir) = git_repo("worktree-created-before-save") else {
            println!("[skip] git を使えません");
            return;
        };
        let mut p = panel_at(&dir);
        add_product_run(&mut p).expect("A計画");
        p.act(TeamAction::Start);
        let a = p.owner().expect("A");
        add_product_run(&mut p).expect("B計画");
        let b_id = p.owner().expect("B").run_id;
        let orphan = super::super::run_workspace::create(&p.home, &dir, &b_id)
            .expect("保存直前に落ちたworktreeを再現");

        p.act(TeamAction::Start);
        let b = p.owner().expect("B");
        assert_eq!(b.workspace, PathBuf::from(&orphan.execution_workspace));
        assert_eq!(p.goal_status(), Some(GoalStatus::Running));
        assert!(a.workspace.is_dir(), "再利用時にAのworktreeを壊した");
        p.close_run(p.run_pos_of_owner(&a).expect("A"));
        p.close_run(0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn worktree削除失敗は墓標と診断を残し別runを巻き込まず再試行する() {
        let Some(dir) = git_repo("worktree-remove-retry") else {
            println!("[skip] git を使えません");
            return;
        };
        let mut p = panel_at(&dir);
        add_product_run(&mut p).expect("A計画");
        p.act(TeamAction::Start);
        let a = p.owner().expect("A");
        add_product_run(&mut p).expect("B計画");
        p.act(TeamAction::Start);
        let b = p.owner().expect("B");
        p.save_if_needed();
        let state = p.state_dir();

        super::super::run_workspace::fault_inject::fail_remove_once();
        p.close_run(p.run_pos_of_owner(&a).expect("A")).expect("Aを閉じる");
        finish_close_for_test(&mut p);
        assert!(a.workspace.is_dir(), "削除失敗を成功扱いした");
        assert!(b.workspace.is_dir(), "Aの失敗でBのworktreeを消した");
        assert!(persistence::is_closed(&state, &a.run_id), "Aの墓標が無い");
        assert!(p.notice.contains("削除に失敗"), "診断が残らない: {}", p.notice);

        let mut q = TeamPanel::default();
        q.home = p.home.clone();
        q.attach_workspace(&dir).expect("再attach");
        q.restore_run(false).expect("Bを復元");
        assert!(!a.workspace.exists(), "再起動でAの削除を再試行していない");
        assert!(b.workspace.is_dir(), "再試行でBのworktreeを消した");
        assert_eq!(q.runs.len(), 1);
        assert_eq!(q.owner().expect("B").run_id, b.run_id);
        assert!(!persistence::is_closed(&state, &a.run_id), "清掃後も墓標が残った");
        q.close_run(0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn activeより前のrunを閉じても同じactive_runを保つ() {
        let dir = ws("close-before-active");
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        for _ in 1..3 {
            add_product_run(&mut p).unwrap();
        }
        let active_owner = p.runs[1].owner();
        p.select_run(1);
        p.close_run(0).expect("先頭を閉じる");
        assert_eq!(p.rt().map(TeamRuntime::owner), Some(active_owner));
        assert_eq!(p.active_run(), 0, "同じ Run の新しい添字へ追従していない");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn runを閉じるとそのrunの検証だけを止める() {
        let dir = ws("close-one-validation");
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        let a = p.owner().unwrap();
        add_product_run(&mut p).unwrap();
        let b = p.owner().unwrap();
        let cancel_a = super::super::launch::new_cancel_flag();
        let cancel_b = super::super::launch::new_cancel_flag();
        let mut senders = Vec::new();
        for (owner, cancel, execution) in [
            (a.clone(), cancel_a.clone(), "a"),
            (b.clone(), cancel_b.clone(), "b"),
        ] {
            let (tx, rx) = std::sync::mpsc::channel();
            senders.push(tx);
            p.watch_validation(ValidationJob {
                owner,
                task: 1,
                execution: execution.into(),
                commands: vec!["cargo test".into()],
                started_at: super::super::model::now_secs(),
                timeout_secs: 60,
                cancel,
                pid: super::super::launch::new_pid_slot(),
                rx,
            });
        }
        let pos = p.run_pos_of_owner(&a).unwrap();
        p.close_run(pos).expect("A を閉じる");
        assert!(cancel_a.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!cancel_b.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(p.running_validations(), 2, "停止完了前に検証記録を捨てた");
        drop(senders.remove(0));
        p.collect_validations();
        p.progress_close();
        assert_eq!(p.running_validations(), 1);
        assert_eq!(p.validation_jobs[0].owner, b);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 別のrunの検証結果は受け取らない() {
        // 外向きの 4 つの口と同じ形で、**戻ってくる側にも持ち主を見る**。
        // 実行 ID にも `run_id` は入っているが、そちらは文字列の書式に
        // 頼った守りなので、構造でも止める。
        let dir = ws("owner-validation");
        let mut p = started_panel(&dir);
        let old_owner = p.owner().expect("Run がある");
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = super::super::launch::new_cancel_flag();
        p.watch_validation(ValidationJob {
            owner: old_owner.clone(),
            task: 1,
            execution: "old".into(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: cancel.clone(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });
        // 2 本目を作り、画面をそちらへ切り替える。
        add_product_run(&mut p).unwrap();
        // 前の Run の worker が結果を返してくる。
        tx.send(("old".into(), 1, vec![ValidationRun::passed("cargo test a")]))
            .unwrap();
        p.collect_validations();
        assert_eq!(p.running_validations(), 0, "前の Run のジョブを抱えたまま");
        assert!(
            !cancel.load(std::sync::atomic::Ordering::Relaxed),
            "生きている非active Runの正常完了をキャンセルした"
        );
        let t = p.rt().and_then(|rt| rt.task(1)).expect("タスク").clone();
        assert!(
            t.validation.runs.is_empty(),
            "別の Run の実測を採った: {:?}",
            t.validation.runs
        );
        let old = p
            .runs
            .iter()
            .find(|rt| rt.owner() == old_owner)
            .and_then(|rt| rt.task(1))
            .expect("発行元Runのタスク");
        assert!(
            old.validation.runs.is_empty(),
            "実行IDの違う結果を発行元Runへ採った"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    /// **実行中の Run を2本目の計画で潰さず、別 Run として保持する。**
    fn 実行中のrunを別の計画で潰さず並行runとして保持する() {
        let dir = ws("replace-busy");
        let mut p = started_panel(&dir);
        assert!(p.live_work().is_busy());
        let before = p.owner().expect("持ち主");
        p.plan(SPEC, "SPEC.md", RunOptions::default())
            .expect("2本目を計画できる");
        assert_eq!(p.runs.len(), 2);
        assert!(
            p.runs.iter().any(|rt| rt.owner() == before),
            "実行中の Run を置き換えた"
        );
        assert_ne!(p.owner(), Some(before), "2本目が独立した Run になっていない");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 別のrunのeffectは実行させない() {
        // 切り替えを断る仕組みだけに頼らない。**持ち主が違う Effect は
        // 渡さない**という構造の検査 (キューが空になる偶然に頼らない)。
        let a = ws("owner-a");
        let mut p = started_panel(&a);
        assert!(!p.pending_launches.is_empty());
        // 別の Run へ差し替える (workspace ごと作り直す = 切り替えと同じ状況)。
        // 断りを外しても**持ち主の照合が効く**ことを見たいので、検査だけを
        // 黙らせて Run を作り直す (本番は `plan` が断る)。
        // **閉じた Run の仕事は実行しない。**
        // 同時に走らせられるようになったので「作り直したら前のは死ぬ」では
        // なくなった。死ぬのは**閉じたとき**なので、そこで確かめる。
        let stale = p.pending_launches.clone();
        add_product_run(&mut p).unwrap();
        p.close_run(0).expect("1 本目を閉じる");
        finish_close_for_test(&mut p);
        p.pending_launches = stale;
        assert!(
            p.take_launches().is_empty(),
            "前の Run の起動要求を新しい Run で実行しようとした"
        );
        assert!(p.dropped_effects() > 0, "捨てたことを数えていない");
        assert!(!p.notice.trim().is_empty(), "黙って捨てている");
        std::fs::remove_dir_all(&a).ok();
    }

    #[test]
    fn 指示と停止と検証も持ち主で選り分ける() {
        let a = ws("owner-all");
        let mut p = started_panel(&a);
        // **外向きの口すべて**に、前の Run のものを積む。
        // 口を 1 つ足したらここへも足すこと (足し忘れると、前の Run の
        // 指示が新しい Run の相手へ届く)。
        let owner = p.owner().expect("持ち主");
        p.pending_instructions
            .push((owner.clone(), "instr:x".into(), (1, 7, "hi".into())));
        p.pending_manual.push((
            owner.clone(),
            "manual:a:1".into(),
            (AgentId("a".into()), 7, "hi".into()),
        ));
        p.pending_stops.push((owner.clone(), "stop:7".into(), 7));
        p.pending_validations.push((
            owner,
            "validate:x".into(),
            super::super::runtime::ValidationSpec {
                task: 1,
                execution: "x".into(),
                commands: vec![super::super::validation_command::ValidationCommand::parse(
                    "cargo test a",
                )
                .unwrap()],
                approved: Vec::new(),
                cwd: a.clone(),
                timeout_secs: 600,
            },
        ));
        let stale = (
            p.pending_launches.clone(),
            p.pending_instructions.clone(),
            p.pending_stops.clone(),
            p.pending_validations.clone(),
            p.pending_manual.clone(),
        );
        // **閉じた Run のものは、どの口からも出さない。**
        add_product_run(&mut p).unwrap();
        p.close_run(0).expect("1 本目を閉じる");
        finish_close_for_test(&mut p);
        p.pending_launches = stale.0;
        p.pending_instructions = stale.1;
        p.pending_stops = stale.2;
        p.pending_validations = stale.3;
        p.pending_manual = stale.4;
        assert!(p.take_launches().is_empty(), "起動が漏れた");
        assert!(p.take_instructions().is_empty(), "指示が漏れた");
        // **停止だけは出る。** 閉じた Run の担当を止める命令は、閉じた後に
        // 実行側が取り出す (捨てるとプロセスが残る)。具体的なセッションを
        // 名指しするので、新しい Run の相手へ届く余地は無い。
        let stops = p.take_stops();
        assert!(
            stops.iter().any(|(_, k, s)| k == "stop:7" && *s == 7),
            "閉じた Run の停止まで捨てた (プロセスが残る): {stops:?}"
        );
        assert!(p.take_validations().is_empty(), "検証が漏れた");
        assert!(p.take_manual_instructions().is_empty(), "人の指示が漏れた");
        std::fs::remove_dir_all(&a).ok();
    }

    /// **Git が無いフォルダでは、走らせる前に分かる。**
    ///
    /// 実測 (`changeset`) は git が出す差分を使うので、Git 管理下でない
    /// フォルダでは**どの完了報告も却下される**。実機では 7 体が並列で
    /// 働いているのに 1 件も終わらず、画面には理由が出ていなかった。
    #[test]
    fn git無しのワークスペースは計画の時点で分かる() {
        let dir = ws("no-git");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = TeamPanel::default();
        let _ = p.attach_workspace(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default())
            .expect("計画");
        assert!(p.needs_git, "Git が無いのに気付いていない");

        // **押したら実測できるようになる。** (人が押したときだけ走る)
        p.init_git().expect("Git を用意できる");
        assert!(!p.needs_git, "用意したのに旗が残っている");
        assert!(
            crate::git::discover_toplevel(&dir).is_some(),
            "git init が効いていない"
        );
        // コミットが 1 つ無いと `git status` に全部が未追跡で並び、
        // 基準点が「全部汚れている」になる。最初のコミットまで打つこと。
        let base = super::super::changeset::capture_baseline(&dir).expect("基準点を取れる");
        assert!(base.usable(), "取れた基準点が使えない");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **HEAD が無いリポジトリを「準備完了」と判定しない。**
    ///
    /// 前の版は `.git` の有無だけを見ていたので、基準点のコミットに失敗した
    /// 後にもう一度押すと「準備完了」と表示し、基準点が無いまま Run が
    /// 走っていた (完了報告の帰属判定も担当外変更の判定も壊れる)。
    #[test]
    fn head無しのリポジトリを準備完了と判定しない() {
        let dir = ws("no-head");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}").unwrap();
        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            println!("[skip] git を使えません");
            std::fs::remove_dir_all(&dir).ok();
            return;
        }
        git(&["config", "user.email", "t@example.invalid"]);
        git(&["config", "user.name", "t"]);
        // `.git` はあるが HEAD が無い = commit に失敗した後の状態。
        assert!(
            crate::git::discover_toplevel(&dir).is_some(),
            "前提: リポジトリはある"
        );
        let mut p = TeamPanel::default();
        p.home = dir.join(".zaivern-test-home");
        let _ = p.attach_workspace(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).expect("計画");
        assert!(
            p.needs_git,
            "HEAD が無いのに準備完了と判定した (基準点が無いまま走る)"
        );
        // **もう一度押せば続きから作れる。**
        p.init_git().expect("続きから作れる");
        assert!(!p.needs_git, "用意したのに旗が残っている");
        let base = super::super::changeset::capture_baseline(&dir).expect("基準点");
        assert!(base.usable() && base.entries.is_empty(), "基準点が綺麗でない");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **秘密情報らしいファイルを黙って履歴へ入れない。**
    /// 断ったら準備完了にもしない (中途半端に進めない)。
    #[test]
    fn 秘密情報らしいファイルがあると準備を断る() {
        let dir = ws("git-secrets");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join(".env"), "TOKEN=abc").unwrap();
        let mut p = TeamPanel::default();
        p.home = dir.join(".zaivern-test-home");
        let _ = p.attach_workspace(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).expect("計画");
        match p.init_git() {
            Err(why) => {
                assert!(why.contains(".env"), "何を止めたのか言っていない: {why}");
                assert!(p.needs_git, "断ったのに準備完了にした");
                assert!(!p.notice.trim().is_empty(), "画面に理由が出ていない");
            }
            Ok(()) => {
                // git を使えない環境ではここへ来ない (使えれば必ず断る)。
                panic!("秘密情報らしいファイルを黙ってコミットした");
            }
        }
        assert_eq!(
            std::fs::read_to_string(dir.join(".env")).unwrap(),
            "TOKEN=abc",
            "利用者のファイルを触った"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **2 本の Run が同時に進む。**
    ///
    /// 「並べられる」だけでは足りない — 画面に出していないほうも
    /// `tick` が回り、その仕事が実行の口から出てくることまで見る
    /// (回さないと、2 本目の担当は起動したまま何も配られない)。
    #[test]
    fn 二本のrunが同時に進む() {
        let dir = ws("two-runs");
        let mut p = started_panel(&dir);
        let first = p.owner().expect("1 本目");
        // 起動要求を片付けてから 2 本目を作る (口を空にして違いを見る)。
        // 1 つは「閉じた Run の古い起動要求」として後で使う。
        let stale_launch = p
            .take_launches()
            .into_iter()
            .next()
            .expect("1 本目の起動要求");
        add_product_run(&mut p).expect("製品経路の2本目");
        let second = p.owner().expect("2 本目");
        assert_ne!(first.run_id, second.run_id, "同じ Run になっている");

        // **画面に出していないほうの仕事も実行の口から出る。**
        //
        // ここが壊れていると 2 本目は「並んでいるだけ」になる
        // (以前は `mine` が「いまの Run」だけを通していたので、
        //  画面に出していない Run の仕事は全部捨てられていた)。
        let _ = p.take_launches();
        p.pending_stops.push((first.clone(), "stop:1".into(), 1));
        p.pending_stops.push((second.clone(), "stop:2".into(), 2));
        let stops = p.take_stops();
        assert_eq!(stops.len(), 2, "片方の Run の仕事が捨てられた: {stops:?}");
        assert_eq!(p.dropped_effects(), 0, "生きている Run のものを捨てた");

        // **閉じた Run のものは捨てる** (死んだ相手へ配らない) — ただし
        // **停止だけは別**。閉じた Run の担当を止める命令は、閉じた後に
        // 実行側が取り出すので、生存 Run で選り分けたら二度と届かない。
        p.pending_stops.push((first.clone(), "stop:1".into(), 1));
        p.pending_launches.push(stale_launch.clone());
        p.close_run(0).expect("1 本目を閉じる");
        let stops = p.take_stops();
        assert!(
            stops.iter().any(|(o, k, s)| o == &first && k == "stop:1" && *s == 1),
            "閉じた Run の停止が捨てられた (プロセスが残る): {stops:?}"
        );
        for (owner, key, _) in stops.iter().cloned() {
            p.watch_stop(owner, key, None);
        }
        p.progress_close();
        assert!(p.take_launches().is_empty(), "閉じた Run の起動を実行した");
        assert!(p.dropped_effects() > 0, "捨てたことを数えていない");
        assert_eq!(p.runs.len(), 1);
        assert_eq!(p.owner().as_ref(), Some(&second), "残ったほうが出ていない");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 画面の切り替えだけを見る (上のテストと分ける — 落ちたときに
    /// 「配達が壊れた」のか「切り替えが壊れた」のかが 1 行で分かる)。
    #[test]
    fn 画面を切り替えても両方の_run_が生きている() {
        let dir = ws("two-runs-switch");
        let mut p = started_panel(&dir);
        let first = p.owner().expect("1 本目");
        let boot_a = p.take_launches();
        p.plan_with(
            SPEC,
            "SPEC-B.md",
            RunOptions::default(),
            Vec::new(),
            "Run B",
        )
        .expect("製品経路の2本目");
        let second_id = p.owner().expect("2 本目").run_id;
        p.act(TeamAction::Start);
        let second = p.owner().expect("開始後の2 本目");
        assert_eq!(second.run_id, second_id);
        p.pump(super::super::runtime::Observation {
            now: 2,
            sessions: Vec::new(),
        });
        let boot_b = p.take_launches();
        let spec_a = &boot_a.first().expect("Aの起動").2;
        let spec_b = &boot_b.first().expect("Bの起動").2;
        assert_eq!(spec_a.agent_id, spec_b.agent_id, "前提: 同じ担当ID");
        p.bind_session(&first, &spec_a.agent_id, 1101, None);
        p.bind_session(&second, &spec_b.agent_id, 2202, None);
        p.pump(super::super::runtime::Observation {
            now: 3,
            sessions: vec![
                super::super::runtime::SessionObs {
                    id: 1101,
                    title: "A terminal".into(),
                    provider: "codex".into(),
                    state: crate::coordinator::SessionState::Working,
                    text: "Aだけのログ".into(),
                },
                super::super::runtime::SessionObs {
                    id: 2202,
                    title: "B terminal".into(),
                    provider: "claude".into(),
                    state: crate::coordinator::SessionState::Working,
                    text: "Bだけのログ".into(),
                },
            ],
        });
        assert_ne!(first.run_id, second.run_id);
        assert_eq!(p.run_tabs().len(), 2, "切り替えの見出しが 2 つ出ない");
        p.select_run(0);
        p.refresh_snapshot(3);
        assert_eq!(p.owner().as_ref(), Some(&first));
        assert_eq!(p.runs.len(), 2, "切り替えで消えた");
        let snap_a = p.snapshot().cloned().expect("Aの表示");
        assert_ne!(snap_a.goal.title, "Run B");
        assert!(snap_a.agents.iter().any(|a| a.preview.contains("Aだけのログ")));
        assert!(!snap_a.agents.iter().any(|a| a.preview.contains("Bだけのログ")));
        let tasks_a: Vec<String> = p.runs[0].tasks().iter().map(|t| t.title.clone()).collect();
        assert_eq!(
            snap_a.tasks.iter().map(|t| t.title.clone()).collect::<Vec<_>>(),
            tasks_a,
            "Aのタスク表示ではない"
        );
        p.expanded_output = Some(spec_a.agent_id.clone());
        p.select_run(1);
        assert_eq!(p.owner().as_ref(), Some(&second));
        assert!(p.expanded_output.is_none(), "Aの展開端末をBへ持ち越した");
        p.refresh_snapshot(3);
        let snap_b = p.snapshot().expect("Bの表示");
        assert_eq!(snap_b.goal.title, "Run B");
        assert!(snap_b.agents.iter().any(|a| a.preview.contains("Bだけのログ")));
        assert!(!snap_b.agents.iter().any(|a| a.preview.contains("Aだけのログ")));
        let tasks_b: Vec<String> = p.runs[1].tasks().iter().map(|t| t.title.clone()).collect();
        assert_eq!(
            snap_b.tasks.iter().map(|t| t.title.clone()).collect::<Vec<_>>(),
            tasks_b,
            "Bのタスク表示ではない"
        );
        // 範囲外は無視する (押せない位置を作らない)。
        p.select_run(99);
        assert_eq!(p.active_run(), 1);
        p.close_run(1);
        p.close_run(0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **同じ SPEC の Run は key も agent ID も同じになる。**
    ///
    /// 取り出したあと owner を落とすと、2 本目を先に処理しただけで 1 本目へ
    /// ACK/bind される。実行順を意図的に逆にして、両方が独立することを見る。
    #[test]
    fn 同一spec二runの同じ起動keyをownerで分離する() {
        let dir = ws("two-runs-same-key");
        let mut p = started_panel(&dir);
        let launches_a = p.take_launches();
        assert!(!launches_a.is_empty(), "1 本目の起動要求が無い");

        add_product_run(&mut p).expect("製品経路の2本目");
        p.act(TeamAction::Start);
        p.pump(super::super::runtime::Observation {
            now: 2,
            sessions: Vec::new(),
        });
        let launches_b = p.take_launches();
        assert!(!launches_b.is_empty(), "2 本目の起動要求が無い");

        let (owner_a, key, spec_a) = launches_a[0].clone();
        let (owner_b, _, spec_b) = launches_b
            .into_iter()
            .find(|(_, candidate, _)| candidate == &key)
            .expect("同じ SPEC なら同じ起動 key が出る");
        assert_ne!(owner_a, owner_b, "別 Run の owner が同じ");
        assert_eq!(spec_a.agent_id, spec_b.agent_id, "前提: agent ID も同じ");

        // 2 本目を先に処理する。key だけで先勝ちすると、ここで A が完了する。
        p.bind_session(&owner_b, &spec_b.agent_id, 2202, None);
        p.ack_done(&owner_b, &key);
        let pos_a = p.run_pos_of_owner(&owner_a).expect("A");
        let pos_b = p.run_pos_of_owner(&owner_b).expect("B");
        assert!(p.runs[pos_b].effect_completed(&key), "B に ACK が入らない");
        assert!(
            !p.runs[pos_a].effect_completed(&key),
            "同じ key の A を誤って ACK した"
        );

        p.bind_session(&owner_a, &spec_a.agent_id, 1101, None);
        p.ack_done(&owner_a, &key);
        let sid_a = p.runs[pos_a]
            .agents()
            .iter()
            .find(|a| a.id == spec_a.agent_id)
            .and_then(|a| a.session_id);
        let sid_b = p.runs[pos_b]
            .agents()
            .iter()
            .find(|a| a.id == spec_b.agent_id)
            .and_then(|a| a.session_id);
        assert_eq!(sid_a, Some(1101), "A の session が混線した");
        assert_eq!(sid_b, Some(2202), "B の session が混線した");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 非activeは「閉じた」と同義ではない。結果は発行元 Run へ返す。
    #[test]
    fn 非active_runのvalidation結果を発行元へ返す() {
        let dir = ws("validation-background-run");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).expect("A");
        let owner_a = p.owner().expect("A owner");
        let pos_a = p.run_pos_of_owner(&owner_a).expect("A pos");
        let execution = p.runs[pos_a].current_execution(1);
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = super::super::launch::new_cancel_flag();
        p.watch_validation(ValidationJob {
            owner: owner_a.clone(),
            task: 1,
            execution: execution.clone(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: cancel.clone(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });

        add_product_run(&mut p).expect("B");
        let owner_b = p.owner().expect("B owner");
        let pos_b = p.run_pos_of_owner(&owner_b).expect("B pos");
        assert_eq!(p.active_run(), pos_b, "前提: B が active ではない");
        tx.send((execution, 1, vec![ValidationRun::passed("cargo test a")]))
            .unwrap();
        p.collect_validations();

        assert_eq!(p.running_validations(), 0, "完了結果を抱えたまま");
        assert!(
            !cancel.load(std::sync::atomic::Ordering::Relaxed),
            "非activeというだけで生きた Run の検証を停止した"
        );
        assert!(
            p.runs[pos_a].task(1).is_some_and(|t| t
                .validation
                .runs
                .iter()
                .any(|r| r.command == "cargo test a")),
            "A に検証結果が返っていない"
        );
        assert!(
            p.runs[pos_b]
                .task(1)
                .is_some_and(|t| t.validation.runs.is_empty()),
            "activeなBへAの検証結果を誤帰属した"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run破棄は走っている検証を先に止める() {
        let dir = ws("discard-cancel");
        let mut p = started_panel(&dir);
        let cancel = super::super::launch::new_cancel_flag();
        let (_tx, rx) = std::sync::mpsc::channel();
        p.watch_validation(ValidationJob {
            owner: p.owner().expect("Run がある"),
            task: 1,
            execution: "x".into(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: cancel.clone(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });
        p.discard_run().expect("破棄できる");
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "破棄したのに検証を止めていない (プロセスが残る)"
        );
        assert!(!p.has_run());
        // **Runtime を捨てても、まだ畳んでいないものは「面倒を見ている」。**
        // ここで 0 と答えると、直後の workspace 切り替えが通ってしまい、
        // 走っているプロセスの管理を手放す。
        assert_eq!(p.live_work().validations, 1, "破棄した瞬間に空と答えた");
        assert!(p.live_work().is_busy());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **実物のプロセスを 1 つ走らせて**、閉じたときに落ちることを見る。
    ///
    /// worker スレッドは**わざと置かない**。アプリを閉じる瞬間は、札を見る
    /// はずの worker ごと消える — そこで誰が木を落とすのか、が要点なので、
    /// worker が生きていると検査が空回りする (実際に、`kill_tree` を外して
    /// も worker が自分で落としてしまい緑のままだった)。
    #[cfg(unix)]
    #[test]
    fn 閉じると実際に検証プロセスが落ちる() {
        use std::os::unix::process::CommandExt;
        let dir = ws("shutdown-kill");
        let mut p = started_panel(&dir);
        let marker = dir.join("still-alive");
        // 落とすのは `stop_all_validations_now` の仕事なので、こちらでは
        // `wait` しない (clippy はそれを疑うので、意図を書いて許す)。
        #[allow(clippy::zombie_processes)]
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("sleep 1; : > {}", marker.display()))
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("子を起こせる");
        let pid = super::super::launch::new_pid_slot();
        pid.store(child.id(), std::sync::atomic::Ordering::Relaxed);
        let (_tx, rx) = std::sync::mpsc::channel();
        p.watch_validation(ValidationJob {
            owner: p.owner().expect("Run がある"),
            task: 1,
            execution: "x".into(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: super::super::launch::new_cancel_flag(),
            pid,
            rx,
        });
        assert_eq!(p.stop_all_validations_now(), 1, "落とした件数が合わない");
        // 生き残っていれば 1 秒後にファイルを作る。
        std::thread::sleep(std::time::Duration::from_millis(1800));
        assert!(
            !marker.exists(),
            "閉じたのに検証プロセスが生き残った: {}",
            marker.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 閉じるときは保存して手放し検証も落とす() {
        // **状態はアプリより長生きする** (`thread_local!`)。持ったまま次の
        // アプリへ渡すと、もう居ないセッションへ結び付いた Runtime を
        // 新しい画面が見ることになる。
        let dir = ws("shutdown");
        let mut p = started_panel(&dir);
        let cancel = super::super::launch::new_cancel_flag();
        let (_tx, rx) = std::sync::mpsc::channel();
        p.watch_validation(ValidationJob {
            owner: p.owner().expect("Run がある"),
            task: 1,
            execution: "x".into(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: cancel.clone(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });
        assert!(p.has_run());
        p.shutdown();
        assert!(!p.has_run(), "Runtime を手放していない");
        assert_eq!(p.running_validations(), 0, "検証を抱えたまま閉じた");
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "検証を止めずに閉じた"
        );
        assert!(p.take_launches().is_empty(), "未実行の仕事が残っている");
        // 保存はされているので、次回は復元から入り直せる。
        assert!(
            persistence::has_run(&persistence::team_dir_in(&p.home, &dir)),
            "保存せずに手放した"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 実行側の返事が来るまで完了扱いにしない() {
        // **画面 (app) が Effect を実行し、成功を返して初めて済んだことになる。**
        // 返す前に落ちても、次の起動でもう一度出る。
        let dir = ws("ack");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        p.act(TeamAction::Start);
        p.pump(super::super::runtime::Observation {
            now: 1,
            sessions: Vec::new(),
        });
        let launches = p.take_launches();
        assert!(!launches.is_empty(), "起動要求が出ていない");
        // 冪等キーが必ず添えられている (返せないと ACK もできない)
        for (_, key, _) in &launches {
            assert!(key.starts_with("start:"), "冪等キーが無い: {key}");
        }
        // 失敗を返せば、次の tick でもう一度出る
        for (owner, key, _) in &launches {
            p.ack_failed(owner, key);
        }
        p.pump(super::super::runtime::Observation {
            now: 2,
            sessions: Vec::new(),
        });
        assert_eq!(
            p.take_launches().len(),
            launches.len(),
            "失敗を返したのに再発行されない"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 検証 1 件を「走っている」状態で預ける (受け口だけ返す)。
    fn queue_job(
        p: &mut TeamPanel,
        task: TaskId,
        execution: &str,
    ) -> std::sync::mpsc::Sender<(String, TaskId, Vec<ValidationRun>)> {
        let (tx, rx) = std::sync::mpsc::channel();
        p.watch_validation(ValidationJob {
            owner: p.owner().expect("Run がある"),
            task,
            execution: execution.to_string(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: super::super::launch::new_cancel_flag(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });
        tx
    }

    #[test]
    fn 実行器との接続が切れたら失敗として戻す() {
        // **握り潰さない。** 受け口を捨てるだけだと `validation.running` が
        // true のまま残り、そのタスクは二度と進まない。
        let dir = ws("disconnect");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        let exec = p.rt().expect("runtime").current_execution(1);
        let tx = queue_job(&mut p, 1, &exec);
        assert_eq!(p.running_validations(), 1);
        drop(tx); // worker が panic した状況
        p.collect_validations();
        assert_eq!(p.running_validations(), 0, "受け口を抱えたまま");
        let t = p.rt().and_then(|rt| rt.task(1)).expect("タスク").clone();
        assert!(!t.validation.running, "実行中のまま残った");
        assert!(
            t.validation
                .runs
                .iter()
                .any(|r| r.outcome() == super::super::model::ValidationOutcome::RunnerDisconnected),
            "接続断を記録していない: {:?}",
            t.validation.runs
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 結果も切断も来ない検証は見張りが決着させる() {
        // 実行器が自分で打ち切れず、送り手も生きたまま黙っている経路
        // (worker がブロックした)。**ここを開けたままにすると、
        // `validation.running` が永久に true のまま残る。**
        let dir = ws("watchdog");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        let exec = p.rt().expect("runtime").current_execution(1);
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = super::super::launch::new_cancel_flag();
        p.watch_validation(ValidationJob {
            owner: p.owner().expect("Run がある"),
            task: 1,
            execution: exec,
            commands: vec!["cargo test a".into()],
            // 時限 + 余白より前に始まっている = もう見切ってよい。
            started_at: super::super::model::now_secs()
                .saturating_sub(60 + WATCHDOG_SLACK_SECS + 10),
            timeout_secs: 60,
            cancel: cancel.clone(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });
        p.collect_validations();
        assert_eq!(p.running_validations(), 0, "見切っていない");
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "見切ったのに停止の札を立てていない (プロセスが残る)"
        );
        let t = p.rt().and_then(|rt| rt.task(1)).expect("タスク").clone();
        assert!(!t.validation.running);
        assert!(
            t.validation
                .runs
                .iter()
                .any(|r| r.outcome() == super::super::model::ValidationOutcome::TimedOut),
            "時間切れとして記録していない: {:?}",
            t.validation.runs
        );
        drop(tx);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 停止の要求は走っている検証にだけ届く() {
        let dir = ws("cancel");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        let _tx = queue_job(&mut p, 1, "run:1:0:1");
        assert!(!p.cancel_validation("run:1:0:99"), "世代違いまで止めた");
        assert!(p.cancel_validation("run:1:0:1"), "止められなかった");
        assert_eq!(p.running_validations(), 1, "札を立てただけで捨てない");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 読み取り専用では操作できない() {
        let dir = ws("ro");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        p.read_only = true;
        p.act(TeamAction::Start);
        assert_eq!(p.goal_status().unwrap(), GoalStatus::Ready);
        assert!(p.notice.contains("読み取り専用"));
        // 置き場もワークスペースの下なので、これ 1 行で全部片付く
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 保存して復元すると未完了runとして出る() {
        let dir = ws("restore");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let mut p = panel_at(&dir);
            p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
            p.act(TeamAction::Start);
            p.save_if_needed();
        }
        let mut q = panel_at(&dir);
        assert_eq!(
            q.restore,
            RestorePrompt::Found,
            "未完了 Run を検出していない"
        );
        q.restore_run(false).unwrap();
        assert!(q.has_run());
        assert_eq!(q.restore, RestorePrompt::None);
        assert!(q.goal_status().is_some());
        // 置き場もワークスペースの下なので、これ 1 行で全部片付く
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 不正なspecは計画に失敗する() {
        let dir = ws("bad");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        assert!(p.plan("   ", "SPEC.md", RunOptions::default()).is_err());
        assert!(!p.has_run(), "失敗したのに Run ができている");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn スナップショットは変わったときだけ作り直す() {
        let dir = ws("snap");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        p.refresh_snapshot(100);
        let a = p.snapshot().cloned().expect("スナップショット");
        // 何も変えずに呼んでも中身は同じ (作り直しても等価)
        p.refresh_snapshot(101);
        assert_eq!(p.snapshot().unwrap(), &a);
        p.act(TeamAction::Start);
        p.refresh_snapshot(102);
        assert_ne!(p.snapshot().unwrap().goal.status, a.goal.status);
        // 置き場もワークスペースの下なので、これ 1 行で全部片付く
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 観測だけの変化でもactive_runのスナップショットを更新する() {
        let dir = ws("snapshot-observation");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        let owner = p.owner().expect("Run");
        let agent = p.rt().unwrap().agents()[0].id.clone();
        p.bind_session(&owner, &agent, 7, None);
        p.refresh_snapshot(100);
        assert!(!p.dirty);

        let observation = super::super::runtime::Observation {
            now: 101,
            sessions: vec![super::super::runtime::SessionObs {
                id: 7,
                title: "worker".into(),
                provider: "codex".into(),
                state: crate::coordinator::SessionState::Working,
                text: "HTML を実装中".into(),
            }],
        };
        p.pump(observation.clone());
        assert!(p.dirty, "Effect の無い観測変化でも dirty にする");
        p.refresh_snapshot(101);
        let view = p
            .snapshot()
            .unwrap()
            .agents
            .iter()
            .find(|a| a.id == agent)
            .expect("対象agent");
        assert_eq!(view.provider, "codex");
        assert!(view.preview.contains("HTML を実装中"));

        p.pump(super::super::runtime::Observation {
            now: 102,
            ..observation
        });
        assert!(!p.dirty, "静止画で snapshot を作り直さない");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn フォームの初期値は仕様どおり() {
        let f = NewRunForm::default();
        assert_eq!(f.agents, 4);
        assert_eq!(f.max_attempts, 3);
        assert!(f.review_required);
        assert_eq!(f.approval_mode, "ask");
        assert!(!f.composition_touched, "開いた直後は手で変えていない");
        // 既定のプリセットは実装 + レビュー
        assert_eq!(
            f.roles,
            vec![
                TeamRole::Planner,
                TeamRole::Architect,
                TeamRole::Implementer,
                TeamRole::Tester,
                TeamRole::Reviewer,
                TeamRole::Integrator,
            ],
            "既定は選べる 6 つ全部 (2 つだとチームとして分担しない)"
        );
    }

    #[test]
    fn 役割プリセットは選べる全役割の部分集合() {
        let f = NewRunForm::default();
        for r in &f.roles {
            assert!(TeamRole::ALL.contains(r), "{} は選択肢に無い", r.key());
        }
    }

    #[test]
    fn タブは同時に一つ() {
        let mut p = TeamPanel::default();
        assert_eq!(p.tab, BoardTab::Organization);
        p.tab = BoardTab::Tasks;
        assert_eq!(p.tab, BoardTab::Tasks);
        // 4 つのタブ ID は重複しない
        let mut seen = std::collections::BTreeSet::new();
        for t in BoardTab::ALL {
            assert!(seen.insert(t.key()));
        }
    }

    /// **エコーの後に届いた本物の報告が、ちゃんと解析器へ届く。**
    ///
    /// 実機で止まっていた形をそのまま置く。指示は PTY へ打ち込むので
    /// エージェントの TUI がひな型ごと描き返し、`[ZAI-TEAM-RESULT]` や
    /// `"blockers": []` は**先に「見た行」になる**。行で重複を落としていた
    /// 頃は、本物の報告が来ても**開始マーカーごと消えて**解析器に届かず、
    /// 却下も受理も記録されないまま止まっていた。
    ///
    /// 「届いたこと」は**居ないタスク番号**で見る。`#99` を名指しした
    /// 断りは、その塊が解析器を通ったときにしか出ない — 通らなければ
    /// 事象は 1 件も増えないので、空回りしない検査になる。
    #[test]
    fn エコーの後の本物の報告が届く() {
        let dir = ws("echo-then-real");
        let mut p = started_panel(&dir);
        let boot = p.take_launches();
        let mut sid: SessionId = 1;
        for (owner, _, spec) in &boot {
            p.bind_session(owner, &spec.agent_id, sid, None);
            sid += 1;
        }
        let open = super::super::result_parser::RESULT_OPEN;
        let close = super::super::result_parser::RESULT_CLOSE;
        let block = |task: u64, summary: &str| {
            format!(
                "{open}\n{{\"task_id\": {task}, \"agent_id\": \"team-lead\", \
                 \"status\": \"completed\", \"summary\": \"{summary}\", \
                 \"changed_files\": [], \"validation\": [], \"blockers\": []}}\n{close}"
            )
        };
        let rows = |text: &str| -> Vec<SessionInput> {
            vec![SessionInput {
                id: 1,
                title: "a".into(),
                provider: "claude".into(),
                state: crate::coordinator::SessionState::Idle,
                tail: text.lines().map(str::to_string).collect(),
            }]
        };
        let saw_99 = |p: &TeamPanel| -> bool {
            p.rt()
                .is_some_and(|r| r.events().any(|e| e.summary.contains("#99")))
        };
        // 1) 指示のエコー (ひな型) が先に画面へ出る。
        let echo = block(1, "何をしたかの 1 行");
        p.pump_sessions(rows(&echo), 100);
        assert!(!saw_99(&p), "まだ本物は来ていない");
        // 2) そのあとに本物が積み上がる。**行の多くはエコーと同じ。**
        p.pump_sessions(rows(&format!("{echo}\n{}", block(99, "本物"))), 101);
        assert!(
            saw_99(&p),
            "エコーの後に届いた本物の報告が解析器へ届いていない (実機で止まっていた形)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **同じ報告を二度取り込まない。**
    ///
    /// 以前はここで「行」の重複を落としていたが、それでは報告そのものが
    /// 分断される — 指示のエコーで `[ZAI-TEAM-RESULT]` は既に「見た行」に
    /// なっているので、本物の報告が来ても開始マーカーごと消えて解析器に
    /// 届かない (実機で、完了報告を出しているのに却下も受理も記録されない
    /// まま止まっていた)。重複は**意味の単位 (塊)** で落とす。
    #[test]
    fn 同じ報告を二度取り込まない() {
        let dir = ws("seen-blocks");
        let mut p = started_panel(&dir);
        let boot = p.take_launches();
        let mut sid: SessionId = 1;
        for (owner, _, spec) in &boot {
            p.bind_session(owner, &spec.agent_id, sid, None);
            sid += 1;
        }
        // 完了報告を含む画面を**2 回**渡す。取り込みは 1 回だけ。
        let block = format!(
            "{}\n{{\"task_id\": 1, \"agent_id\": \"team-lead\", \"status\": \"completed\", \
             \"summary\": \"やった\", \"changed_files\": [], \"validation\": [], \"blockers\": []}}\n{}",
            super::super::result_parser::RESULT_OPEN,
            super::super::result_parser::RESULT_CLOSE
        );
        let rows = |text: &str| -> Vec<SessionInput> {
            vec![SessionInput {
                id: 1,
                title: "a".into(),
                provider: "claude".into(),
                state: crate::coordinator::SessionState::Idle,
                tail: text.lines().map(str::to_string).collect(),
            }]
        };
        // 報告の取り込みで出る事象だけを数える (割り当てなどは別に進む)。
        let report_events = |p: &TeamPanel| -> usize {
            p.rt()
                .map(|r| {
                    r.events()
                        .filter(|e| {
                            matches!(
                                e.kind,
                                TeamEventKind::Rejected | TeamEventKind::TaskCompleted
                            )
                        })
                        .count()
                })
                .unwrap_or(0)
        };
        // **割り当てが落ち着いてから**報告を渡す (配られた直後は、また
        // 出してよい状態へ動いたばかりなので記憶を持たない — それが正しい)。
        p.pump_sessions(rows(""), 99);
        p.pump_sessions(rows(""), 99);
        p.pump_sessions(rows(&block), 100);
        let after_first = report_events(&p);
        assert!(after_first > 0, "1 回目で報告が取り込まれていない");
        // **同じ画面をもう一度渡しても、報告の事象は増えない。**
        p.pump_sessions(rows(&block), 101);
        assert_eq!(
            after_first,
            report_events(&p),
            "同じ報告を二度取り込んでいる (却下や受理が二重に並ぶ)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 起動要求の確認も間隔を空ける() {
        let mut p = TeamPanel::default();
        let t0 = Instant::now();
        assert!(p.launch_poll_due(t0), "初回は見に行く");
        assert!(!p.launch_poll_due(t0), "毎フレーム stat を撃たない");
        assert!(p.launch_poll_due(t0 + LAUNCH_POLL_INTERVAL + Duration::from_millis(1)));
    }

    #[test]
    fn 走査は間隔を空ける() {
        let mut p = TeamPanel::default();
        let t0 = Instant::now();
        assert!(p.scan_due(t0), "初回は走る");
        assert!(!p.scan_due(t0), "同じ瞬間に二度は走らない");
        assert!(p.scan_due(t0 + SCAN_INTERVAL + Duration::from_millis(1)));
    }

    /// **制御面を投函から再開まで 1 本で通す** (どの OS でも同じ経路)。
    ///
    /// GUI そのものは描けないので、GUI の直下 — 起動要求 / 計画 / 開始 /
    /// 起動 Effect / 担当割当 / 検証 Effect / 停止と再開 / 保存と復元 —
    /// を繋げて確かめる。**`cfg` で OS を分けない**ので、Windows の CI でも
    /// そのまま走る: パスの区切り (`\`) と、`~/.zaivern` 配下の置き場と、
    /// JSON へ往復するパスの綴りが、この 1 本で全部通ることになる。
    ///
    /// 個々の段の細かい振る舞いはそれぞれの番人が見ているので、ここは
    /// **鎖が繋がっていること**だけを見る (どこかが切れたら落ちる)。
    #[test]
    fn 制御面は投函から再開までどのosでも通る() {
        use super::super::launch;

        let dir = ws("os-chain");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("a.rs"), "fn a() {}\n").unwrap();
        // **検証は「読むだけ」のものにする。** リポジトリのコードを走らせる
        // ものだと承認待ちで止まり、検証 Effect まで届かない。
        let spec_body =
            "# 認証\n## 要件\n- A を作る (src/a.rs)\n## 検証\n- rustfmt --check src/a.rs\n";
        let spec_path = dir.join("SPEC.md");
        std::fs::write(&spec_path, spec_body).unwrap();
        // **置き場は workspace の外**に置く。中に置くと自分の書いた記録が
        // `git status` に出て、実測が「担当外の変更」として拾ってしまう。
        let home = ws("os-chain-home");
        let now = super::super::model::now_secs();

        // 完了報告は**実測**を通る (`git` が要る)。綺麗なリポジトリから
        // 始めるので、報告そのものの受理まで届く。
        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            eprintln!("[skip] 制御面は投函から再開まで — git を使えません");
            std::fs::remove_dir_all(&dir).ok();
            return;
        }
        git(&["config", "user.email", "t@example.invalid"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        assert!(
            git(&["add", "-A"]) && git(&["commit", "-q", "-m", "init"]),
            "実験場を commit できない"
        );

        // ── 1) 投函 → 拾える。境界の外の SPEC は組み立てで断る ───────────
        let req = launch::build(&dir, &spec_path, 2, false).expect("投函を組み立てられる");
        assert_eq!(req.agent_count, 2);
        assert!(
            launch::request_matches_workspace(&req, &dir),
            "この workspace 宛ての要求を受け取れない"
        );
        launch::post_in(&home, &req).expect("投函できる");
        let got = launch::take_in(&home, &dir, now).expect("投函を拾えない");
        assert_eq!(got.spec_text, spec_body, "SPEC の中身が変わった");
        assert!(
            launch::take_in(&home, &dir, now).is_none(),
            "同じ投函を二度拾った"
        );
        let outside_dir = ws("os-chain-outside");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let outside = outside_dir.join("SPEC.md");
        std::fs::write(&outside, spec_body).unwrap();
        assert!(
            launch::build(&dir, &outside, 2, false).is_err(),
            "workspace の外の SPEC を受理した"
        );

        // ── 2) 計画 (agents=2) ────────────────────────────────────────────
        let mut p = TeamPanel::default();
        p.home = home.clone();
        p.attach_workspace(&dir).expect("attach できる");
        p.plan(
            &got.spec_text,
            "SPEC.md",
            RunOptions {
                agent_count: 2,
                ..RunOptions::default()
            },
        )
        .expect("計画できる");
        assert_eq!(p.rt().expect("Runtime").run().agent_count, 2);

        // ── 3) 開始 → 起動要求。パスはこの OS の綴りのまま運ばれる ───────
        p.act(TeamAction::Start);
        assert_eq!(p.goal_status(), Some(GoalStatus::Running));
        // 開始しただけでは何も起きない。**調停ループが 1 度回って**初めて
        // 「誰を起こすか」が決まる (押した瞬間に副作用を出さない)。
        p.pump(super::super::runtime::Observation {
            now,
            sessions: Vec::new(),
        });
        let launches = p.take_launches();
        assert!(!launches.is_empty(), "起動要求が 1 つも出ない");
        let owner = launches[0].0.clone();
        let mut agents = Vec::new();
        for (launch_owner, key, spec) in &launches {
            assert_eq!(
                spec.workspace_root, owner.workspace,
                "起動先が Run 専用 workspace と一致しない"
            );
            assert_ne!(
                spec.workspace_root, owner.source_workspace,
                "元 workspace を実行先として共有している"
            );
            agents.push(spec.agent_id.clone());
            p.ack_done(launch_owner, key);
        }

        // ── 4) セッションを結び付ける → 担当が付く ────────────────────────
        let sessions: Vec<SessionId> = (0..agents.len() as SessionId).map(|i| 100 + i).collect();
        for (a, s) in agents.iter().zip(&sessions) {
            p.bind_session(&owner, a, *s, None);
        }
        let rows = |target: Option<SessionId>, text: &str| -> Vec<SessionInput> {
            sessions
                .iter()
                .map(|s| SessionInput {
                    id: *s,
                    title: format!("agent{s}"),
                    provider: "claude".into(),
                    state: crate::coordinator::SessionState::Idle,
                    tail: if Some(*s) == target {
                        text.lines().map(|l| l.to_string()).collect()
                    } else {
                        Vec::new()
                    },
                })
                .collect()
        };
        p.pump_sessions(rows(None, ""), now + 1);
        let working: Vec<(TaskId, SessionId, String)> = p
            .rt()
            .expect("Runtime")
            .tasks()
            .iter()
            .filter(|t| t.state.is_working())
            .filter_map(|t| {
                Some((
                    t.id,
                    t.assigned_session?,
                    t.assigned_agent.as_ref()?.0.clone(),
                ))
            })
            .collect();
        assert!(!working.is_empty(), "担当が 1 つも付かない");

        // ── 5) 完了報告 → 検証 Effect (cwd は Run 専用 workspace) ─────────
        let (tid, sid, agent) = working[0].clone();
        let t = p.rt().unwrap().task(tid).expect("タスク").clone();
        let files: Vec<String> = t.files.iter().map(|x| format!("\"{x}\"")).collect();
        let vs: Vec<String> = t
            .validation_commands
            .iter()
            .map(|c| format!("{{\"command\":\"{c}\",\"exit_code\":0}}"))
            .collect();
        let report = format!(
            "{open}\n{{\"task_id\":{tid},\"agent_id\":\"{agent}\",\"status\":\"completed\",\
             \"summary\":\"実装しました\",\"changed_files\":[{f}],\"validation\":[{v}],\"blockers\":[]}}\n{close}",
            open = super::super::result_parser::RESULT_OPEN,
            close = super::super::result_parser::RESULT_CLOSE,
            f = files.join(","),
            v = vs.join(","),
        );
        p.pump_sessions(rows(Some(sid), &report), now + 2);
        let validations = p.take_validations();
        assert!(!validations.is_empty(), "検証の要求が出ない");
        assert_eq!(
            validations[0].2.cwd, owner.workspace,
            "検証の cwd が Run 専用 workspace と一致しない"
        );
        assert!(
            !validations[0].2.commands.is_empty(),
            "空の検証を頼んでいる"
        );

        // ── 6) 停止と再開が状態機械の上で成立する ─────────────────────────
        p.act(TeamAction::Pause);
        assert_eq!(p.goal_status(), Some(GoalStatus::Paused), "止まらない");
        p.act(TeamAction::Resume);
        assert_eq!(p.goal_status(), Some(GoalStatus::Running), "戻らない");
        p.act(TeamAction::Stop);
        assert!(p.rt().expect("Runtime").run().stopped, "停止が効かない");

        // ── 7) 元/実行 workspace の対応を保存して復元できる ───────────────
        p.save_if_needed();
        let saved_owner = p.owner().expect("保存前の持ち主");
        drop(p);
        let mut q = TeamPanel::default();
        q.home = home.clone();
        q.attach_workspace(&dir).expect("attach できる");
        assert_eq!(
            q.restore,
            RestorePrompt::Found,
            "保存済み Run を見つけられない"
        );
        q.restore_run(false).expect("復元できる");
        assert!(q.has_run(), "復元しても Run が無い");
        let restored = q.owner().expect("持ち主");
        assert_eq!(restored.source_workspace, saved_owner.source_workspace);
        assert_eq!(
            restored.workspace, saved_owner.workspace,
            "往復で Run 専用 workspace の対応が変わった"
        );
        q.close_run(0).expect("worktree を片付ける");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside_dir).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// **Team Run の作成で、利用者のグローバル設定を変えない。**
    ///
    /// フォームの既定は `ask` / `0`。`0` は**このコードベースでは
    /// 「上限なし」**なので、既存設定を読まずに流すと、`agent` / `25` で
    /// 使っている人がフォームを開いて計画しただけで承認モードが下がり、
    /// 課金の上限が永続的に外れる。
    #[test]
    fn team_runの計画は既存のguardrailsを初期値にして書き換えない() {
        use super::super::model::RunGuardrails;

        // ── ケース A: フォームは既存設定を初期値として持つ ───────────────
        let dir = ws("guardrails");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        // 既定は「読んでいない」値のまま。
        assert_eq!(p.form.approval_mode, "ask");
        assert_eq!(p.form.cost_limit, 0.0);
        p.seed_guardrails("agent", 25.0);
        assert_eq!(
            p.form.approval_mode, "agent",
            "既存の承認モードを読んでいない"
        );
        assert_eq!(p.form.cost_limit, 25.0, "既存のコスト上限を読んでいない");
        // **開いている間は上書きしない** (人が選び直した値を戻さない)。
        p.form.open = true;
        p.form.approval_mode = "ask".into();
        p.seed_guardrails("agent", 25.0);
        assert_eq!(p.form.approval_mode, "ask", "入力中の値を上書きした");
        p.form.open = false;

        // ── ケース B: Run 側で ask / 0 を選んでも、既存の値は緩まない ────
        let run = RunGuardrails {
            approval_mode: "ask".into(),
            cost_limit: 0.0,
        };
        assert_eq!(
            run.effective_approval("agent"),
            "ask",
            "Run 側の厳しい選択が効いていない"
        );
        assert_eq!(
            run.effective_cost_limit(25.0),
            25.0,
            "Run 側の 0 で既存の上限が外れた (0 は「上限なし」)"
        );

        // **緩める方向には効かない。**
        let loose = RunGuardrails {
            approval_mode: "auto".into(),
            cost_limit: 100.0,
        };
        assert_eq!(
            loose.effective_approval("ask"),
            "ask",
            "Run 側の指定で承認モードが緩んだ"
        );
        assert_eq!(
            loose.effective_cost_limit(25.0),
            25.0,
            "Run 側の大きい上限で既存の上限が緩んだ"
        );
        // 既存に上限が無ければ、Run 側の上限がそのまま効く (締める方向)。
        assert_eq!(loose.effective_cost_limit(0.0), 100.0);
        // 空 = 何も足さない。
        let none = RunGuardrails::default();
        assert_eq!(none.effective_approval("agent"), "agent");
        assert_eq!(none.effective_cost_limit(25.0), 25.0);
        // 読めない綴りは**いちばん厳しい側**に倒す。
        let junk = RunGuardrails {
            approval_mode: "yolo".into(),
            cost_limit: 0.0,
        };
        assert!(
            crate::agents::Approval::from_mode(&junk.effective_approval("auto"))
                == crate::agents::Approval::Ask,
            "読めない綴りを自動側へ倒している"
        );
        assert_eq!(
            super::super::model::approval_looseness("yolo"),
            super::super::model::approval_looseness("ask"),
            "読めない綴りをいちばん厳しい側に置いていない"
        );

        // ── ケース C: Run 固有設定は保存・復元をまたいで同じ ──────────────
        p.plan_with(
            SPEC,
            "SPEC.md",
            RunOptions {
                guardrails: RunGuardrails {
                    approval_mode: "ask".into(),
                    cost_limit: 5.0,
                },
                ..RunOptions::default()
            },
            Vec::new(),
            "",
        )
        .expect("計画できる");
        assert_eq!(
            p.run_guardrails(),
            Some(RunGuardrails {
                approval_mode: "ask".into(),
                cost_limit: 5.0,
            }),
            "Run へ運ばれていない"
        );
        p.act(TeamAction::Start);
        p.save_if_needed();
        let mut q = TeamPanel::default();
        q.home = p.home.clone();
        q.attach_workspace(&dir).expect("attach できる");
        q.restore_run(false).expect("復元できる");
        assert_eq!(
            q.run_guardrails(),
            Some(RunGuardrails {
                approval_mode: "ask".into(),
                cost_limit: 5.0,
            }),
            "復元で Run 固有設定が失われた"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **この欄が無い旧 Run も読める** (`serde(default)`)。
    #[test]
    fn guardrailsを持たない旧runも読める() {
        let doc: super::super::persistence::RunDoc =
            serde_json::from_str(r#"{"version":4,"run_id":"r","workspace":"/w","spec_source":"s","agent_count":2,"max_attempts":3,"review_required":true,"paused":false,"stopped":false,"started_at":0,"updated_at":0}"#)
                .expect("旧 Run が読めない");
        assert_eq!(
            doc.guardrails,
            super::super::model::RunGuardrails::default()
        );
    }

    /// **積めたことを「届いた」にしない。**
    ///
    /// 送信経路は「積めた」と「届いた」を別の時刻に決める。積んだ時点で
    /// 冪等キーを完了にすると、そのあと相手が消えても・入力欄が空かない
    /// まま上限に達しても、Runtime は「指示は届いた」と信じたままタスクを
    /// 抱え続ける (二度と出し直されない = 指示が消える)。
    #[test]
    fn 届かなかった指示は完了にせず配り直せる形へ戻す() {
        let dir = ws("delivery");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        p.act(TeamAction::Start);
        p.pump(super::super::runtime::Observation {
            now: 1,
            sessions: Vec::new(),
        });
        let launches = p.take_launches();
        assert!(!launches.is_empty(), "前提: 起動要求が出ている");
        let mut sessions: Vec<SessionId> = Vec::new();
        for (i, (owner, key, spec)) in launches.iter().enumerate() {
            p.ack_done(owner, key);
            let sid = 700 + i as SessionId;
            p.bind_session(owner, &spec.agent_id, sid, Some(format!("/logs/{sid}.log")));
            sessions.push(sid);
        }
        let rows = || -> Vec<SessionInput> {
            sessions
                .iter()
                .map(|s| SessionInput {
                    id: *s,
                    title: format!("agent{s}"),
                    provider: "claude".into(),
                    state: crate::coordinator::SessionState::Idle,
                    tail: Vec::new(),
                })
                .collect()
        };
        p.pump_sessions(rows(), 2);
        let sent = p.take_instructions();
        assert!(!sent.is_empty(), "前提: 指示が出ている");
        let (owner, key, task, _, _) = sent[0].clone();
        let tag = p
            .delivery_tag(&owner, &key)
            .expect("Run があるので目印は作れる");

        // ── 届かなかった ────────────────────────────────────────────────
        let hit = p.note_delivery(&tag, false);
        assert_eq!(hit, Some(task), "どのタスクが届かなかったか返していない");
        let rt = p.rt().expect("Runtime");
        assert!(
            !rt.effect_completed(&key),
            "届いていないのに完了として記録した (二度と出し直されない)"
        );
        let t = rt.task(task).expect("タスク");
        assert!(
            t.assigned_session.is_none() && t.assigned_agent.is_none(),
            "届かなかったのに担当を握ったまま: {t:?}"
        );
        assert!(
            t.context.iter().any(|c| c.contains("届きません")),
            "理由が残っていない: {:?}",
            t.context
        );

        // ── 届いた ──────────────────────────────────────────────────────
        p.pump_sessions(rows(), 3);
        let again = p.take_instructions();
        assert!(!again.is_empty(), "配り直しの指示が出ない");
        let (owner2, key2, _, _, _) = again[0].clone();
        let tag2 = p.delivery_tag(&owner2, &key2).expect("目印");
        assert_eq!(p.note_delivery(&tag2, true), None, "届いたのに理由を返した");
        assert!(
            p.rt().expect("Runtime").effect_completed(&key2),
            "届いたのに完了として記録していない (もう一度送ってしまう)"
        );
        // **Team のものではない目印は何も起こさない。**
        assert_eq!(p.note_delivery("submit:someone-else", false), None);
        // **別の Run の配達は、いまの Run へ効かない。**
        let other = format!("run-other|{key2}");
        assert_eq!(
            p.note_delivery(&other, false),
            None,
            "別の Run の配達を採った"
        );
        assert!(
            p.rt().expect("Runtime").effect_completed(&key2),
            "別の Run の配達でいまの Run の記録を壊した"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **配達の結末は「出した Run」へ返る。画面に出している Run ではない。**
    ///
    /// 実行するのは `mine` = 走っている**全 Run** の仕事なので、結末も同じ
    /// 範囲で受け取れなければ辻褄が合わない。ここが `owner()` (画面に出して
    /// いる 1 本) だったので、Run が 2 本あるとき・配達中に人がタブを切り
    /// 替えたときに**結末が丸ごと消えて**いた — 台帳に 1 行も残らず、担当は
    /// `running` のまま放置される (実機で 6 体中 2 体が 28 分)。
    #[test]
    fn 画面に出していないrunの配達でも結末が返る() {
        let dir = ws("delivery-two-runs");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);

        // ── 1 本目の Run を起こし、指示を口まで出す ──────────────────────
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        p.act(TeamAction::Start);
        p.pump(super::super::runtime::Observation {
            now: 1,
            sessions: Vec::new(),
        });
        let launches = p.take_launches();
        assert!(!launches.is_empty(), "前提: 起動要求が出ている");
        let mut sessions: Vec<SessionId> = Vec::new();
        for (i, (owner, key, spec)) in launches.iter().enumerate() {
            p.ack_done(owner, key);
            let sid = 900 + i as SessionId;
            p.bind_session(owner, &spec.agent_id, sid, Some(format!("/logs/{sid}.log")));
            sessions.push(sid);
        }
        let rows: Vec<SessionInput> = sessions
            .iter()
            .map(|s| SessionInput {
                id: *s,
                title: format!("agent{s}"),
                provider: "claude".into(),
                state: crate::coordinator::SessionState::Idle,
                tail: Vec::new(),
            })
            .collect();
        p.pump_sessions(rows, 2);
        let sent = p.take_instructions();
        assert!(!sent.is_empty(), "前提: 指示が出ている");
        let (owner, key, task, _, _) = sent[0].clone();

        // **名札は「出した Run」のもの。**
        let run_a = p.runs[0].run().run_id.clone();
        let tag = p
            .delivery_tag(&owner, &key)
            .expect("Run があるので目印は作れる");
        assert_eq!(
            tag,
            format!("{run_a}|{key}"),
            "出した Run の名札が付いていない"
        );

        // ── 配達の途中で 2 本目の Run が立つ (= 画面がそちらへ移る) ──────
        add_product_run(&mut p).unwrap();
        assert_eq!(p.runs.len(), 2, "前提: Run が 2 本ある");
        let run_b = p.runs[1].run().run_id.clone();
        assert_ne!(run_a, run_b, "前提: Run の ID は別");
        assert_eq!(
            p.owner().expect("Run").run_id,
            run_b,
            "前提: 画面は 2 本目を出している"
        );

        // ── 届かなかった結末は、1 本目へ返らなければならない ─────────────
        let hit = p.note_delivery(&tag, false);
        assert_eq!(
            hit,
            Some(task),
            "画面に出していない Run の配達の結末が消えた (台帳に 1 行も残らない)"
        );
        let rt = &p.runs[0];
        assert!(
            !rt.effect_completed(&key),
            "届いていないのに完了として記録した (二度と出し直されない)"
        );
        let t = rt.task(task).expect("タスク");
        assert!(
            t.assigned_session.is_none() && t.assigned_agent.is_none(),
            "届かなかったのに担当を握ったまま: {t:?}"
        );
        assert!(
            t.context.iter().any(|c| c.contains("届きません")),
            "台帳に理由が残っていない: {:?}",
            t.context
        );

        // ── 2 本目 (画面に出している Run) は 1 バイトも触られていない ────
        assert!(
            p.runs[1]
                .task(task)
                .is_none_or(|t| t.context.iter().all(|c| !c.contains("届きません"))),
            "別の Run の結末を、画面に出している Run の台帳へ書いた"
        );

        // ── 死んだ Run の配達は、どの Run にも効かない ───────────────────
        assert_eq!(
            p.note_delivery(&format!("run-gone|{key}"), false),
            None,
            "居ない Run の配達を採った"
        );
        assert_eq!(p.note_delivery("submit:someone-else", false), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// **鍵は Run をまたいで綴りまで重なる。結末は名札の Run だけへ効く。**
    ///
    /// `instr:<task>:<agent>:<attempt>:<seq>` のタスク番号もエージェント名も
    /// Run ごとに 1 から数え直すので、同じ SPEC を 2 本走らせると綴りが一致
    /// する。鍵だけで持ち主を探すと**先に見つけた Run**へ結末が付き、本当の
    /// 宛先はいつまでも返事を受け取れない。目印は Run を持っているので、
    /// そちらで引き直せなければならない。
    #[test]
    fn 鍵が重なっても結末は名札のrunだけへ効く() {
        let dir = ws("delivery-key-collision");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);

        // ── 1 本目の Run が指示を出す ────────────────────────────────────
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        p.act(TeamAction::Start);
        p.pump(super::super::runtime::Observation {
            now: 1,
            sessions: Vec::new(),
        });
        let launches = p.take_launches();
        assert!(!launches.is_empty(), "前提: 起動要求が出ている");
        let mut sessions: Vec<SessionId> = Vec::new();
        for (i, (owner, key, spec)) in launches.iter().enumerate() {
            p.ack_done(owner, key);
            let sid = 1100 + i as SessionId;
            p.bind_session(owner, &spec.agent_id, sid, Some(format!("/logs/{sid}.log")));
            sessions.push(sid);
        }
        let rows: Vec<SessionInput> = sessions
            .iter()
            .map(|s| SessionInput {
                id: *s,
                title: format!("agent{s}"),
                provider: "claude".into(),
                state: crate::coordinator::SessionState::Idle,
                tail: Vec::new(),
            })
            .collect();
        p.pump_sessions(rows, 2);
        let sent = p.take_instructions();
        assert!(!sent.is_empty(), "前提: 指示が出ている");
        let (_owner, key, _, _, _) = sent[0].clone();

        // ── 2 本目の Run が、**同じ綴りの鍵**を発行済みに持つ ────────────
        add_product_run(&mut p).unwrap();
        assert_eq!(p.runs.len(), 2, "前提: Run が 2 本ある");
        let run_b = p.runs[1].run().run_id.clone();
        p.runs[1].note_effect_dispatched_for_test(&key);
        assert!(
            p.runs[0].has_effect(&key) && p.runs[1].has_effect(&key),
            "前提: 2 本とも同じ綴りの鍵を持っている"
        );

        // ── 2 本目の名札で「届いた」を返す ───────────────────────────────
        assert_eq!(
            p.note_delivery(&format!("{run_b}|{key}"), true),
            None,
            "届いたのに理由を返した"
        );
        assert!(
            p.runs[1].effect_completed(&key),
            "名札の Run へ完了が入っていない"
        );
        assert!(
            !p.runs[0].effect_completed(&key),
            "同じ綴りの鍵を持つ別の Run まで完了にした (鍵で先勝ちして取り違えている)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn team_state_does_not_leak_across_workspaces_or_app_contexts() {
        // 状態は `thread_local!` に居て **`ZaivernApp` より長生きする**。
        // 「UI スレッドあたり 1 インスタンス」という前提が破れた日
        // (前のアプリが `on_exit` を通らずに消えた / 同じスレッドに 2 つ目が
        // 立った) に、新しいアプリが前の Run を暗黙に引き継がないことを見る。
        let a = ws("leak-a");
        let b = ws("leak-b");
        std::fs::create_dir_all(&b).unwrap();

        // ── アプリ A: workspace A で Run を作り、セッションまで結び付ける ──
        let mut p = started_panel(&a);
        let owner_a = p.owner().expect("A の持ち主");
        let launches = p.take_launches();
        assert!(!launches.is_empty(), "前提: 起動要求が出ている");
        // **アプリ A のセッションへ結び付ける。** ここを省くと「復元しても
        // 前のアプリのセッションへ結び付かない」の検査が空回りする
        // (結び付いていないものは、外れていて当たり前)。
        let mut sessions: Vec<SessionId> = Vec::new();
        for (i, (owner, key, spec)) in launches.iter().enumerate() {
            p.ack_done(owner, key);
            let sid = 900 + i as SessionId;
            p.bind_session(owner, &spec.agent_id, sid, None);
            sessions.push(sid);
        }
        // **観測にも同じセッションを出す。** 出さないと「消えた」と見なされ、
        // 結び付きが解かれてしまう (この検査が空回りする)。
        p.pump_sessions(
            sessions
                .iter()
                .map(|s| SessionInput {
                    id: *s,
                    title: format!("agent{s}"),
                    provider: "claude".into(),
                    state: crate::coordinator::SessionState::Idle,
                    tail: Vec::new(),
                })
                .collect(),
            2,
        );
        assert!(
            p.rt()
                .expect("Runtime")
                .agents()
                .iter()
                .any(|ag| ag.session_id.is_some()),
            "前提: アプリ A のセッションへ結び付いている"
        );
        let cancel = super::super::launch::new_cancel_flag();
        let (_tx, rx) = std::sync::mpsc::channel();
        p.watch_validation(ValidationJob {
            owner: owner_a.clone(),
            task: 1,
            execution: "x".into(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: cancel.clone(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });
        assert!(p.live_work().is_busy(), "前提: 面倒を見ているものがある");

        // ── workspace 軸: 走っている間は別の workspace へ渡さない ────────
        let err = p
            .attach_workspace(&b)
            .expect_err("実行中なのに別の workspace へ渡した");
        assert!(!err.trim().is_empty(), "断った理由が空");
        assert_eq!(p.owner().as_ref(), Some(&owner_a), "Run が入れ替わった");

        // ── アプリ文脈の軸: 新しいアプリは前の Run を引き継がない ────────
        assert!(p.adopt_new_app_context(), "残っていたものを手放していない");
        assert!(!p.has_run(), "新しいアプリが前の Run を握ったままになった");
        assert_eq!(p.owner(), None, "持ち主が残っている");
        assert_eq!(p.running_validations(), 0, "検証を抱えたまま引き継いだ");
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "面倒を見る相手が居なくなったのに検証を止めていない"
        );
        // **保存はされている。** 続きは復元の案内から入り直せる
        // (「引き継がない」は「捨てる」ではない)。
        assert!(
            persistence::has_run(&persistence::team_dir_in(&p.home, &a)),
            "保存せずに手放した"
        );

        // ── 引き継ぎ後: 同じ workspace を開き直しても、入り直しになる ────
        // `workspace` を空へ戻していないと「同じだから何もしない」で
        // 早々に返り、保存済み Run の案内すら出ない。
        p.attach_workspace(&a)
            .expect("新しいアプリは attach できる");
        assert!(!p.has_run(), "attach しただけで前の Run が復活した");
        assert_eq!(
            p.restore,
            RestorePrompt::Found,
            "保存済み Run の案内が出ない (入り直せない)"
        );

        // ── 復元しても、前のアプリのセッションへは結び付かない ───────────
        // ここが「引き継がない」ことの実質。`run_id` は同じ Run なので
        // 引き継ぐが (それが復元の意味)、**プロセスへの結び付きは全部外れる**
        // ので、新しいアプリが前のアプリの端末を操作することはない。
        p.restore_run(false).expect("復元できるべき");
        let rt = p.rt().expect("復元した Runtime");
        assert!(
            rt.tasks()
                .iter()
                .all(|t| t.assigned_session.is_none() && t.coordinator_task.is_none()),
            "前のアプリのセッションへ結び付いたまま復元した: {:?}",
            rt.tasks()
                .iter()
                .map(|t| (t.id, t.assigned_session))
                .collect::<Vec<_>>()
        );
        assert!(
            rt.agents().iter().all(|a| a.session_id.is_none()),
            "前のアプリのセッションを持つエージェントが残った"
        );
        // 前のアプリが残した仕事も 1 つも持っていない。
        assert!(
            p.take_launches().is_empty(),
            "前のアプリの起動要求が残っている"
        );
        assert!(
            p.take_validations().is_empty(),
            "前のアプリの検証が残っている"
        );

        // ── 普通の起動 (何も残っていない) では 1 命令も走らない ──────────
        let mut fresh = TeamPanel::default();
        assert!(
            !fresh.adopt_new_app_context(),
            "何も無いのに後始末を走らせた"
        );

        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn スレッドローカルの状態は独立して触れる() {
        with_panel(|p| p.notice = "hello".into());
        with_panel(|p| assert_eq!(p.notice, "hello"));
        with_panel(|p| p.notice.clear());
    }

    /// **人が出した指示は、タスクの機構へ流さない。**
    ///
    /// 宛先タスクが無いことがあるし、あっても「いま待っている指示」では
    /// ないので、`note_instruction_undelivered` の照合は必ず外れる。
    /// そこへ流すと、届かなかった事実が毎回「古い配達」として捨てられて
    /// **画面にも記録にも 1 行も残らない**。
    #[test]
    fn 人の指示は端末まで出て届かなければ知らせる() {
        let dir = ws("manual-delivery");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        p.act(TeamAction::Start);
        p.pump(super::super::runtime::Observation {
            now: 1,
            sessions: Vec::new(),
        });
        let launches = p.take_launches();
        assert!(!launches.is_empty(), "前提: 起動要求が出ている");
        let mut first: Option<(AgentId, SessionId)> = None;
        for (i, (owner, key, spec)) in launches.iter().enumerate() {
            p.ack_done(owner, key);
            let sid = 800 + i as SessionId;
            p.bind_session(owner, &spec.agent_id, sid, Some(format!("/logs/{sid}.log")));
            if first.is_none() {
                first = Some((spec.agent_id.clone(), sid));
            }
        }
        let (agent, sid) = first.expect("1 体は起きている");

        // ── 人が指示を出す ──────────────────────────────────────────────
        p.act(TeamAction::InstructAgent {
            agent: agent.clone(),
            text: "先にテストを書いて".into(),
        });
        let sent = p.take_manual_instructions();
        assert_eq!(sent.len(), 1, "人の指示が口まで出ていない: {sent:?}");
        let (owner, key, to, session, text) = sent[0].clone();
        assert_eq!(to, agent, "宛先が違う");
        assert_eq!(session, sid, "端末が違う");
        assert_eq!(text, "先にテストを書いて");
        assert!(key.starts_with("manual:"), "鍵の名前空間が違う: {key}");
        // **Runtime の指示の口には 1 通も漏れない。**
        assert!(
            p.take_instructions().is_empty(),
            "人の指示が Runtime 側の口へ紛れた"
        );

        let tag = p
            .delivery_tag(&owner, &key)
            .expect("Run があるので目印は作れる");

        // ── 届かなかった ────────────────────────────────────────────────
        assert_eq!(
            p.note_delivery(&tag, false),
            None,
            "人の指示をタスクの結末として返している"
        );
        assert!(
            !p.notice.trim().is_empty(),
            "届かなかったのに黙っている (画面にも記録にも残らない)"
        );

        // ── 届いた ──────────────────────────────────────────────────────
        p.notice.clear();
        p.act(TeamAction::InstructAgent {
            agent,
            text: "もう 1 つ".into(),
        });
        let sent = p.take_manual_instructions();
        let tag = p.delivery_tag(&sent[0].0, &sent[0].1).expect("目印");
        assert_eq!(p.note_delivery(&tag, true), None);
        assert!(p.notice.trim().is_empty(), "届いたのに苦情を出している");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// ── Run を閉じる: 停止の配送と保存の後始末 ───────────────────────────
    ///
    /// 閉じた Run の担当を止める命令が実行側へ届くこと、閉じた Run が
    /// 次の起動で復活しないことを、**実ファイル・実プロセス**で見る。
    mod close_run_lifecycle {
        use super::*;

        /// OS 標準のスリーパーで「確かに生きている子」を作る。unix は
        /// [`crate::procx::kill_tree`] がプロセスグループへ撃つので、実行側
        /// (`launch.rs` / `terminal.rs`) と同じく自分のグループで起こす。
        fn sleeper() -> std::process::Child {
            #[cfg(windows)]
            {
                crate::procx::hidden_command("ping")
                    .args(["-n", "30", "127.0.0.1"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("spawn ping")
            }
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                std::process::Command::new("sleep")
                    .arg("30")
                    .process_group(0)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("spawn sleep")
            }
        }

        /// 同じ home / workspace で「次の起動」を作る (前の画面の記憶は持たない)。
        fn reopened(p: &TeamPanel, dir: &Path) -> TeamPanel {
            let mut q = TeamPanel::default();
            q.home = p.home.clone();
            q.attach_workspace(dir).expect("新しい画面は attach できる");
            q
        }

        fn run_ids(p: &TeamPanel) -> Vec<String> {
            p.runs.iter().map(|r| r.run().run_id.clone()).collect()
        }

        fn finish_pending_stops(p: &mut TeamPanel) -> Vec<(RunOwner, String, SessionId)> {
            let stops = p.take_stops();
            for (owner, key, _) in stops.iter().cloned() {
                p.watch_stop(owner, key, None);
            }
            p.progress_close();
            stops
        }

        /// A (開始済み・担当にセッションを結んだ) と B (計画のみ) を保存した状態。
        /// 戻りは (画面, workspace, A, B, A に結んだセッション)。
        fn two_saved(tag: &str) -> (TeamPanel, PathBuf, RunOwner, RunOwner, Vec<SessionId>) {
            let dir = ws(tag);
            let mut p = started_panel(&dir);
            let a = p.owner().expect("A");
            let boot = p.take_launches();
            assert!(!boot.is_empty(), "A の起動要求が無い");
            let mut sids = Vec::new();
            for (i, (o, _, spec)) in boot.iter().enumerate() {
                let sid = 300 + i as SessionId;
                p.bind_session(o, &spec.agent_id, sid, None);
                sids.push(sid);
            }
            add_product_run(&mut p).expect("B");
            let b = p.owner().expect("B");
            p.save_if_needed();
            let state = p.state_dir();
            for o in [&a, &b] {
                assert!(
                    persistence::run_dir_in(&state, &o.run_id)
                        .expect("安全な ID")
                        .exists(),
                    "前提: {} の保存がある",
                    o.run_id
                );
            }
            (p, dir, a, b, sids)
        }

        #[test]
        fn dirty_closeは取消と保持を選べ成果物を消さない() {
            let dir = ws("close-dirty-keep");
            let mut p = started_panel(&dir);
            let owner = p.owner().unwrap();
            let worktree = owner.workspace.clone();
            let artifact = worktree.join("artifact.txt");
            std::fs::write(&artifact, "valuable\n").unwrap();

            p.close_run(0).expect("確認を開く");
            assert!(matches!(p.close_prompt, ClosePrompt::Confirm { .. }));
            assert!(p.has_run(), "確認前にRunを外した");
            assert!(artifact.exists(), "確認前に成果物を消した");

            p.close_run_cancel();
            assert_eq!(p.close_prompt, ClosePrompt::None);
            assert!(p.has_run(), "取消で状態を変えた");
            assert!(artifact.exists(), "取消で成果物を消した");

            p.close_run(0).expect("再確認を開く");
            p.close_run_keep();
            assert!(!p.has_run(), "保持を選んだRunの管理が閉じていない");
            assert!(artifact.exists(), "保持を選んだ成果物を消した");
            let record = persistence::close_record(&p.state_dir(), &owner.run_id).unwrap();
            assert_eq!(record.policy, persistence::ClosePolicy::Keep);
            assert_eq!(record.phase, persistence::ClosePhase::Cleanup);
            assert_eq!(record.artifact_path, worktree.display().to_string());
            assert!(!p.notice.is_empty(), "保持後の案内が無い");
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn 明示破棄はdirtyな対象runだけを削除する() {
            let dir = ws("close-explicit-discard");
            let mut p = started_panel(&dir);
            let a = p.owner().unwrap();
            std::fs::write(a.workspace.join("a-result.txt"), "a\n").unwrap();
            add_product_run(&mut p).unwrap();
            p.act(TeamAction::Start);
            let b = p.owner().unwrap();
            let pos = p.run_pos_of_owner(&a).unwrap();
            p.close_run(pos).unwrap();
            assert!(matches!(p.close_prompt, ClosePrompt::Confirm { .. }));
            p.close_run_discard();
            assert!(!a.workspace.exists(), "明示破棄したAが残った");
            assert!(b.workspace.exists(), "Bのworkspaceまで消した");
            assert_eq!(run_ids(&p), vec![b.run_id]);
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn セッション停止完了前と削除直前に増えた成果物は削除しない() {
            let dir = ws("close-stop-and-toctou");
            let mut p = started_panel(&dir);
            let owner = p.owner().unwrap();
            let (_, _, launch) = p.take_launches().into_iter().next().unwrap();
            p.bind_session(&owner, &launch.agent_id, 700, None);

            p.close_run(0).unwrap();
            let stops = p.take_stops();
            assert_eq!(stops.len(), 1);
            let (stop_owner, key, _) = stops[0].clone();
            let handle = crate::terminal::ReapHandle::for_test(false);
            p.watch_stop(stop_owner, key, Some(handle.clone()));
            p.progress_close();
            assert!(owner.workspace.exists(), "セッション停止前にworktreeを削除した");

            handle.finish_for_test();
            std::fs::write(owner.workspace.join("late.txt"), "late\n").unwrap();
            p.progress_close();
            assert!(matches!(p.close_prompt, ClosePrompt::Confirm { .. }));
            assert!(owner.workspace.join("late.txt").exists(), "競合成果物を削除した");
            assert!(p.has_run(), "再確認前にRunを外した");
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn runを閉じると全セッションの停止が実行側へ届く() {
            let (mut p, dir, a, b, sids) = two_saved("close-stops");
            // 閉じる前に積まれていた A の「停止以外」の仕事。閉じた後は実行しない。
            let stale_launch = {
                let mut q = started_panel(&ws("close-stops-stale"));
                q.take_launches().into_iter().next().expect("起動要求")
            };
            p.pending_launches
                .push((a.clone(), stale_launch.1.clone(), stale_launch.2.clone()));
            p.pending_instructions
                .push((a.clone(), "instr:x".into(), (1, sids[0], "hi".into())));
            let pos = p.run_pos_of_owner(&a).expect("A");
            p.close_run(pos).expect("A を閉じる");
            assert_eq!(run_ids(&p), vec![a.run_id.clone(), b.run_id.clone()]);
            // 実行側が停止完了を返すまではAもworktreeも保持する。
            let stops = finish_pending_stops(&mut p);
            for sid in &sids {
                assert!(
                    stops.iter().any(|(o, k, s)| o == &a && s == sid && k == &format!("stop:{sid}")),
                    "セッション #{sid} の停止が出ていない (プロセスが残る): {stops:?}"
                );
            }
            assert_eq!(stops.len(), sids.len(), "余分な停止が出た: {stops:?}");
            assert_eq!(run_ids(&p), vec![b.run_id.clone()], "停止完了後もAが残った");
            // 停止以外の古い仕事は実行しない (既存の保証はそのまま)。
            assert!(p.take_launches().is_empty(), "閉じた Run の起動を実行した");
            assert!(p.take_instructions().is_empty(), "閉じた Run の指示を送った");
            assert!(p.dropped_effects() > 0, "捨てたことを数えていない");
            // 消えた Run への ACK は落ちるだけで、残った Run を触らない。
            p.ack_done(&a, "stop:300");
            assert_eq!(run_ids(&p), vec![b.run_id.clone()]);
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn 片方を閉じても止めるのは閉じたrunのセッションだけ() {
            // **結ぶのは 2 本とも起動要求を取り終えてから。** 空の観測で tick を
            // 回すと、Runtime は結んだセッションを「消えた」と見て結び付きを外す
            // (それが正しい動き)。
            let dir = ws("close-only-mine");
            let mut p = started_panel(&dir);
            let a = p.owner().expect("A");
            let boot_a = p.take_launches();
            assert!(!boot_a.is_empty(), "A の起動要求が無い");
            add_product_run(&mut p).expect("B");
            let b_id = p.owner().expect("B").run_id;
            p.act(TeamAction::Start);
            let b = p.owner().expect("開始後のB");
            assert_eq!(b.run_id, b_id);
            p.pump(super::super::super::runtime::Observation {
                now: 2,
                sessions: Vec::new(),
            });
            let boot_b = p.take_launches();
            assert!(!boot_b.is_empty(), "B の起動要求が無い");
            let mut a_sids = Vec::new();
            for (i, (o, _, spec)) in boot_a.iter().enumerate() {
                assert_eq!(o, &a, "A の起動要求に B が混ざった");
                let sid = 300 + i as SessionId;
                p.bind_session(o, &spec.agent_id, sid, None);
                a_sids.push(sid);
            }
            let mut b_sids = Vec::new();
            for (i, (o, _, spec)) in boot_b.iter().enumerate() {
                assert_eq!(o, &b, "B の起動要求に A が混ざった");
                let sid = 400 + i as SessionId;
                p.bind_session(o, &spec.agent_id, sid, None);
                b_sids.push(sid);
            }
            assert!(!a_sids.is_empty() && !b_sids.is_empty(), "前提: 両方に担当が居る");
            let pos = p.run_pos_of_owner(&a).expect("A");
            p.close_run(pos).expect("A を閉じる");
            let stops = finish_pending_stops(&mut p);
            let mut got: Vec<SessionId> = stops.iter().map(|(_, _, s)| *s).collect();
            got.sort_unstable();
            let mut want = a_sids.clone();
            want.sort_unstable();
            assert_eq!(got, want, "止める相手が A のセッションと一致しない");
            for s in &b_sids {
                assert!(!got.contains(s), "B のセッション #{s} まで止めようとした");
            }
            assert!(stops.iter().all(|(o, _, _)| o == &a), "持ち主が A でない停止が混ざった");
            // B はそのまま動く。
            assert_eq!(run_ids(&p), vec![b.run_id.clone()]);
            std::fs::remove_dir_all(&dir).ok();
        }

        /// **実物の子プロセスを起こして、閉じたら落ちることを見る。**
        ///
        /// 実行側 (`app/team_glue.rs`) は `take_stops` で受けたセッションを
        /// `close_agent` で畳み、プロセスの木は [`crate::procx::kill_tree`] が
        /// 落とす。ここでは同じ順序をそのまま踏む: 起こす → 結ぶ → 閉じる →
        /// 取り出した停止のぶんだけ落とす。生き残りの検出は固定の sleep では
        /// なく、期限つきの `try_wait` で見る。
        #[test]
        #[allow(clippy::zombie_processes)]
        fn runを閉じると担当の実プロセスが終わる() {
            let dir = ws("close-kills-children");
            let mut p = started_panel(&dir);
            let boot = p.take_launches();
            assert!(!boot.is_empty(), "起動要求が無い");
            let owner = boot[0].0.clone();
            let mut children: Vec<(SessionId, std::process::Child)> = Vec::new();
            for (i, (o, _, spec)) in boot.iter().take(2).enumerate() {
                let sid = 500 + i as SessionId;
                p.bind_session(o, &spec.agent_id, sid, None);
                children.push((sid, sleeper()));
            }
            let pos = p.run_pos_of_owner(&owner).expect("Run");
            p.close_run(pos).expect("閉じる");
            let stops = p.take_stops();
            for (sid, child) in &mut children {
                assert!(
                    stops.iter().any(|(o, _, s)| o == &owner && s == sid),
                    "セッション #{sid} の停止が出ていない: {stops:?}"
                );
                assert!(
                    child.try_wait().expect("try_wait").is_none(),
                    "前提: 止める前に子が死んでいる"
                );
                // 実行側と同じ道具で落とす (生きていることを確かめてから)。
                crate::procx::kill_tree(child.id());
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            for (sid, child) in &mut children {
                loop {
                    if child.try_wait().expect("try_wait").is_some() {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "セッション #{sid} の子プロセスが期限内に終わらない"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn 閉じたrunは再起動後に復元されない() {
            let (mut p, dir, a, b, _) = two_saved("close-no-resurrect");
            let state = p.state_dir();
            let a_dir = persistence::run_dir_in(&state, &a.run_id).unwrap();
            let b_dir = persistence::run_dir_in(&state, &b.run_id).unwrap();
            p.notice.clear();
            let pos = p.run_pos_of_owner(&a).expect("A");
            p.close_run(pos).expect("A を閉じる");
            finish_pending_stops(&mut p);
            assert!(!a_dir.exists(), "A の保存が残っている");
            assert!(b_dir.exists(), "B の保存まで消した");
            assert!(
                !persistence::is_closed(&state, &a.run_id),
                "片付いたのに墓標が残っている"
            );
            assert_eq!(
                p.notice,
                crate::i18n::tr("team.notice.run_closed"),
                "成功以外の通知を出した"
            );
            // 次の起動: B だけが戻る。
            let mut q = reopened(&p, &dir);
            assert_eq!(q.restore, RestorePrompt::Found, "B の案内が出ない");
            q.restore_run(false).expect("B を復元");
            assert_eq!(run_ids(&q), vec![b.run_id.clone()], "閉じた A が復活した");
            // 最後の 1 本を閉じると根の控えも消え、案内そのものが出ない。
            q.close_run(0).expect("B を閉じる");
            finish_pending_stops(&mut q);
            assert!(!persistence::has_run(&state), "根の控えが残っている (復活の温床)");
            let r = reopened(&q, &dir);
            assert_eq!(r.restore, RestorePrompt::None, "閉じた Run を案内した");
            let mut r = r;
            assert!(r.restore_run(false).is_err(), "閉じた Run を復元した");
            assert!(r.runs.is_empty());
            std::fs::remove_dir_all(&dir).ok();
        }

        /// **保存を消せなくても、閉じた Run は復活しない。** 失敗は種類が分かる
        /// 形で帯に出て、次の起動が片付けを試し直す。
        #[test]
        fn 保存の削除に失敗しても閉じたrunは復元されない() {
            let (mut p, dir, a, b, _) = two_saved("close-delete-fails");
            let state = p.state_dir();
            let a_dir = persistence::run_dir_in(&state, &a.run_id).unwrap();
            persistence::fault_inject::fail_remove_under(&a_dir);
            p.notice.clear();
            let pos = p.run_pos_of_owner(&a).expect("A");
            p.close_run(pos).expect("A を閉じる");
            finish_pending_stops(&mut p);
            assert!(a_dir.exists(), "前提: 削除が失敗している");
            assert!(persistence::is_closed(&state, &a.run_id), "墓標が無い");
            // 帯には「保存」の失敗として出る (置き場の失敗とは別の文言)。
            let want = crate::i18n::trf(
                "team.notice.run_state_cleanup_failed",
                &[("run", a.run_id.clone()), ("e", "(テスト) 削除に失敗".into())],
            );
            assert_eq!(p.notice, want, "失敗が帯に出ていない / 種類が違う");
            assert_ne!(
                p.notice,
                crate::i18n::trf(
                    "team.notice.outbox_cleanup_failed",
                    &[("run", a.run_id.clone()), ("e", "(テスト) 削除に失敗".into())],
                ),
                "保存の失敗を置き場の失敗として出した"
            );
            // 失敗が続いている次の起動: A は戻らず、保存も墓標も残る。
            let mut q = reopened(&p, &dir);
            q.restore_run(false).expect("B を復元");
            assert_eq!(run_ids(&q), vec![b.run_id.clone()], "消せなかった A が復活した");
            assert!(a_dir.exists() && persistence::is_closed(&state, &a.run_id));
            // 消せるようになった次の起動: 片付け直して墓標も掃く。
            persistence::fault_inject::clear();
            let mut r = reopened(&q, &dir);
            r.restore_run(false).expect("B を復元");
            assert_eq!(run_ids(&r), vec![b.run_id.clone()]);
            assert!(!a_dir.exists(), "復元時に片付け直していない");
            assert!(
                !persistence::is_closed(&state, &a.run_id),
                "片付いたのに墓標が残っている"
            );
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn 無い保存を消すのはエラーにしない() {
            let (mut p, dir, a, _b, _) = two_saved("close-notfound");
            let state = p.state_dir();
            let a_dir = persistence::run_dir_in(&state, &a.run_id).unwrap();
            std::fs::remove_dir_all(&a_dir).unwrap();
            p.notice.clear();
            let pos = p.run_pos_of_owner(&a).expect("A");
            p.close_run(pos).expect("A を閉じる");
            finish_pending_stops(&mut p);
            assert_eq!(
                p.notice,
                crate::i18n::tr("team.notice.run_closed"),
                "NotFound を失敗として出した"
            );
            assert!(!persistence::is_closed(&state, &a.run_id), "墓標が残っている");
            std::fs::remove_dir_all(&dir).ok();
        }

        /// 中身が壊れていても、墓標が**有れば**閉じた扱い (安全側)。
        #[test]
        fn 壊れた墓標でも閉じた扱いになる() {
            let dir = ws("close-corrupt-tombstone");
            let mut p = started_panel(&dir);
            let a = p.owner().expect("A");
            p.save_if_needed();
            let state = p.state_dir();
            let a_dir = persistence::run_dir_in(&state, &a.run_id).unwrap();
            assert!(a_dir.exists());
            let pen = state.join(persistence::CLOSED_DIR);
            std::fs::create_dir_all(&pen).unwrap();
            std::fs::write(pen.join(&a.run_id), b"\xff{not json").unwrap();
            assert!(persistence::is_closed(&state, &a.run_id));
            let mut q = reopened(&p, &dir);
            assert_eq!(q.restore, RestorePrompt::None, "閉じた Run を案内した");
            assert!(q.restore_run(false).is_err(), "墓標のある Run を復元した");
            assert!(q.runs.is_empty());
            // 壊れた墓標から削除方針を推測しない。保存と診断材料を残す。
            assert!(a_dir.exists(), "方針不明なのに保存を消した");
            assert!(persistence::is_closed(&state, &a.run_id), "壊れた墓標を消した");
            std::fs::remove_dir_all(&dir).ok();
        }

        /// 不正な `run_id` は保存も削除も**状態ディレクトリの外へ出ない**。
        #[test]
        fn 不正なrun_idでは状態ディレクトリの外へ書きも消しもしない() {
            let dir = ws("bad-run-id-io");
            std::fs::create_dir_all(&dir).unwrap();
            let mut p = panel_at(&dir);
            let state = p.state_dir();
            std::fs::create_dir_all(state.join(RUNS_DIR)).unwrap();
            let canaries = [
                dir.join("workspace-canary.txt"),
                state.join("state-canary.txt"),
                state.join(RUNS_DIR).join("runs-canary.txt"),
            ];
            for c in &canaries {
                std::fs::write(c, "x").unwrap();
            }
            let entries = |d: &Path| -> Vec<String> {
                let mut v: Vec<String> = std::fs::read_dir(d)
                    .map(|rd| {
                        rd.filter_map(|e| e.ok())
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                v.sort();
                v
            };
            let before_state = entries(&state);
            let before_runs = entries(&state.join(RUNS_DIR));
            for bad in ["", ".", "..", "../x", "/abs", "a/b", "a\\b", "C:x"] {
                let err = p
                    .plan(
                    SPEC,
                    "SPEC.md",
                    RunOptions {
                        run_id: bad.to_string(),
                        ..RunOptions::default()
                    },
                )
                    .expect_err("不正IDを開始前に拒否する");
                assert!(err.contains("run_id"), "理由が分からない: {err}");
                for c in &canaries {
                    assert!(c.exists(), "{bad:?} で想定外の場所を消した: {}", c.display());
                }
                assert!(!state.join("x").exists(), "{bad:?} で runs/ の外へ書いた");
                assert_eq!(
                    entries(&state.join(RUNS_DIR)),
                    before_runs,
                    "{bad:?} で runs/ に何か作った"
                );
            }
            // 根の控えは正当な置き場 (書かれてよい)。それ以外は増えていない。
            let after_state: Vec<String> = entries(&state)
                .into_iter()
                .filter(|n| {
                    n != persistence::STATE_FILE
                        && n != persistence::PREV_FILE
                        && n != persistence::CLOSED_DIR
                })
                .collect();
            assert_eq!(after_state, before_state, "状態ディレクトリに想定外のものが増えた");
            std::fs::remove_dir_all(&dir).ok();
        }

        /// 復元は同時実行の上限を超えず、既に持っている Run と重複せず、
        /// フォルダ名と中身の `run_id` が食い違う保存を読まない。
        #[test]
        fn 復元は上限と重複と名前の食い違いを守る() {
            let dir = ws("restore-cap-dedupe");
            std::fs::create_dir_all(&dir).unwrap();
            let mut p = panel_at(&dir);
            p.plan(SPEC, "SPEC.md", RunOptions::default()).expect("計画");
            for _ in 1..4 {
                add_product_run(&mut p).expect("製品経路の複数Run");
            }
            p.save_if_needed();
            let state = p.state_dir();
            // 5 本目 (名前は末尾に並ぶ) と、中身の run_id が別物のフォルダ。
            let mut extra = p.runs[0].to_saved();
            extra.run.run_id = "zz-extra".to_string();
            persistence::save(&persistence::run_dir_in(&state, "zz-extra").unwrap(), &extra)
                .expect("保存");
            persistence::save(
                &persistence::run_dir_in(&state, "zz-mismatch").unwrap(),
                &extra,
            )
            .expect("保存");
            let mut q = reopened(&p, &dir);
            q.restore_run(false).expect("復元");
            assert_eq!(q.runs.len(), MAX_CONCURRENT_RUNS, "上限を超えて復元した");
            let ids = run_ids(&q);
            assert!(!ids.iter().any(|i| i == "zz-mismatch"), "名前の食い違う保存を復元した");
            assert!(
                persistence::run_dir_in(&state, "zz-mismatch").unwrap().exists(),
                "読まなかった保存を消した"
            );
            assert!(
                persistence::run_dir_in(&state, "zz-extra").unwrap().exists(),
                "上限で残した保存を消した"
            );
            // もう一度復元しても増えない (重複しない)。
            let _ = q.restore_run(false);
            assert_eq!(q.runs.len(), MAX_CONCURRENT_RUNS, "同じ Run を二重に持った");
            let mut uniq = run_ids(&q);
            uniq.sort();
            uniq.dedup();
            assert_eq!(uniq.len(), MAX_CONCURRENT_RUNS);
            // 上限で残した旧 Run は、先の1本を閉じれば次に復元できる。
            let first = q.close_run(0).expect("先のRunを閉じる");
            q.restore_run(false).expect("残したRunを復元");
            assert_eq!(q.runs.len(), MAX_CONCURRENT_RUNS);
            assert_ne!(q.owner().expect("次のRun").run_id, first);
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn 復元の保存列挙は上限で打ち切る() {
            let dir = ws("restore-scan-cap");
            let mut p = panel_at(&dir);
            let runs = p.state_dir().join(RUNS_DIR);
            std::fs::create_dir_all(&runs).unwrap();
            for n in 0..=RESTORE_SCAN_MAX {
                std::fs::create_dir(runs.join(format!("run-{n:04}"))).unwrap();
            }

            assert!(p.restore_run(false).is_err(), "中身の無い保存を復元した");
            assert_eq!(
                p.notice, "team.notice.restore_skipped",
                "打ち切りの通知経路を通っていない"
            );
            for (lang, _, json) in crate::locale::BUILTIN {
                let dict: std::collections::BTreeMap<String, String> =
                    serde_json::from_str(json).expect("同梱辞書は JSON");
                let text = dict
                    .get("team.notice.restore_skipped")
                    .unwrap_or_else(|| panic!("{lang} に復元skipの訳が無い"));
                assert!(
                    text.contains("{why}"),
                    "{lang}: 打ち切り理由を表示できない: {text}"
                );
            }
            std::fs::remove_dir_all(&dir).ok();
        }

        /// 破棄でも同じ不変条件: 担当へ停止が届き、消せなくても復活しない。
        #[test]
        fn 破棄で保存を消せなくても復活しない() {
            let dir = ws("discard-delete-fails");
            let mut p = started_panel(&dir);
            let a = p.owner().expect("A");
            let boot = p.take_launches();
            let mut sids = Vec::new();
            for (i, (o, _, spec)) in boot.iter().enumerate() {
                let sid = 700 + i as SessionId;
                p.bind_session(o, &spec.agent_id, sid, None);
                sids.push(sid);
            }
            assert!(!sids.is_empty(), "前提: 担当にセッションが結び付いている");
            p.save_if_needed();
            let state = p.state_dir();
            persistence::fault_inject::fail_remove_under(&state.join(RUNS_DIR));
            let err = p.discard_run().expect_err("削除の失敗を返す");
            // 画面へ出る文字列なので `trf` を通る (テストでは辞書が載っていない
            // ので ID がそのまま返る)。**理由が人へ届くこと**は、同梱辞書の
            // ひな型が `{e}` を持っていることで確かめる — 持っていなければ、
            // 訳した瞬間に原因が消える。
            assert_eq!(
                err,
                crate::i18n::trf(
                    "team.notice.run_state_cleanup_failed",
                    &[("run", a.run_id.clone()), ("e", "(テスト) 削除に失敗".into())],
                ),
                "失敗の伝え方が変わっている"
            );
            for (lang, _, json) in crate::locale::BUILTIN {
                let dict: std::collections::BTreeMap<String, String> =
                    serde_json::from_str(json).expect("同梱辞書は JSON");
                let t = dict
                    .get("team.notice.run_state_cleanup_failed")
                    .unwrap_or_else(|| panic!("{lang} に訳が無い"));
                assert!(t.contains("{e}"), "{lang}: 原因 ({{e}}) が訳から落ちている: {t}");
                assert!(t.contains("{run}"), "{lang}: どの Run か分からない: {t}");
            }
            assert!(persistence::is_closed(&state, &a.run_id), "墓標が無い");
            assert!(!p.has_run());
            // **捨てた Run の担当も止める。** 記録だけ消して担当を残すと、
            // 盤面から消えたのに端末では動き続ける。
            let stops = p.take_stops();
            for sid in &sids {
                assert!(
                    stops.iter().any(|(_, _, s)| s == sid),
                    "破棄したのにセッション #{sid} の停止が出ていない: {stops:?}"
                );
            }
            persistence::fault_inject::clear();
            let mut q = reopened(&p, &dir);
            assert_eq!(q.restore, RestorePrompt::None, "破棄した Run を案内した");
            assert!(q.restore_run(false).is_err(), "破棄した Run を復元した");
            assert!(
                !persistence::run_dir_in(&state, &a.run_id).unwrap().exists(),
                "次の起動で片付け直していない"
            );
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    /// ── 報告置き場 (outbox) ────────────────────────────────────────────
    ///
    /// **実ファイル・実 Runtime・実セッション ID** で振る舞いを見る
    /// (ソースの文字列を読むだけの番人にしない)。「届いた」は、居ない
    /// タスク番号を名指しした報告が**その Run にだけ** `#<番号>` を含む却下を
    /// 1 件残すことで見る — 解析器を通らなければ事象は 1 件も増えないので、
    /// 空回りしない検査になる。
    mod outbox_delivery {
        use super::*;

        /// 1 本の Run の、置き場テストに要るもの。
        struct Lane {
            owner: RunOwner,
            /// この Run の置き場 (`outbox/<run_id>/`)。
            dir: PathBuf,
            /// `team-lead` のセッション。
            sid: SessionId,
            /// この Run に結び付けた全セッション。**観測には毎回これを全部載せる**
            /// — 載っていないセッションは Runtime が「消えた」と扱って
            /// 結び付きを外す (それが正しい動き)。
            all: Vec<SessionId>,
        }

        /// 起動要求の担当へセッションを結び、その Run の [`Lane`] を返す。
        fn bind_lane(
            p: &mut TeamPanel,
            boot: &[(RunOwner, String, super::super::super::runtime::AgentLaunchSpec)],
            first_sid: SessionId,
        ) -> Lane {
            assert!(!boot.is_empty(), "起動要求が無い");
            let owner = boot[0].0.clone();
            let mut sid = first_sid;
            let mut all = Vec::new();
            for (o, _, spec) in boot {
                assert_eq!(o, &owner, "1 回の起動要求に複数の Run が混ざった");
                p.bind_session(o, &spec.agent_id, sid, None);
                all.push(sid);
                sid += 1;
            }
            let pos = p.run_pos_of_owner(&owner).expect("Run の位置");
            let rt = &p.runs[pos];
            let lead = rt
                .agents()
                .iter()
                .find(|a| a.id.as_str() == "team-lead")
                .expect("team-lead が居る");
            assert!(!rt.outbox().as_os_str().is_empty(), "置き場が決まっていない");
            std::fs::create_dir_all(rt.outbox()).expect("置き場を作れる");
            Lane {
                owner,
                dir: rt.outbox().to_path_buf(),
                sid: lead.session_id.expect("team-lead にセッション"),
                all,
            }
        }

        /// 指定数の Run を立て、各担当に重ならないセッションを結ぶ。
        fn many_lanes(tag: &str, count: usize) -> (TeamPanel, PathBuf, Vec<Lane>) {
            assert!((1..=4).contains(&count));
            let dir = ws(tag);
            let mut p = started_panel(&dir);
            let mut boots = vec![p.take_launches()];
            for i in 1..count {
                add_product_run(&mut p)
                    .unwrap_or_else(|e| panic!("{} 本目: {e}", i + 1));
                p.act(TeamAction::Start);
                p.pump(super::super::super::runtime::Observation {
                    now: 2 + i as u64,
                    sessions: Vec::new(),
                });
                boots.push(p.take_launches());
            }
            let mut lanes = Vec::with_capacity(count);
            for (i, boot) in boots.iter().enumerate() {
                lanes.push(bind_lane(&mut p, boot, 101 + i as SessionId * 100));
            }
            let owners: HashSet<String> =
                lanes.iter().map(|l| l.owner.run_id.clone()).collect();
            let dirs: HashSet<PathBuf> = lanes.iter().map(|l| l.dir.clone()).collect();
            assert_eq!(owners.len(), count, "Run の持ち主が衝突した");
            assert_eq!(dirs.len(), count, "outbox が衝突した");
            (p, dir, lanes)
        }

        /// 2 本の Run を立て、両方の担当にセッションを結ぶ (A は 101〜、B は 201〜)。
        /// **どちらにも `team-lead` が居る** — ID が同じでも混線しないことを見るため。
        fn two_lanes(tag: &str) -> (TeamPanel, PathBuf, Lane, Lane) {
            let (p, dir, mut lanes) = many_lanes(tag, 2);
            let b = lanes.pop().unwrap();
            let a = lanes.pop().unwrap();
            assert_ne!(a.owner, b.owner, "同じ Run になっている");
            assert_ne!(a.dir, b.dir, "置き場が Run ごとに分かれていない");
            assert_ne!(a.sid, b.sid);
            (p, dir, a, b)
        }

        fn finish_close(p: &mut TeamPanel) {
            let stops = p.take_stops();
            for (owner, key, _) in stops {
                p.watch_stop(owner, key, None);
            }
            p.progress_close();
        }

        /// 観測 (画面は空。置き場の報告だけが流れる)。渡した Run の
        /// **全セッション**を載せる。
        fn rows(lanes: &[&Lane]) -> Vec<SessionInput> {
            lanes
                .iter()
                .flat_map(|l| l.all.iter().copied())
                .map(|id| SessionInput {
                    id,
                    title: "a".into(),
                    provider: "claude".into(),
                    state: crate::coordinator::SessionState::Idle,
                    tail: Vec::new(),
                })
                .collect()
        }

        /// 居ないタスク番号を名指しした報告。届いた Run にだけ `#<task>` を
        /// 含む却下が記録される。
        fn report(agent: &str, task: u64) -> String {
            format!(
                "{{\"task_id\": {task}, \"agent_id\": \"{agent}\", \"status\": \"completed\", \
                 \"summary\": \"置き場から\", \"changed_files\": [], \"validation\": [], \
                 \"blockers\": []}}"
            )
        }

        /// その Run の記録に `needle` を含む事象がいくつあるか。
        ///
        /// **隔離の覚え書きは数えない。** 却下された報告は「Runtime が
        /// 却下した 1 行」と「どこへ隔離したかの 1 行」の 2 つを残すので、
        /// 素朴に数えると 1 通の報告が 2 通に見える。数えたいのは
        /// 「解析器へ届いたか」なので、Runtime 側の 1 行だけを見る。
        fn saw(p: &TeamPanel, owner: &RunOwner, needle: &str) -> usize {
            let pos = p.run_pos_of_owner(owner).expect("Run");
            p.runs[pos]
                .events()
                .filter(|e| !e.summary.starts_with("報告ファイル "))
                .filter(|e| e.summary.contains(needle))
                .count()
        }

        /// 取り決めどおりの提出: 一時ファイルへ書き切ってから改名する。
        fn submit(dir: &Path, agent: &str, unique: &str, body: &str) -> PathBuf {
            let tmp = dir.join(outbox::tmp_name(agent, unique));
            let fin = dir.join(outbox::final_name(agent, unique));
            std::fs::write(&tmp, body).unwrap();
            std::fs::rename(&tmp, &fin).unwrap();
            fin
        }

        /// 隔離の覚え書きの数 (`saw` が数えないほう)。
        fn quarantined(p: &TeamPanel, owner: &RunOwner, name: &str) -> usize {
            let pos = p.run_pos_of_owner(owner).expect("Run");
            p.runs[pos]
                .events()
                .filter(|e| e.summary.starts_with("報告ファイル "))
                .filter(|e| e.summary.contains(name))
                .count()
        }

        /// 提出の包み (`outbox::judge` が読む形)。
        fn envelope(run_id: &str, agent: &str, kind: &str, payload: &str) -> String {
            format!(
                r#"{{"kind": "{kind}", "run_id": "{run_id}", "agent_id": "{agent}", "payload": {payload}}}"#
            )
        }

        fn name_of(f: &Path) -> String {
            f.file_name().unwrap().to_str().unwrap().to_string()
        }

        #[test]
        fn 書きかけの一時ファイルは読まない() {
            let (mut p, dir, a, b) = two_lanes("outbox-tmp");
            // 中身は完全な JSON でも、`.json.tmp` のうちは提出ではない
            let tmp = a.dir.join(outbox::tmp_name("team-lead", "1"));
            std::fs::write(&tmp, report("team-lead", 99)).unwrap();
            for now in 100..105 {
                p.pump_sessions(rows(&[&a, &b]), now);
            }
            assert!(tmp.exists(), "一時ファイルを消した");
            assert_eq!(saw(&p, &a.owner, "#99"), 0, "一時ファイルを報告として取り込んだ");
            assert!(
                p.outbox_ledger.tracked().is_empty(),
                "一時ファイルを読み直しの台帳に載せた"
            );
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn 改名で公開した報告は一度だけ取り込む() {
            let (mut p, dir, a, b) = two_lanes("outbox-rename-once");
            let fin = submit(&a.dir, "team-lead", "1", &report("team-lead", 99));
            p.pump_sessions(rows(&[&a, &b]), 100);
            assert_eq!(saw(&p, &a.owner, "#99"), 1, "改名した報告が届かない");
            assert!(!fin.exists(), "取り込んだのに消していない");
            // もう一度回しても増えない
            p.pump_sessions(rows(&[&a, &b]), 101);
            assert_eq!(saw(&p, &a.owner, "#99"), 1, "同じ報告を二度取り込んだ");
            assert_eq!(saw(&p, &b.owner, "#99"), 0, "別の Run へ流れた");
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn 受理後の削除失敗を再処理しても状態を二重更新しない() {
            let (mut p, dir, a, b) = two_lanes("outbox-remove-retry");
            let event = r#"{"kind":"sub_agent_started","agent_id":"remove-retry-child","parent_id":"team-lead","role":"tester","action":"一度だけ"}"#;
            let file = submit(
                &a.dir,
                "team-lead",
                "remove-fail",
                &envelope(&a.owner.run_id, "team-lead", "event", event),
            );
            outbox::fault_inject::fail_remove_once();
            p.pump_sessions(rows(&[&a, &b]), 100);
            assert!(!file.exists(), "原本をprocessingへ確保していない");
            assert!(
                outbox::list_reports(&a.dir)
                    .iter()
                    .any(|path| {
                        path.parent()
                            .and_then(|slot| slot.parent())
                            .and_then(|parent| parent.file_name())
                            .is_some_and(|name| name == outbox::PROCESSING_DIR)
                    }),
                "削除失敗なのに確保済み報告が残っていない"
            );
            let count = |panel: &TeamPanel| {
                let pos = panel.run_pos_of_owner(&a.owner).expect("A");
                panel.runs[pos]
                    .agents()
                    .iter()
                    .filter(|agent| agent.id.as_str() == "remove-retry-child")
                    .count()
            };
            assert_eq!(count(&p), 1, "初回の状態反映が無い");

            // 再起動しても永続化済みseen台帳が二重反映を止める。
            let mut q = TeamPanel::default();
            q.home = p.home.clone();
            q.attach_workspace(&dir).unwrap();
            q.restore_run(false).unwrap();
            q.pump_sessions(Vec::new(), 101);
            let pos = q.run_pos_of_owner(&a.owner).expect("復元したA");
            assert_eq!(
                q.runs[pos]
                    .agents()
                    .iter()
                    .filter(|agent| agent.id.as_str() == "remove-retry-child")
                    .count(),
                1,
                "再起動後の削除再試行で状態を二重更新した"
            );
            assert!(outbox::list_reports(&a.dir).is_empty(), "受理済み報告を片付けていない");
            std::fs::remove_dir_all(&dir).ok();
        }

        /// 旧手順のエージェント (`.json` へ直接書く) が書いている途中を読んでも、
        /// その報告を失わない。
        #[test]
        fn 壊れたjsonは最初の読み取りで消えない() {
            let (mut p, dir, a, b) = two_lanes("outbox-broken-keep");
            let full = report("team-lead", 99);
            let f = a.dir.join(outbox::final_name("team-lead", "1"));
            let mut writer = std::fs::File::create(&f).unwrap();
            use std::io::Write as _;
            writer.write_all(&full.as_bytes()[..full.len() / 2]).unwrap();
            writer.flush().unwrap();
            p.pump_sessions(rows(&[&a, &b]), 100);
            assert!(
                !outbox::list_reports(&a.dir).is_empty(),
                "書きかけを 1 回目で失った"
            );
            assert_eq!(saw(&p, &a.owner, "#99"), 0, "半分の JSON を解析器へ渡した");
            assert_eq!(
                saw(&p, &a.owner, "取り込めません"),
                0,
                "1 回読めなかっただけで却下を記録した"
            );
            // 書き終わったら、次の tick で届く
            writer.write_all(&full.as_bytes()[full.len() / 2..]).unwrap();
            writer.flush().unwrap();
            p.pump_sessions(rows(&[&a, &b]), 101);
            assert_eq!(saw(&p, &a.owner, "#99"), 1, "書き終わった報告が届かない");
            assert!(!f.exists(), "届いたのに消していない");
            assert!(
                p.outbox_ledger.tracked().is_empty(),
                "片付いたのに台帳に残っている"
            );
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn 壊れたjsonは上限で隔離され理由が一度だけ残る() {
            let (mut p, dir, a, b) = two_lanes("outbox-broken-limit");
            let f = a.dir.join(outbox::final_name("team-lead", "1"));
            std::fs::write(&f, "{\"task_id\": 99, \"agent_id\": \"team-lead\"").unwrap();
            let name = name_of(&f);
            for i in 0..(outbox::MAX_ATTEMPTS - 1) {
                p.pump_sessions(rows(&[&a, &b]), 100 + u64::from(i));
                assert!(
                    !outbox::list_reports(&a.dir).is_empty(),
                    "{} 回目で消した (上限は {})",
                    i + 1,
                    outbox::MAX_ATTEMPTS
                );
            }
            assert_eq!(quarantined(&p, &a.owner, &name), 0, "上限の手前で理由を出した");
            p.pump_sessions(rows(&[&a, &b]), 200);
            assert!(!f.exists(), "上限に達したのに置き場に残っている");
            let pen = a.dir.join(outbox::REJECTED_DIR).join(&name);
            assert!(pen.exists(), "隔離先に無い: {}", pen.display());
            assert_eq!(quarantined(&p, &a.owner, &name), 1, "理由が記録されていない");
            assert_eq!(saw(&p, &a.owner, "#99"), 0, "壊れた報告を解析器へ渡した");
            // 隔離したものは二度と読まない・言い直さない
            for now in 201..205 {
                p.pump_sessions(rows(&[&a, &b]), now);
            }
            assert_eq!(
                quarantined(&p, &a.owner, &name),
                1,
                "隔離したファイルを言い直した"
            );
            assert!(pen.exists(), "隔離したファイルが消えた");
            assert!(p.outbox_ledger.tracked().is_empty(), "隔離したのに台帳に残っている");
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn 同じidの担当が居る二本のrunで報告が混線しない() {
            let (mut p, dir, a, b) = two_lanes("outbox-two-runs");
            for lane in [&a, &b] {
                let pos = p.run_pos_of_owner(&lane.owner).unwrap();
                assert!(
                    p.runs[pos].agents().iter().any(|x| x.id.as_str() == "team-lead"),
                    "前提: どちらの Run にも team-lead が居る"
                );
            }
            submit(&a.dir, "team-lead", "1", &report("team-lead", 98));
            submit(&b.dir, "team-lead", "1", &report("team-lead", 99));
            p.pump_sessions(rows(&[&a, &b]), 100);
            assert_eq!(saw(&p, &a.owner, "#98"), 1, "A の報告が A に届かない");
            assert_eq!(saw(&p, &b.owner, "#99"), 1, "B の報告が B に届かない");
            assert_eq!(saw(&p, &a.owner, "#99"), 0, "B の報告が A へ流れた");
            assert_eq!(saw(&p, &b.owner, "#98"), 0, "A の報告が B へ流れた");
            std::fs::remove_dir_all(&dir).ok();
        }

        /// 配送先は**セッション**で決まる。B のセッションしか観測に無い tick では
        /// A の報告は消さずに残し、A のセッションが観測に載った tick で届く。
        /// A を閉じても B への配送は続く。
        #[test]
        fn 報告はそれぞれのrunのセッションへ届く() {
            let (mut p, dir, a, b) = two_lanes("outbox-per-session");
            let fa = submit(&a.dir, "team-lead", "1", &report("team-lead", 98));
            let fb = submit(&b.dir, "team-lead", "1", &report("team-lead", 99));
            // **セッションの観測に依存しない。** B のセッションしか観測に
            // 載せていなくても、A の報告は A の Run へ届く (画面経由なら
            // ここで落ちる — 置き場は担当 ID だけで配るので落ちない)。
            p.pump_sessions(rows(&[&b]), 100);
            assert_eq!(saw(&p, &b.owner, "#99"), 1, "B の報告が B へ届かない");
            assert_eq!(saw(&p, &a.owner, "#98"), 1, "観測に無いだけで A の報告を落とした");
            assert_eq!(saw(&p, &b.owner, "#98"), 0, "A の報告が B へ流れた");
            assert_eq!(saw(&p, &a.owner, "#99"), 0, "B の報告が A へ流れた");
            assert!(!fa.exists() && !fb.exists(), "受理したのに消していない");
            // A を閉じても B の配送は続く
            let pos_a = p.run_pos_of_owner(&a.owner).unwrap();
            p.close_run(pos_a).expect("A を閉じる");
            finish_close(&mut p);
            let fb2 = submit(&b.dir, "team-lead", "2", &report("team-lead", 97));
            p.pump_sessions(rows(&[&b]), 102);
            assert_eq!(saw(&p, &b.owner, "#97"), 1, "A を閉じたら B へ届かなくなった");
            assert!(!fb2.exists());
            std::fs::remove_dir_all(&dir).ok();
        }

        /// **セッションが 1 度も結び付いていなくても、報告は落ちない。**
        ///
        /// 画面経由は「観測に載っているセッション」からしか拾えないので、
        /// 結び付く前・プロセスが終わった直後に書かれた報告を必ず落とす。
        /// 8 秒 (`MAX_ATTEMPTS` × 走査間隔) で隔離してもいけない。
        #[test]
        fn 未bindでもプロセスが終わっていても報告は失われない() {
            let dir = ws("outbox-unbound");
            let mut p = started_panel(&dir);
            let owner = p.owner().expect("Run");
            let boot = p.take_launches();
            let spec = &boot[0].2;
            let pos = p.run_pos_of_owner(&owner).expect("Run");
            let out = p.runs[pos].outbox().to_path_buf();
            std::fs::create_dir_all(&out).unwrap();
            // **一度も bind していない担当**の報告。
            assert!(
                p.runs[pos]
                    .agents()
                    .iter()
                    .all(|x| x.session_id.is_none()),
                "前提: まだ誰にもセッションが結び付いていない"
            );
            let f = submit(&out, spec.agent_id.as_str(), "1", &report(spec.agent_id.as_str(), 96));
            // 観測は空 (プロセスが終わった直後と同じ状況)。
            p.pump_sessions(Vec::new(), 100);
            assert_eq!(saw(&p, &owner, "#96"), 1, "未 bind の報告を落とした");
            assert!(!f.exists(), "受理したのに消していない");
            // **8 秒ぶん回しても隔離されない** (残っていないので当然だが、
            // 隔離の記録が 1 件も出ないことまで見る)。
            for i in 0..(outbox::MAX_ATTEMPTS + 5) {
                p.pump_sessions(Vec::new(), 101 + u64::from(i));
            }
            assert_eq!(
                saw(&p, &owner, "取り込めません"),
                0,
                "正しい報告を隔離した"
            );
            std::fs::remove_dir_all(&dir).ok();
        }

        /// **伝言と出来事も置き場から届き、二度は効かない。**
        #[test]
        fn 伝言と出来事も置き場から一度だけ届く() {
            let (mut p, dir, a, b) = two_lanes("outbox-msg-event");
            let pos = p.run_pos_of_owner(&a.owner).expect("A");
            let others: Vec<String> = p.runs[pos]
                .agents()
                .iter()
                .map(|x| x.id.0.clone())
                .filter(|id| id != "team-lead")
                .collect();
            let to = others.first().cloned().expect("宛先");
            // 伝言
            let msg = format!(r#"{{"to": "{to}", "text": "レビューお願いします"}}"#);
            let fm = submit(
                &a.dir,
                "team-lead",
                "m1",
                &envelope(&a.owner.run_id, "team-lead", "message", &msg),
            );
            // 出来事 (送り主は parent_id)
            let ev = r#"{"kind": "sub_agent_started", "agent_id": "child-9", "parent_id": "team-lead", "role": "implementer", "action": "調査中"}"#;
            let fe = submit(
                &a.dir,
                "team-lead",
                "e1",
                &envelope(&a.owner.run_id, "team-lead", "event", ev),
            );
            p.pump_sessions(rows(&[&a, &b]), 300);
            assert!(!fm.exists() && !fe.exists(), "受理したのに消していない");
            let msgs = |p: &TeamPanel, o: &RunOwner| -> usize {
                let pos = p.run_pos_of_owner(o).expect("Run");
                p.runs[pos]
                    .events()
                    .filter(|e| e.kind == TeamEventKind::AgentMessage)
                    .count()
            };
            let subs = |p: &TeamPanel, o: &RunOwner| -> usize {
                let pos = p.run_pos_of_owner(o).expect("Run");
                p.runs[pos]
                    .agents()
                    .iter()
                    .filter(|x| x.id.as_str() == "child-9")
                    .count()
            };
            assert_eq!(msgs(&p, &a.owner), 1, "伝言が A に届いていない");
            assert_eq!(subs(&p, &a.owner), 1, "サブエージェントが A に並んでいない");
            assert_eq!(msgs(&p, &b.owner), 0, "伝言が B へ流れた");
            assert_eq!(subs(&p, &b.owner), 0, "出来事が B へ流れた");
            // **同じ中身のファイルが 2 つあっても 1 回しか効かない**
            // (塊の指紋で落ちる)。両方ともその場で片付く。
            let m2 = submit(
                &a.dir,
                "team-lead",
                "m2",
                &envelope(&a.owner.run_id, "team-lead", "message", &msg),
            );
            let m3 = submit(
                &a.dir,
                "team-lead",
                "m3",
                &envelope(&a.owner.run_id, "team-lead", "message", &msg),
            );
            p.pump_sessions(rows(&[&a, &b]), 301);
            assert!(!m2.exists() && !m3.exists(), "重複を片付けていない");
            assert_eq!(
                msgs(&p, &a.owner),
                2,
                "同じ中身のファイル 2 つで 2 回反映した (1 回だけ効くべき)"
            );
            std::fs::remove_dir_all(&dir).ok();
        }

        /// **意味として却下された報告は、消さずに隔離する。**
        ///
        /// 前は `accept_outbox` が無条件に `Ok` を返していたので、居ないタスク
        /// への報告も「受理した」ことにしてファイルごと消えていた。書いた本人も
        /// 人も、**何を書いたのか確かめる手立てが無くなる**。
        #[test]
        fn 却下された報告は消えずに隔離される() {
            let (mut p, dir, a, b) = two_lanes("outbox-rejected");
            let pos = p.run_pos_of_owner(&a.owner).unwrap();
            let other = p.runs[pos]
                .agents()
                .iter()
                .map(|x| x.id.0.clone())
                .find(|id| id != "team-lead")
                .expect("別の担当");
            // 却下される 4 通 (種別ごとに 1 つずつ)。
            let cases: Vec<(&str, String)> = vec![
                // 居ないタスクへの完了報告
                ("nores", report("team-lead", 99)),
                // レビュー担当でないのにレビュー判定
                (
                    "norev",
                    envelope(
                        &a.owner.run_id,
                        "team-lead",
                        "review",
                        r#"{"task_id": 1, "verdict": "APPROVE", "findings": []}"#,
                    ),
                ),
                // 居ない宛先への伝言
                (
                    "nomsg",
                    envelope(
                        &a.owner.run_id,
                        "team-lead",
                        "message",
                        r#"{"to": "who-is-this", "text": "やあ"}"#,
                    ),
                ),
                // 表に無い種別の出来事
                (
                    "noev",
                    envelope(
                        &a.owner.run_id,
                        "team-lead",
                        "event",
                        r#"{"kind": "sub_agent_started", "agent_id": "x-1", "parent_id": "someone-else"}"#,
                    ),
                ),
            ];
            let mut files = Vec::new();
            for (tag, body) in &cases {
                files.push(submit(&a.dir, "team-lead", tag, body));
            }
            p.pump_sessions(rows(&[&a, &b]), 700);
            for f in &files {
                let name = name_of(f);
                assert!(!f.exists(), "{name} が置き場に残っている");
                assert!(
                    a.dir.join(outbox::REJECTED_DIR).join(&name).exists(),
                    "{name} が隔離されていない (黙って消えた)"
                );
                assert_eq!(
                    quarantined(&p, &a.owner, &name),
                    1,
                    "{name} の理由が残っていない"
                );
            }
            // 却下は B へ流れない。
            for f in &files {
                assert_eq!(quarantined(&p, &b.owner, &name_of(f)), 0, "別 Run へ流れた");
            }
            let _ = other;
            std::fs::remove_dir_all(&dir).ok();
        }

        /// **却下された本文は重複台帳へ入らないので、直せば通る。**
        #[test]
        fn 却下された報告は直して出し直せる() {
            let (mut p, dir, a, b) = two_lanes("outbox-resend");
            // 1) 表に無い親を名乗る出来事 → 却下。
            let bad = envelope(
                &a.owner.run_id,
                "team-lead",
                "event",
                r#"{"kind": "sub_agent_started", "agent_id": "fix-1", "parent_id": "nobody"}"#,
            );
            let f1 = submit(&a.dir, "team-lead", "v1", &bad);
            p.pump_sessions(rows(&[&a, &b]), 800);
            assert!(a.dir.join(outbox::REJECTED_DIR).join(name_of(&f1)).exists());
            let subs = |p: &TeamPanel| -> usize {
                let pos = p.run_pos_of_owner(&a.owner).expect("Run");
                p.runs[pos]
                    .agents()
                    .iter()
                    .filter(|x| x.id.as_str() == "fix-1")
                    .count()
            };
            assert_eq!(subs(&p), 0, "却下したのに盤面へ出した");

            // 2) 直して出し直す → 受理される (「もう見た」で捨てられない)。
            let good = envelope(
                &a.owner.run_id,
                "team-lead",
                "event",
                r#"{"kind": "sub_agent_started", "agent_id": "fix-1", "parent_id": "team-lead", "role": "tester", "action": "調査"}"#,
            );
            let f2 = submit(&a.dir, "team-lead", "v2", &good);
            p.pump_sessions(rows(&[&a, &b]), 801);
            assert!(!f2.exists(), "受理したのに消していない");
            assert_eq!(subs(&p), 1, "直した報告が届かない");
            std::fs::remove_dir_all(&dir).ok();
        }

        /// **隔離先へ移せなくても、報告は消えない。**
        ///
        /// `rejected/` を**ファイル**で塞いでディレクトリを作れなくする。
        /// そのときは同じ場所で拡張子を外し、それも駄目ならそのまま残す。
        /// どの道でも `remove_file` は呼ばない。
        #[test]
        fn 隔離できなくても報告は消えない() {
            let (mut p, dir, a, b) = two_lanes("outbox-quarantine-fail");
            // `rejected` という名前の**ファイル**を置く → create_dir_all が失敗する。
            std::fs::write(a.dir.join(outbox::REJECTED_DIR), "not a dir").unwrap();
            let f = submit(&a.dir, "team-lead", "x1", &report("team-lead", 99));
            let name = name_of(&f);
            p.pump_sessions(rows(&[&a, &b]), 900);
            assert!(!f.exists(), "拡張子を外していない");
            let aside = a.dir.join(format!("{name}.{}", outbox::REJECTED_DIR));
            assert!(
                aside.exists(),
                "報告が消えた (隔離先が塞がっていても残すこと): {}",
                aside.display()
            );
            assert_eq!(
                std::fs::read_to_string(&aside).unwrap(),
                report("team-lead", 99),
                "中身が変わった"
            );
            // 読み直しの対象からは外れている (毎 tick 却下を積まない)。
            let before = quarantined(&p, &a.owner, &name);
            for now in 901..905 {
                p.pump_sessions(rows(&[&a, &b]), now);
            }
            assert_eq!(
                quarantined(&p, &a.owner, &name),
                before,
                "外した報告を読み直している"
            );
            std::fs::remove_dir_all(&dir).ok();
        }

        /// **再起動をまたいでも、未処理の報告は回収できる。**
        ///
        /// 置き場は Run ごとに残るので、閉じていない Run の報告は次の起動で
        /// 届く。**閉じた Run のものは別 Run へ流れない** (置き場ごと消える)。
        #[test]
        fn 再起動後に未処理の報告を回収できる() {
            let (mut p, dir, a, b) = two_lanes("outbox-restart");
            let pos_a = p.run_pos_of_owner(&a.owner).unwrap();
            p.close_run(pos_a).expect("A を閉じる");
            finish_close(&mut p);
            // **まだ誰も読んでいない報告**を B の置き場へ置いて、保存して落ちる。
            let fb = submit(&b.dir, "team-lead", "pending", &report("team-lead", 95));
            p.save_if_needed();
            p.shutdown();
            assert!(fb.exists(), "終了で未処理の報告を消した");
            assert!(!a.dir.exists(), "閉じた Run の置き場が残っている");

            // 次の起動: B が戻り、残っていた報告が届く。
            let mut q = TeamPanel::default();
            q.home = p.home.clone();
            q.attach_workspace(&dir).expect("attach");
            q.restore_run(false).expect("B を復元");
            assert_eq!(
                q.runs
                    .iter()
                    .map(|r| r.run().run_id.clone())
                    .collect::<Vec<_>>(),
                vec![b.owner.run_id.clone()],
                "閉じた A が復活した / B が戻らない"
            );
            // 復元直後はセッションが 1 つも結び付いていない (実際の再起動と同じ)。
            assert!(
                q.runs[0].agents().iter().all(|x| x.session_id.is_none()),
                "前提: 復元でセッションの結び付きは外れる"
            );
            q.pump_sessions(Vec::new(), 500);
            assert!(!fb.exists(), "再起動後に未処理の報告を回収できていない");
            let owner_b = q.owner().expect("B");
            assert_eq!(saw(&q, &owner_b, "#95"), 1, "残っていた報告が届かない");
            std::fs::remove_dir_all(&dir).ok();
        }

        /// **同じ ID の担当を持つ 2 本を並べ、片方を閉じても他方は動き続ける。**
        #[test]
        fn 二本のrunを並べて片方を閉じても他方は動き続ける() {
            let (mut p, dir, a, b) = two_lanes("outbox-multi-run-e2e");
            let baseline_a = super::super::super::changeset::capture_baseline(&a.owner.workspace)
                .expect("A の基準点");
            let baseline_b = super::super::super::changeset::capture_baseline(&b.owner.workspace)
                .expect("B の基準点");
            std::fs::write(a.owner.workspace.join("only-a.txt"), "A\n").unwrap();
            std::fs::write(b.owner.workspace.join("only-b.txt"), "B\n").unwrap();
            let changed_a = super::super::super::changeset::measure(
                &a.owner.workspace,
                &baseline_a,
            )
            .expect("A の changeset");
            let changed_b = super::super::super::changeset::measure(
                &b.owner.workspace,
                &baseline_b,
            )
            .expect("B の changeset");
            assert_eq!(
                changed_a.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
                vec!["only-a.txt"],
                "B の変更が A に混入した"
            );
            assert_eq!(
                changed_b.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
                vec!["only-b.txt"],
                "A の変更が B に混入した"
            );
            for lane in [&a, &b] {
                let pos = p.run_pos_of_owner(&lane.owner).unwrap();
                assert!(
                    p.runs[pos]
                        .agents()
                        .iter()
                        .any(|x| x.id.as_str() == "team-lead"),
                    "前提: どちらにも team-lead が居る"
                );
            }
            let pos_a = p.run_pos_of_owner(&a.owner).unwrap();
            let to = p.runs[pos_a]
                .agents()
                .iter()
                .map(|x| x.id.0.clone())
                .find(|id| id != "team-lead")
                .expect("宛先");
            submit(&a.dir, "team-lead", "r1", &report("team-lead", 91));
            submit(&b.dir, "team-lead", "r1", &report("team-lead", 92));
            let event_a = r#"{"kind":"sub_agent_started","agent_id":"e2e-a","parent_id":"team-lead","role":"tester","action":"Aだけ"}"#;
            let event_b = r#"{"kind":"sub_agent_started","agent_id":"e2e-b","parent_id":"team-lead","role":"reviewer","action":"Bだけ"}"#;
            submit(
                &a.dir,
                "team-lead",
                "ea",
                &envelope(&a.owner.run_id, "team-lead", "event", event_a),
            );
            submit(
                &b.dir,
                "team-lead",
                "eb",
                &envelope(&b.owner.run_id, "team-lead", "event", event_b),
            );
            submit(
                &a.dir,
                "team-lead",
                "m1",
                &envelope(
                    &a.owner.run_id,
                    "team-lead",
                    "message",
                    &format!(r#"{{"to": "{to}", "text": "先に進めます"}}"#),
                ),
            );
            p.pump_sessions(rows(&[&a, &b]), 600);
            assert_eq!(saw(&p, &a.owner, "#91"), 1, "A の報告が届かない");
            assert_eq!(saw(&p, &b.owner, "#92"), 1, "B の報告が届かない");
            assert_eq!(saw(&p, &a.owner, "#92"), 0, "B の報告が A へ流れた");
            assert_eq!(saw(&p, &b.owner, "#91"), 0, "A の報告が B へ流れた");
            let has_agent = |panel: &TeamPanel, owner: &RunOwner, id: &str| {
                let pos = panel.run_pos_of_owner(owner).expect("Run");
                panel.runs[pos].agents().iter().any(|agent| agent.id.as_str() == id)
            };
            assert!(has_agent(&p, &a.owner, "e2e-a"), "A の状態が遷移しない");
            assert!(has_agent(&p, &b.owner, "e2e-b"), "B の状態が遷移しない");
            assert!(!has_agent(&p, &a.owner, "e2e-b"), "B の状態が A に混入した");
            assert!(!has_agent(&p, &b.owner, "e2e-a"), "A の状態が B に混入した");

            // 画面を切り替えても両方生きている。
            assert_eq!(p.run_tabs().len(), 2);
            p.select_run(0);
            p.select_run(1);
            assert_eq!(p.runs.len(), 2, "切り替えで消えた");

            // 実際の再起動経路で2本とworkspace対応を復元する。
            p.shutdown();
            let mut q = TeamPanel::default();
            q.home = p.home.clone();
            q.attach_workspace(&dir).expect("再attach");
            q.restore_run(false).expect("2本を復元");
            assert_eq!(q.runs.len(), 2, "再起動でRunが消えた");
            for lane in [&a, &b] {
                let pos = q.run_pos_of(&lane.owner.run_id).expect("復元したRun");
                assert_eq!(
                    q.runs[pos].owner().workspace,
                    lane.owner.workspace,
                    "再起動で実行workspace対応が変わった"
                );
            }
            assert!(has_agent(&q, &a.owner, "e2e-a"), "A の状態を復元できない");
            assert!(has_agent(&q, &b.owner, "e2e-b"), "B の状態を復元できない");

            // A を閉じる → A のworktreeだけ消え、Bは動き続ける。
            let pos_a = q.run_pos_of_owner(&a.owner).unwrap();
            q.close_run(pos_a).expect("A を閉じる");
            assert!(matches!(q.close_prompt, ClosePrompt::Confirm { .. }));
            q.close_run_discard();
            finish_close(&mut q);
            assert!(!a.dir.exists(), "閉じた Run の置き場が残っている");
            assert!(!a.owner.workspace.exists(), "A のworktreeが残っている");
            assert!(b.owner.workspace.is_dir(), "B のworktreeまで削除した");
            let event_b2 = r#"{"kind":"sub_agent_started","agent_id":"e2e-b2","parent_id":"team-lead","role":"tester","action":"継続"}"#;
            submit(
                &b.dir,
                "team-lead",
                "eb2",
                &envelope(&b.owner.run_id, "team-lead", "event", event_b2),
            );
            q.pump_sessions(Vec::new(), 601);
            assert!(has_agent(&q, &b.owner, "e2e-b2"), "A を閉じたら B が止まった");

            // もう一度再起動しても閉じたAは戻らず、Bの対応は維持される。
            q.shutdown();
            let mut r = TeamPanel::default();
            r.home = q.home.clone();
            r.attach_workspace(&dir).expect("再々attach");
            r.restore_run(false).expect("Bを復元");
            assert_eq!(r.runs.len(), 1, "閉じたAが復元された");
            assert_eq!(r.owner().expect("B").workspace, b.owner.workspace);
            assert!(has_agent(&r, &b.owner, "e2e-b2"), "Bの継続状態が消えた");
            r.close_run(0).expect("Bを片付ける");
            assert!(matches!(r.close_prompt, ClosePrompt::Confirm { .. }));
            r.close_run_discard();
            finish_close(&mut r);
            std::fs::remove_dir_all(&dir).ok();
        }

        /// **画面と置き場に同じ報告があっても二重には入らない。**
        ///
        /// **適用される報告**で見る。却下されるものは重複台帳へ入れない
        /// (直して出し直せるように) ので、二重に見えて当たり前になる。
        /// 出来事は担当の割り当てに依らず適用できるので、両方の経路で通る。
        #[test]
        fn 画面と置き場の二重受理を防ぐ() {
            let (mut p, dir, a, b) = two_lanes("outbox-dedupe");
            let ev = r#"{"kind": "sub_agent_started", "agent_id": "twice-1", "parent_id": "team-lead", "role": "tester", "action": "調査"}"#;
            let f = submit(&a.dir, "team-lead", "1", ev);
            // 画面にも**同じ塊**を出す (素直に読める形で)。
            let open = super::super::super::result_parser::EVENT_OPEN;
            let close = super::super::super::result_parser::EVENT_CLOSE;
            let screen: Vec<String> = format!("{open}\n{ev}\n{close}")
                .lines()
                .map(str::to_string)
                .collect();
            let mut input = rows(&[&a, &b]);
            for r in &mut input {
                if r.id == a.sid {
                    r.tail = screen.clone();
                }
            }
            // **同じ tick で両方に出ていても 1 回。** 置き場を先に取り込み、
            // 画面側は構造化キーで落ちる。
            p.pump_sessions(input, 400);
            assert!(!f.exists(), "受理したのに消していない");
            let subs = |p: &TeamPanel, o: &RunOwner| -> usize {
                let pos = p.run_pos_of_owner(o).expect("Run");
                p.runs[pos]
                    .agents()
                    .iter()
                    .filter(|x| x.id.as_str() == "twice-1")
                    .count()
            };
            assert_eq!(
                subs(&p, &a.owner),
                1,
                "同じ報告が画面と置き場の両方から二重に入った"
            );
            assert_eq!(subs(&p, &b.owner), 0, "別の Run へ流れた");
            std::fs::remove_dir_all(&dir).ok();
        }

        /// `X-1` の担当に `X-10-…` の報告を配らない (実ファイル)。
        #[test]
        fn 担当idの前方一致で別の担当の報告を拾わない() {
            let (mut p, dir, a, b) = two_lanes("outbox-prefix");
            let pos = p.run_pos_of_owner(&a.owner).unwrap();
            let ids: Vec<String> = p.runs[pos]
                .agents()
                .iter()
                .map(|x| x.id.0.clone())
                .collect();
            let short = ids
                .iter()
                .find(|id| id.ends_with("-1"))
                .cloned()
                .unwrap_or_else(|| panic!("`…-1` の担当が居ない: {ids:?}"));
            let long = format!("{short}0");
            assert!(!ids.contains(&long), "前提: {long} は居ない");
            // ファイル名も本文も `X-10` (居ない担当) → `X-1` へ配らず、隔離して理由を残す
            let f = submit(&a.dir, &long, "1", &report(&long, 99));
            let name = name_of(&f);
            p.pump_sessions(rows(&[&a, &b]), 100);
            assert_eq!(
                saw(&p, &a.owner, "#99"),
                0,
                "{long} の報告を {short} の報告として取り込んだ"
            );
            assert!(!f.exists(), "置き場に残っている");
            assert!(
                a.dir.join(outbox::REJECTED_DIR).join(&name).exists(),
                "隔離されていない"
            );
            assert_eq!(quarantined(&p, &a.owner, &name), 1, "理由が残っていない");
            // ファイル名は `X-10-…` で本文だけ `X-1` を名乗っても、配らない
            let f2 = submit(&a.dir, &long, "2", &report(&short, 99));
            p.pump_sessions(rows(&[&a, &b]), 101);
            assert_eq!(saw(&p, &a.owner, "#99"), 0, "本文の名乗りだけで {short} へ配った");
            assert!(!f2.exists() && a.dir.join(outbox::REJECTED_DIR).join(name_of(&f2)).exists());
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn 本文のagent_idが違う報告は配送しない() {
            let (mut p, dir, a, b) = two_lanes("outbox-claim-mismatch");
            let pos = p.run_pos_of_owner(&a.owner).unwrap();
            let other = p.runs[pos]
                .agents()
                .iter()
                .map(|x| x.id.0.clone())
                .find(|id| id != "team-lead")
                .expect("team-lead 以外の担当");
            // ファイル名は team-lead、本文は別の担当
            let f = submit(&a.dir, "team-lead", "1", &report(&other, 99));
            let name = name_of(&f);
            p.pump_sessions(rows(&[&a, &b]), 100);
            assert_eq!(saw(&p, &a.owner, "#99"), 0, "担当の食い違う報告を配送した");
            assert!(!f.exists(), "置き場に残っている");
            assert!(
                a.dir.join(outbox::REJECTED_DIR).join(&name).exists(),
                "隔離されていない"
            );
            let why = p.runs[pos]
                .events()
                .find(|e| e.summary.contains(&name))
                .map(|e| e.summary.clone())
                .expect("理由が記録されていない");
            assert!(
                why.contains("team-lead") && why.contains(&other),
                "理由に両方の担当が無い: {why}"
            );
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn runを閉じるとその置き場だけ消える() {
            let (mut p, dir, a, b) = two_lanes("outbox-close");
            let fa = submit(&a.dir, "team-lead", "1", &report("team-lead", 98));
            let fb = submit(&b.dir, "team-lead", "1", &report("team-lead", 99));
            let state = p.state_dir();
            p.save_if_needed();
            let runs_a = state.join(RUNS_DIR).join(&a.owner.run_id);
            let runs_b = state.join(RUNS_DIR).join(&b.owner.run_id);
            assert!(runs_a.exists() && runs_b.exists(), "前提: 保存の置き場がある");
            p.notice.clear();
            let pos_a = p.run_pos_of_owner(&a.owner).unwrap();
            p.close_run(pos_a).expect("A を閉じる");
            finish_close(&mut p);
            assert!(!a.dir.exists(), "A の置き場が残っている");
            assert!(!fa.exists());
            assert!(!runs_a.exists(), "A の保存が残っている");
            assert!(b.dir.exists() && fb.exists(), "B の置き場まで消した");
            assert!(runs_b.exists(), "B の保存まで消した");
            assert!(state.join(outbox::DIR_NAME).exists(), "親フォルダごと消した");
            assert_eq!(
                p.notice,
                crate::i18n::tr("team.notice.run_closed"),
                "成功以外の通知を出した"
            );
            // 残った B にはまだ届く
            p.pump_sessions(rows(&[&b]), 100);
            assert_eq!(saw(&p, &b.owner, "#99"), 1, "A を閉じたら B へ届かない");
            std::fs::remove_dir_all(&dir).ok();
        }

        /// 置き場の名前にできない `run_id` では置き場を作らず、閉じても
        /// 想定外の場所を消さない。
        #[test]
        fn 不正なrun_idでは置き場を作らず閉じても何も消さない() {
            let dir = ws("outbox-bad-run-id");
            std::fs::create_dir_all(&dir).unwrap();
            let mut p = panel_at(&dir);
            let state = p.state_dir();
            std::fs::create_dir_all(state.join(outbox::DIR_NAME)).unwrap();
            let canaries = [
                dir.join("workspace-canary.txt"),
                state.join("state-canary.txt"),
                state.join(outbox::DIR_NAME).join("outbox-canary.txt"),
            ];
            for c in &canaries {
                std::fs::write(c, "x").unwrap();
            }
            for bad in ["", ".", "..", "../x", "/abs", "a/b", "a\\b", "C:x"] {
                let result = p.plan(
                    SPEC,
                    "SPEC.md",
                    RunOptions {
                        run_id: bad.to_string(),
                        ..RunOptions::default()
                    },
                );
                assert!(result.is_err(), "危険なrun_id {bad:?}を計画に通した");
                assert!(!p.has_run(), "拒否したrun_idでRunを残した");
                for c in &canaries {
                    assert!(c.exists(), "{bad:?} で想定外の場所を消した: {}", c.display());
                }
            }
            std::fs::remove_dir_all(&dir).ok();
        }

        /// 先頭の Run が毎 tick の上限を使い切っても、後続 Run の 1 通は
        /// 同じ tick で処理される。Run の走査順を毎回 0 から始める実装では
        /// B が永久に届かない。
        #[test]
        fn 先頭runが上限以上を投入しても後続runは飢餓しない() {
            let (mut p, dir, a, b) = two_lanes("outbox-round-robin");
            for i in 0..(OUTBOX_PER_TICK + 8) {
                submit(
                    &a.dir,
                    "team-lead",
                    &format!("{i:03}"),
                    &report("team-lead", 90 + (i as u64 % 5)),
                );
            }
            let fb = submit(&b.dir, "team-lead", "only", &report("team-lead", 99));

            p.pump_sessions(rows(&[&a, &b]), 100);

            assert_eq!(saw(&p, &b.owner, "#99"), 1, "先頭 Run が予算を独占した");
            assert!(!fb.exists(), "後続 Run の正常な報告が残っている");
            let remaining = outbox::list_reports(&a.dir).len();
            assert!(remaining > 0, "公平性のためでなく全件を読んでしまった");
            assert_eq!(
                remaining,
                OUTBOX_PER_TICK + 8 - (OUTBOX_PER_TICK - 1),
                "1 tick の全体上限が変わった"
            );
            std::fs::remove_dir_all(&dir).ok();
        }

        /// 壊れたファイルは再試行対象でも 1 Run の列を占有させない。
        #[test]
        fn 壊れたファイルの山があっても後続runを止めない() {
            let (mut p, dir, a, b) = two_lanes("outbox-broken-fairness");
            for i in 0..(OUTBOX_PER_TICK + 8) {
                let f = a
                    .dir
                    .join(outbox::final_name("team-lead", &format!("{i:03}")));
                std::fs::write(f, "{書きかけ").unwrap();
            }
            let fb = submit(&b.dir, "team-lead", "only", &report("team-lead", 99));

            p.pump_sessions(rows(&[&a, &b]), 100);

            assert_eq!(saw(&p, &b.owner, "#99"), 1, "壊れた Run が後続 Run を止めた");
            assert!(!fb.exists());
            assert_eq!(
                p.outbox_ledger.tracked().len(),
                OUTBOX_PER_TICK - 1,
                "壊れたファイルを上限以上に読んだ"
            );
            std::fs::remove_dir_all(&dir).ok();
        }

        /// 非 UTF-8 は書き足せば直る JSON ではない。20 tick 再読せず、最初の
        /// bounded read で証拠を隔離する。
        #[test]
        fn 非utf8は一回で隔離して他runを止めない() {
            let (mut p, dir, a, b) = two_lanes("outbox-non-utf8");
            let bad = a.dir.join(outbox::final_name("team-lead", "bad"));
            std::fs::write(&bad, [0xff, 0xfe, 0xfd]).unwrap();
            let good = submit(&b.dir, "team-lead", "good", &report("team-lead", 99));

            p.pump_sessions(rows(&[&a, &b]), 100);

            assert!(!bad.exists(), "非 UTF-8 を再試行のため残した");
            assert!(
                !outbox::list_reports(&a.dir).iter().any(|p| p == &bad),
                "非 UTF-8 が次の tick の走査対象に残った"
            );
            assert_eq!(saw(&p, &b.owner, "#99"), 1);
            assert!(!good.exists());
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn 巨大ファイルは一回で隔離し他runの正常報告を止めない() {
            let (mut p, dir, a, b) = two_lanes("outbox-huge");
            let huge = a.dir.join(outbox::final_name("team-lead", "huge"));
            std::fs::write(
                &huge,
                vec![b'x'; super::super::super::result_parser::BLOCK_MAX_BYTES + 1],
            )
            .unwrap();
            let name = name_of(&huge);
            let good = submit(&b.dir, "team-lead", "good", &report("team-lead", 99));

            p.pump_sessions(rows(&[&a, &b]), 100);

            assert!(!huge.exists(), "上限超過を再試行のため残した");
            let rejected = a.dir.join(outbox::REJECTED_DIR).join(&name);
            assert!(rejected.exists(), "巨大ファイルの証拠を隔離していない");
            assert_eq!(quarantined(&p, &a.owner, &name), 1);
            assert!(!p.outbox_ledger.tracked().contains(&huge), "再試行台帳へ載せた");
            assert_eq!(saw(&p, &b.owner, "#99"), 1, "他Runの報告を止めた");
            assert!(!good.exists());

            for now in 101..101 + u64::from(outbox::MAX_ATTEMPTS) {
                p.pump_sessions(rows(&[&a, &b]), now);
            }
            assert_eq!(
                quarantined(&p, &a.owner, &name),
                1,
                "隔離後も20回読み直した"
            );
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn 四runへ継続投入しても全runが有限tick内に処理される() {
            let (mut p, dir, lanes) = many_lanes("outbox-four-runs", 4);
            for tick in 0..4u64 {
                for (lane_no, lane) in lanes.iter().enumerate() {
                    for file_no in 0..(OUTBOX_PER_TICK / 2) {
                        submit(
                            &lane.dir,
                            "team-lead",
                            &format!("{tick:02}-{file_no:03}"),
                            &report("team-lead", 90 + lane_no as u64),
                        );
                    }
                }
                let refs: Vec<&Lane> = lanes.iter().collect();
                p.pump_sessions(rows(&refs), 100 + tick);
            }
            for (lane_no, lane) in lanes.iter().enumerate() {
                assert!(
                    saw(&p, &lane.owner, &format!("#{}", 90 + lane_no)) > 0,
                    "Run {lane_no} が4 tick待っても一度も処理されない"
                );
            }
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn ファイルcursorは壊れた名前順先頭の後ろへ進む() {
            let (mut p, dir, a, b) = two_lanes("outbox-file-cursor");
            for i in 0..(OUTBOX_PER_TICK + 8) {
                let f = a
                    .dir
                    .join(outbox::final_name("team-lead", &format!("a-{i:03}")));
                std::fs::write(f, "{壊れた").unwrap();
            }
            let valid = submit(&a.dir, "team-lead", "z-valid", &report("team-lead", 99));
            for now in 100..103 {
                p.pump_sessions(rows(&[&a, &b]), now);
            }
            assert_eq!(saw(&p, &a.owner, "#99"), 1, "名前順の後方が永久に読まれない");
            assert!(!valid.exists());
            std::fs::remove_dir_all(&dir).ok();
        }

        /// 1 tick に見る数の上限は残る (全 Run 合わせて)。残りは次の tick で片付く。
        #[test]
        fn 一tickで見る数の上限は残る() {
            let (mut p, dir, a, b) = two_lanes("outbox-budget");
            let extra = 8;
            for i in 0..(OUTBOX_PER_TICK + extra) {
                submit(
                    &a.dir,
                    "team-lead",
                    &format!("{i:03}"),
                    &report("team-lead", 90 + (i as u64 % 5)),
                );
            }
            p.pump_sessions(rows(&[&a, &b]), 100);
            assert_eq!(
                outbox::list_reports(&a.dir).len(),
                extra,
                "1 tick で上限を超えて読んだ"
            );
            p.pump_sessions(rows(&[&a, &b]), 101);
            assert!(
                outbox::list_reports(&a.dir).is_empty(),
                "残りが次の tick で片付かない"
            );
            std::fs::remove_dir_all(&dir).ok();
        }
    }
}
