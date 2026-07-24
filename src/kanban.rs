//! フリート看板 (Fleet Kanban) — 全エージェントを俯瞰・指揮する Ops コンソール画面。
//! ターミナルパネル右端の「📋 看板」タブから、パネル内のビューとして開く。
//!
//! ダッシュボード構成 (Autonomous Ops Console):
//! - ヘッダー: 稼働数チップ + 連続稼働時間 + ブロードキャスト
//! - KPI タイル: 稼働中 / 作業中 / 要対応 / 完了 (ミニスパークライン付き)
//! - 左レール: エージェント一覧 — アバター + 状態 + 「いま何をしているか」一言
//! - 中央: 状態カラム (待機 / 作業中 / 承認待ち / 停滞・異常 / 完了) の看板。
//!   カードは supervisor の判定が変われば勝手に列を移動する — ドラッグ不要。
//!   各カードに割り当てタスクと画面末尾の一言 (ライブ) を出し、
//!   ホバーで末尾数行のライブプレビューを見せる。
//! - 右レール: アクティビティフィード (状態遷移の実況、LIVE)
//! - 下部: 処理スループットの折れ線 (作業中エージェント数の推移)
//!
//! 作法は orchestration.rs と同じ: 判断と描画はこのモジュール、
//! 副作用 (PTY への書き込み・起動・再起動…) は `KanbanAction` で app.rs へ返す。
//! ここでは Session を直接借りない (app.rs が `Card` へ写して渡す)。

use eframe::egui::{self, Color32, Pos2, RichText, Stroke};

use crate::i18n::{tr, trf};
use crate::supervisor;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// 列 (状態から一意に決まる)
// ---------------------------------------------------------------------------

/// カンバンの列。セッションの状態から [`column_for`] で一意に決まる。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Column {
    /// 手が空いている (指示を受けられる)
    Ready,
    /// 出力が動いている
    Working,
    /// ユーザーの承認/入力待ちで止まっている
    Approval,
    /// 停滞・ループ・エラー多発・レート制限
    Trouble,
    /// 完了、またはプロセス終了
    Done,
}

/// 表示順 (左 → 右)。
pub const COLUMNS: [Column; 5] = [
    Column::Ready,
    Column::Working,
    Column::Approval,
    Column::Trouble,
    Column::Done,
];

impl Column {
    /// 列見出し (tr のキーになる日本語原文)。
    pub fn title(self) -> &'static str {
        match self {
            Column::Ready => "待機",
            Column::Working => "作業中",
            Column::Approval => "承認待ち",
            Column::Trouble => "停滞・異常",
            Column::Done => "完了・終了",
        }
    }

    /// 列見出しのホバー説明 (tr のキー)。
    fn hint(self) -> &'static str {
        match self {
            Column::Ready => "手が空いています — 指示を送るとすぐ動きます",
            Column::Working => "いま出力が動いています",
            Column::Approval => "あなたの承認・入力を待って止まっています",
            Column::Trouble => "停滞・ループ・エラー多発・レート制限 — 様子を見てあげてください",
            Column::Done => "タスク完了、またはプロセスが終了しています",
        }
    }

    /// 列のアクセント色。カードの状態ドット・チップも同じ色を使う。
    fn color(self, th: &Theme) -> Color32 {
        match self {
            Column::Ready => th.accent,
            Column::Working => th.ok,
            Column::Approval => th.warn,
            Column::Trouble => th.err,
            Column::Done => th.text_dim,
        }
    }
}

/// セッションの生存フラグ + supervisor 判定から列を決める **純関数**。
///
/// 優先順位は app.rs `coordinator_state` と同じ
/// (終了 > 承認待ち > レート制限 > supervisor 判定)。順序を揃えておかないと、
/// 看板の見た目と coordinator の配達判断が食い違って混乱する。
pub fn column_for(
    running: bool,
    attention: bool,
    rate_limited: bool,
    sup: Option<supervisor::SessionState>,
) -> Column {
    use supervisor::SessionState as S;
    if !running {
        return Column::Done;
    }
    if attention {
        return Column::Approval;
    }
    if rate_limited {
        return Column::Trouble;
    }
    match sup {
        Some(S::Working) => Column::Working,
        Some(S::WaitingApproval) => Column::Approval,
        Some(S::Idle) => Column::Ready,
        Some(S::Stalled) | Some(S::Looping) | Some(S::Errored) | Some(S::Crashed) => {
            Column::Trouble
        }
        Some(S::Done) => Column::Done,
        // まだ一度も観測していない起動直後は待機扱い (すぐ Working へ動く)
        None => Column::Ready,
    }
}

/// カードに出す状態ラベル (tr のキーになる日本語原文)。優先順位は [`column_for`] と同じ。
pub fn state_label(
    running: bool,
    attention: bool,
    rate_limited: bool,
    sup: Option<supervisor::SessionState>,
) -> &'static str {
    if !running {
        return "終了";
    }
    if attention {
        return "承認待ち";
    }
    if rate_limited {
        return "レート制限";
    }
    match sup {
        Some(s) => s.label(),
        None => "起動中",
    }
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
    /// ⚡/🛡 (権限モード対応エージェントのみ、他は "")
    pub permission_badge: &'static str,
    /// 権限モード切替キーを送れるか
    pub can_cycle: bool,
    /// 画面末尾の「意味のある行」たち (時系列順)。最終行が「いま何をしているか」の
    /// 一言になり、全行はホバーのライブプレビューに出す。
    pub tail_lines: Vec<String>,
    /// coordinator に割り当て中のタスク名
    pub task: Option<String>,
}

/// 画面末尾から「いま何をしているか」の一言を取り出す (最後の非空行)。
pub fn now_line(tail: &[String]) -> Option<&str> {
    tail.iter().rev().map(|l| l.trim()).find(|l| !l.is_empty())
}

/// 列ごとの人数の集計。KPI タイルとスループット履歴のデータ点になる。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Tally {
    pub total: usize,
    pub running: usize,
    pub ready: usize,
    pub working: usize,
    pub approval: usize,
    pub trouble: usize,
    pub done: usize,
}

/// カード一覧から列集計を作る純関数。
pub fn tally(cards: &[Card]) -> Tally {
    let mut t = Tally {
        total: cards.len(),
        ..Tally::default()
    };
    for c in cards {
        if c.running {
            t.running += 1;
        }
        match c.column {
            Column::Ready => t.ready += 1,
            Column::Working => t.working += 1,
            Column::Approval => t.approval += 1,
            Column::Trouble => t.trouble += 1,
            Column::Done => t.done += 1,
        }
    }
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
}

impl KanbanState {
    /// 集計を履歴へ記録する。呼び出しは毎フレームで良い (内部で間引く)。
    pub fn record_sample(&mut self, now_ms: u64, t: Tally) {
        match self.samples.last_mut() {
            Some(last) if now_ms.saturating_sub(last.at_ms) < SAMPLE_MS => last.tally = t,
            _ => self.samples.push(Sample { at_ms: now_ms, tally: t }),
        }
        if self.samples.len() > MAX_SAMPLES {
            let drop = self.samples.len() - MAX_SAMPLES;
            self.samples.drain(..drop);
        }
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
    Send { idx: usize, text: String },
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

/// エージェントごとの安定したアバター色 (参考画像と同系の 6 色パレット)。
fn avatar_color(id: u64) -> Color32 {
    const PALETTE: [Color32; 6] = [
        Color32::from_rgb(0x3b, 0x82, 0xf6), // 青
        Color32::from_rgb(0xf5, 0x9e, 0x0b), // 橙
        Color32::from_rgb(0xec, 0x48, 0x99), // 桃
        Color32::from_rgb(0x10, 0xb9, 0x81), // 緑
        Color32::from_rgb(0x8b, 0x5c, 0xf6), // 紫
        Color32::from_rgb(0x06, 0xb6, 0xd4), // 水
    ];
    PALETTE[(id % PALETTE.len() as u64) as usize]
}

// ---------------------------------------------------------------------------
// 描画
// ---------------------------------------------------------------------------

/// 看板画面を描き、押された操作を返す。
///
/// `now_ms` は supervisor の経過時計 (アプリ起動からの ms)。連続稼働表示・
/// アクティビティの相対時刻・スループット履歴のサンプリングを全部この 1 本で賄う。
pub fn ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    cards: &[Card],
    presets: &[(String, String)],
    activity: &[ActivityEntry],
    now_ms: u64,
) -> Vec<KanbanAction> {
    let mut acts: Vec<KanbanAction> = Vec::new();
    let t = tally(cards);
    st.record_sample(now_ms, t);
    // ダッシュボードは「見ているだけで動く」のが売りなので、
    // 出力が止まっていても時計・LIVE ドット・チャートを進め続ける。
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(300));

    egui::Frame::none()
        .inner_margin(egui::Margin::same(10.0))
        .show(ui, |ui| {
            let wide = ui.available_width();
            let tall = ui.available_height();
            // 狭い画面では飾りを畳んで、看板本体に空間を譲る
            let show_rail = wide >= 1000.0;
            let show_feed = wide >= 780.0;
            let show_kpi = tall >= 340.0;
            let show_chart = tall >= 430.0;

            header_ui(st, ui, theme, &t, presets, now_ms, &mut acts);

            if cards.is_empty() {
                empty_ui(ui, theme, presets, &mut acts);
                return;
            }

            if show_kpi {
                kpi_ui(ui, theme, st, &t);
                ui.add_space(8.0);
            }

            let chart_h = if show_chart { 96.0 } else { 0.0 };
            let main_h = (ui.available_height()
                - chart_h
                - if show_chart { 8.0 } else { 0.0 })
            .max(160.0);
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), main_h),
                egui::Layout::left_to_right(egui::Align::Min),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if show_rail {
                        rail_ui(ui, theme, cards, main_h, &mut acts);
                    }
                    let feed_w = if show_feed { 250.0 } else { 0.0 };
                    let board_w = (ui.available_width()
                        - if show_feed { feed_w + 8.0 } else { 0.0 })
                    .max(200.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(board_w, main_h),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            board_ui(st, ui, theme, cards, main_h, &mut acts);
                        },
                    );
                    if show_feed {
                        feed_ui(ui, theme, activity, feed_w, main_h, now_ms);
                    }
                },
            );

            if show_chart {
                ui.add_space(8.0);
                chart_ui(ui, theme, st);
            }
        });

    acts
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

fn header_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    t: &Tally,
    presets: &[(String, String)],
    now_ms: u64,
    acts: &mut Vec<KanbanAction>,
) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("📋 FLEET KANBAN")
                .size(17.0)
                .strong()
                .color(theme.text),
        )
        .on_hover_text(tr(
            "カードは状態が変わると自動でレーンを移動します — ドラッグは不要です",
        ));
        ui.label(
            RichText::new("Autonomous Ops Console")
                .size(11.0)
                .color(theme.text_dim),
        );
        chip(
            ui,
            theme.ok,
            &trf("{n} 稼働中", &[("n", t.running.to_string())]),
        );
        if t.approval > 0 {
            chip(
                ui,
                theme.warn,
                &trf("承認待ち {n}", &[("n", t.approval.to_string())]),
            );
        }
        if t.trouble > 0 {
            chip(
                ui,
                theme.err,
                &trf("要注意 {n}", &[("n", t.trouble.to_string())]),
            );
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(tr("✕ 閉じる")).clicked() {
                acts.push(KanbanAction::Close);
            }
            if ui
                .button("🎛 Cockpit")
                .on_hover_text(tr("Cockpit へ切替"))
                .clicked()
            {
                acts.push(KanbanAction::OpenCockpit);
            }
            ui.menu_button("＋ Agent", |ui| {
                for (i, (icon, name)) in presets.iter().enumerate() {
                    if ui.button(format!("{icon} {name}")).clicked() {
                        acts.push(KanbanAction::Launch(i));
                        ui.close_menu();
                    }
                }
            });
            let send = ui.button(tr("📣 送信"));
            let input = ui.add(
                egui::TextEdit::singleline(&mut st.broadcast_input)
                    .desired_width(220.0)
                    .hint_text(tr("全エージェントへブロードキャスト…")),
            );
            let enter = input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (send.clicked() || enter) && !st.broadcast_input.trim().is_empty() {
                acts.push(KanbanAction::Broadcast(st.broadcast_input.trim().to_string()));
                st.broadcast_input.clear();
            }
            ui.label(
                RichText::new(trf("連続稼働 {t}", &[("t", fmt_uptime(now_ms))]))
                    .size(11.0)
                    .color(theme.text_dim),
            );
        });
    });
    ui.add_space(8.0);
}

fn empty_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    presets: &[(String, String)],
    acts: &mut Vec<KanbanAction>,
) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.25);
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
        ui.add_space(12.0);
        for (i, (icon, name)) in presets.iter().enumerate() {
            if ui
                .add_sized([280.0, 34.0], egui::Button::new(format!("{icon} {name}")))
                .clicked()
            {
                acts.push(KanbanAction::Launch(i));
            }
        }
    });
}

// ---------------------------------------------------------------------------
// KPI タイル
// ---------------------------------------------------------------------------

fn kpi_ui(ui: &mut egui::Ui, theme: &Theme, st: &KanbanState, t: &Tally) {
    let gap = 8.0;
    let tile_w = ((ui.available_width() - gap * 3.0) / 4.0).max(120.0);
    let tiles: [(&str, usize, Color32, fn(&Tally) -> usize); 4] = [
        ("稼働中", t.running, theme.accent, |t| t.running),
        ("作業中", t.working, theme.ok, |t| t.working),
        ("要対応", t.approval + t.trouble, theme.warn, |t| {
            t.approval + t.trouble
        }),
        ("完了・終了", t.done, theme.text_dim, |t| t.done),
    ];
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (label, value, color, pick) in tiles {
            egui::Frame::none()
                .fill(theme.panel)
                .stroke(Stroke::new(1.0_f32, theme.border))
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin::same(9.0))
                .show(ui, |ui| {
                    ui.set_width(tile_w - 18.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(tr(label)).size(11.0).color(theme.text_dim));
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
                            .color(theme.text),
                    );
                    let values: Vec<f32> =
                        st.samples.iter().map(|s| pick(&s.tally) as f32).collect();
                    sparkline(ui, 14.0, color, &values);
                });
        }
    });
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

fn rail_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    cards: &[Card],
    height: f32,
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
                            for c in cards {
                                rail_entry_ui(ui, theme, c, acts);
                                ui.add_space(4.0);
                            }
                        });
                });
        },
    );
}

fn rail_entry_ui(ui: &mut egui::Ui, theme: &Theme, c: &Card, acts: &mut Vec<KanbanAction>) {
    let col_color = c.column.color(theme);
    let stroke = if c.active {
        Stroke::new(1.5_f32, theme.accent)
    } else {
        Stroke::new(1.0_f32, Color32::TRANSPARENT)
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
                        // アバター: エージェント色の円 + アイコン
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(22.0, 22.0),
                            egui::Sense::hover(),
                        );
                        let color = avatar_color(c.id);
                        ui.painter().circle_filled(rect.center(), 11.0, color);
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            &c.icon,
                            egui::FontId::proportional(12.0),
                            Color32::WHITE,
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
                            ui.label(
                                RichText::new(&c.state_label)
                                    .size(10.0)
                                    .strong()
                                    .color(col_color),
                            );
                        });
                    });
                    // 「いま何をしているか」一言 (タスク優先、無ければ画面末尾)
                    let doing = c
                        .task
                        .as_deref()
                        .or_else(|| now_line(&c.tail_lines))
                        .unwrap_or("");
                    if !doing.is_empty() {
                        ui.add(
                            egui::Label::new(
                                RichText::new(doing).size(10.0).color(theme.text_dim),
                            )
                            .truncate(),
                        );
                    }
                });
        },
    );
    if cell.response.clicked() {
        acts.push(KanbanAction::Select(c.idx));
    }
}

// ---------------------------------------------------------------------------
// 中央: 看板カラム
// ---------------------------------------------------------------------------

fn board_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    cards: &[Card],
    height: f32,
    acts: &mut Vec<KanbanAction>,
) {
    let gap = 8.0;
    let n = COLUMNS.len() as f32;
    let col_w = ((ui.available_width() - gap * (n - 1.0)) / n).clamp(172.0, 340.0);
    egui::ScrollArea::horizontal()
        .id_salt("kanban-board-h")
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for col in COLUMNS {
                    let members: Vec<&Card> =
                        cards.iter().filter(|c| c.column == col).collect();
                    column_ui(st, ui, theme, col, &members, col_w, height, acts);
                }
            });
        });
}

/// 状態カラム 1 本: 色付き見出し + 件数 + 所属カード (縦スクロール)。
#[allow(clippy::too_many_arguments)]
fn column_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    col: Column,
    members: &[&Card],
    width: f32,
    height: f32,
    acts: &mut Vec<KanbanAction>,
) {
    let color = col.color(theme);
    ui.allocate_ui_with_layout(
        egui::vec2(width, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::Frame::none()
                .fill(theme.panel)
                .stroke(Stroke::new(1.0_f32, theme.border))
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin::same(7.0))
                .show(ui, |ui| {
                    ui.set_width(width - 14.0);
                    ui.set_min_height(height - 14.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("●").size(10.0).color(color));
                        ui.label(
                            RichText::new(tr(col.title()))
                                .size(12.5)
                                .strong()
                                .color(theme.text),
                        )
                        .on_hover_text(tr(col.hint()));
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    RichText::new(members.len().to_string())
                                        .size(11.0)
                                        .color(theme.text_dim),
                                );
                            },
                        );
                    });
                    // レーンカラーの下線 (どの状態かが遠目にも分かる)
                    let (line, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 2.0),
                        egui::Sense::hover(),
                    );
                    ui.painter()
                        .rect_filled(line, 1.0_f32, color.gamma_multiply(0.6));
                    ui.add_space(6.0);

                    if members.is_empty() {
                        ui.label(
                            RichText::new(tr("— なし —"))
                                .size(10.5)
                                .color(theme.text_dim),
                        );
                        return;
                    }
                    egui::ScrollArea::vertical()
                        .id_salt(("kanban-col", col.title()))
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            for c in members {
                                card_ui(st, ui, theme, c, acts);
                                ui.add_space(6.0);
                            }
                        });
                });
        },
    );
}

fn card_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    c: &Card,
    acts: &mut Vec<KanbanAction>,
) {
    let color = c.column.color(theme);
    let stroke = if c.active {
        Stroke::new(1.5_f32, theme.accent)
    } else {
        Stroke::new(1.0_f32, theme.border)
    };
    // 余白クリックでも選択できるように、コンテナの判定を子より先に登録する
    // (cockpit のセルと同じ作法。描画後の ui.interact だと子のクリックを奪う)。
    let cell = ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(("kanban-card", c.id))
            .sense(egui::Sense::click()),
        |ui| {
            egui::Frame::none()
                .fill(theme.panel_alt)
                .stroke(stroke)
                .rounding(egui::Rounding::same(7.0))
                .inner_margin(egui::Margin::same(7.0))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 3.0;
                        // 1 段目: 状態ドット + アイコン + 名前 / 右端: 稼働時間
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
                                        RichText::new(&c.uptime)
                                            .size(9.5)
                                            .color(theme.text_dim),
                                    );
                                },
                            );
                        });
                        // 2 段目: 状態チップ + ⏳ + 未読
                        ui.horizontal(|ui| {
                            egui::Frame::none()
                                .fill(color.gamma_multiply(0.16))
                                .rounding(egui::Rounding::same(5.0))
                                .inner_margin(egui::Margin::symmetric(6.0, 1.0))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(&c.state_label)
                                            .size(10.0)
                                            .strong()
                                            .color(color),
                                    );
                                });
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
                        // 3 段目: 📋 割り当てタスク = 何を実装させているか
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
                        // 4 段目: 「いま何をしているか」一言 (画面末尾のライブ行)。
                        // ホバーで末尾数行のライブプレビューを出す。
                        let doing = now_line(&c.tail_lines).unwrap_or("");
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
                        if !c.tail_lines.is_empty() {
                            resp.on_hover_ui(|ui| {
                                ui.set_max_width(460.0);
                                for line in &c.tail_lines {
                                    ui.label(
                                        RichText::new(line)
                                            .size(11.0)
                                            .monospace()
                                            .color(theme.text),
                                    );
                                }
                            });
                        }
                        // 承認待ちだけに出る目立つ操作列
                        if c.attention {
                            ui.horizontal(|ui| {
                                if ui
                                    .button(RichText::new(tr("✅ 承認")).color(theme.ok))
                                    .on_hover_text(tr(
                                        "画面のプロンプトに合った承認キーを送ります",
                                    ))
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
                        // 操作列
                        ui.horizontal(|ui| {
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
                        // ✏ 指示入力欄 (開いているカードだけ)
                        if st.prompt_for == Some(c.id) {
                            ui.horizontal(|ui| {
                                let input = ui.add(
                                    egui::TextEdit::singleline(&mut st.prompt_input)
                                        .desired_width(
                                            (ui.available_width() - 56.0).max(80.0),
                                        )
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
                                }
                            });
                        }
                    });
                });
        },
    );
    if cell.response.clicked()
        || (cell.response.contains_pointer()
            && ui.input(|i| i.pointer.primary_pressed()))
    {
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
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                live_dot(ui, theme, now_ms);
                            },
                        );
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
                                        RichText::new("●")
                                            .size(8.0)
                                            .color(e.column.color(theme)),
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
    let current = st.samples.last().map(|s| s.tally.working).unwrap_or(0);
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
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), 44.0),
                egui::Sense::hover(),
            );
            let painter = ui.painter();
            let values: Vec<f32> =
                st.samples.iter().map(|s| s.tally.working as f32).collect();
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
                        let x = rect.left()
                            + rect.width() * i as f32 / (values.len() - 1) as f32;
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
        assert_eq!(column_for(true, false, false, Some(S::Working)), Column::Working);
        assert_eq!(column_for(true, false, false, Some(S::Idle)), Column::Ready);
        assert_eq!(
            column_for(true, false, false, Some(S::WaitingApproval)),
            Column::Approval
        );
        for s in [S::Stalled, S::Looping, S::Errored, S::Crashed] {
            assert_eq!(column_for(true, false, false, Some(s)), Column::Trouble);
        }
        assert_eq!(column_for(true, false, false, Some(S::Done)), Column::Done);
        // 起動直後 (未観測) は待機扱い
        assert_eq!(column_for(true, false, false, None), Column::Ready);
    }

    #[test]
    fn state_label_follows_same_priority() {
        assert_eq!(state_label(false, false, false, Some(S::Working)), "終了");
        assert_eq!(state_label(true, true, false, None), "承認待ち");
        assert_eq!(state_label(true, false, true, None), "レート制限");
        assert_eq!(state_label(true, false, false, Some(S::Working)), "作業中");
        assert_eq!(state_label(true, false, false, None), "起動中");
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
            permission_badge: "",
            can_cycle: false,
            tail_lines: Vec::new(),
            task: None,
        }
    }

    #[test]
    fn tally_counts_columns_and_running() {
        let cards = vec![
            card(Column::Working, true),
            card(Column::Working, true),
            card(Column::Approval, true),
            card(Column::Done, false),
        ];
        let t = tally(&cards);
        assert_eq!(t.total, 4);
        assert_eq!(t.running, 3);
        assert_eq!(t.working, 2);
        assert_eq!(t.approval, 1);
        assert_eq!(t.done, 1);
        assert_eq!(t.ready, 0);
        assert_eq!(t.trouble, 0);
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
        let t1 = Tally { working: 1, ..Tally::default() };
        let t2 = Tally { working: 2, ..Tally::default() };
        st.record_sample(0, t1);
        // 2 秒未満は最新点の上書き (点は増えない)
        st.record_sample(500, t2);
        assert_eq!(st.samples.len(), 1);
        assert_eq!(st.samples[0].tally.working, 2);
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
}
