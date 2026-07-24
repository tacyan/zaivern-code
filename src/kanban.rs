//! フリート看板 (Fleet Kanban) — 全エージェントを状態レーンで俯瞰・指揮する画面。
//! ターミナルパネル右端の「📋 看板」タブから、パネル内のビューとして開く。
//!
//! 各エージェントセッションを 1 枚のカードとして、状態
//! (待機 / 作業中 / 承認待ち / 停滞・異常 / 完了・終了) のレーンに自動で並べる。
//! カードは画面の横幅いっぱいを使い、割り当てタスクと画面末尾のライブ
//! プレビューを載せる — 「いま何を実装中か」がカードだけで分かるのが狙い。
//! ドラッグは不要 — supervisor の状態判定が変われば
//! カードが勝手にレーンを移動するので、目視で「誰が何をしているか」が分かる。
//!
//! 作法は orchestration.rs と同じ: 判断と描画はこのモジュール、
//! 副作用 (PTY への書き込み・起動・再起動…) は `KanbanAction` で app.rs へ返す。
//! ここでは Session を直接借りない (app.rs が `Card` へ写して渡す)。

use eframe::egui::{self, Color32, RichText};

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
// カード / UI 状態 / アクション
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
    /// 画面末尾の「意味のある行」たち (時系列順) = いま何を実装中かのライブプレビュー
    pub tail_lines: Vec<String>,
    /// coordinator に割り当て中のタスク名
    pub task: Option<String>,
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
// 描画
// ---------------------------------------------------------------------------

/// 看板画面を描き、押された操作を返す。
pub fn ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    cards: &[Card],
    presets: &[(String, String)],
) -> Vec<KanbanAction> {
    let mut acts: Vec<KanbanAction> = Vec::new();

    egui::Frame::none()
        .inner_margin(egui::Margin::same(12.0))
        .show(ui, |ui| {
            header_ui(st, ui, theme, cards, presets, &mut acts);

            if cards.is_empty() {
                empty_ui(ui, theme, presets, &mut acts);
                return;
            }
            board_ui(st, ui, theme, cards, &mut acts);
        });

    acts
}

fn header_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    cards: &[Card],
    presets: &[(String, String)],
    acts: &mut Vec<KanbanAction>,
) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("📋 Fleet Kanban")
                .size(20.0)
                .strong()
                .color(theme.accent),
        );
        let running = cards.iter().filter(|c| c.running).count();
        let total = cards.len();
        ui.label(
            RichText::new(trf(
                "{running} 稼働中 / {total} セッション",
                &[("running", running.to_string()), ("total", total.to_string())],
            ))
            .color(theme.text_dim),
        );
        // 列ごとの件数ミニ集計 (どこが詰まっているか一目で分かる)
        for col in COLUMNS {
            let n = cards.iter().filter(|c| c.column == col).count();
            if n > 0 {
                ui.label(
                    RichText::new(format!("●{n}"))
                        .size(11.0)
                        .color(col.color(theme)),
                )
                .on_hover_text(tr(col.title()));
            }
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
                    .desired_width(280.0)
                    .hint_text(tr("全エージェントへブロードキャスト…")),
            );
            let enter = input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (send.clicked() || enter) && !st.broadcast_input.trim().is_empty() {
                acts.push(KanbanAction::Broadcast(st.broadcast_input.trim().to_string()));
                st.broadcast_input.clear();
            }
        });
    });
    ui.label(
        RichText::new(tr(
            "カードは状態が変わると自動でレーンを移動します — ドラッグは不要です",
        ))
        .size(11.5)
        .color(theme.text_dim),
    );
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

fn board_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    cards: &[Card],
    acts: &mut Vec<KanbanAction>,
) {
    // カードに横幅いっぱいを使わせたいので、列を横に並べず
    // 「状態レーン (フル幅の帯) を縦に積む」レイアウトにする。
    // 空のレーンは出さない — 限られた縦空間を実物のカードに回す。
    egui::ScrollArea::vertical()
        .id_salt("kanban-board")
        .auto_shrink(false)
        .show(ui, |ui| {
            for col in COLUMNS {
                let members: Vec<&Card> =
                    cards.iter().filter(|c| c.column == col).collect();
                if members.is_empty() {
                    continue;
                }
                lane_ui(st, ui, theme, col, &members, acts);
                ui.add_space(10.0);
            }
        });
}

/// フル幅の状態レーン: 色付き見出し + 下線 + 所属カード (縦積み)。
fn lane_ui(
    st: &mut KanbanState,
    ui: &mut egui::Ui,
    theme: &Theme,
    col: Column,
    members: &[&Card],
    acts: &mut Vec<KanbanAction>,
) {
    let color = col.color(theme);
    ui.horizontal(|ui| {
        ui.label(RichText::new("●").color(color));
        ui.label(
            RichText::new(tr(col.title()))
                .size(15.0)
                .strong()
                .color(theme.text),
        )
        .on_hover_text(tr(col.hint()));
        ui.label(RichText::new(members.len().to_string()).color(theme.text_dim));
    });
    // レーンカラーの下線 (どの状態かが遠目にも分かる)
    let (line, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 2.0),
        egui::Sense::hover(),
    );
    ui.painter()
        .rect_filled(line, 1.0_f32, color.gamma_multiply(0.6));
    ui.add_space(8.0);

    for c in members {
        card_ui(st, ui, theme, c, acts);
        ui.add_space(10.0);
    }
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
        egui::Stroke::new(2.0_f32, theme.accent)
    } else {
        egui::Stroke::new(1.0_f32, theme.border)
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
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin::same(8.0))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width());
                        // 1 段目: 状態ドット + 権限バッジ + アイコン + タイトル +
                        //         状態チップ + ⏳ + 未読 / 右端: 稼働時間
                        ui.horizontal(|ui| {
                            let dot = if c.running { "●" } else { "○" };
                            ui.label(RichText::new(dot).color(color));
                            ui.label(
                                RichText::new(format!(
                                    "{}{} {}",
                                    c.permission_badge, c.icon, c.title
                                ))
                                .size(15.0)
                                .strong()
                                .color(theme.text),
                            );
                            egui::Frame::none()
                                .fill(theme.accent_soft)
                                .rounding(egui::Rounding::same(6.0))
                                .inner_margin(egui::Margin::symmetric(7.0, 2.0))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(&c.state_label)
                                            .size(11.5)
                                            .strong()
                                            .color(color),
                                    );
                                });
                            if let Some(line) = &c.rate_limited {
                                ui.label(RichText::new("⏳").color(theme.warn))
                                    .on_hover_text(trf(
                                        "レート制限/使用上限: {line}",
                                        &[("line", line.clone())],
                                    ));
                            }
                            if c.unread {
                                ui.label(RichText::new("◆").size(9.0).color(theme.accent))
                                    .on_hover_text(tr("最後に見てから新しい出力があります"));
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        RichText::new(&c.uptime)
                                            .size(10.5)
                                            .color(theme.text_dim),
                                    );
                                },
                            );
                        });
                        // 2 段目: 📋 割り当てタスク = 何を実装させているか (あれば大きく)
                        if let Some(task) = &c.task {
                            ui.add_space(2.0);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!("📋 {task}"))
                                        .size(13.0)
                                        .color(theme.text),
                                )
                                .truncate(),
                            )
                            .on_hover_text(task);
                        }
                        // 3 段目: ライブ画面プレビュー = 画面末尾の意味のある数行。
                        // 「いま何を実装中か」をカードから離れずに読めるようにする。
                        ui.add_space(6.0);
                        egui::Frame::none()
                            .fill(theme.bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border))
                            .rounding(egui::Rounding::same(6.0))
                            .inner_margin(egui::Margin::same(8.0))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.spacing_mut().item_spacing.y = 2.0;
                                if c.tail_lines.is_empty() {
                                    ui.label(
                                        RichText::new(tr("まだ出力がありません"))
                                            .size(11.5)
                                            .color(theme.text_dim),
                                    );
                                }
                                let last = c.tail_lines.len().saturating_sub(1);
                                for (i, line) in c.tail_lines.iter().enumerate() {
                                    // 最新行だけ明るくする = 目が「いまの行」へ行く
                                    let col =
                                        if i == last { theme.text } else { theme.text_dim };
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(line)
                                                .size(11.5)
                                                .monospace()
                                                .color(col),
                                        )
                                        .truncate(),
                                    );
                                }
                            });
                        // 承認待ちだけに出る目立つ操作列
                        if c.attention {
                            ui.add_space(2.0);
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
                        ui.add_space(2.0);
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
}
