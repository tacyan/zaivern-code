//! VS Code 準拠のメニューバー。
//!
//! ファイル / 編集 / 選択 / 表示 / 移動 / 実行 / ターミナル / ヘルプ の
//! 8 メニューを VS Code と同じ並び・同じ項目名 (日本語版 VS Code 準拠) で描画し、
//! 選ばれた操作を `Cmd` として返す。実処理はすべて app.rs の `apply_cmd` が担う。
//! ショートカット表記は実際のキーバインド (config.toml の上書き込み) に追従する。

use crate::i18n::{tr, trf};
use crate::keybinds::{format_shortcut, BindAction, Keybinds};
use crate::palette::Cmd;
use crate::textenc::LineEnding;
use std::path::{Path, PathBuf};

/// 配色テーマ一覧の段。同梱テーマが増えても一覧が読めるように、
/// 明暗とカスタムを見出しで分ける (並べ替えはしない = `theme::all()` の順のまま)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeGroup {
    Dark,
    Light,
    /// ユーザー/プラグイン提供のテーマ JSON (明暗は読み込むまで判らない)
    Custom,
}

impl ThemeGroup {
    /// 段の見出し (翻訳前の原文)。
    fn heading(self) -> &'static str {
        match self {
            ThemeGroup::Dark => "ダーク",
            ThemeGroup::Light => "ライト",
            ThemeGroup::Custom => "カスタム (テーマJSON)",
        }
    }
}

/// メニューに 1 行として出る配色テーマ。
#[derive(Clone)]
pub struct ThemeEntry {
    /// `Cmd::SetTheme` にそのまま渡す値 (同梱テーマ名、またはテーマ JSON のパス)
    pub name: String,
    pub label: String,
    pub selected: bool,
    pub group: ThemeGroup,
}

/// 配色テーマの選択メニュー本体。トップバーの 🎨 と
/// メニューバーの「表示 > 配色テーマ」で **同じ実装**を使う
/// (2 か所で別々に描くと、片方だけ段組みが崩れる)。
pub fn theme_menu_ui(ui: &mut egui::Ui, themes: &[ThemeEntry], cmds: &mut Vec<Cmd>) {
    ui.set_min_width(240.0);
    egui::ScrollArea::vertical()
        .id_salt("zv-theme-menu")
        .max_height(420.0)
        .show(ui, |ui| {
            let mut shown: Option<ThemeGroup> = None;
            for t in themes {
                if shown != Some(t.group) {
                    if shown.is_some() {
                        ui.separator();
                    }
                    heading(ui, &tr(t.group.heading()));
                    shown = Some(t.group);
                }
                if ui.selectable_label(t.selected, &t.label).clicked() {
                    cmds.push(Cmd::SetTheme(t.name.clone()));
                    ui.close_menu();
                }
            }
        });
}

/// メニューの表示状態スナップショット。描画のためだけの読み取り専用情報。
pub struct MenuInfo {
    pub sidebar_open: bool,
    pub terminal_open: bool,
    pub cockpit_open: bool,
    pub kanban_open: bool,
    /// エージェントデッキ (縦 1 本のエージェント管理画面) を開いているか
    pub deck_open: bool,
    pub problems_open: bool,
    pub fullscreen: bool,
    pub auto_save: bool,
    /// エディタ本文の折り返し (表示メニューのチェック状態)
    pub word_wrap: bool,
    /// 空白文字の可視化 (表示メニューのチェック状態)
    pub show_whitespace: bool,
    /// ミニマップ (表示メニューのチェック状態)
    pub minimap: bool,
    /// ブレッドクラム (表示メニューのチェック状態)
    pub breadcrumbs: bool,
    /// ガターの git blame 表示 (表示メニューのチェック状態)
    pub git_blame: bool,
    /// アクティブなエディタタブがあるか (編集系メニューの有効/無効)
    pub has_editor: bool,
    /// エディタが分割されているか (分割の解除・ペイン移動の有効/無効)
    pub editor_split: bool,
    /// アクティブなタブがファイル (path 持ち) か
    pub has_file: bool,
    /// Markdown/HTML プレビュー対象タブか
    pub md_preview: bool,
    pub roots: Vec<PathBuf>,
    pub recent_folders: Vec<PathBuf>,
    pub recent_files: Vec<PathBuf>,
    /// (プラグイン index, コマンド index, アイコン, "プラグイン名: タイトル")
    pub plugin_commands: Vec<(usize, usize, String, String)>,
    /// (プリセット index, アイコン, 名前)
    pub agent_presets: Vec<(usize, String, String)>,
    /// 配色テーマの一覧 (同梱 + カスタム)。`ThemeEntry::group` で段に分かれる
    pub themes: Vec<ThemeEntry>,
    /// アクティブなタブの改行コード表示 (例 "CRLF")。タブが無ければ None
    pub line_ending: Option<String>,
    /// 画面全体のズーム倍率 (1.0 = 等倍)。表示メニューの「戻す」の有効/無効に使う
    pub ui_zoom: f32,
    /// アクティブなタブのズーム倍率。タブが無ければ None (ファイル単位の項目を落とす)
    pub file_zoom: Option<f32>,
    /// **文字サイズだけ**の倍率 (1.0 = 等倍)。「戻す」の有効/無効に使う
    pub text_scale: f32,
    /// 保存時に行末の空白を落とす (編集メニューのチェック状態)
    pub trim_trailing_on_save: bool,
    /// 保存時に末尾の余分な空行を落とす (同上)
    pub trim_final_newlines_on_save: bool,
    /// 保存時に最終行へ改行を入れる (同上)
    pub final_newline_on_save: bool,
    /// ビルドタスクのラベル。**⇧⌘B が実際に走らせる方**を入れる
    /// (tasks.json の既定ビルドがあればそれ、無ければ自動検出のラベル)。
    pub build_task: Option<String>,
    /// ⇧⌘B が tasks.json 由来のタスクを走らせるか。
    /// `true` の間は自動検出のタスクへは ⇧⌘B では届かない。
    pub build_from_tasks_json: bool,
    /// 自動検出したビルドタスクのラベル (Cargo.toml / package.json / Makefile / go.mod)。
    pub detected_task: Option<String>,
    /// `.vscode/tasks.json` 由来のタスク。(index, ラベル, 実行できない理由)。
    /// 理由が `Some` の行はグレーアウトし、ホバーで理由を出す
    /// (黙って壊れたコマンドを走らせない。黙って消しもしない)。
    pub json_tasks: Vec<(usize, String, Option<String>)>,
    /// tasks.json を解釈できなかった理由 (無ければ None)。
    pub tasks_error: Option<String>,
    /// アクティブファイルの実行コマンドラベル (例 "python3 main.py")
    pub run_label: Option<String>,
}

/// メニューバー本体。押された項目を `Cmd` のリストで返す。
pub fn ui(ui: &mut egui::Ui, info: &MenuInfo, keys: &Keybinds) -> Vec<Cmd> {
    let mut cmds: Vec<Cmd> = Vec::new();
    file_menu(ui, info, keys, &mut cmds);
    edit_menu(ui, info, keys, &mut cmds);
    selection_menu(ui, info, keys, &mut cmds);
    view_menu(ui, info, keys, &mut cmds);
    go_menu(ui, info, keys, &mut cmds);
    run_menu(ui, info, keys, &mut cmds);
    terminal_menu(ui, info, keys, &mut cmds);
    help_menu(ui, &mut cmds);
    cmds
}

/// ショートカット表記付きメニュー項目。クリックで Some(()) を返しメニューを閉じる。
fn item(ui: &mut egui::Ui, label: &str, shortcut: &str, enabled: bool) -> bool {
    let mut b = egui::Button::new(label);
    if !shortcut.is_empty() {
        b = b.shortcut_text(shortcut);
    }
    let clicked = ui.add_enabled(enabled, b).clicked();
    if clicked {
        ui.close_menu();
    }
    clicked
}

/// [`item`] にホバー説明を足したもの。**無効な行の理由を黙って捨てない**ために使う
/// (グレーアウトだけだと「なぜ押せないのか」がどこにも出ない)。
fn item_hint(ui: &mut egui::Ui, label: &str, enabled: bool, hint: &str) -> bool {
    let r = ui.add_enabled(enabled, egui::Button::new(label));
    let r = if hint.is_empty() {
        r
    } else if enabled {
        r.on_hover_text(hint)
    } else {
        r.on_disabled_hover_text(hint)
    };
    if r.clicked() {
        ui.close_menu();
        return true;
    }
    false
}

/// メニュー内の出典見出し (押せない小見出し)。
fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).weak().small());
}

/// キーバインド済みアクションのショートカット表記。
fn sc(keys: &Keybinds, a: BindAction) -> String {
    // chord (2 打鍵) も丸ごと出す。1 打鍵目だけ出すと嘘になる
    keys.label(a)
}

/// egui TextEdit が内蔵処理するキー (メニューには表記だけ出す)。
fn native_sc(spec: &str) -> String {
    crate::keybinds::parse_shortcut(spec)
        .map(format_shortcut)
        .unwrap_or_default()
}

fn file_menu(ui: &mut egui::Ui, info: &MenuInfo, keys: &Keybinds, cmds: &mut Vec<Cmd>) {
    ui.menu_button(tr("ファイル"), |ui| {
        ui.set_min_width(280.0);
        if item(
            ui,
            &tr("新しいテキスト ファイル"),
            &sc(keys, BindAction::NewFile),
            true,
        ) {
            cmds.push(Cmd::NewFile);
        }
        if item(
            ui,
            &tr("新しいウィンドウ"),
            &sc(keys, BindAction::NewWindow),
            true,
        ) {
            cmds.push(Cmd::NewWindow);
        }
        ui.separator();
        if item(
            ui,
            &tr("ファイルを開く…"),
            &sc(keys, BindAction::OpenFile),
            true,
        ) {
            cmds.push(Cmd::OpenFileDialog);
        }
        if item(ui, &tr("フォルダーを開く…"), "", true) {
            cmds.push(Cmd::OpenFolder);
        }
        if item(ui, &tr("新しいウィンドウでフォルダーを開く…"), "", true) {
            cmds.push(Cmd::NewWindowFolder);
        }
        ui.menu_button(tr("最近使用した項目を開く"), |ui| {
            ui.set_min_width(320.0);
            if info.recent_folders.is_empty() && info.recent_files.is_empty() {
                ui.label(tr("まだありません"));
            }
            for p in &info.recent_folders {
                if item(ui, &format!("📂 {}", display_path(p)), "", true) {
                    cmds.push(Cmd::OpenRecentFolder(p.clone()));
                }
            }
            if !info.recent_folders.is_empty() && !info.recent_files.is_empty() {
                ui.separator();
            }
            for p in &info.recent_files {
                if item(ui, &format!("📄 {}", display_path(p)), "", true) {
                    cmds.push(Cmd::OpenRecentFile(p.clone()));
                }
            }
            ui.separator();
            if item(ui, &tr("最近使用した項目をクリア"), "", true) {
                cmds.push(Cmd::ClearRecent);
            }
        });
        ui.separator();
        if item(ui, &tr("フォルダーをワークスペースに追加…"), "", true) {
            cmds.push(Cmd::AddFolder);
        }
        if info.roots.len() > 1 {
            ui.menu_button(
                tr("フォルダーをワークスペースから削除"),
                |ui| {
                    ui.set_min_width(280.0);
                    for r in &info.roots {
                        if item(ui, &display_path(r), "", true) {
                            cmds.push(Cmd::RemoveFolder(r.clone()));
                        }
                    }
                },
            );
        }
        ui.separator();
        if item(
            ui,
            &tr("保存"),
            &sc(keys, BindAction::Save),
            info.has_editor,
        ) {
            cmds.push(Cmd::Save);
        }
        if item(
            ui,
            &tr("名前を付けて保存…"),
            &sc(keys, BindAction::SaveAs),
            info.has_editor,
        ) {
            cmds.push(Cmd::SaveAs);
        }
        if item(
            ui,
            &tr("すべて保存"),
            &sc(keys, BindAction::SaveAll),
            info.has_editor,
        ) {
            cmds.push(Cmd::SaveAll);
        }
        let mut auto = info.auto_save;
        if ui.checkbox(&mut auto, tr("自動保存")).clicked() {
            cmds.push(Cmd::ToggleAutoSave);
            ui.close_menu();
        }
        ui.separator();
        if item(ui, &tr("ファイルを元に戻す"), "", info.has_file) {
            cmds.push(Cmd::RevertFile);
        }
        ui.separator();
        ui.menu_button(tr("ユーザー設定"), |ui| {
            ui.set_min_width(280.0);
            if item(ui, &tr("設定"), "", true) {
                cmds.push(Cmd::OpenSettings);
            }
            if item(ui, &tr("設定 config.toml を開く"), "", true) {
                cmds.push(Cmd::OpenConfig);
            }
            if item(ui, &tr("設定を再読み込み"), "", true) {
                cmds.push(Cmd::ReloadConfig);
            }
            // メニューからの到達はここ 1 か所だけ (VS Code と同じ「設定」の下)。
            // ヘルプにも同じ項目があったが、同じ操作への入口を 2 つ持たない。
            if item(
                ui,
                &tr("キーボード ショートカット"),
                &sc(keys, BindAction::KeybindEditor),
                true,
            ) {
                cmds.push(Cmd::ShowShortcuts);
            }
        });
        ui.separator();
        if item(
            ui,
            &tr("エディターを閉じる"),
            &sc(keys, BindAction::CloseTab),
            info.has_editor,
        ) {
            cmds.push(Cmd::CloseTab);
        }
        if item(ui, &tr("すべてのエディターを閉じる"), "", info.has_editor) {
            cmds.push(Cmd::CloseAllTabs);
        }
    });
}

fn edit_menu(ui: &mut egui::Ui, info: &MenuInfo, keys: &Keybinds, cmds: &mut Vec<Cmd>) {
    let ed = info.has_editor;
    ui.menu_button(tr("編集"), |ui| {
        ui.set_min_width(280.0);
        // 取り消しは自前の履歴なので、割り当ては config.toml で変えられる。
        // ここも必ず現在のバインドから表記を作る (ベタ書きは嘘になる)。
        if item(ui, &tr("元に戻す"), &sc(keys, BindAction::Undo), ed) {
            cmds.push(Cmd::Undo);
        }
        if item(ui, &tr("やり直し"), &sc(keys, BindAction::Redo), ed) {
            cmds.push(Cmd::Redo);
        }
        ui.separator();
        if item(ui, &tr("切り取り"), &native_sc("cmd+x"), ed) {
            cmds.push(Cmd::CutSelection);
        }
        if item(ui, &tr("コピー"), &native_sc("cmd+c"), ed) {
            cmds.push(Cmd::CopySelection);
        }
        if item(ui, &tr("貼り付け"), &native_sc("cmd+v"), ed) {
            cmds.push(Cmd::PasteClipboard);
        }
        ui.separator();
        if item(ui, &tr("検索"), &sc(keys, BindAction::Find), ed) {
            cmds.push(Cmd::OpenFind);
        }
        if item(ui, &tr("置換"), &sc(keys, BindAction::OpenReplace), ed) {
            cmds.push(Cmd::OpenReplace);
        }
        ui.separator();
        if item(
            ui,
            &tr("ファイル間で検索"),
            &sc(keys, BindAction::GlobalSearch),
            true,
        ) {
            cmds.push(Cmd::GlobalSearch);
        }
        if item(
            ui,
            &tr("ファイル間で置換"),
            &sc(keys, BindAction::GlobalReplace),
            true,
        ) {
            cmds.push(Cmd::GlobalReplace);
        }
        ui.separator();
        if item(
            ui,
            &tr("行コメントの切り替え"),
            &sc(keys, BindAction::ToggleComment),
            ed,
        ) {
            cmds.push(Cmd::ToggleLineComment);
        }
        ui.separator();
        // 改行コード: いまの様式をラベルに出し、押した先へ揃える
        ui.menu_button(
            trf(
                "改行コードを変換 (現在: {le})",
                &[("le", info.line_ending.clone().unwrap_or_else(|| tr("なし")))],
            ),
            |ui| {
                ui.set_min_width(240.0);
                for (le, label) in [
                    (LineEnding::Lf, "LF (Unix)"),
                    (LineEnding::Crlf, "CRLF (Windows)"),
                    (LineEnding::Cr, "CR (旧 Mac)"),
                ] {
                    if item(ui, &tr(label), "", ed) {
                        cmds.push(Cmd::ConvertLineEnding(le));
                        ui.close_menu();
                    }
                }
            },
        );
        ui.separator();
        // 保存時のクリーンアップ。既定は config.toml
        // (trim_trailing_whitespace / trim_final_newlines / insert_final_newline)
        // で、ここでの切替はセッション中の上書き。
        let trim = if info.trim_trailing_on_save {
            tr("✓ 保存時に末尾空白を除去")
        } else {
            tr("保存時に末尾空白を除去")
        };
        if item(ui, &trim, "", true) {
            cmds.push(Cmd::ToggleTrimTrailingOnSave);
        }
        let tfn = if info.trim_final_newlines_on_save {
            tr("✓ 保存時に末尾の余分な空行を落とす")
        } else {
            tr("保存時に末尾の余分な空行を落とす")
        };
        if item(ui, &tfn, "", true) {
            cmds.push(Cmd::ToggleTrimFinalNewlinesOnSave);
        }
        let fnl = if info.final_newline_on_save {
            tr("✓ 保存時に最終行へ改行を入れる")
        } else {
            tr("保存時に最終行へ改行を入れる")
        };
        if item(ui, &fnl, "", true) {
            cmds.push(Cmd::ToggleFinalNewlineOnSave);
        }
    });
}

fn selection_menu(ui: &mut egui::Ui, info: &MenuInfo, keys: &Keybinds, cmds: &mut Vec<Cmd>) {
    let ed = info.has_editor;
    ui.menu_button(tr("選択"), |ui| {
        ui.set_min_width(280.0);
        if item(ui, &tr("すべて選択"), &native_sc("cmd+a"), ed) {
            cmds.push(Cmd::SelectAll);
        }
        ui.separator();
        if item(
            ui,
            &tr("行を複製"),
            &sc(keys, BindAction::DuplicateLine),
            ed,
        ) {
            cmds.push(Cmd::DuplicateLine);
        }
        if item(
            ui,
            &tr("行を上へ移動"),
            &sc(keys, BindAction::MoveLineUp),
            ed,
        ) {
            cmds.push(Cmd::MoveLineUp);
        }
        if item(
            ui,
            &tr("行を下へ移動"),
            &sc(keys, BindAction::MoveLineDown),
            ed,
        ) {
            cmds.push(Cmd::MoveLineDown);
        }
    });
}

fn view_menu(ui: &mut egui::Ui, info: &MenuInfo, keys: &Keybinds, cmds: &mut Vec<Cmd>) {
    let ed = info.has_editor;
    ui.menu_button(tr("表示"), |ui| {
        ui.set_min_width(300.0);
        if item(
            ui,
            &tr("コマンド パレット…"),
            &sc(keys, BindAction::PaletteCommands),
            true,
        ) {
            cmds.push(Cmd::OpenCommandPalette);
        }
        ui.separator();
        ui.menu_button(tr("外観"), |ui| {
            ui.set_min_width(300.0);
            let full = if info.fullscreen {
                tr("✓ フルスクリーン")
            } else {
                tr("フルスクリーン")
            };
            if item(ui, &full, &sc(keys, BindAction::ToggleFullScreen), true) {
                cmds.push(Cmd::ToggleFullScreen);
            }
            ui.separator();
            let side = if info.sidebar_open {
                tr("✓ サイドバー")
            } else {
                tr("サイドバー")
            };
            if item(ui, &side, &sc(keys, BindAction::ToggleSidebar), true) {
                cmds.push(Cmd::ToggleSidebar);
            }
            let term = if info.terminal_open {
                tr("✓ パネル (ターミナル)")
            } else {
                tr("パネル (ターミナル)")
            };
            if item(ui, &term, &sc(keys, BindAction::ToggleTerminal), true) {
                cmds.push(Cmd::ToggleTerminal);
            }
            let cp = if info.cockpit_open {
                tr("✓ Cockpit")
            } else {
                "Cockpit".to_string()
            };
            if item(ui, &cp, &sc(keys, BindAction::ToggleCockpit), true) {
                cmds.push(Cmd::ToggleCockpit);
            }
            let kb = if info.kanban_open {
                tr("✓ フリート看板")
            } else {
                tr("フリート看板")
            };
            if item(ui, &kb, &sc(keys, BindAction::ToggleKanban), true) {
                cmds.push(Cmd::ToggleKanban);
            }
            let dk = if info.deck_open {
                tr("✓ エージェントデッキ")
            } else {
                tr("エージェントデッキ")
            };
            if item(ui, &dk, &sc(keys, BindAction::ToggleDeck), true) {
                cmds.push(Cmd::ToggleDeck);
            }
            ui.separator();
            ui.menu_button(tr("配色テーマ"), |ui| {
                theme_menu_ui(ui, &info.themes, cmds);
            });
            ui.separator();
            // ズームは二階建て。「画面全体」を先に置き、「このファイルだけ」を
            // その下に置くことで、対象の広い順に読める並びにする。
            if item(ui, &tr("ズームイン"), &sc(keys, BindAction::ZoomIn), true) {
                cmds.push(Cmd::ZoomIn);
            }
            if item(
                ui,
                &tr("ズームアウト"),
                &sc(keys, BindAction::ZoomOut),
                true,
            ) {
                cmds.push(Cmd::ZoomOut);
            }
            // 等倍のときは「戻す」を押せなくする (押しても何も起きない項目を
            // 生かしておくと、効かないのか壊れているのか区別が付かない)。
            if item(
                ui,
                &trf(
                    "ズームを戻す ({pct})",
                    &[("pct", crate::zoom::label(info.ui_zoom))],
                ),
                &sc(keys, BindAction::ZoomReset),
                !crate::zoom::is_default(info.ui_zoom),
            ) {
                cmds.push(Cmd::ZoomReset);
            }
            ui.separator();
            // ファイル単位のズームはタブが開いているときだけ意味を持つ。
            let has_file = info.file_zoom.is_some();
            if item(
                ui,
                &tr("このファイルだけズームイン"),
                &sc(keys, BindAction::FileZoomIn),
                has_file,
            ) {
                cmds.push(Cmd::FileZoomIn);
            }
            if item(
                ui,
                &tr("このファイルだけズームアウト"),
                &sc(keys, BindAction::FileZoomOut),
                has_file,
            ) {
                cmds.push(Cmd::FileZoomOut);
            }
            let file_z = info.file_zoom.unwrap_or(crate::zoom::DEFAULT);
            if item(
                ui,
                &trf(
                    "このファイルのズームを解除 ({pct})",
                    &[("pct", crate::zoom::label(file_z))],
                ),
                &sc(keys, BindAction::FileZoomReset),
                has_file && !crate::zoom::is_default(file_z),
            ) {
                cmds.push(Cmd::FileZoomReset);
            }
            ui.separator();
            // ── 文字サイズだけ (ズームとは別物) ──
            // ズームは余白・ボタン・パネル幅まで大きくするので画面に入る情報が
            // 減る。こちらはレイアウトを変えずに文字だけ掛け直すので、
            // 「窓は広いまま、字だけ読みやすく」ができる。
            // 既定のキーバインドは持たない — ⌘⇧+ は US 配列だと ⌘+ と同じ打鍵に
            // なり、画面全体のズームを壊すため (必要な人は config.toml で割り当てる)。
            if item(ui, &tr("文字サイズを大きく"), "", true) {
                cmds.push(Cmd::TextSizeIn);
            }
            if item(ui, &tr("文字サイズを小さく"), "", true) {
                cmds.push(Cmd::TextSizeOut);
            }
            if item(
                ui,
                &trf(
                    "文字サイズを戻す ({pct})",
                    &[("pct", crate::zoom::label(info.text_scale))],
                ),
                "",
                !crate::zoom::is_default(info.text_scale),
            ) {
                cmds.push(Cmd::TextSizeReset);
            }
        });
        ui.separator();
        // ── エディタの分割 (VS Code の editor group 相当) ──
        // 分割していない間は「解除」「次のペインへ」を押せなくする
        // (押しても何も起きない項目を出さない)。
        ui.menu_button(tr("エディタの分割"), |ui| {
            ui.set_min_width(300.0);
            if item(
                ui,
                &tr("右に分割"),
                &sc(keys, BindAction::SplitEditorRight),
                ed,
            ) {
                cmds.push(Cmd::SplitEditorRight);
            }
            if item(
                ui,
                &tr("下に分割"),
                &sc(keys, BindAction::SplitEditorDown),
                ed,
            ) {
                cmds.push(Cmd::SplitEditorDown);
            }
            ui.separator();
            if item(
                ui,
                &tr("次のペインへ"),
                &sc(keys, BindAction::FocusPane2),
                info.editor_split,
            ) {
                cmds.push(Cmd::FocusNextPane);
            }
            if item(ui, &tr("タブを次のペインへ移動"), "", ed) {
                cmds.push(Cmd::MoveTabToNextPane);
            }
            ui.separator();
            if item(ui, &tr("分割を解除"), "", info.editor_split) {
                cmds.push(Cmd::UnsplitEditor);
            }
        });
        ui.separator();
        if item(
            ui,
            &tr("エクスプローラー"),
            &sc(keys, BindAction::FocusExplorer),
            true,
        ) {
            cmds.push(Cmd::ShowExplorer);
        }
        if item(ui, &tr("検索"), &sc(keys, BindAction::GlobalSearch), true) {
            cmds.push(Cmd::GlobalSearch);
        }
        if item(ui, &tr("セッション (過去の会話)"), "", true) {
            cmds.push(Cmd::ShowSessions);
        }
        if item(ui, &tr("ソース管理"), "", true) {
            cmds.push(Cmd::OpenGitPanel);
        }
        if item(ui, "GitHub", "", true) {
            cmds.push(Cmd::ShowGitHubTab);
        }
        if item(ui, &tr("拡張機能 (プラグイン)"), "", true) {
            cmds.push(Cmd::ShowPlugins);
        }
        ui.separator();
        if item(ui, &tr("プラン使用量"), "", true) {
            cmds.push(Cmd::ShowQuota);
        }
        ui.separator();
        let prob = if info.problems_open {
            tr("✓ 問題")
        } else {
            tr("問題")
        };
        if item(ui, &prob, &sc(keys, BindAction::ToggleProblems), true) {
            cmds.push(Cmd::ToggleProblems);
        }
        let term = if info.terminal_open {
            tr("✓ ターミナル")
        } else {
            tr("ターミナル")
        };
        if item(ui, &term, &sc(keys, BindAction::ToggleTerminal), true) {
            cmds.push(Cmd::ToggleTerminal);
        }
        ui.separator();
        // エディタの表示オプション (VS Code: 表示 > 折り返しの切り替え 相当)
        let ww = if info.word_wrap {
            tr("✓ 折り返し")
        } else {
            tr("折り返し")
        };
        if item(ui, &ww, "", true) {
            cmds.push(Cmd::ToggleWordWrap);
        }
        let ws = if info.show_whitespace {
            tr("✓ 空白文字を表示")
        } else {
            tr("空白文字を表示")
        };
        if item(ui, &ws, "", true) {
            cmds.push(Cmd::ToggleShowWhitespace);
        }
        // ミニマップ / ブレッドクラム (VS Code: 表示 > 外観)
        let mm = if info.minimap {
            tr("✓ ミニマップ")
        } else {
            tr("ミニマップ")
        };
        if item(ui, &mm, "", true) {
            cmds.push(Cmd::ToggleMinimap);
        }
        let bc = if info.breadcrumbs {
            tr("✓ ブレッドクラム")
        } else {
            tr("ブレッドクラム")
        };
        if item(ui, &bc, "", true) {
            cmds.push(Cmd::ToggleBreadcrumbs);
        }
        let gb = if info.git_blame {
            tr("✓ Git blame をガターに表示")
        } else {
            tr("Git blame をガターに表示")
        };
        if item(ui, &gb, "", true) {
            cmds.push(Cmd::ToggleGitBlame);
        }
        ui.separator();
        let md = if info.md_preview {
            tr("✓ Markdown/HTML プレビュー")
        } else {
            tr("Markdown/HTML プレビュー")
        };
        if item(
            ui,
            &md,
            &sc(keys, BindAction::ToggleMdPreview),
            info.has_editor,
        ) {
            cmds.push(Cmd::ToggleMdPreview);
        }
        ui.separator();
        // 折りたたみ (VS Code: 表示 > 折りたたみ)。段数指定は
        // 2 打鍵のコードが要るのでメニューとパレット専用にしてある。
        ui.menu_button(tr("折りたたみ"), |ui| {
            ui.set_min_width(280.0);
            if item(
                ui,
                &tr("折りたたみ切替"),
                &sc(keys, BindAction::ToggleFold),
                ed,
            ) {
                cmds.push(Cmd::ToggleFold);
            }
            if item(ui, &tr("すべて折りたたむ"), "", ed) {
                cmds.push(Cmd::FoldAll);
            }
            if item(
                ui,
                &tr("すべて展開する"),
                &sc(keys, BindAction::UnfoldAll),
                ed,
            ) {
                cmds.push(Cmd::UnfoldAll);
            }
            ui.separator();
            for n in 1..=3usize {
                let label = trf("レベル {n} で折りたたむ", &[("n", n.to_string())]);
                if item(ui, &label, "", ed) {
                    cmds.push(Cmd::FoldLevel(n));
                }
            }
        });
        if item(ui, &tr("テーブル表示の切替 (CSV / TSV)"), "", ed) {
            cmds.push(Cmd::ToggleTableView);
        }
    });
}

fn go_menu(ui: &mut egui::Ui, info: &MenuInfo, keys: &Keybinds, cmds: &mut Vec<Cmd>) {
    let ed = info.has_editor;
    ui.menu_button(tr("移動"), |ui| {
        ui.set_min_width(300.0);
        if item(ui, &tr("戻る"), &sc(keys, BindAction::NavBack), true) {
            cmds.push(Cmd::NavBack);
        }
        if item(ui, &tr("進む"), &sc(keys, BindAction::NavForward), true) {
            cmds.push(Cmd::NavForward);
        }
        ui.separator();
        if item(
            ui,
            &tr("ファイルへ移動…"),
            &sc(keys, BindAction::PaletteFiles),
            true,
        ) {
            cmds.push(Cmd::OpenFilePalette);
        }
        ui.separator();
        if item(
            ui,
            &tr("次のエディター"),
            &sc(keys, BindAction::NextTab),
            ed,
        ) {
            cmds.push(Cmd::NextTab);
        }
        if item(
            ui,
            &tr("前のエディター"),
            &sc(keys, BindAction::PrevTab),
            ed,
        ) {
            cmds.push(Cmd::PrevTab);
        }
        ui.separator();
        if item(
            ui,
            &tr("定義へ移動"),
            &sc(keys, BindAction::GoToDefinition),
            info.has_file,
        ) {
            cmds.push(Cmd::GoToDefinition);
        }
        if item(
            ui,
            &tr("ブラケットへ移動"),
            &sc(keys, BindAction::GoToBracket),
            ed,
        ) {
            cmds.push(Cmd::GoToBracket);
        }
        if item(
            ui,
            &tr("シンボルにジャンプ"),
            &sc(keys, BindAction::LspSymbols),
            info.has_file,
        ) {
            cmds.push(Cmd::LspSymbols);
        }
        if item(
            ui,
            &tr("参照を検索"),
            &sc(keys, BindAction::LspReferences),
            info.has_file,
        ) {
            cmds.push(Cmd::LspReferences);
        }
        ui.separator();
        if item(
            ui,
            &tr("ブックマーク切替"),
            &sc(keys, BindAction::ToggleBookmark),
            ed,
        ) {
            cmds.push(Cmd::ToggleBookmark);
        }
        if item(ui, &tr("次のブックマークへ"), "", ed) {
            cmds.push(Cmd::NextBookmark);
        }
        if item(ui, &tr("前のブックマークへ"), "", ed) {
            cmds.push(Cmd::PrevBookmark);
        }
        if item(ui, &tr("ブックマークをすべて解除"), "", ed) {
            cmds.push(Cmd::ClearBookmarks);
        }
        ui.separator();
        if item(ui, &tr("行/列へ移動…"), &sc(keys, BindAction::GoToLine), ed) {
            cmds.push(Cmd::GoToLine);
        }
    });
}

/// 「タスクの実行…」に出せる中身があるか。
/// 空なら呼び出し側がサブメニューごと出さない (中身の無いセクションを作らない)。
fn has_tasks(info: &MenuInfo) -> bool {
    !info.json_tasks.is_empty()
        || info.tasks_error.is_some()
        || info.detected_task.is_some()
        || !info.plugin_commands.is_empty()
}

/// 「タスクの実行…」サブメニュー。**出典ごとに見出しで分ける** —
/// `.vscode/tasks.json` / 自動検出 (Cargo.toml 等) / プラグインは
/// 壊れ方も直し方も違うので、どこから来た行なのかが見えないと直せない。
///
/// 実行は全て既存の経路 (`Cmd::RunJsonTask` / `Cmd::RunBuildTask` /
/// `Cmd::RunPlugin`) に流す。ここに新しい起動の仕組みは持たない。
fn tasks_submenu(ui: &mut egui::Ui, info: &MenuInfo, cmds: &mut Vec<Cmd>) {
    ui.menu_button(tr("タスクの実行…"), |ui| {
        ui.set_min_width(320.0);
        let mut shown = false;
        if !info.json_tasks.is_empty() || info.tasks_error.is_some() {
            heading(ui, "tasks.json");
            shown = true;
            // パースエラーは黙って消さない。理由はホバーで全文が読める。
            if let Some(e) = &info.tasks_error {
                item_hint(ui, &tr("⚠ tasks.json を読めませんでした"), false, e);
            }
            for (i, label, blocked) in &info.json_tasks {
                let hint = blocked.clone().unwrap_or_default();
                if item_hint(ui, &format!("▶ {label}"), blocked.is_none(), &hint) {
                    cmds.push(Cmd::RunJsonTask(*i));
                }
            }
        }
        if let Some(d) = &info.detected_task {
            if shown {
                ui.separator();
            }
            shown = true;
            heading(ui, &tr("自動検出"));
            // tasks.json の既定ビルドがあるときは ⇧⌘B はそちらへ行く。
            // ここを押せるままにすると「押した物と走る物が違う」ので落とす。
            let hint = if info.build_from_tasks_json {
                tr("tasks.json の既定ビルドタスクが優先されます")
            } else {
                String::new()
            };
            if item_hint(ui, &format!("🔨 {d}"), !info.build_from_tasks_json, &hint) {
                cmds.push(Cmd::RunBuildTask);
            }
        }
        if !info.plugin_commands.is_empty() {
            if shown {
                ui.separator();
            }
            heading(ui, &tr("プラグイン"));
            for (pi, ci, icon, title) in &info.plugin_commands {
                if item(ui, &format!("{icon} {title}"), "", true) {
                    cmds.push(Cmd::RunPlugin(*pi, *ci));
                }
            }
        }
    });
}

fn run_menu(ui: &mut egui::Ui, info: &MenuInfo, keys: &Keybinds, cmds: &mut Vec<Cmd>) {
    ui.menu_button(tr("実行"), |ui| {
        ui.set_min_width(300.0);
        let run_label = match &info.run_label {
            Some(l) => format!("▶ {l}"),
            None => tr("アクティブなファイルを実行"),
        };
        if item(ui, &run_label, "", info.run_label.is_some()) {
            cmds.push(Cmd::RunActiveFile);
        }
        ui.separator();
        let build_label = match &info.build_task {
            Some(l) => format!("🔨 {l}"),
            None => tr("ビルド タスクの実行…"),
        };
        if item(
            ui,
            &build_label,
            &sc(keys, BindAction::RunBuildTask),
            info.build_task.is_some(),
        ) {
            cmds.push(Cmd::RunBuildTask);
        }
        if has_tasks(info) {
            tasks_submenu(ui, info, cmds);
        }
        ui.separator();
        ui.menu_button(tr("エージェントを起動"), |ui| {
            ui.set_min_width(260.0);
            for (i, icon, name) in &info.agent_presets {
                if item(ui, &format!("{icon} {name}"), "", true) {
                    cmds.push(Cmd::NewAgent(*i));
                }
            }
            ui.separator();
            if item(ui, &tr("➕ エージェントを追加…"), "", true) {
                cmds.push(Cmd::OpenAgentPicker);
            }
        });
    });
}

fn terminal_menu(ui: &mut egui::Ui, info: &MenuInfo, keys: &Keybinds, cmds: &mut Vec<Cmd>) {
    ui.menu_button(tr("ターミナル"), |ui| {
        ui.set_min_width(300.0);
        if item(
            ui,
            &tr("新しいターミナル"),
            &sc(keys, BindAction::NewTerminal),
            true,
        ) {
            cmds.push(Cmd::NewTerminal);
        }
        ui.separator();
        let term = if info.terminal_open {
            tr("✓ ターミナル パネル")
        } else {
            tr("ターミナル パネル")
        };
        if item(ui, &term, &sc(keys, BindAction::ToggleTerminal), true) {
            cmds.push(Cmd::ToggleTerminal);
        }
        ui.separator();
        let run_label = match &info.run_label {
            Some(l) => format!("▶ {l}"),
            None => tr("アクティブなファイルを実行"),
        };
        if item(ui, &run_label, "", info.run_label.is_some()) {
            cmds.push(Cmd::RunActiveFile);
        }
        if item(
            ui,
            &tr("選択したテキストをターミナルへ送る"),
            "",
            info.has_editor,
        ) {
            cmds.push(Cmd::RunSelection);
        }
        let build_label = match &info.build_task {
            Some(l) => format!("🔨 {l}"),
            None => tr("ビルド タスクの実行…"),
        };
        if item(
            ui,
            &build_label,
            &sc(keys, BindAction::RunBuildTask),
            info.build_task.is_some(),
        ) {
            cmds.push(Cmd::RunBuildTask);
        }
        if has_tasks(info) {
            ui.separator();
            tasks_submenu(ui, info, cmds);
        }
    });
}

fn help_menu(ui: &mut egui::Ui, cmds: &mut Vec<Cmd>) {
    ui.menu_button(tr("ヘルプ"), |ui| {
        ui.set_min_width(300.0);
        // 初回起動ガイドツアー。「もう一度見たい」を探す場所はここ以外に無い。
        if item(ui, &tr("チュートリアルを再開"), "", true) {
            cmds.push(Cmd::RestartTutorial);
        }
        if item(
            ui,
            &tr("コマンド パレットですべてのコマンドを表示"),
            "",
            true,
        ) {
            cmds.push(Cmd::OpenCommandPalette);
        }
        ui.separator();
        if item(ui, &tr("バージョン情報"), "", true) {
            cmds.push(Cmd::ShowAbout);
        }
    });
}

/// メニュー表示用のパス短縮 (ホームは ~、ファイル/フォルダ名を強調しない素の表記)。
fn display_path(p: &Path) -> String {
    let s = p.display().to_string();
    if let Some(home) = dirs::home_dir() {
        let h = home.display().to_string();
        if let Some(rest) = s.strip_prefix(&h) {
            return format!("~{rest}");
        }
    }
    s
}

// ─── 実行コマンドの推定 (Run メニュー) ─────────────────────────────

/// シングルクォートで安全に囲む (' は '\'' に)。
fn shq(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "'\\''"))
}

/// アクティブなファイルを実行するシェルコマンドを拡張子から推定する。
/// 対応しない拡張子は None (メニュー項目がグレーアウトする)。
pub fn runner_for(path: &Path, root: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let q = shq(path);
    Some(match ext.as_str() {
        "rs" => {
            if root.join("Cargo.toml").is_file() {
                "cargo run".to_string()
            } else {
                return None;
            }
        }
        "py" => format!("python3 {q}"),
        "js" | "mjs" | "cjs" => format!("node {q}"),
        "ts" | "mts" => format!("npx tsx {q}"),
        "sh" | "bash" => format!("bash {q}"),
        "zsh" => format!("zsh {q}"),
        "rb" => format!("ruby {q}"),
        "go" => format!("go run {q}"),
        "php" => format!("php {q}"),
        "pl" => format!("perl {q}"),
        "lua" => format!("lua {q}"),
        "swift" => format!("swift {q}"),
        _ => return None,
    })
}

/// 「タスクの実行…」と コマンドパレットに出す tasks.json の行数の上限。
/// これを超える定義はメニューを縦に破壊するだけなので出さない。
pub const MAX_TASK_ROWS: usize = 40;

/// `.vscode/tasks.json` の走査結果を「タスクの実行…」の行へ落とす純関数。
///
/// 走らせられないタスクを**黙って消さない** — 行は出したまま理由を持たせ、
/// 描画側がグレーアウトとホバーに使う。理由が `None` の行だけが実行できる。
///
/// OS 差分は `cfg!` ではなく引数で選ぶ (そうしないと片側しかテストできない)。
pub fn task_rows(
    doc: &crate::tasks::TasksDoc,
    file: Option<&Path>,
    windows: bool,
) -> Vec<(usize, String, Option<String>)> {
    doc.tasks
        .iter()
        .enumerate()
        .take(MAX_TASK_ROWS)
        .map(|(i, t)| {
            (
                i,
                t.label.clone(),
                crate::tasks::resolve(t, file, windows).err(),
            )
        })
        .collect()
}

/// ワークスペースのビルドタスクを検出する。(ラベル, コマンド)
pub fn build_task_for(root: &Path) -> Option<(String, String)> {
    if root.join("Cargo.toml").is_file() {
        return Some(("cargo build".into(), "cargo build".into()));
    }
    if root.join("package.json").is_file() {
        return Some(("npm run build".into(), "npm run build".into()));
    }
    if root.join("Makefile").is_file() || root.join("makefile").is_file() {
        return Some(("make".into(), "make".into()));
    }
    if root.join("go.mod").is_file() {
        return Some(("go build ./...".into(), "go build ./...".into()));
    }
    None
}

/// OS のクリップボードからテキストを読む (メニューの「貼り付け」用)。
/// egui はクリップボード読み出し API を持たないため、OS コマンドへシェルアウトする。
pub fn clipboard_text() -> Option<String> {
    #[cfg(target_os = "macos")]
    let out = crate::procx::hidden_command("pbpaste").output().ok()?;
    // procx: powershell のコンソール窓を貼り付けのたびに点滅させない
    #[cfg(target_os = "windows")]
    // 出力を UTF-8 に固定してから読む。コンソールのコードページ任せだと、
    // そのコードページに無い文字 (CP932 での絵文字・ハングル等) が PowerShell 側で
    // 「?」に潰されてしまい、こちらでどう復号しても元に戻せない。
    let out = crate::procx::hidden_command("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("{}Get-Clipboard -Raw", crate::textenc::PS_UTF8_PRELUDE),
        ])
        .output()
        .ok()?;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let out = crate::procx::hidden_command("sh")
        .args([
            "-c",
            "command -v wl-paste >/dev/null && wl-paste --no-newline || xclip -selection clipboard -o",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // Windows の Get-Clipboard はコンソールのコードページで返る。UTF-8 として
    // 読むと日本語をコピペした瞬間に化けるので textenc へ通す。
    let s = crate::textenc::decode_output(&out.stdout);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn runner_for_known_extensions() {
        let root = PathBuf::from("/nonexistent-root");
        assert_eq!(
            runner_for(Path::new("/a/b/main.py"), &root),
            Some("python3 '/a/b/main.py'".into())
        );
        assert_eq!(
            runner_for(Path::new("/a/app.js"), &root),
            Some("node '/a/app.js'".into())
        );
        assert_eq!(
            runner_for(Path::new("/a/run.sh"), &root),
            Some("bash '/a/run.sh'".into())
        );
        assert_eq!(
            runner_for(Path::new("/a/tool.go"), &root),
            Some("go run '/a/tool.go'".into())
        );
    }

    #[test]
    fn runner_for_rust_requires_cargo_project() {
        // Cargo.toml が無いルートでは .rs は実行できない
        assert_eq!(
            runner_for(Path::new("/a/main.rs"), Path::new("/nonexistent")),
            None
        );
    }

    #[test]
    fn runner_for_unknown_is_none() {
        let root = PathBuf::from("/nonexistent-root");
        assert_eq!(runner_for(Path::new("/a/b.txt"), &root), None);
        assert_eq!(runner_for(Path::new("/a/noext"), &root), None);
    }

    #[test]
    fn runner_quotes_paths_with_spaces_and_quotes() {
        let root = PathBuf::from("/nonexistent-root");
        assert_eq!(
            runner_for(Path::new("/a dir/o'brien.py"), &root),
            Some("python3 '/a dir/o'\\''brien.py'".into())
        );
    }

    #[test]
    fn build_task_detects_cargo() {
        let dir = std::env::temp_dir().join(format!("zv-menubar-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(
            build_task_for(&dir),
            Some(("cargo build".into(), "cargo build".into()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_task_none_for_plain_dir() {
        let dir = std::env::temp_dir().join(format!("zv-menubar-none-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(build_task_for(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── shq (シェルクォート。安全性に関わるため網羅的に固定) ───

    #[test]
    fn shq_wraps_plain_path_in_single_quotes() {
        assert_eq!(shq(Path::new("/a/b.py")), "'/a/b.py'");
    }

    #[test]
    fn shq_preserves_spaces_inside_quotes() {
        assert_eq!(
            shq(Path::new("/my dir/file name.txt")),
            "'/my dir/file name.txt'"
        );
    }

    #[test]
    fn shq_escapes_single_quote() {
        // ' は '\'' に展開される (クォート終了 → エスケープ ' → クォート再開)
        assert_eq!(shq(Path::new("/a/o'brien.py")), "'/a/o'\\''brien.py'");
    }

    #[test]
    fn shq_escapes_multiple_single_quotes() {
        assert_eq!(shq(Path::new("/a/'quoted'")), "'/a/'\\''quoted'\\'''");
    }

    #[test]
    fn shq_keeps_shell_metacharacters_literal() {
        // シングルクォート内では " $ ` \ はシェルに解釈されないのでそのまま
        assert_eq!(
            shq(Path::new("/a/he said \"hi\"/$HOME/`whoami`\\x.txt")),
            "'/a/he said \"hi\"/$HOME/`whoami`\\x.txt'"
        );
    }

    #[test]
    fn shq_empty_path_is_empty_quotes() {
        assert_eq!(shq(Path::new("")), "''");
    }

    #[test]
    fn shq_japanese_path() {
        assert_eq!(
            shq(Path::new("/ユーザ/山田 太郎/日本語 ファイル.txt")),
            "'/ユーザ/山田 太郎/日本語 ファイル.txt'"
        );
    }

    // ─── display_path (メニュー表示用のホーム短縮) ───

    #[test]
    fn display_path_shortens_home_prefix() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                display_path(&home.join("proj").join("main.rs")),
                format!(
                    "~{}proj{}main.rs",
                    std::path::MAIN_SEPARATOR,
                    std::path::MAIN_SEPARATOR
                )
            );
            // ホームそのものは "~" になる
            assert_eq!(display_path(&home), "~");
        }
    }

    #[test]
    fn display_path_outside_home_unchanged() {
        assert_eq!(
            display_path(Path::new("/zv-no-such-home/etc/hosts")),
            "/zv-no-such-home/etc/hosts"
        );
    }

    // ─── native_sc (ショートカット表記変換) ───

    #[test]
    fn native_sc_representative_specs() {
        let cmd_c = native_sc("cmd+c");
        let ctrl_shift_p = native_sc("ctrl+shift+p");
        let alt_up = native_sc("alt+up");
        if cfg!(target_os = "macos") {
            assert_eq!(cmd_c, "⌘C");
            assert_eq!(ctrl_shift_p, "⌃⇧P");
            assert_eq!(alt_up, "⌥↑");
        } else {
            assert_eq!(cmd_c, "Ctrl+C");
            assert_eq!(ctrl_shift_p, "Ctrl+Shift+P");
            assert_eq!(alt_up, "Alt+↑");
        }
    }

    #[test]
    fn native_sc_invalid_spec_is_empty() {
        assert_eq!(native_sc(""), "");
        assert_eq!(native_sc("nosuchkey"), "");
        assert_eq!(native_sc("badmod+c"), "");
    }

    // ── tasks.json の行 ────────────────────────────────────────

    /// ディスクに**本物の** `.vscode/tasks.json` (コメント・末尾カンマ・
    /// 未対応変数入り) を置いて、「タスクの実行…」の行になるまでを通す。
    #[test]
    fn tasks_json_rows_keep_unrunnable_tasks_with_a_reason() {
        let root = crate::test_util::unique_temp_dir("zaivern-menu-tasks", "rows");
        let p = crate::tasks::tasks_json_path(&root);
        std::fs::create_dir_all(p.parent().expect("親")).expect("mkdir .vscode");
        std::fs::write(
            &p,
            concat!(
                "{\n",
                "  // JSONC: 行コメント\n",
                "  /* ブロックコメントも通る */\n",
                "  \"version\": \"2.0.0\",\n",
                "  \"tasks\": [\n",
                "    {\n",
                "      \"label\": \"say hello\",\n",
                "      \"type\": \"shell\",\n",
                "      \"command\": \"echo\",\n",
                "      \"args\": [\"zaivern hello\"],\n",
                "    },\n",
                "    {\n",
                "      \"label\": \"lint this file\",\n",
                "      \"type\": \"shell\",\n",
                "      \"command\": \"echo ${file}\",\n",
                "    },\n",
                "    {\n",
                "      \"label\": \"pick a target\",\n",
                "      \"type\": \"shell\",\n",
                "      \"command\": \"echo ${input:target} ${command:foo}\",\n",
                "    },\n",
                "  ],\n", // ← 末尾カンマ
                "}\n"
            ),
        )
        .expect("write tasks.json");

        let doc = crate::tasks::load_tasks(&root);
        assert_eq!(doc.error, None, "{doc:?}");

        // アクティブファイルが無いとき
        let rows = task_rows(&doc, None, false);
        let names: Vec<&str> = rows.iter().map(|(_, l, _)| l.as_str()).collect();
        assert_eq!(
            names,
            vec!["say hello", "lint this file", "pick a target"],
            "3 件とも一覧に出る (壊れていても消さない)"
        );
        assert_eq!(rows[0].2, None, "echo タスクは実行できる");
        assert!(rows[1].2.is_some(), "${{file}} はアクティブファイルが要る");
        let why = rows[2].2.clone().expect("未対応変数の理由");
        assert!(
            why.contains("${input:target}") || why.contains("${command:foo}"),
            "理由に未対応の変数が出る: {why}"
        );

        // アクティブファイルがあれば ${file} のタスクだけ実行できるようになる
        let f = root.join("a.rs");
        let rows = task_rows(&doc, Some(&f), false);
        assert_eq!(rows[1].2, None, "${{file}} が解決できる");
        assert!(rows[2].2.is_some(), "未対応変数は依然として実行させない");

        // index はそのまま Cmd::RunJsonTask の引数になる (並びと一致すること)
        assert_eq!(
            rows.iter().map(|(i, ..)| *i).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );

        std::fs::remove_dir_all(&root).expect("後片付け");
    }

    /// 既定のビルドタスクは `⇧⌘B` が拾える形で出てくる。
    #[test]
    fn tasks_json_default_build_is_reachable_from_the_build_shortcut() {
        let root = crate::test_util::unique_temp_dir("zaivern-menu-tasks", "build");
        let p = crate::tasks::tasks_json_path(&root);
        std::fs::create_dir_all(p.parent().expect("親")).expect("mkdir .vscode");
        std::fs::write(
            &p,
            concat!(
                "{\n",
                "  \"tasks\": [\n",
                "    { \"label\": \"other\", \"command\": \"echo other\" },\n",
                "    {\n",
                "      \"label\": \"my build\",\n",
                "      \"command\": \"echo built\",\n",
                "      \"group\": { \"kind\": \"build\", \"isDefault\": true }\n",
                "    }\n",
                "  ]\n",
                "}\n"
            ),
        )
        .expect("write tasks.json");

        let doc = crate::tasks::load_tasks(&root);
        let b = doc.default_build().expect("既定のビルドタスク");
        assert_eq!(b.label, "my build");
        assert_eq!(
            crate::tasks::resolve(b, None, false).expect("実行行"),
            "echo built"
        );
        // 自動検出は素のフォルダでは何も見つけない → tasks.json 側が拾われる
        assert_eq!(build_task_for(&root), None);

        std::fs::remove_dir_all(&root).expect("後片付け");
    }
}
