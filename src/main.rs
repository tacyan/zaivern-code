#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agents;
mod app;
mod config;
mod editor;
mod editor_ops;
mod file_tree;
mod fuzzy;
mod git;
mod git_panel;
mod highlight;
mod html;
mod jsonc;
mod keybinds;
mod lsp;
mod markdown;
mod notify;
mod palette;
mod pet;
mod pet_bubble;
mod pet_variants;
mod plugins;
mod remote;
mod sound;
mod session;
mod snippets;
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
    const BYTES: &[u8] = include_bytes!("../assets/Zaivern.png");
    let img = image::load_from_memory(BYTES).ok()?;
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
    let workspace = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1480.0, 940.0])
        .with_min_inner_size([860.0, 560.0])
        .with_title("Zaivern Code");
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
        Box::new(move |cc| Ok(Box::new(app::ZaivernApp::new(cc, workspace)))),
    )
}
