use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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

/// 複数ルート(マルチルートワークスペース)の正規化。
///
/// ルール（重複・二重表示を防ぐため）:
/// - 入力順を保ち、`[0]` を primary(既存の単一ルート相当)として扱う。
/// - ディレクトリでないものは黙って捨てる。
/// - 比較・保持ともに canonicalize 済みパスを使う(シンボリックリンク差と
///   `..` を吸収する)。canonicalize できない場合は入力パスのまま使う。
/// - 既に採用済みルートと同一、またはその配下(ネスト)なら捨てる
///   — 親ルートのツリーから辿れるので二重に並べない。
/// - 逆に新しいルートが採用済みルートの祖先なら、広い方を採用して
///   配下の既存ルートを取り除く(位置は最初に現れた場所を保つ)。
pub fn normalize_roots(input: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in input {
        if !p.is_dir() {
            continue;
        }
        let c = p.canonicalize().unwrap_or(p);
        if out.iter().any(|r| c.starts_with(r)) {
            continue; // 同一 or 既存ルート配下 → 既に見えている
        }
        // 新ルートが既存ルートの祖先なら、狭い方を畳んで広い方を残す
        if let Some(pos) = out.iter().position(|r| r.starts_with(&c)) {
            out.retain(|r| !r.starts_with(&c));
            out.insert(pos, c);
        } else {
            out.push(c);
        }
    }
    out
}

pub struct FileTree {
    /// ワークスペースのルート一覧(常に 1 件以上)。`roots[0]` が primary。
    pub roots: Vec<PathBuf>,
    cache: HashMap<PathBuf, Vec<Entry>>,
    /// キャッシュ取得時のディレクトリ mtime(外部変更検知用)。
    /// エントリの追加・削除・リネームで親ディレクトリの mtime が変わる。
    mtimes: HashMap<PathBuf, Option<SystemTime>>,
    pub show_hidden: bool,
    edit: Option<EditState>,
}

impl FileTree {
    /// `roots` は 1 件以上を想定 (空でも落ちないが何も描かれない)。
    pub fn new(roots: Vec<PathBuf>, show_hidden: bool) -> Self {
        Self {
            roots,
            cache: HashMap::new(),
            mtimes: HashMap::new(),
            show_hidden,
            edit: None,
        }
    }

    pub fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.roots = roots;
        self.cache.clear();
        self.mtimes.clear();
        self.edit = None;
    }

    /// `p` を含むルート(最長一致)。どのルートにも属さなければ None。
    pub fn root_for(&self, p: &Path) -> Option<&Path> {
        self.roots
            .iter()
            .filter(|r| p.starts_with(r))
            .max_by_key(|r| r.as_os_str().len())
            .map(|r| r.as_path())
    }

    pub fn invalidate(&mut self) {
        self.cache.clear();
        self.mtimes.clear();
    }

    /// キャッシュ済みの各階層をディレクトリ mtime で確認し、外部(エージェント等)で
    /// ファイルが追加・削除・リネームされていたら全キャッシュを破棄する。
    /// 変化があれば true(次フレームの描画でディスクから読み直される)。
    pub fn refresh_if_changed(&mut self) -> bool {
        let changed = self
            .mtimes
            .iter()
            .any(|(dir, cached)| dir_mtime(dir) != *cached);
        if changed {
            self.invalidate();
        }
        changed
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
        self.mtimes.insert(dir.to_path_buf(), dir_mtime(dir));
        v
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, theme: &Theme, actions: &mut TreeActions) {
        let roots = self.roots.clone();
        // 単一ルート時は従来どおりヘッダ無しで直下を描く(見た目を変えない)。
        if roots.len() <= 1 {
            let root = roots.into_iter().next().unwrap_or_else(|| PathBuf::from("."));
            self.dir_ui(ui, &root, theme, actions, 0);
            return;
        }
        for root in &roots {
            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| root.to_string_lossy().to_string());
            let cr = egui::CollapsingHeader::new(
                RichText::new(format!("📚 {name}")).color(theme.text).strong(),
            )
            .id_salt(root)
            .default_open(true)
            .show(ui, |ui| {
                self.dir_ui(ui, root, theme, actions, 0);
            });
            cr.header_response.context_menu(|ui| {
                if ui.button("➕ 新規ファイル").clicked() {
                    self.start_new_file(root.clone());
                    ui.close_menu();
                }
                if ui.button("📂 新規フォルダ").clicked() {
                    self.start_new_dir(root.clone());
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("📋 フルパスをコピー").clicked() {
                    ui.ctx().copy_text(root.to_string_lossy().to_string());
                    ui.close_menu();
                }
            });
        }
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
                    if ui.button("📂 新規フォルダ").clicked() {
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
                    if ui.button("👾 パスをエージェントに送信").clicked() {
                        // マルチルート: そのパスを含むルートからの相対パスにする
                        let rel = self
                            .root_for(&e.path)
                            .and_then(|r| e.path.strip_prefix(r).ok())
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
                // エージェントのターミナルへドラッグ&ドロップでパスを渡せる
                // (クリック=開く はそのまま。ドラッグとクリックは egui が排他にする)
                let resp = resp.interact(egui::Sense::click_and_drag());
                resp.dnd_set_drag_payload(e.path.clone());
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
                    if ui.button("👾 パスをエージェントに送信").clicked() {
                        // マルチルート: そのパスを含むルートからの相対パスにする
                        let rel = self
                            .root_for(&e.path)
                            .and_then(|r| e.path.strip_prefix(r).ok())
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

/// ディレクトリの更新時刻。取得できない(削除された等)場合は None。
fn dir_mtime(dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(dir).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;
    use std::time::Duration;

    /// 記録済みの mtime を過去へずらし、同一秒内の外部変更でも差が出るようにする
    /// （mtime 粒度が粗いファイルシステムでもテストを決定的にするため）。
    fn backdate_recorded(t: &mut FileTree, dir: &Path) {
        let m = t.mtimes.get_mut(dir).expect("dir is cached");
        *m = m.map(|x| x - Duration::from_secs(2));
    }

    /// canonicalize 後の比較用（macOS の /tmp → /private/tmp 等を吸収）。
    fn canon(p: &Path) -> PathBuf {
        p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
    }

    #[test]
    fn normalize_roots_dedups_and_keeps_order() {
        let dir = unique_temp_dir("zaivern-tree-test", "norm-dedup");
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::create_dir_all(&a).expect("mkdir a");
        std::fs::create_dir_all(&b).expect("mkdir b");

        let out = normalize_roots(vec![a.clone(), b.clone(), a.clone()]);
        assert_eq!(out, vec![canon(&a), canon(&b)], "重複は落ち、順序は保たれる");

        // `..` 経由の別表記も canonicalize で同一視される
        let a_alt = dir.join("b").join("..").join("a");
        let out = normalize_roots(vec![a.clone(), a_alt]);
        assert_eq!(out, vec![canon(&a)]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn normalize_roots_drops_nested_and_prefers_ancestor() {
        let dir = unique_temp_dir("zaivern-tree-test", "norm-nest");
        let parent = dir.join("parent");
        let child = parent.join("child");
        let other = dir.join("other");
        std::fs::create_dir_all(&child).expect("mkdir child");
        std::fs::create_dir_all(&other).expect("mkdir other");

        // 親が先: 子は親から辿れるので捨てる
        let out = normalize_roots(vec![parent.clone(), child.clone()]);
        assert_eq!(out, vec![canon(&parent)], "子ルートは二重表示しない");

        // 子が先: 後から来た祖先の方が広いので、子を畳んで親に置き換える
        let out = normalize_roots(vec![child.clone(), other.clone(), parent.clone()]);
        assert_eq!(
            out,
            vec![canon(&parent), canon(&other)],
            "祖先が勝ち、位置は最初に現れた場所を保つ"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn normalize_roots_ignores_non_directories() {
        let dir = unique_temp_dir("zaivern-tree-test", "norm-nondir");
        let real = dir.join("real");
        std::fs::create_dir_all(&real).expect("mkdir");
        let file = dir.join("note.txt");
        std::fs::write(&file, "x").expect("write");

        let out = normalize_roots(vec![file, dir.join("missing"), real.clone()]);
        assert_eq!(out, vec![canon(&real)], "ファイル・存在しないパスは捨てる");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn root_for_picks_longest_match() {
        // ネストしたルートを（正規化を通さず）直接持たせても最長一致で解決する
        let t = FileTree::new(
            vec![PathBuf::from("/ws/a"), PathBuf::from("/ws/a/deep")],
            false,
        );
        assert_eq!(t.root_for(Path::new("/ws/a/x.rs")), Some(Path::new("/ws/a")));
        assert_eq!(
            t.root_for(Path::new("/ws/a/deep/x.rs")),
            Some(Path::new("/ws/a/deep")),
        );
        assert_eq!(t.root_for(Path::new("/elsewhere/x.rs")), None);
        assert_eq!(t.roots[0], PathBuf::from("/ws/a"), "primary は roots[0]");
    }

    #[test]
    fn refresh_is_noop_without_changes() {
        let dir = unique_temp_dir("zaivern-tree-test", "noop");
        std::fs::write(dir.join("a.txt"), "x").expect("write");
        let mut t = FileTree::new(vec![dir.clone()], false);
        assert_eq!(t.entries(&dir).len(), 1);
        assert!(!t.refresh_if_changed());
        assert!(t.cache.contains_key(&dir), "変化が無ければキャッシュは保持");
    }

    #[test]
    fn refresh_detects_external_create() {
        let dir = unique_temp_dir("zaivern-tree-test", "create");
        let mut t = FileTree::new(vec![dir.clone()], false);
        assert!(t.entries(&dir).is_empty());

        std::fs::write(dir.join("agent.rs"), "fn main() {}").expect("external create");
        backdate_recorded(&mut t, &dir);

        assert!(t.refresh_if_changed(), "外部作成を検知する");
        let names: Vec<_> = t.entries(&dir).iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, ["agent.rs"]);
    }

    #[test]
    fn refresh_detects_external_delete() {
        let dir = unique_temp_dir("zaivern-tree-test", "delete");
        let path = dir.join("gone.txt");
        std::fs::write(&path, "x").expect("write");
        let mut t = FileTree::new(vec![dir.clone()], false);
        assert_eq!(t.entries(&dir).len(), 1);

        std::fs::remove_file(&path).expect("external delete");
        backdate_recorded(&mut t, &dir);

        assert!(t.refresh_if_changed(), "外部削除を検知する");
        assert!(t.entries(&dir).is_empty());
    }
}

pub fn icon_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => "🐾",
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
