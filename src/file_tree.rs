use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use eframe::egui::{self, Key, Modifiers, RichText};

use crate::i18n::{tr, trf};
use crate::theme::Theme;

use egui::collapsing_header::CollapsingState;

#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

/// `p` を含むルート(最長一致)。どのルートにも属さなければ None。
///
/// FileTree / App / GitSet のルート解決が共有する唯一の実装。
/// 同じ長さのルートが並んだ場合は `max_by_key` の仕様どおり
/// 「後に並んだ方」が選ばれる(従来 3 実装と同一の挙動)。
pub(crate) fn root_for<'a>(roots: &'a [PathBuf], p: &Path) -> Option<&'a Path> {
    roots
        .iter()
        .filter(|r| p.starts_with(r))
        .max_by_key(|r| r.as_os_str().len())
        .map(|r| r.as_path())
}

/// 貼り付けの種類(コピー or 切り取りによる移動)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transfer {
    Copy,
    Move,
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
    /// 貼り付け (コピー元, 貼り付け先フルパス, 種類)。fs 操作は呼び出し側。
    pub transfer: Option<(PathBuf, PathBuf, Transfer)>,
    /// ユーザーへ知らせたい注意(貼り付け不可など)。呼び出し側がトーストで出す。
    pub notice: Option<String>,
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

/// キーボード操作用の可視行(描画順のスナップショット)。
struct Row {
    path: PathBuf,
    name: String,
    is_dir: bool,
    /// dir のとき、現在展開されているか
    open: bool,
    /// 親ディレクトリ行(可視行として存在する場合のみ)
    parent: Option<PathBuf>,
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
    /// 選択中(キーボードフォーカス)の行。VS Code のエクスプローラー選択に相当。
    selected: Option<PathBuf>,
    /// ツリーがキーボード操作の対象か(最後のクリックがツリー内だったか)。
    focused: bool,
    /// 内部クリップボード (パス, 切り取りか)。VS Code の filesExplorer.copy/cut。
    clipboard: Option<(PathBuf, bool)>,
    /// 次の描画でこの行を可視位置までスクロールする。
    scroll_to: Option<PathBuf>,
    /// タイプアヘッド(文字入力で行へジャンプ)のバッファと最終入力時刻。
    type_buf: String,
    type_at: f64,
    /// 今フレーム、行のコンテキストメニューが開いていたか(フォーカス維持用)。
    menu_open: bool,
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
            selected: None,
            focused: false,
            clipboard: None,
            scroll_to: None,
            type_buf: String::new(),
            type_at: 0.0,
            menu_open: false,
        }
    }

    pub fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.roots = roots;
        self.cache.clear();
        self.mtimes.clear();
        self.edit = None;
        self.selected = None;
        self.clipboard = None;
        self.scroll_to = None;
    }

    /// エクスプローラーへキーボードフォーカスを移す (VS Code: ⌘⇧E)。
    pub fn focus(&mut self) {
        self.focused = true;
    }

    /// 外部(アプリ側)から選択を移す。次フレームで見える位置までスクロールする。
    pub fn select(&mut self, p: &Path) {
        self.selected = Some(p.to_path_buf());
        self.scroll_to = Some(p.to_path_buf());
    }

    /// `p` 配下(自身含む)を指していた選択・クリップボードを外す(削除後の後始末)。
    pub fn deselect_under(&mut self, p: &Path) {
        if self.selected.as_deref().is_some_and(|s| s.starts_with(p)) {
            self.selected = None;
        }
        if self.clipboard.as_ref().is_some_and(|(c, _)| c.starts_with(p)) {
            self.clipboard = None;
        }
    }

    /// 新規作成の対象ディレクトリ(VS Code 同様、選択中の場所を優先)。
    pub fn new_entry_dir(&self) -> PathBuf {
        match self.selected.as_deref() {
            Some(p) if p.is_dir() => p.to_path_buf(),
            Some(p) => p
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.fallback_root()),
            None => self.fallback_root(),
        }
    }

    fn fallback_root(&self) -> PathBuf {
        self.roots
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// `p` を含むルート(最長一致)。どのルートにも属さなければ None。
    pub fn root_for(&self, p: &Path) -> Option<&Path> {
        root_for(&self.roots, p)
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
        self.menu_open = false;
        let ctx = ui.ctx().clone();
        // 描画前に可視行のスナップショットを取り、キーボード操作を先に処理する
        // (選択の移動・開閉が同じフレームの描画へ反映される)。
        let rows = self.visible_rows(&ctx);
        self.handle_keys(ui, actions, &rows);

        let roots = self.roots.clone();
        // 単一ルート時は従来どおりヘッダ無しで直下を描く(見た目を変えない)。
        if roots.len() <= 1 {
            let root = roots.into_iter().next().unwrap_or_else(|| PathBuf::from("."));
            self.dir_ui(ui, &root, theme, actions, 0);
        } else {
            for root in &roots {
                let name = root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| root.to_string_lossy().to_string());
                let sel = self.selected.as_deref() == Some(root.as_path());
                let st = CollapsingState::load_with_default_open(&ctx, dir_state_id(root), true);
                let hr = st.show_header(ui, |ui| {
                    ui.selectable_label(
                        sel,
                        RichText::new(format!("📚 {name}")).color(theme.text).strong(),
                    )
                });
                let (_, header, _) = hr.body(|ui| self.dir_ui(ui, root, theme, actions, 0));
                let resp = header.inner;
                if resp.clicked() {
                    self.select(root);
                    self.focused = true;
                    toggle_open(&ctx, root, true);
                }
                if resp.secondary_clicked() {
                    self.select(root);
                    self.focused = true;
                }
                self.maybe_scroll(&resp, root);
                resp.context_menu(|ui| {
                    self.menu_open = true;
                    if menu_btn(ui, tr("➕ 新規ファイル"), "") {
                        self.start_new_file(root.clone());
                    }
                    if menu_btn(ui, tr("📂 新規フォルダ"), "") {
                        self.start_new_dir(root.clone());
                    }
                    ui.separator();
                    let can_paste = self.clipboard.is_some();
                    if menu_btn_enabled(ui, can_paste, tr("📋 貼り付け"), h("⌘V", "Ctrl+V")) {
                        self.paste_into(root.clone(), actions);
                    }
                    ui.separator();
                    if menu_btn(ui, tr("📋 フルパスをコピー"), h("⌥⌘C", "Shift+Alt+C")) {
                        ui.ctx().copy_text(root.to_string_lossy().to_string());
                    }
                });
            }
        }

        // フォーカスの出入り: ツリー(スクロール領域)内クリックで得て、外クリックで手放す。
        // コンテキストメニューはスクロール領域の外に描かれるため、メニュー操作中は保つ。
        if ui.input(|i| i.pointer.any_pressed()) {
            if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                if ui.clip_rect().contains(pos) {
                    self.focused = true;
                } else if !self.menu_open {
                    self.focused = false;
                }
            }
        }
        // スクロール要求はこのフレームで消化(行が見つからなくても持ち越さない)
        self.scroll_to = None;
    }

    /// 可視行(描画順)のスナップショットを作る。開閉状態は egui 側の
    /// CollapsingState を参照する。
    fn visible_rows(&mut self, ctx: &egui::Context) -> Vec<Row> {
        let mut rows = Vec::new();
        let roots = self.roots.clone();
        if roots.len() <= 1 {
            if let Some(root) = roots.first() {
                self.collect_rows(ctx, root, None, &mut rows, 0);
            }
        } else {
            for root in &roots {
                let open = is_open(ctx, root, true);
                rows.push(Row {
                    path: root.clone(),
                    name: root
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| root.to_string_lossy().to_string()),
                    is_dir: true,
                    open,
                    parent: None,
                });
                if open {
                    self.collect_rows(ctx, root, Some(root), &mut rows, 0);
                }
            }
        }
        rows
    }

    fn collect_rows(
        &mut self,
        ctx: &egui::Context,
        dir: &Path,
        parent: Option<&Path>,
        rows: &mut Vec<Row>,
        depth: usize,
    ) {
        if depth > 24 {
            return;
        }
        for e in self.entries(dir) {
            let open = e.is_dir && is_open(ctx, &e.path, false);
            rows.push(Row {
                path: e.path.clone(),
                name: e.name.clone(),
                is_dir: e.is_dir,
                open,
                parent: parent.map(Path::to_path_buf),
            });
            if open {
                self.collect_rows(ctx, &e.path, Some(&e.path), rows, depth + 1);
            }
        }
    }

    /// VS Code エクスプローラー準拠のキーボード操作。
    /// テキスト入力等が egui フォーカスを持っている間は一切奪わない。
    fn handle_keys(&mut self, ui: &mut egui::Ui, actions: &mut TreeActions, rows: &[Row]) {
        if !self.focused || self.edit.is_some() {
            return;
        }
        if ui.ctx().memory(|m| m.focused().is_some()) {
            return;
        }
        if rows.is_empty() {
            return;
        }
        let mac = cfg!(target_os = "macos");
        let ctx = ui.ctx().clone();
        let sel_idx = self
            .selected
            .as_ref()
            .and_then(|s| rows.iter().position(|r| &r.path == s));
        // タイプアヘッド用の文字は消費前に読む(修飾キー付きは Text にならない)
        let (typed, now) = ui.input(|i| {
            let t: String = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect();
            (t, i.time)
        });
        self.keys_navigate(ui, rows, sel_idx, &ctx, mac);
        self.keys_open_rename(ui, actions, rows, sel_idx, &ctx, mac);
        self.keys_clipboard_delete(ui, actions, rows, sel_idx, &ctx, mac);
        self.keys_type_ahead(rows, sel_idx, &typed, now);
    }

    /// handle_keys のナビゲーション部 (list.focusUp/Down/First/Last, collapse/expand)。
    fn keys_navigate(
        &mut self,
        ui: &mut egui::Ui,
        rows: &[Row],
        sel_idx: Option<usize>,
        ctx: &egui::Context,
        mac: bool,
    ) {
        let pressed = |m: Modifiers, k: Key| ui.input_mut(|i| i.consume_key(m, k));
        let roots = self.roots.clone();
        let is_root = move |p: &Path| roots.iter().any(|r| r == p);

        // ── ナビゲーション (list.focusUp/Down/First/Last) ──
        let mut go: Option<usize> = None;
        if pressed(Modifiers::NONE, Key::ArrowDown) {
            go = Some(sel_idx.map(|i| (i + 1).min(rows.len() - 1)).unwrap_or(0));
        }
        if pressed(Modifiers::NONE, Key::ArrowUp) {
            go = Some(sel_idx.map(|i| i.saturating_sub(1)).unwrap_or(rows.len() - 1));
        }
        if pressed(Modifiers::NONE, Key::Home) {
            go = Some(0);
        }
        if pressed(Modifiers::NONE, Key::End) {
            go = Some(rows.len() - 1);
        }
        if let Some(i) = go {
            self.select(&rows[i].path);
        }

        // ── ← : 折りたたみ / 親へ (list.collapse)。→ : 展開 / 最初の子へ (list.expand) ──
        if pressed(Modifiers::NONE, Key::ArrowLeft) || (mac && pressed(Modifiers::COMMAND, Key::ArrowUp)) {
            if let Some(r) = sel_idx.map(|i| &rows[i]) {
                if r.is_dir && r.open {
                    set_open(ctx, &r.path, false);
                } else if let Some(p) = r.parent.clone() {
                    self.select(&p);
                }
            }
        }
        if pressed(Modifiers::NONE, Key::ArrowRight) {
            if let Some(r) = sel_idx.map(|i| &rows[i]) {
                if r.is_dir && !r.open {
                    set_open(ctx, &r.path, true);
                } else if r.is_dir && r.open {
                    let child = rows
                        .iter()
                        .find(|c| c.parent.as_deref() == Some(r.path.as_path()));
                    if let Some(c) = child {
                        let p = c.path.clone();
                        self.select(&p);
                    }
                }
            }
        }
        // 全折りたたみ (list.collapseAll): Ctrl+← / ⌘←
        if pressed(Modifiers::COMMAND, Key::ArrowLeft) {
            for r in rows.iter().filter(|r| r.is_dir && r.open) {
                // マルチルートのルート見出しは開いたままにする(VS Code と同じ)
                if !is_root(&r.path) {
                    set_open(ctx, &r.path, false);
                }
            }
        }
    }

    /// handle_keys の開く/リネーム部 (renameFile, openAndPassFocus, list.toggleExpand)。
    fn keys_open_rename(
        &mut self,
        ui: &mut egui::Ui,
        actions: &mut TreeActions,
        rows: &[Row],
        sel_idx: Option<usize>,
        ctx: &egui::Context,
        mac: bool,
    ) {
        let pressed = |m: Modifiers, k: Key| ui.input_mut(|i| i.consume_key(m, k));
        let roots = self.roots.clone();
        let is_root = move |p: &Path| roots.iter().any(|r| r == p);

        // ── 開く/リネーム (renameFile: F2 / mac Enter, openAndPassFocus: Enter / ⌘↓) ──
        let open_or_toggle = |r: &Row, actions: &mut TreeActions, ctx: &egui::Context| {
            if r.is_dir {
                toggle_open(ctx, &r.path, is_root(&r.path));
            } else {
                actions.open = Some(r.path.clone());
            }
        };
        if pressed(Modifiers::NONE, Key::Enter) {
            if let Some(r) = sel_idx.map(|i| &rows[i]) {
                if mac {
                    // macOS: Enter は名前の変更 (ルートは対象外)
                    if !is_root(&r.path) {
                        self.start_rename(r.path.clone());
                    }
                } else {
                    open_or_toggle(r, actions, ctx);
                }
            }
        }
        if !mac && pressed(Modifiers::NONE, Key::F2) {
            if let Some(r) = sel_idx.map(|i| &rows[i]) {
                if !is_root(&r.path) {
                    self.start_rename(r.path.clone());
                }
            }
        }
        if mac && pressed(Modifiers::COMMAND, Key::ArrowDown) {
            if let Some(r) = sel_idx.map(|i| &rows[i]) {
                open_or_toggle(r, actions, ctx);
            }
        }
        // Space: ファイルはフォーカスを保ったまま開く / フォルダは開閉 (list.toggleExpand)
        if pressed(Modifiers::NONE, Key::Space) && self.type_buf.is_empty() {
            if let Some(r) = sel_idx.map(|i| &rows[i]) {
                open_or_toggle(r, actions, ctx);
            }
        }
    }

    /// handle_keys のクリップボード/パスコピー/削除部。
    fn keys_clipboard_delete(
        &mut self,
        ui: &mut egui::Ui,
        actions: &mut TreeActions,
        rows: &[Row],
        sel_idx: Option<usize>,
        ctx: &egui::Context,
        mac: bool,
    ) {
        let pressed = |m: Modifiers, k: Key| ui.input_mut(|i| i.consume_key(m, k));
        let roots = self.roots.clone();
        let is_root = move |p: &Path| roots.iter().any(|r| r == p);

        // ── クリップボード (filesExplorer.copy/cut/paste) ──
        if pressed(Modifiers::COMMAND, Key::C) {
            if let Some(r) = sel_idx.map(|i| &rows[i]) {
                if !is_root(&r.path) {
                    self.clipboard = Some((r.path.clone(), false));
                }
            }
        }
        if pressed(Modifiers::COMMAND, Key::X) {
            if let Some(r) = sel_idx.map(|i| &rows[i]) {
                if !is_root(&r.path) {
                    self.clipboard = Some((r.path.clone(), true));
                }
            }
        }
        if pressed(Modifiers::COMMAND, Key::V) {
            let dest = self.paste_dest_dir(rows, sel_idx);
            self.paste_into(dest, actions);
        }
        // Escape: 切り取りの取り消し (filesExplorer.cancelCut)
        if matches!(self.clipboard, Some((_, true))) && pressed(Modifiers::NONE, Key::Escape) {
            self.clipboard = None;
        }

        // ── パスのコピー (copyFilePath: ⌥⌘C / Shift+Alt+C,
        //    copyRelativeFilePath mac: ⇧⌥⌘C。Windows はコード系のため menu のみ) ──
        let copy_path = if mac {
            pressed(Modifiers::COMMAND.plus(Modifiers::ALT), Key::C)
        } else {
            pressed(Modifiers::SHIFT.plus(Modifiers::ALT), Key::C)
        };
        if copy_path {
            if let Some(r) = sel_idx.map(|i| &rows[i]) {
                ctx.copy_text(r.path.to_string_lossy().to_string());
            }
        }
        if mac
            && pressed(
                Modifiers::COMMAND.plus(Modifiers::ALT).plus(Modifiers::SHIFT),
                Key::C,
            )
        {
            if let Some(r) = sel_idx.map(|i| &rows[i]) {
                let rel = self.rel_of(&r.path);
                ctx.copy_text(rel);
            }
        }

        // ── 削除 (moveFileToTrash / deleteFile — アプリ側で確認ダイアログ) ──
        let del = if mac {
            pressed(Modifiers::COMMAND, Key::Backspace)
                || pressed(Modifiers::COMMAND.plus(Modifiers::ALT), Key::Backspace)
                || pressed(Modifiers::NONE, Key::Delete)
        } else {
            pressed(Modifiers::NONE, Key::Delete) || pressed(Modifiers::SHIFT, Key::Delete)
        };
        if del {
            if let Some(r) = sel_idx.map(|i| &rows[i]) {
                if !is_root(&r.path) {
                    actions.delete = Some(r.path.clone());
                }
            }
        }
    }

    /// handle_keys のタイプアヘッド部: 文字入力で名前が前方一致する行へジャンプ。
    fn keys_type_ahead(&mut self, rows: &[Row], sel_idx: Option<usize>, typed: &str, now: f64) {
        // ── タイプアヘッド: 文字入力で名前が前方一致する行へジャンプ ──
        let typed: String = typed.chars().filter(|c| !c.is_control()).collect();
        if !typed.trim().is_empty() {
            if now - self.type_at > 1.2 {
                self.type_buf.clear();
            }
            self.type_at = now;
            self.type_buf.push_str(&typed.to_lowercase());
            let start = sel_idx.unwrap_or(0);
            let hit = (0..rows.len())
                .map(|k| (start + k) % rows.len())
                .find(|&i| rows[i].name.to_lowercase().starts_with(&self.type_buf));
            if let Some(i) = hit {
                let p = rows[i].path.clone();
                self.select(&p);
            }
        } else if now - self.type_at > 1.2 {
            self.type_buf.clear();
        }
    }

    /// キーボード貼り付けの宛先: 選択がフォルダならその中、ファイルなら親、無選択なら primary。
    fn paste_dest_dir(&self, rows: &[Row], sel_idx: Option<usize>) -> PathBuf {
        match sel_idx.map(|i| &rows[i]) {
            Some(r) if r.is_dir => r.path.clone(),
            Some(r) => r
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.fallback_root()),
            None => self.fallback_root(),
        }
    }

    /// クリップボードの内容を `dest_dir` へ貼り付ける(実 fs 操作は actions 経由で呼び出し側)。
    fn paste_into(&mut self, dest_dir: PathBuf, actions: &mut TreeActions) {
        let Some((src, cut)) = self.clipboard.clone() else {
            return;
        };
        match paste_plan(&src, cut, &dest_dir) {
            Ok(None) => {}
            Ok(Some((dest, kind))) => {
                actions.transfer = Some((src, dest, kind));
                if cut {
                    self.clipboard = None;
                }
            }
            Err(msg) => actions.notice = Some(msg),
        }
    }

    /// そのパスを含むルートからの相対パス(どのルートにも属さなければフルパス)。
    fn rel_of(&self, p: &Path) -> String {
        self.root_for(p)
            .and_then(|r| p.strip_prefix(r).ok())
            .unwrap_or(p)
            .to_string_lossy()
            .to_string()
    }

    /// 選択行がスクロール外に出ていたら見える位置まで運ぶ。
    fn maybe_scroll(&self, resp: &egui::Response, path: &Path) {
        if self.scroll_to.as_deref() == Some(path) {
            resp.scroll_to_me(None);
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
                    .hint_text(tr("名前を入力して Enter")),
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
                                // 成否はアプリ側で判るが、成功時に選択が付いて
                                // くるよう先に移しておく(失敗時は無害)
                                self.selected = Some(new_path.clone());
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
            let sel = self.selected.as_deref() == Some(e.path.as_path());
            // 切り取り待ちの項目は薄く描く(VS Code と同じ合図)
            let cut_pending =
                matches!(&self.clipboard, Some((p, true)) if p == &e.path);
            let color = if cut_pending {
                theme.text.gamma_multiply(0.5)
            } else {
                theme.text
            };
            if e.is_dir {
                let ctx = ui.ctx().clone();
                let mut st =
                    CollapsingState::load_with_default_open(&ctx, dir_state_id(&e.path), false);
                // 新規作成の入力を出している間は対象フォルダを強制的に開く
                if self.editing_new_in(&e.path) && !st.is_open() {
                    st.set_open(true);
                    st.store(&ctx);
                }
                let hr = st.show_header(ui, |ui| {
                    ui.selectable_label(sel, RichText::new(format!("📁 {}", e.name)).color(color))
                });
                let (_, header, _) =
                    hr.body(|ui| self.dir_ui(ui, &e.path, theme, actions, depth + 1));
                let resp = header.inner;
                if resp.clicked() {
                    // VS Code: フォルダのクリックは選択 + 開閉
                    self.select(&e.path);
                    self.scroll_to = None; // クリック行は既に見えている
                    self.focused = true;
                    toggle_open(&ctx, &e.path, false);
                }
                if resp.secondary_clicked() {
                    self.select(&e.path);
                    self.scroll_to = None;
                    self.focused = true;
                }
                self.maybe_scroll(&resp, &e.path);
                resp.context_menu(|ui| {
                    self.menu_open = true;
                    if menu_btn(ui, tr("➕ 新規ファイル"), "") {
                        self.start_new_file(e.path.clone());
                    }
                    if menu_btn(ui, tr("📂 新規フォルダ"), "") {
                        self.start_new_dir(e.path.clone());
                    }
                    ui.separator();
                    self.clipboard_menu(ui, &e.path, e.path.clone(), actions);
                    ui.separator();
                    self.path_menu(ui, &e.path, actions);
                    ui.separator();
                    if menu_btn(ui, tr("✏ 名前を変更"), h("Enter", "F2")) {
                        self.start_rename(e.path.clone());
                    }
                    if menu_btn(ui, tr("🗑 削除…"), h("⌘⌫", "Delete")) {
                        actions.delete = Some(e.path.clone());
                    }
                });
            } else {
                let label = format!("{} {}", icon_for(&e.name), e.name);
                let resp = ui.selectable_label(sel, RichText::new(label).color(color));
                // エージェントのターミナルへドラッグ&ドロップでパスを渡せる
                // (クリック=開く はそのまま。ドラッグとクリックは egui が排他にする)
                let resp = resp.interact(egui::Sense::click_and_drag());
                resp.dnd_set_drag_payload(e.path.clone());
                if resp.clicked() {
                    self.select(&e.path);
                    self.scroll_to = None;
                    self.focused = true;
                    actions.open = Some(e.path.clone());
                }
                if resp.secondary_clicked() {
                    self.select(&e.path);
                    self.scroll_to = None;
                    self.focused = true;
                }
                self.maybe_scroll(&resp, &e.path);
                resp.context_menu(|ui| {
                    self.menu_open = true;
                    if menu_btn(ui, tr("📂 エディタで開く"), h("⌘↓", "Enter")) {
                        actions.open = Some(e.path.clone());
                    }
                    ui.separator();
                    let parent = e
                        .path
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| self.fallback_root());
                    self.clipboard_menu(ui, &e.path, parent, actions);
                    ui.separator();
                    self.path_menu(ui, &e.path, actions);
                    ui.separator();
                    if menu_btn(ui, tr("✏ 名前を変更"), h("Enter", "F2")) {
                        self.start_rename(e.path.clone());
                    }
                    if menu_btn(ui, tr("🗑 削除…"), h("⌘⌫", "Delete")) {
                        actions.delete = Some(e.path.clone());
                    }
                });
            }
        }
    }

    /// 切り取り / コピー / 貼り付け のメニュー節。`paste_dir` は貼り付け先。
    fn clipboard_menu(
        &mut self,
        ui: &mut egui::Ui,
        path: &Path,
        paste_dir: PathBuf,
        actions: &mut TreeActions,
    ) {
        if menu_btn(ui, tr("✂ 切り取り"), h("⌘X", "Ctrl+X")) {
            self.clipboard = Some((path.to_path_buf(), true));
            self.focused = true;
        }
        if menu_btn(ui, tr("📄 コピー"), h("⌘C", "Ctrl+C")) {
            self.clipboard = Some((path.to_path_buf(), false));
            self.focused = true;
        }
        let can_paste = self.clipboard.is_some();
        if menu_btn_enabled(ui, can_paste, tr("📋 貼り付け"), h("⌘V", "Ctrl+V")) {
            self.paste_into(paste_dir, actions);
            self.focused = true;
        }
    }

    /// パスのコピー / エージェント送信 のメニュー節。
    fn path_menu(&mut self, ui: &mut egui::Ui, path: &Path, actions: &mut TreeActions) {
        if menu_btn(ui, tr("📋 フルパスをコピー"), h("⌥⌘C", "Shift+Alt+C")) {
            ui.ctx().copy_text(path.to_string_lossy().to_string());
        }
        if menu_btn(ui, tr("📋 相対パスをコピー"), h("⇧⌥⌘C", "")) {
            let rel = self.rel_of(path);
            ui.ctx().copy_text(rel);
        }
        if menu_btn(ui, tr("👾 パスをエージェントに送信"), "") {
            let rel = self.rel_of(path);
            actions.send_to_agent = Some(format!("@{rel} "));
        }
    }
}

/// メニュー項目(右端にショートカット表示付き)。クリックでメニューを閉じる。
fn menu_btn(ui: &mut egui::Ui, label: String, hint: &str) -> bool {
    menu_btn_enabled(ui, true, label, hint)
}

fn menu_btn_enabled(ui: &mut egui::Ui, enabled: bool, label: String, hint: &str) -> bool {
    let mut b = egui::Button::new(label);
    if !hint.is_empty() {
        b = b.shortcut_text(hint);
    }
    let clicked = ui.add_enabled(enabled, b).clicked();
    if clicked {
        ui.close_menu();
    }
    clicked
}

/// プラットフォーム別のショートカット表示 (mac 表記 / Windows・Linux 表記)。
fn h(mac: &'static str, win: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        mac
    } else {
        win
    }
}

/// フォルダ開閉状態の egui 保存キー。Ui の入れ子に依存しない安定 Id。
fn dir_state_id(path: &Path) -> egui::Id {
    egui::Id::new(("zv-tree-dir", path))
}

fn is_open(ctx: &egui::Context, path: &Path, default: bool) -> bool {
    CollapsingState::load(ctx, dir_state_id(path))
        .map(|s| s.is_open())
        .unwrap_or(default)
}

fn set_open(ctx: &egui::Context, path: &Path, open: bool) {
    let mut st = CollapsingState::load_with_default_open(ctx, dir_state_id(path), open);
    st.set_open(open);
    st.store(ctx);
}

fn toggle_open(ctx: &egui::Context, path: &Path, default: bool) {
    let now = is_open(ctx, path, default);
    set_open(ctx, path, !now);
}

/// 貼り付けの実行計画。`Ok(None)` は何もしない(同じ場所への切り取り貼り付け等)。
/// エラーはそのままユーザーへ見せるメッセージ。
pub fn paste_plan(
    src: &Path,
    cut: bool,
    dest_dir: &Path,
) -> Result<Option<(PathBuf, Transfer)>, String> {
    if !src.exists() {
        return Err(tr("貼り付け元が見つかりません"));
    }
    let Some(name) = src.file_name().map(|n| n.to_string_lossy().to_string()) else {
        return Err(tr("貼り付け元が見つかりません"));
    };
    if src.is_dir() && dest_dir.starts_with(src) {
        return Err(tr("フォルダを自身の中へは貼り付けできません"));
    }
    if cut {
        if src.parent() == Some(dest_dir) {
            return Ok(None); // 同じ場所への移動は VS Code 同様なにもしない
        }
        let dest = dest_dir.join(&name);
        if dest.exists() {
            return Err(trf("既に存在します: {path}", &[("path", name)]));
        }
        return Ok(Some((dest, Transfer::Move)));
    }
    Ok(Some((
        next_paste_path(dest_dir, &name, src.is_dir()),
        Transfer::Copy,
    )))
}

/// VS Code の `explorer.incrementalNaming = "simple"` 準拠の重複回避:
/// `file.ts` → `file copy.ts` → `file copy 2.ts` → …(フォルダは拡張子分割なし)。
pub fn next_paste_path(dest_dir: &Path, src_name: &str, is_dir: bool) -> PathBuf {
    let first = dest_dir.join(src_name);
    if !first.exists() {
        return first;
    }
    let (mut stem, ext) = if is_dir {
        (src_name.to_string(), "")
    } else {
        match src_name.rfind('.') {
            Some(i) if i > 0 => {
                let (s, e) = src_name.split_at(i);
                (s.to_string(), e)
            }
            _ => (src_name.to_string(), ""),
        }
    };
    loop {
        stem = bump_copy_name(&stem);
        let cand = dest_dir.join(format!("{stem}{ext}"));
        if !cand.exists() {
            return cand;
        }
    }
}

/// `x` → `x copy` → `x copy 2` → `x copy 3` …(VS Code の /^(.+ copy)( \d+)?$/ と同じ)。
fn bump_copy_name(stem: &str) -> String {
    if let Some(head) = stem.strip_suffix(" copy") {
        return format!("{head} copy 2");
    }
    if let Some(idx) = stem.rfind(" copy ") {
        let (head, tail) = stem.split_at(idx);
        if let Ok(n) = tail[" copy ".len()..].parse::<u64>() {
            return format!("{head} copy {}", n + 1);
        }
    }
    format!("{stem} copy")
}

/// ファイルは fs::copy、フォルダは再帰コピー。
pub fn copy_recursively(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursively(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// ディレクトリの更新時刻。取得できない(削除された等)場合は None。
fn dir_mtime(dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(dir).and_then(|m| m.modified()).ok()
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
    fn paste_naming_follows_vscode_simple_increment() {
        let dir = unique_temp_dir("zaivern-tree-test", "paste-name");
        std::fs::write(dir.join("a.txt"), "x").expect("write");

        // 衝突なし → そのままの名前
        assert_eq!(next_paste_path(&dir, "b.txt", false), dir.join("b.txt"));
        // 1 回目の衝突 → "a copy.txt"
        assert_eq!(next_paste_path(&dir, "a.txt", false), dir.join("a copy.txt"));
        // "a copy.txt" が既にある → "a copy 2.txt" → "a copy 3.txt"
        std::fs::write(dir.join("a copy.txt"), "x").expect("write");
        assert_eq!(next_paste_path(&dir, "a.txt", false), dir.join("a copy 2.txt"));
        std::fs::write(dir.join("a copy 2.txt"), "x").expect("write");
        assert_eq!(next_paste_path(&dir, "a.txt", false), dir.join("a copy 3.txt"));
        // コピー名自体を貼り付けても "copy copy" にはならない
        assert_eq!(
            next_paste_path(&dir, "a copy.txt", false),
            dir.join("a copy 3.txt")
        );
        // フォルダはドットで分割しない
        std::fs::create_dir(dir.join("v1.2")).expect("mkdir");
        assert_eq!(next_paste_path(&dir, "v1.2", true), dir.join("v1.2 copy"));
        // 隠しファイル(先頭ドット)は拡張子扱いしない
        std::fs::write(dir.join(".env"), "x").expect("write");
        assert_eq!(next_paste_path(&dir, ".env", false), dir.join(".env copy"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paste_plan_rules() {
        let dir = unique_temp_dir("zaivern-tree-test", "paste-plan");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("mkdir");
        let file = dir.join("f.txt");
        std::fs::write(&file, "x").expect("write");

        // コピー: 同じフォルダへは "f copy.txt" が生える
        let plan = paste_plan(&file, false, &dir).expect("plan");
        assert_eq!(plan, Some((dir.join("f copy.txt"), Transfer::Copy)));
        // 切り取り: 同じフォルダへは何もしない
        assert_eq!(paste_plan(&file, true, &dir).expect("plan"), None);
        // 切り取り: 別フォルダへは移動
        assert_eq!(
            paste_plan(&file, true, &sub).expect("plan"),
            Some((sub.join("f.txt"), Transfer::Move))
        );
        // フォルダを自分の中へは貼り付けない
        assert!(paste_plan(&dir, false, &sub).is_err());
        // 消えたソースはエラー
        assert!(paste_plan(&dir.join("gone.txt"), false, &sub).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copy_recursively_copies_nested_tree() {
        let dir = unique_temp_dir("zaivern-tree-test", "copy-rec");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("nest")).expect("mkdir");
        std::fs::write(src.join("a.txt"), "A").expect("write");
        std::fs::write(src.join("nest").join("b.txt"), "B").expect("write");

        let dst = dir.join("dst");
        copy_recursively(&src, &dst).expect("copy");
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "A");
        assert_eq!(
            std::fs::read_to_string(dst.join("nest").join("b.txt")).unwrap(),
            "B"
        );
        // 元は残る
        assert!(src.join("a.txt").exists());

        std::fs::remove_dir_all(&dir).ok();
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
