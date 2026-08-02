//! フリート看板 (Fleet Kanban) — 全エージェントを俯瞰・指揮する Ops コンソール画面。
//! ターミナルパネル右端の「📋 看板」タブから、パネル内のビューとして開く。
//!
//! ダッシュボード構成 (Autonomous Ops Console):
//! - ヘッダー: 稼働数チップ + 連続稼働時間 + ブロードキャスト + レイアウト切替
//! - KPI タイル: **8 レーンそのもの** (合計 = 総数。「稼働中」は別軸なので混ぜない)
//! - 左レール: エージェント一覧 — アバター + 状態 + 「いま何をしているか」一言
//! - 中央: **8 レーン (待機 / 思考中 / 編集中 / 実行中 / 検証中 / 承認待ち /
//!   停滞・異常 / 完了)**。列そのものが「誰が何をしているか」の一覧になる。
//!   カードは [`Read::lane`] の判定が変われば勝手にレーンを移動する — ドラッグ不要。
//!   着地したカードは短時間ハイライトするので、目で追える。
//! - 右レール: アクティビティフィード (状態遷移の実況、LIVE)
//! - ライブペイン: 選択中カードの**本物の端末**を横 (or 縦モードでは下) に出す。
//!   端末描画は再実装せず、app.rs が渡すクロージャ (中身は `terminal::draw`) を呼ぶ。
//!   ⊞ で**全画面**にすると端末が看板の領域を丸ごと使う (レーンは隠れる)。
//!   戻すのは ◎ か Esc。全画面かどうかは永続メモリに残る。
//! - 下部: 処理スループットの折れ線 (作業中エージェント数の推移)
//!
//! ## レイアウト
//! 横モード (広い窓) = レーンを横に並べる。縦モード (細く高い窓) = レーンを縦に積み、
//! カードは全幅。`LayoutMode::Auto` なら [`use_vertical`] が窓の縦横比で選ぶ。
//! 選択は egui の永続メモリに持つ (config.rs は他所有のため触らない)。
//!
//! ## 状態の出どころ (重要 — CLAUDE.md 原則 #4)
//! 画面のテキストを読んだだけの推定を「事実」として扱わない。
//! [`classify`] は強い信号から順に見て、必ず出どころ ([`Source`]) を添えて返す:
//! プロセス生死 → 承認プロンプト検出 (terminal.rs) → レート制限検出 (terminal.rs)
//! → 見張りの状態判定 (supervisor.rs) → 最後の手段として画面末尾の表引き。
//! 画面推定のときだけ UI に「推定」(≈) と出す。
//!
//! さらに**確信度の床**を敷いてある ([`Read::lane`]): 画面推定だけの判定は
//! 作業系レーンまでしか動かせない。人を呼ぶ「承認待ち」「停滞・異常」と、
//! 終わったと言い切る「完了」は、画面より上の段 (プロセス / プロンプト / 上限 /
//! 見張り) の裏付けを要求する。
//!
//! **見張りの異常判定も、根拠が画面テキストなら裏取りを要求する** ([`Flow`])。
//! 見張りの「エラー多発」は画面に出た `error` という語を数えているだけで、
//! `Read(src/error_handling.rs)` や `test … FAILED` のような**正常な作業の出力**でも
//! 発火する。実際に「作業中なのに停滞・異常へ入る」と報告された原因がこれ。
//! そこで生の出力ストリーム (数字とスピナーを潰した末尾行が増えているか) と
//! 突き合わせ、**矛盾したら弱いほうを採らない** — 進んでいる証拠がある間は
//! 人を呼ばず、疑いはカードに ⚠ として残す。
//!
//! ## 負荷
//! アイドル時にコストを払わない設計:
//! - PTY 画面の読み直し (`screen_tail_lines`) は [`KanbanState::sample_due`] が
//!   真を返したフレームだけ。動いている間 ~6.7Hz、静かなら 1Hz。
//! - 再描画要求も無条件ではなく [`KanbanState::next_repaint_ms`] が決める
//!   (着地アニメ中 33ms / 稼働中 150ms / 静か 1s / 全員終了 2s)。
//! - 新しい出力が来たときは terminal.rs の読取スレッドが `request_repaint` する。
//!
//! 作法は orchestration.rs と同じ: 判断と描画はこのモジュール、
//! 副作用 (PTY への書き込み・起動・再起動…) は `KanbanAction` で app.rs へ返す。
//! ここでは Session を直接借りない (app.rs が `Card` へ写して渡す)。

use std::collections::HashMap;

use eframe::egui::{self, Color32, Pos2, Rect, RichText, Stroke};

use crate::i18n::{tr, trf};
use crate::supervisor;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// 列 (状態から一意に決まる)
// ---------------------------------------------------------------------------

/// カンバンのレーン。カードは [`Read::lane`] の判定に従って自動で移動する。
///
/// **レーンは「いま何をしているか」で分ける。** 8 本あるので、盤面を見れば
/// 「実装中が 3・検証中が 2・承認待ちが 1」が**数えるまでもなく**列の高さで分かる。
/// これが看板の値打ちで、一度 4 本 (待機/作業中/要対応/完了) へ畳んだところ
/// 「カードが多いときは分散していた方が見やすい」というオーナー判断で 8 本へ戻した。
///
/// 畳んだときに入れた正しい修正はそのまま残している:
/// 確信度の床 ([`Read::lane`])・出どころの表示 (✓/≈)・二重計上しない集計 ([`Tally`])。
///
/// 細かく分けるほどカードはよく動くので、移動には [`Column::hold_ms`] の
/// ヒステリシスを掛ける (思考↔編集↔実行↔検証 の往復でちらつかせない)。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Column {
    /// 手が空いている (指示を受けられる)
    Ready,
    /// 考えている・調べている (出力は動くが編集も実行もしていない)
    Thinking,
    /// ファイルを書き換えている
    Editing,
    /// コマンドを走らせている
    Running,
    /// テスト・ビルド・lint を回している
    Verifying,
    /// **人の手が要る** — 承認・入力待ちで止まっている
    Approval,
    /// **人の手が要る** — 停滞・ループ・エラー多発・レート制限
    Trouble,
    /// 完了、またはプロセス終了
    Done,
}

/// 表示順 (横モードでは左 → 右、縦モードでは上 → 下)。
pub const COLUMNS: [Column; 8] = [
    Column::Ready,
    Column::Thinking,
    Column::Editing,
    Column::Running,
    Column::Verifying,
    Column::Approval,
    Column::Trouble,
    Column::Done,
];

/// レーン本数。幾何 (最小幅など) はここから導くので、本数を変えれば追随する。
pub const LANES: usize = COLUMNS.len();

/// レーン移動のヒステリシス既定値 (ms)。この時間だけ同じ判定が続かないと動かない。
const LANE_HOLD_MS: u64 = 400;

/// 作業系レーン (思考中 / 編集中 / 実行中 / 検証中) へ移るのに要る保持時間 (ms)。
///
/// 動いているエージェントの画面は [`FAST_SAMPLE_MS`] ごとに読む。ツール呼び出しは
/// `Read → Edit → Bash` のように 1 秒の中で何度も表示が変わるので、既定の 400ms では
/// 1 秒に 2 度も列をまたいでカードが飛ぶ。**同じ判定が 8 サンプル続くこと**を求めれば、
/// 本当に仕事の中身が変わったときだけ動く。
///
/// 遅れるのは**カードの位置だけ**で、活動ラベル・詳細行・経過時間は即座に更新される
/// (情報は遅らせない / 視線だけ動かさない、という分担)。
const WORK_HOLD_MS: u64 = FAST_SAMPLE_MS * 8;

/// 「停滞・異常」へ落とすのに要る保持時間 (ms)。
///
/// このレーンは**人を呼ぶ**ので、誤報のコストがいちばん高い。見張りの異常判定は
/// 画面のハッシュ列から出るため、ツールの切り替わりや一瞬の静けさで 1〜2 サンプルだけ
/// 立つことがある。それをそのまま採ると、動いているエージェントが「停滞・異常」へ
/// 飛ぶ (オーナー報告のバグ: 稼働中の全カードが異常レーンに並んだ)。
///
/// **本当に異常なら数秒では消えない**ので、この時間だけ続くことを求める。
/// 続かないあいだカードは作業系レーン (= 処理中) に居る。
/// 承認待ちと完了は人を待たせる/待たせないの判断が逆なので 0 のまま。
const TROUBLE_HOLD_MS: u64 = 5_000;

/// 「新しいレーンへ着地した」ハイライトの寿命 (ms)。
const LAND_HIGHLIGHT_MS: u64 = 900;

impl Column {
    /// 列見出し (tr のキーになる日本語原文)。
    pub fn title(self) -> &'static str {
        match self {
            Column::Ready => "待機",
            Column::Thinking => "思考中",
            Column::Editing => "編集中",
            Column::Running => "実行中",
            Column::Verifying => "検証中",
            Column::Approval => "承認待ち",
            Column::Trouble => "停滞・異常",
            Column::Done => "完了",
        }
    }

    /// 列見出しの絵文字 (縦モードのレーン帯でも使う)。
    ///
    /// 豆腐 (□) にならない字だけを使う — app.rs の `ui_symbols_have_glyphs` /
    /// `ui_glyph_symbols_have_glyphs` が担保している集合から選んでいる。
    pub fn icon(self) -> &'static str {
        match self {
            Column::Ready => "💤",
            Column::Thinking => "🔎",
            Column::Editing => "✏",
            Column::Running => "▶",
            Column::Verifying => "🛠",
            Column::Approval => "✋",
            Column::Trouble => "⚠",
            Column::Done => "✔",
        }
    }

    /// 列見出しのホバー説明 (tr のキー)。
    fn hint(self) -> &'static str {
        match self {
            Column::Ready => "手が空いています — 指示を送るとすぐ動きます",
            Column::Thinking => "考えている・調べています (編集も実行もしていません)",
            Column::Editing => "ファイルを書き換えています (実装中)",
            Column::Running => "コマンドを走らせています",
            Column::Verifying => "テスト・ビルド・lint を回しています",
            Column::Approval => "あなたの承認・入力を待って止まっています",
            Column::Trouble => {
                "停滞・ループ・エラー多発・レート制限 — あなたの手が要ります。ここを空にするのが仕事です"
            }
            Column::Done => "タスク完了、またはプロセスが終了しています",
        }
    }

    /// 列のアクセント色。カードの状態ドット・チップも同じ色を使う。
    /// 色はすべて theme.rs 由来 (リテラルを書かない)。
    fn color(self, th: &Theme) -> Color32 {
        match self {
            Column::Ready => th.accent_soft,
            // ansi の明色は 8..16。テーマごとに定義されているのでここでも安全に使える。
            Column::Thinking => th.ansi[13],
            Column::Editing => th.accent,
            Column::Running => th.ok,
            Column::Verifying => th.ansi[14],
            Column::Approval => th.warn,
            Column::Trouble => th.err,
            Column::Done => th.text_dim,
        }
    }

    /// **視線を最優先で引くレーンか。** 枠線・帯・見出しの強調に使う。
    ///
    /// 人が動かないと止まったままのカードが入る 2 本だけを強調する。
    /// ここが空なら見なくてよい画面 — だから声を大きくするのはこの 2 本に限る。
    pub fn loud(self) -> bool {
        matches!(self, Column::Approval | Column::Trouble)
    }

    /// 表示順の添字 (集計配列 [`Tally`] のキー)。
    pub const fn index(self) -> usize {
        match self {
            Column::Ready => 0,
            Column::Thinking => 1,
            Column::Editing => 2,
            Column::Running => 3,
            Column::Verifying => 4,
            Column::Approval => 5,
            Column::Trouble => 6,
            Column::Done => 7,
        }
    }

    /// 「動いている」レーンか (作業系 4 本)。ヘッダーのまとめチップと折れ線が使う。
    pub fn is_work(self) -> bool {
        matches!(
            self,
            Column::Thinking | Column::Editing | Column::Running | Column::Verifying
        )
    }

    /// このレーンへ移るまでに判定が続く必要のある時間 (ms)。
    ///
    /// - 承認待ち・完了は人がすぐ気づくべき強い信号 (かつ [`Read::lane`] が
    ///   画面推定だけでは入れない) なので 0 = 即時。
    /// - **停滞・異常だけは [`TROUBLE_HOLD_MS`]** — 人を呼ぶのに誤報がいちばん
    ///   高くつくレーンなので、「本当に続いている」ことを確かめてから落とす。
    /// - 作業系 4 本は往復しやすいので [`WORK_HOLD_MS`] を掛ける。
    /// - 待機はその中間 ([`LANE_HOLD_MS`]) — 出力が一瞬途切れただけで落とさない。
    pub fn hold_ms(self) -> u64 {
        match self {
            Column::Approval | Column::Done => 0,
            Column::Trouble => TROUBLE_HOLD_MS,
            Column::Ready => LANE_HOLD_MS,
            _ => WORK_HOLD_MS,
        }
    }
}

// ---------------------------------------------------------------------------
// アクティビティ分類 — 「いま何をしているか」と、その**出どころ**
// ---------------------------------------------------------------------------

/// カードの現在の作業内容。レーンより細かい (`Trouble` レーンは 2 種類を束ねる)。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Activity {
    /// 起動直後でまだ一度も観測できていない
    Starting,
    /// 生きているが動いていない
    Idle,
    /// 考えている・読んでいる・調べている
    Thinking,
    /// ファイルを編集している
    Editing,
    /// コマンドを実行している
    Running,
    /// テスト・ビルド・lint
    Verifying,
    /// 承認/入力待ち
    Approval,
    /// レート制限・使用上限
    RateLimited,
    /// 停滞・ループ・エラー・クラッシュ
    Stalled,
    /// 終了
    Exited,
}

impl Activity {
    /// カードに出すラベル (tr のキーになる日本語原文)。
    pub fn label(self) -> &'static str {
        match self {
            Activity::Starting => "起動中",
            Activity::Idle => "待機",
            Activity::Thinking => "思考中",
            Activity::Editing => "編集中",
            Activity::Running => "実行中",
            Activity::Verifying => "検証中",
            Activity::Approval => "承認待ち",
            Activity::RateLimited => "レート制限中",
            Activity::Stalled => "停滞・異常",
            Activity::Exited => "終了",
        }
    }

    /// このアクティビティが属するレーン (**確信度の床を掛ける前**の素の対応)。
    ///
    /// 実際の配置は [`Read::lane`] を使うこと — 画面推定だけの判定を
    /// 「承認待ち」「停滞・異常」「完了」へ落とさない床がそこに入っている。
    ///
    /// 対応表 (10 アクティビティ → 8 レーン):
    /// - 起動中 / 待機 → 待機
    /// - 思考中 / 編集中 / 実行中 / 検証中 → 同名のレーン (1 対 1)
    /// - 承認待ち → 承認待ち
    /// - レート制限中 / 停滞・異常 → 停滞・異常 (どちらも人が動かないと進まない)
    /// - 終了 → 完了
    pub fn column(self) -> Column {
        match self {
            Activity::Starting | Activity::Idle => Column::Ready,
            Activity::Thinking => Column::Thinking,
            Activity::Editing => Column::Editing,
            Activity::Running => Column::Running,
            Activity::Verifying => Column::Verifying,
            Activity::Approval => Column::Approval,
            Activity::RateLimited | Activity::Stalled => Column::Trouble,
            Activity::Exited => Column::Done,
        }
    }

    /// 「出力が動いているはず」= 速いサンプリングを回す価値があるか。
    pub fn is_busy(self) -> bool {
        matches!(
            self,
            Activity::Thinking | Activity::Editing | Activity::Running | Activity::Verifying
        )
    }
}

/// 判定の**出どころ**。画面推定を事実として扱わないために必ず持ち回る。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// プロセスの生死 (最強)
    Process,
    /// terminal.rs の承認プロンプト検出
    Prompt,
    /// terminal.rs のレート制限検出
    RateLimit,
    /// supervisor.rs の状態判定 (ハッシュ列の時系列)
    Supervisor,
    /// 画面末尾テキストの表引き (**推定**)
    Screen,
}

impl Source {
    /// UI に出す短い名前 (tr のキー)。
    pub fn label(self) -> &'static str {
        match self {
            Source::Process => "プロセス",
            Source::Prompt => "プロンプト検出",
            Source::RateLimit => "上限検出",
            Source::Supervisor => "見張り",
            Source::Screen => "画面推定",
        }
    }

    /// 画面テキストからの推定か (UI で「推定」と断るために使う)。
    pub fn is_guess(self) -> bool {
        matches!(self, Source::Screen)
    }

    /// **信号の段位** (小さいほど強い)。CLAUDE.md 原則 #4 の優先順位そのもの:
    /// 構造化プロトコル > ベンダー提供フック > 状態ファイル > 画面スクレイプ。
    ///
    /// レーンの判断はこの段位で足切りする ([`Read::lane`])。
    pub const fn rung(self) -> u8 {
        match self {
            Source::Process => 0,
            Source::Prompt => 1,
            Source::RateLimit => 2,
            Source::Supervisor => 3,
            Source::Screen => 4,
        }
    }
}

/// 画面推定 ([`Source::Screen`]) だけでは入れない一番弱い段位。
///
/// これより弱い出どころで「承認待ち」「停滞・異常」「完了」を名乗らせない。
const STRONG_RUNG: u8 = Source::Supervisor.rung();

/// 床に弾かれた判定の行き先。
///
/// 「画面に何か出ている = 生きて動いてはいる」ところまでは言えるので、
/// 作業系のいちばん弱い列 (思考中) へ置く。人は呼ばず、情報も捨てない。
const FLOOR_LANE: Column = Column::Thinking;

/// **画面テキストだけで入ってはいけないレーンか** (純関数)。
///
/// 「承認待ち」「停滞・異常」は人を呼ぶ = 誤報のコストが高い。「完了」は
/// 見なくてよいと言い切る = 見落としのコストが高い。どれも画面の文字列一致で
/// 決めてよい判断ではない。
pub fn needs_strong_signal(col: Column) -> bool {
    matches!(col, Column::Approval | Column::Trouble | Column::Done)
}

/// **生の出力ストリームの事実** — 画面の「意味」ではなく「新しい中身が出たか」。
///
/// スピナーやカウンタの回転は [`supervisor::normalize_line`] で潰してから比べるので、
/// 「表示は動いているが中身は 1 行も増えていない」を Live と数えない。
/// 見張りの異常判定 (画面テキスト由来) の**裏取り**に使う。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Flow {
    /// 観測がまだ足りない (判断材料にしない)
    #[default]
    Unknown,
    /// 直近に新しい中身が出ている = 進んでいる
    Live,
    /// 十分な時間、新しい中身が 1 行も出ていない = 止まっている
    Silent,
}

/// **見張りの異常判定を「人を呼ぶレーン」へ上げてよいか** (純関数)。
///
/// CLAUDE.md 原則 #4 の実装。見張り (supervisor.rs) の判定は 1 つの段位ではなく、
/// **根拠の段位がばらばら**であることに注意する:
///
/// | 判定 | 実際の根拠 | 裏取り |
/// |---|---|---|
/// | 異常終了 | プロセスの生死 (最強) | 不要 |
/// | ループ | 同じ出力ブロックの再出現 (構造) | 不要 (出力が出ていても成立する) |
/// | 停滞 | 「画面が変わらない」 | **出力が止まっていること** |
/// | エラー多発 | 画面に出た `error` 等の**語の数** | **出力が止まっていること** |
///
/// 「エラー多発」は `Read(src/error_handling.rs)` や `test … FAILED` のような
/// 正常な作業の出力でも発火する (supervisor::is_error_line は部分一致)。
/// 出力が流れている間は「エージェントは仕事をしている」ほうが強い事実なので、
/// 人は呼ばない。止まったなら停滞側が拾う。
pub fn trouble_confirmed(state: supervisor::SessionState, flow: Flow) -> bool {
    use supervisor::SessionState as S;
    match state {
        // 進捗の有無と無関係に成立する判定 (プロセス / 反復構造)
        S::Crashed | S::Looping => true,
        // 「止まっている」と言う判定は、止まっている事実で裏を取る
        S::Stalled | S::Errored => flow != Flow::Live,
        _ => true,
    }
}

/// [`classify`] の結果。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Read {
    pub activity: Activity,
    pub source: Source,
    /// 補足 (編集中のファイル・実行中のコマンド・異常の理由など)。無ければ空。
    pub detail: String,
    /// **段位不足で採らなかった見張りの異常判定** (tr のキー)。
    ///
    /// レーンは動かさないが、カードには ⚠ として出す — 情報は捨てない。
    pub suspicion: Option<&'static str>,
}

impl Read {
    /// 素の判定 (疑い無し)。
    pub fn new(activity: Activity, source: Source, detail: String) -> Self {
        Self {
            activity,
            source,
            detail,
            suspicion: None,
        }
    }

    /// **実際に置くレーン** — [`Activity::column`] に確信度の床を掛けたもの。
    ///
    /// 画面末尾の表引きしか根拠が無い判定は、どれだけ「承認待ちっぽい」文字列でも
    /// [`FLOOR_LANE`] までしか動かさない。人を呼ぶ (承認待ち / 停滞・異常) /
    /// 見切りをつける (完了) には [`STRONG_RUNG`] 以上 — プロセス生死・
    /// プロンプト検出・上限検出・見張り — が要る。
    pub fn lane(&self) -> Column {
        let col = self.activity.column();
        if needs_strong_signal(col) && self.source.rung() > STRONG_RUNG {
            // 画面の文字列だけで人を呼ばない。動いてはいるので作業系へ置く。
            FLOOR_LANE
        } else {
            col
        }
    }

    /// このレーンに置いた根拠 (ホバーに出す 1 行)。純関数なので表テストできる。
    pub fn reason(&self) -> String {
        let act = tr(self.activity.label());
        let src = tr(self.source.label());
        let d = self.detail.trim();
        let base = if d.is_empty() {
            trf("{act} — 出どころ: {src}", &[("act", act), ("src", src)])
        } else {
            trf(
                "{act} — 出どころ: {src} / {detail}",
                &[("act", act), ("src", src), ("detail", d.to_string())],
            )
        };
        match self.suspicion {
            Some(s) => format!(
                "{base}\n{}",
                trf(
                    "見張りの疑い: {s} — 出力が続いているので人は呼びません",
                    &[("s", tr(s))],
                )
            ),
            None => base,
        }
    }
}

/// 画面テキストから拾う補足の種類。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pick {
    /// 補足を拾わない
    Nothing,
    /// ファイルパスらしいトークン
    Path,
    /// コマンドらしい文字列 (括弧の中 → 無ければ語の後ろ)
    Command,
}

/// 画面末尾テキストを分類する 1 行の規則。
pub struct ScreenRule {
    pub activity: Activity,
    /// 小文字化した行にこのいずれかが**語として**現れればマッチ
    pub needles: &'static [&'static str],
    pub pick: Pick,
}

/// ベンダー CLI の出力 → アクティビティの表。**ここだけ直せば追随できる**。
///
/// 上から順に試し、最初に当たった規則を採る。順序に意味がある:
/// - 編集が最優先 — `Writing tests/mod.rs` は「テストを書いている」であって
///   「テストを回している」ではない。パス中の語に引っ張られないようにする。
/// - 次に検証 — `Bash(cargo test)` は実行でもあるが、人が知りたいのは「検証中」。
/// - 最後に実行 → 思考。
///
/// 語の一致は境界付き ([`contains_word`]) なので "latest" が "test" に当たらない。
/// 日本語 CLI 出力・ANSI 装飾付きの行もそのまま食わせてよい
/// ([`classify_screen`] が前処理する)。
pub const SCREEN_RULES: &[ScreenRule] = &[
    ScreenRule {
        activity: Activity::Editing,
        needles: &[
            "edit",
            "editing",
            "multiedit",
            "notebookedit",
            "str_replace",
            "write",
            "writing",
            "update",
            "updating",
            "patch",
            "apply_patch",
            "applying",
            "編集",
            "書き込み",
            "書き換え",
            "作成中",
        ],
        pick: Pick::Path,
    },
    ScreenRule {
        activity: Activity::Verifying,
        needles: &[
            "test",
            "tests",
            "pytest",
            "jest",
            "lint",
            "clippy",
            "typecheck",
            "type-check",
            "tsc",
            "compiling",
            "building",
            "build",
            "テスト",
            "検証",
            "ビルド",
            "コンパイル",
        ],
        pick: Pick::Command,
    },
    ScreenRule {
        activity: Activity::Running,
        needles: &[
            "bash",
            "shell",
            "run",
            "running",
            "exec",
            "executing",
            "run_command",
            "実行中",
            "コマンド実行",
            "起動中",
        ],
        pick: Pick::Command,
    },
    ScreenRule {
        activity: Activity::Thinking,
        needles: &[
            "thinking",
            "pondering",
            "planning",
            "analyzing",
            "reading",
            "read",
            "search",
            "searching",
            "grep",
            "glob",
            "fetch",
            "esc to interrupt",
            "思考",
            "考え中",
            "分析中",
            "調査中",
            "検索中",
            "読み込み",
            "計画",
        ],
        pick: Pick::Nothing,
    },
];

/// `needle` が `hay` に**語として**含まれるか (前後が ASCII 英数字でない)。
///
/// 単純な `contains` だと "latest" が "test" に当たってしまう。日本語の語は
/// 前後が ASCII 英数字になりにくいので、この境界判定でそのまま扱える。
pub fn contains_word(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = hay.as_bytes();
    let nb = needle.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = hay[from..].find(needle) {
        let at = from + rel;
        let before_ok = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        let end = at + nb.len();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        // 次の候補へ (UTF-8 境界は find が保証する開始位置 + 1 バイトずつ進める)
        from = at + 1;
        while from < hay.len() && !hay.is_char_boundary(from) {
            from += 1;
        }
        if from >= hay.len() {
            break;
        }
    }
    false
}

/// トークンがファイルパスらしいか。拡張子の一覧は持たない (OS 非依存)。
pub fn looks_like_path(tok: &str) -> bool {
    let t = tok.trim_matches(|c: char| {
        !c.is_alphanumeric() && c != '/' && c != '\\' && c != '.' && c != '_' && c != '-'
    });
    if t.len() < 3 || t.contains(char::is_whitespace) {
        return false;
    }
    if !t.chars().any(char::is_alphanumeric) {
        return false;
    }
    if t.contains('/') || t.contains('\\') {
        return true;
    }
    // "foo.rs" 形式: 最後の '.' の前後に中身があり、拡張子が短い英数字
    match t.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && !ext.is_empty()
                && ext.len() <= 8
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// 行から「コマンドらしい文字列」を拾う。括弧の中を最優先。
pub fn pick_command(line: &str) -> String {
    if let Some(open) = line.find('(') {
        if let Some(close) = line[open + 1..].find(')') {
            let inner = line[open + 1..open + 1 + close].trim();
            if !inner.is_empty() {
                return inner.to_string();
            }
        }
    }
    // 括弧が無ければ「見出し: 本体」形式を試し、それも無ければ装飾を落とした行
    let body = line
        .trim_start_matches(|c: char| !c.is_alphanumeric() && !is_cjk(c))
        .trim();
    for sep in [": ", "：", ":"] {
        if let Some((_, rest)) = body.split_once(sep) {
            let rest = rest.trim();
            // Windows パスの "C:\\..." のような切れ方は捨てる
            if !rest.is_empty() && !rest.starts_with('\\') {
                return rest.to_string();
            }
        }
    }
    body.to_string()
}

/// 行から「ファイルパスらしいトークン」を拾う。見つからなければコマンド扱い。
pub fn pick_path(line: &str) -> String {
    let inner = pick_command(line);
    for tok in inner.split_whitespace() {
        let t = tok.trim_matches(|c: char| "\"'`,;:()[]{}".contains(c));
        if looks_like_path(t) {
            return t.to_string();
        }
    }
    for tok in line.split_whitespace() {
        let t = tok.trim_matches(|c: char| "\"'`,;:()[]{}".contains(c));
        if looks_like_path(t) {
            return t.to_string();
        }
    }
    inner
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xff00..=0xffef)
}

/// 表示用に 1 行を整える: ANSI を落とし、行頭の飾り (`⏺ · │ ● ✱` 等) を削る。
pub fn clean_line(line: &str) -> String {
    let no_ansi = supervisor::strip_ansi(line);
    no_ansi
        .trim()
        .trim_start_matches(|c: char| !c.is_alphanumeric() && !is_cjk(c) && c != '/' && c != '.')
        .trim()
        .to_string()
}

/// 画面末尾テキストの表引き (**純関数**)。新しい行から順に見て最初に当たった規則を返す。
///
/// これは推定であって事実ではない。呼び出し側は必ず [`Source::Screen`] を添えること。
pub fn classify_screen(tail: &[String]) -> Option<(Activity, String)> {
    for raw in tail.iter().rev() {
        let line = clean_line(raw);
        if line.is_empty() {
            continue;
        }
        let low = line.to_lowercase();
        for rule in SCREEN_RULES {
            if rule.needles.iter().any(|n| contains_word(&low, n)) {
                let detail = match rule.pick {
                    Pick::Nothing => String::new(),
                    Pick::Path => pick_path(&line),
                    Pick::Command => pick_command(&line),
                };
                return Some((rule.activity, detail));
            }
        }
    }
    None
}

/// セッションの各種信号から「いま何をしているか」を決める **純関数**。
///
/// 生の出力ストリームの観測を持たない呼び出し向け ([`Flow::Unknown`] 相当)。
/// 看板本体は [`classify_flow`] を使う。
pub fn classify(
    running: bool,
    attention: bool,
    rate_limited: bool,
    sup: Option<supervisor::SessionState>,
    tail: &[String],
) -> Read {
    classify_flow(running, attention, rate_limited, sup, tail, Flow::Unknown)
}

/// [`classify`] に**生の出力ストリームの裏取り** ([`Flow`]) を足した本体 (純関数)。
///
/// 強い信号から順に見る (弱い推定に上書きさせない):
/// 1. プロセス生死 — `running == false` なら他を見ずに終了
/// 2. 承認プロンプト検出 (terminal.rs `scan_attention`)
/// 3. レート制限検出 (terminal.rs `detect_rate_limit`)
/// 4. supervisor の異常判定 — ただし [`trouble_confirmed`] の裏取りを通ったものだけ。
///    通らなければ「動いている」扱いに落として、疑いは `suspicion` に残す
///    (**「作業中なのに停滞・異常」の根治**)
/// 5. supervisor が「動いていない」と言うなら待機 (画面の残骸に釣られない)
/// 6. supervisor が「動いている」と言うときだけ、画面末尾で中身を推定
pub fn classify_flow(
    running: bool,
    attention: bool,
    rate_limited: bool,
    sup: Option<supervisor::SessionState>,
    tail: &[String],
    flow: Flow,
) -> Read {
    use supervisor::SessionState as S;
    let read = Read::new;
    // 「動いている」ときの中身 — 画面末尾の表引き (推定と明示する)
    let working = |tail: &[String]| match classify_screen(tail) {
        Some((a, detail)) => read(a, Source::Screen, detail),
        // 進捗はあるが中身が読めない → 「思考中」(見張り由来と明示)
        None => read(Activity::Thinking, Source::Supervisor, String::new()),
    };
    if !running {
        return read(Activity::Exited, Source::Process, String::new());
    }
    if attention {
        let d = now_line(tail).map(clean_line).unwrap_or_default();
        return read(Activity::Approval, Source::Prompt, d);
    }
    if rate_limited {
        return read(Activity::RateLimited, Source::RateLimit, String::new());
    }
    match sup {
        Some(S::WaitingApproval) => read(Activity::Approval, Source::Supervisor, String::new()),
        Some(s @ (S::Stalled | S::Looping | S::Errored | S::Crashed)) => {
            if trouble_confirmed(s, flow) {
                read(Activity::Stalled, Source::Supervisor, tr(s.label()))
            } else {
                // 裏が取れていない = 根拠は画面の語だけ。人は呼ばず、
                // いま実際にやっていること (検証中・実行中…) として置く。
                let mut r = working(tail);
                r.suspicion = Some(s.label());
                r
            }
        }
        Some(S::Done) => read(Activity::Exited, Source::Supervisor, String::new()),
        // 「生きているが動いていない」— 画面に残っている過去のコマンド行に
        // 釣られて「実行中」と言わない。これが構造化信号を優先する意味。
        Some(S::Idle) => read(Activity::Idle, Source::Supervisor, String::new()),
        Some(S::Working) => working(tail),
        // まだ一度も観測していない起動直後
        None => read(Activity::Starting, Source::Process, String::new()),
    }
}

/// セッションの生存フラグ + supervisor 判定から列を決める **純関数**。
///
/// 優先順位は app.rs `coordinator_state` と同じ
/// (終了 > 承認待ち > レート制限 > supervisor 判定)。順序を揃えておかないと、
/// 看板の見た目と coordinator の配達判断が食い違って混乱する。
/// 確信度の床 ([`Read::lane`]) もここで通す。
pub fn column_for(
    running: bool,
    attention: bool,
    rate_limited: bool,
    sup: Option<supervisor::SessionState>,
) -> Column {
    classify(running, attention, rate_limited, sup, &[]).lane()
}

/// カードに出す状態ラベル (tr のキーになる日本語原文)。優先順位は [`classify`] と同じ。
pub fn state_label(
    running: bool,
    attention: bool,
    rate_limited: bool,
    sup: Option<supervisor::SessionState>,
) -> &'static str {
    classify(running, attention, rate_limited, sup, &[])
        .activity
        .label()
}

// ---------------------------------------------------------------------------
// レーン移動ポリシー (デバウンス)
// ---------------------------------------------------------------------------

/// 1 枚のカードのレーン位置を、ちらつかせずに動かす状態機械。
///
/// - 判定が現在のレーンと同じなら何もしない (候補は取り下げ)
/// - 違うレーンの判定が [`Column::hold_ms`] 以上続いたら初めて移動
/// - 承認待ち・完了は `hold_ms == 0` なので即座に動く
///   (「停滞・異常」だけは [`TROUBLE_HOLD_MS`] 続くことを求める)
///
/// 8 レーンでは 思考↔編集↔実行↔検証 の往復がそのままレーンをまたぐので、
/// この機械がいちばん効く場所になる ([`WORK_HOLD_MS`])。
/// 判定が 1 サンプル揺れただけでカードが飛ぶのを**構造的に不可能**にする。
#[derive(Clone, Copy, Debug)]
pub struct LaneTracker {
    lane: Column,
    /// 候補レーンと、その候補が続き始めた時刻
    pending: Option<(Column, u64)>,
    /// 現在のレーンへ着地した時刻 (ハイライト用)
    landed_ms: u64,
}

impl LaneTracker {
    pub fn new(lane: Column, now_ms: u64) -> Self {
        Self {
            lane,
            pending: None,
            landed_ms: now_ms,
        }
    }

    pub fn lane(&self) -> Column {
        self.lane
    }

    /// 現在のレーンへ着地した時刻 (テスト・デバッグ用)。
    #[allow(dead_code)]
    pub fn landed_ms(&self) -> u64 {
        self.landed_ms
    }

    /// 着地ハイライトの強さ (1.0 → 0.0)。0.0 なら描かなくてよい。
    pub fn land_glow(&self, now_ms: u64) -> f32 {
        let age = now_ms.saturating_sub(self.landed_ms);
        if age >= LAND_HIGHLIGHT_MS {
            return 0.0;
        }
        1.0 - age as f32 / LAND_HIGHLIGHT_MS as f32
    }

    /// 望ましいレーン `want` を与えて 1 ステップ進める。移動したら true。
    pub fn step(&mut self, want: Column, now_ms: u64) -> bool {
        if want == self.lane {
            self.pending = None;
            return false;
        }
        let hold = want.hold_ms();
        match self.pending {
            Some((c, since)) if c == want => {
                if now_ms.saturating_sub(since) >= hold {
                    self.lane = want;
                    self.landed_ms = now_ms;
                    self.pending = None;
                    return true;
                }
            }
            _ => {
                if hold == 0 {
                    self.lane = want;
                    self.landed_ms = now_ms;
                    self.pending = None;
                    return true;
                }
                self.pending = Some((want, now_ms));
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// 出力の勢い (スパークライン用の純粋な算術)
// ---------------------------------------------------------------------------

/// 前回サンプルに無かった行の文字数の合計 = このサンプルで「新しく出た量」。
///
/// 生バイト数は取れないので画面末尾の差分で代用する。スクロールしただけの行は
/// `prev` に居るので数えない (スピナーだけが回っている間は 0 に近づく)。
pub fn tail_delta(prev: &[String], cur: &[String]) -> u64 {
    cur.iter()
        .filter(|l| !prev.iter().any(|p| p == *l))
        .map(|l| l.chars().count() as u64)
        .sum()
}

/// 画面末尾を「数字とスピナー記号を潰した」表現へ正規化する。
///
/// supervisor と**同じ規則** ([`supervisor::normalize_line`]) を使うのが要点で、
/// 「経過秒だけが増えている」「点字スピナーが回っている」を進捗と数えない。
pub fn norm_tail(tail: &[String]) -> Vec<String> {
    tail.iter()
        .map(|l| supervisor::normalize_line(l, false))
        .filter(|l| !l.is_empty())
        .collect()
}

/// 前回に無かった行が現れたか = **意味のある進捗があったか** (純関数)。
///
/// スクロールで位置がずれただけの行は `prev` に居るので数えない。
pub fn has_new_content(prev: &[String], cur: &[String]) -> bool {
    cur.iter().any(|l| !prev.contains(l))
}

/// `(時刻, 量)` の列を直近 `window_ms` の `n` バケツへ畳む (古い → 新しい)。
///
/// 窓の外の点は無視する。`n == 0` なら空。純粋な算術なので表テストできる。
pub fn bucket_series(samples: &[(u64, u64)], now_ms: u64, window_ms: u64, n: usize) -> Vec<f32> {
    let mut out = vec![0.0_f32; n];
    if n == 0 || window_ms == 0 {
        return out;
    }
    let from = now_ms.saturating_sub(window_ms);
    let span = window_ms as f64 / n as f64;
    for (t, v) in samples {
        if *t < from || *t > now_ms {
            continue;
        }
        let idx = (((*t - from) as f64) / span) as usize;
        let idx = idx.min(n - 1);
        out[idx] += *v as f32;
    }
    out
}

// ---------------------------------------------------------------------------
// レイアウト (横 / 縦)
// ---------------------------------------------------------------------------

/// 看板の並べ方。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LayoutMode {
    /// 窓の縦横比で自動 ([`use_vertical`])
    #[default]
    Auto,
    /// レーンを横に並べる (従来)
    Horizontal,
    /// レーンを縦に積む (細く高い窓向け)
    Vertical,
}

impl LayoutMode {
    pub fn label(self) -> &'static str {
        match self {
            LayoutMode::Auto => "自動",
            LayoutMode::Horizontal => "横",
            LayoutMode::Vertical => "縦",
        }
    }

    /// 永続メモリ用の数値表現。
    pub fn to_u8(self) -> u8 {
        match self {
            LayoutMode::Auto => 0,
            LayoutMode::Horizontal => 1,
            LayoutMode::Vertical => 2,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => LayoutMode::Horizontal,
            2 => LayoutMode::Vertical,
            _ => LayoutMode::Auto,
        }
    }
}

// ---------------------------------------------------------------------------
// 幾何 (純関数)
//
// 「割り当てられた領域に必ず収まる」ことを egui を起こさずに固定するため、
// 寸法の判断だけをここへ切り出してある。ここが崩れると、レーンやライブペインが
// 右端から落ちて読めなくなる (実際に起きた)。間隔は panels::space の一本の
// 目盛りだけを使う。
// ---------------------------------------------------------------------------

use crate::panels::space;

/// レーン 1 本を読める最小幅 (カードのタイトルと状態文が入る)。
pub const LANE_MIN_W: f32 = 150.0;
/// レーンを読める幅で描ける上限 (広い窓で間延びさせない)。
pub const LANE_MAX_W: f32 = 300.0;
/// 空レーンを畳んだときの幅 (見出しの帯だけ)。
pub const LANE_EMPTY_W: f32 = 96.0;
/// 横モードを選ぶのに要る看板の幅。これを割ると縦モードへ落とす。
///
/// **本数から導く** (直書きしない)。以前は 900px の直書きで、レーン本数を変えた
/// 途端に右端が切れた。本数 × 最小幅 + 間隔で持てば、何本にしても追随する。
pub const BOARD_MIN_W: f32 = LANE_MIN_W * LANES as f32 + space::SM * (LANES as f32 - 1.0);
/// 右のアクティビティフィードの幅。
pub const FEED_W: f32 = 240.0;
/// 左レールの幅。
pub const RAIL_W: f32 = 210.0;
/// 分割バーの太さ。
pub const SPLIT_BAR: f32 = 4.0;

/// ライブペインを開いたときに看板へ残る幅 (**純関数**)。
///
/// `split` はライブペインの取り分 (0.2..0.7)。飾り (レール・フィード) は
/// [`main_rects`] が同じ規則で外すので、ここでは看板とライブの取り合いだけ見る。
pub fn board_width(avail_w: f32, live_open: bool, split: f32) -> f32 {
    if !live_open {
        return avail_w.max(0.0);
    }
    // 下限 > 上限になる幅があるので上限側を持ち上げる (`clamp` はパニックする)。
    let lo = 240.0_f32.min(avail_w.max(0.0));
    let hi = (avail_w * 0.7).max(lo);
    let live = (avail_w * split.clamp(0.2, 0.7)).clamp(lo, hi);
    (avail_w - live - space::SM * 2.0 - SPLIT_BAR).max(0.0)
}

/// 縦モードで描くべきか (**純関数**)。
///
/// 自動判定: [`LANES`] 本を横に並べると 1 レーン [`LANE_MIN_W`] 必要なので、
/// **看板に残る幅**が足りないか、縦長 (高さが幅の 0.95 倍超) なら縦に積む。
///
/// ライブペインを開くと看板の取り分はその 6 割前後まで落ちるので、
/// 同じ窓でも縦へ切り替わる — これが「端末を出したらレーンが画面外へ落ちる」
/// を防ぐ唯一の仕掛け。飾りを畳むだけでは足りない。
pub fn use_vertical(mode: LayoutMode, w: f32, h: f32, live_open: bool, split: f32) -> bool {
    match mode {
        LayoutMode::Horizontal => false,
        LayoutMode::Vertical => true,
        LayoutMode::Auto => board_width(w, live_open, split) < BOARD_MIN_W || h > w * 0.95,
    }
}

/// 主要域 (レール / 看板 / 分割バー / ライブ / フィード) の矩形。
///
/// `board` が `None` = **ライブペインの全画面** (レーンを 1 本も置かない)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MainRects {
    pub rail: Option<Rect>,
    pub board: Option<Rect>,
    pub splitter: Option<Rect>,
    pub live: Option<Rect>,
    pub feed: Option<Rect>,
}

impl MainRects {
    /// 看板だけを置いた状態 (飾りもライブペインも無し)。
    fn only_board(board: Rect) -> Self {
        Self {
            rail: None,
            board: Some(board),
            splitter: None,
            live: None,
            feed: None,
        }
    }

    /// ライブペインだけを置いた状態 (**全画面**)。看板もレールもフィードも出さない。
    fn only_live(live: Rect) -> Self {
        Self {
            rail: None,
            board: None,
            splitter: None,
            live: Some(live),
            feed: None,
        }
    }
}

#[cfg(test)]
impl MainRects {
    /// 実際に置いた矩形を順に返す (不変条件テスト専用の検査補助)。
    pub fn all(&self) -> Vec<Rect> {
        [self.rail, self.board, self.splitter, self.live, self.feed]
            .into_iter()
            .flatten()
            .collect()
    }
}

/// **主要域の割り付け** (純関数)。
///
/// 不変条件 (テストで固定):
/// - すべての矩形は `area` の中
/// - 矩形どうしは重ならない
///
/// 横モードは `[レール] 看板 [バー ライブ] [フィード]`、
/// 縦モードは `看板` を上、`バー ライブ` を下へ積む。
/// 飾り (レール・フィード) はライブペインを開いている間は畳む
/// — 看板が主役の画面で、装飾に幅を食わせない。
///
/// `full` = ライブペインの**全画面**。端末が領域を丸ごと使い、レーンは 1 本も
/// 置かない (`board == None`)。分割バーも飾りも無い — 出入りはヘッダーの ⊞/◎ と Esc。
pub fn main_rects(
    area: Rect,
    vertical: bool,
    live_open: bool,
    split: f32,
    full: bool,
) -> MainRects {
    let frac = split.clamp(0.2, 0.7);
    if live_open && full {
        return MainRects::only_live(area);
    }
    if vertical {
        let live_h = if live_open {
            // 下限が上限を越える窓 (パネルを 200px 以下まで縮めた状態) がある。
            // `f32::clamp` は min > max で **パニックする**ので、上限側を持ち上げる。
            let lo = 150.0_f32.min(area.height());
            let hi = (area.height() * 0.7).max(lo);
            (area.height() * frac).clamp(lo, hi)
        } else {
            0.0
        };
        let gap = if live_open {
            space::SM * 2.0 + SPLIT_BAR
        } else {
            0.0
        };
        let board_h = (area.height() - live_h - gap).max(0.0);
        let board = Rect::from_min_size(area.min, egui::vec2(area.width(), board_h));
        if !live_open {
            return MainRects::only_board(board);
        }
        let sy = board.bottom() + space::SM;
        let splitter = Rect::from_min_size(
            egui::pos2(area.left(), sy),
            egui::vec2(area.width(), SPLIT_BAR),
        );
        let ly = splitter.bottom() + space::SM;
        let live = Rect::from_min_max(
            egui::pos2(area.left(), ly),
            egui::pos2(area.right(), area.bottom()),
        );
        MainRects {
            rail: None,
            board: Some(board),
            splitter: Some(splitter),
            live: Some(live),
            feed: None,
        }
    } else {
        // 飾りは「開いていないとき」かつ「十分広いとき」だけ。
        let show_rail = !live_open && area.width() >= 1200.0;
        let show_feed = !live_open && area.width() >= 1000.0;
        let mut x = area.left();
        let rail = show_rail.then(|| {
            let r =
                Rect::from_min_size(egui::pos2(x, area.top()), egui::vec2(RAIL_W, area.height()));
            x = r.right() + space::SM;
            r
        });
        let right = if show_feed {
            area.right() - FEED_W - space::SM
        } else {
            area.right()
        };
        let feed = show_feed.then(|| {
            Rect::from_min_max(
                egui::pos2(area.right() - FEED_W, area.top()),
                egui::pos2(area.right(), area.bottom()),
            )
        });
        let rest = (right - x).max(0.0);
        let live_w = if live_open {
            // 縦モードと同じ理由で上限側を持ち上げる (min > max はパニック)。
            let lo = 240.0_f32.min(rest);
            let hi = (rest * 0.7).max(lo);
            (rest * frac).clamp(lo, hi)
        } else {
            0.0
        };
        let gap = if live_open {
            space::SM * 2.0 + SPLIT_BAR
        } else {
            0.0
        };
        let board_w = (rest - live_w - gap).max(0.0);
        let board = Rect::from_min_size(
            egui::pos2(x, area.top()),
            egui::vec2(board_w, area.height()),
        );
        if !live_open {
            return MainRects {
                rail,
                board: Some(board),
                splitter: None,
                live: None,
                feed,
            };
        }
        let sx = board.right() + space::SM;
        let splitter = Rect::from_min_size(
            egui::pos2(sx, area.top()),
            egui::vec2(SPLIT_BAR, area.height()),
        );
        let live = Rect::from_min_max(
            egui::pos2(splitter.right() + space::SM, area.top()),
            egui::pos2(right, area.bottom()),
        );
        MainRects {
            rail,
            board: Some(board),
            splitter: Some(splitter),
            live: Some(live),
            feed,
        }
    }
}

/// **レーン幅の割り付け** (純関数)。
///
/// 空のレーンは見出しの帯だけに畳んで [`LANE_EMPTY_W`] に固定し、浮いた幅を
/// 中身のあるレーンへ配る。8 本のころは均等割りだと 1 本 90px を切って
/// カードが読めず、下限で止めると右端の 2〜3 本が画面外へ落ちていた。
/// 規則は本数に依存しない — 空レーンを畳めば、8 本でも中身のある列が広く取れる。
///
/// 不変条件: 返す幅の総和 (+ レーン間の間隔) は `avail` を超えない
/// — ただし空レーンだけでも入らないほど狭い窓では、横スクロールに任せる
/// (それ以上潰すと帯の文字が読めないため)。
pub fn lane_widths(avail: f32, counts: &[usize]) -> Vec<f32> {
    let n = counts.len();
    if n == 0 {
        return Vec::new();
    }
    let gaps = space::SM * (n as f32 - 1.0);
    let full = counts.iter().filter(|c| **c > 0).count();
    let empty = n - full;
    let usable = (avail - gaps).max(0.0);
    if full == 0 {
        // 全部空 = 帯だけ。均等に割って (上限 LANE_EMPTY_W) 収める。
        let w = (usable / n as f32).min(LANE_EMPTY_W).max(1.0);
        return vec![w; n];
    }
    // 空レーンは帯の幅で固定。極端に狭い窓では均等割りまで縮める。
    let empty_w = LANE_EMPTY_W.min((usable / n as f32).max(1.0));
    let left = (usable - empty_w * empty as f32).max(0.0);
    let full_w = (left / full as f32).clamp(LANE_MIN_W, LANE_MAX_W);
    counts
        .iter()
        .map(|c| if *c > 0 { full_w } else { empty_w })
        .collect()
}

/// 見出し行を「アイコンだけ」に縮退させる幅のしきい値。
pub const HEADER_COMPACT_W: f32 = 1000.0;

/// 見出し行を縮退させるか (純関数)。
///
/// 狭い窓でラベル付きのまま並べると、右端の「＋ Agent」「✕ 閉じる」が
/// 画面外へ押し出されて押せなくなる。
pub fn header_compact(avail_w: f32) -> bool {
    avail_w < HEADER_COMPACT_W
}

/// ブロードキャスト入力欄の幅 (純関数)。
///
/// 残り幅から取り、入り切らないときは 0 (= 欄を出さない。📣 ボタンは残るので
/// 機能は失われない)。固定 220px だと狭い窓で右端が切れていた。
pub fn broadcast_input_width(remaining: f32) -> f32 {
    const MIN: f32 = 120.0;
    const MAX: f32 = 260.0;
    // 「連続稼働 …」など残りの表示にも席を残す
    let usable = remaining - space::SM * 2.0;
    if usable < MIN {
        0.0
    } else {
        usable.min(MAX)
    }
}

/// **KPI タイルの段組み** (純関数)。返り値は `(列数, タイル幅)`。
///
/// 狭い窓で幅を下限で止めると右端のタイル (「完了・終了」) が画面外へ落ちる。
/// 落とすくらいなら 2 段に折る。
pub fn kpi_grid(avail: f32, n: usize) -> (usize, f32) {
    const MIN_W: f32 = 120.0;
    let n = n.max(1);
    let mut cols = n;
    while cols > 1 {
        let w = (avail - space::SM * (cols as f32 - 1.0)) / cols as f32;
        if w >= MIN_W {
            break;
        }
        cols -= 1;
    }
    let w = ((avail - space::SM * (cols as f32 - 1.0)) / cols as f32).max(1.0);
    (cols, w)
}

/// KPI タイル 1 段の高さ (見出し + 数字 + スパークライン + 段の余白)。
pub const KPI_ROW_H: f32 = 64.0;
/// KPI を出しても看板そのものに残しておきたい最低の高さ。
/// これを割るなら**タイルより盤面**を優先する (看板が読めなければ意味がない)。
pub const BOARD_KEEP_H: f32 = 230.0;

/// KPI タイルを出すか (**純関数**)。
///
/// [`LANES`] 本ぶんのタイルは狭い窓で 2 段・3 段に折れる ([`kpi_grid`])。
/// 段数ぶんの高さを取ると看板が潰れるので、**折れた段数まで見て**決める
/// (高さの条件を定数 1 つで書くと、8 本のときだけ盤面が消える)。
pub fn show_kpi(avail_w: f32, avail_h: f32) -> bool {
    let (cols, _) = kpi_grid(avail_w, LANES);
    let rows = LANES.div_ceil(cols.max(1)) as f32;
    avail_h >= BOARD_KEEP_H + KPI_ROW_H * rows
}

// ---------------------------------------------------------------------------
// 選択 (セッション ID で持つ — カードが動いても消えない)
// ---------------------------------------------------------------------------

/// 選択中の ID をカード一覧に照らして解決する **純関数**。
///
/// - ID がまだ居れば、その ID と現在位置を返す
/// - 居なくなったら、消える前に居た位置 (`last_pos`) の近くへ寄せる
/// - カードが 1 枚も無ければ None
pub fn resolve_selection(
    sel: Option<u64>,
    last_pos: usize,
    cards: &[Card],
) -> Option<(u64, usize)> {
    if cards.is_empty() {
        return None;
    }
    if let Some(id) = sel {
        if let Some(pos) = cards.iter().position(|c| c.id == id) {
            return Some((id, pos));
        }
    }
    let pos = last_pos.min(cards.len() - 1);
    Some((cards[pos].id, pos))
}

/// 上下キーで選択を動かす **純関数**。`order` は画面上の並び (レーン順)。
pub fn move_selection(order: &[u64], cur: Option<u64>, delta: i32) -> Option<u64> {
    if order.is_empty() {
        return None;
    }
    let at = cur
        .and_then(|id| order.iter().position(|x| *x == id))
        .map(|p| p as i32);
    let next = match at {
        Some(p) => (p + delta).rem_euclid(order.len() as i32),
        None if delta >= 0 => 0,
        None => order.len() as i32 - 1,
    };
    Some(order[next as usize])
}

// ---------------------------------------------------------------------------
// カード / 集計 / アクティビティ / UI 状態 / アクション
// ---------------------------------------------------------------------------

/// セッション 1 件の看板カード。app.rs が毎フレーム写して渡す
/// (`idx` は `AgentManager.sessions` のインデックスで、このフレーム内でのみ有効)。
pub struct Card {
    pub idx: usize,
    pub id: u64,
    pub icon: String,
    pub title: String,
    /// アクティブセッション (紫枠) か
    pub active: bool,
    pub column: Column,
    /// 翻訳済みの状態ラベル
    pub state_label: String,
    pub uptime: String,
    pub unread: bool,
    pub rate_limited: Option<String>,
    pub attention: bool,
    pub running: bool,
    /// 見張り (supervisor.rs) の判定。[`classify`] が画面推定より優先して使う。
    pub sup: Option<supervisor::SessionState>,
    /// ⚡/🛡 (権限モード対応エージェントのみ、他は "")
    pub permission_badge: &'static str,
    /// **指名スーパーエージェント (指揮官) か** ([`config::SuperAgentConfig`])。
    ///
    /// 看板は同じ見た目のカードが並ぶので、「どれが指揮官か」は**枠の色**と
    /// 名前の前の [`COMMANDER_BADGE`] で一目で分かるようにする。
    /// 1 フレームに 1 体だけ true になる (app.rs `super_agent_session`)。
    pub commander: bool,
    /// 権限モード切替キーを送れるか
    pub can_cycle: bool,
    /// 画面末尾の「意味のある行」たち (時系列順)。
    ///
    /// **サンプリングしたフレームだけ中身が入る** ([`KanbanState::sample_due`])。
    /// 空のフレームでは [`KanbanState`] が前回ぶんを使うので、カードはちらつかない。
    pub tail_lines: Vec<String>,
    /// coordinator に割り当て中のタスク名
    pub task: Option<String>,
}

/// 画面末尾から「いま何をしているか」の一言を取り出す (最後の非空行)。
pub fn now_line(tail: &[String]) -> Option<&str> {
    tail.iter().rev().map(|l| l.trim()).find(|l| !l.is_empty())
}

/// レーンごとの人数の集計。KPI タイルとスループット履歴のデータ点になる。
///
/// **不変条件**: レーン別人数の合計 == `total` (二重計上も取りこぼしも無い)。
/// KPI タイルは [`Tally::lanes`] をそのまま出すので、この 1 つの表さえ守れば
/// 「タイルの数字を足すと総数を超える」は構造的に起きない。
/// `running` は**レーンではなくプロセスの生死**なので合計には入れない
/// — ヘッダーのチップだけで使う。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Tally {
    pub total: usize,
    /// 生きているプロセスの数 (レーン集計とは別の軸)
    pub running: usize,
    /// レーン別の人数 ([`Column::index`] 順)
    per: [usize; LANES],
}

impl Tally {
    /// レーンと人数の対 (表示順)。KPI タイルとテストが同じ 1 つの表を見る。
    pub fn lanes(&self) -> [(Column, usize); LANES] {
        let mut out = [(Column::Ready, 0usize); LANES];
        for (i, col) in COLUMNS.into_iter().enumerate() {
            out[i] = (col, self.per[col.index()]);
        }
        out
    }

    /// レーン人数の合計 (= `total` のはず)。二重計上の検出に使う。
    pub fn lane_sum(&self) -> usize {
        self.per.iter().sum()
    }

    /// 1 本ぶんの人数 (履歴からスパークラインを組むときに使う)。
    pub fn lane_count(&self, col: Column) -> usize {
        self.per[col.index()]
    }

    /// 「動いている」人数 = 作業系 4 本の合計 (折れ線とヘッダーのチップ)。
    /// **レーンではなくまとめ**なので、KPI タイルには出さない (二重計上になる)。
    pub fn work_sum(&self) -> usize {
        COLUMNS
            .into_iter()
            .filter(|c| c.is_work())
            .map(|c| self.per[c.index()])
            .sum()
    }

    /// 「人の手が要る」人数 = 承認待ち + 停滞・異常。
    pub fn attention_sum(&self) -> usize {
        COLUMNS
            .into_iter()
            .filter(|c| c.loud())
            .map(|c| self.per[c.index()])
            .sum()
    }
}

/// カード一覧から列集計を作る純関数 (カード自身の素の列を使う)。
/// 実描画は [`tally_lanes`] (デバウンス後のレーン) を使うので、こちらは
/// 「素の判定だけ見たい」テスト・外部呼び出し向け。
#[allow(dead_code)]
pub fn tally(cards: &[Card]) -> Tally {
    let lanes: Vec<Column> = cards.iter().map(|c| c.column).collect();
    tally_lanes(cards, &lanes)
}

/// デバウンス後の実表示レーン (`lanes[i]` が `cards[i]` のレーン) で集計する純関数。
pub fn tally_lanes(cards: &[Card], lanes: &[Column]) -> Tally {
    let mut t = Tally {
        total: cards.len(),
        ..Tally::default()
    };
    for (i, c) in cards.iter().enumerate() {
        if c.running {
            t.running += 1;
        }
        // 1 枚のカードはちょうど 1 本のレーンにだけ数える (二重計上しない)。
        t.per[lanes.get(i).copied().unwrap_or(c.column).index()] += 1;
    }
    // 不変条件: レーンの合計 = 総数 (二重計上も取りこぼしも無い)。
    debug_assert_eq!(
        t.lane_sum(),
        t.total,
        "レーン集計が総数と合わない (二重計上か取りこぼし)"
    );
    t
}

/// アクティビティフィードの 1 行。app.rs が supervisor の状態遷移履歴から作る。
pub struct ActivityEntry {
    /// 今からどれだけ前に起きたか (ms)
    pub age_ms: u64,
    pub icon: String,
    /// エージェント名
    pub title: String,
    /// 翻訳済みの本文 (例: 「作業中」になりました)
    pub text: String,
    /// ホバーで出す判定理由
    pub detail: String,
    /// 色分け用 (遷移先状態に対応する列)
    pub column: Column,
}

/// スループット履歴のデータ点。
#[derive(Clone, Copy)]
struct Sample {
    at_ms: u64,
    tally: Tally,
}

/// 2 秒に 1 点まで間引く (それ未満は最新点を上書き = チャートは常に「今」を指す)。
const SAMPLE_MS: u64 = 2_000;
/// 履歴の上限 (240 点 × 2 秒 = 約 8 分ぶんのウィンドウ)。
const MAX_SAMPLES: usize = 240;

/// 動いているエージェントが居るときの PTY 画面サンプリング間隔 (≈6.7Hz)。
const FAST_SAMPLE_MS: u64 = 150;
/// 誰も動いていないときのサンプリング間隔 (1Hz)。
const SLOW_SAMPLE_MS: u64 = 1_000;
/// 全員終了しているときの再描画間隔 (実質何もしない)。
const ASLEEP_REPAINT_MS: u64 = 2_000;
/// 出力の勢いスパークラインの窓 (30 秒) とバケツ数。
const PULSE_WINDOW_MS: u64 = 30_000;
const PULSE_BUCKETS: usize = 30;

/// 「進んでいる / 止まっている」を言い切るのに要る観測時間 (ms) — [`Track::flow`]。
///
/// 見張りの停滞判定は 180 秒の窓を見るので、こちらはその 1/12 で足りる。
/// 短すぎるとツール呼び出しの合間の数秒の静けさで「止まった」と言ってしまい、
/// 長すぎると本当に固まった相手を見逃す。
const FLOW_WINDOW_MS: u64 = 15_000;

/// 1 セッションぶんの追跡状態 (レーン位置・アクティビティ・出力の勢い)。
#[derive(Clone, Debug)]
pub struct Track {
    lane: LaneTracker,
    /// いまのアクティビティと、それが始まった時刻 (経過タイマー)
    activity: Activity,
    since_ms: u64,
    source: Source,
    detail: String,
    /// 段位不足で採らなかった見張りの異常判定 (カードに ⚠ で出す)
    suspicion: Option<&'static str>,
    /// 直近に触ったファイル / 走らせたコマンド (分かった時点で更新)
    last_file: String,
    last_cmd: String,
    /// 最後にサンプルした画面末尾 (カードの一言・ホバープレビューの元)
    tail: Vec<String>,
    /// 最後にサンプルした画面末尾の**正規化表現** ([`norm_tail`])。
    /// 「新しい中身が出たか」の判定はこちらで行う (スピナーに騙されない)。
    norm: Vec<String>,
    /// 最後に**意味のある進捗**があった時刻
    progress_ms: Option<u64>,
    /// このカードを最初に観測した時刻 (窓が埋まったかの判断に使う)
    born_ms: u64,
    /// 出力の勢い `(時刻, 新規文字数)`
    pulse: Vec<(u64, u64)>,
}

impl Track {
    fn new(read: &Read, now_ms: u64) -> Self {
        Self {
            lane: LaneTracker::new(read.lane(), now_ms),
            activity: read.activity,
            since_ms: now_ms,
            source: read.source,
            detail: read.detail.clone(),
            suspicion: read.suspicion,
            last_file: String::new(),
            last_cmd: String::new(),
            tail: Vec::new(),
            norm: Vec::new(),
            progress_ms: None,
            born_ms: now_ms,
            pulse: Vec::new(),
        }
    }

    /// **生の出力ストリームの事実** — 見張りの異常判定の裏取りに使う ([`Flow`])。
    ///
    /// - 直近 [`FLOW_WINDOW_MS`] に新しい中身が出た → `Live`
    /// - 同じ時間ぶん観測していて 1 行も増えていない → `Silent`
    /// - まだ観測が足りない → `Unknown` (判断材料にしない = 従来通りの扱い)
    pub fn flow(&self, now_ms: u64) -> Flow {
        if self
            .progress_ms
            .is_some_and(|p| now_ms.saturating_sub(p) <= FLOW_WINDOW_MS)
        {
            return Flow::Live;
        }
        if now_ms.saturating_sub(self.born_ms) >= FLOW_WINDOW_MS && !self.norm.is_empty() {
            return Flow::Silent;
        }
        Flow::Unknown
    }

    /// 現在のアクティビティが続いている時間 (ms)。
    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.since_ms)
    }

    /// 直近 30 秒の出力の勢い (古い → 新しい)。
    pub fn pulse_series(&self, now_ms: u64) -> Vec<f32> {
        bucket_series(&self.pulse, now_ms, PULSE_WINDOW_MS, PULSE_BUCKETS)
    }

    /// いまの判定を [`Read`] として組み直す (根拠の文言を 1 か所で作るため)。
    fn read(&self) -> Read {
        Read {
            activity: self.activity,
            source: self.source,
            detail: self.detail.clone(),
            suspicion: self.suspicion,
        }
    }

    /// このカードを**いまのレーンへ入れた根拠** (ホバーで出す 1 行)。
    pub fn reason(&self) -> String {
        self.read().reason()
    }

    /// 直近 3 秒に新しい出力があったか (LIVE 表示・サンプリング速度の判断)。
    fn recently_noisy(&self, now_ms: u64) -> bool {
        self.pulse
            .iter()
            .any(|(t, v)| *v > 0 && now_ms.saturating_sub(*t) <= 3_000)
    }
}

/// 看板画面の UI 状態 (app.rs が保持する)。
#[derive(Default)]
pub struct KanbanState {
    pub broadcast_input: String,
    /// ✏ 指示入力を開いているカード (セッション id。index は次フレームでずれ得る)
    pub prompt_for: Option<u64>,
    pub prompt_input: String,
    /// 入力欄を開いた直後に一度だけフォーカスを移す
    prompt_focus: bool,
    /// スループット/スパークラインの履歴
    samples: Vec<Sample>,
    /// セッション id → 追跡状態
    tracks: HashMap<u64, Track>,
    /// 選択中カード (**セッション id**。レーン移動でも消えない)
    selected: Option<u64>,
    /// 選択カードが最後に居た並び順の位置 (消えたときの寄せ先)
    sel_pos: usize,
    /// ライブペイン (端末) を**開いている**か。
    ///
    /// 既定は閉じる。以前は「選択があれば開く」だったので、看板から 1 体
    /// 起動しただけで端末が半分の幅を占め、レーンが画面外へ落ちていた
    /// (「起動したら画面が組み替わってどこを見ればいいか分からない」)。
    /// 開くのは**明示的な操作のときだけ** — Enter / 👁 / カードのダブルクリック。
    live_open: bool,
    /// ライブペインを**全画面**にしているか (レーンを隠して端末が領域を占める)。
    ///
    /// 選択にもスクロール位置にも触らない純粋な見せ方の切替なので、
    /// 出入りしても「どのカードを見ていたか」は失われない。永続メモリに残る。
    live_full: bool,
    live_full_dirty: bool,
    /// 永続メモリから読み戻し済みか (読み戻しは 1 回だけ)
    live_full_loaded: bool,
    /// 次のフレームでライブペインへフォーカスを移す (Enter)
    live_focus_req: bool,
    /// 新しく現れたカードへスクロールして知らせる (起動直後の 1 フレーム)
    scroll_to_sel: bool,
    /// 前フレームのライブペインの egui Id (Esc を board へ返すために覚える)
    live_id: Option<egui::Id>,
    /// レイアウト (None = 永続メモリから未読込)
    layout: Option<LayoutMode>,
    layout_dirty: bool,
    /// ライブペインの取り分 (0.2..0.7)。None = 永続メモリから未読込
    split: Option<f32>,
    split_dirty: bool,
    /// 最後に PTY 画面をサンプルした時刻
    last_sample_ms: Option<u64>,
    /// 直近のサンプルで「動いている」と判定したか (サンプリング速度に効く)
    busy: bool,
    /// 稼働中のセッションが 1 つでもあるか (居なければ寝る)
    any_running: bool,
    /// 着地アニメーションが走っているか (走っている間だけ高頻度描画)
    animating: bool,
}

impl KanbanState {
    /// 集計を履歴へ記録する。呼び出しは毎フレームで良い (内部で間引く)。
    pub fn record_sample(&mut self, now_ms: u64, t: Tally) {
        match self.samples.last_mut() {
            Some(last) if now_ms.saturating_sub(last.at_ms) < SAMPLE_MS => last.tally = t,
            _ => self.samples.push(Sample {
                at_ms: now_ms,
                tally: t,
            }),
        }
        if self.samples.len() > MAX_SAMPLES {
            let drop = self.samples.len() - MAX_SAMPLES;
            self.samples.drain(..drop);
        }
    }

    /// **PTY 画面を読み直してよいフレームか。**
    ///
    /// app.rs はこれが false のあいだ `screen_tail_lines` を呼ばない
    /// (= 看板を開けっぱなしでも毎フレーム parser をロックしない)。
    /// 動いている間だけ速く回し、静かなら 1 秒に 1 回で足りる。
    pub fn sample_due(&mut self, now_ms: u64) -> bool {
        let interval = if self.busy {
            FAST_SAMPLE_MS
        } else {
            SLOW_SAMPLE_MS
        };
        match self.last_sample_ms {
            Some(last) if now_ms.saturating_sub(last) < interval => false,
            _ => {
                self.last_sample_ms = Some(now_ms);
                true
            }
        }
    }

    /// 次に再描画を要求するまでの ms。無条件の再描画をしないための唯一の窓口。
    pub fn next_repaint_ms(&self) -> u64 {
        if self.animating {
            33
        } else if self.busy {
            FAST_SAMPLE_MS
        } else if self.any_running {
            SLOW_SAMPLE_MS
        } else {
            ASLEEP_REPAINT_MS
        }
    }

    /// 追跡状態を 1 ステップ進める。`fresh` が true のフレームだけ
    /// `cards[..].tail_lines` に新しい画面が入っている。
    ///
    /// 戻り値は `lanes[i]` = `cards[i]` の**表示**レーン (デバウンス済み)。
    pub fn update_tracks(&mut self, cards: &[Card], now_ms: u64, fresh: bool) -> Vec<Column> {
        let mut lanes = Vec::with_capacity(cards.len());
        let mut busy = false;
        let mut animating = false;
        let mut any_running = false;
        // 追跡がまだ空 = 看板を開いた最初のフレーム。ここでは全カードが
        // 「初めて見るカード」なので、**起動の合図として扱ってはいけない**。
        let first_fill = self.tracks.is_empty();
        // このフレームで初めて現れたカード。ループの中で選択を書き換えると
        // 最後の 1 枚が必ず勝ってしまうので、数え終わってから決める。
        let mut arrived: Vec<u64> = Vec::new();
        for c in cards {
            if c.running {
                any_running = true;
            }
            // 画面が来ていないフレームは、前回サンプルした画面で判定する
            // (構造化信号 — 生死・承認・レート制限 — は毎フレーム最新)。
            // `Read` は所有値なので、ここで tracks の借用は閉じる (複製しない)。
            let rl = c.rate_limited.is_some();
            let read = {
                let prev = self.tracks.get(&c.id);
                // 生の出力ストリームの裏取り — 見張りの「停滞/エラー多発」が
                // 実際の進捗と矛盾していないかを、ここで初めて突き合わせる。
                let flow = prev.map(|t| t.flow(now_ms)).unwrap_or_default();
                match (fresh, prev) {
                    (true, _) => {
                        classify_flow(c.running, c.attention, rl, c.sup, &c.tail_lines, flow)
                    }
                    (false, Some(t)) => {
                        classify_flow(c.running, c.attention, rl, c.sup, &t.tail, flow)
                    }
                    (false, None) => classify_flow(c.running, c.attention, rl, c.sup, &[], flow),
                }
            };

            if !self.tracks.contains_key(&c.id) {
                arrived.push(c.id);
            }
            let track = self
                .tracks
                .entry(c.id)
                .or_insert_with(|| Track::new(&read, now_ms));

            if fresh {
                let delta = tail_delta(&track.tail, &c.tail_lines);
                track.pulse.push((now_ms, delta));
                let from = now_ms.saturating_sub(PULSE_WINDOW_MS);
                track.pulse.retain(|(t, _)| *t >= from);
                track.tail = c.tail_lines.clone();
                // **意味のある進捗**の時刻を更新する (スピナー/カウンタは潰す)。
                let norm = norm_tail(&c.tail_lines);
                if has_new_content(&track.norm, &norm) {
                    track.progress_ms = Some(now_ms);
                }
                track.norm = norm;
            }

            if track.activity != read.activity {
                track.activity = read.activity;
                track.since_ms = now_ms;
            }
            track.source = read.source;
            track.detail = read.detail.clone();
            track.suspicion = read.suspicion;
            match read.activity {
                Activity::Editing if !read.detail.is_empty() => {
                    track.last_file = read.detail.clone();
                }
                Activity::Running | Activity::Verifying if !read.detail.is_empty() => {
                    track.last_cmd = read.detail.clone();
                }
                _ => {}
            }

            // **確信度の床を通したレーン**へ寄せる
            // (画面推定だけで承認待ち/停滞・異常/完了にしない)。
            track.lane.step(read.lane(), now_ms);
            lanes.push(track.lane.lane());
            if track.lane.land_glow(now_ms) > 0.0 {
                animating = true;
            }
            if read.activity.is_busy() || track.recently_noisy(now_ms) {
                busy = true;
            }
        }
        // 新しく起動したエージェントは、**画面を組み替えず**選択とスクロールだけで
        // 「これが始まった」を示す (端末を勝手に開くと看板が半分に潰れる)。
        //
        // 示すのは **1 体だけ増えたとき** に限る。初回の総取り込みや、ワークスペース
        // 復元でまとめて現れたときにも書き換えていたため、ループの最後 =
        // 起動順で一番最後のエージェントが必ず選ばれ、ユーザーがどれを選んでも
        // そこへ吸われていた。誰の意思でもない選択は動かさない方が驚きが少ない。
        if !first_fill && arrived.len() == 1 {
            self.selected = Some(arrived[0]);
            self.scroll_to_sel = true;
        }
        // 消えたセッションの追跡は捨てる (無限に太らせない)
        self.tracks
            .retain(|id, _| cards.iter().any(|c| c.id == *id));
        self.busy = busy;
        self.animating = animating;
        self.any_running = any_running;
        lanes
    }

    /// 選択中のセッション id (テスト・app.rs 用)。
    #[allow(dead_code)]
    pub fn selected(&self) -> Option<u64> {
        self.selected
    }

    /// 全画面の出入り。**選択・スクロール・追跡状態には一切触らない**
    /// (触ると「全画面にしたら見ていたカードが変わった」が起きる)。
    pub fn set_live_full(&mut self, on: bool) {
        if self.live_full != on {
            self.live_full = on;
            self.live_full_dirty = true;
        }
    }

    /// 選択をカード一覧に照らして解決し、位置を覚え直す。
    fn sync_selection(&mut self, cards: &[Card]) -> Option<u64> {
        // 初回はアクティブ (紫枠) のカードを選んでおく — 開いた瞬間に
        // 「いま見ているエージェント」の中身が出る方が迷わない。
        if self.selected.is_none() {
            self.selected = cards.iter().find(|c| c.active).map(|c| c.id);
        }
        match resolve_selection(self.selected, self.sel_pos, cards) {
            Some((id, pos)) => {
                self.selected = Some(id);
                self.sel_pos = pos;
                Some(id)
            }
            None => {
                self.selected = None;
                None
            }
        }
    }

    /// 追跡状態の参照 (テスト用。描画側は `tracks` を直接見る)。
    #[allow(dead_code)]
    pub fn track(&self, id: u64) -> Option<&Track> {
        self.tracks.get(&id)
    }
}

/// UI から返る要求。実行は app.rs (`kanban_ui`) 側。
pub enum KanbanAction {
    /// プリセット index のエージェントを起動
    Launch(usize),
    /// アクティブ (紫枠) をこのセッションへ
    Select(usize),
    /// 下部パネルへフォーカス
    Focus(usize),
    Approve(usize),
    Deny(usize),
    Restart(usize),
    Remove(usize),
    CyclePermission(usize),
    /// このセッションへ指示を 1 行送信 (Enter 付き)
    Send {
        idx: usize,
        text: String,
    },
    Broadcast(String),
    OpenCockpit,
    Close,
}

// ---------------------------------------------------------------------------
// 表示用の純関数 (テスト対象)
// ---------------------------------------------------------------------------

/// 連続稼働時間の表示 (例: `1日 04:13:01` / `00:41:09`)。
pub fn fmt_uptime(ms: u64) -> String {
    let s = ms / 1000;
    let (d, h, m, sec) = (s / 86_400, (s / 3600) % 24, (s / 60) % 60, s % 60);
    if d > 0 {
        trf(
            "{d}日 {rest}",
            &[
                ("d", d.to_string()),
                ("rest", format!("{h:02}:{m:02}:{sec:02}")),
            ],
        )
    } else {
        format!("{h:02}:{m:02}:{sec:02}")
    }
}

/// 相対時刻の表示 (例: `たった今` / `30秒前` / `5分前` / `2時間前`)。
pub fn fmt_age(ms: u64) -> String {
    let s = ms / 1000;
    if s < 5 {
        tr("たった今")
    } else if s < 60 {
        trf("{n}秒前", &[("n", s.to_string())])
    } else if s < 3600 {
        trf("{n}分前", &[("n", (s / 60).to_string())])
    } else {
        trf("{n}時間前", &[("n", (s / 3600).to_string())])
    }
}

/// 現在のアクティビティの経過時間 (例: `0:07` / `2:31` / `1:04:00`)。
pub fn fmt_elapsed(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// エージェントごとの安定したアバター色。テーマの ANSI 明色 (8..16) から選ぶので
/// テーマを変えれば追随する (リテラルを持たない)。
fn avatar_color(theme: &Theme, id: u64) -> Color32 {
    // 9,10,11,13,14,12 = 赤/緑/黄/紫/水/青の明色。灰 (8,15) は避ける。
    const SLOTS: [usize; 6] = [9, 10, 11, 13, 14, 12];
    theme.ansi[SLOTS[(id % SLOTS.len() as u64) as usize]]
}

// ---------------------------------------------------------------------------
// 描画
// ---------------------------------------------------------------------------

/// 永続メモリのキー (config.rs は他所有なので egui の memory に持つ)。
fn layout_id() -> egui::Id {
    egui::Id::new("zv-kanban-layout-mode")
}

fn split_id() -> egui::Id {
    egui::Id::new("zv-kanban-live-split")
}

/// ライブペインを全画面にしていたかの永続キー。
fn live_full_id() -> egui::Id {
    egui::Id::new("zv-kanban-live-full")
}

/// ライブペインへ選択カードの端末を描くためのコールバック。
///
/// app.rs が `terminal::draw` を呼ぶだけの実装を渡す。看板側は端末を再実装しない。
/// 返り値の `Response` があればフォーカス移動 (Enter/Esc) に使う。
pub type LiveDraw<'a> = &'a mut dyn FnMut(&mut egui::Ui, usize) -> Option<egui::Response>;

/// 看板画面を描き、押された操作を返す。
///
/// `now_ms` は supervisor の経過時計 (アプリ起動からの ms)。連続稼働表示・
/// アクティビティの相対時刻・スループット履歴のサンプリングを全部この 1 本で賄う。
/// `fresh_tail` は「このフレームの `cards[..].tail_lines` が新しいか」
/// (app.rs が [`KanbanState::sample_due`] で決める)。
#[allow(clippy::too_many_arguments)]
pub fn ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    cards: &[Card],
    presets: &[(String, String)],
    activity: &[ActivityEntry],
    now_ms: u64,
    fresh_tail: bool,
    live: LiveDraw<'_>,
) -> Vec<KanbanAction> {
    let mut acts: Vec<KanbanAction> = Vec::new();

    // 永続メモリ (レイアウト・分割比) の読み込み / 書き戻し
    let ctx = ui.ctx().clone();
    if st.layout.is_none() {
        let v = ctx.data_mut(|d| *d.get_persisted_mut_or(layout_id(), 0_u8));
        st.layout = Some(LayoutMode::from_u8(v));
    }
    if st.layout_dirty {
        let v = st.layout.unwrap_or_default().to_u8();
        ctx.data_mut(|d| d.insert_persisted(layout_id(), v));
        st.layout_dirty = false;
    }
    if st.split.is_none() {
        let v = ctx.data_mut(|d| *d.get_persisted_mut_or(split_id(), 0.38_f32));
        st.split = Some(v.clamp(0.2, 0.7));
    }
    if st.split_dirty {
        let v = st.split.unwrap_or(0.38);
        ctx.data_mut(|d| d.insert_persisted(split_id(), v));
        st.split_dirty = false;
    }
    if !st.live_full_loaded {
        // 起動直後の 1 回だけ読み戻す (ライブペイン自体は自動では開かない —
        // 「起動しただけで画面が激変しない」ため、開くのは明示操作のときだけ)。
        st.live_full = ctx.data_mut(|d| *d.get_persisted_mut_or(live_full_id(), false));
        st.live_full_loaded = true;
    }
    if st.live_full_dirty {
        let v = st.live_full;
        ctx.data_mut(|d| d.insert_persisted(live_full_id(), v));
        st.live_full_dirty = false;
    }

    let lanes = st.update_tracks(cards, now_ms, fresh_tail);
    let t = tally_lanes(cards, &lanes);
    st.record_sample(now_ms, t);
    // 無条件の再描画はしない。動きがあるときだけ速く回す (アイドル時は 1〜2 秒)。
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(st.next_repaint_ms()));

    egui::Frame::none()
        .inner_margin(egui::Margin::same(10.0))
        .show(ui, |ui| {
            // ボトムパネルは「中身が実際に使った矩形」を次フレームの高さとして
            // 保存する (egui 0.29) ので、中身が割り当てを埋め切らないと上端の
            // リサイズバーが毎フレームずり落ちる。内部レイアウトの計算誤差に
            // 依存しないよう、先に割り当てられた全高を消費しておく。
            ui.set_min_height(ui.available_height());
            let wide = ui.available_width();
            let tall = ui.available_height();
            let split = st.split.unwrap_or(0.38);
            // ライブペインの開閉は**見せ方そのもの**を変える (開いていれば看板の
            // 取り分が減るので縦モードへ落とす)。判定は純関数側で一元化する。
            let live_on = st.live_open && !cards.is_empty();
            // 全画面のあいだは端末に領域を明け渡す (レーンも KPI も折れ線も出さない)。
            let full = live_on && st.live_full;
            let vertical = use_vertical(st.layout.unwrap_or_default(), wide, tall, live_on, split);
            let show_kpi = !full && show_kpi(wide, tall);
            let show_chart = !full && !vertical && !live_on && tall >= 470.0;

            header_ui(st, ui, theme, &t, presets, now_ms, vertical, &mut acts);

            if cards.is_empty() {
                empty_ui(ui, theme, presets, &mut acts);
                return;
            }

            // 画面上の並び (レーン順 → レーン内はカード順) — 上下キーの移動順
            let order: Vec<u64> = COLUMNS
                .iter()
                .flat_map(|col| {
                    cards
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| lanes.get(*i).copied() == Some(*col))
                        .map(|(_, c)| c.id)
                        .collect::<Vec<_>>()
                })
                .collect();
            keyboard_ui(st, ui, &order, &mut acts, cards);
            let selected = st.sync_selection(cards);

            if show_kpi {
                kpi_ui(ui, theme, st, &t);
                ui.add_space(space::SM);
            }

            let chart_h = if show_chart { 96.0 + space::SM } else { 0.0 };
            let area = ui.available_rect_before_wrap().intersect(ui.clip_rect());
            let main = Rect::from_min_max(
                area.min,
                egui::pos2(area.right(), (area.bottom() - chart_h).max(area.top())),
            );
            // **矩形はすべて純関数が決める** (main_rects)。egui のレイアウトに
            // 引き算を任せると、飾りの幅ぶんライブペインが右へはみ出して
            // 端末の行が切れる (実際に起きた)。
            let r = main_rects(main, vertical, live_on, split, full);
            let main_h = main.height();

            if let Some(rr) = r.rail {
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rr), |ui| {
                    rail_ui(st, ui, theme, cards, &lanes, rr.height(), now_ms, &mut acts);
                });
            }
            // 全画面のときは看板を**そもそも置かない** (畳んだ空白も作らない)。
            if let Some(br) = r.board {
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(br), |ui| {
                    if vertical {
                        board_vertical_ui(
                            st,
                            ui,
                            theme,
                            cards,
                            &lanes,
                            br.height(),
                            now_ms,
                            &mut acts,
                        );
                    } else {
                        board_ui(st, ui, theme, cards, &lanes, br.height(), now_ms, &mut acts);
                    }
                });
            }
            if let Some(sr) = r.splitter {
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(sr), |ui| {
                    splitter_ui(
                        st,
                        ui,
                        theme,
                        !vertical,
                        if vertical { main_h } else { main.width() },
                    );
                });
            }
            if let Some(lr) = r.live {
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(lr), |ui| {
                    live_pane_ui(
                        st,
                        ui,
                        theme,
                        cards,
                        &lanes,
                        selected,
                        lr.size(),
                        now_ms,
                        live,
                        &mut acts,
                    );
                });
            }
            if let Some(fr) = r.feed {
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(fr), |ui| {
                    feed_ui(ui, theme, activity, fr.width(), fr.height(), now_ms);
                });
            }
            // 割り当てた領域を消費して、下のチャートを主要域の下から描く。
            ui.advance_cursor_after_rect(main);

            if show_chart {
                ui.add_space(space::SM);
                chart_ui(ui, theme, st);
            }
        });

    acts
}

/// キーボード操作。テキスト入力中とライブペインにフォーカスがある間は
/// 選択移動を奪わない (端末へ打った文字が選択を動かしたら事故になる)。
fn keyboard_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    order: &[u64],
    acts: &mut Vec<KanbanAction>,
    cards: &[Card],
) {
    let focus = ui.ctx().memory(|m| m.focused());
    let live_focused = focus.is_some() && focus == st.live_id;

    if live_focused {
        // Esc で看板へ戻る。端末が同じ Esc を PTY へ流さないよう先に取り上げる。
        let esc = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        if esc {
            // 全画面なら**まず全画面を降りる** (看板が見えるところまで 1 段ずつ戻す)。
            if st.live_full {
                st.set_live_full(false);
            } else if let Some(id) = st.live_id {
                ui.ctx().memory_mut(|m| m.surrender_focus(id));
            }
        }
        return;
    }
    if focus.is_some() {
        return; // ブロードキャスト欄・指示欄などを打っている
    }
    // 端末にフォーカスが無くても Esc で全画面から出られる (出口を 1 つに限らない)。
    if st.live_full && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        st.set_live_full(false);
        return;
    }

    let (up, down, enter) = ui.input(|i| {
        (
            i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::K),
            i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::J),
            i.key_pressed(egui::Key::Enter),
        )
    });
    let delta = i32::from(down) - i32::from(up);
    if delta != 0 {
        if let Some(id) = move_selection(order, st.selected, delta) {
            // 選択の移動では**ライブペインを開かない**。開くと画面が組み替わり、
            // 上下キーで見比べているあいだ看板がずっと半分に潰れる。
            st.selected = Some(id);
            st.scroll_to_sel = true;
            if let Some(c) = cards.iter().find(|c| c.id == id) {
                acts.push(KanbanAction::Select(c.idx));
            }
        }
    }
    // Enter = 「この端末を開いて入力する」という明示的な操作。ここだけが開く。
    if enter && st.selected.is_some() {
        st.live_open = true;
        st.live_focus_req = true;
    }
}

/// 看板とライブペインの間のドラッグバー。
fn splitter_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    horizontal: bool,
    span: f32,
) {
    let size = if horizontal {
        egui::vec2(4.0, ui.available_height())
    } else {
        egui::vec2(ui.available_width(), 4.0)
    };
    let resp = ui.allocate_response(size, egui::Sense::drag());
    let hot = resp.hovered() || resp.dragged();
    ui.painter().rect_filled(
        resp.rect,
        2.0,
        if hot { theme.accent } else { theme.border },
    );
    resp.clone().on_hover_cursor(if horizontal {
        egui::CursorIcon::ResizeHorizontal
    } else {
        egui::CursorIcon::ResizeVertical
    });
    if resp.dragged() && span > 1.0 {
        let d = resp.drag_delta();
        // 看板が左/上なので、バーを進める方向はライブペインを縮める方向
        let delta = if horizontal { -d.x } else { -d.y } / span;
        let next = (st.split.unwrap_or(0.38) + delta).clamp(0.2, 0.7);
        st.split = Some(next);
        st.split_dirty = true;
    }
}

/// 選択中エージェントのライブ端末 (cmux 風)。端末描画は app.rs のクロージャに任せる。
#[allow(clippy::too_many_arguments)]
fn live_pane_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    cards: &[Card],
    lanes: &[Column],
    selected: Option<u64>,
    size: egui::Vec2,
    now_ms: u64,
    live: LiveDraw<'_>,
    acts: &mut Vec<KanbanAction>,
) {
    let Some(id) = selected else { return };
    let Some((i, c)) = cards.iter().enumerate().find(|(_, c)| c.id == id) else {
        return;
    };
    let lane = lanes.get(i).copied().unwrap_or(c.column);
    let color = lane.color(theme);
    // 幅は**呼び出し側が決めた割り当て**をそのまま使う。`available_width` を
    // 見ると飾りの取り分まで飲み込んで右へはみ出す (端末の行が切れる)。
    let (w, height) = (size.x, size.y);
    ui.allocate_ui_with_layout(
        egui::vec2(w, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::Frame::none()
                .fill(theme.panel)
                .stroke(Stroke::new(1.0_f32, theme.accent))
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin::same(space::SM))
                .show(ui, |ui| {
                    ui.set_width(w - space::SM * 2.0);
                    ui.set_min_height(height - space::SM * 2.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("●").size(10.0).color(color));
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("{} {}", c.icon, c.title))
                                    .size(12.5)
                                    .strong()
                                    .color(theme.text),
                            )
                            .truncate(),
                        );
                        if let Some(tr_) = st.tracks.get(&id) {
                            chip(ui, color, &tr(tr_.activity.label()));
                        }
                        // ヘッダーのボタンはラベル文字列から Id を作るので、
                        // カード側の同じ絵文字と衝突しないよう囲っておく。
                        ui.push_id("kanban-live-header", |ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .small_button("✕")
                                        .on_hover_text(tr("ライブ表示を閉じる"))
                                        .clicked()
                                    {
                                        st.live_open = false;
                                    }
                                    // 全画面トグル。豆腐にならない字だけを使う
                                    // (⊞ / ◎ は app.rs の記号テストが担保している)。
                                    let full = st.live_full;
                                    let (icon, tip) = if full {
                                        ("◎", "全画面をやめて看板へ戻す (Esc)")
                                    } else {
                                        ("⊞", "この端末を全画面にする (レーンは隠れます)")
                                    };
                                    if ui
                                        .selectable_label(full, icon)
                                        .on_hover_text(tr(tip))
                                        .clicked()
                                    {
                                        // 選択もスクロールも触らない — 見せ方だけを変える。
                                        st.set_live_full(!full);
                                    }
                                    if ui
                                        .small_button("🔍")
                                        .on_hover_text(tr("下部パネルにフォーカス"))
                                        .clicked()
                                    {
                                        acts.push(KanbanAction::Focus(i));
                                    }
                                    ui.label(
                                RichText::new(if st.live_full {
                                    tr("Esc: 全画面をやめる / ◎ でも戻せます")
                                } else {
                                    tr("↑↓/jk: 選択 / Enter: 入力 / Esc: 看板へ戻る / ⊞: 全画面")
                                })
                                .size(9.5)
                                .color(theme.text_dim),
                            );
                                },
                            );
                        });
                    });
                    ui.add_space(4.0);
                    let inner_h = (ui.available_height()).max(60.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), inner_h),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            let resp = live(ui, i);
                            match resp {
                                Some(r) => {
                                    st.live_id = Some(r.id);
                                    if st.live_focus_req {
                                        r.request_focus();
                                        st.live_focus_req = false;
                                    }
                                }
                                None => {
                                    st.live_id = None;
                                    st.live_focus_req = false;
                                    ui.label(
                                        RichText::new(tr("この端末はいま表示できません"))
                                            .size(11.0)
                                            .color(theme.text_dim),
                                    );
                                }
                            }
                        },
                    );
                    let _ = now_ms;
                });
        },
    );
}

/// 角丸チップ (稼働数バッジなど)。
fn chip(ui: &mut egui::Ui, color: Color32, text: &str) {
    egui::Frame::none()
        .fill(color.gamma_multiply(0.18))
        .rounding(egui::Rounding::same(9.0))
        .inner_margin(egui::Margin::symmetric(8.0, 3.0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.0).strong().color(color));
        });
}

/// 赤く脈打つ LIVE インジケータ。
fn live_dot(ui: &mut egui::Ui, theme: &Theme, now_ms: u64) {
    let pulse = ((now_ms as f32 / 500.0).sin() * 0.35 + 0.65).clamp(0.0, 1.0);
    ui.label(
        RichText::new("● LIVE")
            .size(10.0)
            .strong()
            .color(theme.err.gamma_multiply(pulse)),
    );
}

#[allow(clippy::too_many_arguments)]
fn header_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    t: &Tally,
    presets: &[(String, String)],
    now_ms: u64,
    vertical: bool,
    acts: &mut Vec<KanbanAction>,
) {
    // 狭い窓では文字を落としてアイコンだけにする。右端でボタンが切れて
    // 押せなくなる (「＋ Agent」「✕ 閉じる」が見えない) のを防ぐ。
    let compact = header_compact(ui.available_width());
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(if compact { "📋" } else { "📋 FLEET KANBAN" })
                .size(if compact { 15.0 } else { 17.0 })
                .strong()
                .color(theme.text),
        )
        .on_hover_text(tr(
            "カードは状態が変わると自動でレーンを移動します — ドラッグは不要です",
        ));
        if !compact {
            ui.label(
                RichText::new("Autonomous Ops Console")
                    .size(11.0)
                    .color(theme.text_dim),
            );
        }
        chip(
            ui,
            theme.ok,
            &trf("{n} 稼働中", &[("n", t.running.to_string())]),
        );
        // レーン別の内訳 (0 のレーンは出さない — いま意味のある数だけ目に入る)。
        // 「稼働中」はレーンではなくプロセスの生死なので、上のチップとは別の軸。
        //
        // 8 本ぶんのチップを常に並べると、狭い窓でヘッダーが折り返して
        // 盤面が下へ押し出される。**人の手が要る 2 本は必ず**、作業系は
        // 幅に余裕があるときだけ細かく出す (無ければまとめて「作業中 n」)。
        let detailed = !compact && ui.available_width() >= HEADER_COMPACT_W;
        let lane_chip = |ui: &mut egui::Ui, col: Column, n: usize| {
            if n > 0 {
                chip(
                    ui,
                    col.color(theme),
                    &format!("{} {} {}", col.icon(), tr(col.title()), n),
                );
            }
        };
        if detailed {
            for col in COLUMNS.into_iter().filter(|c| c.loud() || c.is_work()) {
                lane_chip(ui, col, t.lane_count(col));
            }
        } else {
            // 狭い窓ではまとめる (「人の手が要る」だけは必ず出す)。
            if t.attention_sum() > 0 {
                chip(
                    ui,
                    Column::Trouble.color(theme),
                    &trf("⚠ 要対応 {n}", &[("n", t.attention_sum().to_string())]),
                );
            }
            if t.work_sum() > 0 {
                chip(
                    ui,
                    Column::Running.color(theme),
                    &trf("{n} 作業中", &[("n", t.work_sum().to_string())]),
                );
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(tr("✕ 閉じる")).clicked() {
                acts.push(KanbanAction::Close);
            }
            // レイアウト切替 (自動 / 横 / 縦)。選択は egui の永続メモリへ。
            let mode = st.layout.unwrap_or_default();
            let icon = match mode {
                LayoutMode::Auto => "🖥",
                LayoutMode::Horizontal => "▤",
                LayoutMode::Vertical => "▥",
            };
            let mode_label = if compact {
                icon.to_string()
            } else {
                format!("{icon} {}", tr(mode.label()))
            };
            ui.menu_button(mode_label, |ui| {
                for m in [
                    LayoutMode::Auto,
                    LayoutMode::Horizontal,
                    LayoutMode::Vertical,
                ] {
                    if ui.selectable_label(mode == m, tr(m.label())).clicked() {
                        st.layout = Some(m);
                        st.layout_dirty = true;
                        ui.close_menu();
                    }
                }
            })
            .response
            .on_hover_text(if vertical {
                tr("いまは縦モード — レーンを縦に積み、カードは全幅で出します")
            } else {
                tr("いまは横モード — レーンを横に並べます")
            });
            if ui
                .button(if compact { "🎛" } else { "🎛 Cockpit" })
                .on_hover_text(tr("Cockpit へ切替"))
                .clicked()
            {
                acts.push(KanbanAction::OpenCockpit);
            }
            ui.menu_button(if compact { "＋" } else { "＋ Agent" }, |ui| {
                for (i, (icon, name)) in presets.iter().enumerate() {
                    if ui.button(format!("{icon} {name}")).clicked() {
                        acts.push(KanbanAction::Launch(i));
                        ui.close_menu();
                    }
                }
            })
            .response
            .on_hover_text(tr("エージェントを起動"));
            let send = ui
                .button(if compact { "📣" } else { "📣 送信" })
                .on_hover_text(tr("全エージェントへブロードキャスト"));
            // 入力欄は**残り幅から**取る。固定 220px だと右端で切れて、
            // 「連続稼働」やボタンが画面外へ落ちる。
            let input_w = broadcast_input_width(ui.available_width());
            let input = (input_w > 0.0).then(|| {
                ui.add(
                    egui::TextEdit::singleline(&mut st.broadcast_input)
                        .id(egui::Id::new("kanban-broadcast-input"))
                        .desired_width(input_w)
                        .hint_text(tr("全エージェントへブロードキャスト…")),
                )
            });
            let enter = input
                .as_ref()
                .is_some_and(|i| i.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if (send.clicked() || enter) && !st.broadcast_input.trim().is_empty() {
                acts.push(KanbanAction::Broadcast(
                    st.broadcast_input.trim().to_string(),
                ));
                st.broadcast_input.clear();
            }
            // Enter でフォーカスが外れるので戻し、連続入力できるようにする
            // (空入力の Enter でも戻す — 戻さないと入力欄が死んだように見える)
            if enter {
                if let Some(i) = input.as_ref() {
                    i.request_focus();
                }
            }
            if !compact {
                ui.label(
                    RichText::new(trf("連続稼働 {t}", &[("t", fmt_uptime(now_ms))]))
                        .size(11.0)
                        .color(theme.text_dim),
                );
            }
        });
    });
    ui.add_space(space::SM);
}

/// 空状態 — **カード 1 枚を利用可能領域の中央に**。
///
/// 旧実装は可用高の 25% を上詰めしていたので、窓の高さが変わるたびに位置が
/// 上下し、低い窓では起動ボタンが下端を突き抜けて押せなかった。
fn empty_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    presets: &[(String, String)],
    acts: &mut Vec<KanbanAction>,
) {
    // 見えている範囲で中央寄せする (clip_rect と交差させないと下へ突き抜ける)。
    let avail = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    let l = crate::panels::empty_card(avail, presets.len());
    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(l.card), |ui| {
        egui::Frame::none()
            .fill(theme.panel)
            .stroke(Stroke::new(1.0_f32, theme.border))
            .rounding(egui::Rounding::same(10.0))
            .inner_margin(egui::Margin::same(space::MD))
            .show(ui, |ui| {
                ui.set_width(l.card.width() - space::MD * 2.0);
                let mut body = |ui: &mut egui::Ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("📋").size(52.0));
                        ui.label(
                            RichText::new(tr("エージェントがまだいません"))
                                .size(18.0)
                                .color(theme.text),
                        );
                        ui.label(
                            RichText::new(tr("プリセットから並列セッションを起動しましょう"))
                                .color(theme.text_dim),
                        );
                    });
                    ui.add_space(space::MD);
                    for row in 0..l.rows {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = space::SM;
                            let used = l.btn_w * l.cols as f32 + space::SM * (l.cols as f32 - 1.0);
                            ui.add_space(((ui.available_width() - used) * 0.5).max(0.0));
                            for col in 0..l.cols {
                                let i = row * l.cols + col;
                                let Some((icon, name)) = presets.get(i) else {
                                    break;
                                };
                                let label = format!("{icon} {name}");
                                if ui
                                    .add_sized(
                                        [l.btn_w, crate::panels::EMPTY_BTN_H],
                                        egui::Button::new(RichText::new(&label).size(13.0))
                                            .wrap_mode(egui::TextWrapMode::Truncate),
                                    )
                                    .on_hover_text(&label)
                                    .clicked()
                                {
                                    acts.push(KanbanAction::Launch(i));
                                }
                            }
                        });
                        ui.add_space(space::SM);
                    }
                };
                if l.scroll {
                    egui::ScrollArea::vertical()
                        .id_salt("kanban-empty-state")
                        .auto_shrink([false, false])
                        .show(ui, &mut body);
                } else {
                    body(ui);
                }
            });
    });
}

// ---------------------------------------------------------------------------
// KPI タイル
// ---------------------------------------------------------------------------

/// KPI タイル = **[`LANES`] 本のレーンそのもの**。合計は必ず総数に一致する
/// ([`Tally::lane_sum`])。以前は「稼働中」タイルが他と重なっていて、
/// 数字を足すと総数を超えていた (二重計上)。
/// 「稼働中」はレーンではないのでヘッダーのチップへ移した。
fn kpi_ui(ui: &mut egui::Ui, theme: &Theme, st: &KanbanState, t: &Tally) {
    let gap = space::SM;
    // 狭い窓ではタイルを 1 枚ずつ潰さず**段に折る** ([`show_kpi`] が段数まで見て
    // 出し入れを決める)。下限で止めると右端の「完了」が画面外へ落ちる。
    let (cols, tile_w) = kpi_grid(ui.available_width(), LANES);
    let tiles = t.lanes();
    let inner_pad = space::SM + 1.0;
    for chunk in tiles.chunks(cols.max(1)) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (col, value) in chunk {
                let color = col.color(theme);
                // 要対応だけは 0 でないとき枠を強くする — ここが空かどうかが要点。
                let hot = col.loud() && *value > 0;
                egui::Frame::none()
                    .fill(if hot {
                        theme.panel.lerp_to_gamma(color, 0.12)
                    } else {
                        theme.panel
                    })
                    .stroke(Stroke::new(
                        if hot { 1.5_f32 } else { 1.0_f32 },
                        if hot { color } else { theme.border },
                    ))
                    .rounding(egui::Rounding::same(8.0))
                    .inner_margin(egui::Margin::same(inner_pad))
                    .show(ui, |ui| {
                        ui.set_width((tile_w - inner_pad * 2.0).max(1.0));
                        let label = col.title();
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!("{} {}", col.icon(), tr(label)))
                                        .size(11.0)
                                        .color(if hot { color } else { theme.text_dim }),
                                )
                                .truncate(),
                            )
                            .on_hover_text(tr(col.hint()));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(RichText::new("●").size(9.0).color(color));
                                },
                            );
                        });
                        ui.label(
                            RichText::new(value.to_string())
                                .size(21.0)
                                .strong()
                                .color(if hot { color } else { theme.text }),
                        );
                        let values: Vec<f32> = st
                            .samples
                            .iter()
                            .map(|s| s.tally.lane_count(*col) as f32)
                            .collect();
                        sparkline(ui, 14.0, color, &values);
                    });
            }
        });
        if cols < LANES {
            ui.add_space(space::XS);
        }
    }
}

/// 小さな折れ線 (KPI タイルの足元)。データが 1 点以下ならベースラインだけ描く。
fn sparkline(ui: &mut egui::Ui, height: f32, color: Color32, values: &[f32]) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    if values.len() < 2 {
        painter.line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            Stroke::new(1.0_f32, color.gamma_multiply(0.4)),
        );
        return;
    }
    let max = values.iter().cloned().fold(1.0_f32, f32::max);
    let pts: Vec<Pos2> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = rect.left() + rect.width() * i as f32 / (values.len() - 1) as f32;
            let y = rect.bottom() - (rect.height() - 2.0) * (v / max);
            egui::pos2(x, y)
        })
        .collect();
    painter.add(egui::Shape::line(pts, Stroke::new(1.5_f32, color)));
}

// ---------------------------------------------------------------------------
// 左レール: エージェント一覧
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn rail_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    cards: &[Card],
    lanes: &[Column],
    height: f32,
    now_ms: u64,
    acts: &mut Vec<KanbanAction>,
) {
    let w = 208.0;
    ui.allocate_ui_with_layout(
        egui::vec2(w, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::Frame::none()
                .fill(theme.panel)
                .stroke(Stroke::new(1.0_f32, theme.border))
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin::same(8.0))
                .show(ui, |ui| {
                    ui.set_width(w - 16.0);
                    ui.set_min_height(height - 16.0);
                    ui.label(
                        RichText::new(tr("AIエージェント"))
                            .size(11.0)
                            .strong()
                            .color(theme.text_dim),
                    );
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .id_salt("kanban-rail")
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            for (i, c) in cards.iter().enumerate() {
                                let lane = lanes.get(i).copied().unwrap_or(c.column);
                                rail_entry_ui(st, ui, theme, c, lane, now_ms, acts);
                                ui.add_space(4.0);
                            }
                        });
                });
        },
    );
}

fn rail_entry_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    c: &Card,
    lane: Column,
    now_ms: u64,
    acts: &mut Vec<KanbanAction>,
) {
    let col_color = lane.color(theme);
    let stroke = if st.selected == Some(c.id) {
        Stroke::new(1.5_f32, theme.accent)
    } else if c.active {
        Stroke::new(1.0_f32, theme.accent_soft)
    } else {
        Stroke::new(1.0_f32, Color32::TRANSPARENT)
    };
    let (act_label, doing) = match st.tracks.get(&c.id) {
        Some(t) => (tr(t.activity.label()), status_line(t)),
        None => (c.state_label.clone(), String::new()),
    };
    let cell = ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(("kanban-rail-entry", c.id))
            .sense(egui::Sense::click()),
        |ui| {
            egui::Frame::none()
                .fill(theme.panel_alt)
                .stroke(stroke)
                .rounding(egui::Rounding::same(7.0))
                .inner_margin(egui::Margin::same(6.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        // アバター (円 + 絵文字)
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
                        let color = avatar_color(theme, c.id);
                        ui.painter().circle_filled(rect.center(), 11.0, color);
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            &c.icon,
                            egui::FontId::proportional(12.0),
                            theme.term_bg,
                        );
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&c.title)
                                        .size(12.0)
                                        .strong()
                                        .color(theme.text),
                                )
                                .truncate(),
                            );
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(&act_label)
                                        .size(10.0)
                                        .strong()
                                        .color(col_color),
                                );
                                if let Some(t) = st.tracks.get(&c.id) {
                                    ui.label(
                                        RichText::new(fmt_elapsed(t.elapsed_ms(now_ms)))
                                            .size(9.5)
                                            .color(theme.text_dim),
                                    );
                                }
                            });
                        });
                    });

                    let doing = if doing.is_empty() {
                        c.task.clone().unwrap_or_default()
                    } else {
                        doing
                    };
                    if !doing.is_empty() {
                        ui.add(
                            egui::Label::new(RichText::new(doing).size(10.0).color(theme.text_dim))
                                .truncate(),
                        );
                    }
                });
        },
    );
    if cell.response.clicked() {
        // 単なる選択。ライブペインは開かない (画面を組み替えない)。
        st.selected = Some(c.id);
        acts.push(KanbanAction::Select(c.idx));
    }
}

// ---------------------------------------------------------------------------
// 看板本体
// ---------------------------------------------------------------------------

/// カードの 1 行状態文 (「いま何をしているか」)。追跡状態から組み立てる純関数。
pub fn status_line(t: &Track) -> String {
    let label = tr(t.activity.label());
    let detail = t.detail.trim();
    if detail.is_empty() {
        label
    } else {
        format!("{label}: {detail}")
    }
}

/// 横モード: レーンを横に並べる。
#[allow(clippy::too_many_arguments)]
fn board_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    cards: &[Card],
    lanes: &[Column],
    height: f32,
    now_ms: u64,
    acts: &mut Vec<KanbanAction>,
) {
    let members: Vec<Vec<&Card>> = COLUMNS
        .iter()
        .map(|col| {
            cards
                .iter()
                .enumerate()
                .filter(|(i, c)| lanes.get(*i).copied().unwrap_or(c.column) == *col)
                .map(|(_, c)| c)
                .collect()
        })
        .collect();
    let counts: Vec<usize> = members.iter().map(Vec::len).collect();
    // 空レーンは帯だけに畳み、浮いた幅を中身のあるレーンへ配る。
    // 均等割りのままだと 8 本が下限で止まり、右端の 2〜3 本が画面外へ落ちる。
    let widths = lane_widths(ui.available_width(), &counts);
    egui::ScrollArea::horizontal()
        .id_salt("kanban-board-h")
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = space::SM;
                for (i, col) in COLUMNS.into_iter().enumerate() {
                    let w = widths.get(i).copied().unwrap_or(LANE_MIN_W);
                    column_ui(st, ui, theme, col, &members[i], w, height, now_ms, acts);
                }
            });
        });
}

/// 縦モード: レーンを縦に積み、カードは全幅の 1 列。
///
/// 細く高い窓 (サブディスプレイの縦置き・スマホからのリモート) では、
/// [`LANES`] 本を横に並べても 1 本が読める幅に届かない。縦に積めばカードが全幅を
/// 使えるので、状態文・最後のファイル・最後のコマンド・勢いのグラフが
/// 1 枚に収まる。空のレーンは帯だけに畳んで、視線が要点に届くようにする。
#[allow(clippy::too_many_arguments)]
fn board_vertical_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    cards: &[Card],
    lanes: &[Column],
    height: f32,
    now_ms: u64,
    acts: &mut Vec<KanbanAction>,
) {
    let w = ui.available_width();
    egui::Frame::none()
        .fill(theme.panel)
        .stroke(Stroke::new(1.0_f32, theme.border))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::same(7.0))
        .show(ui, |ui| {
            ui.set_width(w - 14.0);
            ui.set_min_height(height - 14.0);
            egui::ScrollArea::vertical()
                .id_salt("kanban-board-v")
                .auto_shrink(false)
                .show(ui, |ui| {
                    for col in COLUMNS {
                        let members: Vec<&Card> = cards
                            .iter()
                            .enumerate()
                            .filter(|(i, c)| lanes.get(*i).copied().unwrap_or(c.column) == col)
                            .map(|(_, c)| c)
                            .collect();
                        lane_header_ui(ui, theme, col, members.len());
                        if members.is_empty() {
                            ui.add_space(3.0);
                            continue;
                        }
                        ui.add_space(4.0);
                        for c in members {
                            card_ui(st, ui, theme, c, col, now_ms, true, acts);
                            ui.add_space(5.0);
                        }
                        ui.add_space(4.0);
                    }
                });
        });
}

/// 縦モードのレーン帯 (色 + 絵文字 + 名前 + 件数)。
///
/// 中身のある「人を呼ぶレーン」だけ帯を濃くして、縦に積んでも目に飛び込むようにする。
fn lane_header_ui(ui: &mut egui::Ui, theme: &Theme, col: Column, count: usize) {
    let color = col.color(theme);
    let hot = col.loud() && count > 0;
    let alpha = if count == 0 {
        0.06
    } else if hot {
        0.30
    } else {
        0.16
    };
    egui::Frame::none()
        .fill(color.gamma_multiply(alpha))
        .stroke(if hot {
            Stroke::new(1.0_f32, color)
        } else {
            Stroke::NONE
        })
        .rounding(egui::Rounding::same(5.0))
        .inner_margin(egui::Margin::symmetric(7.0, 3.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width() - 14.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(col.icon()).size(11.0).color(color));
                ui.label(
                    RichText::new(tr(col.title()))
                        .size(11.5)
                        .strong()
                        .color(if count == 0 { theme.text_dim } else { color }),
                )
                .on_hover_text(tr(col.hint()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(if count == 0 {
                            "—".to_string()
                        } else {
                            count.to_string()
                        })
                        .size(10.5)
                        .color(theme.text_dim),
                    );
                });
            });
        });
}

#[allow(clippy::too_many_arguments)]
fn column_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    col: Column,
    members: &[&Card],
    width: f32,
    height: f32,
    now_ms: u64,
    acts: &mut Vec<KanbanAction>,
) {
    let color = col.color(theme);
    let empty = members.is_empty();
    // 中身のある「人を呼ぶレーン」だけは枠も背景も強くする。この画面の存在理由なので、
    // 目を走らせずに気づけること自体が仕様。
    let hot = col.loud() && !empty;
    // **空のレーンは見出しだけに畳む。** 高さを丸ごと取ると、空カラムが
    // 窓の底まで伸びて「読むところが無い縦線」が並ぶ (実際に起きた)。
    let pad = space::XS + 3.0;
    ui.allocate_ui_with_layout(
        egui::vec2(width, if empty { 0.0 } else { height }),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::Frame::none()
                .fill(if empty {
                    theme.panel_alt
                } else if hot {
                    theme.panel.lerp_to_gamma(color, 0.10)
                } else {
                    theme.panel
                })
                .stroke(Stroke::new(
                    if hot { 1.5_f32 } else { 1.0_f32 },
                    if hot { color } else { theme.border },
                ))
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin::same(pad))
                .show(ui, |ui| {
                    ui.set_width(width - pad * 2.0);
                    if !empty {
                        ui.set_min_height(height - pad * 2.0);
                    }
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(col.icon()).size(10.0).color(color));
                        ui.add(
                            egui::Label::new(
                                RichText::new(tr(col.title()))
                                    .size(if hot { 12.5 } else { 12.0 })
                                    .strong()
                                    .color(if empty {
                                        theme.text_dim
                                    } else if hot {
                                        color
                                    } else {
                                        theme.text
                                    }),
                            )
                            .truncate(),
                        )
                        .on_hover_text(tr(col.hint()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(if empty {
                                    "—".to_string()
                                } else {
                                    members.len().to_string()
                                })
                                .size(11.0)
                                .color(theme.text_dim),
                            );
                        });
                    });
                    if empty {
                        // 帯だけで終わり — 高さも幅も予約しない。
                        return;
                    }
                    // 見出し下の色帯 (列の識別)。人を呼ぶレーンは太く・濃く。
                    let (line, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), if hot { 3.0 } else { 2.0 }),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(
                        line,
                        1.0_f32,
                        color.gamma_multiply(if hot { 1.0 } else { 0.6 }),
                    );
                    ui.add_space(space::XS);

                    egui::ScrollArea::vertical()
                        .id_salt(("kanban-col", col.title()))
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            for c in members {
                                card_ui(st, ui, theme, c, col, now_ms, false, acts);
                                ui.add_space(space::XS);
                            }
                        });
                });
        },
    );
}

/// 出力の勢いバー (直近 30 秒)。折れ線より棒の方が「呼吸している」感じが出る。
fn pulse_bars(ui: &mut egui::Ui, height: f32, color: Color32, values: &[f32]) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    if values.is_empty() {
        return;
    }
    let painter = ui.painter();
    let max = values.iter().cloned().fold(1.0_f32, f32::max);
    let n = values.len() as f32;
    let bw = (rect.width() / n).max(1.0);
    for (i, v) in values.iter().enumerate() {
        let h = (rect.height() * (v / max)).max(if *v > 0.0 { 1.5 } else { 0.0 });
        if h <= 0.0 {
            continue;
        }
        let x = rect.left() + bw * i as f32;
        let bar = egui::Rect::from_min_max(
            egui::pos2(x, rect.bottom() - h),
            egui::pos2(x + (bw - 1.0).max(1.0), rect.bottom()),
        );
        // 新しいバケツほど濃く = 「いま」が目に入る
        let a = 0.35 + 0.65 * (i as f32 / n);
        painter.rect_filled(bar, 0.0, color.gamma_multiply(a));
    }
    painter.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0_f32, color.gamma_multiply(0.25)),
    );
}

/// 1 枚のカード。`wide` は縦モード (全幅) かどうか。
#[allow(clippy::too_many_arguments)]
fn card_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    c: &Card,
    lane: Column,
    now_ms: u64,
    wide: bool,
    acts: &mut Vec<KanbanAction>,
) {
    let color = lane.color(theme);
    let selected = st.selected == Some(c.id);
    // 着地ハイライト: 新しいレーンへ来た直後だけ枠が光って目を引く
    let glow = st
        .tracks
        .get(&c.id)
        .map(|t| t.lane.land_glow(now_ms))
        .unwrap_or(0.0);
    let stroke = if selected {
        Stroke::new(2.0_f32, theme.accent)
    } else if glow > 0.0 {
        Stroke::new(1.0 + glow, color.gamma_multiply(0.4 + 0.6 * glow))
    } else if c.active {
        Stroke::new(1.5_f32, theme.accent_soft)
    } else {
        Stroke::new(1.0_f32, theme.border)
    };
    let fill = if glow > 0.0 {
        theme.panel_alt.lerp_to_gamma(color, 0.18 * glow)
    } else {
        theme.panel_alt
    };

    let track = st.tracks.get(&c.id).cloned();
    // 👁 ボタンでライブ表示を閉じたか。閉じた直後に外側のクリック判定が
    // 開き直してしまうのを防ぐため、内側から持ち帰る。
    let cell = ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(("kanban-card", c.id))
            .sense(egui::Sense::click()),
        |ui| {
            let mut eye_toggled = false;
            egui::Frame::none()
                .fill(fill)
                .stroke(stroke)
                .rounding(egui::Rounding::same(7.0))
                .inner_margin(egui::Margin::same(7.0))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 3.0;

                        // ── 1 行目: 名前と稼働時間 ──
                        ui.horizontal(|ui| {
                            let dot = if c.running { "●" } else { "○" };
                            ui.label(RichText::new(dot).size(10.0).color(color));
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!(
                                        "{}{} {}",
                                        c.permission_badge, c.icon, c.title
                                    ))
                                    .size(12.5)
                                    .strong()
                                    .color(theme.text),
                                )
                                .truncate(),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        RichText::new(&c.uptime).size(9.5).color(theme.text_dim),
                                    );
                                },
                            );
                        });

                        // ── 2 行目: 状態チップ + 経過 + 出どころ ──
                        ui.horizontal(|ui| {
                            let (label, elapsed, source) = match &track {
                                Some(t) => (
                                    tr(t.activity.label()),
                                    fmt_elapsed(t.elapsed_ms(now_ms)),
                                    Some(t.source),
                                ),
                                None => (c.state_label.clone(), String::new(), None),
                            };
                            egui::Frame::none()
                                .fill(color.gamma_multiply(0.16))
                                .rounding(egui::Rounding::same(5.0))
                                .inner_margin(egui::Margin::symmetric(6.0, 1.0))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(&label).size(10.0).strong().color(color),
                                    );
                                });
                            if !elapsed.is_empty() {
                                ui.label(
                                    RichText::new(format!("⏱ {elapsed}"))
                                        .size(9.5)
                                        .color(theme.text_dim),
                                )
                                .on_hover_text(tr("この状態が続いている時間"));
                            }
                            // 画面テキストからの推定は必ず「推定」(≈) と断る。
                            // 人を呼ぶレーンのカードは「**何がここへ入れたか**」まで見せる。
                            if let Some(s) = source {
                                let mark = if s.is_guess() { "≈" } else { "✓" };
                                let why = match (&track, lane.loud()) {
                                    (Some(t), true) => {
                                        trf("このレーンに居る理由: {why}", &[("why", t.reason())])
                                    }
                                    _ => trf("判定の出どころ: {src}", &[("src", tr(s.label()))]),
                                };
                                ui.label(RichText::new(mark).size(9.0).color(if s.is_guess() {
                                    theme.warn
                                } else {
                                    theme.text_dim
                                }))
                                .on_hover_text(why);
                            }
                            // 見張りが疑ったが、出力が続いていたので採らなかった判定。
                            // **レーンは動かさないが、握り潰しもしない。**
                            if let Some(sus) = track.as_ref().and_then(|t| t.suspicion) {
                                ui.label(RichText::new("⚠").size(9.5).color(theme.warn))
                                    .on_hover_text(trf(
                                        "見張りの疑い: {s} — ただし出力は進んでいるので\
                                         「停滞・異常」へは動かしていません",
                                        &[("s", tr(sus))],
                                    ));
                            }
                            if let Some(line) = &c.rate_limited {
                                ui.label(RichText::new("⏳").size(10.0).color(theme.warn))
                                    .on_hover_text(trf(
                                        "レート制限/使用上限: {line}",
                                        &[("line", line.clone())],
                                    ));
                            }
                            if c.unread {
                                ui.label(RichText::new("◆").size(8.0).color(theme.accent))
                                    .on_hover_text(tr("最後に見てから新しい出力があります"));
                            }
                        });

                        // ── 細かい作業内容の 1 行 (例: 「編集中: src/foo.rs」) ──
                        //
                        // レーンで「何をしているか」は分かるので、ここは
                        // **対象**を出す (どのファイル・どのコマンドか)。
                        // レーンが動くまでの間もラベルと詳細は即座に更新される。
                        if let Some(t) = &track {
                            if !t.detail.trim().is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(status_line(t)).size(10.5).color(theme.text),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(t.reason());
                            }
                        }

                        if let Some(task) = &c.task {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!("📋 {task}"))
                                        .size(11.0)
                                        .color(theme.text),
                                )
                                .truncate(),
                            )
                            .on_hover_text(task);
                        }

                        // ── 直近の作業内容 (ファイル / コマンド) ──
                        if let Some(t) = &track {
                            if !t.last_file.is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(format!("✎ {}", t.last_file))
                                            .size(10.0)
                                            .monospace()
                                            .color(theme.accent),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(tr("直近に触ったファイル (画面からの推定)"));
                            }
                            if !t.last_cmd.is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(format!("▸ {}", t.last_cmd))
                                            .size(10.0)
                                            .monospace()
                                            .color(theme.ok),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(tr("直近のコマンド (画面からの推定)"));
                            }
                        }

                        // ── 画面末尾の一言 (ホバーで数行のライブプレビュー) ──
                        let tail: &[String] = track.as_ref().map(|t| &t.tail[..]).unwrap_or(&[]);
                        let doing = now_line(tail).unwrap_or("");
                        let doing_label = if doing.is_empty() {
                            RichText::new(tr("まだ出力がありません"))
                                .size(10.5)
                                .color(theme.text_dim)
                        } else {
                            RichText::new(format!("💬 {doing}"))
                                .size(10.5)
                                .monospace()
                                .color(theme.text_dim)
                        };
                        let resp = ui.add(egui::Label::new(doing_label).truncate());
                        if !tail.is_empty() {
                            resp.on_hover_ui(|ui| {
                                ui.set_max_width(460.0);
                                for line in tail {
                                    ui.label(
                                        RichText::new(line)
                                            .size(11.0)
                                            .monospace()
                                            .color(theme.text),
                                    );
                                }
                            });
                        }

                        // ── 出力の勢い (直近 30 秒) ──
                        if let Some(t) = &track {
                            let series = t.pulse_series(now_ms);
                            if series.iter().any(|v| *v > 0.0) || c.running {
                                pulse_bars(ui, if wide { 16.0 } else { 12.0 }, color, &series);
                            }
                        }

                        if c.attention {
                            ui.horizontal(|ui| {
                                if ui
                                    .button(RichText::new(tr("✅ 承認")).color(theme.ok))
                                    .on_hover_text(tr("画面のプロンプトに合った承認キーを送ります"))
                                    .clicked()
                                {
                                    acts.push(KanbanAction::Approve(c.idx));
                                }
                                if ui
                                    .button(RichText::new(tr("❌ 拒否")).color(theme.err))
                                    .clicked()
                                {
                                    acts.push(KanbanAction::Deny(c.idx));
                                }
                            });
                        }

                        // ボタン群はカード id で囲む。selectable_label / small_button は
                        // ラベル文字列から ID を自動生成するため、全カードで同じ絵文字を
                        // 使っているとどのカードを押しても最後の 1 枚が反応する
                        // (実際に「エージェントを選んでも一番最後が選ばれる」として報告された)。
                        ui.push_id(c.id, |ui| {
                            ui.horizontal(|ui| {
                                if ui
                                    .selectable_label(selected, "👁")
                                    .on_hover_text(tr("このエージェントのライブ画面を開く"))
                                    .clicked()
                                {
                                    // 👁 は明示的な操作なので、ここは開いてよい。
                                    // 同じカードの 👁 をもう一度押したら閉じる。
                                    let same = st.selected == Some(c.id);
                                    st.selected = Some(c.id);
                                    st.live_open = !(same && st.live_open);
                                    eye_toggled = true;
                                }
                                if ui
                                    .small_button("🔍")
                                    .on_hover_text(tr("下部パネルにフォーカス"))
                                    .clicked()
                                {
                                    acts.push(KanbanAction::Focus(c.idx));
                                }
                                let editing = st.prompt_for == Some(c.id);
                                if ui
                                    .selectable_label(editing, "✏")
                                    .on_hover_text(tr("このエージェントへ指示を送る"))
                                    .clicked()
                                {
                                    if editing {
                                        st.prompt_for = None;
                                    } else {
                                        st.prompt_for = Some(c.id);
                                        st.prompt_input.clear();
                                        st.prompt_focus = true;
                                    }
                                }
                                if c.can_cycle
                                    && ui
                                        .small_button("🛡")
                                        .on_hover_text(tr("権限モード切替を送信"))
                                        .clicked()
                                {
                                    acts.push(KanbanAction::CyclePermission(c.idx));
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .small_button("✕")
                                            .on_hover_text(tr("閉じる"))
                                            .clicked()
                                        {
                                            acts.push(KanbanAction::Remove(c.idx));
                                        }
                                        if ui
                                            .small_button("⟳")
                                            .on_hover_text(tr("再起動"))
                                            .clicked()
                                        {
                                            acts.push(KanbanAction::Restart(c.idx));
                                        }
                                    },
                                );
                            });
                        });

                        if st.prompt_for == Some(c.id) {
                            ui.horizontal(|ui| {
                                let input = ui.add(
                                    egui::TextEdit::singleline(&mut st.prompt_input)
                                        // カードが列を移動しても入力欄が生き残るよう
                                        // セッション id で Id を固定する
                                        .id(egui::Id::new(("kanban-prompt", c.id)))
                                        .desired_width((ui.available_width() - 56.0).max(80.0))
                                        .hint_text(tr("指示を入力… (Enter で送信)")),
                                );
                                if st.prompt_focus {
                                    input.request_focus();
                                    st.prompt_focus = false;
                                }
                                let enter = input.lost_focus()
                                    && ui.input(|i| i.key_pressed(egui::Key::Enter));
                                if (ui.button(tr("✏ 送信")).clicked() || enter)
                                    && !st.prompt_input.trim().is_empty()
                                {
                                    acts.push(KanbanAction::Send {
                                        idx: c.idx,
                                        text: st.prompt_input.trim().to_string(),
                                    });
                                    st.prompt_input.clear();
                                    st.prompt_for = None;
                                } else if enter {
                                    // 空 Enter は閉じずにフォーカスを戻す
                                    input.request_focus();
                                }
                            });
                        }
                    });
                });
            eye_toggled
        },
    );
    // 起動直後 / キー移動で選んだカードを視界へ入れる。**これが「何が始まったか」を
    // 示す唯一の手段** — 端末を勝手に開いて画面を組み替えることはしない。
    if selected && st.scroll_to_sel {
        cell.response.scroll_to_me(Some(egui::Align::Center));
        st.scroll_to_sel = false;
    }
    if !cell.inner
        && (cell.response.clicked()
            || (cell.response.contains_pointer() && ui.input(|i| i.pointer.primary_pressed())))
    {
        // 単なる選択。ライブペインは開かない (画面を組み替えない)。
        st.selected = Some(c.id);
        acts.push(KanbanAction::Select(c.idx));
    }
}

// ---------------------------------------------------------------------------
// 右レール: アクティビティフィード
// ---------------------------------------------------------------------------

fn feed_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    activity: &[ActivityEntry],
    width: f32,
    height: f32,
    now_ms: u64,
) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::Frame::none()
                .fill(theme.panel)
                .stroke(Stroke::new(1.0_f32, theme.border))
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin::same(8.0))
                .show(ui, |ui| {
                    ui.set_width(width - 16.0);
                    ui.set_min_height(height - 16.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(tr("アクティビティ"))
                                .size(11.0)
                                .strong()
                                .color(theme.text_dim),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            live_dot(ui, theme, now_ms);
                        });
                    });
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .id_salt("kanban-feed")
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            if activity.is_empty() {
                                ui.label(
                                    RichText::new(tr("まだ動きがありません"))
                                        .size(10.5)
                                        .color(theme.text_dim),
                                );
                            }
                            for e in activity.iter().take(60) {
                                ui.horizontal_top(|ui| {
                                    ui.label(
                                        RichText::new("●").size(8.0).color(e.column.color(theme)),
                                    );
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 1.0;
                                        let resp = ui.add(
                                            egui::Label::new(
                                                RichText::new(format!(
                                                    "{} {} {}",
                                                    e.icon, e.title, e.text
                                                ))
                                                .size(11.0)
                                                .color(theme.text),
                                            )
                                            .wrap(),
                                        );
                                        if !e.detail.is_empty() {
                                            resp.on_hover_text(&e.detail);
                                        }
                                        ui.label(
                                            RichText::new(fmt_age(e.age_ms))
                                                .size(9.5)
                                                .color(theme.text_dim),
                                        );
                                    });
                                });
                                ui.add_space(5.0);
                            }
                        });
                });
        },
    );
}

// ---------------------------------------------------------------------------
// 下部: 処理スループット
// ---------------------------------------------------------------------------

fn chart_ui(ui: &mut egui::Ui, theme: &Theme, st: &KanbanState) {
    let current = st.samples.last().map(|s| s.tally.work_sum()).unwrap_or(0);
    egui::Frame::none()
        .fill(theme.panel)
        .stroke(Stroke::new(1.0_f32, theme.border))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::same(9.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(tr("処理スループット"))
                        .size(11.5)
                        .strong()
                        .color(theme.text),
                );
                ui.label(
                    RichText::new(tr("作業中エージェント数の推移 (約8分)"))
                        .size(10.0)
                        .color(theme.text_dim),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(current.to_string())
                            .size(15.0)
                            .strong()
                            .color(theme.accent),
                    );
                    live_dot(ui, theme, st.samples.last().map(|s| s.at_ms).unwrap_or(0));
                });
            });
            ui.add_space(2.0);
            let (rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 44.0), egui::Sense::hover());
            let painter = ui.painter();
            let values: Vec<f32> = st
                .samples
                .iter()
                .map(|s| s.tally.work_sum() as f32)
                .collect();
            let max = values.iter().cloned().fold(2.0_f32, f32::max);
            // 薄い水平グリッド 3 本
            for i in 1..=3 {
                let y = rect.top() + rect.height() * i as f32 / 4.0;
                painter.line_segment(
                    [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                    Stroke::new(1.0_f32, theme.border.gamma_multiply(0.5)),
                );
            }
            if values.len() >= 2 {
                let pts: Vec<Pos2> = values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let x = rect.left() + rect.width() * i as f32 / (values.len() - 1) as f32;
                        let y = rect.bottom() - (rect.height() - 3.0) * (v / max);
                        egui::pos2(x, y)
                    })
                    .collect();
                let last = *pts.last().expect("len >= 2");
                painter.add(egui::Shape::line(pts, Stroke::new(1.8_f32, theme.accent)));
                painter.circle_filled(last, 2.5, theme.accent);
            }
            // 右端に最大値の目盛り
            painter.text(
                rect.right_top() + egui::vec2(-2.0, 0.0),
                egui::Align2::RIGHT_TOP,
                format!("{}", max as usize),
                egui::FontId::proportional(9.0),
                theme.text_dim,
            );
        });
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::SessionState as S;

    /// **アクティビティ → レーンの対応表**。ここが仕様書。
    ///
    /// 8 本あるので、細かい状態 (実装中 / 検証中 / 思考中 / 実行中) は
    /// **カードの位置そのもの**で読める — 一覧を別に作る必要が無い。
    #[test]
    fn アクティビティは八レーンへ対応する() {
        let table: &[(Activity, Column)] = &[
            // 待機 (指示を受けられる)
            (Activity::Starting, Column::Ready),
            (Activity::Idle, Column::Ready),
            // 動いている 4 本 — 1 対 1
            (Activity::Thinking, Column::Thinking),
            (Activity::Editing, Column::Editing),
            (Activity::Running, Column::Running),
            (Activity::Verifying, Column::Verifying),
            // 人の手が要る
            (Activity::Approval, Column::Approval),
            (Activity::RateLimited, Column::Trouble),
            (Activity::Stalled, Column::Trouble),
            // 完了
            (Activity::Exited, Column::Done),
        ];
        for (act, want) in table {
            assert_eq!(act.column(), *want, "{act:?} の行き先");
        }
        // 表が全アクティビティを覆っていること (増えたら落ちる)
        assert_eq!(table.len(), 10, "Activity を増やしたら表も足すこと");
        assert_eq!(COLUMNS.len(), 8);
        assert_eq!(LANES, 8);
        // 添字は重複しない (集計配列のキーになる)
        let mut seen = [false; LANES];
        for c in COLUMNS {
            assert!(!seen[c.index()], "{c:?} の添字が重複");
            seen[c.index()] = true;
        }
        // 「人の手が要る」= 声を大きくする 2 本だけ
        let loud: Vec<Column> = COLUMNS.into_iter().filter(|c| c.loud()).collect();
        assert_eq!(loud, vec![Column::Approval, Column::Trouble]);
        // 「動いている」= 作業系 4 本
        assert_eq!(COLUMNS.into_iter().filter(|c| c.is_work()).count(), 4);
    }

    #[test]
    fn exited_always_lands_in_done() {
        // 終了は他のどのフラグより強い (attention が残っていても Done)
        for sup in [None, Some(S::Working), Some(S::WaitingApproval)] {
            assert_eq!(column_for(false, true, true, sup), Column::Done);
        }
    }

    #[test]
    fn attention_beats_rate_limit_and_supervisor() {
        assert_eq!(
            column_for(true, true, true, Some(S::Working)),
            Column::Approval
        );
    }

    #[test]
    fn rate_limit_is_trouble() {
        assert_eq!(
            column_for(true, false, true, Some(S::Working)),
            Column::Trouble
        );
    }

    #[test]
    fn supervisor_states_map_to_columns() {
        // 画面テキストを渡さない版でも「動いている」は作業系レーン
        assert_eq!(
            column_for(true, false, false, Some(S::Working)),
            Column::Thinking
        );
        assert_eq!(column_for(true, false, false, Some(S::Idle)), Column::Ready);
        assert_eq!(
            column_for(true, false, false, Some(S::WaitingApproval)),
            Column::Approval
        );
        // 出力ストリームの観測が無い呼び出し (Flow::Unknown) では従来通り
        for s in [S::Stalled, S::Looping, S::Errored, S::Crashed] {
            assert_eq!(column_for(true, false, false, Some(s)), Column::Trouble);
        }
        assert_eq!(column_for(true, false, false, Some(S::Done)), Column::Done);
        // 起動直後 (未観測) は待機扱い
        assert_eq!(column_for(true, false, false, None), Column::Ready);
    }

    #[test]
    fn state_label_follows_same_priority() {
        // ラベル (= 細かいアクティビティ) はレーンを畳んでも粒度を保つ
        assert_eq!(state_label(false, false, false, Some(S::Working)), "終了");
        assert_eq!(state_label(true, true, false, None), "承認待ち");
        assert_eq!(state_label(true, false, true, None), "レート制限中");
        assert_eq!(state_label(true, false, false, Some(S::Working)), "思考中");
        assert_eq!(state_label(true, false, false, None), "起動中");
    }

    // ── 確信度の床 (信号の段位) ──

    /// **画面テキストだけの判定は「承認待ち」「停滞・異常」「完了」へ入れない。**
    /// 構造化信号 (生死 / プロンプト / 上限 / 見張り) なら入れる。
    #[test]
    fn 画面推定だけでは人を呼ぶレーンにも完了にも入れない() {
        // (アクティビティ, 出どころ, 期待レーン)
        let table: &[(Activity, Source, Column)] = &[
            // 画面推定で「承認待ち」「終了」を名乗っても作業系で頭打ち
            (Activity::Approval, Source::Screen, FLOOR_LANE),
            (Activity::Stalled, Source::Screen, FLOOR_LANE),
            (Activity::RateLimited, Source::Screen, FLOOR_LANE),
            (Activity::Exited, Source::Screen, FLOOR_LANE),
            // 同じアクティビティでも、上の段の裏付けがあれば入れる
            (Activity::Approval, Source::Prompt, Column::Approval),
            (Activity::Approval, Source::Supervisor, Column::Approval),
            (Activity::RateLimited, Source::RateLimit, Column::Trouble),
            (Activity::Stalled, Source::Supervisor, Column::Trouble),
            (Activity::Exited, Source::Process, Column::Done),
            (Activity::Exited, Source::Supervisor, Column::Done),
            // 作業系・待機は画面推定でも動かしてよい (誤っても人を呼ばない)
            (Activity::Editing, Source::Screen, Column::Editing),
            (Activity::Verifying, Source::Screen, Column::Verifying),
            (Activity::Running, Source::Screen, Column::Running),
            (Activity::Thinking, Source::Screen, Column::Thinking),
            (Activity::Idle, Source::Supervisor, Column::Ready),
        ];
        for (activity, source, want) in table {
            let r = Read::new(*activity, *source, String::new());
            assert_eq!(r.lane(), *want, "{activity:?} / {source:?}");
        }
        // 段位の順序そのもの (原則 #4: 構造化 > フック > 状態 > 画面)
        assert!(Source::Process.rung() < Source::Prompt.rung());
        assert!(Source::Prompt.rung() < Source::RateLimit.rung());
        assert!(Source::RateLimit.rung() < Source::Supervisor.rung());
        assert!(Source::Supervisor.rung() < Source::Screen.rung());
        assert_eq!(Source::Supervisor.rung(), STRONG_RUNG);
        // 床が要るのは「人を呼ぶ 2 本」と「完了」だけ
        for col in COLUMNS {
            assert_eq!(
                needs_strong_signal(col),
                col.loud() || col == Column::Done,
                "{col:?}"
            );
        }
        assert!(!needs_strong_signal(FLOOR_LANE), "床の行き先が床に弾かれる");
    }

    /// 人を呼ぶカードは「何がここへ入れたか」を必ず言える。
    #[test]
    fn 要対応の根拠は必ず読める() {
        let r = Read::new(
            Activity::Approval,
            Source::Prompt,
            "Do you want to proceed?".into(),
        );
        let why = r.reason();
        assert!(why.contains("承認待ち"), "{why}");
        assert!(why.contains("プロンプト検出"), "{why}");
        assert!(why.contains("Do you want to proceed?"), "{why}");
        // 補足が無くても出どころは必ず出る
        let r = Read::new(Activity::Stalled, Source::Supervisor, String::new());
        let why = r.reason();
        assert!(why.contains("停滞") && why.contains("見張り"), "{why}");
        // 採らなかった疑いも読める (握り潰さない)
        let mut r = Read::new(Activity::Verifying, Source::Screen, "cargo test".into());
        r.suspicion = Some(S::Errored.label());
        let why = r.reason();
        assert!(why.contains("エラー多発") && why.contains("疑い"), "{why}");
    }

    // ── アクティビティ分類 (表引き) ──

    #[test]
    fn contains_word_respects_ascii_boundaries() {
        assert!(contains_word("cargo test --lib", "test"));
        assert!(contains_word("bash(cargo test)", "test"));
        // "latest" の中の "test" には当たらない
        assert!(!contains_word("the latest build", "test"));
        assert!(!contains_word("tests/foo.rs", "test"));
        assert!(contains_word("tests/foo.rs", "tests"));
        // 日本語は前後が ASCII 英数字にならないのでそのまま通る
        assert!(contains_word("テストを実行中です", "テスト"));
        assert!(!contains_word("", "test"));
        assert!(!contains_word("test", ""));
    }

    #[test]
    fn looks_like_path_accepts_any_os_shape() {
        assert!(looks_like_path("src/kanban.rs"));
        assert!(looks_like_path("C:\\work\\a.rs"));
        assert!(looks_like_path("foo.rs"));
        assert!(looks_like_path("./a/b"));
        assert!(!looks_like_path("cargo"));
        assert!(!looks_like_path("a.b c"));
        assert!(!looks_like_path("--"));
        // 拡張子が長すぎる (文中の句点など) は拾わない
        assert!(!looks_like_path("foo.verylongextension"));
    }

    #[test]
    fn pick_command_prefers_parens_then_colon() {
        assert_eq!(pick_command("⏺ Bash(cargo test --lib)"), "cargo test --lib");
        assert_eq!(pick_command("Running: npm install"), "npm install");
        assert_eq!(pick_command("コマンド実行： ls -la"), "ls -la");
        // 括弧もコロンも無ければ装飾だけ落として行全体
        assert_eq!(pick_command("● Compiling foo"), "Compiling foo");
        // Windows パスのドライブコロンで切らない
        assert_eq!(
            pick_command("Writing C:\\work\\a.rs"),
            "Writing C:\\work\\a.rs"
        );
    }

    /// PTY 末尾テキスト → アクティビティ の表テスト。
    /// ベンダー CLI の表示が変わったら `SCREEN_RULES` だけ直せばよい。
    #[test]
    fn classify_screen_table() {
        let cases: &[(&str, Option<Activity>, &str)] = &[
            // 編集系
            (
                "⏺ Update(src/kanban.rs)",
                Some(Activity::Editing),
                "src/kanban.rs",
            ),
            ("Edit(src/app.rs)", Some(Activity::Editing), "src/app.rs"),
            (
                "● Writing tests/mod.rs",
                Some(Activity::Editing),
                "tests/mod.rs",
            ),
            (
                "apply_patch to lib/x.py",
                Some(Activity::Editing),
                "lib/x.py",
            ),
            (
                "ファイルを編集中: src/foo.rs",
                Some(Activity::Editing),
                "src/foo.rs",
            ),
            // パスに "tests" が入っていても「テストを回している」ではない
            (
                "⏺ Update(tests/mod.rs)",
                Some(Activity::Editing),
                "tests/mod.rs",
            ),
            // 検証系 (実行より優先 — 人が知りたいのは「検証中」)
            (
                "⏺ Bash(cargo test --lib)",
                Some(Activity::Verifying),
                "cargo test --lib",
            ),
            (
                "Compiling zaivern-code v0.4.15",
                Some(Activity::Verifying),
                "",
            ),
            ("テストを実行しています", Some(Activity::Verifying), ""),
            // 実行系
            ("⏺ Bash(git status)", Some(Activity::Running), "git status"),
            (
                "Running: npm install",
                Some(Activity::Running),
                "npm install",
            ),
            ("コマンド実行: ls -la", Some(Activity::Running), "ls -la"),
            // 思考系
            (
                "✻ Thinking… (esc to interrupt)",
                Some(Activity::Thinking),
                "",
            ),
            ("調査中です…", Some(Activity::Thinking), ""),
            ("Searching for foo", Some(Activity::Thinking), ""),
            // 当たらない / 空
            ("", None, ""),
            ("   ", None, ""),
            ("╭──────────────╮", None, ""),
            ("こんにちは", None, ""),
        ];
        for (line, want, want_detail) in cases {
            let tail = vec![line.to_string()];
            let got = classify_screen(&tail);
            match want {
                Some(a) => {
                    let (ga, gd) = got.unwrap_or_else(|| panic!("分類されない: {line:?}"));
                    assert_eq!(ga, *a, "行 {line:?}");
                    if !want_detail.is_empty() {
                        assert_eq!(gd, *want_detail, "行 {line:?} の詳細");
                    }
                }
                None => assert!(got.is_none(), "誤検知: {line:?} → {got:?}"),
            }
        }
    }

    #[test]
    fn classify_screen_strips_ansi_and_reads_newest_line() {
        // ANSI 装飾つき + 新しい行が勝つ
        let tail = vec![
            "\u{1b}[32m⏺ Bash(git status)\u{1b}[0m".to_string(),
            "\u{1b}[1m⏺ Update(src/theme.rs)\u{1b}[0m".to_string(),
        ];
        assert_eq!(
            classify_screen(&tail),
            Some((Activity::Editing, "src/theme.rs".to_string()))
        );
        // 空行は読み飛ばして、その手前の行で判定する
        let tail = vec!["⏺ Bash(cargo build)".to_string(), "".to_string()];
        assert_eq!(
            classify_screen(&tail).map(|(a, _)| a),
            Some(Activity::Verifying)
        );
    }

    #[test]
    fn classify_prefers_structured_signals_over_screen() {
        let editing = vec!["⏺ Update(src/a.rs)".to_string()];
        // 終了 > すべて
        let r = classify(false, true, true, Some(S::Working), &editing);
        assert_eq!((r.activity, r.source), (Activity::Exited, Source::Process));
        // 承認プロンプト検出 > レート制限 > 見張り > 画面
        let r = classify(true, true, true, Some(S::Working), &editing);
        assert_eq!((r.activity, r.source), (Activity::Approval, Source::Prompt));
        let r = classify(true, false, true, Some(S::Working), &editing);
        assert_eq!(
            (r.activity, r.source),
            (Activity::RateLimited, Source::RateLimit)
        );
        // 見張りが「動いていない」と言うなら、画面の残骸に釣られない
        let r = classify(true, false, false, Some(S::Idle), &editing);
        assert_eq!((r.activity, r.source), (Activity::Idle, Source::Supervisor));
        // 見張りが「動いている」と言うときだけ画面推定を採り、出どころを明示する
        let r = classify(true, false, false, Some(S::Working), &editing);
        assert_eq!((r.activity, r.source), (Activity::Editing, Source::Screen));
        assert!(r.source.is_guess());
        // 動いているが中身が読めない → 思考中 (見張り由来 = 推定ではない)
        let r = classify(true, false, false, Some(S::Working), &[]);
        assert_eq!(
            (r.activity, r.source),
            (Activity::Thinking, Source::Supervisor)
        );
        assert!(!r.source.is_guess());
        // 観測が無いとき (Flow::Unknown) は従来通り異常をそのまま採る
        for s in [S::Stalled, S::Looping, S::Errored, S::Crashed] {
            let r = classify(true, false, false, Some(s), &editing);
            assert_eq!(r.activity, Activity::Stalled);
            assert_eq!(r.lane(), Column::Trouble);
        }
    }

    /// 画面推定の判定は **SCREEN_RULES 側が何を返しても** 作業系で頭打ちになる。
    /// (規則表に「承認待ち」を足されても、勝手に人を呼ばない安全弁)
    #[test]
    fn 画面由来の分類は人を呼ぶレーンへ入れない() {
        for (a, _, _) in SCREEN_RULES.iter().map(|r| (r.activity, r.needles, r.pick)) {
            let r = Read::new(a, Source::Screen, String::new());
            assert!(
                !r.lane().loud() && r.lane() != Column::Done,
                "画面規則 {a:?} が {:?} へ入ろうとした",
                r.lane()
            );
        }
    }

    // ── #2 「作業中なのに停滞・異常」の根治 ──────────────────────────────

    /// **根本原因の証拠**: 見張りの「エラー多発」は、正常な作業の出力でも当たる。
    ///
    /// 上流 (`supervisor::is_error_line`) には語境界の照合とパストークンの除外を
    /// 入れたので、`src/error_handling.rs` のような**ファイル名はもう数えない**。
    /// それでもテストの `FAILED` 行と、エージェントが読んでいる診断の引用は残る
    /// — これらは「エラーという語が画面にある」だけで、異常の証拠ではない。
    /// → 画面テキストだけでは人を呼ぶ根拠として弱い。だから下の裏取りが要る。
    #[test]
    fn 見張りのエラー判定は正常な作業の出力でも当たる() {
        let cfg = crate::supervisor::SupervisorConfig::default();
        // どれも「エージェントが普通に仕事をしている」画面の行
        let work_lines = [
            "⏺ Read(src/error_handling.rs)",
            "⏺ Update(src/errors.rs)",
            "test kanban::tests::lane_moves ... FAILED",
            "  |     assert failed: expected error, got ok",
            "error[E0308]: mismatched types",
        ];
        let hits = work_lines
            .iter()
            .filter(|l| {
                crate::supervisor::is_error_line(&crate::supervisor::normalize_line(l, false), &cfg)
            })
            .count();
        // 先頭 2 行 (パス中の error) はもう数えない。残る 3 行は
        // FAILED × 2 と本物の error[E0308] — どれも「作業中の画面」に普通に出る。
        assert_eq!(
            hits, 3,
            "パス中の error 語は数えない。だが FAILED と引用は残る (だから裏取りが要る)"
        );
        // その裏取り: 出力が流れている間は人を呼ばない / 止まったら呼ぶ
        assert!(!trouble_confirmed(S::Errored, Flow::Live));
        assert!(trouble_confirmed(S::Errored, Flow::Silent));
        assert!(!trouble_confirmed(S::Stalled, Flow::Live));
        assert!(trouble_confirmed(S::Stalled, Flow::Silent));
        // 反復とプロセス死は出力の有無と無関係に成立する
        assert!(trouble_confirmed(S::Looping, Flow::Live));
        assert!(trouble_confirmed(S::Crashed, Flow::Live));
        // 観測が足りないときは従来通り (証拠が無いことを証拠にしない)
        for s in [S::Stalled, S::Errored, S::Looping, S::Crashed] {
            assert!(trouble_confirmed(s, Flow::Unknown), "{s:?}");
        }
    }

    /// **本体の回帰テスト**: 動いているエージェントは「停滞・異常」に入らない。
    #[test]
    fn 出力が続いている間はエラー多発でも停滞レーンへ入れない() {
        let tail = vec![
            "⏺ Bash(cargo test --lib)".to_string(),
            "error[E0308]: mismatched types".to_string(),
        ];
        // 出力が流れている = 仕事をしている → 検証中のまま、疑いだけ残す
        let r = classify_flow(true, false, false, Some(S::Errored), &tail, Flow::Live);
        assert_eq!(r.activity, Activity::Verifying, "{r:?}");
        assert_eq!(r.lane(), Column::Verifying);
        assert_eq!(r.suspicion, Some(S::Errored.label()), "疑いは捨てない");
        // 止まったなら人を呼ぶ (検出を殺したわけではない)
        let r = classify_flow(true, false, false, Some(S::Errored), &tail, Flow::Silent);
        assert_eq!(r.activity, Activity::Stalled);
        assert_eq!(r.lane(), Column::Trouble);
        assert_eq!(r.suspicion, None);
        // ループ・異常終了は出力が流れていても人を呼ぶ
        for s in [S::Looping, S::Crashed] {
            let r = classify_flow(true, false, false, Some(s), &tail, Flow::Live);
            assert_eq!(r.lane(), Column::Trouble, "{s:?}");
        }
        // 承認プロンプト検出は出力の有無に関係なく最優先 (段位 1)
        let r = classify_flow(true, true, false, Some(S::Errored), &tail, Flow::Live);
        assert_eq!(
            (r.activity, r.lane()),
            (Activity::Approval, Column::Approval)
        );
    }

    /// スピナー / 経過秒だけが動いている画面を「進捗」と数えない。
    #[test]
    fn 進捗の判定はスピナーと経過秒を潰す() {
        let a = norm_tail(&["⠋ Thinking… (12s · 340 tokens)".to_string()]);
        let b = norm_tail(&["⠙ Thinking… (18s · 520 tokens)".to_string()]);
        assert_eq!(a, b, "スピナーと数字だけの差は進捗ではない");
        assert!(!has_new_content(&a, &b));
        // 本当に新しい行が出たら進捗
        let c = norm_tail(&[
            "⠹ Thinking… (19s · 600 tokens)".to_string(),
            "⏺ Update(src/kanban.rs)".to_string(),
        ]);
        assert!(has_new_content(&a, &c));
    }

    // ── レーン移動のデバウンス ──

    /// **8 レーンでいちばん暴れる系列 (思考↔編集↔実行↔検証)** を、
    /// 画面サンプリングと同じ 150ms 刻みで流してもカードが飛ばないこと。
    ///
    /// ツール呼び出しは 1 秒の中で何度も表示が変わる。[`WORK_HOLD_MS`] 未満の
    /// 往復では 1 度も動かず、同じ判定が続いたときだけ動く — これが
    /// 「8 本に戻してもちらつかない」の担保。
    #[test]
    fn 作業系の往復はホールド時間未満では動かない() {
        let acts = [
            Activity::Thinking,
            Activity::Editing,
            Activity::Running,
            Activity::Verifying,
        ];
        let mut lt = LaneTracker::new(Column::Thinking, 0);
        // 150ms ごとに 4 種類を巡回 = 同じ判定は最大 150ms しか続かない
        for (i, a) in acts.iter().cycle().take(40).enumerate() {
            let want = Read::new(*a, Source::Screen, String::new()).lane();
            let t = i as u64 * FAST_SAMPLE_MS;
            assert!(!lt.step(want, t), "t={t} ({a:?}) で動いてはいけない");
            assert_eq!(lt.lane(), Column::Thinking);
        }
        // 同じ判定が続けば (8 サンプル = WORK_HOLD_MS) ちゃんと動く
        let mut t = 10_000;
        assert!(!lt.step(Column::Editing, t));
        while t < 10_000 + WORK_HOLD_MS - FAST_SAMPLE_MS {
            t += FAST_SAMPLE_MS;
            assert!(!lt.step(Column::Editing, t), "t={t} はまだ早い");
        }
        assert!(lt.step(Column::Editing, 10_000 + WORK_HOLD_MS));
        assert_eq!(lt.lane(), Column::Editing);
    }

    #[test]
    fn lane_does_not_flicker_below_hold_threshold() {
        let mut lt = LaneTracker::new(Column::Running, 0);
        // 100ms ごとに 待機↔実行中 が往復しても、しきい値未満なので動かない
        let seq: [(u64, Column); 6] = [
            (100, Column::Ready),
            (200, Column::Running),
            (300, Column::Ready),
            (400, Column::Running),
            (500, Column::Ready),
            (600, Column::Running),
        ];
        for (t, want) in seq {
            let moved = lt.step(want, t);
            assert!(!moved, "t={t} で動いてはいけない");
            assert_eq!(lt.lane(), Column::Running);
        }
    }

    #[test]
    fn lane_moves_promptly_once_state_holds() {
        let mut lt = LaneTracker::new(Column::Running, 0);
        assert!(!lt.step(Column::Ready, 1_000)); // 候補として登録
        assert!(!lt.step(Column::Ready, 1_300)); // まだ 300ms
        assert!(lt.step(Column::Ready, 1_400)); // 400ms 到達 → 移動
        assert_eq!(lt.lane(), Column::Ready);
        assert_eq!(lt.landed_ms(), 1_400);
        // 着地ハイライトは時間で減衰し、やがて消える
        assert!(lt.land_glow(1_400) > 0.9);
        assert!(lt.land_glow(1_850) > 0.0);
        assert_eq!(lt.land_glow(2_400), 0.0);
    }

    #[test]
    fn strong_signals_move_without_waiting() {
        // 承認待ち・完了は hold_ms == 0 なので即座に動く (人を待たせない)
        for col in [Column::Approval, Column::Done] {
            for from in [Column::Ready, Column::Editing, Column::Verifying] {
                let mut lt = LaneTracker::new(from, 0);
                assert!(lt.step(col, 10), "{from:?} → {col:?} は即移動のはず");
                assert_eq!(lt.lane(), col);
            }
        }
        for col in COLUMNS {
            let want = match col {
                Column::Approval | Column::Done => 0,
                Column::Trouble => TROUBLE_HOLD_MS,
                Column::Ready => LANE_HOLD_MS,
                _ => WORK_HOLD_MS,
            };
            assert_eq!(col.hold_ms(), want, "{col:?}");
        }
        // 作業系のホールドは「1 サンプルで動かない」ために十分な長さがある
        assert!(WORK_HOLD_MS >= FAST_SAMPLE_MS * 4, "ホールドが短すぎる");
        // 「停滞・異常」は作業系より長く粘る — 誤報のコストがいちばん高いレーン
        assert!(
            TROUBLE_HOLD_MS > WORK_HOLD_MS,
            "異常レーンのホールドが作業系より短い"
        );
    }

    /// **本題の回帰テスト**: 一瞬だけ立った異常判定では「停滞・異常」へ落ちない。
    ///
    /// 見張りの判定はハッシュ列から出るので 1〜2 サンプルだけ立つことがある。
    /// それで人を呼んでいたため、動いているエージェントが異常レーンに並んだ。
    #[test]
    fn 一瞬の異常判定では異常レーンへ落ちない() {
        let mut lt = LaneTracker::new(Column::Running, 0);
        // 数サンプル異常が立つ (TROUBLE_HOLD_MS 未満)
        let mut t = 0;
        while t < TROUBLE_HOLD_MS {
            assert!(!lt.step(Column::Trouble, t), "{t}ms で異常レーンへ飛んだ");
            t += FAST_SAMPLE_MS;
        }
        // 収まれば候補は取り下げ — 作業中のまま
        assert!(!lt.step(Column::Running, t));
        assert_eq!(lt.lane(), Column::Running);
    }

    /// 逆に**続く異常はちゃんと落ちる** (人を呼び損ねない)。
    #[test]
    fn 続く異常判定は異常レーンへ落ちる() {
        let mut lt = LaneTracker::new(Column::Running, 0);
        let mut t = 0;
        let mut moved = false;
        while t <= TROUBLE_HOLD_MS + FAST_SAMPLE_MS {
            moved |= lt.step(Column::Trouble, t);
            t += FAST_SAMPLE_MS;
        }
        assert!(moved, "続いている異常が異常レーンへ落ちない");
        assert_eq!(lt.lane(), Column::Trouble);
    }

    #[test]
    fn lane_cancels_pending_when_state_returns() {
        let mut lt = LaneTracker::new(Column::Running, 0);
        assert!(!lt.step(Column::Ready, 100));
        // 元に戻ったら候補は取り下げ → その後 Ready が来ても 0 から数え直す
        assert!(!lt.step(Column::Running, 200));
        assert!(!lt.step(Column::Ready, 300));
        assert!(!lt.step(Column::Ready, 600)); // 300ms しか経っていない
        assert!(lt.step(Column::Ready, 700));
        assert_eq!(lt.lane(), Column::Ready);
    }

    /// **どんな順序で判定が来てもちらつかない** — 移動には必ず
    /// 「同じ判定が hold_ms 続く」か「hold_ms == 0 の強い信号」が要る。
    #[test]
    fn どの系列でも一サンプルではレーンが飛ばない() {
        for start in COLUMNS {
            for want in COLUMNS {
                let mut lt = LaneTracker::new(start, 0);
                // 1 サンプルだけ違う判定が来ても、弱いレーンへは動かない
                let moved = lt.step(want, 10);
                if want == start {
                    assert!(!moved);
                } else if want.hold_ms() == 0 {
                    assert!(moved, "{start:?} → {want:?} は即時のはず");
                } else {
                    assert!(!moved, "{start:?} → {want:?} が 1 サンプルで飛んだ");
                    // 直後に元へ戻れば候補は取り下げ = 何も起きない
                    assert!(!lt.step(start, 20));
                    assert_eq!(lt.lane(), start);
                }
            }
        }
    }

    // ── 選択の安定性 ──

    #[test]
    fn selection_survives_reorder_and_insert() {
        let cards = vec![
            card_id(10, Column::Ready),
            card_id(20, Column::Running),
            card_id(30, Column::Done),
        ];
        assert_eq!(resolve_selection(Some(20), 1, &cards), Some((20, 1)));
        // 並べ替え: 位置は変わっても同じ ID が選ばれたまま
        let reordered = vec![
            card_id(30, Column::Done),
            card_id(20, Column::Running),
            card_id(10, Column::Ready),
        ];
        assert_eq!(resolve_selection(Some(20), 1, &reordered), Some((20, 1)));
        // 挿入で後ろへずれても追随する
        let inserted = vec![
            card_id(99, Column::Ready),
            card_id(10, Column::Ready),
            card_id(20, Column::Running),
        ];
        assert_eq!(resolve_selection(Some(20), 1, &inserted), Some((20, 2)));
    }

    #[test]
    fn selection_falls_back_when_card_removed() {
        let cards = vec![card_id(10, Column::Ready), card_id(30, Column::Done)];
        // 20 が消えた → 直前に居た位置 (1) のカードへ寄せる
        assert_eq!(resolve_selection(Some(20), 1, &cards), Some((30, 1)));
        // 位置が範囲外なら末尾へ丸める
        assert_eq!(resolve_selection(Some(20), 9, &cards), Some((30, 1)));
        // 1 枚も無ければ選択なし
        assert_eq!(resolve_selection(Some(20), 0, &[]), None);
    }

    #[test]
    fn move_selection_wraps_and_handles_empty() {
        let order = [10_u64, 20, 30];
        assert_eq!(move_selection(&order, Some(10), 1), Some(20));
        assert_eq!(move_selection(&order, Some(30), 1), Some(10)); // 折り返し
        assert_eq!(move_selection(&order, Some(10), -1), Some(30));
        // 未選択なら端から
        assert_eq!(move_selection(&order, None, 1), Some(10));
        assert_eq!(move_selection(&order, None, -1), Some(30));
        // 消えた ID を持っていても壊れない
        assert_eq!(move_selection(&order, Some(99), 1), Some(10));
        assert_eq!(move_selection(&[], Some(10), 1), None);
    }

    // ── レイアウト判定 ──

    #[test]
    fn layout_auto_picks_vertical_for_tall_or_narrow() {
        let s = 0.38_f32;
        // 広い横長 → 横
        assert!(!use_vertical(LayoutMode::Auto, 1600.0, 700.0, false, s));
        // 8 本を読める幅に届かなければ縦へ (敷居は本数から導く)
        assert!(use_vertical(LayoutMode::Auto, 700.0, 500.0, false, s));
        assert!(use_vertical(LayoutMode::Auto, 500.0, 400.0, false, s));
        // 縦長 (サブディスプレイ縦置き) → 縦
        assert!(use_vertical(LayoutMode::Auto, 1000.0, 1400.0, false, s));
        // 手動指定は窓の形に関係なく従う
        assert!(!use_vertical(
            LayoutMode::Horizontal,
            400.0,
            1400.0,
            false,
            s
        ));
        assert!(use_vertical(LayoutMode::Vertical, 2000.0, 300.0, false, s));
    }

    /// **ライブペインを開いたら、看板の取り分で縦横を選び直す。**
    ///
    /// 起きていた不具合: 端末を出すと看板が半分の幅になり、レーンのうち
    /// 「承認」が半分隠れて「完了」が画面外へ落ちた。看板に残る幅で
    /// 判定すれば、同じ窓でも縦モードへ切り替わって全レーンが読める。
    #[test]
    fn ライブペインを開くと縦モードへ落ちる() {
        let s = 0.38_f32;
        // 1600x700: 閉じていれば横。開けば看板は ~970 (< BOARD_MIN_W) → 縦へ
        assert!(!use_vertical(LayoutMode::Auto, 1600.0, 700.0, false, s));
        assert!(use_vertical(LayoutMode::Auto, 1600.0, 700.0, true, s));
        // 十分広ければ開いても横のまま (看板の取り分が敷居を超える)
        assert!(!use_vertical(LayoutMode::Auto, 2400.0, 1000.0, true, s));
        // 取り分を広げるほど早く縦へ落ちる
        assert!(use_vertical(LayoutMode::Auto, 2400.0, 1000.0, true, 0.7));
    }

    #[test]
    fn layout_mode_round_trips_through_persisted_u8() {
        for m in [
            LayoutMode::Auto,
            LayoutMode::Horizontal,
            LayoutMode::Vertical,
        ] {
            assert_eq!(LayoutMode::from_u8(m.to_u8()), m);
        }
        // 未知の値は既定 (自動) へ倒す
        assert_eq!(LayoutMode::from_u8(200), LayoutMode::Auto);
    }

    // ── 経過時間 / 出力の勢い ──

    #[test]
    fn fmt_elapsed_formats_minutes_and_hours() {
        assert_eq!(fmt_elapsed(0), "0:00");
        assert_eq!(fmt_elapsed(7_400), "0:07");
        assert_eq!(fmt_elapsed(151_000), "2:31");
        assert_eq!(fmt_elapsed(3_840_000), "1:04:00");
    }

    #[test]
    fn tail_delta_counts_only_new_lines() {
        let prev = vec!["abc".to_string(), "de".to_string()];
        // 同じ行は数えない (スクロールしただけ)
        assert_eq!(tail_delta(&prev, &prev), 0);
        // 新しい行の文字数だけ
        let cur = vec!["de".to_string(), "hello".to_string()];
        assert_eq!(tail_delta(&prev, &cur), 5);
        // 全部新しい
        assert_eq!(tail_delta(&[], &cur), 7);
        // マルチバイトは文字数で数える
        assert_eq!(tail_delta(&[], &["あいう".to_string()]), 3);
        assert_eq!(tail_delta(&prev, &[]), 0);
    }

    #[test]
    fn bucket_series_splits_window_into_buckets() {
        // 窓 1000ms / 4 バケツ = 250ms 刻み。now=1000 なら窓は [0, 1000]
        let samples = [(0_u64, 1_u64), (300, 2), (600, 4), (900, 8)];
        let got = bucket_series(&samples, 1_000, 1_000, 4);
        assert_eq!(got, vec![1.0, 2.0, 4.0, 8.0]);
        // 窓の外 (古すぎる / 未来) は無視
        let samples = [(0_u64, 5_u64), (2_000, 7)];
        assert_eq!(bucket_series(&samples, 1_500, 1_000, 2), vec![0.0, 0.0]);
        // 同じバケツに落ちる点は足し合わせる
        let samples = [(510_u64, 1_u64), (590, 2)];
        assert_eq!(bucket_series(&samples, 1_000, 1_000, 2), vec![0.0, 3.0]);
        // 端の点 (now ちょうど) は最後のバケツへ丸める
        assert_eq!(bucket_series(&[(1_000, 9)], 1_000, 1_000, 4)[3], 9.0);
        assert!(bucket_series(&[(0, 1)], 1_000, 1_000, 0).is_empty());
    }

    // ── 追跡状態 (サンプリング周期・レーン集計) ──

    #[test]
    fn sample_due_is_slow_while_idle_and_fast_while_busy() {
        let mut st = KanbanState::default();
        // 初回は必ずサンプルする
        assert!(st.sample_due(0));
        // 静かなら 1 秒に 1 回
        assert!(!st.sample_due(500));
        assert!(st.sample_due(1_000));
        // 動き出したら ~6.7Hz
        let mut c = card_id(1, Column::Running);
        c.sup = Some(S::Working);
        c.tail_lines = vec!["⏺ Bash(cargo build)".to_string()];
        st.update_tracks(&[c], 1_000, true);
        assert!(!st.sample_due(1_100));
        assert!(st.sample_due(1_200));
        assert_eq!(st.next_repaint_ms(), 33, "着地アニメ中は高頻度");
    }

    #[test]
    fn idle_board_sleeps_and_drops_dead_tracks() {
        let mut st = KanbanState::default();
        let mut a = card_id(1, Column::Done);
        a.running = false;
        let lanes = st.update_tracks(&[a], 0, true);
        assert_eq!(lanes, vec![Column::Done]);
        // 誰も動いていない → 2 秒に 1 回でよい
        assert!(!st.busy);
        assert!(!st.any_running);
        // 着地アニメが切れたら寝る
        let mut a = card_id(1, Column::Done);
        a.running = false;
        st.update_tracks(&[a], 5_000, true);
        assert_eq!(st.next_repaint_ms(), ASLEEP_REPAINT_MS);
        // カードが消えたら追跡も捨てる
        st.update_tracks(&[], 6_000, true);
        assert!(st.track(1).is_none());
    }

    /// **看板を開いただけで「一番最後のエージェント」が選ばれない。**
    ///
    /// 追跡が空の初回フレームは全カードが「初めて見るカード」になる。
    /// ここで 1 枚ずつ選択を書き換えると、ループの最後 = 起動順で最後の
    /// エージェントが必ず選ばれ、ユーザーの選択も毎回そこへ吸われる。
    #[test]
    fn 初回の取り込みは選択を最後のカードへ奪わない() {
        // 同名のエージェントを複製起動した並び (id だけが違う)
        let deal = |ids: &[u64]| -> Vec<Card> {
            ids.iter().map(|id| card_id(*id, Column::Running)).collect()
        };
        let mut st = KanbanState::default();

        st.update_tracks(&deal(&[1, 2, 3, 4]), 0, true);
        assert_eq!(
            st.selected(),
            None,
            "看板を開いた初回に、最後のカードが勝手に選ばれている"
        );

        // ユーザーが真ん中を選ぶ → 描き直しても動かない
        st.selected = Some(2);
        for frame in 0..3 {
            st.update_tracks(&deal(&[1, 2, 3, 4]), 100 * frame + 100, true);
            assert_eq!(st.selected(), Some(2), "{frame}: 選択が最後へ移った");
        }

        // 本当に「いま起動した」1 体だけは、これまで通り選択とスクロールで示す
        st.update_tracks(&deal(&[1, 2, 3, 4, 9]), 1_000, true);
        assert_eq!(
            st.selected(),
            Some(9),
            "新しく起動した 1 体を示さなくなった"
        );

        // 一度に複数増えたとき (ワークスペース復元) はどれも奪わない
        st.update_tracks(&deal(&[1, 2, 3, 4, 9, 10, 11]), 2_000, true);
        assert_eq!(st.selected(), Some(9), "まとめて増えたときに最後が選ばれた");
    }

    #[test]
    fn tracks_debounce_lane_and_keep_last_file_and_command() {
        let mut st = KanbanState::default();
        let mk = |tail: &str| {
            let mut c = card_id(7, Column::Ready);
            c.sup = Some(S::Working);
            c.tail_lines = vec![tail.to_string()];
            c
        };
        let idle = || {
            let mut c = card_id(7, Column::Ready);
            c.sup = Some(S::Idle);
            c
        };
        // 生まれた瞬間はその場に置く (ヒステリシスは「移動」に掛かる)
        let lanes = st.update_tracks(&[idle()], 0, true);
        assert_eq!(lanes, vec![Column::Ready]);
        // 編集が WORK_HOLD_MS 続いて初めて「編集中」レーンへ動く
        st.update_tracks(&[mk("⏺ Update(src/a.rs)")], 100, true);
        let lanes = st.update_tracks(&[mk("⏺ Update(src/a.rs)")], 100 + WORK_HOLD_MS - 1, true);
        assert_eq!(lanes, vec![Column::Ready], "ホールド未満では動かない");
        st.update_tracks(&[mk("⏺ Update(src/a.rs)")], 100 + WORK_HOLD_MS, true);
        assert_eq!(st.track(7).unwrap().lane.lane(), Column::Editing);
        assert_eq!(st.track(7).unwrap().last_file, "src/a.rs");
        // **編集 → 実行 → 検証 と細かい中身が短時間で変わってもレーンは動かない。**
        // (400ms のころはここでカードが 3 回飛んでいた = 視覚的な雑音)
        let base = 100 + WORK_HOLD_MS;
        for (dt, tail) in [
            (100_u64, "⏺ Bash(git status)"),
            (400, "⏺ Bash(cargo test)"),
            (900, "⏺ Update(src/b.rs)"),
        ] {
            let lanes = st.update_tracks(&[mk(tail)], base + dt, true);
            assert_eq!(lanes, vec![Column::Editing], "t=+{dt} でレーンが動いた");
        }
        let t = st.track(7).unwrap();
        // 細かい中身は消えず、カードの 1 行 (状態文) として読める
        assert_eq!(t.activity, Activity::Editing);
        assert_eq!(t.last_cmd, "cargo test");
        assert_eq!(t.last_file, "src/b.rs");
        assert_eq!(status_line(t), "編集中: src/b.rs");
        // 経過タイマーは「アクティビティが変わった時点」から数える
        assert_eq!(t.elapsed_ms(base + 1_100), 200);
        // 同じ判定が続けばちゃんと動く (実行中を保持し続ける)
        for i in 1..=12 {
            st.update_tracks(
                &[mk("⏺ Bash(git status)")],
                base + 1_000 + i * FAST_SAMPLE_MS,
                true,
            );
        }
        assert_eq!(st.track(7).unwrap().lane.lane(), Column::Running);
        // 見張りが「動いていない」と言い続ければ、LANE_HOLD_MS 後に待機へ落ちる
        let start = base + 3_000;
        let lanes = st.update_tracks(&[idle()], start, true);
        assert_eq!(lanes, vec![Column::Running], "1 サンプルでは落ちない");
        let lanes = st.update_tracks(&[idle()], start + LANE_HOLD_MS, true);
        assert_eq!(lanes, vec![Column::Ready]);
    }

    /// **#2 の回帰テスト (実際の分類器を通す)。**
    ///
    /// `cargo test` を回して落ちたエージェントの軌跡。見張りは「エラー多発」と
    /// 言い続けるが、画面には新しい行が出続けている = 仕事は進んでいる。
    /// 以前はこれで「停滞・異常」レーンへ飛んでいた (オーナー報告のバグ)。
    #[test]
    fn 動いているエージェントは停滞レーンへ入らない() {
        let mut st = KanbanState::default();
        // 見張りがエラー多発と判定した状態で、出力は毎サンプル増えていく
        let mk = |n: usize, lines: &[&str]| {
            let mut c = card_id(1, Column::Ready);
            c.sup = Some(S::Errored);
            c.tail_lines = lines.iter().map(|l| l.to_string()).collect();
            let _ = n;
            c
        };
        let trace = [
            "⏺ Bash(cargo test --lib)",
            "error[E0308]: mismatched types",
            "  --> src/kanban.rs:12:5",
            "error[E0599]: no method named `lane`",
            "test kanban::tests::lane ... FAILED",
            "⏺ Update(src/kanban.rs)",
        ];
        let mut lanes = Vec::new();
        for (i, _) in trace.iter().enumerate() {
            // 毎サンプル 1 行ずつ新しい内容が増える = 進捗がある
            let card = mk(i, &trace[..=i]);
            lanes = st.update_tracks(&[card], (i as u64 + 1) * 3_000, true);
        }
        assert_ne!(lanes[0], Column::Trouble, "作業中なのに停滞レーンへ入った");
        assert!(
            !lanes[0].loud(),
            "人を呼ぶレーンへ入れてはいけない: {:?}",
            lanes[0]
        );
        // 疑いは握り潰さず、カードの ⚠ として残っている
        assert_eq!(st.track(1).unwrap().suspicion, Some(S::Errored.label()));
        assert!(st.track(1).unwrap().reason().contains("エラー多発"));

        // **止まったら人を呼ぶ** — 同じ画面のまま時間だけ進める
        let frozen = mk(0, &trace);
        let mut lanes = Vec::new();
        for i in 1..=8u64 {
            let mut c = card_id(1, Column::Ready);
            c.sup = frozen.sup;
            c.tail_lines = frozen.tail_lines.clone();
            lanes = st.update_tracks(&[c], 20_000 + i * FLOW_WINDOW_MS / 2, true);
        }
        assert_eq!(lanes[0], Column::Trouble, "止まったのに人を呼ばない");
    }

    /// スピナーだけが回っている画面は「進捗」ではない (経過秒に騙されない)。
    #[test]
    fn スピナーだけの画面は進捗と数えない() {
        let mut st = KanbanState::default();
        for i in 0..12u64 {
            let mut c = card_id(2, Column::Ready);
            c.sup = Some(S::Stalled);
            c.tail_lines = vec![format!("⠋ Thinking… ({i}s · {} tokens)", i * 40)];
            st.update_tracks(&[c], i * 3_000, true);
        }
        // 表示は毎フレーム変わっているが、中身は 1 行も増えていない → 停滞は本物
        let t = st.track(2).expect("追跡あり");
        assert_eq!(t.flow(33_000), Flow::Silent);
        assert_eq!(t.lane.lane(), Column::Trouble);
    }

    #[test]
    fn stale_frames_reuse_last_sampled_screen() {
        let mut st = KanbanState::default();
        let mut c = card_id(3, Column::Ready);
        c.sup = Some(S::Working);
        c.tail_lines = vec!["⏺ Bash(cargo test)".to_string()];
        st.update_tracks(&[c], 0, true);
        assert_eq!(st.track(3).unwrap().activity, Activity::Verifying);
        // 画面を渡さないフレーム (fresh=false) でも前回ぶんで同じ判定になる
        let mut c = card_id(3, Column::Ready);
        c.sup = Some(S::Working);
        st.update_tracks(&[c], 100, false);
        assert_eq!(st.track(3).unwrap().activity, Activity::Verifying);
        assert_eq!(st.track(3).unwrap().elapsed_ms(100), 100, "状態は継続");
    }

    #[test]
    fn tally_lanes_uses_debounced_lanes() {
        let cards = vec![
            card_id(1, Column::Ready),
            card_id(2, Column::Ready),
            card_id(3, Column::Ready),
        ];
        let lanes = [Column::Running, Column::Verifying, Column::Trouble];
        let t = tally_lanes(&cards, &lanes);
        assert_eq!(t.total, 3);
        // 素の列 (Ready) ではなく**デバウンス後のレーン**で数える
        assert_eq!(t.lane_count(Column::Running), 1);
        assert_eq!(t.lane_count(Column::Verifying), 1);
        assert_eq!(t.lane_count(Column::Trouble), 1);
        assert_eq!(t.lane_count(Column::Ready), 0);
        // まとめの軸 (レーンではない) — タイルには出さない
        assert_eq!(t.work_sum(), 2, "実行中 + 検証中");
        assert_eq!(t.attention_sum(), 1);
        assert_eq!(t.lane_sum(), t.total, "レーンの合計 = 総数");
    }

    /// **サマリー (KPI) タイルの合計 = 総数。二重計上しない。**
    /// 以前は「稼働中」タイルが他と重なり、タイルを足すと総数を超えていた。
    #[test]
    fn サマリータイルの合計は総数に一致する() {
        let combos: &[&[Column]] = &[
            &[],
            &[Column::Ready],
            &[Column::Running, Column::Editing, Column::Verifying],
            &[Column::Trouble, Column::Done],
            // 8 本すべてに 1 枚ずつ + 重複
            &[
                Column::Ready,
                Column::Thinking,
                Column::Editing,
                Column::Running,
                Column::Verifying,
                Column::Approval,
                Column::Trouble,
                Column::Done,
                Column::Editing,
                Column::Trouble,
            ],
        ];
        for lanes in combos {
            let cards: Vec<Card> = lanes
                .iter()
                .enumerate()
                .map(|(i, col)| card_id(i as u64 + 1, *col))
                .collect();
            let t = tally_lanes(&cards, lanes);
            assert_eq!(t.total, lanes.len());
            assert_eq!(t.lane_sum(), t.total, "{lanes:?}: タイルの合計が総数と違う");
            // タイルは LANES 枚 = レーンそのもの (稼働中は別軸なので混ぜない)
            assert_eq!(t.lanes().len(), LANES);
            for (col, n) in t.lanes() {
                assert_eq!(
                    n,
                    lanes.iter().filter(|c| **c == col).count(),
                    "{col:?} の枚数"
                );
            }
            // まとめの軸はタイルと重ならない (足し込むと二重計上になる値)
            assert!(t.work_sum() <= t.total && t.attention_sum() <= t.total);
            assert_eq!(
                t.work_sum(),
                lanes.iter().filter(|c| c.is_work()).count(),
                "{lanes:?}: 作業中のまとめ"
            );
        }
    }

    /// カードの 1 行 (detail line) は**細かいアクティビティを保つ**。
    /// レーンの外にも「いま何をしているか」を必ず 1 行で出す。
    #[test]
    fn カードの一行は細かい作業内容を保つ() {
        let cases: &[(Activity, &str, &str)] = &[
            (Activity::Editing, "src/foo.rs", "編集中: src/foo.rs"),
            (Activity::Running, "git status", "実行中: git status"),
            (Activity::Verifying, "cargo test", "検証中: cargo test"),
            (Activity::Thinking, "", "思考中"),
            (Activity::Approval, "続けますか?", "承認待ち: 続けますか?"),
            (Activity::Stalled, "停滞", "停滞・異常: 停滞"),
            (Activity::RateLimited, "", "レート制限中"),
            (Activity::Idle, "", "待機"),
            (Activity::Starting, "", "起動中"),
            (Activity::Exited, "", "終了"),
        ];
        for (activity, detail, want) in cases {
            let read = Read::new(*activity, Source::Supervisor, (*detail).to_string());
            let mut track = Track::new(&read, 0);
            track.detail = read.detail.clone();
            assert_eq!(status_line(&track), *want, "{activity:?}");
            // レーンは 8 本のどれか (ラベルと詳細はレーンとは別に必ず出る)
            assert!(COLUMNS.contains(&read.lane()));
        }
    }

    // ── ダッシュボード用の純関数 ──

    fn card(column: Column, running: bool) -> Card {
        Card {
            idx: 0,
            id: 1,
            icon: "👾".into(),
            title: "t".into(),
            active: false,
            column,
            state_label: String::new(),
            uptime: String::new(),
            unread: false,
            rate_limited: None,
            attention: false,
            running,
            sup: None,
            permission_badge: "",
            can_cycle: false,
            tail_lines: Vec::new(),
            task: None,
        }
    }

    /// id を指定したカード (選択安定性のテスト用)。
    fn card_id(id: u64, column: Column) -> Card {
        Card {
            id,
            ..card(column, true)
        }
    }

    #[test]
    fn tally_counts_columns_and_running() {
        let cards = vec![
            card(Column::Running, true),
            card(Column::Running, true),
            card(Column::Trouble, true),
            card(Column::Done, false),
        ];
        let t = tally(&cards);
        assert_eq!(t.total, 4);
        // 稼働中はレーンではなくプロセスの生死 (合計には入れない別軸)
        assert_eq!(t.running, 3);
        assert_eq!(t.lane_count(Column::Running), 2);
        assert_eq!(t.lane_count(Column::Trouble), 1);
        assert_eq!(t.lane_count(Column::Done), 1);
        assert_eq!(t.lane_count(Column::Ready), 0);
        assert_eq!(t.lane_sum(), t.total);
    }

    #[test]
    fn now_line_picks_last_meaningful_line() {
        let tail = vec![
            "compiling foo".to_string(),
            "  tests passed".to_string(),
            "   ".to_string(),
            String::new(),
        ];
        assert_eq!(now_line(&tail), Some("tests passed"));
        assert_eq!(now_line(&[]), None);
        assert_eq!(now_line(&["  ".to_string()]), None);
    }

    #[test]
    fn record_sample_throttles_and_caps() {
        let mut st = KanbanState::default();
        let lanes1 = [Column::Running];
        let lanes2 = [Column::Running, Column::Editing];
        let cards1 = vec![card_id(1, Column::Running)];
        let cards2 = vec![card_id(1, Column::Running), card_id(2, Column::Editing)];
        let t1 = tally_lanes(&cards1, &lanes1);
        let t2 = tally_lanes(&cards2, &lanes2);
        st.record_sample(0, t1);
        // 2 秒未満は最新点の上書き (点は増えない)
        st.record_sample(500, t2);
        assert_eq!(st.samples.len(), 1);
        assert_eq!(st.samples[0].tally.work_sum(), 2);
        // 2 秒経てば新しい点
        st.record_sample(2_500, t1);
        assert_eq!(st.samples.len(), 2);
        // 上限を超えたら古い点から捨てる
        for i in 0..400u64 {
            st.record_sample(10_000 + i * 3_000, t1);
        }
        assert!(st.samples.len() <= MAX_SAMPLES);
    }

    #[test]
    fn fmt_uptime_formats_days_and_clock() {
        assert_eq!(fmt_uptime(0), "00:00:00");
        assert_eq!(fmt_uptime(41 * 60_000 + 9_000), "00:41:09");
        // 1日 + 01:01:01
        assert_eq!(fmt_uptime((86_400 + 3_661) * 1_000), "1日 01:01:01");
    }

    #[test]
    fn fmt_age_buckets() {
        assert_eq!(fmt_age(3_000), "たった今");
        assert_eq!(fmt_age(30_000), "30秒前");
        assert_eq!(fmt_age(5 * 60_000), "5分前");
        assert_eq!(fmt_age(2 * 3_600_000), "2時間前");
    }

    // ── パネルリサイズ (ドラッグバー) ──

    /// app.rs の「zv-terminal」パネル + 看板タブをヘッドレスで 1 フレーム描く。
    /// 返り値はそのフレームでパネル中身に渡された高さ。
    struct PanelHarness {
        ctx: eframe::egui::Context,
        st: KanbanState,
        theme: crate::theme::Theme,
        t: f64,
        now_ms: u64,
        /// 直近フレームのパネル上端 y (フレーム余白込み)
        last_top: f32,
    }

    impl PanelHarness {
        fn new() -> Self {
            Self {
                ctx: eframe::egui::Context::default(),
                st: KanbanState::default(),
                theme: crate::theme::all().remove(0),
                t: 0.0,
                now_ms: 0,
                last_top: 0.0,
            }
        }

        fn frame(&mut self, events: Vec<egui::Event>, cards: &[Card]) -> f32 {
            self.frame_sized(events, cards, egui::vec2(1600.0, 900.0))
        }

        /// 実アプリの構造 (上部バー/ステータスバー/タブバー/中央エディタ) を
        /// 模したフレームを 1 枚描く。
        fn frame_sized(
            &mut self,
            events: Vec<egui::Event>,
            cards: &[Card],
            screen: egui::Vec2,
        ) -> f32 {
            self.t += 1.0 / 60.0;
            self.now_ms += 16;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, screen)),
                time: Some(self.t),
                events,
                ..Default::default()
            };
            let mut panel_h = 0.0_f32;
            let mut panel_top = 0.0_f32;
            let st = &mut self.st;
            let theme = &self.theme;
            let now_ms = self.now_ms;
            let _ = self.ctx.run(input, |ctx| {
                egui::TopBottomPanel::top("zv-top").show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("menu");
                    });
                });
                egui::TopBottomPanel::bottom("zv-status").show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("status");
                    });
                });
                egui::TopBottomPanel::bottom("zv-terminal")
                    .resizable(true)
                    .default_height(300.0)
                    .min_height(140.0)
                    .frame(egui::Frame::none().inner_margin(egui::Margin::same(6.0)))
                    .show_animated(ctx, true, |ui| {
                        panel_h = ui.max_rect().height();
                        panel_top = ui.max_rect().top() - 6.0;
                        // 実アプリ同様のタブバー (横スクロール + 右側コントロール)
                        ui.horizontal(|ui| {
                            egui::ScrollArea::horizontal()
                                .id_salt("term-tabs")
                                .max_width((ui.available_width() - 150.0).max(120.0))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let _ = ui.selectable_label(true, "🤖 Claude Code");
                                        let _ = ui.selectable_label(true, "📋 看板");
                                    });
                                });
                        });
                        ui.add_space(4.0);
                        let mut live = |_: &mut egui::Ui, _: usize| None;
                        let _ = super::ui(st, ui, theme, cards, &[], &[], now_ms, true, &mut live);
                    });
                egui::CentralPanel::default().show(ctx, |ui| {
                    // エディタ/ターミナル相当: 全域が click_and_drag を持つ
                    let size = ui.available_size();
                    let _ = ui.allocate_response(size, egui::Sense::click_and_drag());
                });
            });
            self.last_top = panel_top;
            panel_h
        }

        /// バーを `from_y` から `to_y` までドラッグして離す。フレームごとの
        /// パネル上端 y を返す (追従の観察用)。
        fn drag_bar(&mut self, from_y: f32, to_y: f32, cards: &[Card]) -> Vec<f32> {
            let mut tops = Vec::new();
            let x = 800.0;
            self.frame(
                vec![egui::Event::PointerMoved(egui::pos2(x, from_y))],
                cards,
            );
            self.frame(
                vec![egui::Event::PointerButton {
                    pos: egui::pos2(x, from_y),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                }],
                cards,
            );
            let step = if to_y < from_y { -25.0 } else { 25.0 };
            let mut y = from_y;
            while (y - to_y).abs() > 25.0 {
                y += step;
                self.frame(vec![egui::Event::PointerMoved(egui::pos2(x, y))], cards);
                tops.push(self.last_top);
            }
            self.frame(vec![egui::Event::PointerMoved(egui::pos2(x, to_y))], cards);
            tops.push(self.last_top);
            self.frame(
                vec![egui::Event::PointerButton {
                    pos: egui::pos2(x, to_y),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                cards,
            );
            tops.push(self.last_top);
            tops
        }
    }

    /// パネル上端のバーを画面中央までドラッグしたら、その高さに留まること。
    /// (中身がパネル高さより低いと egui が実コンテンツ矩形を次フレームの
    ///  高さとして保存するため、バーがずり落ちるリグレッションを検知する)
    #[test]
    fn kanban_panel_keeps_dragged_height() {
        let mut h = PanelHarness::new();
        let cards = vec![card(Column::Ready, true)];
        let none: Vec<egui::Event> = Vec::new();

        for _ in 0..3 {
            h.frame(none.clone(), &cards);
        }
        let start_top = h.last_top;
        // バーをつかんで画面中央 (y=450) まで引き上げる
        let tops = h.drag_bar(start_top, 450.0, &cards);
        // 手を離した後も高さが維持されること
        let mut after = Vec::new();
        for _ in 0..8 {
            h.frame(none.clone(), &cards);
            after.push(h.last_top);
        }
        let released = *tops.last().unwrap();
        assert!(
            released < 470.0,
            "ドラッグ中にバーが追従していない: start={start_top} tops={tops:?}"
        );
        let last = *after.last().unwrap();
        assert!(
            (released - last).abs() < 10.0,
            "手を離すとバーがずり落ちる: released={released} after={after:?}"
        );
    }

    /// 画面上端近くまで引き上げても維持されること (中央越え)。
    #[test]
    fn kanban_panel_keeps_height_near_top() {
        let mut h = PanelHarness::new();
        let cards = vec![card(Column::Ready, true)];
        let none: Vec<egui::Event> = Vec::new();
        for _ in 0..3 {
            h.frame(none.clone(), &cards);
        }
        let tops = h.drag_bar(h.last_top, 60.0, &cards);
        let mut after = Vec::new();
        for _ in 0..8 {
            h.frame(none.clone(), &cards);
            after.push(h.last_top);
        }
        let released = *tops.last().unwrap();
        let last = *after.last().unwrap();
        assert!(
            (released - last).abs() < 10.0,
            "上端付近でずり落ちる: released={released} after={after:?} tops={tops:?}"
        );
    }

    /// カードが 0 枚 (empty_ui) でもずり落ちないこと。
    #[test]
    fn kanban_panel_keeps_height_with_no_cards() {
        let mut h = PanelHarness::new();
        let cards: Vec<Card> = Vec::new();
        let none: Vec<egui::Event> = Vec::new();
        for _ in 0..3 {
            h.frame(none.clone(), &cards);
        }
        let tops = h.drag_bar(h.last_top, 450.0, &cards);
        let mut after = Vec::new();
        for _ in 0..8 {
            h.frame(none.clone(), &cards);
            after.push(h.last_top);
        }
        let released = *tops.last().unwrap();
        let last = *after.last().unwrap();
        assert!(
            (released - last).abs() < 10.0,
            "カード0枚でずり落ちる: released={released} after={after:?} tops={tops:?}"
        );
    }

    /// 下方向 (縮める) ドラッグも追従して維持されること。
    #[test]
    fn kanban_panel_shrinks_and_stays() {
        let mut h = PanelHarness::new();
        let cards = vec![card(Column::Ready, true)];
        let none: Vec<egui::Event> = Vec::new();
        for _ in 0..3 {
            h.frame(none.clone(), &cards);
        }
        h.drag_bar(h.last_top, 400.0, &cards);
        for _ in 0..3 {
            h.frame(none.clone(), &cards);
        }
        // いったん上げてから 650 まで下げ直す
        let tops = h.drag_bar(h.last_top, 650.0, &cards);
        let mut after = Vec::new();
        for _ in 0..8 {
            h.frame(none.clone(), &cards);
            after.push(h.last_top);
        }
        let released = *tops.last().unwrap();
        let last = *after.last().unwrap();
        assert!(
            released > 600.0,
            "縮めるドラッグが追従しない: tops={tops:?}"
        );
        assert!(
            (released - last).abs() < 10.0,
            "縮めた後に高さが跳ね戻る: released={released} after={after:?}"
        );
    }

    /// ウィンドウ矩形が一時的に縮んでも (fullscreen_guard の遷移など)、
    /// 戻ったときにパネル高さが失われないこと。
    #[test]
    fn kanban_panel_survives_screen_rect_wobble() {
        let mut h = PanelHarness::new();
        let cards = vec![card(Column::Ready, true)];
        let none: Vec<egui::Event> = Vec::new();
        for _ in 0..3 {
            h.frame(none.clone(), &cards);
        }
        h.drag_bar(h.last_top, 450.0, &cards);
        let before = h.last_top;
        // 一時的に縦 600 に縮む → 900 に戻る
        for _ in 0..3 {
            h.frame_sized(none.clone(), &cards, egui::vec2(1600.0, 600.0));
        }
        let mut after = Vec::new();
        for _ in 0..5 {
            h.frame(none.clone(), &cards);
            after.push(h.last_top);
        }
        let last = *after.last().unwrap();
        assert!(
            (before - last).abs() < 10.0,
            "画面矩形の変動で高さが失われる: before={before} after={after:?}"
        );
    }

    // ── #1 ライブペインの全画面 ────────────────────────────────────────

    fn esc() -> Vec<egui::Event> {
        vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]
    }

    /// **全画面の出入りで選択もスクロールも失わない。Esc で降りられる。永続する。**
    #[test]
    fn 全画面の出入りで選択とスクロールを失わない() {
        let mut h = PanelHarness::new();
        let cards = vec![
            card_id(11, Column::Ready),
            card_id(22, Column::Editing),
            card_id(33, Column::Verifying),
        ];
        let none: Vec<egui::Event> = Vec::new();
        for _ in 0..3 {
            h.frame(none.clone(), &cards);
        }
        // 明示的にライブペインを開き、2 枚目を選んでおく
        h.st.selected = Some(22);
        h.st.live_open = true;
        h.frame(none.clone(), &cards);
        h.st.scroll_to_sel = false; // 起動直後の 1 回ぶんを消化しておく
        let before = h.st.selected;
        assert_eq!(before, Some(22));

        // 全画面へ
        h.st.set_live_full(true);
        h.frame(none.clone(), &cards);
        assert!(h.st.live_full, "全画面にならない");
        assert_eq!(h.st.selected, before, "全画面にしたら選択が変わった");
        assert!(!h.st.scroll_to_sel, "全画面にしただけでスクロールが動く");
        // 永続メモリに残る (次回起動時も同じ見せ方)
        let saved: Option<bool> = h.ctx.data_mut(|d| d.get_persisted(live_full_id()));
        assert_eq!(saved, Some(true), "全画面の選択が永続していない");

        // Esc で降りる (端末にフォーカスが無くても出口がある)
        h.frame(esc(), &cards);
        assert!(!h.st.live_full, "Esc で全画面から降りられない");
        assert_eq!(h.st.selected, before, "全画面を出たら選択が変わった");
        assert!(!h.st.scroll_to_sel, "全画面を出ただけでスクロールが動く");
        assert!(
            h.st.live_open,
            "全画面をやめただけで端末まで閉じてはいけない"
        );
        h.frame(none.clone(), &cards);
        let saved: Option<bool> = h.ctx.data_mut(|d| d.get_persisted(live_full_id()));
        assert_eq!(saved, Some(false));
    }

    /// 全画面でも**アイドル時の再描画コストは増えない** (原則 #3 の非回帰)。
    #[test]
    fn 全画面でもアイドル時は寝たまま() {
        let mut h = PanelHarness::new();
        let mut c = card_id(1, Column::Done);
        c.running = false;
        let cards = vec![c];
        let none: Vec<egui::Event> = Vec::new();
        h.frame(none.clone(), &cards);
        h.st.live_open = true;
        h.st.set_live_full(true);
        // 着地ハイライトが切れるまで回す (それ以降は 1 フレームも急がない)
        for _ in 0..80 {
            h.frame(none.clone(), &cards);
        }
        assert!(!h.st.animating && !h.st.busy && !h.st.any_running);
        assert_eq!(h.st.next_repaint_ms(), ASLEEP_REPAINT_MS);
    }
}

/// 看板の幾何 (純関数)。**スクリーンショットで確認した不具合を数値で固定する**:
///
/// ① 1 体起動しただけで端末が半分の幅を占め、「承認」が半分隠れて
///    「完了・終了」が画面外へ落ちた (= 起動が画面を組み替えていた)
/// ② 空のレーンが 5 本、窓の底まで伸びていた
/// ③ ライブペインが右端で切れて端末の行が途中で欠けた
/// ④ 上の KPI タイルが右端で切れた
#[cfg(test)]
mod geometry_tests {
    use super::*;

    /// 代表的な窓 (900x700 と極端に低い窓を含む)。
    fn areas() -> Vec<Rect> {
        [
            (600.0_f32, 400.0_f32),
            (900.0, 700.0),
            (1200.0, 300.0), // 極端に低い窓
            (1400.0, 900.0),
            (1720.0, 1148.0),
            (2560.0, 1440.0),
        ]
        .into_iter()
        .map(|(w, h)| Rect::from_min_size(egui::pos2(10.0, 24.0), egui::vec2(w, h)))
        .collect()
    }

    fn overlaps(a: Rect, b: Rect) -> bool {
        a.intersects(b) && a.intersect(b).area() > 0.01
    }

    /// **主要域の矩形は領域内に収まり、互いに重ならない。**
    #[test]
    fn 主要域の矩形は領域内で重ならない() {
        for area in areas() {
            for vertical in [false, true] {
                for live in [false, true] {
                    for split in [0.2_f32, 0.38, 0.7] {
                        let r = main_rects(area, vertical, live, split, false);
                        let all = r.all();
                        for rect in &all {
                            assert!(
                                rect.left() >= area.left() - 0.01
                                    && rect.right() <= area.right() + 0.01
                                    && rect.top() >= area.top() - 0.01
                                    && rect.bottom() <= area.bottom() + 0.01,
                                "area={area:?} v={vertical} live={live} s={split}: \
                                 {rect:?} が領域外"
                            );
                            assert!(rect.width() >= 0.0 && rect.height() >= 0.0);
                        }
                        for i in 0..all.len() {
                            for j in (i + 1)..all.len() {
                                assert!(
                                    !overlaps(all[i], all[j]),
                                    "area={area:?} v={vertical} live={live}: \
                                     {:?} と {:?} が重なった",
                                    all[i],
                                    all[j]
                                );
                            }
                        }
                        assert_eq!(r.live.is_some(), live, "ライブペインの有無が食い違う");
                        // ライブペインを開いている間は飾りを畳む (看板の幅を守る)
                        if live {
                            assert!(r.rail.is_none() && r.feed.is_none());
                        }
                    }
                }
            }
        }
    }

    /// ライブペインは**割り当てられた幅ぴったり**。
    /// フィードの取り分まで飲み込むと右端で端末の行が切れる (実際に起きた)。
    #[test]
    fn ライブペインはフィードの幅を食わない() {
        let area = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2000.0, 900.0));
        let r = main_rects(area, false, true, 0.38, false);
        let live = r.live.expect("開いている");
        assert!(live.right() <= area.right() + 0.01);
        // 飾りが畳まれているので、看板 + バー + ライブで領域を使い切る
        let used = r.board.expect("看板あり").width() + SPLIT_BAR + live.width() + space::SM * 2.0;
        assert!(
            (used - area.width()).abs() < 0.5,
            "used={used} area={}",
            area.width()
        );
    }

    /// **レーン幅**: 空レーンは帯だけに畳み、総幅は可用幅を超えない。
    #[test]
    fn レーン幅は可用幅を超えない() {
        for avail in [400.0_f32, 700.0, 900.0, 1200.0, 1600.0, 2400.0] {
            for filled in 0..=LANES {
                let counts: Vec<usize> = (0..LANES).map(|i| usize::from(i < filled)).collect();
                let w = lane_widths(avail, &counts);
                assert_eq!(w.len(), LANES);
                let total: f32 = w.iter().sum::<f32>() + space::SM * (LANES as f32 - 1.0);
                // 中身のあるレーンが下限に張り付くほど狭いときだけ横スクロールへ逃がす
                let at_min = w
                    .iter()
                    .zip(&counts)
                    .any(|(x, c)| *c > 0 && (*x - LANE_MIN_W).abs() < 0.01);
                if !at_min {
                    assert!(
                        total <= avail + 0.5,
                        "avail={avail} filled={filled}: 総幅 {total} がはみ出した ({w:?})"
                    );
                }
                for (x, c) in w.iter().zip(&counts) {
                    if *c == 0 {
                        assert!(*x <= LANE_EMPTY_W + 0.01, "空レーンが畳まれていない: {x}");
                    } else {
                        assert!(*x >= LANE_MIN_W - 0.01 && *x <= LANE_MAX_W + 0.01);
                    }
                }
            }
        }
    }

    /// 1 体だけ動いている 1400x900: 空レーンを畳めば全レーンが 1 画面に入る。
    #[test]
    fn 一体だけでも全レーンが一画面に入る() {
        let mut counts = [0usize; LANES];
        counts[1] = 1;
        let w = lane_widths(1400.0, &counts);
        let total: f32 = w.iter().sum::<f32>() + space::SM * (LANES as f32 - 1.0);
        assert!(total <= 1400.5, "総幅 {total}");
        assert!(w[1] > LANE_MIN_W, "中身のあるレーンは広く取る: {}", w[1]);
    }

    /// **KPI タイル**: 総幅は可用幅を超えない (右端で「完了」を切らない)。
    #[test]
    fn kpiタイルは可用幅を超えない() {
        for avail in [280.0_f32, 400.0, 560.0, 900.0, 1400.0, 2400.0] {
            let (cols, w) = kpi_grid(avail, LANES);
            assert!((1..=LANES).contains(&cols));
            let total = w * cols as f32 + space::SM * (cols as f32 - 1.0);
            assert!(total <= avail + 0.5, "avail={avail}: 総幅 {total}");
        }
        // 狭ければ折る / 広ければ 1 段
        assert_eq!(kpi_grid(1400.0, LANES).0, LANES);
        assert!(kpi_grid(400.0, LANES).0 < LANES);
    }

    /// ブロードキャスト欄は残り幅から取り、入らなければ出さない。
    #[test]
    fn ブロードキャスト欄は残り幅に従う() {
        assert_eq!(broadcast_input_width(0.0), 0.0);
        assert_eq!(broadcast_input_width(100.0), 0.0);
        assert!(broadcast_input_width(200.0) > 0.0);
        assert!(
            broadcast_input_width(2000.0) <= 260.0,
            "広くても広げすぎない"
        );
        for r in [0.0_f32, 50.0, 136.0, 300.0, 1000.0] {
            let w = broadcast_input_width(r);
            assert!(w == 0.0 || w <= r, "r={r}: {w} が残り幅を超えた");
        }
    }

    /// 見出しは狭いとアイコンだけに縮退する。
    #[test]
    fn 見出しは狭いとアイコンだけになる() {
        assert!(header_compact(700.0));
        assert!(header_compact(HEADER_COMPACT_W - 1.0));
        assert!(!header_compact(HEADER_COMPACT_W));
        assert!(!header_compact(1920.0));
    }

    /// 900x700 の窓: 8 レーンは横に並ばないので縦モードへ落ち、全レーンが読める。
    ///
    /// ここが横のままだと、1 レーン 105px しか無くカードの題名が読めない
    /// (スクリーンショットの「承認が半分隠れ、完了が画面外」)。
    #[test]
    fn 九百七百の窓では起動しても看板が読める() {
        let s = 0.38_f32;
        // 8 本を読める幅で並べるには BOARD_MIN_W が要る → 900px では縦へ落とす
        assert!(BOARD_MIN_W > 900.0, "8 本なら 900px には並ばない");
        assert!(use_vertical(LayoutMode::Auto, 900.0, 700.0, false, s));
        // 縦モードの看板は全幅を使い、カードは 1 列で全部読める
        let area = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 700.0));
        let r = main_rects(area, true, false, s, false);
        let board = r.board.expect("看板あり");
        assert_eq!(board.width(), area.width(), "縦モードの看板は全幅");
        assert_eq!(board.height(), area.height(), "閉じていれば全高");
        // 端末を開いても縦のまま (看板と端末が上下に分かれる)
        assert!(use_vertical(LayoutMode::Auto, 900.0, 700.0, true, s));
        let r = main_rects(area, true, true, s, false);
        let board = r.board.expect("看板あり");
        assert_eq!(board.width(), area.width());
        assert!(board.height() > 0.0 && r.live.expect("開いている").height() > 0.0);
        // 横モードを選べる幅なら 8 本が下限を割らずに並ぶ
        let mut counts = [0usize; LANES];
        counts[1] = 1;
        let w = lane_widths(BOARD_MIN_W, &counts);
        let total: f32 = w.iter().sum::<f32>() + space::SM * (LANES as f32 - 1.0);
        assert!(total <= BOARD_MIN_W + 0.5, "敷居幅に {LANES} 本が入らない");
    }

    /// **8 レーンの幾何**: どの窓幅でも全レーンが領域内に収まり、重ならない。
    /// 900x700 を含む代表的な窓で、レーンの矩形を実際に積んで確かめる。
    #[test]
    fn 全レーンの矩形は領域内に収まり重ならない() {
        // 中身の入り方をいろいろ変える (空レーンの畳みが効いているか)
        let patterns: &[[usize; LANES]] = &[
            [0, 0, 0, 0, 0, 0, 0, 0],
            [1, 0, 0, 0, 0, 0, 0, 0],
            [0, 3, 0, 0, 0, 0, 0, 0],
            [0, 0, 1, 0, 0, 2, 0, 0],
            [2, 5, 1, 3, 2, 1, 1, 4],
            [9, 9, 9, 9, 9, 9, 9, 9],
        ];
        for area in areas() {
            let r = main_rects(area, false, false, 0.38, false);
            let board = r.board.expect("看板あり");
            let avail = board.width();
            for counts in patterns {
                let w = lane_widths(avail, counts);
                assert_eq!(w.len(), LANES);
                // 空レーンは帯だけ / 中身のあるレーンは読める幅
                for (x, c) in w.iter().zip(counts) {
                    assert!(*x > 0.0);
                    if *c == 0 {
                        assert!(*x <= LANE_EMPTY_W + 0.01, "空レーンが畳まれていない: {x}");
                    }
                }
                // 左から順に積んだ矩形が重ならず、板の中に収まる
                let mut x = board.left();
                let mut rects: Vec<Rect> = Vec::with_capacity(LANES);
                for lw in &w {
                    rects.push(Rect::from_min_size(
                        egui::pos2(x, board.top()),
                        egui::vec2(*lw, board.height()),
                    ));
                    x += lw + space::SM;
                }
                for i in 0..rects.len() {
                    assert!(
                        rects[i].top() >= area.top() - 0.01
                            && rects[i].bottom() <= area.bottom() + 0.01,
                        "area={area:?}: レーン {i} が縦に溢れた"
                    );
                    for j in (i + 1)..rects.len() {
                        assert!(
                            !overlaps(rects[i], rects[j]),
                            "レーン {i} と {j} が重なった"
                        );
                    }
                }
                // 中身のあるレーンが下限に張り付いていなければ、右端も板の中
                let at_min = w
                    .iter()
                    .zip(counts)
                    .any(|(x, c)| *c > 0 && (*x - LANE_MIN_W).abs() < 0.01);
                if !at_min {
                    let right = rects.last().expect("LANES > 0").right();
                    assert!(
                        right <= board.right() + 0.5,
                        "area={area:?} counts={counts:?}: 右端 {right} が板 {} を越えた",
                        board.right()
                    );
                }
            }
        }
    }

    /// 横モードの敷居は**本数から導く** (定数直書きにしない — 本数を変えれば追随する)。
    #[test]
    fn 横モードの敷居はレーン本数から導く() {
        assert!(
            (BOARD_MIN_W - (LANE_MIN_W * LANES as f32 + space::SM * (LANES as f32 - 1.0))).abs()
                < 0.01
        );
        // 敷居ちょうどの幅に LANES 本が下限幅で並ぶ
        let counts = vec![1usize; LANES];
        let w = lane_widths(BOARD_MIN_W, &counts);
        for x in &w {
            assert!(*x >= LANE_MIN_W - 0.01, "敷居幅で下限を割った: {x}");
        }
        // 敷居を超える幅なら横 / 割れば縦
        assert!(!use_vertical(
            LayoutMode::Auto,
            BOARD_MIN_W + 1.0,
            700.0,
            false,
            0.38
        ));
        assert!(use_vertical(
            LayoutMode::Auto,
            BOARD_MIN_W - 1.0,
            400.0,
            false,
            0.38
        ));
    }

    /// **KPI タイルは段に折れても盤面を潰さない。**
    ///
    /// 8 本ぶんのタイルは狭い窓で 2〜3 段になる。段数ぶんの高さが取れないなら
    /// タイルより盤面を優先する (看板が読めなければ画面の意味が無い)。
    #[test]
    fn kpiタイルは段数を見て出し入れする() {
        for w in [400.0_f32, 700.0, 1100.0, 1600.0, 2400.0] {
            let (cols, _) = kpi_grid(w, LANES);
            let rows = LANES.div_ceil(cols.max(1)) as f32;
            // 出すと決めた高さでは、タイルと盤面の両方が席を持てる
            let h = BOARD_KEEP_H + KPI_ROW_H * rows;
            assert!(show_kpi(w, h), "w={w}: 出せるはずの高さで出ない");
            assert!(!show_kpi(w, h - 1.0), "w={w}: 高さ不足でも出してしまう");
            // 低い窓では必ず盤面優先
            assert!(!show_kpi(w, 200.0));
        }
    }

    /// **ライブペインの全画面**: 端末が領域を丸ごと取り、レーンの矩形は 1 つも無い。
    #[test]
    fn 全画面では端末が領域を占めレーンの矩形が無い() {
        for area in areas() {
            for vertical in [false, true] {
                for split in [0.2_f32, 0.38, 0.7] {
                    let r = main_rects(area, vertical, true, split, true);
                    assert_eq!(r.live, Some(area), "端末が領域と一致しない");
                    assert!(r.board.is_none(), "全画面なのに看板の矩形がある");
                    assert!(r.rail.is_none() && r.feed.is_none() && r.splitter.is_none());
                    // 置いた矩形は 1 つだけ = 空白も重なりも作りようがない
                    assert_eq!(r.all().len(), 1);
                }
                // 端末を開いていなければ full は無視される (レーンが消えない)
                let r = main_rects(area, vertical, false, 0.38, true);
                assert!(r.board.is_some() && r.live.is_none());
            }
        }
    }
}
