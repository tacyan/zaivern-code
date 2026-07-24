#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent_input;
mod agent_picker;
mod agents;
mod app;
mod cli;
mod commander;
mod config;
mod coordinator;
mod desktop;
mod diagnostician;
mod diff;
mod editor;
mod editor_ops;
mod file_search;
mod file_tree;
mod fuzzy;
mod git;
mod git_panel;
mod github;
mod highlight;
mod html;
mod i18n;
mod ide;
mod jsonc;
mod keybinds;
mod lsp;
mod markdown;
mod menu_bar;
mod notify;
mod orchestration;
mod palette;
mod panels;
mod pet;
mod pet_bubble;
mod pet_variants;
mod plugins;
mod recent;
mod remote;
mod sound;
mod session;
mod snippets;
mod supervisor;
mod terminal;
#[cfg(test)]
mod test_util;
mod theme;
mod theme_json;
mod voice;

use eframe::egui;

/// アプリアイコン(assets/Zaivern.png をバイナリに埋め込む)。
/// ウィンドウ/タスクバーアイコンとして 256px に縮小して使う。
/// 失敗してもアイコン無しで起動を続ける。
fn load_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(desktop::ICON_PNG).ok()?;
    let img = img.resize_exact(256, 256, image::imageops::FilterType::Lanczos3);
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    Some(egui::IconData {
        rgba: rgba.into_raw(),
        width,
        height,
    })
}

fn main() -> eframe::Result<()> {
    // サブコマンド指定なら CLI として処理して終了する。
    // 引数なし / パス指定のときは None が返り、そのまま GUI 起動へ進む。
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = cli::try_run_cli(&args) {
        std::process::exit(code);
    }

    // 引数はマルチルートワークスペースとして解釈する: `zai dirA dirB dirC`。
    // ディレクトリはルートに、ファイルは起動後に開くタブになる。
    // 存在しない引数・その他は黙って無視する（起動は止めない）。
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for a in &args {
        let p = std::path::PathBuf::from(a);
        if p.is_dir() {
            dirs.push(p);
        } else if p.is_file() {
            files.push(p);
        }
    }
    // 引数無し = カレントディレクトリ（従来どおり）。roots は決して空にしない。
    let mut roots = file_tree::normalize_roots(dirs);
    if roots.is_empty() {
        roots = file_tree::normalize_roots(std::env::current_dir().ok());
    }
    if roots.is_empty() {
        roots.push(std::path::PathBuf::from("."));
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1480.0, 940.0])
        .with_min_inner_size([860.0, 560.0])
        .with_title("Zaivern Code")
        // Linux で .desktop (zaivern-code.desktop) と結び付ける ID。
        // desktop.rs の Icon= / StartupWMClass= と一致させること。
        .with_app_id("zaivern-code");
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Zaivern Code",
        options,
        Box::new(move |cc| Ok(Box::new(app::ZaivernApp::new(cc, roots, files)))),
    )
}
