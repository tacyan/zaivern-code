//! VS Code 準拠のメニューバー。
//!
//! ファイル / 編集 / 選択 / 表示 / 移動 / 実行 / ターミナル / ヘルプ の
//! 8 メニューを VS Code と同じ並び・同じ項目名 (日本語版 VS Code 準拠) で描画し、
//! 選ばれた操作を `Cmd` として返す。実処理はすべて app.rs の `apply_cmd` が担う。
//! ショートカット表記は実際のキーバインド (config.toml の上書き込み) に追従する。

use crate::i18n::tr;
use crate::keybinds::{format_shortcut, BindAction, Keybinds};
use crate::palette::Cmd;
use std::path::{Path, PathBuf};

/// メニューの表示状態スナップショット。描画のためだけの読み取り専用情報。
pub struct MenuInfo {
    pub sidebar_open: bool,
    pub terminal_open: bool,
    pub cockpit_open: bool,
    pub problems_open: bool,
    pub fullscreen: bool,
    pub auto_save: bool,
    /// アクティブなエディタタブがあるか (編集系メニューの有効/無効)
    pub has_editor: bool,
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
    /// (テーマ name, ラベル, 選択中か)。カスタムテーマも同じ形で混ぜる
    pub themes: Vec<(String, String, bool)>,
    /// ビルドタスクのラベル (検出できたときだけ Some。例 "cargo build")
    pub build_task: Option<String>,
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

/// キーバインド済みアクションのショートカット表記。
fn sc(keys: &Keybinds, a: BindAction) -> String {
    format_shortcut(keys.get(a))
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
        if item(ui, &tr("新しいテキスト ファイル"), &sc(keys, BindAction::NewFile), true) {
            cmds.push(Cmd::NewFile);
        }
        if item(ui, &tr("ファイルを開く…"), &sc(keys, BindAction::OpenFile), true) {
            cmds.push(Cmd::OpenFileDialog);
        }
        if item(ui, &tr("フォルダーを開く…"), "", true) {
            cmds.push(Cmd::OpenFolder);
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
            ui.menu_button(tr("フォルダーをワークスペースから削除"), |ui| {
                ui.set_min_width(280.0);
                for r in &info.roots {
                    if item(ui, &display_path(r), "", true) {
                        cmds.push(Cmd::RemoveFolder(r.clone()));
                    }
                }
            });
        }
        ui.separator();
        if item(ui, &tr("保存"), &sc(keys, BindAction::Save), info.has_editor) {
            cmds.push(Cmd::Save);
        }
        if item(ui, &tr("名前を付けて保存…"), &sc(keys, BindAction::SaveAs), info.has_editor) {
            cmds.push(Cmd::SaveAs);
        }
        if item(ui, &tr("すべて保存"), &sc(keys, BindAction::SaveAll), info.has_editor) {
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
            if item(ui, &tr("設定 config.toml を開く"), "", true) {
                cmds.push(Cmd::OpenConfig);
            }
            if item(ui, &tr("設定を再読み込み"), "", true) {
                cmds.push(Cmd::ReloadConfig);
            }
            if item(ui, &tr("キーボード ショートカット"), "", true) {
                cmds.push(Cmd::ShowShortcuts);
            }
        });
        ui.separator();
        if item(ui, &tr("エディターを閉じる"), &sc(keys, BindAction::CloseTab), info.has_editor) {
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
        if item(ui, &tr("元に戻す"), &native_sc("cmd+z"), ed) {
            cmds.push(Cmd::Undo);
        }
        if item(ui, &tr("やり直し"), &native_sc("cmd+shift+z"), ed) {
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
        if item(ui, &tr("ファイル間で検索"), &sc(keys, BindAction::GlobalSearch), true) {
            cmds.push(Cmd::GlobalSearch);
        }
        ui.separator();
        if item(ui, &tr("行コメントの切り替え"), &sc(keys, BindAction::ToggleComment), ed) {
            cmds.push(Cmd::ToggleLineComment);
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
        if item(ui, &tr("行を複製"), &sc(keys, BindAction::DuplicateLine), ed) {
            cmds.push(Cmd::DuplicateLine);
        }
        if item(ui, &tr("行を上へ移動"), &sc(keys, BindAction::MoveLineUp), ed) {
            cmds.push(Cmd::MoveLineUp);
        }
        if item(ui, &tr("行を下へ移動"), &sc(keys, BindAction::MoveLineDown), ed) {
            cmds.push(Cmd::MoveLineDown);
        }
    });
}

fn view_menu(ui: &mut egui::Ui, info: &MenuInfo, keys: &Keybinds, cmds: &mut Vec<Cmd>) {
    ui.menu_button(tr("表示"), |ui| {
        ui.set_min_width(300.0);
        if item(ui, &tr("コマンド パレット…"), &sc(keys, BindAction::PaletteCommands), true) {
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
            ui.separator();
            ui.menu_button(tr("配色テーマ"), |ui| {
                ui.set_min_width(260.0);
                egui::ScrollArea::vertical()
                    .id_salt("menubar-themes")
                    .max_height(380.0)
                    .show(ui, |ui| {
                        for (name, label, selected) in &info.themes {
                            if ui.selectable_label(*selected, label).clicked() {
                                cmds.push(Cmd::SetTheme(name.clone()));
                                ui.close_menu();
                            }
                        }
                    });
            });
            ui.separator();
            if item(ui, &tr("ズームイン"), &sc(keys, BindAction::FontInc), true) {
                cmds.push(Cmd::FontInc);
            }
            if item(ui, &tr("ズームアウト"), &sc(keys, BindAction::FontDec), true) {
                cmds.push(Cmd::FontDec);
            }
        });
        ui.separator();
        if item(ui, &tr("エクスプローラー"), &sc(keys, BindAction::FocusExplorer), true) {
            cmds.push(Cmd::ShowExplorer);
        }
        if item(ui, &tr("検索"), &sc(keys, BindAction::GlobalSearch), true) {
            cmds.push(Cmd::GlobalSearch);
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
        let md = if info.md_preview {
            tr("✓ Markdown/HTML プレビュー")
        } else {
            tr("Markdown/HTML プレビュー")
        };
        if item(ui, &md, &sc(keys, BindAction::ToggleMdPreview), info.has_editor) {
            cmds.push(Cmd::ToggleMdPreview);
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
        if item(ui, &tr("ファイルへ移動…"), &sc(keys, BindAction::PaletteFiles), true) {
            cmds.push(Cmd::OpenFilePalette);
        }
        ui.separator();
        if item(ui, &tr("次のエディター"), &sc(keys, BindAction::NextTab), ed) {
            cmds.push(Cmd::NextTab);
        }
        if item(ui, &tr("前のエディター"), &sc(keys, BindAction::PrevTab), ed) {
            cmds.push(Cmd::PrevTab);
        }
        ui.separator();
        if item(ui, &tr("定義へ移動"), &sc(keys, BindAction::GoToDefinition), info.has_file) {
            cmds.push(Cmd::GoToDefinition);
        }
        if item(ui, &tr("ブラケットへ移動"), &sc(keys, BindAction::GoToBracket), ed) {
            cmds.push(Cmd::GoToBracket);
        }
        ui.separator();
        if item(ui, &tr("行/列へ移動…"), &sc(keys, BindAction::GoToLine), ed) {
            cmds.push(Cmd::GoToLine);
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
        if !info.plugin_commands.is_empty() {
            ui.menu_button(tr("タスクの実行…"), |ui| {
                ui.set_min_width(300.0);
                for (pi, ci, icon, title) in &info.plugin_commands {
                    if item(ui, &format!("{icon} {title}"), "", true) {
                        cmds.push(Cmd::RunPlugin(*pi, *ci));
                    }
                }
            });
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
        if item(ui, &tr("新しいターミナル"), &sc(keys, BindAction::NewTerminal), true) {
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
        if item(ui, &tr("選択したテキストをターミナルへ送る"), "", info.has_editor) {
            cmds.push(Cmd::RunSelection);
        }
        let build_label = match &info.build_task {
            Some(l) => format!("🔨 {l}"),
            None => tr("ビルド タスクの実行…"),
        };
        if item(ui, &build_label, &sc(keys, BindAction::RunBuildTask), info.build_task.is_some())
        {
            cmds.push(Cmd::RunBuildTask);
        }
        if !info.plugin_commands.is_empty() {
            ui.separator();
            ui.menu_button(tr("タスクの実行…"), |ui| {
                ui.set_min_width(300.0);
                for (pi, ci, icon, title) in &info.plugin_commands {
                    if item(ui, &format!("{icon} {title}"), "", true) {
                        cmds.push(Cmd::RunPlugin(*pi, *ci));
                    }
                }
            });
        }
    });
}

fn help_menu(ui: &mut egui::Ui, cmds: &mut Vec<Cmd>) {
    ui.menu_button(tr("ヘルプ"), |ui| {
        ui.set_min_width(300.0);
        if item(ui, &tr("キーボード ショートカットのリファレンス"), "", true) {
            cmds.push(Cmd::ShowShortcuts);
        }
        if item(ui, &tr("コマンド パレットですべてのコマンドを表示"), "", true) {
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
    let out = std::process::Command::new("pbpaste").output().ok()?;
    #[cfg(target_os = "windows")]
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "Get-Clipboard -Raw"])
        .output()
        .ok()?;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let out = std::process::Command::new("sh")
        .args([
            "-c",
            "command -v wl-paste >/dev/null && wl-paste --no-newline || xclip -selection clipboard -o",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).to_string();
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
        assert_eq!(runner_for(Path::new("/a/main.rs"), Path::new("/nonexistent")), None);
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
}
