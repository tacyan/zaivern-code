use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eframe::egui::{self, RichText};

use crate::theme::Theme;

#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

/// What the user asked for via the tree UI this frame.
#[derive(Default)]
pub struct TreeActions {
    pub open: Option<PathBuf>,
    pub send_to_agent: Option<String>,
    /// 新規ファイル作成(確定済みのフルパス)
    pub create_file: Option<PathBuf>,
    /// 新規フォルダ作成(確定済みのフルパス)
    pub create_dir: Option<PathBuf>,
    /// 名前の変更 (旧パス, 新パス)
    pub rename: Option<(PathBuf, PathBuf)>,
    /// 削除要求(確認ダイアログは呼び出し側が出す)
    pub delete: Option<PathBuf>,
}

/// ツリー内インライン編集の種類。
#[derive(PartialEq)]
enum EditKind {
    NewFile,
    NewDir,
    Rename,
}

/// ツリー内インライン編集(VS Code 風: その場で名前を入力)。
struct EditState {
    kind: EditKind,
    /// NewFile/NewDir: 親ディレクトリ / Rename: 対象パス
    target: PathBuf,
    text: String,
    /// 次フレームでテキスト欄へフォーカスを移す
    focus: bool,
}

pub struct FileTree {
    pub root: PathBuf,
    cache: HashMap<PathBuf, Vec<Entry>>,
    pub show_hidden: bool,
    edit: Option<EditState>,
}

impl FileTree {
    pub fn new(root: PathBuf, show_hidden: bool) -> Self {
        Self {
            root,
            cache: HashMap::new(),
            show_hidden,
            edit: None,
        }
    }

    pub fn set_root(&mut self, root: PathBuf) {
        self.root = root;
        self.cache.clear();
        self.edit = None;
    }

    pub fn invalidate(&mut self) {
        self.cache.clear();
    }

    /// `dir` 直下への新規ファイル作成を開始する(インライン入力を出す)。
    pub fn start_new_file(&mut self, dir: PathBuf) {
        self.edit = Some(EditState {
            kind: EditKind::NewFile,
            target: dir,
            text: String::new(),
            focus: true,
        });
    }

    /// `dir` 直下への新規フォルダ作成を開始する。
    pub fn start_new_dir(&mut self, dir: PathBuf) {
        self.edit = Some(EditState {
            kind: EditKind::NewDir,
            target: dir,
            text: String::new(),
            focus: true,
        });
    }

    /// `path` の名前変更を開始する(現在の名前入りのインライン入力を出す)。
    pub fn start_rename(&mut self, path: PathBuf) {
        let text = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        self.edit = Some(EditState {
            kind: EditKind::Rename,
            target: path,
            text,
            focus: true,
        });
    }

    fn entries(&mut self, dir: &Path) -> Vec<Entry> {
        if let Some(v) = self.cache.get(dir) {
            return v.clone();
        }
        let mut v: Vec<Entry> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name == ".git" || name == ".DS_Store" {
                    continue;
                }
                if !self.show_hidden && name.starts_with('.') {
                    continue;
                }
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                v.push(Entry {
                    path: e.path(),
                    name,
                    is_dir,
                });
            }
        }
        v.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        self.cache.insert(dir.to_path_buf(), v.clone());
        v
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, theme: &Theme, actions: &mut TreeActions) {
        let root = self.root.clone();
        self.dir_ui(ui, &root, theme, actions, 0);
    }

    /// この dir 直下に New 系のインライン入力を出すべきか。
    fn editing_new_in(&self, dir: &Path) -> bool {
        self.edit
            .as_ref()
            .is_some_and(|es| es.kind != EditKind::Rename && es.target == dir)
    }

    /// このパスがリネーム編集中か。
    fn renaming(&self, path: &Path) -> bool {
        self.edit
            .as_ref()
            .is_some_and(|es| es.kind == EditKind::Rename && es.target == path)
    }

    /// インライン入力行を描く。Enter で確定(actions へ書き込み)、Esc / フォーカス喪失でキャンセル。
    fn edit_row_ui(&mut self, ui: &mut egui::Ui, actions: &mut TreeActions) {
        let Some(mut es) = self.edit.take() else {
            return;
        };
        let icon = match es.kind {
            EditKind::NewDir => "📁",
            EditKind::NewFile => "📄",
            EditKind::Rename => {
                if es.target.is_dir() {
                    "📁"
                } else {
                    icon_for(&es.text)
                }
            }
        };
        let mut done = false;
        ui.horizontal(|ui| {
            ui.label(icon);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut es.text)
                    .desired_width(f32::INFINITY)
                    .hint_text("名前を入力して Enter"),
            );
            if es.focus {
                resp.request_focus();
                es.focus = false;
            }
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let cancel =
                ui.input(|i| i.key_pressed(egui::Key::Escape)) || (resp.lost_focus() && !enter);
            if enter {
                let name = es.text.trim();
                // 空・パス区切り入りは不正として無視(その場でキャンセル扱い)
                if !name.is_empty() && !name.contains('/') && !name.contains('\\') {
                    match es.kind {
                        EditKind::NewFile => actions.create_file = Some(es.target.join(name)),
                        EditKind::NewDir => actions.create_dir = Some(es.target.join(name)),
                        EditKind::Rename => {
                            let new_path = es
                                .target
                                .parent()
                                .map(|p| p.join(name))
                                .unwrap_or_else(|| PathBuf::from(name));
                            if new_path != es.target {
                                actions.rename = Some((es.target.clone(), new_path));
                            }
                        }
                    }
                }
                done = true;
            } else if cancel {
                done = true;
            }
        });
        if !done {
            self.edit = Some(es);
        }
    }

    fn dir_ui(
        &mut self,
        ui: &mut egui::Ui,
        dir: &Path,
        theme: &Theme,
        actions: &mut TreeActions,
        depth: usize,
    ) {
        if depth > 24 {
            return;
        }
        // 新規ファイル/フォルダのインライン入力(この階層が対象のとき先頭に出す)
        if self.editing_new_in(dir) {
            self.edit_row_ui(ui, actions);
        }
        for e in self.entries(dir) {
            // リネーム中の項目は行ごと入力欄に置き換える
            if self.renaming(&e.path) {
                self.edit_row_ui(ui, actions);
                continue;
            }
            if e.is_dir {
                // 新規作成の入力を出している間は対象フォルダを強制的に開く
                let force_open = self.editing_new_in(&e.path).then_some(true);
                let cr = egui::CollapsingHeader::new(
                    RichText::new(format!("📁 {}", e.name)).color(theme.text),
                )
                .id_salt(&e.path)
                .default_open(false)
                .open(force_open)
                .show(ui, |ui| {
                    self.dir_ui(ui, &e.path, theme, actions, depth + 1);
                });
                cr.header_response.context_menu(|ui| {
                    if ui.button("➕ 新規ファイル").clicked() {
                        self.start_new_file(e.path.clone());
                        ui.close_menu();
                    }
                    if ui.button("🗂 新規フォルダ").clicked() {
                        self.start_new_dir(e.path.clone());
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("✏ 名前を変更").clicked() {
                        self.start_rename(e.path.clone());
                        ui.close_menu();
                    }
                    if ui.button("🗑 削除…").clicked() {
                        actions.delete = Some(e.path.clone());
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("🤖 パスをエージェントに送信").clicked() {
                        let rel = e
                            .path
                            .strip_prefix(&self.root)
                            .unwrap_or(&e.path)
                            .to_string_lossy()
                            .to_string();
                        actions.send_to_agent = Some(format!("@{rel} "));
                        ui.close_menu();
                    }
                    if ui.button("📋 フルパスをコピー").clicked() {
                        ui.ctx().copy_text(e.path.to_string_lossy().to_string());
                        ui.close_menu();
                    }
                });
            } else {
                let label = format!("{} {}", icon_for(&e.name), e.name);
                let resp = ui.selectable_label(false, RichText::new(label).color(theme.text));
                if resp.clicked() {
                    actions.open = Some(e.path.clone());
                }
                resp.context_menu(|ui| {
                    if ui.button("📂 エディタで開く").clicked() {
                        actions.open = Some(e.path.clone());
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("✏ 名前を変更").clicked() {
                        self.start_rename(e.path.clone());
                        ui.close_menu();
                    }
                    if ui.button("🗑 削除…").clicked() {
                        actions.delete = Some(e.path.clone());
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("🤖 パスをエージェントに送信").clicked() {
                        let rel = e
                            .path
                            .strip_prefix(&self.root)
                            .unwrap_or(&e.path)
                            .to_string_lossy()
                            .to_string();
                        actions.send_to_agent = Some(format!("@{rel} "));
                        ui.close_menu();
                    }
                    if ui.button("📋 フルパスをコピー").clicked() {
                        ui.ctx().copy_text(e.path.to_string_lossy().to_string());
                        ui.close_menu();
                    }
                });
            }
        }
    }
}

pub fn icon_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => "🦀",
        "md" | "markdown" => "📝",
        "toml" | "json" | "yaml" | "yml" | "ini" | "cfg" => "⚙️",
        "js" | "jsx" | "ts" | "tsx" | "mjs" => "📜",
        "py" => "🐍",
        "go" => "🐹",
        "html" | "htm" => "🌐",
        "css" | "scss" | "sass" => "🎨",
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" => "🖼",
        "lock" => "🔒",
        "sh" | "bash" | "zsh" | "fish" => "💲",
        _ => "📄",
    }
}
