use std::collections::{HashMap, HashSet};
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
    /// `.gitignore` 等で git が無視する対象か。
    /// 隠す設定なら `entries()` の時点で落ちるので、ここに残るのは
    /// 「薄く表示する」設定のときだけ。
    pub ignored: bool,
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
    /// 削除要求(確認ダイアログは呼び出し側が出す)。**複数選択に対応**するため
    /// 集合で渡す。単一選択なら 1 件だけ入る(従来と同じ見え方)。
    pub delete: Vec<PathBuf>,
    /// 貼り付け (コピー元, 貼り付け先フルパス, 種類)。fs 操作は呼び出し側。
    /// 複数選択のクリップボードに対応するため集合で渡す。
    pub transfer: Vec<(PathBuf, PathBuf, Transfer)>,
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
/// - Windows の `\\?\` 接頭辞は落として素のパスにする ([`crate::pathx`])。
///   ルートはそのままエージェント/ターミナルの作業ディレクトリになるため、
///   verbatim 形式のまま渡すと `cmd.exe` が `C:\Windows` へ落ちてしまう。
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
        let c = crate::pathx::canonical(&p);
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

/// ツリーの選択集合 (VS Code のエクスプローラー同様、複数選択できる)。
///
/// **単一選択のときの挙動は従来どおり**: `set_single` で `items` が 1 件になり、
/// `lead` がその 1 件を指す。既存のキーボード操作・新規作成の基準は
/// すべて `lead` を見るので、複数選択を足しても 1 件のときの動きは変わらない。
#[derive(Default)]
pub struct Selection {
    /// 選択中のパス(クリック順)。
    items: Vec<PathBuf>,
    /// キーボード操作・新規作成の基準になる「最後に触れた」行。
    lead: Option<PathBuf>,
    /// Shift+クリックの範囲起点。
    anchor: Option<PathBuf>,
}

impl Selection {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn contains(&self, p: &Path) -> bool {
        self.items.iter().any(|x| x == p)
    }

    /// キーボード操作の基準 (従来の `selected` 相当)。
    pub fn lead(&self) -> Option<&Path> {
        self.lead.as_deref()
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.items
    }

    /// 単一選択にする (修飾キー無しのクリック / 外部からの `select`)。
    pub fn set_single(&mut self, p: &Path) {
        self.items.clear();
        self.items.push(p.to_path_buf());
        self.lead = Some(p.to_path_buf());
        self.anchor = Some(p.to_path_buf());
    }

    /// ⌘/Ctrl+クリック: 選択に足す / 外す。
    pub fn toggle(&mut self, p: &Path) {
        if let Some(i) = self.items.iter().position(|x| x == p) {
            self.items.remove(i);
            if self.lead.as_deref() == Some(p) {
                self.lead = self.items.last().cloned();
            }
        } else {
            self.items.push(p.to_path_buf());
            self.lead = Some(p.to_path_buf());
        }
        self.anchor = Some(p.to_path_buf());
    }

    /// Shift+クリック: 起点から `to` までを選択する (起点は動かさない)。
    pub fn set_range(&mut self, picked: Vec<PathBuf>, to: &Path) {
        self.items = picked;
        self.lead = Some(to.to_path_buf());
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.lead = None;
        self.anchor = None;
    }

    /// `p` 配下(自身含む)を選択から外す(削除後の後始末)。
    /// 選択中の要素が消えても `lead` / `anchor` が宙に浮かないようにする。
    pub fn remove_under(&mut self, p: &Path) {
        self.items.retain(|x| !x.starts_with(p));
        if self.lead.as_deref().is_some_and(|l| l.starts_with(p)) {
            self.lead = self.items.last().cloned();
        }
        if self.anchor.as_deref().is_some_and(|a| a.starts_with(p)) {
            self.anchor = self.lead.clone();
        }
    }
}

/// Shift+クリックの範囲選択 (純粋関数)。`rows` は描画順のパス列。
///
/// **逆順 (下から上へ Shift+クリック) でも同じ範囲**を返す。
/// どちらかが可視行に無ければ、クリックされた行だけを選ぶ。
pub fn range_select(rows: &[PathBuf], anchor: &Path, to: &Path) -> Vec<PathBuf> {
    let a = rows.iter().position(|r| r == anchor);
    let b = rows.iter().position(|r| r == to);
    match (a, b) {
        (Some(a), Some(b)) => {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            rows[lo..=hi].to_vec()
        }
        _ => vec![to.to_path_buf()],
    }
}

/// 1 ディレクトリの表示件数を決める純粋関数。
///
/// 戻りは `(描く件数, 残り件数)`。`page == 0` は上限なし。
/// 「さらに N 件」を押すたびに `extra_pages` が 1 増え、同じ幅だけ伸びる。
pub fn dir_page(total: usize, page: usize, extra_pages: usize) -> (usize, usize) {
    if page == 0 {
        return (total, 0);
    }
    let shown = total.min(page.saturating_mul(extra_pages.saturating_add(1)));
    (shown, total - shown)
}

/// ツリーの絞り込み結果 (クエリと、描いてよいパスの集合)。
struct FilterHit {
    /// この結果を作ったクエリ(変わったら作り直す)。
    query: String,
    /// 一致した要素とその祖先ディレクトリ。
    keep: HashSet<PathBuf>,
    /// 一致した件数。
    matched: usize,
    /// 走査を予算で打ち切ったか。
    truncated: bool,
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
    /// 選択集合。VS Code のエクスプローラー選択に相当 (⌘/Ctrl+クリックで追加、
    /// Shift+クリックで範囲)。単一選択のときの挙動は従来と同じ。
    sel: Selection,
    /// 直前フレームの可視行(描画順)。Shift+クリックの範囲選択に使う。
    row_paths: Vec<PathBuf>,
    /// ツリーがキーボード操作の対象か(最後のクリックがツリー内だったか)。
    focused: bool,
    /// 内部クリップボード (パス集合, 切り取りか)。VS Code の filesExplorer.copy/cut。
    clipboard: Option<(Vec<PathBuf>, bool)>,
    /// 次の描画でこの行を可視位置までスクロールする。
    scroll_to: Option<PathBuf>,
    /// タイプアヘッド(文字入力で行へジャンプ)のバッファと最終入力時刻。
    type_buf: String,
    type_at: f64,
    /// 今フレーム、行のコンテキストメニューが開いていたか(フォーカス維持用)。
    menu_open: bool,
    /// エディタから通知された現在のアクティブファイル (VS Code の追従表示)。
    active_file: Option<PathBuf>,
    /// 次の描画で reveal (祖先フォルダ展開 + 選択 + スクロール) するパス。
    pending_reveal: Option<PathBuf>,
    /// アクティブファイル追従の ON/OFF。None = egui 永続メモリから未ロード。
    /// config.rs には触れない設計: トグル状態は egui の永続メモリに保存する。
    auto_reveal: Option<bool>,
    /// auto_reveal がトグルされ、永続メモリへ書き戻しが必要。
    auto_reveal_dirty: bool,
    /// 前フレームのウィンドウフォーカス (復帰の立ち上がりで git 再スキャン要求)。
    window_focused: bool,
    /// `.gitignore` の判定器 (設定で無効化できる)。
    ignorer: crate::ignore::Ignorer,
    /// 無視されたファイルを隠さず薄く表示するか。
    dim_ignored: bool,
    /// 1 ディレクトリで一度に描く行数 (0 = 上限なし)。設定から入れる。
    dir_page: usize,
    /// 「さらに N 件」を押した回数 (ディレクトリごと)。
    more_pages: HashMap<PathBuf, usize>,
    /// 絞り込みの走査で辿ってよい最大件数・最大深さ (設定から入れる)。
    scan_budget: usize,
    max_depth: usize,
    /// ツリー上部の絞り込み入力 (`fuzzy` の既存あいまい検索を使う)。
    pub filter: String,
    /// 絞り込みの計算結果 (クエリが変わるまで使い回す)。
    filter_hit: Option<FilterHit>,
    /// 絞り込みの**走査専用**キャッシュ。`cache` と分けているのは、
    /// 走査で読んだ数千階層を `mtimes` に載せると `refresh_if_changed()` が
    /// 毎秒その数だけ stat を撃つことになるため (描画に使う階層だけを
    /// `cache`/`mtimes` に置く、という元の性質を守る)。
    scan_cache: HashMap<PathBuf, Vec<Entry>>,
    /// テスト専用: 実際に `read_dir` を叩いた回数 (キャッシュ効果の計測)。
    #[cfg(test)]
    io_reads: usize,
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
            sel: Selection::default(),
            row_paths: Vec::new(),
            focused: false,
            clipboard: None,
            scroll_to: None,
            type_buf: String::new(),
            type_at: 0.0,
            menu_open: false,
            active_file: None,
            pending_reveal: None,
            auto_reveal: None,
            auto_reveal_dirty: false,
            window_focused: true,
            ignorer: crate::ignore::Ignorer::new(true),
            dim_ignored: false,
            dir_page: crate::config::DEFAULT_TREE_DIR_PAGE,
            more_pages: HashMap::new(),
            scan_budget: crate::config::DEFAULT_INDEX_MAX_FILES,
            max_depth: crate::config::DEFAULT_INDEX_MAX_DEPTH,
            filter: String::new(),
            filter_hit: None,
            scan_cache: HashMap::new(),
            #[cfg(test)]
            io_reads: 0,
        }
    }

    /// 設定 (`.gitignore` の尊重 / 薄表示 / 1 階層の描画上限 / 走査予算) を反映する。
    /// 値が変わったときだけキャッシュを捨てる (毎フレーム呼んでよい)。
    pub fn apply_config(&mut self, cfg: &crate::config::Config) {
        let before = (self.dim_ignored, self.dir_page);
        self.ignorer.set_enabled(cfg.respect_gitignore);
        self.dim_ignored = cfg.respect_gitignore && cfg.dim_ignored_files;
        self.dir_page = cfg.tree_dir_page;
        self.scan_budget = cfg.index_max_files;
        self.max_depth = cfg.index_max_depth;
        if before != (self.dim_ignored, self.dir_page) {
            self.cache.clear();
            self.mtimes.clear();
            self.filter_hit = None;
            self.scan_cache.clear();
        }
    }

    pub fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.roots = roots;
        self.cache.clear();
        self.mtimes.clear();
        self.ignorer.clear();
        self.more_pages.clear();
        self.filter_hit = None;
        self.scan_cache.clear();
        self.edit = None;
        self.sel.clear();
        self.clipboard = None;
        self.scroll_to = None;
        // アクティブファイルは次の set_active_file で改めて reveal し直す
        self.active_file = None;
        self.pending_reveal = None;
    }

    /// エディタ側のアクティブファイルをツリーへ通知する (毎フレーム呼んでよい)。
    /// パスが変わったときだけ 1 回 reveal を予約する (追従 OFF なら描画時に捨てる)。
    pub fn set_active_file(&mut self, path: Option<&Path>) {
        if self.active_file.as_deref() == path {
            return;
        }
        self.active_file = path.map(Path::to_path_buf);
        if let Some(p) = path {
            if self.root_for(p).is_some() {
                self.pending_reveal = Some(p.to_path_buf());
            }
        }
    }

    /// アクティブファイル追従が有効か (未ロード時は既定 ON)。
    pub fn auto_reveal(&self) -> bool {
        self.auto_reveal.unwrap_or(true)
    }

    /// アクティブファイル追従の ON/OFF (次の描画で egui 永続メモリへ保存)。
    /// ON へ切り替えた瞬間に現在のアクティブファイルを reveal する。
    pub fn set_auto_reveal(&mut self, on: bool) {
        self.auto_reveal = Some(on);
        self.auto_reveal_dirty = true;
        if on {
            self.pending_reveal = self.active_file.clone();
        }
    }

    /// 切り取り移動が成功したときに呼ぶ (アプリ側から)。
    pub fn clear_clipboard(&mut self) {
        self.clipboard = None;
    }

    /// エクスプローラーへキーボードフォーカスを移す (VS Code: ⌘⇧E)。
    pub fn focus(&mut self) {
        self.focused = true;
    }

    /// 外部(アプリ側)から選択を移す。次フレームで見える位置までスクロールする。
    /// 常に**単一選択**へ戻す (複数選択は明示的な修飾キー操作でだけ作る)。
    pub fn select(&mut self, p: &Path) {
        self.sel.set_single(p);
        self.scroll_to = Some(p.to_path_buf());
    }

    /// 行クリック時の選択更新 (VS Code と同じ修飾キー規則)。
    /// 修飾キー無し = 単一選択 / ⌘(Ctrl) = 追加・解除 / Shift = 範囲。
    fn click_select(&mut self, p: &Path, mods: egui::Modifiers) {
        if mods.command {
            self.sel.toggle(p);
        } else if mods.shift {
            match self.sel.anchor.clone().or_else(|| self.sel.lead.clone()) {
                Some(a) => {
                    let picked = range_select(&self.row_paths, &a, p);
                    self.sel.set_range(picked, p);
                }
                None => self.sel.set_single(p),
            }
        } else {
            self.sel.set_single(p);
        }
        self.focused = true;
    }

    /// 指定フォルダを**その場で**開いて選択する (ブレッドクラムのフォルダ押下)。
    ///
    /// `set_active_file` の予約と違い、追従トグル (`auto_reveal`) が OFF でも効く
    /// — ユーザーが自分でそのフォルダを押したのだから、その 1 回は必ず開く。
    pub fn reveal_dir(&mut self, ctx: &egui::Context, p: &Path) {
        // ルート自身のときは祖先を辿らない (ワークスペースの外まで開いてしまうため)
        if !self.roots.iter().any(|r| r == p) {
            for anc in reveal_ancestors(&self.roots, p) {
                set_open(ctx, &anc, true);
            }
        }
        set_open(ctx, p, true);
        self.sel.set_single(p);
        self.scroll_to = Some(p.to_path_buf());
    }

    /// `p` 配下(自身含む)を指していた選択・クリップボードを外す(削除後の後始末)。
    pub fn deselect_under(&mut self, p: &Path) {
        self.sel.remove_under(p);
        if let Some((paths, _)) = self.clipboard.as_mut() {
            paths.retain(|c| !c.starts_with(p));
        }
        if self.clipboard.as_ref().is_some_and(|(c, _)| c.is_empty()) {
            self.clipboard = None;
        }
    }

    /// 新規作成の対象ディレクトリ(VS Code 同様、選択中の場所を優先)。
    pub fn new_entry_dir(&self) -> PathBuf {
        match self.sel.lead() {
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
        self.ignorer.clear();
        self.more_pages.clear();
        self.filter_hit = None;
        self.scan_cache.clear();
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
        let v = self.read_entries(dir);
        self.cache.insert(dir.to_path_buf(), v.clone());
        self.mtimes.insert(dir.to_path_buf(), dir_mtime(dir));
        v
    }

    /// 絞り込みの走査用。描画キャッシュを汚さない (mtime 監視も増やさない)。
    fn scan_entries(&mut self, dir: &Path) -> Vec<Entry> {
        if let Some(v) = self.cache.get(dir).or_else(|| self.scan_cache.get(dir)) {
            return v.clone();
        }
        let v = self.read_entries(dir);
        self.scan_cache.insert(dir.to_path_buf(), v.clone());
        v
    }

    /// 1 階層をディスクから読む (隠しファイル / `.gitignore` の判定込み)。
    fn read_entries(&mut self, dir: &Path) -> Vec<Entry> {
        #[cfg(test)]
        {
            self.io_reads += 1;
        }
        // `.gitignore` はこのルート基準で解釈する (マルチルートでも取り違えない)。
        let root = root_for(&self.roots, dir).map(Path::to_path_buf);
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
                let path = e.path();
                let ignored = match &root {
                    Some(r) => self.ignorer.is_ignored(r, &path, is_dir),
                    None => false,
                };
                // 既定は VS Code と同じで「隠す」。薄表示の設定なら残して淡く描く。
                if ignored && !self.dim_ignored {
                    continue;
                }
                v.push(Entry {
                    path,
                    name,
                    is_dir,
                    ignored,
                });
            }
        }
        v.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        v
    }

    /// 絞り込みを掛けたあとの `dir` 直下(描画順)。
    fn shown_entries(&mut self, dir: &Path) -> Vec<Entry> {
        let mut v = self.entries(dir);
        if let Some(f) = &self.filter_hit {
            v.retain(|e| f.keep.contains(&e.path));
        }
        v
    }

    /// `dir` 直下のうち今フレーム描く件数と、畳んだ残り件数。
    fn page_of(&self, dir: &Path, total: usize) -> (usize, usize) {
        let extra = self.more_pages.get(dir).copied().unwrap_or(0);
        dir_page(total, self.dir_page, extra)
    }

    /// 絞り込みを (必要なら) 計算し直し、一致した要素の祖先を展開する。
    ///
    /// クエリが変わるまで結果を使い回すので、毎フレーム走査はしない。
    /// 走査は `scan_budget` 件・`max_depth` 段で打ち切る
    /// (巨大リポジトリでも 1 フレームを食い潰さない)。
    fn recompute_filter(&mut self, ctx: &egui::Context) {
        let q = self.filter.trim().to_string();
        if q.is_empty() {
            self.filter_hit = None;
            self.scan_cache.clear();
            return;
        }
        if self.filter_hit.as_ref().is_some_and(|f| f.query == q) {
            return;
        }
        let pq = crate::fuzzy::PreparedQuery::new(&q);
        let mut keep: HashSet<PathBuf> = HashSet::new();
        let mut open_dirs: HashSet<PathBuf> = HashSet::new();
        let mut matched = 0usize;
        let mut visited = 0usize;
        let mut truncated = false;
        let roots = self.roots.clone();
        let mut stack: Vec<(PathBuf, usize)> = roots.iter().map(|r| (r.clone(), 0usize)).collect();
        stack.reverse();
        while let Some((dir, depth)) = stack.pop() {
            if depth >= self.max_depth {
                continue;
            }
            for e in self.scan_entries(&dir) {
                visited += 1;
                if visited > self.scan_budget {
                    truncated = true;
                    break;
                }
                if pq.score(&e.name).is_some() {
                    matched += 1;
                    keep.insert(e.path.clone());
                    // 祖先をたどって「見える道」を作る (ルートまで)
                    for anc in reveal_ancestors(&roots, &e.path) {
                        keep.insert(anc.clone());
                        open_dirs.insert(anc);
                    }
                }
                if e.is_dir {
                    stack.push((e.path.clone(), depth + 1));
                }
            }
            if truncated {
                break;
            }
        }
        for d in &open_dirs {
            set_open(ctx, d, true);
        }
        self.filter_hit = Some(FilterHit {
            query: q,
            keep,
            matched,
            truncated,
        });
    }

    /// ツリー上部の絞り込み入力。一致件数は入力があるときだけ 1 行足す
    /// (空のときは案内行を描かないので高さも取らない)。
    ///
    /// **スクロール領域の外**で呼ぶこと (中に置くとツリーと一緒に流れて
    /// 見えなくなる)。呼び出しは `app.rs` の sidebar_files_ui。
    pub fn filter_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 26.0).max(40.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .id_salt("zv-tree-filter")
                    .hint_text(tr("🔎 ツリーを絞り込み"))
                    .desired_width(w),
            );
            if !self.filter.is_empty() && ui.small_button("✖").clicked() {
                self.filter.clear();
                self.filter_hit = None;
            }
        });
        let Some(f) = &self.filter_hit else {
            return; // 空のときは案内行を出さない (高さも取らない)
        };
        let msg = if f.matched == 0 {
            trf(
                "「{q}」に一致するファイルはありません",
                &[("q", f.query.clone())],
            )
        } else if f.truncated {
            trf(
                "{n} 件一致 (走査を打ち切りました)",
                &[("n", f.matched.to_string())],
            )
        } else {
            trf("{n} 件一致", &[("n", f.matched.to_string())])
        };
        ui.label(RichText::new(msg).small().color(theme.text_dim));
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        gitinfo: &crate::git::GitSet,
        actions: &mut TreeActions,
    ) {
        self.menu_open = false;
        let ctx = ui.ctx().clone();

        // アクティブファイル追従トグル: egui 永続メモリと同期する
        // (config.rs に触れないため、開閉状態と同じ保存先を使う)。
        if self.auto_reveal.is_none() {
            let v = ctx.data_mut(|d| *d.get_persisted_mut_or(auto_reveal_id(), true));
            self.auto_reveal = Some(v);
        }
        if self.auto_reveal_dirty {
            let v = self.auto_reveal();
            ctx.data_mut(|d| d.insert_persisted(auto_reveal_id(), v));
            self.auto_reveal_dirty = false;
        }

        // ウィンドウフォーカス復帰の立ち上がりで git status の再スキャンを要求
        // (外部での commit / checkout / 編集をすぐ反映する。VS Code と同じ契機)。
        let focused_now = ctx.input(|i| i.focused);
        if focused_now && !self.window_focused {
            gitinfo.request_refresh();
        }
        self.window_focused = focused_now;

        // アクティブファイル追従 (VS Code の explorer.autoReveal):
        // 祖先フォルダを展開してから行を選択し、可視位置までスクロールする。
        // 展開は visible_rows より前に行い、同フレームのキー操作にも反映する。
        if let Some(target) = self.pending_reveal.take() {
            if self.auto_reveal() {
                for anc in reveal_ancestors(&self.roots, &target) {
                    set_open(&ctx, &anc, true);
                }
                self.sel.set_single(&target);
                self.scroll_to = Some(target);
            }
        }

        // 絞り込み (クエリが変わったときだけ走査し、一致の祖先を展開する)。
        // 入力欄そのものはスクロール領域の外 (sidebar_files_ui) が描く。
        self.recompute_filter(&ctx);

        // 描画前に可視行のスナップショットを取り、キーボード操作を先に処理する
        // (選択の移動・開閉が同じフレームの描画へ反映される)。
        let rows = self.visible_rows(&ctx);
        self.row_paths = rows.iter().map(|r| r.path.clone()).collect();
        self.handle_keys(ui, actions, &rows);

        let roots = self.roots.clone();
        // 単一ルート時は従来どおりヘッダ無しで直下を描く(見た目を変えない)。
        if roots.len() <= 1 {
            let root = roots
                .into_iter()
                .next()
                .unwrap_or_else(|| PathBuf::from("."));
            self.dir_ui(ui, &root, theme, gitinfo, actions, 0);
        } else {
            for root in &roots {
                let name = root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| root.to_string_lossy().to_string());
                let sel = self.sel.contains(root);
                let st = CollapsingState::load_with_default_open(&ctx, dir_state_id(root), true);
                let (color, badge_text) = if let Some((st_type, count)) = gitinfo.dir_status(root) {
                    let (c, b, _) = git_status_style(st_type, theme);
                    (c, format!(" {b}•{count}"))
                } else {
                    (theme.text, String::new())
                };
                let hr = st.show_header(ui, |ui| {
                    ui.selectable_label(
                        sel,
                        RichText::new(format!("📚 {name}{badge_text}"))
                            .color(color)
                            .strong(),
                    )
                });
                let (_, header, _) =
                    hr.body(|ui| self.dir_ui(ui, root, theme, gitinfo, actions, 0));
                let resp = header.inner;
                if resp.clicked() {
                    let mods = ui.input(|i| i.modifiers);
                    self.click_select(root, mods);
                    toggle_open(&ctx, root, true);
                }
                if resp.secondary_clicked() {
                    if !self.sel.contains(root) {
                        self.select(root);
                    }
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
                    if menu_btn_enabled(ui, can_paste, tr("📋 貼り付け"), h("⌘V", "Ctrl+V"))
                    {
                        self.paste_into(root.clone(), actions);
                    }
                    ui.separator();
                    if menu_btn(ui, tr("📋 フルパスをコピー"), h("⌥⌘C", "Shift+Alt+C"))
                    {
                        ui.ctx().copy_text(root.to_string_lossy().to_string());
                    }
                    ui.separator();
                    self.auto_reveal_menu(ui);
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
        // 描画と同じ上限で切る (キーボードの行と画面の行がずれないように)
        let all = self.shown_entries(dir);
        let (shown, _rest) = self.page_of(dir, all.len());
        for e in all.into_iter().take(shown) {
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
            .sel
            .lead()
            .and_then(|s| rows.iter().position(|r| r.path == s));
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
            go = Some(
                sel_idx
                    .map(|i| i.saturating_sub(1))
                    .unwrap_or(rows.len() - 1),
            );
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
        if pressed(Modifiers::NONE, Key::ArrowLeft)
            || (mac && pressed(Modifiers::COMMAND, Key::ArrowUp))
        {
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
        // 対象は「選択集合のうちルート以外」。単一選択なら 1 件で従来と同じ。
        let targets: Vec<PathBuf> = self
            .sel
            .paths()
            .iter()
            .filter(|p| !is_root(p))
            .cloned()
            .collect();
        if pressed(Modifiers::COMMAND, Key::C) && !targets.is_empty() {
            self.clipboard = Some((targets.clone(), false));
        }
        if pressed(Modifiers::COMMAND, Key::X) && !targets.is_empty() {
            self.clipboard = Some((targets.clone(), true));
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
                Modifiers::COMMAND
                    .plus(Modifiers::ALT)
                    .plus(Modifiers::SHIFT),
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
        if del && !targets.is_empty() {
            actions.delete = targets;
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
        let Some((srcs, cut)) = self.clipboard.clone() else {
            return;
        };
        for src in srcs {
            match paste_plan(&src, cut, &dest_dir) {
                Ok(None) => {}
                Ok(Some((dest, kind))) => {
                    actions.transfer.push((src, dest, kind));
                    // 切り取りのクリップボードはここでは消さない。移動の成否は
                    // アプリ側で判るため、成功時に clear_clipboard() を呼んで
                    // もらう (失敗時に切り取り内容が失われないように)。
                }
                // 1 件だめでも残りは貼り付ける (最後の理由だけ知らせる)
                Err(msg) => actions.notice = Some(msg),
            }
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
                                self.sel.set_single(&new_path);
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
        // 未確定なら編集状態を書き戻す。これを忘れると入力欄が 1 フレームで
        // 消え、ツリーからの新規作成・リネームが一切できなくなる
        // (33c9fe6 のリファクタで消えた回帰の再修正)。
        if !done {
            self.edit = Some(es);
        }
    }
}

/// VS Code の `gitDecoration.*ResourceForeground` 既定色を写した定数。
/// ダーク系テーマ / ライト系テーマで別の色を使う (テーマの `dark` で選択)。
/// 出典: VS Code 既定テーマの Theme Color リファレンス。
mod vscode_git_colors {
    use eframe::egui::Color32;

    // gitDecoration.modifiedResourceForeground
    pub const DARK_MODIFIED: Color32 = Color32::from_rgb(0xE2, 0xC0, 0x8D);
    pub const LIGHT_MODIFIED: Color32 = Color32::from_rgb(0x89, 0x55, 0x03);
    // gitDecoration.addedResourceForeground
    pub const DARK_ADDED: Color32 = Color32::from_rgb(0x81, 0xB8, 0x8B);
    pub const LIGHT_ADDED: Color32 = Color32::from_rgb(0x58, 0x7C, 0x0C);
    // gitDecoration.untrackedResourceForeground
    pub const DARK_UNTRACKED: Color32 = Color32::from_rgb(0x73, 0xC9, 0x91);
    pub const LIGHT_UNTRACKED: Color32 = Color32::from_rgb(0x00, 0x71, 0x00);
    // gitDecoration.deletedResourceForeground
    pub const DARK_DELETED: Color32 = Color32::from_rgb(0xC7, 0x4E, 0x39);
    pub const LIGHT_DELETED: Color32 = Color32::from_rgb(0xAD, 0x07, 0x07);
    // gitDecoration.renamedResourceForeground (VS Code は untracked と同系の緑)
    pub const DARK_RENAMED: Color32 = Color32::from_rgb(0x73, 0xC9, 0x91);
    pub const LIGHT_RENAMED: Color32 = Color32::from_rgb(0x00, 0x71, 0x00);
    // gitDecoration.conflictingResourceForeground
    pub const DARK_CONFLICTING: Color32 = Color32::from_rgb(0xE4, 0x67, 0x6B);
    pub const LIGHT_CONFLICTING: Color32 = Color32::from_rgb(0xAD, 0x07, 0x07);
}

/// FileStatus に対応する VS Code 風カラー (テーマの明暗で切替) と
/// ステータスバッジ文字・ホバー説明。
///
/// ダーク/ライトで別の色を返すので、どちらのテーマでもバッジと
/// フォルダの淡色が背景から浮く (git_panel の PR レビュー表示でも同じ関数を使い、
/// ツリーとレビューで色の意味がずれないようにしている)。
pub(crate) fn git_status_style(
    status: crate::git::FileStatus,
    theme: &Theme,
) -> (egui::Color32, &'static str, &'static str) {
    use crate::git::FileStatus;
    use vscode_git_colors as gc;
    let dark = theme.dark;
    let pick = |d: egui::Color32, l: egui::Color32| if dark { d } else { l };
    match status {
        FileStatus::Modified => (
            pick(gc::DARK_MODIFIED, gc::LIGHT_MODIFIED),
            "M",
            "変更あり (Modified)",
        ),
        FileStatus::Added => (
            pick(gc::DARK_ADDED, gc::LIGHT_ADDED),
            "A",
            "追加済み (Added)",
        ),
        FileStatus::Untracked => (
            pick(gc::DARK_UNTRACKED, gc::LIGHT_UNTRACKED),
            "U",
            "未追跡 (Untracked)",
        ),
        FileStatus::Deleted => (
            pick(gc::DARK_DELETED, gc::LIGHT_DELETED),
            "D",
            "削除済み (Deleted)",
        ),
        FileStatus::Renamed => (
            pick(gc::DARK_RENAMED, gc::LIGHT_RENAMED),
            "R",
            "名前変更 (Renamed)",
        ),
        FileStatus::Conflicted => (
            pick(gc::DARK_CONFLICTING, gc::LIGHT_CONFLICTING),
            "C",
            "コンフリクト (Conflicted)",
        ),
    }
}

impl FileTree {
    fn dir_ui(
        &mut self,
        ui: &mut egui::Ui,
        dir: &Path,
        theme: &Theme,
        gitinfo: &crate::git::GitSet,
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
        let all = self.shown_entries(dir);
        let (shown, rest) = self.page_of(dir, all.len());
        for e in all.into_iter().take(shown) {
            // リネーム中の項目は行ごと入力欄に置き換える
            if self.renaming(&e.path) {
                self.edit_row_ui(ui, actions);
                continue;
            }
            let sel = self.sel.contains(&e.path);
            // 切り取り待ちの項目は薄く描く(VS Code と同じ合図)
            let cut_pending = matches!(&self.clipboard, Some((p, true)) if p.contains(&e.path));
            // git が無視する項目は (薄表示の設定のときだけ現れ) 淡く描く
            let dim = if e.ignored { 0.45_f32 } else { 1.0 };
            if e.is_dir {
                let ctx = ui.ctx().clone();
                let mut st =
                    CollapsingState::load_with_default_open(&ctx, dir_state_id(&e.path), false);
                // 新規作成の入力を出している間は対象フォルダを強制的に開く
                if self.editing_new_in(&e.path) && !st.is_open() {
                    st.set_open(true);
                    st.store(&ctx);
                }

                // 折りたたんだままでも「下で何か変わった」が分かるよう、
                // 配下の変更件数を常にバッジに出す (深さは問わない)。
                let (dir_color, dir_badge, dir_hint) = if cut_pending {
                    (theme.text.gamma_multiply(0.5), String::new(), String::new())
                } else if e.ignored {
                    (
                        theme.text.gamma_multiply(dim),
                        String::new(),
                        tr("git が無視しています (.gitignore)"),
                    )
                } else if let Some((st_type, count)) = gitinfo.dir_status(&e.path) {
                    let (c, b, h) = git_status_style(st_type, theme);
                    (
                        c,
                        format!(" {b}•{count}"),
                        format!(
                            "{}\n{}",
                            trf("配下に {n} 件の変更", &[("n", count.to_string())]),
                            tr(h)
                        ),
                    )
                } else {
                    (theme.text, String::new(), String::new())
                };

                let hr = st.show_header(ui, |ui| {
                    let r = ui.selectable_label(
                        sel,
                        RichText::new(format!("📁 {}{}", e.name, dir_badge)).color(dir_color),
                    );
                    if dir_hint.is_empty() {
                        r
                    } else {
                        r.on_hover_text(&dir_hint)
                    }
                });
                let (_, header, _) =
                    hr.body(|ui| self.dir_ui(ui, &e.path, theme, gitinfo, actions, depth + 1));
                let resp = header.inner;
                if resp.clicked() {
                    // VS Code: フォルダのクリックは選択 + 開閉
                    // (⌘/Ctrl・Shift 付きは選択だけを広げ、開閉しない)
                    let mods = ui.input(|i| i.modifiers);
                    self.click_select(&e.path, mods);
                    self.scroll_to = None; // クリック行は既に見えている
                    if !mods.command && !mods.shift {
                        toggle_open(&ctx, &e.path, false);
                    }
                }
                if resp.secondary_clicked() {
                    if !self.sel.contains(&e.path) {
                        self.select(&e.path);
                    }
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
                        actions.delete = self.delete_targets(&e.path);
                    }
                    ui.separator();
                    self.auto_reveal_menu(ui);
                });
            } else {
                let (file_color, file_badge, hint) = if cut_pending {
                    (theme.text.gamma_multiply(0.5), String::new(), "")
                } else if e.ignored {
                    (
                        theme.text.gamma_multiply(dim),
                        String::new(),
                        "git が無視しています (.gitignore)",
                    )
                } else if let Some(st_type) = gitinfo.file_status(&e.path) {
                    let (c, b, h) = git_status_style(st_type, theme);
                    (c, format!("  {b}"), h)
                } else {
                    (theme.text, String::new(), "")
                };

                let label = format!("{} {}{}", icon_for(&e.name), e.name, file_badge);
                let mut resp = ui.selectable_label(sel, RichText::new(label).color(file_color));
                if !hint.is_empty() {
                    resp = resp.on_hover_text(tr(hint));
                }
                // エージェントのターミナルへドラッグ&ドロップでパスを渡せる
                // (クリック=開く はそのまま。ドラッグとクリックは egui が排他にする)
                let resp = resp.interact(egui::Sense::click_and_drag());
                resp.dnd_set_drag_payload(e.path.clone());
                if resp.clicked() {
                    let mods = ui.input(|i| i.modifiers);
                    self.click_select(&e.path, mods);
                    self.scroll_to = None;
                    // ⌘/Ctrl・Shift 付きは「選ぶ」だけ (まとめて消す/移す前段)
                    if !mods.command && !mods.shift {
                        actions.open = Some(e.path.clone());
                    }
                }
                if resp.secondary_clicked() {
                    if !self.sel.contains(&e.path) {
                        self.select(&e.path);
                    }
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
                        actions.delete = self.delete_targets(&e.path);
                    }
                    ui.separator();
                    self.auto_reveal_menu(ui);
                });
            }
        }
        if rest > 0 {
            // 巨大ディレクトリは全部描かず、押した回数だけ伸ばす
            // (数万行を毎フレーム描いてフレームを落とさないため)
            let label = trf("… さらに {n} 件", &[("n", rest.to_string())]);
            let resp = ui
                .add(egui::Button::new(
                    RichText::new(label).small().color(theme.text_dim),
                ))
                .on_hover_text(trf(
                    "この階層は {n} 件ずつ表示します (設定 tree_dir_page)",
                    &[("n", self.dir_page.to_string())],
                ));
            if resp.clicked() {
                *self.more_pages.entry(dir.to_path_buf()).or_insert(0) += 1;
            }
        }
        self.deleted_ghost_rows(ui, dir, theme, gitinfo);
    }

    /// git 上は削除済みで fs には存在しないファイルを、VS Code のように
    /// 打ち消し線付きの幽霊行として表示する (表示のみ・クリック不可)。
    /// スキャン時に整理済みの「ディレクトリ → 削除ファイル名」の O(1) 参照なので安い。
    fn deleted_ghost_rows(
        &self,
        ui: &mut egui::Ui,
        dir: &Path,
        theme: &Theme,
        gitinfo: &crate::git::GitSet,
    ) {
        let names = gitinfo.deleted_names_in(dir);
        if names.is_empty() {
            return;
        }
        let (color, badge, hint) = git_status_style(crate::git::FileStatus::Deleted, theme);
        for name in names {
            if !self.show_hidden && name.starts_with('.') {
                continue;
            }
            // まれに同名ファイルが再作成済み (D + ?? など) の場合は実体行を優先
            if dir.join(name).exists() {
                continue;
            }
            let text = RichText::new(format!("{} {}  {}", icon_for(name), name, badge))
                .color(color)
                .strikethrough();
            ui.label(text).on_hover_text(tr(hint));
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
        // 右クリックした行が選択に入っていれば選択集合ごと、なければその 1 件。
        let targets: Vec<PathBuf> = if self.sel.contains(path) && self.sel.len() > 1 {
            self.sel.paths().to_vec()
        } else {
            vec![path.to_path_buf()]
        };
        if menu_btn(ui, tr("✂ 切り取り"), h("⌘X", "Ctrl+X")) {
            self.clipboard = Some((targets.clone(), true));
            self.focused = true;
        }
        if menu_btn(ui, tr("📄 コピー"), h("⌘C", "Ctrl+C")) {
            self.clipboard = Some((targets.clone(), false));
            self.focused = true;
        }
        let can_paste = self.clipboard.is_some();
        if menu_btn_enabled(ui, can_paste, tr("📋 貼り付け"), h("⌘V", "Ctrl+V")) {
            self.paste_into(paste_dir, actions);
            self.focused = true;
        }
    }

    /// アクティブファイル追従のトグル (VS Code: explorer.autoReveal 相当)。
    /// 状態は egui の永続メモリに保存する (ui() 冒頭で書き戻し)。
    fn auto_reveal_menu(&mut self, ui: &mut egui::Ui) {
        let on = self.auto_reveal();
        let label = if on {
            tr("🎯 アクティブファイル追従: ON")
        } else {
            tr("🎯 アクティブファイル追従: OFF")
        };
        if menu_btn(ui, label, "") {
            self.set_auto_reveal(!on);
        }
    }

    /// 右クリックされた行に対する削除対象。選択集合に入っていれば
    /// **集合ごと**、入っていなければその 1 件だけ (ルートは常に除く)。
    fn delete_targets(&self, path: &Path) -> Vec<PathBuf> {
        let is_root = |p: &Path| self.roots.iter().any(|r| r == p);
        if self.sel.contains(path) && self.sel.len() > 1 {
            self.sel
                .paths()
                .iter()
                .filter(|p| !is_root(p))
                .cloned()
                .collect()
        } else if is_root(path) {
            Vec::new()
        } else {
            vec![path.to_path_buf()]
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

/// アクティブファイル追従トグルの egui 永続メモリキー。
fn auto_reveal_id() -> egui::Id {
    egui::Id::new("zv-tree-auto-reveal")
}

/// `path` を見えるようにするため展開すべき祖先ディレクトリ一覧
/// (所属ルート含む・ルート側から順・`path` 自身は含まない)。
/// どのルートにも属さなければ空 = reveal しない。
pub(crate) fn reveal_ancestors(roots: &[PathBuf], path: &Path) -> Vec<PathBuf> {
    let Some(root) = root_for(roots, path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cur = path.parent();
    while let Some(d) = cur {
        out.push(d.to_path_buf());
        if d == root {
            break;
        }
        cur = d.parent();
    }
    out.reverse();
    out
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
/// シンボリックリンクは辿らずスキップする (祖先を指すリンクがあると
/// 無限再帰でスタックオーバーフローする)。深さも保険で制限する。
pub fn copy_recursively(src: &Path, dst: &Path) -> std::io::Result<()> {
    copy_recursively_inner(src, dst, 0)
}

fn copy_recursively_inner(src: &Path, dst: &Path, depth: usize) -> std::io::Result<()> {
    if depth > 64 {
        return Err(std::io::Error::other("フォルダが深すぎます (>64)"));
    }
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        // リンクはコピーしない (辿ると循環・意図しない大量コピーの危険)
        return Ok(());
    }
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursively_inner(&e.path(), &dst.join(e.file_name()), depth + 1)?;
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
    // 拡張子は大小無視で判定する (README.MD 等が既定アイコンに落ちないように)
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
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

    /// `.gitignore` を尊重する設定でツリーを作る (既定値をそのまま使う)。
    fn tree_with(root: &Path, cfg: &crate::config::Config) -> FileTree {
        let mut t = FileTree::new(vec![root.to_path_buf()], true);
        t.apply_config(cfg);
        t
    }

    fn names_of(t: &mut FileTree, dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = t.entries(dir).into_iter().map(|e| e.name).collect();
        v.sort();
        v
    }

    // ── .gitignore ────────────────────────────────────────────────

    #[test]
    fn gitignore_hides_generated_dirs_from_the_tree() {
        let root = unique_temp_dir("zaivern-tree-test", "gitignore");
        std::fs::write(
            root.join(".gitignore"),
            "node_modules/\ntarget/\ndist/\nbuild/\n*.log\n",
        )
        .unwrap();
        for d in ["node_modules", "target", "dist", "build", "src"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("a.log"), "x").unwrap();
        std::fs::write(root.join("README.md"), "x").unwrap();

        let cfg = crate::config::Config::default();
        let mut t = tree_with(&root, &cfg);
        assert_eq!(names_of(&mut t, &root), [".gitignore", "README.md", "src"]);

        // 薄く表示する設定なら消えずに残り、`ignored` が立つ
        let dim = crate::config::Config {
            dim_ignored_files: true,
            ..crate::config::Config::default()
        };
        let mut t = tree_with(&root, &dim);
        let entries = t.entries(&root);
        let nm = entries
            .iter()
            .find(|e| e.name == "node_modules")
            .expect("薄表示なら残る");
        assert!(nm.ignored, "無視対象の印が立つ");
        assert!(
            entries.iter().any(|e| e.name == "src" && !e.ignored),
            "無視されないものには印が付かない"
        );

        // 設定で切れば全部出る
        let off = crate::config::Config {
            respect_gitignore: false,
            ..crate::config::Config::default()
        };
        let mut t = tree_with(&root, &off);
        assert!(names_of(&mut t, &root).contains(&"node_modules".to_string()));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn nested_gitignore_can_re_include_in_the_tree() {
        let root = unique_temp_dir("zaivern-tree-test", "nested");
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(root.join("logs").join(".gitignore"), "!important.log\n").unwrap();
        std::fs::write(root.join("logs").join("important.log"), "x").unwrap();
        std::fs::write(root.join("logs").join("noise.log"), "x").unwrap();

        let cfg = crate::config::Config::default();
        let mut t = tree_with(&root, &cfg);
        assert_eq!(
            names_of(&mut t, &root.join("logs")),
            [".gitignore", "important.log"]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    // ── 複数選択 ──────────────────────────────────────────────────

    #[test]
    fn range_select_is_the_same_in_both_directions() {
        let rows: Vec<PathBuf> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let down = range_select(&rows, Path::new("b"), Path::new("d"));
        let up = range_select(&rows, Path::new("d"), Path::new("b"));
        assert_eq!(down, up, "下から上へ Shift+クリックしても同じ範囲");
        assert_eq!(down, rows[1..=3].to_vec());
        // 同じ行なら 1 件
        assert_eq!(
            range_select(&rows, Path::new("c"), Path::new("c")),
            vec![PathBuf::from("c")]
        );
        // 起点が可視行に無ければクリックされた行だけ
        assert_eq!(
            range_select(&rows, Path::new("zzz"), Path::new("c")),
            vec![PathBuf::from("c")]
        );
        assert_eq!(range_select(&[], Path::new("a"), Path::new("b")).len(), 1);
    }

    #[test]
    fn selection_single_click_behaviour_is_unchanged() {
        let mut sel = Selection::default();
        sel.set_single(Path::new("a"));
        assert_eq!(sel.len(), 1);
        assert_eq!(sel.lead(), Some(Path::new("a")));
        // 別の行を単純クリックすると入れ替わる (従来の Option<PathBuf> と同じ)
        sel.set_single(Path::new("b"));
        assert_eq!(sel.paths(), [PathBuf::from("b")]);
        assert!(!sel.contains(Path::new("a")));
    }

    #[test]
    fn selection_toggles_and_mixes_dirs_and_files() {
        let mut sel = Selection::default();
        sel.set_single(Path::new("w-src"));
        sel.toggle(Path::new("w-README.md"));
        sel.toggle(Path::new("w-docs"));
        assert_eq!(sel.len(), 3, "ディレクトリとファイルを混ぜて選べる");
        assert!(sel.contains(Path::new("w-src")));
        assert!(sel.contains(Path::new("w-README.md")));
        // もう一度 ⌘クリックすると外れる
        sel.toggle(Path::new("w-README.md"));
        assert_eq!(sel.len(), 2);
        assert!(!sel.contains(Path::new("w-README.md")));
        assert_eq!(sel.lead(), Some(Path::new("w-docs")));
    }

    #[test]
    fn selection_stays_consistent_when_selected_items_are_deleted() {
        let base = unique_temp_dir("zaivern-tree-test", "sel-del");
        let mut sel = Selection::default();
        sel.set_single(&base.join("a"));
        sel.toggle(&base.join("b").join("child.txt"));
        sel.toggle(&base.join("c"));
        // `b` フォルダごと消える → 配下の選択も消え、lead は生き残りへ移る
        sel.remove_under(&base.join("b"));
        assert_eq!(sel.len(), 2);
        assert!(!sel.contains(&base.join("b").join("child.txt")));
        assert_eq!(sel.lead(), Some(base.join("c").as_path()));
        // 全部消えたら空 (宙に浮いた lead を残さない)
        sel.remove_under(&base);
        assert_eq!(sel.len(), 0);
        assert_eq!(sel.lead(), None);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn deselect_under_also_prunes_the_clipboard() {
        let root = unique_temp_dir("zaivern-tree-test", "deselect");
        let mut t = FileTree::new(vec![root.clone()], true);
        t.sel.set_single(&root.join("a"));
        t.sel.toggle(&root.join("b"));
        t.clipboard = Some((vec![root.join("a"), root.join("b")], true));
        t.deselect_under(&root.join("a"));
        assert_eq!(t.sel.paths(), [root.join("b")]);
        assert_eq!(
            t.clipboard.as_ref().map(|(v, _)| v.clone()),
            Some(vec![root.join("b")])
        );
        // 最後の 1 件も消えたらクリップボードごと空にする
        t.deselect_under(&root.join("b"));
        assert!(t.clipboard.is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    // ── 1 ディレクトリの描画上限 ────────────────────────────────

    #[test]
    fn dir_page_table() {
        // (総数, 1 ページ, 「さらに」を押した回数) → (描く件数, 残り)
        let cases: &[(usize, usize, usize, usize, usize)] = &[
            (10, 300, 0, 10, 0),
            (1000, 300, 0, 300, 700),
            (1000, 300, 1, 600, 400),
            (1000, 300, 3, 1000, 0),
            (1000, 0, 0, 1000, 0), // 0 = 上限なし
            (0, 300, 0, 0, 0),
        ];
        for (total, page, extra, shown, rest) in cases {
            assert_eq!(
                dir_page(*total, *page, *extra),
                (*shown, *rest),
                "total={total} page={page} extra={extra}"
            );
        }
        // 押しすぎても総数を超えない / 桁あふれしない
        assert_eq!(dir_page(5, 300, usize::MAX), (5, 0));
    }

    /// 1 万エントリのディレクトリでも (1) 描く行は上限で頭打ち、
    /// (2) `read_dir` は 1 回しか叩かない (キャッシュが効く) こと。
    /// 実時間の assert はフレーキーなので**回数**で固定する。
    #[test]
    fn huge_directory_is_capped_and_read_once() {
        let root = unique_temp_dir("zaivern-tree-test", "huge");
        const N: usize = 10_000;
        for i in 0..N {
            std::fs::write(root.join(format!("f{i:05}.txt")), "").unwrap();
        }
        let cfg = crate::config::Config::default();
        let page = cfg.tree_dir_page;
        let mut t = tree_with(&root, &cfg);

        let ctx = egui::Context::default();
        let rows = t.visible_rows(&ctx);
        assert_eq!(rows.len(), page, "1 階層は設定の上限まで (全部は描かない)");
        assert_eq!(t.io_reads, 1, "ディスクは 1 回だけ読む");

        // 「さらに N 件」を 1 回押した相当 → ちょうど 1 ページ分伸びる
        *t.more_pages.entry(root.clone()).or_insert(0) += 1;
        let rows = t.visible_rows(&ctx);
        assert_eq!(rows.len(), page * 2);
        assert_eq!(t.io_reads, 1, "再描画で読み直さない");

        // 走査ノード数 = 上限で止まる (残りは「さらに N 件」に畳まれる)
        assert_eq!(dir_page(N, page, 0), (page, N - page));

        std::fs::remove_dir_all(&root).ok();
    }

    // ── 絞り込み ──────────────────────────────────────────────────

    #[test]
    fn filter_keeps_matches_and_their_ancestors() {
        let root = unique_temp_dir("zaivern-tree-test", "filter");
        std::fs::create_dir_all(root.join("src").join("deep")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("src").join("deep").join("widget.rs"), "").unwrap();
        std::fs::write(root.join("src").join("other.rs"), "").unwrap();
        std::fs::write(root.join("docs").join("guide.md"), "").unwrap();

        let cfg = crate::config::Config::default();
        let mut t = tree_with(&root, &cfg);
        let ctx = egui::Context::default();

        t.filter = "widget".into();
        t.recompute_filter(&ctx);
        let hit = t.filter_hit.as_ref().expect("結果ができる");
        assert_eq!(hit.matched, 1);
        assert!(hit
            .keep
            .contains(&root.join("src").join("deep").join("widget.rs")));
        assert!(hit.keep.contains(&root.join("src")), "祖先も残す");
        assert!(hit.keep.contains(&root.join("src").join("deep")));
        assert!(!hit.keep.contains(&root.join("docs")), "無関係な枝は落ちる");
        // 祖先は自動展開される (押さなくても一致が見える)
        assert!(is_open(&ctx, &root.join("src"), false));
        assert!(is_open(&ctx, &root.join("src").join("deep"), false));

        // 一致した行だけが可視行になる (祖先ディレクトリ + 一致)
        let names: Vec<String> = t
            .visible_rows(&ctx)
            .iter()
            .map(|r| r.name.clone())
            .collect();
        assert_eq!(names, ["src", "deep", "widget.rs"]);

        // 空にすると全体へ戻る
        t.filter.clear();
        t.recompute_filter(&ctx);
        assert!(t.filter_hit.is_none(), "空のときは結果を持たない");
        let names: Vec<String> = t
            .visible_rows(&ctx)
            .iter()
            .map(|r| r.name.clone())
            .collect();
        assert!(names.contains(&"docs".to_string()));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn filter_uses_the_existing_fuzzy_matcher() {
        // マッチャは fuzzy.rs のものをそのまま使う (新しい実装を書かない)
        let root = unique_temp_dir("zaivern-tree-test", "fuzzy");
        std::fs::write(root.join("main_window.rs"), "").unwrap();
        std::fs::write(root.join("other.txt"), "").unwrap();
        let cfg = crate::config::Config::default();
        let mut t = tree_with(&root, &cfg);
        let ctx = egui::Context::default();
        // 飛び飛びの部分列でも当たる = fuzzy::score と同じ挙動
        t.filter = "mnwin".into();
        t.recompute_filter(&ctx);
        let hit = t.filter_hit.as_ref().expect("結果");
        assert_eq!(hit.matched, 1);
        assert!(hit.keep.contains(&root.join("main_window.rs")));
        assert!(crate::fuzzy::score("mnwin", "main_window.rs").is_some());
        std::fs::remove_dir_all(&root).ok();
    }

    /// 記録済みの mtime を過去へずらし、同一秒内の外部変更でも差が出るようにする
    /// （mtime 粒度が粗いファイルシステムでもテストを決定的にするため）。
    fn backdate_recorded(t: &mut FileTree, dir: &Path) {
        let m = t.mtimes.get_mut(dir).expect("dir is cached");
        *m = m.map(|x| x - Duration::from_secs(2));
    }

    /// 正規化後の比較用（macOS の /tmp → /private/tmp 等を吸収）。
    /// `normalize_roots` と同じ正規化 (Windows の `\\?\` 除去まで) を使う。
    fn canon(p: &Path) -> PathBuf {
        crate::pathx::canonical(p)
    }

    #[test]
    fn normalize_roots_dedups_and_keeps_order() {
        let dir = unique_temp_dir("zaivern-tree-test", "norm-dedup");
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::create_dir_all(&a).expect("mkdir a");
        std::fs::create_dir_all(&b).expect("mkdir b");

        let out = normalize_roots(vec![a.clone(), b.clone(), a.clone()]);
        assert_eq!(
            out,
            vec![canon(&a), canon(&b)],
            "重複は落ち、順序は保たれる"
        );

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

    /// DoD: ルートはそのままエージェント / ターミナルの作業ディレクトリになる。
    /// Windows の `\\?\` 付きパスを渡すと cmd.exe がカレントディレクトリを捨てて
    /// `C:\Windows` で起動してしまうため、ルートには残していけない。
    #[test]
    fn normalize_roots_returns_plain_paths() {
        let dir = unique_temp_dir("zaivern-tree-test", "norm-plain");
        let a = dir.join("a");
        std::fs::create_dir_all(&a).expect("mkdir a");

        for root in normalize_roots(vec![a.clone()]) {
            assert!(
                !root.to_string_lossy().starts_with(r"\\?\"),
                "ルートは素のパス: {}",
                root.display()
            );
            assert!(
                root.is_dir(),
                "指しているものは変わらない: {}",
                root.display()
            );
        }

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
        assert_eq!(
            t.root_for(Path::new("/ws/a/x.rs")),
            Some(Path::new("/ws/a"))
        );
        assert_eq!(
            t.root_for(Path::new("/ws/a/deep/x.rs")),
            Some(Path::new("/ws/a/deep")),
        );
        assert_eq!(t.root_for(Path::new("/elsewhere/x.rs")), None);
        assert_eq!(t.roots[0], PathBuf::from("/ws/a"), "primary は roots[0]");
    }

    #[test]
    fn icon_for_ignores_extension_case() {
        assert_eq!(icon_for("README.MD"), icon_for("readme.md"));
        assert_eq!(icon_for("Main.RS"), icon_for("main.rs"));
        assert_ne!(
            icon_for("README.MD"),
            icon_for("unknown.zzz"),
            "既定に落ちていない"
        );
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
        assert_eq!(
            next_paste_path(&dir, "a.txt", false),
            dir.join("a copy.txt")
        );
        // "a copy.txt" が既にある → "a copy 2.txt" → "a copy 3.txt"
        std::fs::write(dir.join("a copy.txt"), "x").expect("write");
        assert_eq!(
            next_paste_path(&dir, "a.txt", false),
            dir.join("a copy 2.txt")
        );
        std::fs::write(dir.join("a copy 2.txt"), "x").expect("write");
        assert_eq!(
            next_paste_path(&dir, "a.txt", false),
            dir.join("a copy 3.txt")
        );
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

    #[test]
    fn reveal_ancestors_expands_root_to_parent() {
        // 純粋なパス計算なので fs 不要
        let root = PathBuf::from("/ws/project");
        let roots = vec![root.clone()];
        let file = root.join("src").join("deep").join("mod.rs");
        assert_eq!(
            reveal_ancestors(&roots, &file),
            vec![
                root.clone(),
                root.join("src"),
                root.join("src").join("deep")
            ],
            "ルート → 親 の順で、対象自身は含まない"
        );
        // ルート直下のファイルはルートだけ
        assert_eq!(
            reveal_ancestors(&roots, &root.join("main.rs")),
            vec![root.clone()]
        );
    }

    #[test]
    fn reveal_ancestors_outside_roots_is_empty() {
        let roots = vec![PathBuf::from("/ws/a"), PathBuf::from("/ws/b")];
        assert!(reveal_ancestors(&roots, &PathBuf::from("/etc/hosts")).is_empty());
        // マルチルートでは所属ルート (最長一致) 側の祖先だけを返す
        let f = PathBuf::from("/ws/b/x/y.rs");
        assert_eq!(
            reveal_ancestors(&roots, &f),
            vec![PathBuf::from("/ws/b"), PathBuf::from("/ws/b/x")]
        );
    }

    #[test]
    fn set_active_file_queues_reveal_only_on_change() {
        let dir = unique_temp_dir("zaivern-tree-test", "reveal");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("mkdir");
        let f = sub.join("a.rs");
        std::fs::write(&f, "x").expect("write");

        let mut t = FileTree::new(vec![dir.clone()], false);
        t.set_active_file(Some(&f));
        assert_eq!(t.pending_reveal.as_deref(), Some(f.as_path()));

        // 同じパスの再通知では再予約しない (毎フレーム呼ばれる前提)
        t.pending_reveal = None;
        t.set_active_file(Some(&f));
        assert!(t.pending_reveal.is_none());

        // ルート外のパスは reveal 対象にしない
        t.set_active_file(Some(Path::new("/no/such/root.rs")));
        assert!(t.pending_reveal.is_none());

        // OFF → ON でアクティブファイルを reveal し直す
        t.set_active_file(Some(&f));
        t.pending_reveal = None;
        t.set_auto_reveal(false);
        assert!(t.pending_reveal.is_none());
        t.set_auto_reveal(true);
        assert_eq!(t.pending_reveal.as_deref(), Some(f.as_path()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_status_style_follows_theme_darkness() {
        use crate::git::FileStatus;
        let dark = crate::theme::by_name("zaivern-dark");
        let light = crate::theme::by_name("zaivern-light");
        for st in [
            FileStatus::Modified,
            FileStatus::Added,
            FileStatus::Untracked,
            FileStatus::Deleted,
            FileStatus::Renamed,
            FileStatus::Conflicted,
        ] {
            let (dc, db, _) = git_status_style(st, &dark);
            let (lc, lb, _) = git_status_style(st, &light);
            assert_eq!(db, lb, "バッジ文字はテーマ非依存");
            assert_ne!(dc, lc, "ダーク/ライトで色を切り替える: {st:?}");
        }
        // バッジは VS Code 同様の 1 文字
        assert_eq!(git_status_style(FileStatus::Conflicted, &dark).1, "C");
        assert_eq!(git_status_style(FileStatus::Untracked, &dark).1, "U");
    }

    /// 全ステータス × 全テーマで、色がパネル背景から十分に浮くこと。
    /// (ライトテーマで明るい緑を使うと消える、といった事故を止める)
    #[test]
    fn git_status_colors_are_legible_in_every_theme() {
        use crate::git::FileStatus;
        // sRGB の相対輝度 (WCAG)。
        let lum = |c: egui::Color32| {
            let f = |v: u8| {
                let s = v as f32 / 255.0;
                if s <= 0.03928 {
                    s / 12.92
                } else {
                    ((s + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
        };
        let contrast = |a: egui::Color32, b: egui::Color32| {
            let (x, y) = (lum(a), lum(b));
            let (hi, lo) = if x > y { (x, y) } else { (y, x) };
            (hi + 0.05) / (lo + 0.05)
        };
        let statuses = [
            FileStatus::Modified,
            FileStatus::Added,
            FileStatus::Untracked,
            FileStatus::Deleted,
            FileStatus::Renamed,
            FileStatus::Conflicted,
        ];
        for theme in crate::theme::all() {
            for st in statuses {
                let (c, badge, hint) = git_status_style(st, &theme);
                assert!(
                    !badge.is_empty() && !hint.is_empty(),
                    "{st:?} のバッジ/説明"
                );
                let ratio = contrast(c, theme.panel);
                assert!(
                    ratio >= 3.0,
                    "{} テーマの {st:?} が背景に埋もれる (コントラスト比 {ratio:.2})",
                    theme.name
                );
            }
        }
    }
}
