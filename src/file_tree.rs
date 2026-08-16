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
    /// 削除要求(確認ダイアログは呼び出し側が出す)。**複数選択に対応**する。
    /// `None` = 削除要求なし。中の `paths` が実際の対象 (1 件でも複数でも同じ形)。
    pub delete: Option<DeleteRequest>,
    /// 移動/コピーの実行計画。**fs 操作も確認ダイアログも呼び出し側**が行う。
    /// 複数選択のドロップ / 貼り付けは 1 ジョブに複数件入る
    /// (同名衝突を「すべてに適用」で 1 回だけ聞くため)。
    pub transfer: Option<TransferJob>,
    /// ファイル操作の取り消し要求 (ツリーの ⌘Z / Ctrl+Z)。
    ///
    /// エディタ本文の取り消し (`editor::History`) とは**別の履歴**で、
    /// ここが立つのは [`FileTree::handle_keys`] の先頭ガードを通ったとき
    /// = ツリーがフォーカスを持ち、かつどの egui ウィジェットもキーボード
    /// フォーカスを持っていないときだけ。エディタに居るあいだは構造的に
    /// 立たない (「本文を ⌘Z したらファイルが動いた」を起こさない)。
    pub undo: bool,
    /// 設定の変更要求 (ツリーのコンテキストメニューのトグル)。
    /// `Some(v)` なら呼び出し側が config へ書いて永続化する。
    pub set_confirm_dnd: Option<bool>,
    pub set_use_trash: Option<bool>,
    /// ユーザーへ知らせたい注意(貼り付け不可など)。呼び出し側がトーストで出す。
    pub notice: Option<String>,
}

/// 削除要求。**複数選択に対応**するので対象は集合で持つ。
/// `permanent` は 1 回の操作に 1 つ (Shift 併用 = ゴミ箱を通さない完全削除)。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct DeleteRequest {
    /// 消す対象 (ルートは含まない)。空なら何もしない。
    pub paths: Vec<PathBuf>,
    /// true なら復元できない完全削除 (Shift+削除 / 設定でゴミ箱を切っている)。
    pub permanent: bool,
}

/// 1 回のドロップ / 貼り付けでまとめて動かすもの。
///
/// 「1 操作 = 1 ジョブ」にしてあるのは、同名衝突の確認で
/// **「すべてに適用」を 1 回だけ聞く**ため (1 件ずつ聞かない)。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TransferJob {
    pub items: Vec<TransferItem>,
    pub kind: Transfer,
    /// ドラッグ&ドロップ由来か。VS Code の `explorer.confirmDragAndDrop` は
    /// **D&D のときだけ**「移動しますか?」を出すので、貼り付けと区別する。
    pub from_drag: bool,
    /// フォルダ同士のマージとして展開されたジョブか (後始末で空フォルダを畳む)。
    pub merge_root: Option<(PathBuf, PathBuf)>,
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

/// 絞り込みの走査で 1 フレームに辿ってよい件数。
///
/// **フレームを止めないための唯一の約束**。以前は 1 打鍵ごとに
/// `scan_budget` (既定 50,000) 件を同期で辿り切っていたので、大きな
/// リポジトリでは打つたびに数秒フリーズした (実際に「. と打つと
/// くるくるになる」と報告された)。件数で刻めば、クエリが何であっても
/// 1 フレームの費用は同じ。
pub const FILTER_STEP: usize = 800;

/// 一致をここで打ち切る。
///
/// `.` や `s` のような「ほぼ全部に当たる」クエリでは、一致そのものより
/// **一致の祖先を全部展開して描くこと**が重い (フォルダを数千個開くと
/// 可視行が数万行になる)。件数を有限にすれば、どんな文字列でも描画量が
/// 頭打ちになる。ツリーの絞り込みは「目で見て選ぶ」ための道具なので、
/// 300 件より先は見ない。
pub const FILTER_MAX_HITS: usize = 300;

/// ツリーの絞り込み結果 (クエリと、描いてよいパスの集合)。
struct FilterHit {
    /// この結果を作ったクエリ(変わったら作り直す)。
    query: String,
    /// 一致した要素とその祖先ディレクトリ。
    keep: HashSet<PathBuf>,
    /// 一致した件数。
    matched: usize,
    /// 走査を予算 (件数 / 一致数) で打ち切ったか。
    truncated: bool,
    /// まだ走査の途中か (打ち切りとは別物 — 待てば増える)。
    scanning: bool,
}

/// 絞り込み走査の途中経過。フレームをまたいで持ち越す。
struct FilterScan {
    query: String,
    pq: crate::fuzzy::PreparedQuery,
    /// まだ辿っていないディレクトリ (深さつき)。
    stack: Vec<(PathBuf, usize)>,
    keep: HashSet<PathBuf>,
    open_dirs: HashSet<PathBuf>,
    matched: usize,
    visited: usize,
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
    /// 呼び出し側 (config.rs) が持つファイル操作の設定の**写し**。
    /// ツリーは表示とトグル要求 (`TreeActions::set_*`) を出すだけで、
    /// 値の持ち主にはならない (設定の真実源を 2 つにしない)。
    confirm_dnd: bool,
    use_trash: bool,
    /// 取り消せるファイル操作の表示名。`None` = 履歴が空。
    undo_hint: Option<String>,
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
    /// 走査の途中経過。`None` = 走っていない (完走済みか、クエリが空)。
    filter_scan: Option<FilterScan>,
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
            confirm_dnd: true,
            use_trash: true,
            undo_hint: None,
            ignorer: crate::ignore::Ignorer::new(true),
            // `Config::default()` と同じ値から始める (apply_config 前に描いても
            // 既定の見え方 = 「消さずに薄く出す」になる)。
            dim_ignored: true,
            dir_page: crate::config::DEFAULT_TREE_DIR_PAGE,
            more_pages: HashMap::new(),
            scan_budget: crate::config::DEFAULT_INDEX_MAX_FILES,
            max_depth: crate::config::DEFAULT_INDEX_MAX_DEPTH,
            filter: String::new(),
            filter_hit: None,
            filter_scan: None,
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
            self.filter_scan = None;
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
        self.filter_scan = None;
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
    /// ファイル操作まわりの設定と取り消し履歴の状態を毎フレーム受け取る。
    /// 値は config.rs 側が持ち、ここは表示に使うだけ。
    pub fn set_file_ops_state(
        &mut self,
        confirm_dnd: bool,
        use_trash: bool,
        undo_hint: Option<String>,
    ) {
        self.confirm_dnd = confirm_dnd;
        self.use_trash = use_trash;
        self.undo_hint = undo_hint;
    }

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
        self.filter_scan = None;
        self.scan_cache.clear();
    }

    /// キャッシュ済みの各階層をディレクトリ mtime で確認し、外部(エージェント等)で
    /// ファイルが追加・削除・リネームされていたら全キャッシュを破棄する。
    /// 変化があれば true(次フレームの描画でディスクから読み直される)。
    /// 外部変更の見張り対象 (読んだフォルダ → 憶えている mtime)。
    ///
    /// [`refresh_if_changed`](Self::refresh_if_changed) が突き合わせるのと
    /// **まったく同じ組**を、描画スレッドの外の見張り (`crate::fswatch`) へ
    /// そのまま渡すために公開する。片方だけ増減すると「見張っているのに
    /// 気付かない」フォルダができるので、出所は必ずここ 1 つにする。
    pub fn watch_dirs(&self) -> impl Iterator<Item = (&Path, Option<SystemTime>)> + '_ {
        self.mtimes.iter().map(|(d, m)| (d.as_path(), *m))
    }

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

    /// 絞り込みを (必要なら) 少しだけ進め、一致した要素の祖先を展開する。
    ///
    /// **1 フレームで辿るのは [`FILTER_STEP`] 件まで。** 以前は 1 打鍵ごとに
    /// `scan_budget` (既定 50,000) 件を同期で辿り切っていたので、大きな
    /// リポジトリでは打つたびに数秒フリーズした。走査を刻んで持ち越せば、
    /// **どんな文字列が来ても 1 フレームの費用は同じ**になる。
    ///
    /// 一致は [`FILTER_MAX_HITS`] 件で打ち切る。`.` のように「ほぼ全部に
    /// 当たる」クエリは、探すことよりも**一致の祖先を全部展開して描くこと**が
    /// 重いので、件数を有限にして描画量を頭打ちにする。
    ///
    /// 走査中は途中経過をそのまま出す (真っ白にして待たせない)。
    fn recompute_filter(&mut self, ctx: &egui::Context) {
        let q = self.filter.trim().to_string();
        if q.is_empty() {
            self.filter_hit = None;
            self.filter_scan = None;
            self.scan_cache.clear();
            return;
        }
        // 完走済みの結果はそのまま使い回す (毎フレーム走査しない)
        if self.filter_scan.is_none() && self.filter_hit.as_ref().is_some_and(|f| f.query == q) {
            return;
        }
        // クエリが変わったら走査をやり直す (前の途中経過は捨てる)
        if self.filter_scan.as_ref().is_none_or(|s| s.query != q) {
            let mut stack: Vec<(PathBuf, usize)> =
                self.roots.iter().map(|r| (r.clone(), 0usize)).collect();
            stack.reverse();
            self.filter_scan = Some(FilterScan {
                query: q.clone(),
                pq: crate::fuzzy::PreparedQuery::new(&q),
                stack,
                keep: HashSet::new(),
                open_dirs: HashSet::new(),
                matched: 0,
                visited: 0,
                truncated: false,
            });
        }
        self.step_filter(ctx);
    }

    /// 走査を [`FILTER_STEP`] 件ぶんだけ進める。完走したら `filter_scan` を畳む。
    fn step_filter(&mut self, ctx: &egui::Context) {
        let Some(mut scan) = self.filter_scan.take() else {
            return;
        };
        let roots = self.roots.clone();
        let mut steps = 0usize;
        // このフレームで新しく開くフォルダだけを集める
        // (毎フレーム全部へ set_open を撃たない)。
        let mut opened: Vec<PathBuf> = Vec::new();
        while let Some((dir, depth)) = scan.stack.pop() {
            if depth >= self.max_depth {
                continue;
            }
            for e in self.scan_entries(&dir) {
                scan.visited += 1;
                steps += 1;
                if scan.visited > self.scan_budget {
                    scan.truncated = true;
                    break;
                }
                if scan.pq.score(&e.name).is_some() {
                    // 一致が多すぎるクエリは、ここで止める方が親切
                    // (数千件を展開して描くと、目で選べる画面ではなくなる)。
                    if scan.matched >= FILTER_MAX_HITS {
                        scan.truncated = true;
                        break;
                    }
                    scan.matched += 1;
                    scan.keep.insert(e.path.clone());
                    // 祖先をたどって「見える道」を作る (ルートまで)
                    for anc in reveal_ancestors(&roots, &e.path) {
                        scan.keep.insert(anc.clone());
                        if scan.open_dirs.insert(anc.clone()) {
                            opened.push(anc);
                        }
                    }
                }
                // **無視フォルダの中までは探さない。**
                //
                // `.gitignore` に載っているフォルダは (薄表示の設定なら) 行としては
                // 出るが、中身は「成果物」であって探し物ではない。
                //
                // 実測 (このリポジトリ自身・`target/` は 154GB): 降りると
                // **予算 50,000 件を使い切っても `src/terminal.rs` へ届かない**
                // (届くのに 258,332 件辿る必要があった)。つまり 1 打鍵ごとに
                // 数秒固まったうえ、**肝心のソースは 1 件も出てこなかった**。
                // 降りなければ全部で **7,011 件**、`src/terminal.rs` は
                // **186 件目**で見つかる。
                //
                // 遅いだけでなく「探しているものが見つからない」ので、
                // 打ち切りを増やして解決する話ではない。
                //
                // 中まで探したい人は「.gitignore を尊重する」を切る
                // (そのとき `ignored` は全部 false になり、ここは素通りする)。
                if e.is_dir && !e.ignored {
                    scan.stack.push((e.path.clone(), depth + 1));
                }
            }
            if scan.truncated || steps >= FILTER_STEP {
                break;
            }
        }
        for d in &opened {
            set_open(ctx, d, true);
        }
        let done = scan.truncated || scan.stack.is_empty();
        self.filter_hit = Some(FilterHit {
            query: scan.query.clone(),
            keep: scan.keep.clone(),
            matched: scan.matched,
            truncated: scan.truncated,
            scanning: !done,
        });
        if done {
            self.filter_scan = None;
        } else {
            self.filter_scan = Some(scan);
            // 走査が残っている間だけ次のフレームを予約する
            // (終わったら 1 枚も予約しない = アイドルの費用はゼロ)。
            crate::perf::repaint(ctx, "tree_filter");
        }
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
                self.filter_scan = None;
            }
        });
        let Some(f) = &self.filter_hit else {
            return; // 空のときは案内行を出さない (高さも取らない)
        };
        // 走査は刻んで進むので、途中は「探しています」と言い切る。
        // 0 件と「まだ見つかっていない」を混ぜると、打った直後に必ず
        // 「一致するファイルはありません」が一瞬出て嘘になる。
        let msg = if f.scanning {
            trf(
                "{n} 件一致 (探しています…)",
                &[("n", f.matched.to_string())],
            )
        } else if f.matched == 0 {
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
                // ルート見出しもドロップ先にする (ルート直下へ動かせる)
                let resp = header.inner;
                self.drop_target(ui, &resp, root, theme, actions);
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
                    self.file_ops_menu(ui, actions);
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
        // ドラッグ中のゴースト (何を・移動かコピーか)。前景レイヤに 1 枚だけ。
        self.drag_ghost(&ctx, theme);

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
        // IME 変換中はキーを一切拾わない (⌘Z のファイル操作取り消しも含む)。
        // 状態機械を進めない peek 版を使う — `handle_shortcuts` が同じフレームで
        // 進めるので、ここで 2 回目を撃つと変換の開始/終了を食ってしまう。
        if crate::keybinds::ime_blocks_shortcuts_peek(ui.ctx()) {
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
        self.keys_undo(ui, actions);
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
        // **素の ⌘C / ⌘X / ⌘V は `consume_key` では絶対に拾えない。**
        // egui-winit 0.29 は押下イベントを握り潰して `Event::Copy` / `Cut` /
        // `Paste` に差し替えるため (`egui-winit-0.29.1/src/lib.rs:758-774`)。
        // ここは `handle_keys` の冒頭で「自分がキーボードの持ち主」だと
        // 確かめた後 (`self.focused` かつ egui の focus が空) なので、
        // 差し替え後のイベントを直接受け取ってよい。
        let clip = |a: crate::keybinds::ClipboardAlias| {
            ui.input_mut(|i| crate::keybinds::take_clipboard_event(i, a))
        };
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
        // 修飾キーの多い ⌥⌘C (パスのコピー) と食い合わないよう、
        // ⌥ が押されているフレームでは素の ⌘C を取らない
        // (どちらも `Event::Copy` に化けて届くので、修飾キーで見分ける)。
        let alt_held = ui.input(|i| i.modifiers.alt);
        if !alt_held && clip(crate::keybinds::ClipboardAlias::Copy) && !targets.is_empty() {
            self.clipboard = Some((targets.clone(), false));
        }
        if clip(crate::keybinds::ClipboardAlias::Cut) && !targets.is_empty() {
            self.clipboard = Some((targets.clone(), true));
        }
        if clip(crate::keybinds::ClipboardAlias::Paste) {
            let dest = self.paste_dest_dir(rows, sel_idx);
            self.paste_into(dest, actions);
        }
        // Escape: 切り取りの取り消し (filesExplorer.cancelCut)
        if matches!(self.clipboard, Some((_, true))) && pressed(Modifiers::NONE, Key::Escape) {
            self.clipboard = None;
        }

        // ── パスのコピー (copyFilePath: ⌥⌘C / Shift+Alt+C,
        //    copyRelativeFilePath mac: ⇧⌥⌘C。Windows はコード系のため menu のみ) ──
        // ⌥⌘C / ⇧⌥C も `command + C` なので同じすり替えを食らう。
        // `consume_shortcut_compat` はこの形 (⌘ + shift/alt + C) を
        // 逆再生できるので、そちらを通す。
        let copy_path_sc = if mac {
            egui::KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::ALT), Key::C)
        } else {
            egui::KeyboardShortcut::new(Modifiers::SHIFT.plus(Modifiers::ALT), Key::C)
        };
        let copy_path = ui.input_mut(|i| crate::keybinds::consume_shortcut_compat(i, copy_path_sc));
        if copy_path {
            if let Some(r) = sel_idx.map(|i| &rows[i]) {
                ctx.copy_text(r.path.to_string_lossy().to_string());
            }
        }
        if mac
            && ui.input_mut(|i| {
                crate::keybinds::consume_shortcut_compat(
                    i,
                    egui::KeyboardShortcut::new(
                        Modifiers::COMMAND
                            .plus(Modifiers::ALT)
                            .plus(Modifiers::SHIFT),
                        Key::C,
                    ),
                )
            })
        {
            if let Some(r) = sel_idx.map(|i| &rows[i]) {
                let rel = self.rel_of(&r.path);
                ctx.copy_text(rel);
            }
        }

        // ── 削除 (VS Code と同じ 2 系統。どちらもアプリ側で確認ダイアログ) ──
        //   moveFileToTrash: ⌘⌫ (mac) / Delete      → ゴミ箱 (戻せる)
        //   deleteFile     : ⌥⌘⌫ (mac) / ⇧Delete    → 完全削除 (戻せない)
        // 修飾キーの多い方を先に消費する (少ない方に吸われないように)。
        let perm = if mac {
            pressed(Modifiers::COMMAND.plus(Modifiers::ALT), Key::Backspace)
        } else {
            pressed(Modifiers::SHIFT, Key::Delete)
        };
        let trash = if mac {
            pressed(Modifiers::COMMAND, Key::Backspace) || pressed(Modifiers::NONE, Key::Delete)
        } else {
            pressed(Modifiers::NONE, Key::Delete)
        };
        if (perm || trash) && !targets.is_empty() {
            actions.delete = Some(DeleteRequest {
                paths: targets,
                // 設定でゴミ箱を切っているときも完全削除になる
                permanent: perm || !self.use_trash,
            });
        }
    }

    /// handle_keys の取り消し部 — **ファイル操作**の ⌘Z / Ctrl+Z。
    ///
    /// エディタ本文の取り消し (`editor::History`) とは別の履歴を戻す。
    /// ここへ来られるのは [`FileTree::handle_keys`] の先頭ガード
    /// (`self.focused` かつ `memory().focused().is_none()`) を通ったときだけ
    /// なので、エディタや入力欄に居るあいだは**構造的に**発火しない。
    fn keys_undo(&mut self, ui: &mut egui::Ui, actions: &mut TreeActions) {
        if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Z)) {
            actions.undo = true;
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
        let kind = if cut { Transfer::Move } else { Transfer::Copy };
        // コピーは VS Code の incrementalNaming どおり自動採番 (既存を壊さない)。
        // 切り取り = 移動なので、既存があれば確認を取る (Ask)。
        let numbering = if cut { Numbering::Ask } else { Numbering::Auto };
        // 複数選択ぶんを**1 ジョブ**にまとめる。ここが「すべてに適用」の土台:
        // 1 件ずつジョブにすると、衝突のたびに聞き直すことになる。
        let mut items = Vec::new();
        for src in srcs {
            match transfer_plan(&src, &dest_dir, kind, numbering) {
                Ok(None) => {}
                Ok(Some(item)) => items.push(item),
                // 1 件だめでも残りは貼り付ける (最後の理由だけ知らせる)
                Err(msg) => actions.notice = Some(msg),
            }
        }
        if !items.is_empty() {
            actions.transfer = Some(TransferJob {
                items,
                kind,
                from_drag: false,
                merge_root: None,
            });
            // 切り取りのクリップボードはここでは消さない。移動の成否は
            // アプリ側で判るため、成功時に clear_clipboard() を呼んで
            // もらう (失敗時に切り取り内容が失われないように)。
        }
    }

    /// ドロップされた `src` を `dest_dir` へ入れる計画を立てる。
    ///
    /// `alt` (macOS の ⌥ / Windows・Linux の Alt) が押されていればコピー、
    /// でなければ移動 — VS Code と同じ。`Modifiers::alt` は egui-winit が
    /// 両 OS 分を正規化して入れてくれるので OS 分岐は要らない。
    ///
    /// **掴んだ行が選択に入っていれば選択集合ごと運ぶ** (VS Code と同じ)。
    /// 集合はそのまま 1 ジョブになるので、同名衝突は「すべてに適用」で
    /// 1 回だけ聞ける。
    fn drop_into(&mut self, src: &Path, dest_dir: PathBuf, alt: bool, actions: &mut TreeActions) {
        let kind = if alt { Transfer::Copy } else { Transfer::Move };
        let mut items = Vec::new();
        for s in self.selection_targets(src) {
            match transfer_plan(&s, &dest_dir, kind, Numbering::Ask) {
                Ok(None) => {}
                Ok(Some(item)) => items.push(item),
                // 1 件だめでも残りは運ぶ (最後の理由だけ知らせる)
                Err(msg) => actions.notice = Some(msg),
            }
        }
        if items.is_empty() {
            return;
        }
        actions.transfer = Some(TransferJob {
            items,
            kind,
            from_drag: true,
            merge_root: None,
        });
        self.focused = true;
        // 移動先がツリーから見えなくなるなら、黙って消えたように見せない
        if let Some(msg) = self.hidden_dest_notice(&dest_dir) {
            actions.notice = Some(msg);
        }
    }

    /// ドロップ先がツリーから見えなくなる場合の注意文。
    ///
    /// - **絞り込み中**: 絞り込みは「一致した要素 + その祖先」を残す剪定なので、
    ///   行は本物のツリーのまま = 落とし先は曖昧にならない (だから受け付ける)。
    ///   ただし移動後に新しい場所がクエリに当たらないと画面から消える。
    /// - **`.gitignore` の対象**: 隠す設定なら行が無いので構造的に落とせない。
    ///   薄表示のときは落とせるが、隠す設定に戻すと見えなくなる。
    fn hidden_dest_notice(&mut self, dest_dir: &Path) -> Option<String> {
        let ignored = root_for(&self.roots, dest_dir).is_some_and(|root| {
            let root = root.to_path_buf();
            self.ignorer.is_ignored(&root, dest_dir, true)
        });
        let filtering = !self.filter.trim().is_empty();
        drop_visibility_notice(&label_of(dest_dir), ignored, filtering)
    }

    /// 右クリック / ドラッグの対象。その行が選択に入っていれば選択集合ごと、
    /// 入っていなければその 1 件 (VS Code と同じ)。ルートは常に外す。
    fn selection_targets(&self, path: &Path) -> Vec<PathBuf> {
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

    /// 行をツリー内 D&D の受け口にする。
    /// フォルダ行はその中、ファイル行は親フォルダが宛先 (VS Code と同じ)。
    ///
    /// ドロップできない先 (自分自身 / 自分の配下) は赤枠 + `NotAllowed` で示し、
    /// Alt を押していれば `Copy` カーソルで「コピーになる」ことを見せる。
    fn drop_target(
        &mut self,
        ui: &egui::Ui,
        resp: &egui::Response,
        dest_dir: &Path,
        theme: &Theme,
        actions: &mut TreeActions,
    ) {
        let Some(src) = resp.dnd_hover_payload::<PathBuf>() else {
            return;
        };
        let alt = ui.input(|i| i.modifiers.alt);
        let kind = if alt { Transfer::Copy } else { Transfer::Move };
        let ok = matches!(
            transfer_plan(&src, dest_dir, kind, Numbering::Ask),
            Ok(Some(_))
        );
        let color = if ok { theme.accent } else { theme.err };
        ui.painter().rect_stroke(
            resp.rect.expand(1.0),
            egui::Rounding::same(4.0),
            egui::Stroke::new(1.5_f32, color),
        );
        ui.ctx().set_cursor_icon(if !ok {
            egui::CursorIcon::NotAllowed
        } else if alt {
            egui::CursorIcon::Copy
        } else {
            egui::CursorIcon::Move
        });
        if let Some(src) = resp.dnd_release_payload::<PathBuf>() {
            self.drop_into(&src, dest_dir.to_path_buf(), alt, actions);
        }
    }

    /// ドラッグ中にカーソルの脇へ出す小さなゴースト。
    /// 「何を」「移動なのかコピーなのか」を掴んだまま見せる (VS Code 相当)。
    fn drag_ghost(&self, ctx: &egui::Context, theme: &Theme) {
        let Some(src) = egui::DragAndDrop::payload::<PathBuf>(ctx) else {
            return;
        };
        let Some(pos) = ctx.pointer_interact_pos() else {
            return;
        };
        let alt = ctx.input(|i| i.modifiers.alt);
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let label = if alt {
            trf("📄 {name} をコピー", &[("name", name)])
        } else {
            trf("➡ {name} を移動", &[("name", name)])
        };
        egui::Area::new(egui::Id::new("zv-tree-dnd-ghost"))
            .order(egui::Order::Tooltip)
            .fixed_pos(pos + egui::vec2(16.0, 16.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(theme.panel)
                    .stroke(egui::Stroke::new(1.0_f32, theme.accent))
                    .rounding(egui::Rounding::same(6.0))
                    .inner_margin(egui::Margin::symmetric(8.0, 4.0))
                    .show(ui, |ui| {
                        ui.label(RichText::new(label).small().color(theme.text));
                    });
            });
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

/// 切り取り待ちの行を薄める係数 (VS Code と同じ「掴んでいる」合図)。
const CUT_PENDING_ALPHA: f32 = 0.5;

/// git が無視している行の前景色。
///
/// VS Code の `gitDecoration.ignoredResourceForeground` に相当する。
/// **係数 (`gamma_multiply`) を掛けない** — 掛け算はテーマの地の明るさを
/// 見ないので、ライトテーマでは本文が背景へ寄って薄くなりすぎ、
/// ダークテーマでは逆に十分弱くならない。テーマが自分で決めた
/// 「読めるが弱い」色 = `text_dim` を使う (`text_dim/bg` のコントラスト
/// 下限は `theme::tests::every_theme_meets_the_text_contrast_floor` が守る)。
pub(crate) fn ignored_fg(theme: &Theme) -> egui::Color32 {
    theme.text_dim
}

/// ツリー 1 行の前景色。**優先順位はここだけにある**。
///
/// 切り取り待ち > 無視 > git ステータス > 通常。
/// 無視が git ステータスより強いのは VS Code と同じで、
/// 「コミットされない物」であることのほうが「変わった」より先に伝わるべきだから
/// (`.gitignore` の中の変更を M の色で見せると、追跡されていると誤読される)。
pub(crate) fn row_fg(
    theme: &Theme,
    cut_pending: bool,
    ignored: bool,
    status: Option<crate::git::FileStatus>,
) -> egui::Color32 {
    if cut_pending {
        theme.text.gamma_multiply(CUT_PENDING_ALPHA)
    } else if ignored {
        ignored_fg(theme)
    } else if let Some(st) = status {
        git_status_style(st, theme).0
    } else {
        theme.text
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
                // 色より先に勝つ条件 (切り取り待ち / 無視) が立っていれば
                // git へ問い合わせない (畳んだ node_modules で無駄に引かない)。
                let dir_st = if cut_pending || e.ignored {
                    None
                } else {
                    gitinfo.dir_status(&e.path)
                };
                let dir_color = row_fg(theme, cut_pending, e.ignored, dir_st.map(|(s, _)| s));
                let (dir_badge, dir_hint) = if cut_pending {
                    (String::new(), String::new())
                } else if e.ignored {
                    (String::new(), tr("git が無視しています (.gitignore)"))
                } else if let Some((st_type, count)) = dir_st {
                    let (_, b, h) = git_status_style(st_type, theme);
                    (
                        format!(" {b}•{count}"),
                        format!(
                            "{}\n{}",
                            trf("配下に {n} 件の変更", &[("n", count.to_string())]),
                            tr(h)
                        ),
                    )
                } else {
                    (String::new(), String::new())
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
                // ツリー内 D&D: フォルダは掴む側にも落とす側にもなる
                let resp = header.inner.interact(egui::Sense::click_and_drag());
                resp.dnd_set_drag_payload(e.path.clone());
                self.drop_target(ui, &resp, &e.path, theme, actions);
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
                    self.delete_menu(ui, &e.path, actions);
                    ui.separator();
                    self.file_ops_menu(ui, actions);
                    self.auto_reveal_menu(ui);
                });
            } else {
                let file_st = if cut_pending || e.ignored {
                    None
                } else {
                    gitinfo.file_status(&e.path)
                };
                let file_color = row_fg(theme, cut_pending, e.ignored, file_st);
                let (file_badge, hint) = if cut_pending {
                    (String::new(), "")
                } else if e.ignored {
                    (String::new(), "git が無視しています (.gitignore)")
                } else if let Some(st_type) = file_st {
                    let (_, b, h) = git_status_style(st_type, theme);
                    (format!("  {b}"), h)
                } else {
                    (String::new(), "")
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
                // ファイル行へ落としたら「その隣」= 親フォルダへ入れる (VS Code と同じ)
                self.drop_target(ui, &resp, dir, theme, actions);
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
                    self.delete_menu(ui, &e.path, actions);
                    ui.separator();
                    self.file_ops_menu(ui, actions);
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

    /// 削除メニュー。ゴミ箱行きと完全削除を**別の項目**にして、
    /// どちらを押したのかが文言で分かるようにする (VS Code と同じ 2 系統)。
    fn delete_menu(&mut self, ui: &mut egui::Ui, path: &Path, actions: &mut TreeActions) {
        // 右クリックした行が選択に入っていれば選択集合ごと消す (VS Code と同じ)
        let targets = self.delete_targets(path);
        let n = targets.len();
        let suffix = if n > 1 {
            trf(" ({n} 件)", &[("n", n.to_string())])
        } else {
            String::new()
        };
        if self.use_trash
            && menu_btn(
                ui,
                format!("{}{suffix}", tr("🗑 ゴミ箱へ移動…")),
                h("⌘⌫", "Delete"),
            )
        {
            actions.delete = Some(DeleteRequest {
                paths: targets.clone(),
                permanent: false,
            });
        }
        if menu_btn(
            ui,
            format!("{}{suffix}", tr("🗑 完全に削除…")),
            h("⌥⌘⌫", "Shift+Delete"),
        ) {
            actions.delete = Some(DeleteRequest {
                paths: targets,
                permanent: true,
            });
        }
    }

    /// ファイル操作の設定 (移動の確認 / ゴミ箱) と取り消し。
    /// 設定値は config.rs 側が持ち、ここは要求を出すだけ。
    fn file_ops_menu(&mut self, ui: &mut egui::Ui, actions: &mut TreeActions) {
        if let Some(hint) = self.undo_hint.clone() {
            if menu_btn(
                ui,
                trf("↩ 元に戻す: {op}", &[("op", hint)]),
                h("⌘Z", "Ctrl+Z"),
            ) {
                actions.undo = true;
            }
        }
        let label = if self.confirm_dnd {
            tr("🖐 移動の確認: ON")
        } else {
            tr("🖐 移動の確認: OFF")
        };
        if menu_btn(ui, label, "") {
            actions.set_confirm_dnd = Some(!self.confirm_dnd);
        }
        let label = if self.use_trash {
            tr("🗑 削除はゴミ箱へ: ON")
        } else {
            tr("🗑 削除はゴミ箱へ: OFF (常に完全削除)")
        };
        if menu_btn(ui, label, "") {
            actions.set_use_trash = Some(!self.use_trash);
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
        self.selection_targets(path)
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

// ─── 同名衝突の検出 (VS Code の「置き換えますか?」の土台) ─────────

/// 移動/コピー先に既にあるものの種類。文言の出し分けに使う。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Clash {
    /// 同名のファイル (壊れたリンクを含むシンボリックリンクも) がある。
    /// 実行すると**中身が置き換わる**。
    Overwrite,
    /// フォルダ同士。VS Code と同じく**中身のマージ**になるので、
    /// ファイルの上書きとは別の文言にする。
    Merge,
    /// 種類が違う (ファイル ↔ フォルダ)。片方が丸ごと消えるので最も危険。
    Mismatch {
        /// 移動先がフォルダか (= フォルダが中身ごと消える側か)。
        dest_is_dir: bool,
    },
}

/// パスの実体の種類。`symlink_metadata` を使うので**リンクは辿らない**。
/// 壊れたリンクも「ある」と数える — `fs::rename` はそれを黙って上書きするため、
/// `Path::exists()` (リンク先を見る) で判定すると取りこぼす。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EntryKind {
    Dir,
    File,
    Symlink,
}

fn entry_kind(p: &Path) -> Option<EntryKind> {
    let t = std::fs::symlink_metadata(p).ok()?.file_type();
    Some(if t.is_symlink() {
        EntryKind::Symlink
    } else if t.is_dir() {
        EntryKind::Dir
    } else {
        EntryKind::File
    })
}

/// `a` と `b` が**同じ実体**を指すか。
///
/// macOS (APFS/HFS+ の既定) と Windows (NTFS) は大文字小文字を区別しないため、
/// `a.txt` → `A.txt` のリネームでも `b` は「存在する」ことになる。これを同名衝突と
/// 数えると case-only rename が永久にできなくなる (実際に起きやすい事故) ので、
/// 正規化したパスが一致するものは衝突ではないと判定する。
/// `canonicalize` は macOS / Windows とも**実際のディスク上の綴り**を返すので、
/// 大小違いの 2 つの綴りは同じ結果に畳まれる。
pub fn same_entry(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// `src` を `dest` へ動かしたときに壊すものがあるか。無ければ `None`。
pub fn clash_at(src: &Path, dest: &Path) -> Option<Clash> {
    let dk = entry_kind(dest)?;
    if same_entry(src, dest) {
        return None; // 大文字小文字だけの違い等 = 自分自身
    }
    match (entry_kind(src), dk) {
        (Some(EntryKind::Dir), EntryKind::Dir) => Some(Clash::Merge),
        (_, EntryKind::Dir) => Some(Clash::Mismatch { dest_is_dir: true }),
        (Some(EntryKind::Dir), _) => Some(Clash::Mismatch { dest_is_dir: false }),
        _ => Some(Clash::Overwrite),
    }
}

/// 既存があったときの振る舞い。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Numbering {
    /// `x copy.txt` へ自動採番する (⌘C→⌘V / 同じフォルダへの複製)。
    /// 既存を壊さないので確認は要らない。
    Auto,
    /// 既存があれば [`Clash`] として返し、確認は呼び出し側に任せる
    /// (D&D / 切り取り貼り付け)。
    Ask,
}

/// 移動/コピー 1 件の実行計画。**この構造体を作るだけでは fs は変わらない**。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TransferItem {
    pub src: PathBuf,
    pub dest: PathBuf,
    pub kind: Transfer,
    /// 実行すると壊れるものがあるか。`Some` なら確認を取ってからでないと
    /// 実行してはいけない。
    pub clash: Option<Clash>,
}

/// 移動/コピー 1 件の計画を立てる。`Ok(None)` は何もしない
/// (同じ場所への移動など)。エラーはそのままユーザーへ見せるメッセージ。
pub fn transfer_plan(
    src: &Path,
    dest_dir: &Path,
    kind: Transfer,
    numbering: Numbering,
) -> Result<Option<TransferItem>, String> {
    let Some(sk) = entry_kind(src) else {
        return Err(tr("移動元が見つかりません"));
    };
    let Some(name) = src.file_name().map(|n| n.to_string_lossy().to_string()) else {
        return Err(tr("移動元が見つかりません"));
    };
    // 自分自身 / 自分の配下へは入れられない (無限再帰になる)
    if sk == EntryKind::Dir && dest_dir.starts_with(src) {
        return Err(tr("フォルダを自身の中へは移動できません"));
    }
    let same_dir = src.parent() == Some(dest_dir);
    if same_dir && kind == Transfer::Move {
        return Ok(None); // 同じ場所への移動は VS Code 同様なにもしない
    }
    // 同じフォルダへのコピーは「複製」ジェスチャなので必ず採番する
    let numbering = if same_dir { Numbering::Auto } else { numbering };
    let (dest, clash) = match numbering {
        Numbering::Auto => (next_paste_path(dest_dir, &name, sk == EntryKind::Dir), None),
        Numbering::Ask => {
            let d = dest_dir.join(&name);
            let c = clash_at(src, &d);
            (d, c)
        }
    };
    Ok(Some(TransferItem {
        src: src.to_path_buf(),
        dest,
        kind,
        clash,
    }))
}

/// フォルダ同士のマージを「1 ファイル 1 件」の計画へ展開する。
///
/// VS Code と同じく、フォルダの衝突は上書きではなく**中身のマージ**なので、
/// 実際に上書きが起きるのは中の個々のファイルだけ。ここで 1 件ずつ
/// [`Clash`] を載せておくと、呼び出し側が「すべてに適用」でまとめて答えられる。
/// **fs は一切変更しない。**
pub fn expand_merge(
    src_dir: &Path,
    dest_dir: &Path,
    kind: Transfer,
) -> Result<Vec<TransferItem>, String> {
    let mut out = Vec::new();
    walk_merge(src_dir, dest_dir, kind, 0, &mut out)?;
    Ok(out)
}

fn walk_merge(
    src_dir: &Path,
    dest_dir: &Path,
    kind: Transfer,
    depth: usize,
    out: &mut Vec<TransferItem>,
) -> Result<(), String> {
    if depth > 64 {
        return Err(tr("フォルダが深すぎます (>64)"));
    }
    let rd = std::fs::read_dir(src_dir)
        .map_err(|e| trf("読み取れません: {e}", &[("e", e.to_string())]))?;
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    // 並びをファイルシステム依存にしない (確認の順序を決定的にする)
    entries.sort();
    for src in entries {
        let Some(name) = src.file_name() else {
            continue;
        };
        let dest = dest_dir.join(name);
        // リンクは辿らない (辿ると循環・意図しない大量コピーの危険)
        if entry_kind(&src) == Some(EntryKind::Dir) && entry_kind(&dest) == Some(EntryKind::Dir) {
            walk_merge(&src, &dest, kind, depth + 1, out)?;
            continue;
        }
        out.push(TransferItem {
            clash: clash_at(&src, &dest),
            src,
            dest,
            kind,
        });
    }
    Ok(())
}

/// マージ完了後の後始末。
///
/// 中身を 1 件ずつ動かしただけでは**空フォルダが移動先に生まれない**ので、
/// ここで `src_dir` 側の階層をなぞって移動先に作り直す。
///
/// `remove_src` が true (移動) のときは、空になった元フォルダを畳む。
/// **中身が残っているフォルダ (= 衝突をスキップした) は消さない** —
/// `remove_dir` は中身があれば必ず失敗するので、構造的にそうなる。
/// コピーでは false を渡す (元は 1 つも触らない)。
pub fn prune_merged_dirs(src_dir: &Path, dest_dir: &Path, remove_src: bool) {
    prune_inner(src_dir, dest_dir, remove_src, 0);
}

fn prune_inner(src_dir: &Path, dest_dir: &Path, remove_src: bool, depth: usize) {
    if depth > 64 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(src_dir) else {
        return;
    };
    for e in rd.filter_map(|e| e.ok()) {
        let sp = e.path();
        if entry_kind(&sp) != Some(EntryKind::Dir) {
            continue;
        }
        let Some(name) = sp.file_name() else { continue };
        prune_inner(&sp, &dest_dir.join(name), remove_src, depth + 1);
    }
    // 階層は必ず作り直す (空フォルダを失わない)
    let _ = std::fs::create_dir_all(dest_dir);
    if remove_src {
        // 空になったものだけ畳む (remove_dir は中身があれば必ず失敗する = 安全)
        let _ = std::fs::remove_dir(src_dir);
    }
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

// ══════════════════════════════════════════════════════════════════
//  ファイル所有リースの門
// ══════════════════════════════════════════════════════════════════

/// 門の判定。テストから差し替えられるように 1 段挟む
/// (シングルトンを経由せずに「拒否されたら実行しない」を試せる)。
pub type Gate<'a> = &'a dyn Fn(&Path) -> Option<String>;

/// 既定の門: ファイル所有リースへ問い合わせる。
///
/// ガードが無効なスコープでは [`crate::lease::check_write`] が即 `Allow` を
/// 返すので、単独で使う人の払うコストはゼロ (設計原則 3)。
/// **早期 return を自前で書かない** — 二重判定はいつかズレる。
fn lease_deny(path: &Path) -> Option<String> {
    match crate::lease::check_write(path) {
        crate::lease::Verdict::Deny(msg) => Some(msg),
        crate::lease::Verdict::Allow => None,
    }
}

/// ファイルは fs::copy、フォルダは再帰コピー。
/// シンボリックリンクは辿らずスキップする (祖先を指すリンクがあると
/// 無限再帰でスタックオーバーフローする)。深さも保険で制限する。
///
/// **他の担当が持っているファイルの上へは書かない。** 拒否されたら
/// `PermissionDenied` で理由を返す (`src/editor.rs` の保存と同じ形)。
pub fn copy_recursively(src: &Path, dst: &Path) -> std::io::Result<()> {
    copy_recursively_inner(src, dst, 0, &lease_deny)
}

fn copy_recursively_inner(
    src: &Path,
    dst: &Path,
    depth: usize,
    gate: Gate<'_>,
) -> std::io::Result<()> {
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
            copy_recursively_inner(&e.path(), &dst.join(e.file_name()), depth + 1, gate)?;
        }
        Ok(())
    } else {
        // 書く直前に門を通す (見るのは**書かれる側**だけ。元は 1 つも触らない)。
        if let Some(msg) = gate(dst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                msg,
            ));
        }
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

// ─── ファイル操作の取り消し履歴 ───────────────────────────────

/// ファイル操作の取り消し履歴 1 件。
///
/// **エディタ本文の取り消し (`crate::editor::History`) とは完全に別物。**
/// 混ぜると「本文で ⌘Z したらファイルが移動した」という最悪の事故になるので、
/// 型も履歴もフォーカスの条件も分けてある。
/// **完全削除は復元できないので、そもそもここへ積まない。**
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FileOp {
    /// 名前の変更 (旧, 新)。
    Rename { from: PathBuf, to: PathBuf },
    /// 移動 (元, 先) の並び。1 回の D&D / 貼り付けをまとめて 1 手で戻す。
    Move {
        pairs: Vec<(PathBuf, PathBuf)>,
        /// フォルダ統合として展開したときの (元, 先)。戻すときに畳む。
        merge_root: Option<(PathBuf, PathBuf)>,
    },
    /// 新規作成 / 貼り付けで増えたもの。取り消しは**ゴミ箱経由**で消す。
    Create { path: PathBuf, is_dir: bool },
    /// ゴミ箱へ送ったもの (元の場所, ゴミ箱の中の実体) の並び。
    /// 複数選択の削除をまとめて 1 手で戻す。
    Trash { items: Vec<(PathBuf, PathBuf)> },
}

impl FileOp {
    /// メニューに出す表示名 (「元に戻す: ○○」の ○○)。
    pub fn label(&self) -> String {
        match self {
            FileOp::Rename { .. } => tr("名前の変更"),
            FileOp::Move { .. } => tr("移動"),
            FileOp::Create { is_dir: true, .. } => tr("フォルダの作成"),
            FileOp::Create { is_dir: false, .. } => tr("ファイルの作成"),
            FileOp::Trash { .. } => tr("ゴミ箱へ移動"),
        }
    }
}

/// ファイル操作の取り消し履歴 (エディタ本文の履歴とは別の入れ物)。
#[derive(Default)]
pub struct FileHistory {
    ops: Vec<FileOp>,
}

impl FileHistory {
    /// 保持する手数の上限。増え続けさせない。
    pub const MAX: usize = 64;

    pub fn push(&mut self, op: FileOp) {
        self.ops.push(op);
        if self.ops.len() > Self::MAX {
            self.ops.remove(0);
        }
    }

    pub fn pop(&mut self) -> Option<FileOp> {
        self.ops.pop()
    }

    /// 直近の操作の表示名。`None` = 履歴が空。
    pub fn hint(&self) -> Option<String> {
        self.ops.last().map(FileOp::label)
    }
}

/// 確認キューの 1 件を「実行してよいか」の判定 (純粋関数)。
///
/// - `None` を返したら**ユーザーに聞かなければならない** — ここが
///   「確認を経ずに壊さない」の要。
/// - 衝突が無い項目は常に実行してよい (聞かない)。
/// - 「すべてに適用」(`apply_all`) が決まっていれば残り全部に効く。
///   決まっていなければ「いま答えた 1 件ぶん」(`answer`) だけを使う。
pub fn queue_answer(
    clash: Option<Clash>,
    answer: Option<bool>,
    apply_all: Option<bool>,
) -> Option<bool> {
    match clash {
        None => Some(true),
        Some(_) => answer.or(apply_all),
    }
}

/// ドロップ先がツリーから見えなくなるときの注意文 (純粋関数)。
///
/// **「落とした先が画面から消える」を黙って起こさない**ためだけのもので、
/// ドロップ自体は止めない (行はどちらの場合も本物のツリーの行なので、
/// 落とし先が曖昧になることはない)。
pub fn drop_visibility_notice(
    dest_name: &str,
    dest_ignored: bool,
    filtering: bool,
) -> Option<String> {
    match (dest_ignored, filtering) {
        (true, _) => Some(trf(
            "移動先の「{name}」は .gitignore の対象です (ツリーに出ないことがあります)",
            &[("name", dest_name.to_string())],
        )),
        (false, true) => Some(trf(
            "絞り込み中です: 移動先「{name}」の中身は条件に合うものだけが出ます",
            &[("name", dest_name.to_string())],
        )),
        (false, false) => None,
    }
}

/// 取り消しのための移動。
///
/// **対象が消えている / 戻り先が既に塞がっているときは、何もせず失敗する。**
/// 取り消しが新しい破壊 (上書き) を生まないための一番大事な性質で、
/// ここを緩めると「⌘Z したら別のファイルが消えた」になる。
pub fn move_back(from: &Path, to: &Path) -> Result<(), String> {
    move_back_gated(from, to, &lease_deny)
}

fn move_back_gated(from: &Path, to: &Path, gate: Gate<'_>) -> Result<(), String> {
    if entry_kind(from).is_none() {
        return Err(trf(
            "取り消せません: {name} が見つかりません",
            &[("name", label_of(from))],
        ));
    }
    if entry_kind(to).is_some() {
        return Err(trf(
            "取り消せません: {name} が既にあります",
            &[("name", label_of(to))],
        ));
    }
    // **改名は元と先の両方を見る。** 他人のファイルを動かすのは、他人の
    // ファイルを潰すのと同じだけ悪い。ディレクトリを作る前に判定する
    // (拒否されたときに痕跡を残さない)。
    if let Some(msg) = gate(from) {
        return Err(msg);
    }
    if let Some(msg) = gate(to) {
        return Err(msg);
    }
    if let Some(d) = to.parent() {
        std::fs::create_dir_all(d)
            .map_err(|e| trf("取り消せません: {e}", &[("e", e.to_string())]))?;
    }
    std::fs::rename(from, to).map_err(|e| trf("取り消せません: {e}", &[("e", e.to_string())]))
}

/// 表示用のファイル名 (取れなければフルパス)。
pub fn label_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}

/// OS のゴミ箱へ送る (VS Code の `files.enableTrash` 相当)。
///
/// ## 方針
/// - **3 OS ぶん全部を実装する。** 外部コマンド (`osascript` / `gio` / `trash-put`)
///   には一切頼らない — 入っていない環境で黙って壊れるため。
/// - **失敗したら完全削除へ落ちない。** 理由を文字列で返して止める。
///   「ゴミ箱へ入れたつもりが消えていた」を構造的に起こさない。
/// - 戻り先が分かる OS では**ゴミ箱の中の実体パス**を返す。ファイル操作の
///   取り消し (⌘Z) はこれを使って元の場所へ戻す。
///
/// ## OS ごとの実体
/// - **macOS**: `~/.Trash`。Finder がそのまま「ゴミ箱」として見せる場所。
///   (Finder の「戻す」に要るメタデータは書かないので、戻すのはアプリ側の ⌘Z か
///   ゴミ箱からのドラッグになる)
/// - **Linux / その他 Unix**: freedesktop.org Trash 仕様。
///   `$XDG_DATA_HOME/Trash/{files,info}` (既定 `~/.local/share/Trash`)。
///   `.trashinfo` を書くので、どのファイルマネージャからでも「元に戻す」が効く。
/// - **Windows**: `SHFileOperationW(FO_DELETE | FOF_ALLOWUNDO)` = 本物のごみ箱。
///   ごみ箱の中のパスは API から返らないため、取り消しでは戻せない (`Ok(None)`)。
pub mod trash {
    use crate::i18n::{tr, trf};
    // 「移す」形のゴミ箱 (macOS / Unix) を組み立てるためだけの型。
    // Windows は SHFileOperationW にパスを渡すだけなので使わない —
    // cfg を付けないと Windows ビルドでだけ unused_imports 警告になる
    // (CI の clippy は ubuntu 1 台でしか回らないため、この警告は
    //  tools/windows-check.sh --clippy でしか出てこない)。
    #[cfg(any(unix, test))]
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};
    #[cfg(any(unix, test))]
    use std::time::SystemTime;

    /// ゴミ箱へ「移す」形の OS (macOS / Unix) 用の実行計画。
    ///
    /// **これを組み立てるだけでは fs は 1 バイトも変わらない。**
    /// テストはこの純粋関数だけを検証するので、実行するユーザーの
    /// ゴミ箱には何も入らない。
    #[cfg(any(unix, test))]
    #[derive(Clone, PartialEq, Eq, Debug)]
    pub struct MovePlan {
        /// 実体を置くディレクトリ (`~/.Trash` または `<trash>/files`)。
        pub files_dir: PathBuf,
        /// 置くときの名前 (既存とぶつかるなら `.2` `.3` … を足したもの)。
        pub file_name: OsString,
        /// freedesktop の `.trashinfo` (書き出し先, 中身)。macOS では `None`。
        pub info: Option<(PathBuf, String)>,
    }

    /// ゴミ箱の中で使える名前を選ぶ。`taken` は「そこに何かあるか」を返す述語で、
    /// テストからは fs を触らないダミーを渡せる。
    #[cfg(any(unix, test))]
    fn unique_name(
        dir: &Path,
        base: &OsStr,
        now: SystemTime,
        taken: &dyn Fn(&Path) -> bool,
    ) -> OsString {
        if !taken(&dir.join(base)) {
            return base.to_os_string();
        }
        for n in 2..=9999u32 {
            let mut c = base.to_os_string();
            c.push(format!(".{n}"));
            if !taken(&dir.join(&c)) {
                return c;
            }
        }
        // 9999 まで埋まっている異常時だけ時刻を混ぜる (実質起きない)
        let nanos = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let mut c = base.to_os_string();
        c.push(format!(".{nanos}"));
        c
    }

    /// freedesktop の `Path=` 用パーセントエンコード。
    /// 予約されていない文字と `/` はそのまま、それ以外を `%XX` にする。
    #[cfg(any(unix, test))]
    pub fn pct_encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.as_bytes() {
            let c = *b as char;
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '/') {
                out.push(c);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    /// `YYYY-MM-DDThh:mm:ss` (UTC)。chrono を足さずに済ませるための最小実装。
    /// 仕様上はローカル時刻だが、ずれるのは表示される削除日時だけで
    /// 復元には影響しない。
    #[cfg(any(unix, test))]
    pub fn iso8601_utc(unix_secs: i64) -> String {
        let days = unix_secs.div_euclid(86_400);
        let rem = unix_secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
            rem / 3600,
            (rem % 3600) / 60,
            rem % 60
        )
    }

    /// 1970-01-01 からの日数 → (年, 月, 日)。Howard Hinnant の civil_from_days。
    #[cfg(any(unix, test))]
    fn civil_from_days(z: i64) -> (i64, u32, u32) {
        let z = z + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    /// ゴミ箱へ移す計画を立てる (fs は触らない)。
    ///
    /// `home` / `data` は `dirs` から渡す — **場所を直書きしない**ので、
    /// どのユーザー名でも `$XDG_DATA_HOME` を変えた環境でも動く。
    /// テストからは一時ディレクトリを渡せるので、実ゴミ箱には触れない。
    #[cfg(any(unix, test))]
    pub fn move_plan(
        path: &Path,
        home: Option<&Path>,
        data: Option<&Path>,
        now: SystemTime,
        taken: &dyn Fn(&Path) -> bool,
    ) -> Result<MovePlan, String> {
        let Some(base) = path.file_name() else {
            return Err(tr("削除できません: 名前が取れません"));
        };
        if cfg!(target_os = "macos") {
            let Some(home) = home else {
                return Err(tr("ホームディレクトリが分からないためゴミ箱を使えません"));
            };
            let files_dir = home.join(".Trash");
            let file_name = unique_name(&files_dir, base, now, taken);
            return Ok(MovePlan {
                files_dir,
                file_name,
                info: None,
            });
        }
        let Some(data) = data else {
            return Err(tr("データディレクトリが分からないためゴミ箱を使えません"));
        };
        let root = data.join("Trash");
        let files_dir = root.join("files");
        let file_name = unique_name(&files_dir, base, now, taken);
        let secs = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut info_name = file_name.clone();
        info_name.push(".trashinfo");
        let body = format!(
            "[Trash Info]\nPath={}\nDeletionDate={}\n",
            pct_encode(&path.to_string_lossy()),
            iso8601_utc(secs)
        );
        Ok(MovePlan {
            files_dir,
            file_name,
            info: Some((root.join("info").join(info_name), body)),
        })
    }

    /// Windows のごみ箱 API へ渡す引数を組み立てる (呼び出しはしない)。
    ///
    /// `pFrom` は**二重 NUL 終端**のワイド文字列でなければならない
    /// (1 個しか付けないと隣のメモリまで対象として読まれる)。
    /// フラグは「確認ダイアログもエラー UI も出さず、元に戻せる形で削除」。
    /// 確認はアプリ側で既に取っているので OS 側では出さない。
    #[cfg(any(windows, test))]
    pub fn windows_delete_args(path: &Path) -> (Vec<u16>, u16) {
        // FOF_ALLOWUNDO(0x40) | FOF_NOCONFIRMATION(0x10) | FOF_SILENT(0x4)
        // | FOF_NOERRORUI(0x400) | FOF_WANTNUKEWARNING(0x4000)
        // WANTNUKEWARNING は「ごみ箱に入らず完全に消える」ときだけ OS に
        // 警告を出させるためのもの (黙って消えるのを防ぐ)。
        const FLAGS: u16 = 0x0040 | 0x0010 | 0x0004 | 0x0400 | 0x4000;
        let mut from: Vec<u16> = path.to_string_lossy().encode_utf16().collect();
        from.push(0);
        from.push(0);
        (from, FLAGS)
    }

    /// `path` を OS のゴミ箱へ送る。
    ///
    /// `Ok(Some(p))` = ゴミ箱の中の実体 (取り消しで戻せる)。
    /// `Ok(None)`    = 送れたが戻り先が分からない (Windows のごみ箱)。
    /// `Err(msg)`    = 送れなかった。**このとき対象は消えていない。**
    ///
    /// **他の担当が持っているファイルは消さない。** 消すのは上書きより
    /// 取り返しがつかないので、門は OS ごとの実装の**手前**に 1 つだけ置く
    /// (3 OS ぶんに散らすと、いつか 1 つだけ忘れる)。
    pub fn send(path: &Path) -> Result<Option<PathBuf>, String> {
        send_gated(path, &super::lease_deny)
    }

    /// 判定を差し替えられる中身。
    pub(super) fn send_gated(
        path: &Path,
        gate: super::Gate<'_>,
    ) -> Result<Option<PathBuf>, String> {
        if let Some(msg) = gate(path) {
            return Err(msg);
        }
        send_inner(path)
    }

    #[cfg(unix)]
    fn send_inner(path: &Path) -> Result<Option<PathBuf>, String> {
        let home = dirs::home_dir();
        let data = dirs::data_dir();
        let now = SystemTime::now();
        let plan = move_plan(path, home.as_deref(), data.as_deref(), now, &|p| {
            std::fs::symlink_metadata(p).is_ok()
        })?;
        std::fs::create_dir_all(&plan.files_dir)
            .map_err(|e| trf("ゴミ箱を用意できません: {e}", &[("e", e.to_string())]))?;
        if let Some((info_path, body)) = &plan.info {
            if let Some(d) = info_path.parent() {
                std::fs::create_dir_all(d)
                    .map_err(|e| trf("ゴミ箱を用意できません: {e}", &[("e", e.to_string())]))?;
            }
            std::fs::write(info_path, body)
                .map_err(|e| trf("ゴミ箱を用意できません: {e}", &[("e", e.to_string())]))?;
        }
        let dest = plan.files_dir.join(&plan.file_name);
        match std::fs::rename(path, &dest) {
            Ok(()) => Ok(Some(dest)),
            Err(e) => {
                // 予約した .trashinfo を残さない (幽霊エントリになる)
                if let Some((info_path, _)) = &plan.info {
                    let _ = std::fs::remove_file(info_path);
                }
                if e.raw_os_error() == Some(libc::EXDEV) {
                    // 別ボリューム。仕様上は <ボリューム>/.Trash-$uid だが、
                    // 場所を推測して書くより「消さずに理由を出す」方を選ぶ。
                    Err(tr(
                        "別のボリュームにあるためゴミ箱へ送れません (完全に削除するなら Shift を押しながら削除してください)",
                    ))
                } else {
                    Err(trf("ゴミ箱へ送れません: {e}", &[("e", e.to_string())]))
                }
            }
        }
    }

    #[cfg(windows)]
    fn send_inner(path: &Path) -> Result<Option<PathBuf>, String> {
        use windows_sys::Win32::UI::Shell::{SHFileOperationW, FO_DELETE, SHFILEOPSTRUCTW};
        if std::fs::symlink_metadata(path).is_err() {
            return Err(tr("削除できません: 見つかりません"));
        }
        let (from, flags) = windows_delete_args(path);
        // SAFETY: op は zeroed で埋めてから必要な項目だけ設定する。
        // pFrom は `from` を指し、`from` は呼び出しの間ずっと生きている。
        let rc = unsafe {
            let mut op: SHFILEOPSTRUCTW = std::mem::zeroed();
            op.wFunc = FO_DELETE;
            op.pFrom = from.as_ptr();
            op.fFlags = flags;
            let rc = SHFileOperationW(&mut op);
            if rc == 0 && op.fAnyOperationsAborted != 0 {
                return Err(tr("ゴミ箱への移動が中止されました"));
            }
            rc
        };
        if rc != 0 {
            return Err(trf(
                "ゴミ箱へ送れません (コード {code})",
                &[("code", rc.to_string())],
            ));
        }
        // ごみ箱の中のパスは API から返らないため、取り消しでは戻せない
        Ok(None)
    }

    #[cfg(not(any(unix, windows)))]
    fn send_inner(_path: &Path) -> Result<Option<PathBuf>, String> {
        Err(tr("この環境ではゴミ箱を使えません"))
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

    /// `.gitignore` の 3 経路。**既定は「残して薄く」** (VS Code と同じ) で、
    /// 「隠す」は `dim_ignored_files = false` を選んだときだけ。
    #[test]
    fn gitignore_dims_by_default_and_hides_only_when_asked() {
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

        // 既定: 消えずに残り、`ignored` が立つ (VS Code の既定と同じ見せ方。
        // 既定で消していると「置いたはずのファイルがツリーに無い」が黙って起きる)
        let cfg = crate::config::Config::default();
        assert!(cfg.dim_ignored_files, "既定は「隠さず薄く出す」");
        assert!(cfg.respect_gitignore, "無視の判定自体は既定でオン");
        let mut t = tree_with(&root, &cfg);
        let entries = t.entries(&root);
        let flag = |n: &str| entries.iter().find(|e| e.name == n).map(|e| e.ignored);
        assert_eq!(
            flag("node_modules"),
            Some(true),
            "無視ディレクトリも行に残る"
        );
        assert_eq!(flag("target"), Some(true));
        assert_eq!(flag("a.log"), Some(true), "パターン一致のファイルも残る");
        assert_eq!(flag("src"), Some(false), "無視されないものには印が付かない");
        assert_eq!(flag("README.md"), Some(false));

        // 隠す設定を選んだときだけ消える
        let hide = crate::config::Config {
            dim_ignored_files: false,
            ..crate::config::Config::default()
        };
        let mut t = tree_with(&root, &hide);
        assert_eq!(names_of(&mut t, &root), [".gitignore", "README.md", "src"]);

        // 設定で切れば印も付かない
        let off = crate::config::Config {
            respect_gitignore: false,
            ..crate::config::Config::default()
        };
        let mut t = tree_with(&root, &off);
        let entries = t.entries(&root);
        assert!(entries.iter().any(|e| e.name == "node_modules"));
        assert!(
            entries.iter().all(|e| !e.ignored),
            "尊重しない設定では無視の印が 1 つも立たない"
        );

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

        // 既定 (薄表示) では両方並ぶが、印が付くのは除外されたほうだけ
        let cfg = crate::config::Config::default();
        let mut t = tree_with(&root, &cfg);
        let logs = t.entries(&root.join("logs"));
        let flag = |n: &str| logs.iter().find(|e| e.name == n).map(|e| e.ignored);
        assert_eq!(
            flag("important.log"),
            Some(false),
            "`!` で戻したものは無視されない"
        );
        assert_eq!(flag("noise.log"), Some(true));

        // 隠す設定なら、戻したものだけが残る
        let hide = crate::config::Config {
            dim_ignored_files: false,
            ..crate::config::Config::default()
        };
        let mut t = tree_with(&root, &hide);
        assert_eq!(
            names_of(&mut t, &root.join("logs")),
            [".gitignore", "important.log"]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// 無視された行の色が「通常より弱いが読める」こと。
    ///
    /// **絶対値では線を引かない** — テーマごとに地の明るさが違うので固定の
    /// しきい値は必ず嘘になる。「通常色より背景に近い」という**関係**と、
    /// 「背景に沈み切っていない」下限だけを、ライト/ダーク両方で見る。
    #[test]
    fn 無視色は全テーマで通常色より背景に近く読める() {
        use crate::theme::contrast_ratio;
        let (mut dark, mut light) = (0usize, 0usize);
        for t in crate::theme::all() {
            if t.dark {
                dark += 1;
            } else {
                light += 1;
            }
            let normal = row_fg(&t, false, false, None);
            let ignored = row_fg(&t, false, true, None);
            assert_ne!(ignored, normal, "{}: 無視行が通常行と同じ色", t.name);
            // ツリーはサイドパネルの上に乗る (theme 適用時 panel_fill = panel)
            let ci = contrast_ratio(ignored, t.panel);
            let cn = contrast_ratio(normal, t.panel);
            assert!(
                ci < cn,
                "{}: 無視行が通常行より浮いている (無視 {ci:.2} / 通常 {cn:.2})",
                t.name
            );
            assert!(
                ci >= 4.0,
                "{}: 無視行が地に沈んで読めない ({ci:.2})",
                t.name
            );
        }
        assert!(
            dark > 0 && light > 0,
            "ライト/ダーク両方を見ている (dark={dark} light={light})"
        );
    }

    /// 行の前景色の優先順位を表で固定する。
    /// 切り取り待ち > 無視 > git ステータス > 通常 (無視は VS Code と同じく M/U に勝つ)。
    #[test]
    fn 無視はgitステータス色より優先される() {
        use crate::git::FileStatus;
        let t = crate::theme::all()
            .into_iter()
            .next()
            .expect("テーマがある");
        let cut = t.text.gamma_multiply(CUT_PENDING_ALPHA);
        let ign = ignored_fg(&t);
        let m = git_status_style(FileStatus::Modified, &t).0;
        let u = git_status_style(FileStatus::Untracked, &t).0;
        let cases: [(bool, bool, Option<FileStatus>, egui::Color32); 8] = [
            (false, false, None, t.text),
            (false, false, Some(FileStatus::Modified), m),
            (false, false, Some(FileStatus::Untracked), u),
            (false, true, None, ign),
            (false, true, Some(FileStatus::Modified), ign),
            (false, true, Some(FileStatus::Untracked), ign),
            (true, false, Some(FileStatus::Modified), cut),
            (true, true, Some(FileStatus::Modified), cut),
        ];
        for (cut_pending, ignored, st, want) in cases {
            assert_eq!(
                row_fg(&t, cut_pending, ignored, st),
                want,
                "cut={cut_pending} ignored={ignored} status={st:?}"
            );
        }
        assert_ne!(
            ign, m,
            "無視の色と M の色が同じでは優先順位を確かめられない"
        );
        assert_ne!(ign, u);
    }

    /// `.gitignore` を尊重しない設定なら、薄表示の設定が立っていても
    /// **薄くならない** (無視の判定そのものが無いため)。
    #[test]
    fn gitignoreを尊重しなければ薄表示にならない() {
        let root = unique_temp_dir("zaivern-tree-test", "no-respect");
        std::fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();

        let off = crate::config::Config {
            respect_gitignore: false,
            dim_ignored_files: true,
            ..crate::config::Config::default()
        };
        let mut t = tree_with(&root, &off);
        assert!(!t.dim_ignored, "尊重しない設定では薄表示も立たない");
        let entries = t.entries(&root);
        let nm = entries
            .iter()
            .find(|e| e.name == "node_modules")
            .expect("尊重しない設定なら当然出る");
        assert!(!nm.ignored, "無視の印が付かない");
        let theme = crate::theme::all()
            .into_iter()
            .next()
            .expect("テーマがある");
        assert_eq!(
            row_fg(&theme, false, nm.ignored, None),
            theme.text,
            "通常色のまま"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// 巨大な無視ディレクトリを既定で表示しても、**中身は開くまで読まない**。
    /// 実時間ではなく `read_dir` の**回数**で固定する (時間は必ず嘘をつく)。
    #[test]
    fn 無視ディレクトリを既定表示しても中身は読まない() {
        let root = unique_temp_dir("zaivern-tree-test", "lazy-ignored");
        std::fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        let nm = root.join("node_modules");
        std::fs::create_dir_all(&nm).unwrap();
        for i in 0..200 {
            std::fs::create_dir_all(nm.join(format!("p{i:03}"))).unwrap();
        }
        std::fs::write(root.join("README.md"), "x").unwrap();

        let cfg = crate::config::Config::default();
        let mut t = tree_with(&root, &cfg);
        let ctx = egui::Context::default();

        let rows = t.visible_rows(&ctx);
        assert!(
            rows.iter().any(|r| r.name == "node_modules"),
            "無視ディレクトリ自体は既定で出る"
        );
        assert!(
            !rows.iter().any(|r| r.name.starts_with('p')),
            "畳んだままなら中身は 1 行も出ない"
        );
        assert_eq!(t.io_reads, 1, "ルートの 1 階層しか読まない (中身は遅延)");

        // 開いたときだけ 1 階層ぶん読み足す
        set_open(&ctx, &nm, true);
        let rows = t.visible_rows(&ctx);
        assert!(
            rows.iter().any(|r| r.name.starts_with('p')),
            "開けば中身が出る"
        );
        assert_eq!(t.io_reads, 2, "開いた 1 階層だけを読み足す");

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

    /// **無視フォルダ (`target/` `node_modules/` …) の中までは探さない。**
    ///
    /// 薄表示の設定 (既定) では無視フォルダも行としては出るので、以前は
    /// 絞り込みの走査もそこへ降りていた。このリポジトリ自身の `target/` は
    /// **154GB / 25 万件以上**あるので、1 文字打つたびに数秒固まったうえ、
    /// 予算をそこで使い切って**肝心のソースまで辿り着かなかった**。
    #[test]
    fn 絞り込みは無視フォルダの中まで探さない() {
        let root = unique_temp_dir("zaivern-tree-test", "filter-ignored");
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        let t_dir = root.join("target");
        std::fs::create_dir_all(t_dir.join("deep")).unwrap();
        std::fs::write(t_dir.join("widget.rs"), "").unwrap();
        std::fs::write(t_dir.join("deep").join("widget.rs"), "").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("widget.rs"), "").unwrap();

        let cfg = crate::config::Config::default();
        assert!(cfg.respect_gitignore, "既定は .gitignore を尊重する");
        let mut t = tree_with(&root, &cfg);
        let ctx = egui::Context::default();
        t.filter = "widget".into();
        while {
            t.recompute_filter(&ctx);
            t.filter_scan.is_some()
        } {}
        let hit = t.filter_hit.as_ref().expect("結果");
        assert!(
            hit.keep.contains(&root.join("src").join("widget.rs")),
            "追跡しているファイルは見つかる"
        );
        assert!(
            !hit.keep.contains(&t_dir.join("widget.rs")),
            "無視フォルダの中まで探している"
        );
        assert!(
            !hit.keep.contains(&t_dir.join("deep").join("widget.rs")),
            "無視フォルダの奥まで降りている"
        );
        assert_eq!(hit.matched, 1);

        // 尊重をやめれば中まで探す (「探せない」で終わらせない)
        let mut cfg2 = cfg.clone();
        cfg2.respect_gitignore = false;
        let mut t2 = tree_with(&root, &cfg2);
        t2.filter = "widget".into();
        while {
            t2.recompute_filter(&ctx);
            t2.filter_scan.is_some()
        } {}
        assert_eq!(t2.filter_hit.as_ref().expect("結果").matched, 3);

        std::fs::remove_dir_all(&root).ok();
    }

    /// **どんな文字列でも 1 フレームの費用は同じ。**
    ///
    /// `.` のように「ほぼ全部に当たる」クエリを打つと、以前は 1 打鍵ごとに
    /// 予算 (既定 50,000 件) いっぱいまで同期で辿り切っていたので、大きな
    /// リポジトリでは打つたびに数秒フリーズした。
    ///
    /// 見張るのは**実時間ではなく回数** (絶対時間の線は必ず嘘をつく):
    ///   * 1 回の `recompute_filter` が辿る件数は [`FILTER_STEP`] + α で頭打ち
    ///   * 一致は [`FILTER_MAX_HITS`] で頭打ち (展開して描く量が有限になる)
    ///   * 何度か呼べば必ず終わる (走査が永久に走り続けない)
    #[test]
    fn 絞り込みは一度に少しだけ辿り一致も頭打ちにする() {
        let root = unique_temp_dir("zaivern-tree-test", "filter-step");
        // 1 階層あたりを小さくして「1 ディレクトリで予算を使い切る」影響を消す
        // (刻めていることを見たいので、刻み目をまたぐ形にする)。
        let per_dir = 40usize;
        let dirs = 60usize;
        for d in 0..dirs {
            let sub = root.join(format!("d{d:03}"));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..per_dir {
                std::fs::write(sub.join(format!("f{f:03}.txt")), "").unwrap();
            }
        }
        let total = dirs * per_dir + dirs; // ファイル + ディレクトリ
        assert!(total > FILTER_STEP, "刻み目をまたぐ大きさで試す");

        let cfg = crate::config::Config::default();
        let mut t = tree_with(&root, &cfg);
        let ctx = egui::Context::default();

        // "." はこのツリーのファイル全部に当たる (拡張子の点)
        t.filter = ".".into();
        t.recompute_filter(&ctx);
        let first = t.filter_hit.as_ref().expect("途中でも結果を出す");
        assert!(
            first.scanning || first.truncated,
            "1 回で全部辿り切っている (刻めていない)"
        );
        let visited_once = t.filter_scan.as_ref().map(|s| s.visited).unwrap_or(0);
        if visited_once > 0 {
            assert!(
                visited_once <= FILTER_STEP + per_dir + 1,
                "1 回で {visited_once} 件辿った (上限 {} 件)",
                FILTER_STEP + per_dir + 1
            );
        }

        // 何度呼んでも必ず終わる。終わったら走査は 1 本も残らない
        let mut rounds = 0usize;
        while t.filter_scan.is_some() {
            t.recompute_filter(&ctx);
            rounds += 1;
            assert!(rounds < 1000, "走査が終わらない");
        }
        let hit = t.filter_hit.as_ref().expect("結果");
        assert!(!hit.scanning, "終わったのに走査中のまま");
        assert!(
            hit.matched <= FILTER_MAX_HITS,
            "一致が頭打ちになっていない ({} 件)",
            hit.matched
        );
        assert!(hit.truncated, "打ち切ったことを伝えていない");

        // 一致しないクエリでも、1 回あたりの費用は同じ (刻んで終わる)
        t.filter = "。".into();
        t.recompute_filter(&ctx);
        let mut rounds = 0usize;
        while t.filter_scan.is_some() {
            t.recompute_filter(&ctx);
            rounds += 1;
            assert!(rounds < 1000, "走査が終わらない");
        }
        let hit = t.filter_hit.as_ref().expect("結果");
        assert_eq!(hit.matched, 0);
        assert!(!hit.scanning);

        // 走査は打鍵ごとにやり直す。前のクエリの結果を引きずらない
        assert_eq!(hit.query, "。");

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
    fn transfer_plan_rules() {
        let dir = unique_temp_dir("zaivern-tree-test", "transfer-plan");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("mkdir");
        let file = dir.join("f.txt");
        std::fs::write(&file, "x").expect("write");

        // コピー: 同じフォルダへは "f copy.txt" が生える (衝突しないので確認不要)
        let plan = transfer_plan(&file, &dir, Transfer::Copy, Numbering::Auto)
            .expect("plan")
            .expect("some");
        assert_eq!(plan.dest, dir.join("f copy.txt"));
        assert_eq!(plan.clash, None);
        // 同じフォルダへのコピーは Ask を渡しても採番になる (複製ジェスチャ)
        let plan = transfer_plan(&file, &dir, Transfer::Copy, Numbering::Ask)
            .expect("plan")
            .expect("some");
        assert_eq!(plan.dest, dir.join("f copy.txt"));
        assert_eq!(plan.clash, None);
        // 移動: 同じフォルダへは何もしない
        assert_eq!(
            transfer_plan(&file, &dir, Transfer::Move, Numbering::Ask).expect("plan"),
            None
        );
        // 移動: 別フォルダへは衝突なしで移動
        let plan = transfer_plan(&file, &sub, Transfer::Move, Numbering::Ask)
            .expect("plan")
            .expect("some");
        assert_eq!(plan.dest, sub.join("f.txt"));
        assert_eq!(plan.clash, None);
        assert_eq!(plan.kind, Transfer::Move);
        // フォルダを自分の中へは入れられない
        assert!(transfer_plan(&dir, &sub, Transfer::Copy, Numbering::Ask).is_err());
        assert!(transfer_plan(&dir, &dir, Transfer::Move, Numbering::Ask).is_err());
        // 消えたソースはエラー
        assert!(
            transfer_plan(&dir.join("gone.txt"), &sub, Transfer::Copy, Numbering::Ask).is_err()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // ─── 同名衝突の検出 ─────────────────────────────────────────

    #[test]
    fn 衝突検出は同名だけを拾い大文字小文字違いは拾わない() {
        let dir = unique_temp_dir("zaivern-tree-test", "clash-case");
        let lower = dir.join("a.txt");
        let upper = dir.join("A.txt");
        std::fs::write(&lower, "x").expect("write");

        // 同名が無い → 衝突なし
        assert_eq!(clash_at(&lower, &dir.join("b.txt")), None);

        // 同名がある → 上書きの衝突
        let other = dir.join("b.txt");
        std::fs::write(&other, "y").expect("write");
        assert_eq!(clash_at(&other, &lower), Some(Clash::Overwrite));

        // 大文字小文字だけが違う名前。
        // cfg! ではなく実測で分岐する: macOS には大小を区別する APFS ボリューム
        // が作れるし、Linux でも大小無視の FS をマウントできるため、
        // 「mac/Windows なら必ず case-insensitive」は成り立たない。
        let insensitive = upper.exists();
        if insensitive {
            // macOS / Windows の既定。exists() は真になるが、同じ実体なので
            // 衝突ではない (ここを衝突とすると case-only rename が永久にできない)
            assert!(
                same_entry(&lower, &upper),
                "大小違いの綴りは同じ実体として畳まれる"
            );
        } else {
            // Linux 等の case-sensitive: そもそも存在しない
            assert!(!upper.exists());
        }
        assert_eq!(
            clash_at(&lower, &upper),
            None,
            "a.txt → A.txt は大小どちらの FS でも衝突ではない"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 衝突検出はフォルダ同士とファイルフォルダ違いを区別する() {
        let dir = unique_temp_dir("zaivern-tree-test", "clash-kind");
        let src_dir = dir.join("src").join("shared");
        let dst_dir = dir.join("dst").join("shared");
        std::fs::create_dir_all(&src_dir).expect("mkdir");
        std::fs::create_dir_all(&dst_dir).expect("mkdir");
        // フォルダ同士 → マージ (上書きではない)
        assert_eq!(clash_at(&src_dir, &dst_dir), Some(Clash::Merge));

        // ファイル → フォルダ (フォルダが中身ごと消える)
        let f = dir.join("src").join("thing");
        std::fs::write(&f, "x").expect("write");
        assert_eq!(
            clash_at(&f, &dst_dir),
            Some(Clash::Mismatch { dest_is_dir: true })
        );
        // フォルダ → ファイル
        let g = dir.join("dst").join("thing");
        std::fs::write(&g, "y").expect("write");
        assert_eq!(
            clash_at(&src_dir, &g),
            Some(Clash::Mismatch { dest_is_dir: false })
        );
        // ファイル → ファイル
        assert_eq!(clash_at(&f, &g), Some(Clash::Overwrite));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 衝突検出はシンボリックリンクも既存として数える() {
        let dir = unique_temp_dir("zaivern-tree-test", "clash-link");
        let real = dir.join("real.txt");
        std::fs::write(&real, "x").expect("write");
        let src = dir.join("src.txt");
        std::fs::write(&src, "y").expect("write");
        let link = dir.join("link.txt");
        let broken = dir.join("broken.txt");

        #[cfg(unix)]
        let made = {
            std::os::unix::fs::symlink(&real, &link).expect("symlink");
            // 壊れたリンク: exists() は false でも rename は黙って潰す
            std::os::unix::fs::symlink(dir.join("nope"), &broken).expect("symlink");
            true
        };
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&real, &link).is_ok()
            && std::os::windows::fs::symlink_file(dir.join("nope"), &broken).is_ok();

        if made {
            assert_eq!(
                clash_at(&src, &link),
                Some(Clash::Overwrite),
                "リンクは辿らず、リンク自身が既存として衝突する"
            );
            assert!(!broken.exists(), "壊れたリンクは exists() では見えない");
            assert_eq!(
                clash_at(&src, &broken),
                Some(Clash::Overwrite),
                "壊れたリンクも既存として数える (rename が黙って潰すため)"
            );
        } else {
            // Windows で開発者モードが無いとリンクを作れない。その環境では
            // 「リンクを作れないので試験対象が無い」だけで、判定式は同じ。
            assert_eq!(clash_at(&src, &real), Some(Clash::Overwrite));
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn パス区切りは環境をまたいで同じ計画になる() {
        let dir = unique_temp_dir("zaivern-tree-test", "sep");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("mkdir");
        let file = dir.join("f.txt");
        std::fs::write(&file, "x").expect("write");

        // OS の区切り文字で文字列から組んだ宛先でも、join で組んだものと同じ計画
        let sep = if cfg!(windows) { "\\" } else { "/" };
        let dest_dir = PathBuf::from(format!("{}{}sub", dir.display(), sep));
        let plan = transfer_plan(&file, &dest_dir, Transfer::Move, Numbering::Ask)
            .expect("plan")
            .expect("some");
        assert_eq!(plan.dest, sub.join("f.txt"));

        if cfg!(windows) {
            // Windows は '/' も区切りとして受け付ける
            let alt = PathBuf::from(format!("{}/sub", dir.display()));
            let p2 = transfer_plan(&file, &alt, Transfer::Move, Numbering::Ask)
                .expect("plan")
                .expect("some");
            assert_eq!(p2.dest.file_name(), Some(std::ffi::OsStr::new("f.txt")));
            assert!(p2.dest.parent().is_some_and(|d| d.ends_with("sub")));
        } else {
            // Unix では '\\' はただの文字。フォルダの区切りにはならない
            assert_eq!(
                Path::new("a\\b").file_name(),
                Some(std::ffi::OsStr::new("a\\b")),
                "Unix では逆スラッシュは名前の一部"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    // ─── 自動採番 ───────────────────────────────────────────────

    #[test]
    fn 自動採番は多重拡張子と連番を守る() {
        let dir = unique_temp_dir("zaivern-tree-test", "numbering");
        // 拡張子なし
        std::fs::write(dir.join("a"), "x").expect("write");
        assert_eq!(next_paste_path(&dir, "a", false), dir.join("a copy"));
        // 単一拡張子
        std::fs::write(dir.join("a.txt"), "x").expect("write");
        assert_eq!(
            next_paste_path(&dir, "a.txt", false),
            dir.join("a copy.txt")
        );
        // 多重拡張子: VS Code と同じく**最後のドットだけ**を拡張子とみなす
        std::fs::write(dir.join("a.tar.gz"), "x").expect("write");
        assert_eq!(
            next_paste_path(&dir, "a.tar.gz", false),
            dir.join("a.tar copy.gz")
        );
        // "a copy.txt" が既にある → "a copy 2.txt"
        std::fs::write(dir.join("a copy.txt"), "x").expect("write");
        assert_eq!(
            next_paste_path(&dir, "a.txt", false),
            dir.join("a copy 2.txt")
        );
        // 2 まである → 3
        std::fs::write(dir.join("a copy 2.txt"), "x").expect("write");
        assert_eq!(
            next_paste_path(&dir, "a.txt", false),
            dir.join("a copy 3.txt")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 自動採番は_999_まで埋まっていても続く() {
        let dir = unique_temp_dir("zaivern-tree-test", "numbering-999");
        std::fs::write(dir.join("a.txt"), "x").expect("write");
        std::fs::write(dir.join("a copy.txt"), "x").expect("write");
        for n in 2..=999u32 {
            std::fs::write(dir.join(format!("a copy {n}.txt")), "x").expect("write");
        }
        assert_eq!(
            next_paste_path(&dir, "a.txt", false),
            dir.join("a copy 1000.txt"),
            "3 桁で止まらない (桁上がりを文字列比較でやっていない)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ─── フォルダの統合 ─────────────────────────────────────────

    #[test]
    fn フォルダの統合は中身を一件ずつの計画へ展開する() {
        let dir = unique_temp_dir("zaivern-tree-test", "merge");
        let src = dir.join("src").join("shared");
        let dst = dir.join("dst").join("shared");
        std::fs::create_dir_all(src.join("deep")).expect("mkdir");
        std::fs::create_dir_all(dst.join("deep")).expect("mkdir");
        std::fs::write(src.join("both.txt"), "s").expect("write");
        std::fs::write(dst.join("both.txt"), "d").expect("write");
        std::fs::write(src.join("only-src.txt"), "s").expect("write");
        std::fs::write(src.join("deep").join("x.txt"), "s").expect("write");
        std::fs::write(dst.join("deep").join("x.txt"), "d").expect("write");

        let items = expand_merge(&src, &dst, Transfer::Move).expect("expand");
        let clashing: Vec<&TransferItem> = items.iter().filter(|i| i.clash.is_some()).collect();
        assert_eq!(
            clashing.len(),
            2,
            "重なるのは both.txt と deep/x.txt の 2 件"
        );
        assert!(items
            .iter()
            .any(|i| i.dest == dst.join("only-src.txt") && i.clash.is_none()));
        assert!(
            items
                .iter()
                .any(|i| i.dest == dst.join("deep").join("x.txt")
                    && i.clash == Some(Clash::Overwrite))
        );
        // フォルダ自身は項目にならない (上書きではなくマージなので)
        assert!(!items.iter().any(|i| i.src == src.join("deep")));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 統合の後始末は空フォルダだけを畳む() {
        let dir = unique_temp_dir("zaivern-tree-test", "prune");
        let src = dir.join("src");
        let dst = dir.join("dst");
        std::fs::create_dir_all(src.join("empty").join("nested")).expect("mkdir");
        std::fs::create_dir_all(src.join("kept")).expect("mkdir");
        std::fs::write(src.join("kept").join("stay.txt"), "x").expect("write");
        std::fs::create_dir_all(&dst).expect("mkdir");

        // コピー (remove_src=false): 階層だけ作り、元は 1 つも触らない
        prune_merged_dirs(&src, &dst, false);
        assert!(
            dst.join("empty").join("nested").is_dir(),
            "コピーでも空フォルダの階層は移動先へ作る"
        );
        assert!(src.join("empty").exists(), "コピーは元を消さない");

        // 移動 (remove_src=true): 空になったものだけ畳む
        prune_merged_dirs(&src, &dst, true);
        assert!(!src.join("empty").exists(), "空フォルダは畳まれる");
        assert!(
            dst.join("empty").join("nested").is_dir(),
            "階層は移動先に残る"
        );
        assert!(
            src.join("kept").join("stay.txt").exists(),
            "中身が残っているフォルダは絶対に消さない"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ─── ゴミ箱 (実ゴミ箱には一切触らない) ─────────────────────

    #[test]
    fn ゴミ箱の計画は実ゴミ箱に触れずに組み立てられる() {
        // home / data は一時ディレクトリを注入する = 実 ~/.Trash には触らない。
        // 「取られている名前」も注入した述語で答えるので fs も読まない。
        let home = PathBuf::from("/zv-test-home");
        let data = PathBuf::from("/zv-test-data");
        let target = PathBuf::from("/zv-test-work/やった.txt");
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);

        let none_taken = |_: &Path| false;
        let plan =
            trash::move_plan(&target, Some(&home), Some(&data), now, &none_taken).expect("plan");
        if cfg!(target_os = "macos") {
            assert_eq!(plan.files_dir, home.join(".Trash"));
            assert!(
                plan.info.is_none(),
                "macOS の ~/.Trash に .trashinfo は無い"
            );
        } else {
            assert_eq!(plan.files_dir, data.join("Trash").join("files"));
            let (info_path, body) = plan.info.as_ref().expect("freedesktop は info を書く");
            assert_eq!(
                info_path,
                &data.join("Trash").join("info").join("やった.txt.trashinfo")
            );
            assert!(body.starts_with("[Trash Info]\n"));
            assert!(
                body.contains("Path=/zv-test-work/%E3%82%84%E3%81%A3%E3%81%9F.txt"),
                "元のパスは percent-encode されて入る: {body}"
            );
            assert!(body.contains("DeletionDate=2023-11-14T22:13:20"));
        }
        assert_eq!(plan.file_name, std::ffi::OsString::from("やった.txt"));

        // 同名が埋まっていたら .2 .3 と避ける
        let taken = |p: &Path| {
            let n = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            n == "やった.txt" || n == "やった.txt.2"
        };
        let plan = trash::move_plan(&target, Some(&home), Some(&data), now, &taken).expect("plan");
        assert_eq!(plan.file_name, std::ffi::OsString::from("やった.txt.3"));

        // home / data が取れない環境では**消さずに理由を返す**
        assert!(trash::move_plan(&target, None, None, now, &none_taken).is_err());
    }

    #[test]
    fn ゴミ箱の日付とエンコードは仕様どおり() {
        assert_eq!(trash::iso8601_utc(0), "1970-01-01T00:00:00");
        assert_eq!(trash::iso8601_utc(951_782_400), "2000-02-29T00:00:00");
        assert_eq!(trash::iso8601_utc(1_700_000_000), "2023-11-14T22:13:20");
        assert_eq!(trash::pct_encode("/a b/c#d"), "/a%20b/c%23d");
        assert_eq!(trash::pct_encode("/plain-_.~/x"), "/plain-_.~/x");
    }

    #[test]
    fn ごみ箱を呼ぶ引数は二重ヌルで終わる() {
        let (from, flags) = trash::windows_delete_args(Path::new("C:\\tmp\\a.txt"));
        assert_eq!(
            &from[from.len() - 2..],
            &[0u16, 0u16],
            "pFrom は二重 NUL 終端でないと隣のメモリまで対象になる"
        );
        let text: String = String::from_utf16(&from[..from.len() - 2]).expect("utf16");
        assert_eq!(text, "C:\\tmp\\a.txt");
        // FOF_ALLOWUNDO が無いと「ごみ箱へ入れたつもりが完全削除」になる
        assert_eq!(flags & 0x0040, 0x0040, "FOF_ALLOWUNDO は必須");
        assert_eq!(flags & 0x0010, 0x0010, "確認はアプリ側で済ませている");
    }

    // ─── 取り消し ───────────────────────────────────────────────

    #[test]
    fn 取り消しはリネームと移動とゴミ箱行きを戻す() {
        let dir = unique_temp_dir("zaivern-tree-test", "undo");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, "x").expect("write");

        // リネーム → 取り消し
        std::fs::rename(&a, &b).expect("rename");
        assert!(move_back(&b, &a).is_ok());
        assert!(a.exists() && !b.exists());

        // 移動 → 取り消し
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("mkdir");
        let moved = sub.join("a.txt");
        std::fs::rename(&a, &moved).expect("rename");
        assert!(move_back(&moved, &a).is_ok());
        assert!(a.exists() && !moved.exists());

        // 削除(ゴミ箱行き) → 取り消し。ゴミ箱は一時ディレクトリで代用する
        // (実ゴミ箱へは物を入れない)
        let fake_trash = dir.join("trash-files");
        std::fs::create_dir_all(&fake_trash).expect("mkdir");
        let trashed = fake_trash.join("a.txt");
        std::fs::rename(&a, &trashed).expect("rename");
        assert!(move_back(&trashed, &a).is_ok());
        assert!(a.exists() && !trashed.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 取り消しは対象が消えていても戻り先が塞がっていても壊さない() {
        let dir = unique_temp_dir("zaivern-tree-test", "undo-fail");
        let a = dir.join("a.txt");
        let gone = dir.join("gone.txt");
        std::fs::write(&a, "keep").expect("write");

        // 戻す対象が消えている → 失敗するだけ
        let e = move_back(&gone, &dir.join("x.txt")).expect_err("消えていれば失敗");
        assert!(e.contains("見つかりません"), "{e}");
        assert!(!dir.join("x.txt").exists());

        // 戻り先が既に埋まっている → 上書きせずに失敗する (ここが一番大事)
        let other = dir.join("other.txt");
        std::fs::write(&other, "other").expect("write");
        let e = move_back(&other, &a).expect_err("塞がっていれば失敗");
        assert!(e.contains("既にあります"), "{e}");
        assert_eq!(std::fs::read_to_string(&a).expect("read"), "keep");
        assert_eq!(std::fs::read_to_string(&other).expect("read"), "other");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 履歴は上限で古いものから落ちる() {
        let mut h = FileHistory::default();
        assert_eq!(h.hint(), None);
        for i in 0..FileHistory::MAX + 5 {
            h.push(FileOp::Create {
                path: PathBuf::from(format!("f{i}")),
                is_dir: false,
            });
        }
        h.push(FileOp::Rename {
            from: PathBuf::from("a"),
            to: PathBuf::from("b"),
        });
        assert_eq!(h.hint(), Some(tr("名前の変更")));
        let mut n = 0;
        while h.pop().is_some() {
            n += 1;
        }
        assert_eq!(n, FileHistory::MAX, "上限を超えて溜め込まない");
    }

    // ─── 構造検査 (ソースを読んで不変条件を固定する) ─────────────

    /// ソースを「関数名 → その関数の本文(次の fn まで)」へ大雑把に切る。
    /// CRLF を先に潰すのは Windows のチェックアウト対策。
    fn split_fns(src: &str) -> Vec<(String, String)> {
        let re = regex::Regex::new(
            r"(?m)^\s*(?:pub(?:\([a-z()]+\))?\s+)?(?:async\s+)?fn\s+([A-Za-z0-9_]+)",
        )
        .expect("regex");
        let mut marks: Vec<(usize, String)> = re
            .captures_iter(src)
            .map(|c| {
                (
                    c.get(0).expect("m").start(),
                    c.get(1).expect("name").as_str().to_string(),
                )
            })
            .collect();
        marks.push((src.len(), String::new()));
        marks
            .windows(2)
            .map(|w| (w[0].1.clone(), src[w[0].0..w[1].0].to_string()))
            .collect()
    }

    /// `needle` を含む関数の名前 (重複除去・整列)。
    fn owners(fns: &[(String, String)], needle: &str) -> Vec<String> {
        let mut v: Vec<String> = fns
            .iter()
            .filter(|(_, b)| b.contains(needle))
            .map(|(n, _)| n.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    fn body(fns: &[(String, String)], name: &str) -> String {
        fns.iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| b.clone())
            .unwrap_or_else(|| panic!("{name} が見つからない"))
    }

    /// app のソースを関数へ切る。
    ///
    /// **`SRC_IMPL` は `#[cfg(test)]` も含む。** つまり `app/*.rs` の中に
    /// テスト用の `remove_dir_all` を書くと、この検査は製品コードの削除と
    /// 区別できずに落ちる。それでよい — 後始末は
    /// `test_util::unique_temp_dir` の掃除に任せる作法なので、
    /// **テストでも直に消さない**のが正しい (実際に `remote_api.rs` の
    /// テストがこれで落ち、後始末を消して直した)。
    fn app_fns() -> Vec<(String, String)> {
        let src = crate::app::SRC_IMPL.replace("\r\n", "\n");
        split_fns(&src)
    }

    #[test]
    fn 破壊的なファイル操作は確認を経ずに呼ばれない() {
        let fns = app_fns();

        // ① 復元できない fs 操作 (完全削除 / 置き換えのための退避) を持つ関数は
        //    この 2 つだけ。増えたらここが落ちる = レビューを強制する。
        let mut destructive = owners(&fns, "std::fs::remove_dir_all(");
        destructive.extend(owners(&fns, "std::fs::remove_file("));
        destructive.sort();
        destructive.dedup();
        assert_eq!(
            destructive,
            vec!["delete_permanently".to_string(), "replace_dest".to_string()],
            "復元できない削除は delete_permanently / replace_dest の中だけに置くこと"
        );

        // ② その 2 つへ至る経路は 1 本ずつしかない
        assert_eq!(
            owners(&fns, "self.delete_permanently("),
            vec!["perform_delete".to_string()],
        );
        assert_eq!(
            owners(&fns, "self.perform_delete("),
            vec!["delete_confirm_ui".to_string()],
            "削除の実体は確認ダイアログからしか呼ばない"
        );
        assert_eq!(
            owners(&fns, "self.replace_dest("),
            vec!["run_transfer_item".to_string()],
        );
        assert_eq!(
            owners(&fns, "self.run_transfer_item("),
            vec!["drain_transfer".to_string()],
        );
        assert_eq!(
            owners(&fns, "self.drain_transfer("),
            vec!["transfer_confirm_ui".to_string()],
        );

        // ③ 削除の実体は「ユーザーが決めた」分岐の後ろにしか無い
        let dc = body(&fns, "delete_confirm_ui");
        let decided = dc.find("Some(true) =>").expect("決定の分岐がある");
        let call = dc.find("self.perform_delete(").expect("呼び出しがある");
        assert!(
            decided < call,
            "確認の答えを見る前に削除を実行している (デフォルトで消えてしまう)"
        );

        // ④ 上書きは衝突の答えを持つ項目でしか起きない
        let rt = body(&fns, "run_transfer_item");
        assert!(
            rt.contains("if item.clash.is_some()"),
            "衝突が無い項目で置き換え (削除) を走らせてはいけない"
        );

        // ⑤ 実行してよいかの判定は純粋関数 1 つに寄せる
        //    (テストで固定した実装と、動いている実装を食い違わせない)
        assert_eq!(
            owners(&fns, "file_tree::queue_answer("),
            vec!["drain_transfer".to_string()],
        );

        // ⑥ 完全削除は履歴へ積まない (積むと「戻せる」と嘘になる)。
        //    「戻せるもの」と「戻せないもの」を関数の境界で分けてあるので、
        //    完全削除側の関数に履歴が出てこないことだけ確かめればよい。
        assert!(
            !body(&fns, "delete_permanently").contains("push_file_op"),
            "完全削除は取り消せないので履歴へ積まないこと"
        );
        // 履歴へ積むゴミ箱行きは、戻り先が分かる経路からしか来ない
        assert_eq!(
            owners(&fns, "FileOp::Trash {"),
            vec!["perform_delete".to_string(), "revert_file_op".to_string()],
        );
    }

    #[test]
    fn 複数選択の一括移動は答えを一度だけ聞けば残り全部に効く() {
        // 「すべてに適用」の実体は queue_answer。ここを通って初めて実行される
        // ので、これが None を返すあいだは 1 バイトも動かない。
        // 衝突なしは常に実行 (聞かない)
        assert_eq!(queue_answer(None, None, None), Some(true));
        // 衝突あり + 未回答 → 必ず聞く
        assert_eq!(queue_answer(Some(Clash::Overwrite), None, None), None);
        // 「すべてに適用」が決まっていれば残り全部に効く
        assert_eq!(
            queue_answer(Some(Clash::Overwrite), None, Some(true)),
            Some(true)
        );
        assert_eq!(
            queue_answer(Some(Clash::Overwrite), None, Some(false)),
            Some(false)
        );
        // 1 件ぶんの答えは「すべてに適用」より優先される
        assert_eq!(
            queue_answer(Some(Clash::Merge), Some(false), Some(true)),
            Some(false)
        );

        // 実際に 3 件を一括移動して、答えが 1 回で済むことを確かめる
        let dir = unique_temp_dir("zaivern-tree-test", "apply-all");
        let src = dir.join("src");
        let dst = dir.join("dst");
        std::fs::create_dir_all(&src).expect("mkdir");
        std::fs::create_dir_all(&dst).expect("mkdir");
        for n in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(src.join(n), "new").expect("write");
            std::fs::write(dst.join(n), "old").expect("write");
        }
        // 複数選択のドロップ = 1 ジョブに 3 件
        let items: Vec<TransferItem> = ["a.txt", "b.txt", "c.txt"]
            .iter()
            .map(|n| {
                transfer_plan(&src.join(n), &dst, Transfer::Move, Numbering::Ask)
                    .expect("plan")
                    .expect("some")
            })
            .collect();
        assert!(
            items.iter().all(|i| i.clash == Some(Clash::Overwrite)),
            "3 件とも衝突する"
        );

        // app 側の drain_transfer と同じ順路を辿る (判定は同じ純粋関数)
        let mut asked = 0usize;
        let mut apply_all: Option<bool> = None;
        for it in &items {
            let mut answer = queue_answer(it.clash, None, apply_all);
            if answer.is_none() {
                // ここが確認ダイアログ。「置き換える」+「すべてに適用」を選ぶ
                asked += 1;
                apply_all = Some(true);
                answer = Some(true);
            }
            if answer == Some(true) {
                std::fs::remove_file(&it.dest).expect("replace");
                std::fs::rename(&it.src, &it.dest).expect("move");
            }
        }
        assert_eq!(asked, 1, "3 件でも聞かれるのは 1 回だけ");
        for n in ["a.txt", "b.txt", "c.txt"] {
            assert_eq!(
                std::fs::read_to_string(dst.join(n)).expect("read"),
                "new",
                "{n} が置き換わっている"
            );
            assert!(!src.join(n).exists(), "{n} は移動済み");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 複数選択の対象は掴んだ行が選択に入っているときだけ集合になる() {
        let dir = unique_temp_dir("zaivern-tree-test", "sel-targets");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        let c = dir.join("c.txt");
        for p in [&a, &b, &c] {
            std::fs::write(p, "x").expect("write");
        }
        let mut t = FileTree::new(vec![dir.clone()], true);
        t.sel.set_single(&a);
        t.sel.toggle(&b);
        // 選択に入っている行を掴んだら選択集合ごと
        let mut got = t.selection_targets(&a);
        got.sort();
        assert_eq!(got, vec![a.clone(), b.clone()]);
        // 選択の外を掴んだらその 1 件だけ (VS Code と同じ)
        assert_eq!(t.selection_targets(&c), vec![c.clone()]);
        // ルートは絶対に対象にしない (ワークスペースごと動かさない)
        assert!(t.selection_targets(&dir).is_empty());
        // 削除の対象も同じ規則 (実装を 1 本にしてある)
        let mut del = t.delete_targets(&a);
        del.sort();
        assert_eq!(del, got);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 見えなくなるドロップ先は黙って受けずに知らせる() {
        // 普通のフォルダ: 何も言わない
        assert_eq!(drop_visibility_notice("src", false, false), None);
        // .gitignore の対象へ入れた: ツリーから消えるので知らせる
        let m = drop_visibility_notice("target", true, false).expect("注意が出る");
        assert!(m.contains("target") && m.contains(".gitignore"), "{m}");
        // 絞り込み中: 行は本物のツリーのままなので受け付けるが、
        // 条件に合わないものは出ないことを知らせる
        let m = drop_visibility_notice("src", false, true).expect("注意が出る");
        assert!(m.contains("絞り込み"), "{m}");
        // 両方なら .gitignore の方を出す (消える理由として強い)
        let m = drop_visibility_notice("target", true, true).expect("注意が出る");
        assert!(m.contains(".gitignore"), "{m}");
    }

    #[test]
    fn ファイル操作の取り消しはツリーがフォーカスを持つときだけ発火する() {
        let src = include_str!("file_tree.rs").replace("\r\n", "\n");
        let cut = src.find("\n#[cfg(test)]\nmod ").unwrap_or(src.len());
        let fns = split_fns(&src[..cut]);

        // 取り消し要求を立てるのは、キー処理とツリーのメニューだけ
        assert_eq!(
            owners(&fns, "actions.undo = true"),
            vec!["file_ops_menu".to_string(), "keys_undo".to_string()],
            "ツリーの外から取り消しを立てないこと"
        );
        // キー経路は handle_keys からしか来ない
        assert_eq!(
            owners(&fns, "self.keys_undo("),
            vec!["handle_keys".to_string()]
        );

        // handle_keys は「ツリーがフォーカス」「どのウィジェットもキーボード
        // フォーカスを持たない」を確かめてからでないと keys_* を呼ばない。
        // = エディタや入力欄に居るあいだ ⌘Z は本文の取り消しのまま。
        let hk = body(&fns, "handle_keys");
        let g1 = hk
            .find("if !self.focused || self.edit.is_some()")
            .expect("ツリーフォーカスのガード");
        let g2 = hk
            .find("m.focused().is_some()")
            .expect("ウィジェットフォーカスのガード");
        let call = hk.find("self.keys_undo(").expect("取り消しの呼び出し");
        assert!(
            g1 < call && g2 < call,
            "フォーカスを確かめる前にファイル操作の取り消しを撃っている"
        );
        assert!(
            hk[g1..call].contains("return"),
            "ガードは return で抜けること"
        );
        assert!(
            hk[g2..call].contains("return"),
            "ガードは return で抜けること"
        );

        // app 側の入口も 1 本だけ
        let app = app_fns();
        assert_eq!(
            owners(&app, "self.undo_file_op("),
            vec!["apply_tree_actions".to_string()],
            "ファイル操作の取り消しはツリーの要求からしか呼ばない"
        );
    }

    // ── ファイル所有リースの門 ──────────────────────────────

    /// 「別の担当が持っている」門 (シングルトンを経由せずに試す)。
    fn 拒む門(p: &Path) -> Option<String> {
        Some(format!(
            "{} は別の担当 (sess-other) が編集中です",
            p.display()
        ))
    }

    /// 「誰も持っていない」門 (= ガードが無効なときと同じ答え)。
    fn 通す門(_p: &Path) -> Option<String> {
        None
    }

    #[test]
    fn 他人が持つファイルの上へはコピーしない() {
        let dir = unique_temp_dir("zaivern-tree-test", "lease-copy");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        std::fs::write(src.join("a.txt"), "新しい中身").expect("write");
        let dst = dir.join("dst");
        std::fs::create_dir_all(&dst).expect("mkdir");
        std::fs::write(dst.join("a.txt"), "他人の作業中の中身").expect("write");

        let e = copy_recursively_inner(&src, &dst, 0, &拒む門).expect_err("門で止まらない");
        assert_eq!(
            e.kind(),
            std::io::ErrorKind::PermissionDenied,
            "他のエラーと区別できない"
        );
        assert!(e.to_string().contains("別の担当"), "理由が返らない: {e}");
        assert_eq!(
            std::fs::read_to_string(dst.join("a.txt")).expect("read"),
            "他人の作業中の中身",
            "拒否されたのに上書きされた"
        );

        // 門が通れば従来どおりコピーできる
        copy_recursively_inner(&src, &dst, 0, &通す門).expect("通らない");
        assert_eq!(
            std::fs::read_to_string(dst.join("a.txt")).expect("read"),
            "新しい中身"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 他人が持つファイルはリネームの取り消しで動かさない() {
        let dir = unique_temp_dir("zaivern-tree-test", "lease-undo");
        let from = dir.join("b.txt");
        let to = dir.join("a.txt");
        std::fs::write(&from, "中身").expect("write");

        let e = move_back_gated(&from, &to, &拒む門).expect_err("門で止まらない");
        assert!(e.contains("別の担当"), "理由が返らない: {e}");
        assert!(from.exists(), "拒否されたのに動かされた");
        assert!(!to.exists(), "拒否されたのに戻り先ができている");

        // 戻り先だけを他人が持っている場合も止める (潰さない)
        let only_dest = |p: &Path| {
            if p == to.as_path() {
                拒む門(p)
            } else {
                None
            }
        };
        let e = move_back_gated(&from, &to, &only_dest).expect_err("戻り先を見ていない");
        assert!(e.contains("別の担当"), "{e}");
        assert!(from.exists() && !to.exists());

        // 門が通れば従来どおり戻せる
        move_back_gated(&from, &to, &通す門).expect("通らない");
        assert!(to.exists() && !from.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 他人が持つファイルはゴミ箱へ送らない() {
        let dir = unique_temp_dir("zaivern-tree-test", "lease-trash");
        let f = dir.join("a.txt");
        std::fs::write(&f, "中身").expect("write");

        // 実ゴミ箱には触れない: 門で止まるので OS の実装まで進まない。
        let e = trash::send_gated(&f, &拒む門).expect_err("門で止まらない");
        assert!(e.contains("別の担当"), "理由が返らない: {e}");
        assert!(f.exists(), "拒否されたのに消えた");
        assert_eq!(std::fs::read_to_string(&f).expect("read"), "中身");

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
