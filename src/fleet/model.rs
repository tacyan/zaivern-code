//! Fleet の**値**の型 — 観測 (入力) と、正準ビュー / スナップショット (出力)。
//!
//! ここには判断が 1 つも無い。判断は [`crate::fleet::engine`] が持ち、
//! そのために要る材料と、そこから出てくる答の形だけを置く。
//!
//! ## なぜ「観測」と「ビュー」を分けるのか
//!
//! 従来は `kanban::classify_stream` が**純関数として公開**されていたので、
//! 呼ぶ側が引数を選べた。実際に `column_for` は
//! `ladder = None` / `tail = &[]` / `flow = Unknown` を渡していて、
//! 構造化プロトコルが「編集中 ◆」と言っていても「思考中 ≈」を返していた
//! (スマホの一覧と看板カードの初期値がこの経路)。
//!
//! [`Observation`] を 1 つの型にまとめると、**入力の選択権が呼び出し側から
//! 消える** — 材料は全部渡すか、渡さないかのどちらかになる。

use crate::kanban::{Activity, Column, Flow, Source, Tally};
use crate::supervisor;

/// エージェントの駆動方式。**Fleet 集計に載るものはすべてここに種別を持つ。**
///
/// 種別ごとに UI を分けるためではなく、「同じ 1 本の集計に載っている」ことを
/// 型で言うために持つ (ACP セッションが総数から漏れていたのが Phase 1 の
/// 直す対象の 1 つ)。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum AgentKind {
    /// PTY で走る CLI エージェント (`terminal::Session`)。
    Pty,
    /// ACP (Agent Client Protocol) で駆動しているエージェント (`acp::AcpClient`)。
    Acp,
}

/// **1 ティック分の観測** — エージェント 1 体について、そのとき分かっている事実。
///
/// 「状態」は 1 つも入っていない (`sup` / `ladder` は**下位層の判定結果**であって
/// Fleet の状態ではない)。ここから [`AgentView`] を作るのは
/// [`crate::fleet::engine`] の仕事。
#[derive(Clone, Debug, Default)]
pub struct Observation {
    /// セッション ID (PTY は `Session::id`、ACP は `acp::ACP_SESSION_ID_BASE` 以降)。
    pub id: u64,
    pub kind: AgentKindOpt,
    pub title: String,
    pub icon: String,
    /// プロセスが生きているか。**事実**なので推測より強い。
    pub running: bool,
    /// 承認プロンプトで止まっているか (`Session::attention`)。
    pub attention: bool,
    /// レート制限の警告行 (無ければ `None`)。
    pub rate_limited: Option<String>,
    /// 見張り (`supervisor.rs`) の状態判定。
    pub sup: Option<supervisor::SessionState>,
    /// 状態ラダー上位 3 段の判定 (構造化プロトコル / フック / シェル統合)。
    pub ladder: Option<supervisor::LadderRead>,
    /// 画面末尾の「意味のある行」。
    ///
    /// **`None` = このティックでは画面を読んでいない。**
    /// そのとき [`crate::fleet::engine`] は前回サンプルを使い回すので、
    /// 「読まなかったせいで判定が落ちる」ことは起きない。
    pub tail_lines: Option<Vec<String>>,
    /// 起動からの経過 (ms)。
    pub uptime_ms: u64,
}

/// [`Observation::kind`] の既定値つきラッパ。
///
/// `Observation` は `Default` を持つ (テストで一部だけ埋めたい) が、
/// `AgentKind` に「既定の種別」は無い。既定を `Pty` と決め打つと、
/// ACP 側の埋め忘れが**黙って端末扱いになる**ので、明示的に包んでおく。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct AgentKindOpt(Option<AgentKind>);

impl AgentKindOpt {
    pub fn pty() -> Self {
        AgentKindOpt(Some(AgentKind::Pty))
    }
    pub fn acp() -> Self {
        AgentKindOpt(Some(AgentKind::Acp))
    }
    /// 埋め忘れは端末扱いにする (集計から落とすよりは良い)。
    pub fn get(self) -> AgentKind {
        self.0.unwrap_or(AgentKind::Pty)
    }
}

impl From<AgentKind> for AgentKindOpt {
    fn from(k: AgentKind) -> Self {
        AgentKindOpt(Some(k))
    }
}

/// **エージェント 1 体の正準ビュー** — すべての画面と API が読む唯一の値。
///
/// 看板・デッキ・Cockpit・サイドバー・スマホは、これを**読むだけ**にする。
/// 自前で `classify` / `column_for` を呼び直さない (それが Phase 1 の目的)。
#[derive(Clone, Debug, PartialEq)]
pub struct AgentView {
    pub id: u64,
    pub kind: AgentKind,
    pub title: String,
    pub icon: String,
    /// **実際に置くレーン** (確信度の床 + ヒステリシス適用済み)。
    pub lane: Column,
    /// いま何をしているか (レーンより細かい)。
    pub activity: Activity,
    /// 判定の出どころ = 段位。`Source::mark()` で `◆◇◈✓≈` になる。
    pub source: Source,
    /// 補足 (ツール名 / ファイル / 理由)。無ければ空。
    pub detail: String,
    /// 段位不足で採らなかった見張りの疑い (カードに ⚠ で出す)。
    pub suspicion: Option<&'static str>,
    /// 生の出力ストリームの事実 (異常判定の裏取りに使ったもの)。
    pub flow: Flow,
    pub running: bool,
    pub attention: bool,
    pub rate_limited: Option<String>,
    /// このアクティビティが始まった時刻 (ms)。
    pub since_ms: u64,
    /// 現在のレーンへ着地した時刻 (ms)。着地ハイライトに使う。
    pub landed_ms: u64,
    pub uptime_ms: u64,
    /// 最後にサンプルした画面末尾 (カードの一言・ホバープレビュー)。
    pub tail: Vec<String>,
    /// 直近に触ったファイル / 走らせたコマンド。
    pub last_file: String,
    pub last_cmd: String,
    /// 直近 30 秒の出力の勢い (古い → 新しい)。スパークラインがそのまま描く。
    ///
    /// **描画側が `Track` を覗かなくて済むように、ここまで畳んで載せる。**
    /// 読み取り経路を `Snapshot → AgentView` の 1 本に保つための項目で、
    /// 費用は 1 体あたり `f32` 30 個 (= 120 バイト)。
    pub pulse: Vec<f32>,
}

impl AgentView {
    /// このレーンに置いた根拠 (ホバーに出す 1 行)。
    ///
    /// 文言の組み立ては [`crate::kanban::Read::reason`] 1 か所に置いたまま
    /// (2 つ持つと必ずずれる)。
    pub fn reason(&self) -> String {
        crate::kanban::Read {
            activity: self.activity,
            source: self.source,
            detail: self.detail.clone(),
            suspicion: self.suspicion,
        }
        .reason()
    }

    /// 現在のアクティビティが続いている時間 (ms)。
    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.since_ms)
    }

    /// 翻訳済みの状態ラベル (カード / 一覧 / スマホが同じ 1 本を使う)。
    pub fn state_label(&self) -> String {
        crate::i18n::tr(self.activity.label())
    }
}

/// **ある瞬間の Fleet 全体**。読み手は必ずこれを丸ごと受け取る。
///
/// 「エージェント A はこの Snapshot、B は次の Snapshot」という混ざり方をすると、
/// 集計と一覧が食い違う。`Arc` で配って**丸ごと差し替える**。
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// 観測順のビュー。並べ替えは読み手の仕事。
    pub agents: Vec<AgentView>,
    /// 直近のサンプルで「出力が動いている」と判定したか (再描画の刻みに使う)。
    pub busy: bool,
    /// 生きているエージェントが 1 体でも居るか。
    pub any_running: bool,
    /// 着地ハイライトが走っているか (走っている間だけ高頻度描画)。
    pub animating: bool,
    /// このティックで**初めて現れた** ID。UI が「これが始まった」を示すのに使う。
    pub arrived: Vec<u64>,
    /// 追跡表が空から埋まったティックか。**起動の合図として扱ってはいけない**
    /// (初回の総取り込み / ワークスペース復元でまとめて現れた場合)。
    pub first_fill: bool,
}

impl Snapshot {
    pub fn view(&self, id: u64) -> Option<&AgentView> {
        self.agents.iter().find(|a| a.id == id)
    }

    /// **レーン別集計** (`kind` を渡すとその駆動方式だけ数える)。
    ///
    /// 不変条件: レーン別人数の合計 == 総数。
    ///
    /// 画面 (看板の KPI タイル / スマホの見出し) は `Some(AgentKind::Pty)` で
    /// 呼ぶ。レーンに並ぶカードが PTY セッションだけなので、**タイルの数字と
    /// 並んでいるカードの数を必ず一致させる**ためである
    /// (ACP は Store には載っているが、操作 API がセッション index を宛先に
    /// 使うので、まだレーンへは並べない — `docs/control-plane.md` Phase 5)。
    /// `None` は Fleet 全体の総数で、`Total Agents` はこちらが正しい。
    pub fn tally(&self, kind: Option<AgentKind>) -> Tally {
        Tally::from_rows(
            self.agents
                .iter()
                .filter(|a| kind.is_none_or(|k| a.kind == k))
                .map(|a| (a.lane, a.running)),
        )
    }
}
