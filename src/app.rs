use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, FontId, RichText};

use crate::agents::{AgentManager, SessionEvent};
use crate::config::{self, Config};
use crate::editor::{disk_mtime, hash_str, Buffer, Editor, ExternalEvent};
use crate::editor_ops;
use crate::file_tree::{self, FileTree, TreeActions};
use crate::fuzzy;
use crate::git;
use crate::git_panel;
use crate::highlight::Highlighter;
use crate::html;
use crate::keybinds::{parse_shortcut, BindAction, Keybinds};
use crate::lsp;
use crate::markdown;
use crate::notify;
use crate::palette::{Action, Cmd, Item, Palette};
use crate::pet;
use crate::pet_bubble;
use crate::remote;
use crate::session;
use crate::snippets::{self, Snippet};
use crate::sound::{self, SoundKind};
use crate::terminal;
use crate::theme::{self, Theme};
use crate::plugins;
use crate::theme_json;
use crate::voice;

#[derive(PartialEq, Clone, Copy)]
enum SidebarTab {
    Files,
    Agents,
    Plugins,
    Git,
}

impl SidebarTab {
    /// セッション保存用のキー文字列。
    fn as_key(self) -> &'static str {
        match self {
            SidebarTab::Files => "files",
            SidebarTab::Agents => "agents",
            SidebarTab::Plugins => "plugins",
            SidebarTab::Git => "git",
        }
    }

    /// セッションのキー文字列から復元する。未知/空なら既定の Files。
    fn from_key(s: &str) -> Self {
        match s {
            "agents" => SidebarTab::Agents,
            "plugins" => SidebarTab::Plugins,
            "git" => SidebarTab::Git,
            _ => SidebarTab::Files,
        }
    }
}

/// 音声入力 (プッシュトゥトーク) の実行状態。
///
/// 認識結果は対象セッションの入力欄へ「挿入するだけ」で Enter は送らない。
/// 送信するかどうかは必ずユーザーが自分で決める (誤送信防止)。
///
/// 認識中のテキストは確定を待たずに入力欄へ流し込む。話している途中の文字は
/// 変換のたびに書き換わるので、直前に書いた分を `live` に覚えておき、
/// 食い違うところだけ Backspace で消してから続きを送る。
#[derive(Default)]
struct VoiceState {
    /// 起動中の認識プロセス (None = 停止中)。⏹ を押すまで動き続ける
    session: Option<voice::Session>,
    /// マイクが開いたか (認識準備完了)
    ready: bool,
    /// 認識テキストの届け先
    target: voice::Target,
    /// 認識途中のテキスト (HUD 表示用)
    partial: String,
    /// 停止要求を出した時刻 (確定待ちのタイムアウト用)
    stopping_at: Option<Instant>,
    /// 直前に文字を送った先。宛先が変わったら区切りの空白を入れない
    last_sent_to: Option<u64>,
    /// 直前に送った文字列の末尾の 1 文字 (区切り空白を入れるか決めるのに使う)
    last_char: Option<char>,
    /// いま入力欄に書き込んである「まだ確定していない」文字列。
    /// 区切りの空白を付けたならそれも含む (差分計算をこの 1 本で完結させるため)。
    live: String,
    /// `live` の先頭に区切りの空白を付けたか
    live_space: bool,
}

/// 入力欄へ 1 回ぶん反映するための編集。
struct VoiceEdit {
    /// Backspace (0x7f) で消す文字数
    del: usize,
    /// 消したあとに書き足す文字列
    add: String,
    /// 反映後、入力欄に書いてあるはずの文字列 (区切りの空白を含む)
    want: String,
    /// `want` の先頭に区切りの空白を付けたか
    space: bool,
}

impl VoiceEdit {
    /// 送るものが無い (同じ途中経過がもう一度届いた) か。
    fn is_noop(&self) -> bool {
        self.del == 0 && self.add.is_empty()
    }

    /// 端末へ送るバイト列。`submit` なら最後に Enter まで付ける。
    fn bytes(&self, submit: bool) -> Vec<u8> {
        let mut out: Vec<u8> = vec![0x7f; self.del]; // 0x7f = DEL、端末の Backspace
        out.extend_from_slice(self.add.as_bytes());
        if submit {
            out.push(b'\r');
        }
        out
    }
}

impl VoiceState {
    /// 入力欄が空になった (送信した / ユーザーが手で消した) ときに呼ぶ。
    /// 書き込み済みの追跡を捨てるので、次の認識テキストは先頭から書き出される。
    fn reset_live(&mut self) {
        self.live.clear();
        self.live_space = false;
        self.last_char = None;
    }

    /// 認識テキスト `body` を届け先 `dest` の入力欄へ反映するための編集を組み立てる。
    /// ここでは状態を変えない — 実際に書き込めたら `commit` を呼ぶこと。
    fn plan(&self, body: &str, dest: u64) -> VoiceEdit {
        // 区切りの空白を入れるかは、その区切りの書き出し時に一度だけ決めて据え置く
        // (話している途中で変換が変わっても、空白が付いたり消えたりしないように)。
        let space = if self.live.is_empty() {
            self.last_sent_to == Some(dest) && needs_space(self.last_char, body.chars().next())
        } else {
            self.live_space
        };
        let want = if space {
            format!(" {body}")
        } else {
            body.to_string()
        };
        let (del, add) = diff_edit(&self.live, &want);
        VoiceEdit {
            del,
            add,
            want,
            space,
        }
    }

    /// 書き込めた編集を状態へ反映する。
    ///
    /// 確定した分 (`is_final`) はもう書き換えないので追跡をやめる。これで次の
    /// ひとことは前の文を消さずにその後ろへ書き足される — 2 回目以降の発話が
    /// 同じ入力欄に溜まっていくのはここが効いている。
    fn commit(&mut self, edit: VoiceEdit, is_final: bool, submit: bool, dest: u64) {
        if submit {
            // Enter まで送ったので入力欄は空。次はまた先頭から書き出す
            self.reset_live();
            self.last_sent_to = None;
            return;
        }
        self.last_sent_to = Some(dest);
        if is_final {
            self.last_char = edit.want.chars().last();
            self.live.clear();
            self.live_space = false;
        } else {
            self.live = edit.want;
            self.live_space = edit.space;
        }
    }
}

/// kind: 0 = ok(緑), 1 = warn(黄), 2 = err(赤)
struct Toast {
    msg: String,
    kind: u8,
    at: Instant,
}

struct FindState {
    open: bool,
    query: String,
    focus: bool,
    last: Option<usize>,
}

/// キーバインド駆動のエディタ編集操作
enum EditOp {
    ToggleComment,
    Duplicate,
    Move(bool),
}

pub struct ZaivernApp {
    cfg: Config,
    theme: Theme,
    workspace: PathBuf,
    tree: FileTree,
    editor: Editor,
    agents: AgentManager,
    palette: Palette,
    highlighter: Highlighter,
    cockpit: bool,
    /// Markdown/HTML ファイルをレンダリング表示するモード (Cockpit 以外で有効)
    md_preview: bool,
    /// プレビューが参照するローカル画像のテクスチャキャッシュ
    md_images: markdown::ImageCache,
    /// プレビュー用の変換結果キャッシュ (バッファ id, テキストハッシュ, 変換後 Markdown)
    md_pre_cache: Option<(u64, u64, String)>,
    sidebar_open: bool,
    sidebar_tab: SidebarTab,
    file_index: Vec<String>,
    index_at: Option<Instant>,
    /// カスタムテーマ (~/.zaivern/themes + プラグイン同梱): (表示名, JSONフルパス)
    custom_themes: Vec<(String, String)>,
    find: FindState,
    toasts: Vec<Toast>,
    pending_close: Option<usize>,
    /// ファイルツリーからの削除確認待ち(対象パス)
    pending_delete: Option<PathBuf>,
    pending_select: Option<(usize, usize)>,
    pending_scroll: Option<f32>,
    last_row_h: f32,
    /// エディタ可視領域の高さ(前フレーム値)。PageUp/Down・検索ジャンプで使用
    last_view_h: f32,
    /// エディタの垂直スクロール量(前フレーム値)
    last_scroll_y: f32,
    /// スマホリモートサーバ (起動失敗時は None + remote_err)
    remote: Option<remote::RemoteServer>,
    remote_err: Option<String>,
    remote_open: bool,
    qr_tex: Option<egui::TextureHandle>,
    broadcast_input: String,
    git: (Option<String>, Option<Instant>),
    gitinfo: git::Git,
    git_panel: git_panel::GitPanel,
    /// 外部変更チェックの直近実行時刻(約1秒スロットリング)
    ext_check_at: Option<Instant>,
    keys: Keybinds,
    /// ペットの固定位置(None=右下うろうろ)
    pet_pos: Option<egui::Pos2>,
    /// ユーザー指定ペット画像のテクスチャ
    pet_tex: Option<egui::TextureHandle>,
    /// ペットのアニメ状態(フレームを跨いで保持)
    pet_rt: pet::PetRuntime,
    /// 効果音プレイヤー(種類ごとの連続再生クールダウン付き)
    sound: sound::SoundPlayer,
    /// この時刻までペットが喜ぶ(直近のエージェント正常終了)
    pet_happy_until: Option<Instant>,
    /// この時刻までペットが落ち込む(直近のエージェント異常終了)
    pet_error_until: Option<Instant>,
    /// × で閉じた承認バブルのセッション id(承認待ち解除で自動掃除。
    /// index はセッション削除でずれるため安定 id をキーにする)
    pet_bubble_dismissed: HashSet<u64>,
    /// 承認/拒否に応答した時刻(セッション id 毎)。キー入力がプロンプトを
    /// 消すまでの3秒間はバブルの再表示を抑止する(再検出ループ対策)
    pet_bubble_answered: HashMap<u64, Instant>,
    /// 承認待ちトースト+効果音の直近通知時刻(セッションタイトル毎)。
    /// 同じプロンプトの再検出による多重通知を10秒に1回へ抑える
    pet_attention_notified: HashMap<String, Instant>,
    /// インストール済みプラグイン(~/.zaivern/plugins)
    plugins: Vec<plugins::Plugin>,
    /// プラグインコマンドのキーバインド: (shortcut, plugins index, commands index)
    plugin_keys: Vec<(egui::KeyboardShortcut, usize, usize)>,
    /// プラグインコマンド実行結果の受け渡し(ワーカースレッド → UI)
    plugin_tx: mpsc::Sender<plugins::RunOutcome>,
    plugin_rx: mpsc::Receiver<plugins::RunOutcome>,
    /// 「➕ 新規プラグイン」ダイアログの入力名(None = 閉)
    new_plugin_name: Option<String>,
    /// 言語ID → スニペット一覧(拡張の snippet ファイル由来)
    snippets_by_lang: HashMap<String, Vec<Snippet>>,
    /// 言語ID → 起動済み LSP クライアント
    lsp: HashMap<String, lsp::LspClient>,
    /// did_open 済みのパス(重複送信の防止)
    lsp_opened: HashSet<PathBuf>,
    /// 診断の変更をデバウンスするための保留(パス→(最新テキスト, 受信時刻, 言語ID))
    lsp_pending: HashMap<PathBuf, (String, Instant, String)>,
    /// which() の「見つからなかった」結果のキャッシュ(実行ファイル名→最後に確認した時刻)。
    /// 肯定結果は入れない(見つかればサーバーが起動して self.lsp に載り、二度と which されない)。
    lsp_which_missing: HashMap<String, Instant>,
    /// アクティブバッファの診断件数 (エラー, 警告) — ステータスバー用
    diag_counts: (usize, usize),
    /// 音声入力の実行状態
    voice: VoiceState,
}

impl ZaivernApp {
    pub fn new(cc: &eframe::CreationContext<'_>, workspace: PathBuf) -> Self {
        install_fonts(&cc.egui_ctx);
        let cfg = config::load(&workspace, true);
        let theme = resolve_theme(&cfg.theme);
        theme::apply(&cc.egui_ctx, &theme);

        let ws_name = workspace
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| workspace.to_string_lossy().to_string());
        cc.egui_ctx
            .send_viewport_cmd(egui::ViewportCommand::Title(format!(
                "Zaivern Code — {ws_name}"
            )));

        let (plugin_tx, plugin_rx) = mpsc::channel();
        let mut app = Self {
            tree: FileTree::new(workspace.clone(), cfg.show_hidden_files),
            gitinfo: git::Git::new(workspace.clone()),
            git_panel: git_panel::GitPanel::new(workspace.clone()),
            ext_check_at: None,
            keys: Keybinds::from_overrides(&cfg.keybindings),
            theme,
            workspace,
            editor: Editor::new(),
            agents: AgentManager::new(),
            palette: Palette::new(),
            highlighter: Highlighter::new(),
            cockpit: false,
            md_preview: false,
            md_images: markdown::ImageCache::default(),
            md_pre_cache: None,
            sidebar_open: true,
            sidebar_tab: SidebarTab::Files,
            file_index: Vec::new(),
            index_at: None,
            custom_themes: Vec::new(),
            find: FindState {
                open: false,
                query: String::new(),
                focus: false,
                last: None,
            },
            toasts: Vec::new(),
            pending_close: None,
            pending_delete: None,
            pending_select: None,
            pending_scroll: None,
            last_row_h: 18.0,
            last_view_h: 620.0,
            last_scroll_y: 0.0,
            remote: None,
            remote_err: None,
            remote_open: false,
            voice: VoiceState::default(),
            qr_tex: None,
            broadcast_input: String::new(),
            git: (None, None),
            pet_pos: match (cfg.pet_x, cfg.pet_y) {
                (Some(x), Some(y)) => Some(egui::pos2(x, y)),
                _ => None,
            },
            pet_tex: None,
            pet_rt: pet::PetRuntime::default(),
            sound: sound::SoundPlayer::default(),
            pet_happy_until: None,
            pet_error_until: None,
            pet_bubble_dismissed: HashSet::new(),
            pet_bubble_answered: HashMap::new(),
            pet_attention_notified: HashMap::new(),
            plugins: Vec::new(),
            plugin_keys: Vec::new(),
            plugin_tx,
            plugin_rx,
            new_plugin_name: None,
            snippets_by_lang: HashMap::new(),
            lsp: HashMap::new(),
            lsp_opened: HashSet::new(),
            lsp_pending: HashMap::new(),
            lsp_which_missing: HashMap::new(),
            diag_counts: (0, 0),
            cfg,
        };
        // ユーザー指定のペット画像をロード
        if let Some(path) = app.cfg.pet_image.clone() {
            app.pet_tex = load_pet_texture(&cc.egui_ctx, Path::new(&path));
        }
        app.rebuild_plugins();
        // スマホリモートサーバを起動 (LAN 内からブラウザで操作可能に)
        match remote::RemoteServer::start(cc.egui_ctx.clone()) {
            Ok(s) => app.remote = Some(s),
            Err(e) => app.remote_err = Some(e),
        }
        app.rebuild_index();
        app.restore_session();
        app
    }

    // ─── プラグイン (コマンド / スニペット / テーマ) ─────────────────

    /// インストール済みプラグインを再スキャンし、スニペット辞書・テーマ一覧・
    /// コマンドキーバインドを作り直す。
    fn rebuild_plugins(&mut self) {
        self.plugins = plugins::scan_installed();

        // スニペットを言語IDごとに集約
        let mut by_lang: HashMap<String, Vec<Snippet>> = HashMap::new();
        for p in &self.plugins {
            for (lang, path) in &p.snippet_files {
                let snips = snippets::parse_file(path, lang);
                if !snips.is_empty() {
                    by_lang.entry(lang.clone()).or_default().extend(snips);
                }
            }
        }
        self.snippets_by_lang = by_lang;

        // テーマ一覧 = ~/.zaivern/themes + プラグイン同梱テーマ(パスで重複排除)
        let mut themes = theme_json::discover_user_themes();
        let mut seen: HashSet<String> = themes.iter().map(|(_, p)| p.clone()).collect();
        for p in &self.plugins {
            for (label, path) in &p.themes {
                let ps = path.to_string_lossy().to_string();
                if seen.insert(ps.clone()) {
                    themes.push((label.clone(), ps));
                }
            }
        }
        themes.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        self.custom_themes = themes;

        // コマンドの keybind をパースしてキャッシュ (不正な文字列は無視)
        self.plugin_keys.clear();
        for (pi, p) in self.plugins.iter().enumerate() {
            for (ci, c) in p.commands.iter().enumerate() {
                if let Some(sc) = c.keybind.as_deref().and_then(parse_shortcut) {
                    self.plugin_keys.push((sc, pi, ci));
                }
            }
        }
    }

    /// プラグインコマンドを実行する。stdin へ渡す入力(選択範囲/ファイル)を集めて
    /// ワーカースレッドへ投げ、結果は plugin_rx 経由で process_plugin_results が適用する。
    fn run_plugin_command(&mut self, pi: usize, ci: usize, ctx: &egui::Context) {
        let (Some(plugin), Some(command)) = (
            self.plugins.get(pi),
            self.plugins.get(pi).and_then(|p| p.commands.get(ci)),
        ) else {
            return;
        };
        let plugin_name = plugin.name.clone();
        let plugin_dir = plugin.dir.clone();
        let command = command.clone();

        let active = self.editor.active.map(|i| &self.editor.buffers[i]);
        let lang_id = active
            .map(|b| snippets::lang_id_for(&b.lang).to_string())
            .unwrap_or_default();
        if !command.lang_matches(&lang_id) {
            self.toast(
                format!("「{}」は {:?} 用のコマンドです", command.title, command.langs),
                false,
            );
            return;
        }

        // 入力の収集 (selection は TextEdit の選択 char 範囲)
        let (stdin_text, buffer_id, replace_range) = match command.input {
            plugins::CmdInput::None => (String::new(), active.map(|b| b.id), None),
            plugins::CmdInput::File => match active {
                Some(b) => (b.text.clone(), Some(b.id), None),
                None => {
                    self.toast("実行にはファイルを開いてください", false);
                    return;
                }
            },
            plugins::CmdInput::Selection => {
                let Some(b) = active else {
                    self.toast("実行にはファイルを開いてください", false);
                    return;
                };
                let ed_id = egui::Id::new(("zaivern-buffer", b.id));
                let range = egui::TextEdit::load_state(ctx, ed_id)
                    .and_then(|st| st.cursor.char_range())
                    .map(|r| (r.primary.index, r.secondary.index))
                    .unwrap_or((0, 0));
                let (s, e) = (range.0.min(range.1), range.0.max(range.1));
                if s == e {
                    self.toast("選択範囲がありません", false);
                    return;
                }
                let sel: String = b.text.chars().skip(s).take(e - s).collect();
                (sel, Some(b.id), Some((s, e)))
            }
        };

        let file = active
            .and_then(|b| b.path.as_ref())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let envs = vec![
            ("ZV_FILE".to_string(), file),
            ("ZV_LANG".to_string(), lang_id),
            (
                "ZV_WORKSPACE".to_string(),
                self.workspace.to_string_lossy().to_string(),
            ),
            (
                "ZV_PLUGIN_DIR".to_string(),
                plugin_dir.to_string_lossy().to_string(),
            ),
        ];
        let title = command.title.clone();
        plugins::run_async(
            plugins::RunRequest {
                plugin: plugin_name,
                command,
                stdin_text,
                envs,
                workdir: self.workspace.clone(),
                buffer_id,
                replace_range,
                resave: false,
            },
            self.plugin_tx.clone(),
            ctx.clone(),
        );
        self.toast(format!("🔌 {title} を実行中…"), true);
    }

    /// ワーカースレッドから届いたプラグインコマンドの結果をエディタへ適用する。
    fn process_plugin_results(&mut self, ctx: &egui::Context) {
        while let Ok(r) = self.plugin_rx.try_recv() {
            if !r.ok {
                let msg = r.stderr.trim();
                let msg = if msg.is_empty() { "失敗しました (出力なし)" } else { msg };
                self.toast(
                    format!("🔌 {} ({}): {}", r.title, r.plugin, notify::truncate_chars(msg, 200)),
                    false,
                );
                continue;
            }
            match r.output {
                plugins::CmdOutput::Silent => {}
                plugins::CmdOutput::Notify => {
                    let msg = if r.stdout.trim().is_empty() {
                        "完了しました".to_string()
                    } else {
                        notify::truncate_chars(r.stdout.trim(), 200)
                    };
                    self.toast(format!("🔌 {}: {msg}", r.title), true);
                    notify::notify(&format!("Zaivern — {}", r.title), &msg);
                }
                plugins::CmdOutput::NewTab => {
                    self.editor.new_untitled();
                    if let Some(i) = self.editor.active {
                        let b = &mut self.editor.buffers[i];
                        b.title = r.title.clone();
                        b.text = r.stdout.clone();
                        b.cache = None;
                        b.gutter = None;
                    }
                    self.toast(format!("🔌 {} → 新規タブ", r.title), true);
                }
                plugins::CmdOutput::Insert => {
                    let Some(i) = self
                        .editor
                        .buffers
                        .iter()
                        .position(|b| Some(b.id) == r.buffer_id)
                    else {
                        self.toast(format!("🔌 {}: 反映先のタブが閉じられています", r.title), false);
                        continue;
                    };
                    let ed_id = egui::Id::new(("zaivern-buffer", self.editor.buffers[i].id));
                    let cur = egui::TextEdit::load_state(ctx, ed_id)
                        .and_then(|st| st.cursor.char_range())
                        .map(|c| c.primary.index)
                        .unwrap_or_else(|| self.editor.buffers[i].text.chars().count());
                    let b = &mut self.editor.buffers[i];
                    let cur = cur.min(b.text.chars().count());
                    let byte = editor_ops::char_to_byte(&b.text,cur);
                    b.text.insert_str(byte, &r.stdout);
                    b.cache = None;
                    b.gutter = None;
                    let end = cur + r.stdout.chars().count();
                    self.pending_select = Some((end, end));
                    self.toast(format!("🔌 {} を挿入しました", r.title), true);
                }
                plugins::CmdOutput::Replace => {
                    let Some(i) = self
                        .editor
                        .buffers
                        .iter()
                        .position(|b| Some(b.id) == r.buffer_id)
                    else {
                        self.toast(format!("🔌 {}: 反映先のタブが閉じられています", r.title), false);
                        continue;
                    };
                    let b = &mut self.editor.buffers[i];
                    match r.replace_range {
                        // 選択範囲の置換: 実行中に編集されていたら黙って上書きしない
                        Some((s, e)) => {
                            let cur_sel: String = b.text.chars().skip(s).take(e - s).collect();
                            if cur_sel != r.original {
                                self.toast(
                                    format!("🔌 {}: 実行中に編集されたため適用を中止しました", r.title),
                                    false,
                                );
                                continue;
                            }
                            let start = editor_ops::char_to_byte(&b.text,s);
                            let end = editor_ops::char_to_byte(&b.text,e);
                            b.text.replace_range(start..end, &r.stdout);
                            b.cache = None;
                            b.gutter = None;
                            let np = s + r.stdout.chars().count();
                            self.pending_select = Some((np, np));
                            self.toast(format!("🔌 {} を適用しました", r.title), true);
                        }
                        // ファイル全体の置換 (整形など)
                        None => {
                            if b.text != r.original {
                                self.toast(
                                    format!("🔌 {}: 実行中に編集されたため適用を中止しました", r.title),
                                    false,
                                );
                                continue;
                            }
                            if b.text == r.stdout {
                                if r.resave {
                                    continue; // 保存時フックで変更なし → 静かに終了
                                }
                                self.toast(format!("🔌 {}: 変更はありません", r.title), true);
                                continue;
                            }
                            b.text = r.stdout.clone();
                            b.cache = None;
                            b.gutter = None;
                            // 保存時フック由来なら整形結果をそのままファイルへ書き戻す
                            if r.resave {
                                if let Some(path) = b.path.clone() {
                                    match std::fs::write(&path, &b.text) {
                                        Ok(()) => {
                                            b.saved_hash = hash_str(&b.text);
                                            b.disk_mtime = disk_mtime(&path);
                                            b.conflict_notified = None;
                                            self.toast(
                                                format!("🔌 {} → 整形して保存しました", r.title),
                                                true,
                                            );
                                        }
                                        Err(e) => self.toast(
                                            format!("🔌 {}: 再保存に失敗: {e}", r.title),
                                            false,
                                        ),
                                    }
                                }
                            } else {
                                self.toast(format!("🔌 {} を適用しました", r.title), true);
                            }
                        }
                    }
                }
            }
        }
    }

    /// 「➕ 新規プラグイン」の名前入力ダイアログ。
    /// 作成後は plugin.toml をエディタで開き、すぐ編集を始められるようにする。
    fn new_plugin_ui(&mut self, ctx: &egui::Context) {
        let Some(mut name) = self.new_plugin_name.clone() else {
            return;
        };
        let theme = self.theme.clone();
        let mut open = true;
        let mut create = false;
        let mut cancel = false;
        egui::Window::new("➕ 新規プラグイン")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, -80.0))
            .show(ctx, |ui| {
                ui.label("プラグイン名 (小文字英数と - _ のみ):");
                let re = ui.text_edit_singleline(&mut name);
                if re.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    create = true;
                }
                let ok = plugins::valid_name(&name.trim().to_lowercase());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(ok, egui::Button::new("作成")).clicked() {
                        create = true;
                    }
                    if ui.button("キャンセル").clicked() {
                        cancel = true;
                    }
                    if !name.trim().is_empty() && !ok {
                        ui.label(RichText::new("名前が不正です").color(theme.warn));
                    }
                });
                ui.label(
                    RichText::new(
                        "~/.zaivern/plugins/<名前>/ にコマンド・テーマ・スニペットの\nテンプレート一式を生成し、plugin.toml を開きます",
                    )
                    .size(10.5)
                    .color(theme.text_dim),
                );
            });
        if create && plugins::valid_name(&name.trim().to_lowercase()) {
            match plugins::create_template(name.trim()) {
                Ok(dir) => {
                    self.rebuild_plugins();
                    self.open_path(&dir.join("plugin.toml"));
                    self.toast(format!("➕ 作成しました: {}", dir.display()), true);
                    self.new_plugin_name = None;
                }
                Err(e) => {
                    self.toast(format!("作成失敗: {e}"), false);
                    self.new_plugin_name = Some(name);
                }
            }
        } else if cancel || !open {
            self.new_plugin_name = None;
        } else {
            self.new_plugin_name = Some(name);
        }
    }

    /// 保存直後に on_save フック (整形など) を持つプラグインコマンドを起動する。
    fn run_on_save_hooks(&mut self, buf_index: usize, ctx: &egui::Context) {
        let b = &self.editor.buffers[buf_index];
        let lang_id = snippets::lang_id_for(&b.lang).to_string();
        let Some(path) = b.path.clone() else {
            return;
        };
        let (text, buffer_id) = (b.text.clone(), b.id);
        let mut launched: Vec<(String, plugins::PluginCommand, PathBuf)> = Vec::new();
        for p in &self.plugins {
            for c in &p.commands {
                if c.on_save && c.lang_matches(&lang_id) {
                    launched.push((p.name.clone(), c.clone(), p.dir.clone()));
                }
            }
        }
        for (plugin_name, command, plugin_dir) in launched {
            let envs = vec![
                ("ZV_FILE".to_string(), path.to_string_lossy().to_string()),
                ("ZV_LANG".to_string(), lang_id.clone()),
                (
                    "ZV_WORKSPACE".to_string(),
                    self.workspace.to_string_lossy().to_string(),
                ),
                (
                    "ZV_PLUGIN_DIR".to_string(),
                    plugin_dir.to_string_lossy().to_string(),
                ),
            ];
            plugins::run_async(
                plugins::RunRequest {
                    plugin: plugin_name,
                    command,
                    stdin_text: text.clone(),
                    envs,
                    workdir: self.workspace.clone(),
                    buffer_id: Some(buffer_id),
                    replace_range: None,
                    resave: true,
                },
                self.plugin_tx.clone(),
                ctx.clone(),
            );
        }
    }

    // ─── LSP (言語サーバー) ─────────────────────────────────────────

    /// バッファを開いた/表示したときに、その言語のサーバーを必要なら起動し did_open する。
    ///
    /// `buf_idx` は did_open に送る本文を持つバッファの添字。本文は初回の did_open で
    /// しか使わないので、呼び出し側で毎フレーム clone せず、必要になった所で借りる。
    fn ensure_lsp(&mut self, ctx: &egui::Context, path: &Path, lang: &str, buf_idx: usize) {
        let lang_id = snippets::lang_id_for(lang).to_string();
        let Some(server_cmd) = lsp_server_for(&lang_id) else {
            return;
        };
        if !self.lsp.contains_key(&lang_id) {
            let bin = server_cmd.split_whitespace().next().unwrap_or("");
            // which() は $SHELL -lc のサブプロセスを起動する。ここは毎フレーム通るので、
            // 「見つからなかった」結果を短時間だけ覚えて spawn 連発を防ぐ。
            let now = Instant::now();
            if which_result_is_fresh(self.lsp_which_missing.get(bin).copied(), now, WHICH_MISS_TTL)
            {
                return; // 直近で確認済み。未インストールのまま
            }
            if !which(bin) {
                self.lsp_which_missing.insert(bin.to_string(), now);
                return; // サーバー未インストールなら静かに無効
            }
            self.lsp_which_missing.remove(bin);
            match lsp::LspClient::spawn(server_cmd, &self.workspace, ctx.clone()) {
                Ok(client) => {
                    self.lsp.insert(lang_id.clone(), client);
                }
                Err(_) => return,
            }
        }
        if !self.lsp_opened.contains(path) {
            if let Some(client) = self.lsp.get(&lang_id) {
                // 本文はこの一回だけ必要。self.lsp / self.editor はどちらも不変借用なので両立する
                let text = self
                    .editor
                    .buffers
                    .get(buf_idx)
                    .map(|b| b.text.as_str())
                    .unwrap_or("");
                client.did_open(path, &lang_id, text);
            }
            // クライアントの有無に関わらず登録するのは元の insert と同じ挙動
            self.lsp_opened.insert(path.to_path_buf());
        }
    }

    /// デバウンスした did_change を実際に送る(update から毎フレーム呼ぶ)。
    fn flush_lsp_changes(&mut self) {
        if self.lsp_pending.is_empty() {
            return;
        }
        let ready: Vec<PathBuf> = self
            .lsp_pending
            .iter()
            .filter(|(_, (_, at, _))| at.elapsed().as_millis() >= 250)
            .map(|(p, _)| p.clone())
            .collect();
        for p in ready {
            if let Some((text, _, lang_id)) = self.lsp_pending.remove(&p) {
                if let Some(client) = self.lsp.get(&lang_id) {
                    client.did_change(&p, &text);
                }
            }
        }
    }

    /// アクティブバッファの診断: 行→最悪 severity のマップと (エラー数, 警告数)。
    fn active_diagnostics(&self) -> (HashMap<usize, u8>, usize, usize) {
        let mut by_line: HashMap<usize, u8> = HashMap::new();
        let (mut errs, mut warns) = (0usize, 0usize);
        let Some(i) = self.editor.active else {
            return (by_line, 0, 0);
        };
        let Some(path) = self.editor.buffers[i].path.as_ref() else {
            return (by_line, 0, 0);
        };
        let lang_id = snippets::lang_id_for(&self.editor.buffers[i].lang);
        let Some(client) = self.lsp.get(lang_id) else {
            return (by_line, 0, 0);
        };
        for d in client.diagnostics(path) {
            match d.severity {
                1 => errs += 1,
                2 => warns += 1,
                _ => {}
            }
            let e = by_line.entry(d.line).or_insert(4);
            if d.severity < *e {
                *e = d.severity;
            }
        }
        (by_line, errs, warns)
    }

    /// 現在のタブ構成などをワークスペース単位で保存する。
    fn persist_session(&self) {
        let data = session::SessionData {
            open_files: self
                .editor
                .buffers
                .iter()
                .filter_map(|b| b.path.as_ref().map(|p| p.to_string_lossy().to_string()))
                .collect(),
            active: self.editor.active,
            sidebar_open: self.sidebar_open,
            panel_open: self.agents.panel_open,
            sidebar_tab: self.sidebar_tab.as_key().to_string(),
        };
        session::save(&self.workspace, &data);
    }

    /// 保存済みセッション(開いていたタブ等)を復元する。
    fn restore_session(&mut self) {
        let Some(sess) = session::load(&self.workspace) else {
            return;
        };
        let base = self.editor.buffers.len();
        for f in &sess.open_files {
            let _ = self.editor.open(Path::new(f), &self.highlighter);
        }
        if let Some(a) = sess.active {
            let idx = base + a;
            if idx < self.editor.buffers.len() {
                self.editor.active = Some(idx);
            }
        }
        self.sidebar_open = sess.sidebar_open;
        self.sidebar_tab = SidebarTab::from_key(&sess.sidebar_tab);
        self.agents.panel_open = sess.panel_open;
    }

    /// アクティブバッファへ editor_ops の編集操作を適用する。
    fn editor_op(&mut self, ctx: &egui::Context, op: EditOp) {
        let Some(i) = self.editor.active else {
            return;
        };
        let ed_id = egui::Id::new(("zaivern-buffer", self.editor.buffers[i].id));
        let range = egui::TextEdit::load_state(ctx, ed_id)
            .and_then(|st| st.cursor.char_range())
            .map(|r| (r.primary.index, r.secondary.index))
            .unwrap_or((0, 0));
        let (start, end) = (range.0.min(range.1), range.0.max(range.1));

        let prefix = editor_ops::comment_prefix_for(&self.editor.buffers[i].lang);
        if matches!(op, EditOp::ToggleComment) && prefix.is_none() {
            let lang = self.editor.buffers[i].lang.clone();
            self.toast(format!("{lang} の行コメント記法が不明です"), false);
            return;
        }

        let buf = &mut self.editor.buffers[i];
        let (new_text, new_sel) = match op {
            EditOp::ToggleComment => {
                let (t, s, e) =
                    editor_ops::toggle_comment(&buf.text, start, end, prefix.unwrap());
                (t, (s, e))
            }
            EditOp::Duplicate => {
                let (t, c) = editor_ops::duplicate_line(&buf.text, end);
                (t, (c, c))
            }
            EditOp::Move(up) => {
                let (t, c) = editor_ops::move_line(&buf.text, end, up);
                (t, (c, c))
            }
        };
        if new_text != buf.text {
            buf.text = new_text;
            buf.cache = None;
            buf.gutter = None;
        }
        self.pending_select = Some(new_sel);
    }

    fn toast(&mut self, msg: impl Into<String>, ok: bool) {
        self.push_toast(msg, if ok { 0 } else { 2 });
    }

    fn toast_warn(&mut self, msg: impl Into<String>) {
        self.push_toast(msg, 1);
    }

    fn push_toast(&mut self, msg: impl Into<String>, kind: u8) {
        self.toasts.push(Toast {
            msg: msg.into(),
            kind,
            at: Instant::now(),
        });
        if self.toasts.len() > 5 {
            self.toasts.remove(0);
        }
    }

    fn rebuild_index(&mut self) {
        const SKIP: [&str; 10] = [
            "target",
            "node_modules",
            ".git",
            ".venv",
            "venv",
            "__pycache__",
            "dist",
            "build",
            ".next",
            ".cache",
        ];
        let mut out: Vec<String> = Vec::new();
        let mut stack = vec![(self.workspace.clone(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            if depth > 12 || out.len() >= 8000 {
                continue;
            }
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir {
                    if !SKIP.contains(&name.as_str()) && !name.starts_with('.') {
                        stack.push((e.path(), depth + 1));
                    }
                } else if name != ".DS_Store" {
                    let rel = e
                        .path()
                        .strip_prefix(&self.workspace)
                        .unwrap_or(&e.path())
                        .to_string_lossy()
                        .to_string();
                    out.push(rel);
                }
            }
        }
        out.sort();
        self.file_index = out;
        self.index_at = Some(Instant::now());
    }

    fn git_branch(&mut self) -> Option<String> {
        let fresh = self
            .git
            .1
            .map(|t| t.elapsed().as_secs() < 3)
            .unwrap_or(false);
        if !fresh {
            self.git.1 = Some(Instant::now());
            self.git.0 = std::fs::read_to_string(git_head_path(&self.workspace))
                .ok()
                .map(|s| {
                    let s = s.trim();
                    s.strip_prefix("ref: refs/heads/")
                        .map(str::to_string)
                        .unwrap_or_else(|| s.chars().take(8).collect())
                });
        }
        self.git.0.clone()
    }

    fn open_path(&mut self, path: &Path) {
        match self.editor.open(path, &self.highlighter) {
            Ok(reloaded) => {
                // Cockpit 表示中はエディタが隠れており「開けていない」ように
                // 見えるため、ファイルを開いたらエディタ画面へ戻す
                self.cockpit = false;
                if reloaded {
                    if let Some(i) = self.editor.active {
                        let title = self.editor.buffers[i].title.clone();
                        self.toast(format!("↻ {title} を再読み込みしました(外部で変更)"), true);
                        self.queue_lsp_change(i);
                    }
                }
                self.persist_session()
            }
            Err(e) => self.toast(e, false),
        }
    }

    /// 開いているタブのファイルが外部(エージェント等)で書き換えられていないか
    /// 約1秒ごとに確認する。未保存の編集が無いバッファはディスクの内容へ自動で
    /// 読み直し、編集と競合したバッファは上書きせず一度だけ警告する。
    /// あわせてファイルツリーも外部でのファイル追加・削除を検知して自動更新する。
    fn check_external_changes(&mut self) {
        let fresh = self
            .ext_check_at
            .map(|t| t.elapsed().as_millis() < 1000)
            .unwrap_or(false);
        if fresh {
            return;
        }
        self.ext_check_at = Some(Instant::now());
        self.tree.refresh_if_changed();
        for ev in self.editor.check_external() {
            match ev {
                ExternalEvent::Reloaded { index, title } => {
                    self.toast(format!("↻ {title} を再読み込みしました(外部で変更)"), true);
                    self.queue_lsp_change(index);
                }
                ExternalEvent::Conflict { title } => {
                    self.toast_warn(format!(
                        "⚠ {title} が外部で変更されました — 未保存の編集があるため読み直していません(⌘S で上書き)"
                    ));
                }
            }
        }
    }

    /// リロード後のテキストを LSP へ(デバウンス付きで)通知する
    fn queue_lsp_change(&mut self, i: usize) {
        let Some(b) = self.editor.buffers.get(i) else {
            return;
        };
        let Some(p) = b.path.clone() else {
            return;
        };
        let lang_id = snippets::lang_id_for(&b.lang).to_string();
        if self.lsp.contains_key(&lang_id) {
            self.lsp_pending
                .insert(p, (b.text.clone(), Instant::now(), lang_id));
        }
    }

    fn launch_preset(&mut self, i: usize, ctx: &egui::Context) {
        use crate::agents::{apply_approval, command_is_bypass, Approval};
        let Some(p) = self.cfg.agents.get(i).cloned() else {
            return;
        };
        let approval = Approval::from_mode(&self.cfg.approval_mode);
        // 実際に起動されるコマンドで bypass かどうかを判定する
        // (Agent優先モードではプリセットのフラグがそのまま効く)
        let is_bypass = command_is_bypass(&apply_approval(&p.command, approval));
        let head = p.command.split_whitespace().next().unwrap_or("");
        let is_agent_cli = matches!(head, "claude" | "codex" | "agy");
        let via = if approval == Approval::Agent {
            "（Agent欄の指定どおり）"
        } else {
            "（既定モード）"
        };
        match self.agents.launch(&p, &self.workspace, approval, ctx) {
            Ok(()) => {
                if is_agent_cli && is_bypass {
                    self.toast_warn(format!(
                        "⚡ {} を全自動モードで起動しました{via}",
                        p.name
                    ));
                } else if is_agent_cli {
                    self.toast(
                        format!("🛡 {} を承認モードで起動しました{via}", p.name),
                        true,
                    );
                } else {
                    self.toast(format!("{} {} を起動しました", p.icon, p.name), true);
                }
            }
            Err(e) => self.toast(e, false),
        }
    }

    fn send_to_agent(&mut self, text: String) {
        if let Some(s) = self.agents.active_session() {
            s.write_bytes(text.as_bytes());
            self.agents.panel_open = true;
            self.toast("アクティブなエージェントに送信しました", true);
        } else {
            self.toast("エージェントセッションがありません（🤖 Agent＋ から起動）", false);
        }
    }

    fn save_active(&mut self, as_new: bool) -> bool {
        let Some(i) = self.editor.active else {
            return false;
        };
        let (need_dialog, cur_path) = {
            let b = &self.editor.buffers[i];
            (as_new || b.path.is_none(), b.path.clone())
        };
        let path = if need_dialog {
            match rfd::FileDialog::new()
                .set_directory(&self.workspace)
                .save_file()
            {
                Some(p) => p,
                None => return false,
            }
        } else {
            cur_path.unwrap()
        };

        let text = self.editor.buffers[i].text.clone();
        match std::fs::write(&path, &text) {
            Ok(()) => {
                let lang = self.highlighter.lang_for(Some(&path), &text);
                let b = &mut self.editor.buffers[i];
                b.title = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "???".into());
                b.path = Some(path.clone());
                b.saved_hash = hash_str(&text);
                b.lang = lang;
                b.cache = None;
                b.disk_mtime = disk_mtime(&path);
                b.conflict_notified = None;
                self.tree.invalidate();
                self.toast(format!("💾 保存しました: {}", path.display()), true);
                true
            }
            Err(e) => {
                self.toast(format!("保存に失敗しました: {e}"), false);
                false
            }
        }
    }

    fn request_close(&mut self, i: usize) {
        if self
            .editor
            .buffers
            .get(i)
            .map(|b| b.dirty())
            .unwrap_or(false)
        {
            self.pending_close = Some(i);
        } else {
            self.editor.close(i);
            self.persist_session();
        }
    }

    fn find_next(&mut self) {
        let Some(i) = self.editor.active else {
            return;
        };
        if self.find.query.is_empty() {
            return;
        }
        let text = self.editor.buffers[i].text.clone();
        let hay_lower = text.to_lowercase();
        let needle_lower = self.find.query.to_lowercase();
        // Lowercasing can shift byte offsets for exotic chars; fall back to
        // case-sensitive search when lengths diverge.
        let (hay, needle) = if hay_lower.len() == text.len() {
            (hay_lower.as_str(), needle_lower.as_str())
        } else {
            (text.as_str(), self.find.query.as_str())
        };

        let start_char = self.find.last.map(|c| c + 1).unwrap_or(0);
        let start_byte = text
            .char_indices()
            .nth(start_char)
            .map(|(b, _)| b)
            .unwrap_or(text.len());

        let found = hay[start_byte.min(hay.len())..]
            .find(needle)
            .map(|p| p + start_byte)
            .or_else(|| hay.find(needle));

        let Some(byte_pos) = found else {
            self.toast("見つかりませんでした", false);
            self.find.last = None;
            return;
        };

        let char_pos = text[..byte_pos].chars().count();
        let n_chars = self.find.query.chars().count();
        self.find.last = Some(char_pos);
        self.pending_select = Some((char_pos, char_pos + n_chars));
        let line = text[..byte_pos].matches('\n').count();
        // VS Code 同様、ヒット行が画面の中央付近に来るようにスクロールする
        self.pending_scroll =
            Some((line as f32 * self.last_row_h - self.last_view_h * 0.4).max(0.0));
    }

    /// 指定フォルダをワークスペースとして開き直す (フォルダを開く / worktree を開く)。
    fn open_workspace(&mut self, dir: PathBuf, ctx: &egui::Context) {
        self.persist_session();
        self.workspace = dir.clone();
        self.tree.set_root(dir.clone());
        self.gitinfo.set_workspace(dir.clone());
        self.git_panel.set_workspace(dir.clone());
        self.rebuild_index();
        self.restore_session();
        self.git = (None, None);
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
            "Zaivern Code — {name}"
        )));
        self.toast(format!("📂 {} を開きました", dir.display()), true);
    }

    fn apply_cmd(&mut self, cmd: Cmd, ctx: &egui::Context) {
        match cmd {
            Cmd::Save => {
                if self.save_active(false) {
                    self.persist_session();
                    if let Some(i) = self.editor.active {
                        self.run_on_save_hooks(i, ctx);
                    }
                }
            }
            Cmd::SaveAs => {
                if self.save_active(true) {
                    self.persist_session();
                    if let Some(i) = self.editor.active {
                        self.run_on_save_hooks(i, ctx);
                    }
                }
            }
            Cmd::CloseTab => {
                if let Some(i) = self.editor.active {
                    self.request_close(i);
                }
            }
            Cmd::NewFile => self.editor.new_untitled(),
            Cmd::OpenFolder => {
                if let Some(dir) = rfd::FileDialog::new()
                    .set_directory(&self.workspace)
                    .pick_folder()
                {
                    self.open_workspace(dir, ctx);
                }
            }
            Cmd::ToggleTerminal => {
                if self.agents.sessions.is_empty() && !self.agents.panel_open {
                    // 開くものがなければシェルを起動する
                    let shell_idx = self
                        .cfg
                        .agents
                        .iter()
                        .position(|p| p.command.trim().is_empty())
                        .unwrap_or(0);
                    self.launch_preset(shell_idx, ctx);
                } else {
                    self.agents.panel_open = !self.agents.panel_open;
                }
                self.persist_session();
            }
            Cmd::ToggleCockpit => self.cockpit = !self.cockpit,
            Cmd::ToggleMdPreview => {
                // Cockpit ビュー中はエディタが出ていないので何もしない
                if !self.cockpit {
                    let ok = self
                        .editor
                        .active
                        .map(|i| {
                            let b = &self.editor.buffers[i];
                            markdown::is_markdown(&b.title, &b.lang)
                                || html::is_html(&b.title, &b.lang)
                        })
                        .unwrap_or(false);
                    if ok {
                        self.md_preview = !self.md_preview;
                    } else {
                        self.toast("Markdown / HTML ファイルではありません", false);
                    }
                }
            }
            Cmd::ToggleSidebar => {
                self.sidebar_open = !self.sidebar_open;
                self.persist_session();
            }
            Cmd::OpenGitPanel => {
                self.sidebar_open = true;
                self.sidebar_tab = SidebarTab::Git;
                self.persist_session();
            }
            Cmd::OpenFind => {
                self.find.open = true;
                self.find.focus = true;
            }
            Cmd::NewAgent(i) => self.launch_preset(i, ctx),
            Cmd::FocusAgent(i) => {
                if i < self.agents.sessions.len() {
                    self.agents.active = i;
                    self.agents.panel_open = true;
                    self.cockpit = false;
                }
            }
            Cmd::RestartAgent => {
                let i = self.agents.active;
                if let Err(e) = self.agents.restart(i, ctx) {
                    self.toast(e, false);
                }
            }
            Cmd::KillAgent => {
                let i = self.agents.active;
                self.agents.remove(i);
            }
            Cmd::SetTheme(name) => {
                self.theme = resolve_theme(&name);
                self.cfg.theme = name;
                theme::apply(ctx, &self.theme);
                for b in &mut self.editor.buffers {
                    b.cache = None;
                }
                config::save_state(&self.cfg);
                self.toast(format!("🎨 {} を適用しました", self.theme.label), true);
            }
            Cmd::OpenConfig => {
                config::ensure_default();
                self.open_path(&config::config_path());
            }
            Cmd::ReloadConfig => {
                self.cfg = config::load(&self.workspace, false);
                self.theme = resolve_theme(&self.cfg.theme);
                theme::apply(ctx, &self.theme);
                self.tree.show_hidden = self.cfg.show_hidden_files;
                self.tree.invalidate();
                self.rebuild_plugins();
                self.keys = Keybinds::from_overrides(&self.cfg.keybindings);
                for b in &mut self.editor.buffers {
                    b.cache = None;
                    b.gutter = None;
                }
                config::save_state(&self.cfg);
                self.toast("🔄 設定を再読み込みしました", true);
            }
            Cmd::FontInc => {
                self.cfg.editor_font_size = (self.cfg.editor_font_size + 1.0).min(32.0);
                self.cfg.terminal_font_size = (self.cfg.terminal_font_size + 1.0).min(28.0);
            }
            Cmd::FontDec => {
                self.cfg.editor_font_size = (self.cfg.editor_font_size - 1.0).max(8.0);
                self.cfg.terminal_font_size = (self.cfg.terminal_font_size - 1.0).max(7.0);
            }
            Cmd::SendFileToAgent => {
                let rel = self.editor.active.and_then(|i| {
                    let b = &self.editor.buffers[i];
                    b.path.as_ref().map(|p| {
                        p.strip_prefix(&self.workspace)
                            .unwrap_or(p)
                            .to_string_lossy()
                            .to_string()
                    })
                });
                match rel {
                    Some(r) => self.send_to_agent(format!("@{r} ")),
                    None => self.toast("保存済みのファイルを開いてください", false),
                }
            }
            Cmd::RefreshTree => {
                self.tree.invalidate();
                self.rebuild_index();
                self.toast("🌲 ツリーを再読み込みしました", true);
            }
            Cmd::SetApproval(mode) => {
                let mode = match mode.as_str() {
                    "auto" | "agent" => mode,
                    _ => "ask".into(),
                };
                self.cfg.approval_mode = mode.clone();
                config::save_state(&self.cfg);
                match mode.as_str() {
                    "auto" => self.toast_warn(
                        "⚡ 既定=全自動: 以後起動する Claude/Codex/Antigravity はすべて自動承認 (bypass フラグ付与)",
                    ),
                    "agent" => self.toast(
                        "🤖 既定=Agent優先: 以後は各プリセットのコマンドどおりに起動します（(全自動) プリセットのみ自動承認）",
                        true,
                    ),
                    _ => self.toast(
                        "🛡 既定=承認: 以後起動する Claude/Codex/Antigravity は操作ごとに許可が必要です",
                        true,
                    ),
                }
                if self.agents.running_count() > 0 {
                    self.toast("実行中のセッションは各行の 🛡 ボタン（または 🛡 全切替）で切替できます", true);
                }
            }
            Cmd::TogglePet => {
                self.cfg.show_pet = !self.cfg.show_pet;
                config::save_state(&self.cfg);
                self.toast(
                    if self.cfg.show_pet {
                        "🦀 ペットを表示しました"
                    } else {
                        "🦀 ペットを隠しました（🐾 で再表示）"
                    },
                    true,
                );
            }
            Cmd::CyclePermissionAll => {
                let n = self.agents.cycle_permission_all();
                if n > 0 {
                    self.toast_warn(format!(
                        "🛡 {n} 件のエージェントに権限モード切替を送信しました（各画面の表示を確認してください）"
                    ));
                } else {
                    self.toast("実行中の対応エージェントがありません", false);
                }
            }
            Cmd::SetPetImage => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("画像", &["png", "jpg", "jpeg", "gif", "webp"])
                    .pick_file()
                {
                    match load_pet_texture(ctx, &path) {
                        Some(tex) => {
                            self.pet_tex = Some(tex);
                            self.cfg.pet_image = Some(path.to_string_lossy().to_string());
                            self.cfg.show_pet = true;
                            config::save_state(&self.cfg);
                            self.toast("🖼 ペット画像を変更しました", true);
                        }
                        None => self.toast("画像を読み込めませんでした", false),
                    }
                }
            }
            Cmd::ResetPetImage => {
                self.pet_tex = None;
                self.cfg.pet_image = None;
                config::save_state(&self.cfg);
                self.toast("↺ ペットを既定の絵に戻しました", true);
            }
            Cmd::ResetPetPos => {
                self.pet_pos = None;
                self.cfg.pet_x = None;
                self.cfg.pet_y = None;
                config::save_state(&self.cfg);
                self.toast("🦀 ペットの位置を既定(右下)に戻しました", true);
            }
            Cmd::SetPetVariant(name) => {
                self.cfg.pet_variant = name;
                config::save_state(&self.cfg);
            }
            Cmd::SetPetScale(s) => {
                self.cfg.pet_scale = s;
                config::save_state(&self.cfg);
            }
            Cmd::TogglePetFreeRoam => {
                self.cfg.pet_free_roam = !self.cfg.pet_free_roam;
                config::save_state(&self.cfg);
            }
            Cmd::TogglePetSleep => {
                self.cfg.pet_sleep = !self.cfg.pet_sleep;
                config::save_state(&self.cfg);
            }
            Cmd::TogglePetSounds => {
                self.cfg.pet_sounds = !self.cfg.pet_sounds;
                config::save_state(&self.cfg);
                self.toast(
                    if self.cfg.pet_sounds {
                        "🔔 効果音を有効にしました"
                    } else {
                        "🔕 効果音を無効にしました"
                    },
                    true,
                );
            }
            Cmd::TogglePetBubbles => {
                self.cfg.pet_bubbles = !self.cfg.pet_bubbles;
                config::save_state(&self.cfg);
            }
            Cmd::ToggleRemote => {
                self.remote_open = !self.remote_open;
            }
            Cmd::VoiceInput(target) => {
                // 🎤 のトグル。録音中に押したら止める
                if self.voice.session.is_some() {
                    self.stop_voice();
                } else {
                    self.start_voice(target, ctx);
                }
            }
            Cmd::VoiceStop => self.stop_voice(),
            Cmd::SetVoiceTarget(t) => {
                self.voice.target = t;
                self.voice.last_sent_to = None;
                self.voice.reset_live();
                self.cfg.voice_target = t.name().to_string();
                config::save_state(&self.cfg);
            }
            Cmd::SetVoiceEngine(e) => {
                self.cfg.voice_engine = e;
                config::save_state(&self.cfg);
                if self.cfg.voice_engine == "command" && self.cfg.voice_command.trim().is_empty() {
                    self.toast_warn(
                        "外部エンジンを使うには config.toml の voice_command を設定してください",
                    );
                } else {
                    self.toast(
                        format!("🎤 音声認識エンジン: {}", self.cfg.voice_engine),
                        true,
                    );
                }
            }
            Cmd::SetVoiceLang(l) => {
                self.cfg.voice_lang = l;
                config::save_state(&self.cfg);
                self.toast(format!("🎤 認識言語: {}", self.cfg.voice_lang), true);
            }
            Cmd::SetVoiceKeyword(k) => {
                self.cfg.voice_keyword = k;
                config::save_state(&self.cfg);
                if self.cfg.voice_keyword.is_empty() {
                    self.toast("🎤 送信は常に手動 Enter になりました", true);
                } else {
                    self.toast(
                        format!(
                            "🎤 「{}」と話すとそのまま送信します",
                            self.cfg.voice_keyword
                        ),
                        true,
                    );
                }
            }
            Cmd::NewPlugin => {
                if self.new_plugin_name.is_none() {
                    self.new_plugin_name = Some(String::new());
                }
            }
            Cmd::InstallPlugin => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Zaivern プラグイン", &["zvplug", "zip"])
                    .pick_file()
                {
                    match plugins::install(&path) {
                        Ok(p) => {
                            let msg = format!(
                                "📦 {} v{} をインストールしました(コマンド{} / テーマ{} / スニペット{})",
                                p.name,
                                p.version,
                                p.commands.len(),
                                p.themes.len(),
                                p.snippet_files.len()
                            );
                            self.rebuild_plugins();
                            self.sidebar_open = true;
                            self.sidebar_tab = SidebarTab::Plugins;
                            self.toast(msg, true);
                        }
                        Err(e) => self.toast(format!("インストール失敗: {e}"), false),
                    }
                }
            }
            Cmd::RescanPlugins => {
                self.rebuild_plugins();
                self.toast(
                    format!("🔌 プラグインを再スキャンしました({} 件)", self.plugins.len()),
                    true,
                );
            }
            Cmd::ShowPlugins => {
                self.sidebar_open = true;
                self.sidebar_tab = SidebarTab::Plugins;
            }
            Cmd::RunPlugin(pi, ci) => {
                self.run_plugin_command(pi, ci, ctx);
            }
        }
    }

    fn run_action(&mut self, a: Action, ctx: &egui::Context) {
        match a {
            Action::OpenFile(p) => {
                let p = self.workspace.join(p);
                self.open_path(&p);
            }
            Action::Cmd(c) => self.apply_cmd(c, ctx),
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        let consume = |ctx: &egui::Context, sc: KeyboardShortcut| -> bool {
            ctx.input_mut(|i| i.consume_shortcut(&sc))
        };
        let mut cmds: Vec<Cmd> = Vec::new();
        let mut ops: Vec<EditOp> = Vec::new();

        // 修飾キーの多いものを先に消費する
        if consume(ctx, self.keys.get(BindAction::PaletteCommands)) {
            self.palette.open_commands();
        }
        if consume(ctx, self.keys.get(BindAction::PaletteFiles)) {
            self.palette.open_files();
        }
        if consume(ctx, self.keys.get(BindAction::SaveAs)) {
            cmds.push(Cmd::SaveAs);
        }
        if consume(ctx, self.keys.get(BindAction::Save)) {
            cmds.push(Cmd::Save);
        }
        if consume(ctx, self.keys.get(BindAction::CloseTab)) {
            cmds.push(Cmd::CloseTab);
        }
        if consume(ctx, self.keys.get(BindAction::NewFile)) {
            cmds.push(Cmd::NewFile);
        }
        if consume(ctx, self.keys.get(BindAction::ToggleTerminal))
            || consume(ctx, KeyboardShortcut::new(Modifiers::COMMAND, Key::Backtick))
        {
            cmds.push(Cmd::ToggleTerminal);
        }
        if consume(ctx, self.keys.get(BindAction::ToggleSidebar)) {
            cmds.push(Cmd::ToggleSidebar);
        }
        if consume(ctx, self.keys.get(BindAction::Find)) {
            cmds.push(Cmd::OpenFind);
        }
        if consume(ctx, self.keys.get(BindAction::ToggleCockpit)) {
            cmds.push(Cmd::ToggleCockpit);
        }
        if consume(ctx, self.keys.get(BindAction::ToggleMdPreview)) {
            cmds.push(Cmd::ToggleMdPreview);
        }
        if consume(ctx, self.keys.get(BindAction::NewAgent)) {
            cmds.push(Cmd::NewAgent(0));
        }
        if consume(ctx, self.keys.get(BindAction::FontInc))
            || consume(ctx, KeyboardShortcut::new(Modifiers::COMMAND, Key::Equals))
        {
            cmds.push(Cmd::FontInc);
        }
        if consume(ctx, self.keys.get(BindAction::FontDec)) {
            cmds.push(Cmd::FontDec);
        }

        // プラグインコマンドの keybind (plugin.toml の keybind = "cmd+alt+u" など)
        for (sc, pi, ci) in self.plugin_keys.clone() {
            if consume(ctx, sc) {
                cmds.push(Cmd::RunPlugin(pi, ci));
            }
        }

        // エディタ編集操作はエディタにフォーカスがあるときだけ消費する
        // (ターミナル内の alt+↑ 等を奪わないため)
        let editor_focused = self
            .editor
            .active
            .map(|i| {
                let id = egui::Id::new(("zaivern-buffer", self.editor.buffers[i].id));
                ctx.memory(|m| m.has_focus(id))
            })
            .unwrap_or(false);
        let mut pages: Vec<bool> = Vec::new();
        if editor_focused {
            if consume(ctx, self.keys.get(BindAction::ToggleComment)) {
                ops.push(EditOp::ToggleComment);
            }
            if consume(ctx, self.keys.get(BindAction::DuplicateLine)) {
                ops.push(EditOp::Duplicate);
            }
            if consume(ctx, self.keys.get(BindAction::MoveLineUp)) {
                ops.push(EditOp::Move(true));
            }
            if consume(ctx, self.keys.get(BindAction::MoveLineDown)) {
                ops.push(EditOp::Move(false));
            }
            // PageUp / PageDown: VS Code 同様に 1 画面ぶんカーソル移動+スクロール
            let (pgup, pgdn) = ctx.input_mut(|i| {
                (
                    i.consume_key(Modifiers::NONE, Key::PageUp),
                    i.consume_key(Modifiers::NONE, Key::PageDown),
                )
            });
            if pgup {
                pages.push(true);
            }
            if pgdn {
                pages.push(false);
            }
        }

        for c in cmds {
            self.apply_cmd(c, ctx);
        }
        for op in ops {
            self.editor_op(ctx, op);
        }
        for up in pages {
            self.page_move(ctx, up);
        }
    }

    /// PageUp/PageDown: カーソルを 1 画面ぶん上下の行へ移動し、
    /// ビューも同じ量だけスクロールする (VS Code の挙動)。
    fn page_move(&mut self, ctx: &egui::Context, up: bool) {
        let Some(i) = self.editor.active else {
            return;
        };
        let page = ((self.last_view_h / self.last_row_h.max(1.0)).floor() as usize)
            .saturating_sub(2)
            .max(1);
        let ed_id = egui::Id::new(("zaivern-buffer", self.editor.buffers[i].id));
        let cur = egui::TextEdit::load_state(ctx, ed_id)
            .and_then(|st| st.cursor.char_range())
            .map(|r| r.primary.index)
            .unwrap_or(0);
        let text = &self.editor.buffers[i].text;

        // 現在の (行, 桁) を求める
        let mut line = 0usize;
        let mut col = 0usize;
        for ch in text.chars().take(cur) {
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        let lines: Vec<&str> = text.split('\n').collect();
        let target = if up {
            line.saturating_sub(page)
        } else {
            (line + page).min(lines.len().saturating_sub(1))
        };

        // 移動先の char インデックス (桁は VS Code 同様できるだけ維持)
        let mut idx = 0usize;
        for l in lines.iter().take(target) {
            idx += l.chars().count() + 1;
        }
        idx += col.min(lines[target].chars().count());

        self.pending_select = Some((idx, idx));
        let dir = if up { -1.0 } else { 1.0 };
        self.pending_scroll =
            Some((self.last_scroll_y + dir * page as f32 * self.last_row_h).max(0.0));
    }

    // ─── UI: top bar ────────────────────────────────────────────────

    fn top_bar(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let mut cmds: Vec<Cmd> = Vec::new();
        let branch = self.git_branch();

        egui::TopBottomPanel::top("zv-top")
            .exact_height(42.0)
            .frame(
                egui::Frame::none()
                    .fill(theme.panel)
                    .inner_margin(egui::Margin::symmetric(10.0, 6.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        RichText::new("⚡ ZAIVERN")
                            .strong()
                            .size(16.0)
                            .color(theme.accent),
                    );
                    ui.separator();

                    let ws_name = self
                        .workspace
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| self.workspace.to_string_lossy().to_string());
                    ui.menu_button(format!("📂 {ws_name}"), |ui| {
                        if ui.button("フォルダを開く…").clicked() {
                            cmds.push(Cmd::OpenFolder);
                            ui.close_menu();
                        }
                        if ui.button("ツリーを再読み込み").clicked() {
                            cmds.push(Cmd::RefreshTree);
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("⚙ 設定 config.toml を開く").clicked() {
                            cmds.push(Cmd::OpenConfig);
                            ui.close_menu();
                        }
                        if ui.button("🔄 設定を再読み込み").clicked() {
                            cmds.push(Cmd::ReloadConfig);
                            ui.close_menu();
                        }
                    });

                    if let Some(b) = &branch {
                        ui.label(RichText::new(format!("🌿 {b}")).color(theme.text_dim));
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.menu_button("🎨", |ui| {
                            for t in theme::all() {
                                let sel = t.name == self.cfg.theme;
                                if ui.selectable_label(sel, t.label.clone()).clicked() {
                                    cmds.push(Cmd::SetTheme(t.name.clone()));
                                    ui.close_menu();
                                }
                            }
                            if !self.custom_themes.is_empty() {
                                ui.separator();
                                ui.menu_button(
                                    format!("🔌 カスタムテーマ ({})", self.custom_themes.len()),
                                    |ui| {
                                        egui::ScrollArea::vertical()
                                            .id_salt("custom-themes")
                                            .max_height(340.0)
                                            .show(ui, |ui| {
                                                for (label, path) in self.custom_themes.clone() {
                                                    let sel = self.cfg.theme == path;
                                                    if ui.selectable_label(sel, label).clicked() {
                                                        cmds.push(Cmd::SetTheme(path));
                                                        ui.close_menu();
                                                    }
                                                }
                                            });
                                    },
                                );
                            }
                        })
                        .response
                        .on_hover_text("テーマ（プラグインのカスタムテーマも使えます）");

                        // スマホリモート (QR コード表示)
                        if ui
                            .selectable_label(self.remote_open, "📱")
                            .on_hover_text(
                                "スマホから操作 — QR コードを表示\n\
                                 同じ Wi-Fi のスマホで読み取るだけで、編集・保存・\n\
                                 エージェント操作(Claude の承認も)ができます\n\
                                 🎤 音声入力: PC は Cockpit 各タブの 🎤 /\n\
                                 ブロードキャスト欄の 🎤、スマホは「エージェント」タブ",
                            )
                            .clicked()
                        {
                            cmds.push(Cmd::ToggleRemote);
                        }

                        // 音声入力: 🎤 で開始、隣の ⏹ で停止。押している間だけの
                        // 録音キーは無し — ボタンだけで完結する
                        let rec = self.voice.session.is_some();
                        if rec
                            && ui
                                .button(RichText::new("⏹").color(theme.err).strong())
                                .on_hover_text("音声入力を止める")
                                .clicked()
                        {
                            cmds.push(Cmd::VoiceStop);
                        }
                        if ui
                            .selectable_label(
                                rec,
                                RichText::new(if rec { "🔴" } else { "🎤" })
                                    .color(if rec { theme.err } else { theme.text }),
                            )
                            .on_hover_text(if rec {
                                "録音中 — もう一度押すと止まります".to_string()
                            } else {
                                // この PC で実際に通る経路を先に見せる (押してから
                                // 「使えません」と言われるのを避ける)
                                format!(
                                    "音声入力を始める\n\
                                     ⏹ を押すまで、話した内容が入力欄に入り続けます\n\
                                     (Enter は送られないので、確認して自分で送信)\n\
                                     {}",
                                    voice::route_hint(
                                        &self.cfg.voice_engine,
                                        &self.cfg.voice_lang,
                                        &self.cfg.voice_command,
                                    )
                                )
                            })
                            .clicked()
                        {
                            let t = voice::Target::from_name(&self.cfg.voice_target);
                            cmds.push(Cmd::VoiceInput(t));
                        }
                        ui.menu_button("▾", |ui| {
                            ui.label(
                                RichText::new(
                                    "話した内容は入力欄に入るだけです。\n\
                                     送信されるのは自分で Enter を押したときだけ。",
                                )
                                .size(11.0)
                                .color(theme.text_dim),
                            );
                            ui.separator();
                            if ui
                                .button(if rec {
                                    "⏹ 録音を止める"
                                } else {
                                    "🎤 いま録音する (アクティブなエージェントへ)"
                                })
                                .clicked()
                            {
                                let t = voice::Target::from_name(&self.cfg.voice_target);
                                cmds.push(Cmd::VoiceInput(t));
                                ui.close_menu();
                            }
                            ui.separator();
                            // 届け先。録音中は HUD からも切り替えられる
                            let cur = if rec {
                                self.voice.target
                            } else {
                                voice::Target::from_name(&self.cfg.voice_target)
                            };
                            ui.label(RichText::new("届け先").size(11.0).color(theme.text_dim));
                            for (t, label) in [
                                (voice::Target::Active, "🎯 アクティブなエージェント"),
                                (voice::Target::Broadcast, "📣 全エージェントへブロードキャスト"),
                            ] {
                                if ui.radio(cur == t, label).clicked() {
                                    cmds.push(Cmd::SetVoiceTarget(t));
                                    ui.close_menu();
                                }
                            }
                            ui.menu_button(format!("🌐 言語: {}", self.cfg.voice_lang), |ui| {
                                for (code, label) in [
                                    ("ja-JP", "日本語"),
                                    ("en-US", "English (US)"),
                                    ("zh-CN", "中文"),
                                    ("ko-KR", "한국어"),
                                ] {
                                    if ui.radio(self.cfg.voice_lang == code, label).clicked() {
                                        cmds.push(Cmd::SetVoiceLang(code.to_string()));
                                        ui.close_menu();
                                    }
                                }
                            });
                            ui.menu_button(
                                if self.cfg.voice_keyword.is_empty() {
                                    "🗣 合図で送信: なし (常に手動 Enter)".to_string()
                                } else {
                                    format!("🗣 合図で送信: 「{}」", self.cfg.voice_keyword)
                                },
                                |ui| {
                                    ui.label(
                                        RichText::new(
                                            "この言葉で終わったときだけ Enter まで送ります",
                                        )
                                        .size(11.0)
                                        .color(theme.text_dim),
                                    );
                                    for kw in ["", "送信", "送って", "オーケー"] {
                                        let sel = self.cfg.voice_keyword == kw;
                                        let label = if kw.is_empty() { "なし" } else { kw };
                                        if ui.radio(sel, label).clicked() {
                                            cmds.push(Cmd::SetVoiceKeyword(kw.to_string()));
                                            ui.close_menu();
                                        }
                                    }
                                },
                            );
                            ui.separator();
                            ui.menu_button(
                                format!("⚙ エンジン: {}", self.cfg.voice_engine),
                                |ui| {
                                    for (v, label) in [
                                        ("auto", "自動 (この OS に合わせる)"),
                                        ("mac", "macOS 内蔵の音声認識"),
                                        ("powershell", "Windows 標準の音声認識"),
                                        ("browser", "ブラウザの音声入力ページ"),
                                        ("command", "外部コマンド (config.toml の voice_command)"),
                                        ("off", "無効"),
                                    ] {
                                        if ui.radio(self.cfg.voice_engine == v, label).clicked() {
                                            cmds.push(Cmd::SetVoiceEngine(v.to_string()));
                                            ui.close_menu();
                                        }
                                    }
                                },
                            );
                        })
                        .response
                        .on_hover_text(
                            "音声入力 — キーを押している間だけ録音し、\n\
                             認識テキストをエージェントの入力欄へ挿入します。\n\
                             Enter は送られないので、確認してから自分で送信できます。",
                        );

                        // ペットメニュー(表示切替・画像変更)
                        ui.menu_button("🐾", |ui| {
                            let show = self.cfg.show_pet;
                            if ui
                                .selectable_label(show, if show { "🦀 表示中" } else { "🦀 非表示" })
                                .clicked()
                            {
                                cmds.push(Cmd::TogglePet);
                                ui.close_menu();
                            }
                            ui.separator();
                            if ui.button("🖼 画像を変更…").clicked() {
                                cmds.push(Cmd::SetPetImage);
                                ui.close_menu();
                            }
                            if ui.button("↺ 既定の絵に戻す").clicked() {
                                cmds.push(Cmd::ResetPetImage);
                                ui.close_menu();
                            }
                            if ui.button("🦀 位置を右下に戻す").clicked() {
                                cmds.push(Cmd::ResetPetPos);
                                ui.close_menu();
                            }
                            ui.separator();
                            // 見た目バリアント(ラジオ選択。候補は pet::PetVariant から生成)
                            ui.menu_button("🎭 見た目", |ui| {
                                for (v, label) in [
                                    (pet::PetVariant::Blocky, "🟦 ブロック"),
                                    (pet::PetVariant::Crab, "🦀 カニ"),
                                    (pet::PetVariant::Cat, "🐱 ネコ"),
                                    (pet::PetVariant::Cloud, "☁ クラウド"),
                                ] {
                                    if ui.radio(self.cfg.pet_variant == v.name(), label).clicked() {
                                        cmds.push(Cmd::SetPetVariant(v.name().to_string()));
                                        ui.close_menu();
                                    }
                                }
                            });
                            // 表示スケール(ラジオ選択)
                            ui.menu_button("📏 サイズ", |ui| {
                                for (v, label) in [(0.75f32, "小"), (1.0, "中"), (1.4, "大")] {
                                    let sel = (self.cfg.pet_scale - v).abs() < 0.01;
                                    if ui.radio(sel, label).clicked() {
                                        cmds.push(Cmd::SetPetScale(v));
                                        ui.close_menu();
                                    }
                                }
                            });
                            ui.separator();
                            // 挙動の切替(チェックボックス。cfg は apply_cmd 側で保存)
                            let mut roam = self.cfg.pet_free_roam;
                            if ui.checkbox(&mut roam, "🚶 うろうろ散歩").clicked() {
                                cmds.push(Cmd::TogglePetFreeRoam);
                            }
                            let mut sleep = self.cfg.pet_sleep;
                            if ui.checkbox(&mut sleep, "💤 居眠り").clicked() {
                                cmds.push(Cmd::TogglePetSleep);
                            }
                            let mut sounds = self.cfg.pet_sounds;
                            if ui.checkbox(&mut sounds, "🔔 効果音").clicked() {
                                cmds.push(Cmd::TogglePetSounds);
                            }
                            let mut bubbles = self.cfg.pet_bubbles;
                            if ui.checkbox(&mut bubbles, "💬 承認バブル").clicked() {
                                cmds.push(Cmd::TogglePetBubbles);
                            }
                        })
                        .response
                        .on_hover_text("デスクトップペット 🦀 の表示・画像変更");

                        // 実行中の対応エージェントを一括で権限モード切替
                        if self.agents.running_count() > 0
                            && ui
                                .button(RichText::new("🛡 全切替").color(theme.ok))
                                .on_hover_text(
                                    "実行中の Claude/Codex/Antigravity に権限モード切替を送信します。\n\
                                     Claude/Antigravity は Shift+Tab、Codex は /permissions を送ります",
                                )
                                .clicked()
                        {
                            cmds.push(Cmd::CyclePermissionAll);
                        }

                        // 承認モード切替(次回起動の既定)。クリックで 承認→全自動→Agent優先 を順送り
                        let mode = self.cfg.approval_mode.as_str();
                        let (ap_label, next_mode, highlight) = match mode {
                            "auto" => (
                                RichText::new("⚡ 既定:全自動").color(theme.warn).strong(),
                                "agent",
                                true,
                            ),
                            "agent" => (
                                RichText::new("🤖 既定:Agent優先").color(theme.ok).strong(),
                                "ask",
                                true,
                            ),
                            _ => (RichText::new("🛡 既定:承認").color(theme.ok), "auto", false),
                        };
                        if ui
                            .selectable_label(highlight, ap_label)
                            .on_hover_text(
                                "「次に起動する」エージェント (Claude/Codex/Antigravity) の既定権限モード\n\
                                 🛡 承認 = 操作のたびに許可が必要（bypass フラグを除去）\n\
                                 ⚡ 全自動 = すべて自動YES（bypass フラグを付与）\n\
                                 🤖 Agent優先 = Agent欄プリセットのコマンドどおり（(全自動) プリセットのみ自動YES）\n\
                                 クリックで 承認→全自動→Agent優先 の順に切替\n\
                                 ※ 実行中のセッションは各行の 🛡 ボタンで個別に切替できます",
                            )
                            .clicked()
                        {
                            cmds.push(Cmd::SetApproval(next_mode.into()));
                        }

                        let cockpit =
                            ui.selectable_label(self.cockpit, RichText::new("🎛 Cockpit"));
                        if cockpit.on_hover_text("全エージェント一覧 (⌘⇧C)").clicked() {
                            cmds.push(Cmd::ToggleCockpit);
                        }

                        ui.menu_button("🤖 Agent ＋", |ui| {
                            for (i, p) in self.cfg.agents.clone().into_iter().enumerate() {
                                if ui.button(format!("{} {}", p.icon, p.name)).clicked() {
                                    cmds.push(Cmd::NewAgent(i));
                                    ui.close_menu();
                                }
                            }
                        })
                        .response
                        .on_hover_text("エージェントを起動 (⌘⇧A)");

                        if ui
                            .button("🔍")
                            .on_hover_text("コマンドパレット (⌘P / ⌘⇧P)")
                            .clicked()
                        {
                            self.palette.open_files();
                        }

                        let running = self.agents.running_count();
                        if running > 0 {
                            ui.label(
                                RichText::new(format!("● {running} 稼働中")).color(theme.ok),
                            );
                        }
                    });
                });
            });

        for c in cmds {
            self.apply_cmd(c, ctx);
        }
    }

    // ─── UI: status bar ─────────────────────────────────────────────

    fn status_bar(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let branch = self.git_branch();
        self.gitinfo.refresh_if_stale();
        let dirty = self.gitinfo.dirty_count();
        let mut toggle_cockpit = false;

        egui::TopBottomPanel::bottom("zv-status")
            .exact_height(26.0)
            .frame(
                egui::Frame::none()
                    .fill(theme.panel_alt)
                    .inner_margin(egui::Margin::symmetric(10.0, 4.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    let dim = |s: String| RichText::new(s).size(11.5).color(theme.text_dim);
                    ui.label(dim(format!("📂 {}", self.workspace.display())));
                    if let Some(b) = &branch {
                        ui.label(dim(format!("🌿 {b}")));
                        if dirty > 0 {
                            ui.label(
                                RichText::new(format!("±{dirty}"))
                                    .size(11.5)
                                    .color(theme.warn),
                            );
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(dim("Zaivern v0.2".into()));
                        if let Some(r) = &self.remote {
                            ui.label(dim(format!("📱 :{}", r.port)));
                        }
                        let (ap_text, ap_color) = match self.cfg.approval_mode.as_str() {
                            "auto" => ("⚡ 全自動", theme.warn),
                            "agent" => ("🤖 Agent優先", theme.ok),
                            _ => ("🛡 承認", theme.ok),
                        };
                        ui.label(RichText::new(ap_text).size(11.5).color(ap_color));
                        ui.label(dim(self.theme.label.clone()));
                        let (ln, col) = self.editor.cursor;
                        if let Some(i) = self.editor.active {
                            ui.label(dim(format!("Ln {ln}, Col {col}")));
                            ui.label(dim(self.editor.buffers[i].lang.clone()));
                        }
                        // LSP 診断件数
                        let (derr, dwarn) = self.diag_counts;
                        if derr > 0 {
                            ui.label(
                                RichText::new(format!("⛔ {derr}")).size(11.5).color(theme.err),
                            );
                        }
                        if dwarn > 0 {
                            ui.label(
                                RichText::new(format!("⚠ {dwarn}")).size(11.5).color(theme.warn),
                            );
                        }
                        let total = self.agents.sessions.len();
                        let running = self.agents.running_count();
                        if total > 0 {
                            let r = ui.add(
                                egui::Label::new(
                                    RichText::new(format!("🤖 {running}/{total}"))
                                        .size(11.5)
                                        .color(if running > 0 {
                                            theme.ok
                                        } else {
                                            theme.text_dim
                                        }),
                                )
                                .sense(egui::Sense::click()),
                            );
                            if r.on_hover_text("Cockpit を開く").clicked() {
                                toggle_cockpit = true;
                            }
                        }
                    });
                });
            });

        if toggle_cockpit {
            self.cockpit = !self.cockpit;
        }
    }

    // ─── UI: sidebar ────────────────────────────────────────────────

    fn sidebar(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let mut actions = TreeActions::default();
        let mut launch: Option<usize> = None;
        let mut focus: Option<usize> = None;
        let mut restart: Option<usize> = None;
        let mut remove: Option<usize> = None;
        let mut cycle: Option<usize> = None;
        let mut refresh = false;
        let mut nf_root = false;
        let mut nd_root = false;
        let mut pl_new = false;
        let mut pl_install = false;
        let mut pl_rescan = false;
        let mut pl_uninstall: Option<PathBuf> = None;
        let mut pl_theme: Option<String> = None;
        let mut pl_run: Option<(usize, usize)> = None;
        let mut pl_export: Option<usize> = None;
        let mut pl_open: Option<PathBuf> = None;
        let mut git_actions = git_panel::GitActions::default();

        egui::SidePanel::left("zv-side")
            .resizable(true)
            .default_width(255.0)
            .width_range(180.0..=440.0)
            .show_animated(ctx, self.sidebar_open, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.sidebar_tab, SidebarTab::Files, "📁 ファイル");
                    let agents_label = format!("🤖 Agents ({})", self.agents.sessions.len());
                    ui.selectable_value(&mut self.sidebar_tab, SidebarTab::Agents, agents_label);
                    let pl_label = format!("🔌 プラグイン ({})", self.plugins.len());
                    ui.selectable_value(&mut self.sidebar_tab, SidebarTab::Plugins, pl_label);
                    ui.selectable_value(&mut self.sidebar_tab, SidebarTab::Git, "🌿 Git");
                });
                ui.separator();

                match self.sidebar_tab {
                    SidebarTab::Files => {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(
                                    self.workspace
                                        .file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_default(),
                                )
                                .strong(),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("⟳").on_hover_text("再読み込み").clicked() {
                                        refresh = true;
                                    }
                                    if ui.button("🗂").on_hover_text("新規フォルダ").clicked() {
                                        nd_root = true;
                                    }
                                    if ui.button("➕").on_hover_text("新規ファイル").clicked() {
                                        nf_root = true;
                                    }
                                },
                            );
                        });
                        egui::ScrollArea::vertical()
                            .id_salt("zv-tree")
                            .auto_shrink(false)
                            .show(ui, |ui| {
                                self.tree.ui(ui, &theme, &mut actions);
                            });
                    }
                    SidebarTab::Agents => {
                        egui::ScrollArea::vertical()
                            .id_salt("zv-agents")
                            .auto_shrink(false)
                            .show(ui, |ui| {
                                for (i, s) in self.agents.sessions.iter().enumerate() {
                                    let active = i == self.agents.active;
                                    let frame = egui::Frame::none()
                                        .fill(if active {
                                            theme.accent_soft
                                        } else {
                                            Color32::TRANSPARENT
                                        })
                                        .rounding(egui::Rounding::same(6.0))
                                        .inner_margin(egui::Margin::symmetric(8.0, 6.0));
                                    let fr = frame.show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            let dot = if s.running() {
                                                if s.attention {
                                                    RichText::new("●").color(theme.warn)
                                                } else {
                                                    RichText::new("●").color(theme.ok)
                                                }
                                            } else {
                                                RichText::new("○").color(theme.err)
                                            };
                                            ui.label(dot);
                                            let badge = if s.is_permission_agent() {
                                                s.approval_badge()
                                            } else {
                                                ""
                                            };
                                            let permission_hint = s.permission_switch_hint();
                                            ui.label(
                                                RichText::new(format!(
                                                    "{}{} {}",
                                                    badge, s.icon, s.title
                                                ))
                                                .color(theme.text),
                                            );
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if ui.small_button("✕").clicked() {
                                                        remove = Some(i);
                                                    }
                                                    if ui.small_button("⟳").clicked() {
                                                        restart = Some(i);
                                                    }
                                                    if let Some(hint) = permission_hint {
                                                        if ui
                                                            .small_button("🛡")
                                                            .on_hover_text(hint)
                                                            .clicked()
                                                        {
                                                            cycle = Some(i);
                                                        }
                                                    }
                                                    ui.label(
                                                        RichText::new(s.uptime())
                                                            .size(10.5)
                                                            .color(theme.text_dim),
                                                    );
                                                },
                                            );
                                        });
                                    });
                                    let resp = ui.interact(
                                        fr.response.rect,
                                        egui::Id::new(("agent-row", i)),
                                        egui::Sense::click(),
                                    );
                                    if resp.clicked() {
                                        focus = Some(i);
                                    }
                                }

                                ui.add_space(8.0);
                                ui.label(RichText::new("── プリセット ──").color(theme.text_dim));
                                for (i, p) in self.cfg.agents.iter().enumerate() {
                                    if ui
                                        .add_sized(
                                            [ui.available_width(), 26.0],
                                            egui::Button::new(format!("{} {}", p.icon, p.name)),
                                        )
                                        .clicked()
                                    {
                                        launch = Some(i);
                                    }
                                }
                            });
                    }
                    SidebarTab::Plugins => {
                        ui.horizontal(|ui| {
                            if ui
                                .button("➕ 新規作成")
                                .on_hover_text("プラグインのテンプレート一式を生成")
                                .clicked()
                            {
                                pl_new = true;
                            }
                            if ui
                                .button("📦 インストール…")
                                .on_hover_text(".zvplug / .zip を取り込む")
                                .clicked()
                            {
                                pl_install = true;
                            }
                            if ui.button("⟳").on_hover_text("再スキャン").clicked() {
                                pl_rescan = true;
                            }
                        });
                        ui.label(
                            RichText::new(
                                "コマンド・テーマ・スニペットを 1 フォルダで。📤 で配布用 .zvplug を作成",
                            )
                            .size(10.5)
                            .color(theme.text_dim),
                        );
                        ui.separator();
                        egui::ScrollArea::vertical()
                            .id_salt("zv-plugins")
                            .auto_shrink(false)
                            .show(ui, |ui| {
                                if self.plugins.is_empty() {
                                    ui.label(
                                        RichText::new(
                                            "プラグインがありません。➕ から自作できます",
                                        )
                                        .color(theme.text_dim),
                                    );
                                }
                                for (pi, p) in self.plugins.iter().enumerate() {
                                    egui::Frame::none()
                                        .rounding(egui::Rounding::same(6.0))
                                        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                                        .fill(theme.panel_alt)
                                        .show(ui, |ui| {
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    RichText::new(&p.name)
                                                        .strong()
                                                        .color(theme.text),
                                                );
                                                ui.label(
                                                    RichText::new(format!("v{}", p.version))
                                                        .size(10.5)
                                                        .color(theme.text_dim),
                                                );
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        if ui
                                                            .small_button("🗑")
                                                            .on_hover_text("アンインストール")
                                                            .clicked()
                                                        {
                                                            pl_uninstall = Some(p.dir.clone());
                                                        }
                                                        if ui
                                                            .small_button("📤")
                                                            .on_hover_text(
                                                                "配布用 .zvplug をエクスポート",
                                                            )
                                                            .clicked()
                                                        {
                                                            pl_export = Some(pi);
                                                        }
                                                        if ui
                                                            .small_button("📝")
                                                            .on_hover_text("plugin.toml を開く")
                                                            .clicked()
                                                        {
                                                            pl_open =
                                                                Some(p.dir.join("plugin.toml"));
                                                        }
                                                    },
                                                );
                                            });
                                            if let Some(err) = &p.error {
                                                ui.label(
                                                    RichText::new(format!("⚠ {err}"))
                                                        .size(10.5)
                                                        .color(theme.warn),
                                                );
                                                return;
                                            }
                                            if !p.description.is_empty() {
                                                ui.label(
                                                    RichText::new(&p.description)
                                                        .size(10.5)
                                                        .color(theme.text_dim),
                                                );
                                            }
                                            ui.label(
                                                RichText::new(format!(
                                                    "▶{}  🎨{}  ✂{}{}",
                                                    p.commands.len(),
                                                    p.themes.len(),
                                                    p.snippet_files.len(),
                                                    if p.author.is_empty() {
                                                        String::new()
                                                    } else {
                                                        format!("  by {}", p.author)
                                                    }
                                                ))
                                                .size(10.5)
                                                .color(theme.text_dim),
                                            );
                                            for (ci, c) in p.commands.iter().enumerate() {
                                                let btn = ui
                                                    .small_button(format!("{} {}", c.icon, c.title));
                                                let btn = match &c.keybind {
                                                    Some(k) => btn.on_hover_text(k),
                                                    None => btn,
                                                };
                                                if btn.clicked() {
                                                    pl_run = Some((pi, ci));
                                                }
                                            }
                                            for (label, path) in &p.themes {
                                                if ui
                                                    .small_button(format!("🎨 {label}"))
                                                    .clicked()
                                                {
                                                    pl_theme = Some(
                                                        path.to_string_lossy().to_string(),
                                                    );
                                                }
                                            }
                                        });
                                    ui.add_space(4.0);
                                }
                            });
                    }
                    SidebarTab::Git => {
                        egui::ScrollArea::vertical()
                            .id_salt("zv-git")
                            .auto_shrink(false)
                            .show(ui, |ui| {
                                self.git_panel.ui(ui, &theme, &mut git_actions);
                            });
                    }
                }
            });

        if let Some((msg, ok)) = git_actions.toast {
            self.toast(msg, ok);
        }
        if let Some(dir) = git_actions.open_path {
            self.open_workspace(dir, ctx);
        }
        if pl_new {
            self.apply_cmd(Cmd::NewPlugin, ctx);
        }
        if pl_install {
            self.apply_cmd(Cmd::InstallPlugin, ctx);
        }
        if pl_rescan {
            self.apply_cmd(Cmd::RescanPlugins, ctx);
        }
        if let Some(dir) = pl_uninstall {
            match plugins::uninstall(&dir) {
                Ok(()) => {
                    self.rebuild_plugins();
                    self.toast("🗑 プラグインをアンインストールしました", true);
                }
                Err(e) => self.toast(format!("アンインストール失敗: {e}"), false),
            }
        }
        if let Some(pi) = pl_export {
            let res = self
                .plugins
                .get(pi)
                .map(|p| plugins::export(p, &self.workspace));
            match res {
                Some(Ok(path)) => {
                    self.toast(format!("📤 エクスポートしました: {}", path.display()), true)
                }
                Some(Err(e)) => self.toast(format!("エクスポート失敗: {e}"), false),
                None => {}
            }
        }
        if let Some(path) = pl_open {
            self.open_path(&path);
        }
        if let Some((pi, ci)) = pl_run {
            self.apply_cmd(Cmd::RunPlugin(pi, ci), ctx);
        }
        if let Some(t) = pl_theme {
            self.apply_cmd(Cmd::SetTheme(t), ctx);
        }
        if refresh {
            self.apply_cmd(Cmd::RefreshTree, ctx);
        }
        if let Some(p) = actions.open {
            self.open_path(&p);
        }
        if let Some(t) = actions.send_to_agent {
            self.send_to_agent(t);
        }
        if nf_root {
            self.tree.start_new_file(self.workspace.clone());
        }
        if nd_root {
            self.tree.start_new_dir(self.workspace.clone());
        }
        if let Some(p) = actions.create_file {
            let res = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&p)
                .map(|_| ());
            match res {
                Ok(()) => {
                    self.tree.invalidate();
                    self.open_path(&p);
                    self.toast(
                        format!("➕ {} を作成しました", rel_label(&p, &self.workspace)),
                        true,
                    );
                }
                Err(e) => self.toast(format!("作成できません: {e}"), false),
            }
        }
        if let Some(p) = actions.create_dir {
            if p.exists() {
                self.toast(
                    format!("既に存在します: {}", rel_label(&p, &self.workspace)),
                    false,
                );
            } else {
                match std::fs::create_dir(&p) {
                    Ok(()) => {
                        self.tree.invalidate();
                        self.toast(
                            format!("🗂 {} を作成しました", rel_label(&p, &self.workspace)),
                            true,
                        );
                    }
                    Err(e) => self.toast(format!("フォルダを作成できません: {e}"), false),
                }
            }
        }
        if let Some((from, to)) = actions.rename {
            if to.exists() {
                self.toast(
                    format!("既に存在します: {}", rel_label(&to, &self.workspace)),
                    false,
                );
            } else {
                match std::fs::rename(&from, &to) {
                    Ok(()) => {
                        self.retarget_buffers(&from, &to);
                        self.tree.invalidate();
                        self.persist_session();
                        self.toast(
                            format!("✏ {} に変更しました", rel_label(&to, &self.workspace)),
                            true,
                        );
                    }
                    Err(e) => self.toast(format!("名前を変更できません: {e}"), false),
                }
            }
        }
        if let Some(p) = actions.delete {
            self.pending_delete = Some(p);
        }
        if let Some(i) = launch {
            self.launch_preset(i, ctx);
        }
        if let Some(i) = focus {
            self.apply_cmd(Cmd::FocusAgent(i), ctx);
        }
        if let Some(i) = restart {
            if let Err(e) = self.agents.restart(i, ctx) {
                self.toast(e, false);
            }
        }
        if let Some(i) = cycle {
            match self.agents.cycle_permission(i) {
                Some(hint) => self.toast_warn(format!(
                    "🛡 権限モード切替を送信しました（{hint} / 画面を確認してください）"
                )),
                None => self.toast("このセッションは権限モード切替に未対応です", false),
            }
        }
        if let Some(i) = remove {
            self.agents.remove(i);
        }
    }

    // ─── UI: terminal panel ─────────────────────────────────────────

    fn terminal_panel(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let show = self.agents.panel_open && !self.cockpit;
        let mut launch: Option<usize> = None;
        let mut restart: Option<usize> = None;
        let mut remove: Option<usize> = None;
        let mut cycle: Option<usize> = None;

        egui::TopBottomPanel::bottom("zv-terminal")
            .resizable(true)
            .default_height(300.0)
            .min_height(140.0)
            .frame(
                egui::Frame::none()
                    .fill(theme.panel)
                    .inner_margin(egui::Margin::same(6.0)),
            )
            .show_animated(ctx, show, |ui| {
                ui.horizontal(|ui| {
                    let controls_w = 150.0;
                    egui::ScrollArea::horizontal()
                        .id_salt("term-tabs")
                        .max_width((ui.available_width() - controls_w).max(120.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let active_ix = self.agents.active;
                                let mut set_active: Option<usize> = None;
                                for (i, s) in self.agents.sessions.iter().enumerate() {
                                    let dot = if s.running() {
                                        if s.attention {
                                            RichText::new("●").size(10.0).color(theme.warn)
                                        } else {
                                            RichText::new("●").size(10.0).color(theme.ok)
                                        }
                                    } else {
                                        RichText::new("○").size(10.0).color(theme.err)
                                    };
                                    ui.label(dot);
                                    let badge = if s.is_permission_agent() {
                                        s.approval_badge()
                                    } else {
                                        ""
                                    };
                                    let r = ui.selectable_label(
                                        i == active_ix,
                                        format!("{}{} {}", badge, s.icon, s.title),
                                    );
                                    if r.clicked() {
                                        set_active = Some(i);
                                    }
                                    r.context_menu(|ui| {
                                        if let Some(hint) = s.permission_switch_hint() {
                                            if ui.button(format!("🛡 {hint}")).clicked() {
                                                cycle = Some(i);
                                                ui.close_menu();
                                            }
                                        }
                                        if ui.button("⟳ 再起動").clicked() {
                                            restart = Some(i);
                                            ui.close_menu();
                                        }
                                        if ui.button("✕ 閉じる").clicked() {
                                            remove = Some(i);
                                            ui.close_menu();
                                        }
                                    });
                                }
                                if let Some(i) = set_active {
                                    self.agents.active = i;
                                }
                            });
                        });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("⌄").on_hover_text("パネルを隠す (⌘J)").clicked() {
                            self.agents.panel_open = false;
                        }
                        ui.menu_button("＋", |ui| {
                            for (i, p) in self.cfg.agents.iter().enumerate() {
                                if ui.button(format!("{} {}", p.icon, p.name)).clicked() {
                                    launch = Some(i);
                                    ui.close_menu();
                                }
                            }
                        });
                        if !self.agents.sessions.is_empty() {
                            if ui.button("✕").on_hover_text("セッションを閉じる").clicked() {
                                remove = Some(self.agents.active);
                            }
                            if ui.button("⟳").on_hover_text("再起動").clicked() {
                                restart = Some(self.agents.active);
                            }
                            let permission_hint = self
                                .agents
                                .sessions
                                .get(self.agents.active)
                                .and_then(|s| s.permission_switch_hint());
                            if let Some(hint) = permission_hint {
                                if ui
                                    .button("🛡")
                                    .on_hover_text(format!(
                                        "{hint}\n\
                                         実行中セッションの画面表示を確認してください"
                                    ))
                                    .clicked()
                                {
                                    cycle = Some(self.agents.active);
                                }
                            }
                        }
                    });
                });

                ui.add_space(4.0);

                let font = self.cfg.terminal_font_size;
                if let Some(s) = self.agents.active_session() {
                    terminal::draw(ui, s, &theme, font, true, true, true);
                } else {
                    ui.vertical_centered(|ui| {
                        ui.add_space(20.0);
                        ui.label(
                            RichText::new("セッションがありません — ＋ から起動してください")
                                .color(theme.text_dim),
                        );
                    });
                }
            });

        if let Some(i) = launch {
            self.launch_preset(i, ctx);
        }
        if let Some(i) = restart {
            if let Err(e) = self.agents.restart(i, ctx) {
                self.toast(e, false);
            }
        }
        if let Some(i) = cycle {
            match self.agents.cycle_permission(i) {
                Some(hint) => self.toast_warn(format!(
                    "🛡 権限モード切替を送信しました（{hint} / 画面を確認してください）"
                )),
                None => self.toast("このセッションは権限モード切替に未対応です", false),
            }
        }
        if let Some(i) = remove {
            self.agents.remove(i);
        }
    }

    // ─── UI: cockpit ────────────────────────────────────────────────

    fn cockpit_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let mut launch: Option<usize> = None;
        let mut focus: Option<usize> = None;
        let mut restart: Option<usize> = None;
        let mut remove: Option<usize> = None;
        let mut cycle: Option<usize> = None;
        let mut cycle_all = false;
        let mut broadcast: Option<String> = None;
        let mut voice: Option<u64> = None;
        let mut voice_all = false;
        let mut voice_stop = false;

        egui::Frame::none()
            .inner_margin(egui::Margin::same(12.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("🎛 Agent Cockpit")
                            .size(20.0)
                            .strong()
                            .color(theme.accent),
                    );
                    let running = self.agents.running_count();
                    let total = self.agents.sessions.len();
                    ui.label(
                        RichText::new(format!("{running} 稼働中 / {total} セッション"))
                            .color(theme.text_dim),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("✕ 閉じる").clicked() {
                            self.cockpit = false;
                        }
                        if ui
                            .button(RichText::new("🛡 全切替").color(theme.ok))
                            .on_hover_text(
                                "実行中の Claude/Codex/Antigravity に権限モード切替を送信します。\n\
                                 Claude/Antigravity は Shift+Tab、Codex は /permissions を送ります",
                            )
                            .clicked()
                        {
                            cycle_all = true;
                        }
                        ui.menu_button("＋ Agent", |ui| {
                            for (i, p) in self.cfg.agents.iter().enumerate() {
                                if ui.button(format!("{} {}", p.icon, p.name)).clicked() {
                                    launch = Some(i);
                                    ui.close_menu();
                                }
                            }
                        });
                        let send = ui.button("📣 送信");
                        let input = ui.add(
                            egui::TextEdit::singleline(&mut self.broadcast_input)
                                .desired_width(300.0)
                                .hint_text("全エージェントへブロードキャスト…"),
                        );
                        let enter = input.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if (send.clicked() || enter) && !self.broadcast_input.trim().is_empty() {
                            broadcast = Some(self.broadcast_input.trim().to_string());
                            self.broadcast_input.clear();
                        }
                        // 音声で全エージェントの入力欄へ入れる (送信は各自 Enter)
                        let rec = self.voice.session.is_some();
                        if rec
                            && ui
                                .button(RichText::new("⏹").color(theme.err).strong())
                                .on_hover_text("音声入力を止める")
                                .clicked()
                        {
                            voice_stop = true;
                        }
                        if ui
                            .selectable_label(
                                rec && self.voice.target == voice::Target::Broadcast,
                                if rec { "🔴" } else { "🎤" },
                            )
                            .on_hover_text(
                                "音声入力 → 全エージェントの入力欄へ\n\
                                 ⏹ を押すまで話した内容が入り続けます。\n\
                                 送信はされないので、自分で Enter を押してください",
                            )
                            .clicked()
                        {
                            voice_all = true;
                        }
                    });
                });
                ui.add_space(8.0);

                let n = self.agents.sessions.len();
                if n == 0 {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.25);
                        ui.label(RichText::new("🎛").size(52.0));
                        ui.label(
                            RichText::new("エージェントがまだいません")
                                .size(18.0)
                                .color(theme.text),
                        );
                        ui.label(
                            RichText::new("プリセットから並列セッションを起動しましょう")
                                .color(theme.text_dim),
                        );
                        ui.add_space(12.0);
                        for (i, p) in self.cfg.agents.clone().into_iter().enumerate() {
                            if ui
                                .add_sized(
                                    [280.0, 34.0],
                                    egui::Button::new(format!("{} {}", p.icon, p.name)),
                                )
                                .clicked()
                            {
                                launch = Some(i);
                            }
                        }
                    });
                    return;
                }

                let cols = if n <= 1 { 1 } else { 2 };
                let rows = n.div_ceil(cols);
                let spacing = 10.0;
                let avail = ui.available_size();
                let cell_w = (avail.x - spacing * (cols as f32 - 1.0)) / cols as f32 - 4.0;
                let cell_h = (((avail.y - spacing * (rows as f32 - 1.0)) / rows as f32) - 4.0)
                    .max(150.0);
                let mini_font = (self.cfg.terminal_font_size - 3.0).clamp(8.0, 14.0);

                egui::ScrollArea::vertical()
                    .id_salt("cockpit-grid")
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        for row in 0..rows {
                            ui.horizontal(|ui| {
                                for col in 0..cols {
                                    let i = row * cols + col;
                                    if i >= n {
                                        continue;
                                    }
                                    let active = i == self.agents.active;
                                    let stroke = if active {
                                        egui::Stroke::new(1.5_f32, theme.accent)
                                    } else {
                                        egui::Stroke::new(1.0_f32, theme.border)
                                    };
                                    egui::Frame::none()
                                        .fill(theme.panel_alt)
                                        .stroke(stroke)
                                        .rounding(egui::Rounding::same(8.0))
                                        .inner_margin(egui::Margin::same(8.0))
                                        .show(ui, |ui| {
                                            // Frame は親 (horizontal な行) のレイアウトを
                                            // 継承するため、明示的に縦積みへ切り替える。
                                            // これが無いとヘッダーとターミナルが横に並び
                                            // 画面外へはみ出す。
                                            ui.vertical(|ui| {
                                            ui.set_width(cell_w - 18.0);
                                            ui.set_height(cell_h - 18.0);
                                            let s = &mut self.agents.sessions[i];
                                            let sid = s.id;
                                            ui.horizontal(|ui| {
                                                let dot = if s.running() {
                                                    if s.attention {
                                                        RichText::new("●").color(theme.warn)
                                                    } else {
                                                        RichText::new("●").color(theme.ok)
                                                    }
                                                } else {
                                                    RichText::new("○").color(theme.err)
                                                };
                                                ui.label(dot);
                                                let badge = if s.is_permission_agent() {
                                                    s.approval_badge()
                                                } else {
                                                    ""
                                                };
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{}{} {}",
                                                        badge, s.icon, s.title
                                                    ))
                                                    .strong()
                                                    .color(theme.text),
                                                );
                                                ui.label(
                                                    RichText::new(s.uptime())
                                                        .size(10.5)
                                                        .color(theme.text_dim),
                                                );
                                                let permission_hint = s.permission_switch_hint();
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        if ui
                                                            .small_button("✕")
                                                            .on_hover_text("閉じる")
                                                            .clicked()
                                                        {
                                                            remove = Some(i);
                                                        }
                                                        if ui
                                                            .small_button("⟳")
                                                            .on_hover_text("再起動")
                                                            .clicked()
                                                        {
                                                            restart = Some(i);
                                                        }
                                                        if let Some(hint) = permission_hint {
                                                            if ui
                                                                .small_button("🛡")
                                                                .on_hover_text(hint)
                                                                .clicked()
                                                            {
                                                                cycle = Some(i);
                                                            }
                                                        }
                                                        if ui
                                                            .small_button("🔍")
                                                            .on_hover_text(
                                                                "下部パネルにフォーカス",
                                                            )
                                                            .clicked()
                                                        {
                                                            focus = Some(i);
                                                        }
                                                        if ui
                                                            .small_button(
                                                                if self.voice.target == voice::Target::Session(sid)
                                                                    && self.voice.session.is_some()
                                                                {
                                                                    "🔴"
                                                                } else {
                                                                    "🎤"
                                                                },
                                                            )
                                                            .on_hover_text(
                                                                "このエージェントへ音声入力\n\
                                                                 話した内容がこのタブの入力欄に入ります。\n\
                                                                 送信されないので、確認して Enter を押してください",
                                                            )
                                                            .clicked()
                                                        {
                                                            voice = Some(sid);
                                                        }
                                                    },
                                                );
                                            });
                                            terminal::draw(
                                                ui, s, &theme, mini_font, true, true, false,
                                            );
                                            });
                                        });
                                }
                            });
                        }
                    });
            });

        if let Some(text) = broadcast {
            self.agents.broadcast(&text);
            self.toast(format!("📣 {} セッションへ送信しました", self.agents.running_count()), true);
        }
        if voice_stop {
            self.stop_voice();
        }
        if let Some(id) = voice {
            self.apply_cmd(Cmd::VoiceInput(voice::Target::Session(id)), ctx);
        }
        if voice_all {
            self.apply_cmd(Cmd::VoiceInput(voice::Target::Broadcast), ctx);
        }
        if cycle_all {
            self.apply_cmd(Cmd::CyclePermissionAll, ctx);
        }
        if let Some(i) = cycle {
            match self.agents.cycle_permission(i) {
                Some(hint) => self.toast_warn(format!(
                    "🛡 権限モード切替を送信しました（{hint} / 画面を確認してください）"
                )),
                None => self.toast("このセッションは権限モード切替に未対応です", false),
            }
        }
        if let Some(i) = launch {
            self.launch_preset(i, ctx);
        }
        if let Some(i) = focus {
            self.apply_cmd(Cmd::FocusAgent(i), ctx);
        }
        if let Some(i) = restart {
            if let Err(e) = self.agents.restart(i, ctx) {
                self.toast(e, false);
            }
        }
        if let Some(i) = remove {
            self.agents.remove(i);
        }
    }

    // ─── UI: editor ─────────────────────────────────────────────────

    fn editor_area(&mut self, ui: &mut egui::Ui) {
        let theme = self.theme.clone();

        if !self.editor.buffers.is_empty() {
            let mut close_req: Option<usize> = None;
            let mut activate: Option<usize> = None;
            egui::Frame::none()
                .fill(theme.panel_alt)
                .inner_margin(egui::Margin {
                    left: 6.0,
                    right: 6.0,
                    top: 6.0,
                    bottom: 0.0,
                })
                .show(ui, |ui| {
                    egui::ScrollArea::horizontal()
                        .id_salt("editor-tabs")
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                for (i, b) in self.editor.buffers.iter().enumerate() {
                                    let active = Some(i) == self.editor.active;
                                    let fill = if active {
                                        theme.bg
                                    } else {
                                        Color32::TRANSPARENT
                                    };
                                    egui::Frame::none()
                                        .fill(fill)
                                        .rounding(egui::Rounding {
                                            nw: 7.0,
                                            ne: 7.0,
                                            sw: 0.0,
                                            se: 0.0,
                                        })
                                        .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                                        .show(ui, |ui| {
                                            ui.spacing_mut().item_spacing.x = 6.0;
                                            let icon = file_tree::icon_for(&b.title);
                                            let name = if b.dirty() {
                                                format!("{icon} {} ●", b.title)
                                            } else {
                                                format!("{icon} {}", b.title)
                                            };
                                            let color = if active {
                                                theme.text
                                            } else {
                                                theme.text_dim
                                            };
                                            let r = ui.add(
                                                egui::Label::new(
                                                    RichText::new(name).color(color),
                                                )
                                                .sense(egui::Sense::click()),
                                            );
                                            if r.clicked() {
                                                activate = Some(i);
                                            }
                                            let x = ui.add(
                                                egui::Label::new(
                                                    RichText::new("×").color(theme.text_dim),
                                                )
                                                .sense(egui::Sense::click()),
                                            );
                                            if x.clicked() {
                                                close_req = Some(i);
                                            }
                                        });
                                }
                            });
                        });
                });
            if let Some(i) = activate {
                self.editor.active = Some(i);
                self.find.last = None;
            }
            if let Some(i) = close_req {
                self.request_close(i);
            }
        }

        if self.find.open && self.editor.active.is_some() {
            self.find_bar(ui);
        }

        if self.editor.active.is_none() {
            self.welcome_ui(ui);
            return;
        }
        // Markdown / HTML ファイルは 編集/プレビュー の切替バーを出す
        // (Cockpit ビュー中は editor_area 自体が描画されないため自動的に除外)
        let (is_md, is_html) = self
            .editor
            .active
            .map(|i| {
                let b = &self.editor.buffers[i];
                (
                    markdown::is_markdown(&b.title, &b.lang),
                    html::is_html(&b.title, &b.lang),
                )
            })
            .unwrap_or((false, false));
        if is_md || is_html {
            self.md_toggle_bar(ui, is_html);
            if self.md_preview {
                self.markdown_preview_ui(ui, is_html);
                return;
            }
        }
        self.code_editor_ui(ui);
    }

    /// Markdown / HTML 用の 編集/プレビュー 切替バー。
    fn md_toggle_bar(&mut self, ui: &mut egui::Ui, is_html: bool) {
        let theme = self.theme.clone();
        let path = self
            .editor
            .active
            .and_then(|i| self.editor.buffers[i].path.clone());
        egui::Frame::none()
            .fill(theme.panel_alt)
            .inner_margin(egui::Margin::symmetric(10.0, 3.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let label = if is_html { "🌐 HTML" } else { "Ⓜ Markdown" };
                    ui.label(RichText::new(label).size(11.5).color(theme.text_dim));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let p = ui.selectable_label(
                            self.md_preview,
                            RichText::new("👁 プレビュー").size(12.0),
                        );
                        if p.on_hover_text("レンダリング表示 (⌘⇧V)").clicked() {
                            self.md_preview = true;
                        }
                        let e = ui.selectable_label(
                            !self.md_preview,
                            RichText::new("✏ 編集").size(12.0),
                        );
                        if e.on_hover_text("ソースを編集 (⌘⇧V)").clicked() {
                            self.md_preview = false;
                        }
                        // HTML はブラウザで開けば完全な見た目で確認できる
                        if is_html {
                            let b = ui.add_enabled(
                                path.is_some(),
                                egui::Button::new(
                                    RichText::new("🌐 ブラウザで開く").size(12.0),
                                ),
                            );
                            if b.on_hover_text(
                                "既定ブラウザで完全表示 (ディスクに保存済みの内容)",
                            )
                            .clicked()
                            {
                                if let Some(p) = &path {
                                    open_external(&p.display().to_string());
                                }
                            }
                        }
                    });
                });
            });
    }

    /// Markdown / HTML のレンダリングプレビュー画面。
    /// HTML は Markdown へ変換してから同じレンダラで描く。
    fn markdown_preview_ui(&mut self, ui: &mut egui::Ui, is_html: bool) {
        let Some(active) = self.editor.active else {
            return;
        };
        let id = self.editor.buffers[active].id;
        // 変換 (HTML→MD / 埋め込みHTML展開) は重いので内容が変わったときだけ行う
        let h = hash_str(&self.editor.buffers[active].text);
        let cached = self
            .md_pre_cache
            .as_ref()
            .is_some_and(|(cid, ch, _)| *cid == id && *ch == h);
        if !cached {
            let raw = &self.editor.buffers[active].text;
            let processed = if is_html {
                html::html_to_md(raw)
            } else {
                html::preprocess_markdown(raw)
            };
            self.md_pre_cache = Some((id, h, processed));
        }
        let text = match &self.md_pre_cache {
            Some((_, _, t)) => t.clone(),
            None => return,
        };
        let dir = self.editor.buffers[active]
            .path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let theme = self.theme.clone();
        let base = self.cfg.editor_font_size;
        let hl = &self.highlighter;
        let images = &mut self.md_images;
        egui::ScrollArea::vertical()
            .id_salt(("md-preview", id))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // 読みやすい紙面幅に絞って中央寄せする
                let max = 860.0f32.min(ui.available_width());
                let pad = ((ui.available_width() - max) * 0.5).max(0.0);
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    ui.vertical(|ui| {
                        ui.set_max_width(max);
                        egui::Frame::none()
                            .inner_margin(egui::Margin::symmetric(18.0, 14.0))
                            .show(ui, |ui| {
                                let mut rctx = markdown::RenderCtx {
                                    dir: dir.as_deref(),
                                    images,
                                };
                                markdown::render(ui, &theme, hl, base, &text, &mut rctx);
                            });
                    });
                });
            });
    }

    fn find_bar(&mut self, ui: &mut egui::Ui) {
        let theme = self.theme.clone();
        let mut do_find = false;
        let mut close = false;

        egui::Frame::none()
            .fill(theme.panel_alt)
            .inner_margin(egui::Margin::symmetric(8.0, 5.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("🔍");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.find.query)
                            .desired_width(260.0)
                            .hint_text("ファイル内検索…"),
                    );
                    if self.find.focus {
                        resp.request_focus();
                        self.find.focus = false;
                    }
                    if resp.changed() {
                        self.find.last = None;
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        do_find = true;
                    }
                    if ui.button("次へ ↓").clicked() {
                        do_find = true;
                    }
                    if let Some(i) = self.editor.active {
                        if !self.find.query.is_empty() {
                            let count = self.editor.buffers[i]
                                .text
                                .to_lowercase()
                                .matches(&self.find.query.to_lowercase())
                                .count();
                            ui.label(
                                RichText::new(format!("{count} 件")).color(theme.text_dim),
                            );
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("✕").clicked() {
                            close = true;
                        }
                    });
                });
            });

        if do_find {
            self.find_next();
        }
        if close {
            self.find.open = false;
        }
    }

    fn welcome_ui(&mut self, ui: &mut egui::Ui) {
        let theme = self.theme.clone();
        let mut launch_claude = false;
        let mut open_folder = false;

        let key = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+" };

        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.16);
            ui.label(RichText::new("⚡").size(64.0).color(theme.accent));
            ui.label(
                RichText::new("ZAIVERN CODE")
                    .size(30.0)
                    .strong()
                    .color(theme.text),
            );
            ui.label(
                RichText::new("Rust製 AI-Native エディタ — Zed の速度 × Cmux の並列エージェント × AGI Cockpit の操縦席")
                    .color(theme.text_dim),
            );
            ui.add_space(22.0);

            if ui
                .add_sized([300.0, 36.0], egui::Button::new("📂 フォルダを開く"))
                .clicked()
            {
                open_folder = true;
            }
            if ui
                .add_sized([300.0, 36.0], egui::Button::new("🤖 Claude Code を起動"))
                .clicked()
            {
                launch_claude = true;
            }
            if ui
                .add_sized([300.0, 36.0], egui::Button::new("🎛 Agent Cockpit"))
                .clicked()
            {
                self.cockpit = true;
            }

            ui.add_space(26.0);
            let hint = |s: &str, k: String| -> RichText {
                RichText::new(format!("{k}  —  {s}")).size(12.5).color(theme.text_dim)
            };
            ui.label(hint("ファイル検索", format!("{key}P")));
            ui.label(hint("コマンドパレット", format!("{key}⇧P")));
            ui.label(hint("ターミナル / エージェントパネル", format!("{key}J")));
            ui.label(hint("エージェント起動", format!("{key}⇧A")));
            ui.label(hint("Cockpit ビュー", format!("{key}⇧C")));
        });

        let ctx = ui.ctx().clone();
        if open_folder {
            self.apply_cmd(Cmd::OpenFolder, &ctx);
        }
        if launch_claude {
            let idx = self
                .cfg
                .agents
                .iter()
                .position(|p| p.command.contains("claude"))
                .unwrap_or(0);
            self.launch_preset(idx, &ctx);
        }
    }

    fn code_editor_ui(&mut self, ui: &mut egui::Ui) {
        let Some(active) = self.editor.active else {
            return;
        };
        let theme_text = self.theme.text;
        let theme_dim = self.theme.text_dim;
        let syntect_theme = self.theme.syntect_theme.clone();
        let font = FontId::monospace(self.cfg.editor_font_size);
        let row_h = ui.fonts(|f| f.row_height(&font));
        self.last_row_h = row_h;
        // galley をフレーム跨ぎでキャッシュするためのフォント世代キー。
        // egui は pixels_per_point 変更時とフォントアトラス逼迫時(fill_ratio > 0.8)に
        // FontsImpl ごと作り直し、そのとき全グリフの UV が変わる。古い galley を
        // 使い回すと描画が壊れるため、作り直しを検知できる値をキーに混ぜておく。
        // (アトラスは作り直しで初期サイズに戻るのでサイズ変化で検知できる)
        let font_gen = {
            let sz = ui.fonts(|f| f.font_image_size());
            (sz[0] as u64).rotate_left(23)
                ^ (sz[1] as u64).rotate_left(47)
                ^ (ui.ctx().pixels_per_point().to_bits() as u64).rotate_left(41)
        };
        let view_h = self.last_view_h;
        let theme_bg = self.theme.bg;

        let mut pending_select = self.pending_select.take();
        let pending_scroll = self.pending_scroll.take();

        // Git 行マーク(バッファの可変借用前に取得)
        let theme_ok = self.theme.ok;
        let theme_warn = self.theme.warn;
        let theme_err = self.theme.err;
        self.gitinfo.refresh_if_stale();
        let rel = self.editor.buffers[active]
            .path
            .as_ref()
            .and_then(|p| p.strip_prefix(&self.workspace).ok())
            .map(|p| p.to_string_lossy().to_string());
        let text_hash = hash_str(&self.editor.buffers[active].text);
        let marks: Vec<(usize, git::LineMark)> = match rel {
            Some(r) => self.gitinfo.line_marks(&r, text_hash),
            None => Vec::new(),
        };

        // LSP: この言語のサーバーを必要なら起動し did_open、診断を取得
        let path_clone = self.editor.buffers[active].path.clone();
        let lang_clone = self.editor.buffers[active].lang.clone();
        if let Some(p) = path_clone.clone() {
            let ctx = ui.ctx().clone();
            self.ensure_lsp(&ctx, &p, &lang_clone, active);
        }
        let (diag_by_line, derr, dwarn) = self.active_diagnostics();
        self.diag_counts = (derr, dwarn);

        // スニペット Tab 展開: エディタにフォーカスがあり、選択が空で、
        // カーソル直前の単語が prefix に一致するときだけ Tab を横取りする
        // (一致しなければ Tab はそのまま TextEdit のタブ挿入に流す)。
        let ed_id_early = egui::Id::new(("zaivern-buffer", self.editor.buffers[active].id));
        let has_focus = ui.memory(|m| m.has_focus(ed_id_early));
        let expand = if has_focus {
            let lang_id = snippets::lang_id_for(&lang_clone);
            match self.snippets_by_lang.get(lang_id) {
                Some(snips) if !snips.is_empty() => {
                    let cursor = egui::TextEdit::load_state(ui.ctx(), ed_id_early)
                        .and_then(|st| st.cursor.char_range())
                        .filter(|r| r.primary.index == r.secondary.index)
                        .map(|r| r.primary.index);
                    match cursor {
                        Some(cursor_char) => {
                            let filename = path_clone
                                .as_ref()
                                .and_then(|p| p.file_name())
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default();
                            let text = self.editor.buffers[active].text.clone();
                            snippets::try_expand_at(&text, cursor_char, snips, &filename)
                        }
                        None => None,
                    }
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some((nt, ncur)) = expand {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)) {
                self.editor.buffers[active].text = nt;
                self.editor.buffers[active].cache = None;
                pending_select = Some((ncur, ncur));
            }
        }

        let hl = &self.highlighter;
        let buf = &mut self.editor.buffers[active];
        let Buffer {
            id,
            text,
            lang,
            cache,
            gutter,
            ..
        } = buf;

        // 行番号ガター: git マークで行ごとに色分けした LayoutJob をキャッシュ
        let line_count = text.split('\n').count();
        let mut marks_hash: u64 = marks.len() as u64;
        for (l, m) in &marks {
            marks_hash = marks_hash
                .wrapping_mul(31)
                .wrapping_add(((*l as u64) << 1) | matches!(m, git::LineMark::Added) as u64);
        }
        let mut diag_hash: u64 = diag_by_line.len() as u64;
        for (l, sev) in &diag_by_line {
            diag_hash = diag_hash
                .wrapping_mul(37)
                .wrapping_add((*l as u64) << 3 | *sev as u64);
        }
        // galley までキャッシュするので、キーには LayoutJob の内容(行数/マーク/診断/
        // フォントサイズ/テーマ)に加えてラスタライズ側の font_gen も含める。
        // font family は常に Monospace 固定なので font.size のみで足りる。
        let gutter_key = (line_count as u64)
            ^ marks_hash.rotate_left(17)
            ^ diag_hash.rotate_left(29)
            ^ (font.size.to_bits() as u64)
            ^ font_gen
            ^ hash_str(&syntect_theme).rotate_left(3);
        if gutter.as_ref().map(|(k, _)| *k) != Some(gutter_key) {
            let width = line_count.to_string().len().max(3);
            let mark_map: HashMap<usize, git::LineMark> = marks.iter().cloned().collect();
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = f32::INFINITY;
            for n in 0..line_count {
                // 診断色(エラー/警告)を git マークより優先する
                let color = match diag_by_line.get(&n) {
                    Some(1) => theme_err,
                    Some(2) => theme_warn,
                    _ => match mark_map.get(&n) {
                        Some(git::LineMark::Added) => theme_ok,
                        Some(git::LineMark::Modified) => theme_warn,
                        None => theme_dim,
                    },
                };
                let s = if n + 1 < line_count {
                    format!("{:>width$}\n", n + 1)
                } else {
                    format!("{:>width$}", n + 1)
                };
                job.append(
                    &s,
                    0.0,
                    egui::TextFormat {
                        font_id: font.clone(),
                        color,
                        ..Default::default()
                    },
                );
            }
            *gutter = Some((gutter_key, ui.fonts(|f| f.layout_job(job))));
        }

        let ed_id = egui::Id::new(("zaivern-buffer", *id));
        // wrap 幅(_wrap)を無視してよい理由: highlight::layout_job は常に
        // wrap.max_width = f32::INFINITY を設定する(横スクロールのため折り返さない)。
        // よって galley は wrap 幅に依存せず、フレーム跨ぎで使い回せる。
        let mut layouter = |ui: &egui::Ui, t: &str, _wrap: f32| {
            let key = hash_str(t)
                ^ hash_str(lang.as_str())
                ^ hash_str(&syntect_theme)
                ^ (font.size.to_bits() as u64)
                ^ font_gen;
            match cache {
                // ヒット時は Arc の参照カウント増加のみ。
                // LayoutJob のコピーも egui 側の job ハッシュ計算も起きない。
                Some((k, g)) if *k == key => g.clone(),
                _ => {
                    let j = hl.layout_job(t, lang, &syntect_theme, font.clone(), theme_text);
                    let g = ui.fonts(|f| f.layout_job(j));
                    *cache = Some((key, g.clone()));
                    g
                }
            }
        };

        // ガター(行番号)は VS Code 同様、水平スクロールでは動かない固定表示。
        // 本文の上に後描きするため、幅と galley を先に確定しておく。
        let gutter_galley = match gutter.as_ref() {
            // Arc の参照カウント増加だけ。LayoutJob のコピーも再レイアウトも起きない。
            Some((_, g)) => g.clone(),
            None => ui.fonts(|f| f.layout_job(Default::default())),
        };
        let gutter_w = gutter_galley.size().x + 22.0;

        let mut sa = egui::ScrollArea::both()
            .id_salt(("editor-scroll", *id))
            .auto_shrink(false);
        if let Some(y) = pending_scroll {
            sa = sa.vertical_scroll_offset(y);
        }

        // VS Code の scrollBeyondLastLine: 最終行を越えてスクロールできる余白
        let past_end = (view_h - row_h * 3.0).max(0.0);

        let inner = sa.show(ui, |ui| {
            if let Some((s, e)) = pending_select {
                let mut st =
                    egui::TextEdit::load_state(ui.ctx(), ed_id).unwrap_or_default();
                st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(s),
                    egui::text::CCursor::new(e),
                )));
                st.store(ui.ctx(), ed_id);
                ui.ctx().memory_mut(|m| m.request_focus(ed_id));
            }

            let mut cursor_out: Option<(usize, usize)> = None;
            let mut changed_flag = false;
            let mut text_top: Option<f32> = None;
            ui.horizontal_top(|ui| {
                // ガターぶんの余白だけ空けて本文を置く
                // (ガター自体はスクロール確定後に上から固定描画する)
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add_space(gutter_w);

                let output = egui::TextEdit::multiline(text)
                    .id(ed_id)
                    .font(font.clone())
                    .code_editor()
                    .frame(false)
                    .desired_width(f32::INFINITY)
                    .margin(egui::Margin::ZERO)
                    .layouter(&mut layouter)
                    .show(ui);
                changed_flag = output.response.changed();
                // 本文が実際に描かれた y 原点。ScrollArea はホイールの
                // オフセットを配置後に適用するため、state.offset ではなく
                // これを使わないとガターが 1 フレームずれて「泳ぐ」
                text_top = Some(output.response.rect.top());

                if let Some(cr) = output.cursor_range {
                    let idx = cr.primary.ccursor.index;
                    let mut line = 1usize;
                    let mut col = 1usize;
                    for ch in text.chars().take(idx) {
                        if ch == '\n' {
                            line += 1;
                            col = 1;
                        } else {
                            col += 1;
                        }
                    }
                    cursor_out = Some((line, col));
                }

                // Enter 直後の自動インデント
                if output.response.changed()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                {
                    if let Some(cr) = output.cursor_range {
                        let cursor = cr.primary.ccursor.index;
                        if let Some((new_text, new_cursor)) =
                            editor_ops::auto_indent_after_newline(text, cursor)
                        {
                            // cache はキーが text ハッシュなので書き換えだけで無効化される
                            *text = new_text;
                            let mut st = egui::TextEdit::load_state(ui.ctx(), ed_id)
                                .unwrap_or_default();
                            st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                                egui::text::CCursor::new(new_cursor),
                            )));
                            st.store(ui.ctx(), ed_id);
                        }
                    }
                }
            });
            // 最終行より先までスクロールできる余白 (VS Code の scrollBeyondLastLine)
            if past_end > 0.0 {
                ui.add_space(past_end);
            }
            (cursor_out, changed_flag, text_top)
        });

        let (cursor_out, changed, text_top) = inner.inner;

        // ガターを固定描画: 垂直スクロールには追従し、水平スクロールでは動かない
        let vis = inner.inner_rect;
        self.last_view_h = vis.height();
        self.last_scroll_y = inner.state.offset.y;
        let painter = ui.painter_at(vis);
        painter.rect_filled(
            egui::Rect::from_min_max(
                vis.min,
                egui::pos2(vis.left() + gutter_w - 10.0, vis.bottom()),
            ),
            0.0,
            theme_bg,
        );
        painter.galley(
            egui::pos2(
                vis.left() + 6.0,
                text_top.unwrap_or(vis.top() - inner.state.offset.y),
            ),
            gutter_galley,
            theme_dim,
        );

        if let Some(c) = cursor_out {
            self.editor.cursor = c;
        }

        // LSP: テキストが変わったらデバウンスして did_change を予約
        if changed {
            if let (Some(p), lang) = (path_clone.clone(), lang_clone.clone()) {
                let lang_id = snippets::lang_id_for(&lang).to_string();
                if self.lsp.contains_key(&lang_id) {
                    let text = self.editor.buffers[active].text.clone();
                    self.lsp_pending
                        .insert(p, (text, Instant::now(), lang_id));
                }
            }
        }
    }

    // ─── UI: palette ────────────────────────────────────────────────

    fn palette_items(&self) -> Vec<Item> {
        let q = self.palette.query().to_string();
        let mut items: Vec<Item> = Vec::new();

        if self.palette.is_command_mode() {
            let mut cmds: Vec<(String, String, String, Cmd)> = vec![
                ("💾".into(), "保存".into(), "⌘S".into(), Cmd::Save),
                ("💾".into(), "名前を付けて保存".into(), "⌘⇧S".into(), Cmd::SaveAs),
                ("📄".into(), "新規ファイル".into(), "⌘N".into(), Cmd::NewFile),
                ("📂".into(), "フォルダを開く…".into(), String::new(), Cmd::OpenFolder),
                ("❌".into(), "タブを閉じる".into(), "⌘W".into(), Cmd::CloseTab),
                ("🔍".into(), "ファイル内検索".into(), "⌘F".into(), Cmd::OpenFind),
                ("🖥".into(), "ターミナル表示切替".into(), "⌘J".into(), Cmd::ToggleTerminal),
                ("🎛".into(), "Cockpit 切替".into(), "⌘⇧C".into(), Cmd::ToggleCockpit),
                ("👁".into(), "Markdown/HTML プレビュー切替".into(), "⌘⇧V".into(), Cmd::ToggleMdPreview),
                ("📁".into(), "サイドバー切替".into(), "⌘B".into(), Cmd::ToggleSidebar),
                ("🌿".into(), "Git パネルを開く".into(), String::new(), Cmd::OpenGitPanel),
                (
                    "🤖".into(),
                    "現在のファイルをエージェントに送信 (@path)".into(),
                    String::new(),
                    Cmd::SendFileToAgent,
                ),
                ("⟳".into(), "アクティブなエージェントを再起動".into(), String::new(), Cmd::RestartAgent),
                ("🗑".into(), "アクティブなエージェントを終了".into(), String::new(), Cmd::KillAgent),
                ("⚙".into(), "設定 config.toml を開く".into(), String::new(), Cmd::OpenConfig),
                ("🔄".into(), "設定を再読み込み".into(), String::new(), Cmd::ReloadConfig),
                ("🔠".into(), "フォント拡大".into(), "⌘+".into(), Cmd::FontInc),
                ("🔠".into(), "フォント縮小".into(), "⌘-".into(), Cmd::FontDec),
                ("🌲".into(), "ファイルツリー再読み込み".into(), String::new(), Cmd::RefreshTree),
                (
                    "🛡".into(),
                    "承認モード: 毎回ユーザー承認 (Claude/Codex/Antigravity)".into(),
                    String::new(),
                    Cmd::SetApproval("ask".into()),
                ),
                (
                    "⚡".into(),
                    "承認モード: 全自動 YES (Claude/Codex/Antigravity)".into(),
                    String::new(),
                    Cmd::SetApproval("auto".into()),
                ),
                (
                    "🤖".into(),
                    "承認モード: Agent欄優先 (プリセットのコマンドどおり)".into(),
                    String::new(),
                    Cmd::SetApproval("agent".into()),
                ),
                ("🐾".into(), "ペット表示切替".into(), String::new(), Cmd::TogglePet),
                (
                    "📱".into(),
                    "スマホリモート (QR コード表示)".into(),
                    String::new(),
                    Cmd::ToggleRemote,
                ),
                (
                    "🎤".into(),
                    "音声入力: 全エージェントの入力欄へ (送信は自分で Enter)".into(),
                    String::new(),
                    Cmd::VoiceInput(voice::Target::Broadcast),
                ),
                (
                    "🛡".into(),
                    "実行中の全エージェントの権限モードを切替".into(),
                    String::new(),
                    Cmd::CyclePermissionAll,
                ),
                ("🖼".into(), "ペット画像を変更…".into(), String::new(), Cmd::SetPetImage),
                ("↺".into(), "ペット画像を既定に戻す".into(), String::new(), Cmd::ResetPetImage),
                ("🦀".into(), "ペット位置を右下に戻す".into(), String::new(), Cmd::ResetPetPos),
                ("➕".into(), "新規プラグインを作成…".into(), String::new(), Cmd::NewPlugin),
                ("📦".into(), "プラグインをインストール… (.zvplug / .zip)".into(), String::new(), Cmd::InstallPlugin),
                ("🔌".into(), "プラグインを表示".into(), String::new(), Cmd::ShowPlugins),
                ("⟳".into(), "プラグインを再スキャン".into(), String::new(), Cmd::RescanPlugins),
            ];
            // 実行中のセッション毎に音声入力エントリを出す (パレットで「音声」検索用)
            for s in self.agents.sessions.iter().take(20) {
                cmds.push((
                    "🎤".into(),
                    format!("音声入力: {} {} の入力欄へ (送信は自分で Enter)", s.icon, s.title),
                    String::new(),
                    Cmd::VoiceInput(voice::Target::Session(s.id)),
                ));
            }
            for t in theme::all() {
                cmds.push((
                    "🎨".into(),
                    format!("テーマ: {}", t.label),
                    String::new(),
                    Cmd::SetTheme(t.name.clone()),
                ));
            }
            for (label, path) in self.custom_themes.iter().take(80) {
                cmds.push((
                    "🔌".into(),
                    format!("テーマ (カスタム): {label}"),
                    String::new(),
                    Cmd::SetTheme(path.clone()),
                ));
            }
            for (pi, p) in self.plugins.iter().enumerate() {
                for (ci, c) in p.commands.iter().enumerate() {
                    cmds.push((
                        c.icon.clone(),
                        format!("{}: {}", p.name, c.title),
                        c.keybind.clone().unwrap_or_default(),
                        Cmd::RunPlugin(pi, ci),
                    ));
                }
            }
            for (i, p) in self.cfg.agents.iter().enumerate() {
                cmds.push((
                    p.icon.clone(),
                    format!("エージェント起動: {}", p.name),
                    String::new(),
                    Cmd::NewAgent(i),
                ));
            }
            for (i, s) in self.agents.sessions.iter().enumerate() {
                cmds.push((
                    s.icon.clone(),
                    format!("エージェントへ移動: {}", s.title),
                    String::new(),
                    Cmd::FocusAgent(i),
                ));
            }
            for (icon, label, detail, cmd) in cmds {
                if let Some(score) = fuzzy::score(&q, &label) {
                    items.push(Item {
                        icon,
                        label,
                        detail,
                        action: Action::Cmd(cmd),
                        score,
                    });
                }
            }
        } else {
            for rel in &self.file_index {
                if let Some(score) = fuzzy::score(&q, rel) {
                    let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
                    items.push(Item {
                        icon: file_tree::icon_for(&name).to_string(),
                        label: name,
                        detail: rel.clone(),
                        action: Action::OpenFile(PathBuf::from(rel)),
                        score,
                    });
                }
            }
        }

        items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
        items.truncate(100);
        items
    }

    fn palette_ui(&mut self, ctx: &egui::Context) {
        if !self.palette.open {
            return;
        }
        let theme = self.theme.clone();
        let items = self.palette_items();
        let mut execute: Option<Action> = None;
        let mut close = false;

        egui::Area::new(egui::Id::new("zv-palette"))
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_TOP, egui::vec2(0.0, 100.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(theme.panel)
                    .stroke(egui::Stroke::new(1.0_f32, theme.accent.gamma_multiply(0.55)))
                    .rounding(egui::Rounding::same(10.0))
                    .inner_margin(egui::Margin::same(10.0))
                    .shadow(egui::epaint::Shadow {
                        offset: egui::vec2(0.0, 8.0),
                        blur: 24.0,
                        spread: 0.0,
                        color: Color32::from_black_alpha(140),
                    })
                    .show(ui, |ui| {
                        ui.set_width(640.0);

                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.palette.input)
                                .hint_text("ファイル検索…  （先頭に > でコマンド）")
                                .font(FontId::proportional(16.0))
                                .desired_width(f32::INFINITY),
                        );
                        if self.palette.just_opened {
                            resp.request_focus();
                            self.palette.just_opened = false;
                        }
                        if resp.changed() {
                            self.palette.selected = 0;
                        }

                        let (down, up, enter, escape) = ctx.input(|i| {
                            (
                                i.key_pressed(egui::Key::ArrowDown),
                                i.key_pressed(egui::Key::ArrowUp),
                                i.key_pressed(egui::Key::Enter),
                                i.key_pressed(egui::Key::Escape),
                            )
                        });
                        if escape {
                            close = true;
                        }
                        let len = items.len();
                        if len > 0 {
                            if down {
                                self.palette.selected = (self.palette.selected + 1) % len;
                            }
                            if up {
                                self.palette.selected =
                                    (self.palette.selected + len - 1) % len;
                            }
                            self.palette.selected = self.palette.selected.min(len - 1);
                        }
                        if enter && !close {
                            if let Some(it) = items.get(self.palette.selected) {
                                execute = Some(it.action.clone());
                            }
                            close = true;
                        }
                        if !close && !resp.has_focus() {
                            resp.request_focus();
                        }

                        ui.add_space(6.0);
                        egui::ScrollArea::vertical()
                            .id_salt("palette-list")
                            .max_height(420.0)
                            .show(ui, |ui| {
                                for (i, it) in items.iter().enumerate() {
                                    let selected = i == self.palette.selected;
                                    let fill = if selected {
                                        theme.accent_soft
                                    } else {
                                        Color32::TRANSPARENT
                                    };
                                    let fr = egui::Frame::none()
                                        .fill(fill)
                                        .rounding(egui::Rounding::same(6.0))
                                        .inner_margin(egui::Margin::symmetric(8.0, 5.0))
                                        .show(ui, |ui| {
                                            ui.set_width(ui.available_width());
                                            ui.horizontal(|ui| {
                                                ui.label(&it.icon);
                                                ui.label(
                                                    RichText::new(&it.label)
                                                        .color(theme.text),
                                                );
                                                if !it.detail.is_empty() {
                                                    ui.label(
                                                        RichText::new(&it.detail)
                                                            .size(11.5)
                                                            .color(theme.text_dim),
                                                    );
                                                }
                                            });
                                        });
                                    let r = ui.interact(
                                        fr.response.rect,
                                        egui::Id::new(("pal-item", i)),
                                        egui::Sense::click(),
                                    );
                                    if r.clicked() {
                                        execute = Some(it.action.clone());
                                        close = true;
                                    }
                                    if selected && (down || up) {
                                        r.scroll_to_me(None);
                                    }
                                }
                                if items.is_empty() {
                                    ui.label(
                                        RichText::new("該当なし").color(theme.text_dim),
                                    );
                                }
                            });
                    });
            });

        if close {
            self.palette.close();
        }
        if let Some(a) = execute {
            self.run_action(a, ctx);
        }
    }

    // ─── UI: modals & toasts ────────────────────────────────────────

    fn close_confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(i) = self.pending_close else {
            return;
        };
        if i >= self.editor.buffers.len() {
            self.pending_close = None;
            return;
        }
        let title = self.editor.buffers[i].title.clone();
        let mut decided: Option<u8> = None;

        egui::Window::new("未保存の変更")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("「{title}」には未保存の変更があります。"));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("💾 保存して閉じる").clicked() {
                        decided = Some(0);
                    }
                    if ui.button("🗑 保存せずに閉じる").clicked() {
                        decided = Some(1);
                    }
                    if ui.button("キャンセル").clicked() {
                        decided = Some(2);
                    }
                });
            });

        match decided {
            Some(0) => {
                self.editor.active = Some(i);
                if self.save_active(false) {
                    self.editor.close(i);
                }
                self.pending_close = None;
                self.persist_session();
            }
            Some(1) => {
                self.editor.close(i);
                self.pending_close = None;
                self.persist_session();
            }
            Some(2) => self.pending_close = None,
            _ => {}
        }
    }

    /// リネーム/移動後、開いているバッファのパス・タイトル・言語を追従させる。
    /// `from` がフォルダの場合は配下のバッファも新パスへ付け替える。
    fn retarget_buffers(&mut self, from: &Path, to: &Path) {
        for b in &mut self.editor.buffers {
            let Some(p) = b.path.clone() else { continue };
            let new_path = if p == from {
                to.to_path_buf()
            } else if let Ok(rest) = p.strip_prefix(from) {
                to.join(rest)
            } else {
                continue;
            };
            b.title = new_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "???".into());
            b.lang = self.highlighter.lang_for(Some(&new_path), &b.text);
            b.path = Some(new_path);
            b.cache = None;
            b.gutter = None;
        }
    }

    /// ファイルツリーからの削除の確認モーダル。
    fn delete_confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(path) = self.pending_delete.clone() else {
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let is_dir = path.is_dir();
        let warn = self.theme.warn;
        let mut decided: Option<bool> = None;

        egui::Window::new("削除の確認")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let what = if is_dir { "フォルダ(中身ごと)" } else { "ファイル" };
                ui.label(format!("{what}「{name}」を削除しますか？"));
                ui.label(
                    RichText::new("この操作は取り消せません")
                        .small()
                        .color(warn),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("🗑 削除").color(warn)).clicked() {
                        decided = Some(true);
                    }
                    if ui.button("キャンセル").clicked() {
                        decided = Some(false);
                    }
                });
            });

        match decided {
            Some(true) => {
                let res = if is_dir {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                match res {
                    Ok(()) => {
                        // 開いていたタブの後始末: 変更なしは閉じ、未保存の変更が
                        // あるものはパスを外して内容を保持する(⌘S で保存先を選び直せる)
                        let mut close: Vec<usize> = Vec::new();
                        for (i, b) in self.editor.buffers.iter_mut().enumerate() {
                            let Some(p) = b.path.as_ref() else { continue };
                            if p == &path || p.starts_with(&path) {
                                if b.dirty() {
                                    b.path = None;
                                } else {
                                    close.push(i);
                                }
                            }
                        }
                        for i in close.into_iter().rev() {
                            self.editor.close(i);
                        }
                        self.tree.invalidate();
                        self.persist_session();
                        self.toast(format!("🗑 {name} を削除しました"), true);
                    }
                    Err(e) => self.toast(format!("削除できません: {e}"), false),
                }
                self.pending_delete = None;
            }
            Some(false) => self.pending_delete = None,
            _ => {}
        }
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.at.elapsed().as_secs_f32() < 4.2);
        if self.toasts.is_empty() {
            return;
        }
        let theme = self.theme.clone();
        egui::Area::new(egui::Id::new("zv-toasts"))
            .order(egui::Order::Foreground)
            .anchor(Align2::RIGHT_BOTTOM, egui::vec2(-14.0, -76.0))
            .show(ctx, |ui| {
                for t in &self.toasts {
                    let color = match t.kind {
                        0 => theme.ok,
                        1 => theme.warn,
                        _ => theme.err,
                    };
                    egui::Frame::none()
                        .fill(theme.panel)
                        .stroke(egui::Stroke::new(1.0_f32, color.gamma_multiply(0.7)))
                        .rounding(egui::Rounding::same(8.0))
                        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                        .show(ui, |ui| {
                            ui.label(RichText::new(&t.msg).color(theme.text));
                        });
                }
            });
        ctx.request_repaint_after(std::time::Duration::from_millis(300));
    }

    // ─── スマホリモート ─────────────────────────────────────────────

    // ─── 音声入力 (Zaivern 内で完結) ────────────────────────────────

    /// 音声入力を開始する。⏹ を押すまで録音し続ける。
    fn start_voice(&mut self, target: voice::Target, ctx: &egui::Context) {
        if self.voice.session.is_some() {
            return;
        }
        if self.agents.running_count() == 0 {
            self.toast_warn("音声入力の宛先がありません — 先にエージェントを起動してください");
            return;
        }
        // ブラウザ経路は子プロセスを持たない — /voice をブラウザで開いて、
        // 認識結果は Web Speech API から /api/voice 経由で戻ってくる。
        if voice::resolve_engine(
            &self.cfg.voice_engine,
            &self.cfg.voice_lang,
            &self.cfg.voice_command,
        ) == "browser"
        {
            self.open_voice_page();
            return;
        }
        match voice::start(
            &self.cfg.voice_engine,
            &self.cfg.voice_lang,
            &self.cfg.voice_command,
            ctx,
        ) {
            Ok(s) => {
                self.voice = VoiceState {
                    session: Some(s),
                    target,
                    ..Default::default()
                };
                if self.cfg.pet_sounds {
                    self.sound.play(SoundKind::Confirm);
                }
            }
            Err(e) => {
                self.voice = VoiceState::default();
                self.toast(format!("🎤 {e}"), false);
            }
        }
    }

    /// ブラウザの音声入力ページ (`/voice`) を開く。
    ///
    /// `http://127.0.0.1:PORT` は W3C の Secure Contexts 上「信頼できるオリジン」
    /// なので、TLS 無しでも Web Speech API が動く。マイクはブラウザ側なので
    /// Zaivern 内に録音プロセスは立たない (⏹ も出ない — 閉じれば止まる)。
    fn open_voice_page(&mut self) {
        let Some(r) = self.remote.as_ref() else {
            self.toast(
                "🎤 ブラウザの音声入力ページを開けません — スマホリモートが起動していません\
                 (config.toml の voice_command に外部コマンドを設定する手もあります)"
                    .to_string(),
                false,
            );
            return;
        };
        let url = format!("http://127.0.0.1:{}/voice?t={}", r.port, r.token);
        // Edge の webkitSpeechRecognition は不安定なので Chrome があればそちらを使う。
        // どちらで開いたかは必ず伝える (黙って既定ブラウザに投げない)。
        let browser = match chrome_path() {
            Some(p) => {
                let _ = std::process::Command::new(p).arg(&url).spawn();
                "Chrome"
            }
            None => {
                open_external(&url);
                "既定のブラウザ"
            }
        };
        self.toast(
            format!(
                "🎤 {browser} で音声入力ページを開きました — これから先はそちらのマイクが 🎤 です\
                 (認識テキストは入力欄に入るだけ。送信は自分で Enter)"
            ),
            true,
        );
    }

    /// 録音を止める。認識プロセスは最後の確定テキストを返してから終了するので、
    /// ここでは kill せず `stopping_at` を立てて確定を待つ。
    fn stop_voice(&mut self) {
        if let Some(s) = self.voice.session.as_mut() {
            s.stop();
            if self.voice.stopping_at.is_none() {
                self.voice.stopping_at = Some(Instant::now());
            }
        }
    }

    /// 音声入力の主処理。毎フレーム呼ぶ。
    fn poll_voice(&mut self, ctx: &egui::Context) {
        let events = match self.voice.session.as_ref() {
            Some(s) => s.poll(),
            None => return,
        };
        let mut ended = false;
        for ev in events {
            match ev {
                voice::Event::Ready => {
                    self.voice.ready = true;
                }
                // 途中経過も確定も同じ経路で入力欄へ流す。違いは、確定した分は
                // もう書き換えないので追跡をやめる (= 次のひとことが後ろへ続く) 点だけ。
                voice::Event::Partial(t) => {
                    self.voice.partial = t.clone();
                    self.apply_voice_text(&t, false);
                }
                voice::Event::Final(t) => {
                    self.voice.partial.clear();
                    self.apply_voice_text(&t, true);
                }
                voice::Event::Error(e) => {
                    self.toast(format!("🎤 {e}"), false);
                    ended = true;
                }
                voice::Event::Ended => ended = true,
            }
        }

        // 停止要求から一定時間たっても終わらないプロセスは打ち切る
        let timed_out = self
            .voice
            .stopping_at
            .is_some_and(|at| at.elapsed() > Duration::from_secs(5));
        if ended || timed_out {
            if let Some(mut s) = self.voice.session.take() {
                s.kill();
            }
            self.voice = VoiceState::default();
        } else {
            // 録音中は HUD を動かし続ける
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }

    /// 認識テキストを対象セッションの入力欄へ流し込む。
    ///
    /// 確定を待たずに、話している途中 (`is_final == false`) の文字もそのまま
    /// 入力欄へ書き込む。喋りが進んで変換が変わると前に書いた文字列は書き換わるので、
    /// **前回書いた分と食い違うところだけ Backspace で消してから続きを送る**。
    /// これで入力欄が二重になったり、消し残しが出たりしない。
    ///
    /// **Enter は送らない**。ユーザーが内容を見て自分で Enter を押すまで
    /// エージェントへは送信されない。設定した合図キーワードを話したときだけ、
    /// キーワードを取り除いたうえで Enter まで送る。合図の判定は確定したときだけ
    /// 行う (途中経過で誤爆させない)。
    fn apply_voice_text(&mut self, text: &str, is_final: bool) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let kw = self.cfg.voice_keyword.trim().to_string();
        let (body, submit) = match is_final && !kw.is_empty() {
            false => (text.to_string(), false),
            true => match strip_trailing_keyword(text, &kw) {
                Some(rest) => (rest, true),
                None => (text.to_string(), false),
            },
        };
        let body = body.trim().to_string();
        if body.is_empty() && !submit {
            return;
        }

        // 宛先が変わったら、前の入力欄に書いた文字はそのまま残して書き出しからやり直す
        // (別のセッションへ Backspace を送り込んでしまわないように)。
        let dest = self.resolve_voice_target();
        let key = match dest {
            Some(id) => id,
            None => u64::MAX,
        };
        if self.voice.last_sent_to.is_some_and(|k| k != key) {
            self.voice.live.clear();
            self.voice.last_char = None;
        }

        // 録音中に人が手で打った (Enter で送った・自分で消した) なら、覚えている
        // 書き込み内容はもう当てにならない。Backspace を送り込まず書き出しから
        // やり直す — Enter で入力欄が空になったあとも、そのまま話し続けられる。
        let typed = match dest {
            Some(id) => self
                .agents
                .sessions
                .iter_mut()
                .find(|s| s.id == id)
                .is_some_and(|s| s.take_user_typed()),
            None => self
                .agents
                .sessions
                .iter_mut()
                .fold(false, |acc, s| s.take_user_typed() || acc),
        };
        if typed {
            self.voice.reset_live();
        }

        let edit = self.voice.plan(&body, key);
        // 同じ途中経過がもう一度届いただけなら端末へ何も送らない。
        // ただし確定と送信は、送るバイトが無くても追跡の締めが要るので通す。
        if edit.is_noop() && !submit && !is_final {
            return;
        }
        let out = edit.bytes(submit);

        let sent = match dest {
            Some(id) => match self.agents.sessions.iter_mut().find(|s| s.id == id) {
                Some(s) if s.running() => {
                    s.write_bytes(&out);
                    Some(s.title.clone())
                }
                _ => None,
            },
            None if self.voice.target == voice::Target::Broadcast => {
                let n = self.agents.running_count();
                if n == 0 {
                    None
                } else {
                    // ブロードキャストは Enter 込みの broadcast() を使わず、
                    // 書き込みのみ / 送信ありを自分で選ぶ
                    for s in self.agents.sessions.iter_mut().filter(|s| s.running()) {
                        s.write_bytes(&out);
                    }
                    Some(format!("{n} セッション"))
                }
            }
            None => None,
        };

        let Some(where_) = sent else {
            self.toast_warn("音声入力の宛先セッションが見つかりません");
            return;
        };
        self.voice.commit(edit, is_final, submit, key);
        if submit {
            self.toast(format!("🎤▶ {where_} へ送信: {body}"), true);
        }
    }

    /// いま文字を届けるべきセッション id。ブロードキャストなら None。
    /// `Active` は毎回引き直すので、録音中にタブを切り替えれば宛先も移る。
    fn resolve_voice_target(&self) -> Option<u64> {
        match self.voice.target {
            voice::Target::Broadcast => None,
            voice::Target::Active => {
                self.agents.sessions.get(self.agents.active).map(|s| s.id)
            }
            voice::Target::Session(id) => Some(id),
        }
    }

    /// 録音中に画面上部へ出すパネル。認識中の文字・届け先の切替・⏹ 停止を持つ。
    fn voice_hud(&mut self, ctx: &egui::Context) {
        if self.voice.session.is_none() {
            return;
        }
        let theme = self.theme.clone();
        let stopping = self.voice.stopping_at.is_some();
        let head = if stopping {
            "🎤 最後のひとことを待っています…".to_string()
        } else if self.voice.ready {
            let dots = (self.voice.partial.len() % 3) + 1;
            format!("🔴 録音中{}", "・".repeat(dots))
        } else {
            "🎤 マイクを準備しています…".to_string()
        };
        let target_label = self.voice_target_label();
        let mut stop = false;
        let mut set_target: Option<voice::Target> = None;

        egui::Area::new(egui::Id::new("zv-voice-hud"))
            .anchor(Align2::CENTER_TOP, egui::vec2(0.0, 56.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(theme.panel)
                    .stroke(egui::Stroke::new(
                        1.5_f32,
                        if stopping { theme.accent } else { theme.err },
                    ))
                    .rounding(egui::Rounding::same(10.0))
                    .inner_margin(egui::Margin::symmetric(16.0, 10.0))
                    .show(ui, |ui| {
                        ui.set_max_width(600.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(head).strong().color(theme.text));
                            ui.label(
                                RichText::new(format!("→ {target_label}")).color(theme.text_dim),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .button(RichText::new("⏹ 停止").strong())
                                        .on_hover_text("録音をやめます")
                                        .clicked()
                                    {
                                        stop = true;
                                    }
                                },
                            );
                        });
                        // 録音したまま届け先を切り替えられる
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("届け先:").size(11.0).color(theme.text_dim));
                            for (t, label) in [
                                (voice::Target::Active, "🎯 アクティブなエージェント"),
                                (voice::Target::Broadcast, "📣 全エージェント"),
                            ] {
                                let sel = self.voice.target == t;
                                if ui.selectable_label(sel, RichText::new(label).size(11.5)).clicked()
                                    && !sel
                                {
                                    set_target = Some(t);
                                }
                            }
                        });
                        if !self.voice.partial.is_empty() {
                            ui.label(RichText::new(&self.voice.partial).color(theme.accent));
                        }
                        ui.label(
                            RichText::new(
                                "話しながらリアルタイムで入力欄へ書き込まれます。送信は自分で Enter を押したときだけ。\n\
                                 Enter で空になっても録音は続いているので、そのまま話し続けられます",
                            )
                            .size(11.0)
                            .color(theme.text_dim),
                        );
                    });
            });

        if let Some(t) = set_target {
            self.voice.target = t;
            // 宛先が変わったら、前の入力欄の追跡を捨てて書き出しからやり直す
            self.voice.last_sent_to = None;
            self.voice.reset_live();
            if t != voice::Target::Session(0) {
                self.cfg.voice_target = t.name().to_string();
                config::save_state(&self.cfg);
            }
        }
        if stop {
            self.stop_voice();
        }
    }

    /// 届け先の表示名。
    fn voice_target_label(&self) -> String {
        match self.voice.target {
            voice::Target::Broadcast => {
                format!("📣 全エージェント ({})", self.agents.running_count())
            }
            voice::Target::Active | voice::Target::Session(_) => {
                match self.resolve_voice_target() {
                    Some(id) => self
                        .agents
                        .sessions
                        .iter()
                        .find(|s| s.id == id)
                        .map(|s| format!("{} {}", s.icon, s.title))
                        .unwrap_or_else(|| "(見つかりません)".into()),
                    None => "(エージェントがいません)".into(),
                }
            }
        }
    }

    /// リモートサーバに溜まったリクエストを処理して応答する。毎フレーム呼ぶ。
    fn poll_remote(&mut self, ctx: &egui::Context) {
        let reqs: Vec<remote::Request> = match &self.remote {
            Some(r) => r.poll(),
            None => return,
        };
        for req in reqs {
            let json = self.remote_reply(&req.query, ctx);
            req.respond(json);
        }
    }

    /// リモートからの問い合わせ 1 件に応答 JSON を返す。
    fn remote_reply(&mut self, q: &remote::Query, ctx: &egui::Context) -> String {
        use serde_json::json;
        match q {
            remote::Query::State => {
                let ws = self
                    .workspace
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let tabs: Vec<_> = self
                    .editor
                    .buffers
                    .iter()
                    .map(|b| json!({"title": b.title, "dirty": b.dirty()}))
                    .collect();
                let (file, dirty) = match self.editor.active {
                    Some(i) => (
                        self.editor.buffers[i].title.clone(),
                        self.editor.buffers[i].dirty(),
                    ),
                    None => (String::new(), false),
                };
                let agents: Vec<_> = self
                    .agents
                    .sessions
                    .iter()
                    .map(|s| {
                        json!({
                            "id": s.id, "title": s.title, "icon": s.icon,
                            "running": s.running(), "attention": s.attention,
                        })
                    })
                    .collect();
                let presets: Vec<_> = self
                    .cfg
                    .agents
                    .iter()
                    .map(|p| json!({"name": p.name, "icon": p.icon}))
                    .collect();
                json!({
                    "ok": true, "workspace": ws, "tabs": tabs,
                    "active": self.editor.active, "file": file, "dirty": dirty,
                    "cursor": [self.editor.cursor.0, self.editor.cursor.1],
                    "agents": agents, "agent_active": self.agents.active,
                    "presets": presets, "approval": self.cfg.approval_mode,
                    // 音声入力ページ (スマホ) が参照する設定
                    "voice": {"kw": self.cfg.voice_keyword, "lang": self.cfg.voice_lang},
                })
                .to_string()
            }
            remote::Query::File => match self.editor.active {
                Some(i) => {
                    let b = &self.editor.buffers[i];
                    json!({
                        "ok": true, "title": b.title, "text": b.text,
                        "lang": b.lang, "dirty": b.dirty(), "index": i,
                    })
                    .to_string()
                }
                None => json!({"ok": false}).to_string(),
            },
            remote::Query::Files => {
                let files: Vec<&String> = self.file_index.iter().take(4000).collect();
                json!({"ok": true, "files": files}).to_string()
            }
            remote::Query::SetText { text, index, save } => {
                let Some(active) = self.editor.active else {
                    return json!({"ok": false, "error": "ファイルが開かれていません"})
                        .to_string();
                };
                // スマホが編集していたタブと PC のアクティブタブが違えば拒否
                // (別ファイルを誤って上書きしない)
                if *index >= 0 && *index as usize != active {
                    return json!({
                        "ok": false,
                        "error": "PC 側でタブが切り替わっています — 再読込してください",
                    })
                    .to_string();
                }
                let b = &mut self.editor.buffers[active];
                b.text = text.clone();
                b.cache = None;
                b.gutter = None;
                if !*save {
                    return json!({"ok": true, "dirty": b.dirty()}).to_string();
                }
                // 保存も同一リクエストで原子的に行う。rfd ダイアログは開かない
                let Some(path) = b.path.clone() else {
                    return json!({
                        "ok": false,
                        "error": "名前のないファイルは PC 側で保存してください (⌘S)",
                    })
                    .to_string();
                };
                match std::fs::write(&path, &b.text) {
                    Ok(()) => {
                        b.saved_hash = hash_str(&b.text);
                        b.disk_mtime = disk_mtime(&path);
                        b.conflict_notified = None;
                        self.tree.invalidate();
                        self.toast(
                            format!("💾 保存しました (スマホから): {}", path.display()),
                            true,
                        );
                        json!({"ok": true, "dirty": false}).to_string()
                    }
                    Err(e) => {
                        json!({"ok": false, "error": format!("保存に失敗しました: {e}")})
                            .to_string()
                    }
                }
            }
            remote::Query::OpenFile(rel) => {
                // パストラバーサル防御: ワークスペース外は開かせない
                let p = self.workspace.join(rel);
                let ws = self
                    .workspace
                    .canonicalize()
                    .unwrap_or_else(|_| self.workspace.clone());
                let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
                if !canon.starts_with(&ws) {
                    return json!({"ok": false, "error": "ワークスペース外は開けません"})
                        .to_string();
                }
                match self.editor.open(&p, &self.highlighter) {
                    Ok(reloaded) => {
                        if reloaded {
                            if let Some(i) = self.editor.active {
                                self.queue_lsp_change(i);
                            }
                        }
                        self.persist_session();
                        json!({"ok": true}).to_string()
                    }
                    Err(e) => json!({"ok": false, "error": e}).to_string(),
                }
            }
            remote::Query::Tab(i) => {
                if *i < self.editor.buffers.len() {
                    self.editor.active = Some(*i);
                    self.find.last = None;
                    json!({"ok": true}).to_string()
                } else {
                    json!({"ok": false, "error": "タブがありません"}).to_string()
                }
            }
            remote::Query::Term => match self.agents.active_session() {
                Some(s) => {
                    let text = s.parser.lock().unwrap().screen().contents();
                    json!({
                        "ok": true, "title": s.title, "running": s.running(), "text": text,
                    })
                    .to_string()
                }
                None => json!({"ok": false}).to_string(),
            },
            remote::Query::VoiceSend { text, id, submit } => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    return json!({"ok": false, "error": "テキストが空です"}).to_string();
                }
                // submit=false は入力欄へ挿入するだけ (Enter は送らない)
                let payload = if *submit {
                    format!("{text}\r")
                } else {
                    text.clone()
                };
                let verb = if *submit { "送信" } else { "入力欄へ" };
                if *id < 0 {
                    // 全エージェントへブロードキャスト
                    let n = self.agents.running_count();
                    if n == 0 {
                        return json!({"ok": false, "error": "実行中のセッションがありません"})
                            .to_string();
                    }
                    for s in self.agents.sessions.iter_mut().filter(|s| s.running()) {
                        s.write_bytes(payload.as_bytes());
                    }
                    self.toast(format!("🎤📣 {n} セッション {verb}: {text}"), true);
                    json!({"ok": true, "sent": n}).to_string()
                } else {
                    // セッション id 指定 (インデックスではなく id — 閉じてもずれない)
                    match self
                        .agents
                        .sessions
                        .iter_mut()
                        .find(|s| s.id == *id as u64)
                    {
                        Some(s) if s.running() => {
                            s.write_bytes(payload.as_bytes());
                            let title = s.title.clone();
                            self.toast(format!("🎤 {title} {verb}: {text}"), true);
                            json!({"ok": true, "sent": 1}).to_string()
                        }
                        Some(_) => json!({"ok": false, "error": "セッションが停止しています"})
                            .to_string(),
                        None => json!({
                            "ok": false,
                            "error": "セッションが見つかりません (閉じられた可能性)",
                        })
                        .to_string(),
                    }
                }
            }
            remote::Query::TermInput(payload, raw) => match self.agents.active_session() {
                Some(s) if s.running() => {
                    if *raw {
                        s.write_bytes(payload.as_bytes());
                    } else {
                        s.write_bytes(format!("{payload}\r").as_bytes());
                    }
                    json!({"ok": true}).to_string()
                }
                _ => {
                    json!({"ok": false, "error": "実行中のセッションがありません"}).to_string()
                }
            },
            remote::Query::Cmd(name, arg) => {
                // 無題バッファへの save はブロッキングな rfd ダイアログを
                // PC 側に開いてしまうため、リモートからは拒否する
                if name == "save" {
                    let no_path = self
                        .editor
                        .active
                        .map(|i| self.editor.buffers[i].path.is_none())
                        .unwrap_or(true);
                    if no_path {
                        return json!({
                            "ok": false,
                            "error": "名前のないファイルは PC 側で保存してください (⌘S)",
                        })
                        .to_string();
                    }
                }
                let cmd = match name.as_str() {
                    "save" => Some(Cmd::Save),
                    "new" => Some(Cmd::NewFile),
                    "close_tab" => Some(Cmd::CloseTab),
                    "terminal" => Some(Cmd::ToggleTerminal),
                    "sidebar" => Some(Cmd::ToggleSidebar),
                    "git" => Some(Cmd::OpenGitPanel),
                    "cockpit" => Some(Cmd::ToggleCockpit),
                    "font_inc" => Some(Cmd::FontInc),
                    "font_dec" => Some(Cmd::FontDec),
                    "tree" => Some(Cmd::RefreshTree),
                    "approval_auto" => Some(Cmd::SetApproval("auto".into())),
                    "approval_ask" => Some(Cmd::SetApproval("ask".into())),
                    "approval_agent" => Some(Cmd::SetApproval("agent".into())),
                    "permission_cycle" => Some(Cmd::CyclePermissionAll),
                    "agent_launch" => Some(Cmd::NewAgent((*arg).max(0) as usize)),
                    "agent_focus" => Some(Cmd::FocusAgent((*arg).max(0) as usize)),
                    "agent_restart" => Some(Cmd::RestartAgent),
                    "agent_kill" => Some(Cmd::KillAgent),
                    _ => None,
                };
                match cmd {
                    Some(c) => {
                        self.apply_cmd(c, ctx);
                        json!({"ok": true}).to_string()
                    }
                    None => json!({"ok": false, "error": "unknown cmd"}).to_string(),
                }
            }
        }
    }

    /// QR コード付きの接続ウィンドウ。📱 ボタンで開閉する。
    fn remote_window(&mut self, ctx: &egui::Context) {
        if !self.remote_open {
            return;
        }
        // QR テクスチャを一度だけ生成 (トークン付き接続 URL)
        if self.qr_tex.is_none() {
            if let Some(r) = &self.remote {
                let full = format!("{}?t={}", r.url, r.token);
                if let Ok(code) = qrcode::QrCode::new(full.as_bytes()) {
                    let w = code.width();
                    let colors = code.to_colors();
                    let m = 2usize;
                    let size = w + m * 2;
                    let mut pixels = vec![Color32::WHITE; size * size];
                    for y in 0..w {
                        for x in 0..w {
                            if colors[y * w + x] == qrcode::Color::Dark {
                                pixels[(y + m) * size + (x + m)] = Color32::BLACK;
                            }
                        }
                    }
                    let img = egui::ColorImage {
                        size: [size, size],
                        pixels,
                    };
                    self.qr_tex = Some(ctx.load_texture(
                        "zv-remote-qr",
                        img,
                        egui::TextureOptions::NEAREST,
                    ));
                }
            }
        }

        let theme = self.theme.clone();
        let url_full = self
            .remote
            .as_ref()
            .map(|r| format!("{}?t={}", r.url, r.token));
        let err = self.remote_err.clone();
        let qr_tex = self.qr_tex.clone();
        let mut open = self.remote_open;
        let mut copy = false;
        let mut open_voice = false;

        egui::Window::new("📱 スマホリモート")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(340.0);
                match (&url_full, &err) {
                    (Some(url), _) => {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("同じ Wi-Fi のスマホで QR を読み取るだけで接続")
                                    .color(theme.text),
                            );
                            ui.add_space(8.0);
                            if let Some(tex) = &qr_tex {
                                ui.add(
                                    egui::Image::new(tex)
                                        .fit_to_exact_size(egui::vec2(240.0, 240.0)),
                                );
                            }
                            ui.add_space(8.0);
                            let mut u = url.clone();
                            ui.add(
                                egui::TextEdit::singleline(&mut u)
                                    .desired_width(320.0)
                                    .font(FontId::monospace(12.0)),
                            );
                            if ui.button("📋 URL をコピー").clicked() {
                                copy = true;
                            }
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new(
                                    "スマホから: ファイルの編集・保存・オープン、\n\
                                     エージェント操作 (Claude の承認・指示も OK)、各種コマンド\n\
                                     🎤 音声入力: スマホは「エージェント」タブのマイクボタン",
                                )
                                .size(11.5)
                                .color(theme.text_dim),
                            );
                            ui.add_space(6.0);
                            ui.separator();
                            if ui
                                .button("🎤 PC で音声入力する")
                                .on_hover_text(
                                    "Zaivern 内で音声認識し、話した内容を\n\
                                     エージェントの入力欄へ入れます (送信は自分で Enter)",
                                )
                                .clicked()
                            {
                                open_voice = true;
                            }
                        });
                    }
                    (None, Some(e)) => {
                        ui.colored_label(theme.err, format!("リモートサーバ起動失敗: {e}"));
                    }
                    _ => {}
                }
            });

        self.remote_open = open;
        if open_voice {
            self.apply_cmd(Cmd::VoiceInput(voice::Target::Broadcast), ctx);
        }
        if copy {
            if let Some(u) = url_full {
                ctx.copy_text(u);
            }
            self.toast("URL をコピーしました", true);
        }
    }
}

impl eframe::App for ZaivernApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 音声入力が先。押している間だけ録音するキーは他所へ渡さない
        // (ターミナルが PTY へ転送してしまうため)
        self.poll_voice(ctx);

        self.handle_shortcuts(ctx);

        // スマホリモートからのリクエストを処理する
        self.poll_remote(ctx);

        // プラグインコマンドの実行結果をエディタへ反映する
        self.process_plugin_results(ctx);

        // 外部(エージェント等)によるファイル書き換えを検知して自動リロードする
        self.check_external_changes();

        // LSP: デバウンスした変更を送信し、閉じたドキュメントを did_close する
        self.flush_lsp_changes();
        if !self.lsp_opened.is_empty() {
            let open_paths: HashSet<PathBuf> = self
                .editor
                .buffers
                .iter()
                .filter_map(|b| b.path.clone())
                .collect();
            let closed: Vec<PathBuf> =
                self.lsp_opened.difference(&open_paths).cloned().collect();
            for p in closed {
                for client in self.lsp.values() {
                    client.did_close(&p);
                }
                self.lsp_opened.remove(&p);
                self.lsp_pending.remove(&p);
            }
        }

        // エージェントの状態変化を通知する(非フォーカス時は OS 通知も)
        let win_focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        for ev in self.agents.poll_events() {
            match ev {
                SessionEvent::NeedsApproval(title) => {
                    // 同じセッションへのトースト+効果音は10秒に1回まで
                    // (プロンプトが画面に残ると再検出で連発するため)
                    let throttled = self
                        .pet_attention_notified
                        .get(&title)
                        .is_some_and(|at| at.elapsed().as_secs() < 10);
                    if !throttled {
                        self.pet_attention_notified.insert(title.clone(), Instant::now());
                        self.toast_warn(format!(
                            "🔔 {title} が承認待ちです — パネルで確認してください"
                        ));
                        if self.cfg.pet_sounds {
                            self.sound.play(SoundKind::Confirm);
                        }
                    }
                    // OS 通知はペット導入前からの挙動なのでそのまま
                    if !win_focused {
                        notify::notify("Zaivern Code", &format!("🔔 {title} が承認待ちです"));
                    }
                }
                SessionEvent::AutoApproved(title, desc) => {
                    self.toast(format!("⚡ {title}: {desc} を自動送信しました"), true);
                }
                SessionEvent::Exited(title, code) => {
                    if code == 0 {
                        self.toast(format!("✅ {title} が終了しました"), true);
                        // ペットが少しのあいだ喜ぶ + 完了音
                        self.pet_happy_until = Some(Instant::now() + Duration::from_secs(4));
                        if self.cfg.pet_sounds {
                            self.sound.play(SoundKind::Complete);
                        }
                    } else {
                        self.toast(format!("❌ {title} が終了しました (code {code})"), false);
                        // ペットが少しのあいだ落ち込む + エラー音
                        self.pet_error_until = Some(Instant::now() + Duration::from_secs(6));
                        if self.cfg.pet_sounds {
                            self.sound.play(SoundKind::Error);
                        }
                    }
                    if !win_focused {
                        let mark = if code == 0 { "✅" } else { "❌" };
                        notify::notify("Zaivern Code", &format!("{mark} {title} が終了しました"));
                    }
                }
            }
        }

        // ペットバブル関連の記録を毎フレーム掃除する(ペット非表示中も行い、
        // セッションの増減で無関係なセッションの記録が残らないようにする)
        {
            let sessions = &self.agents.sessions;
            // 承認待ちでなくなったセッションの却下記録は外す(次のプロンプトで再表示)
            self.pet_bubble_dismissed
                .retain(|&id| sessions.iter().any(|s| s.id == id && s.attention && s.running()));
            // 応答済み記録は3秒経過またはセッション消滅で外す
            self.pet_bubble_answered.retain(|&id, at| {
                at.elapsed().as_secs() < 3 && sessions.iter().any(|s| s.id == id)
            });
            // 通知スロットルの古い記録も掃除する
            self.pet_attention_notified.retain(|_, at| at.elapsed().as_secs() < 10);
        }

        self.top_bar(ctx);
        self.status_bar(ctx);
        self.sidebar(ctx);
        self.terminal_panel(ctx);

        let theme_bg = self.theme.bg;
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme_bg))
            .show(ctx, |ui| {
                if self.cockpit {
                    let ctx = ui.ctx().clone();
                    self.cockpit_ui(ui, &ctx);
                } else {
                    self.editor_area(ui);
                }
            });

        self.palette_ui(ctx);
        self.new_plugin_ui(ctx);
        self.close_confirm_ui(ctx);
        self.delete_confirm_ui(ctx);
        self.remote_window(ctx);
        self.voice_hud(ctx);
        self.toasts_ui(ctx);

        // デスクトップペット 🦀
        if self.cfg.show_pet {
            let now = Instant::now();
            let attention = self
                .agents
                .sessions
                .iter()
                .filter(|s| s.attention && s.running())
                .count();
            let input = pet::PetInput {
                working: self.agents.running_count(),
                attention,
                recent_success: self.pet_happy_until.is_some_and(|t| now < t),
                recent_error: self.pet_error_until.is_some_and(|t| now < t),
                variant: pet::PetVariant::from_name(&self.cfg.pet_variant),
                scale: self.cfg.pet_scale,
                free_roam: self.cfg.pet_free_roam,
                sleep_enabled: self.cfg.pet_sleep,
            };
            let r = pet::draw(
                ctx,
                &self.theme,
                &input,
                &mut self.pet_pos,
                self.pet_tex.as_ref(),
                &mut self.pet_rt,
            );
            if r.drag_released {
                // ドラッグ後の位置を保存する
                if let Some(p) = self.pet_pos {
                    self.cfg.pet_x = Some(p.x);
                    self.cfg.pet_y = Some(p.y);
                    config::save_state(&self.cfg);
                }
            }
            // ダブルクリックのご機嫌ホップに合わせて効果音を鳴らす
            if r.double_clicked && self.cfg.pet_sounds {
                self.sound.play(SoundKind::Confirm);
            }
            // クリック(ドラッグでない)のときだけアクション
            if r.clicked && !r.dragged {
                if let Some(i) = self
                    .agents
                    .sessions
                    .iter()
                    .position(|s| s.attention && s.running())
                {
                    self.apply_cmd(Cmd::FocusAgent(i), ctx);
                } else {
                    self.cockpit = !self.cockpit;
                }
            }

            // 承認待ちの吹き出し(ペットより後に描いて頭上に重ねる)
            if self.cfg.pet_bubbles {
                let items: Vec<pet_bubble::BubbleItem> = self
                    .agents
                    .sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| {
                        // 却下済み・応答直後(3秒以内)のセッションは出さない
                        s.attention
                            && s.running()
                            && !self.pet_bubble_dismissed.contains(&s.id)
                            && !self.pet_bubble_answered.contains_key(&s.id)
                    })
                    .map(|(i, s)| pet_bubble::BubbleItem {
                        session_idx: i,
                        key: s.id,
                        icon: if s.icon.is_empty() { "🤖".into() } else { s.icon.clone() },
                        title: s.title.clone(),
                    })
                    .collect();
                for act in pet_bubble::draw(ctx, &self.theme, &items, r.bubble_anchor) {
                    match act {
                        pet_bubble::BubbleAction::Approve(i) => {
                            let fallback = self.cfg.pet_approve_keys.clone();
                            let sent = self.agents.sessions.get_mut(i).map(|s| {
                                // 画面のプロンプトに合った承認キーを優先する
                                // (Bypass 警告は Enter だと「No, exit」になるため)
                                let keys = s
                                    .approve_reply()
                                    .map(str::to_string)
                                    .unwrap_or(fallback);
                                let ok = s.send_text(&keys);
                                if ok {
                                    s.resolve_attention();
                                }
                                (ok, s.title.clone(), s.id)
                            });
                            if let Some((true, title, id)) = sent {
                                self.pet_bubble_answered.insert(id, Instant::now());
                                self.toast(format!("✔ 承認を送信: {title}"), true);
                            }
                        }
                        pet_bubble::BubbleAction::Deny(i) => {
                            let keys = self.cfg.pet_deny_keys.clone();
                            let sent = self.agents.sessions.get_mut(i).map(|s| {
                                let ok = s.send_text(&keys);
                                if ok {
                                    s.resolve_attention();
                                }
                                (ok, s.title.clone(), s.id)
                            });
                            if let Some((true, title, id)) = sent {
                                self.pet_bubble_answered.insert(id, Instant::now());
                                self.toast(format!("✖ 拒否を送信: {title}"), true);
                            }
                        }
                        pet_bubble::BubbleAction::Focus(i) => {
                            self.apply_cmd(Cmd::FocusAgent(i), ctx);
                        }
                        pet_bubble::BubbleAction::Dismiss(i) => {
                            // index を安定 id に変換して記録する(index は次フレームでずれ得る)
                            if let Some(s) = self.agents.sessions.get(i) {
                                self.pet_bubble_dismissed.insert(s.id);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// ワークスペースからの相対パス表示(外なら絶対パス)。
/// `.git` がファイルのとき、その中身 (`gitdir: <path>`) から実際の git ディレクトリを取り出す。
/// 相対パスは workspace 基準で解決する。
fn parse_gitdir_file(contents: &str, workspace: &Path) -> Option<PathBuf> {
    let raw = contents
        .lines()
        .find_map(|l| l.trim().strip_prefix("gitdir:"))?
        .trim();
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(raw);
    Some(if p.is_absolute() { p } else { workspace.join(p) })
}

/// ブランチ表示のために読むべき HEAD のパス。
/// 通常のリポジトリは `<ws>/.git/HEAD` だが、linked worktree では `.git` が
/// ディレクトリではなくファイルなので、それが指す git ディレクトリ配下の HEAD を読む。
fn git_head_path(workspace: &Path) -> PathBuf {
    let dot_git = workspace.join(".git");
    if dot_git.is_file() {
        if let Some(dir) = std::fs::read_to_string(&dot_git)
            .ok()
            .and_then(|s| parse_gitdir_file(&s, workspace))
        {
            return dir.join("HEAD");
        }
    }
    dot_git.join("HEAD")
}

fn rel_label(p: &Path, ws: &Path) -> String {
    p.strip_prefix(ws).unwrap_or(p).display().to_string()
}

/// ペット画像を読み込み egui テクスチャ化する。長辺 256px に縮小する。
/// URL やファイルを OS の既定アプリ (ブラウザ等) で開く。
/// 入力欄に書いてある `old` を `new` にするための編集を求める。
///
/// 返すのは (消す文字数, 書き足す文字列)。端末の入力欄はカーソル位置から
/// Backspace で消すしかないので、**共通する先頭はそのまま残し、そこから後ろを
/// まるごと消して書き直す**。話しながら変換が変わっても、変わった部分だけの
/// やり取りで済む。
fn diff_edit(old: &str, new: &str) -> (usize, String) {
    let common = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let del = old.chars().count() - common;
    let add: String = new.chars().skip(common).collect();
    (del, add)
}

/// 音声のひとまとまりを前の続きへ書き足すとき、間に空白が要るか。
///
/// 息継ぎのたびに区切って入力欄へ足していくので、英文は単語がつながらないよう
/// 空白を入れる。日本語は元々分かち書きしないため、入れると逆に読みにくい。
fn needs_space(tail: Option<char>, head: Option<char>) -> bool {
    let (Some(a), Some(b)) = (tail, head) else {
        return false;
    };
    if a.is_whitespace() || b.is_whitespace() {
        return false;
    }
    !is_cjk(a) && !is_cjk(b)
}

/// 分かち書きしない文字 (かな・漢字・全角記号など)。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F   // 全角の句読点・記号
        | 0x3040..=0x30FF // ひらがな・カタカナ
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF // 漢字
        | 0xF900..=0xFAFF // 互換漢字
        | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 // 全角英数・記号
    )
}

/// 認識テキストの末尾が合図キーワードなら、それを取り除いた本文を返す。
/// 音声認識は句読点を付けることがあるので、末尾の記号は無視して判定する。
fn strip_trailing_keyword(text: &str, keyword: &str) -> Option<String> {
    let trimmed = text.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '。' | '、' | '.' | ',' | '!' | '?' | '！' | '？')
    });
    let rest = trimmed.strip_suffix(keyword)?;
    Some(rest.trim_end().to_string())
}

/// Chrome / Chromium の実行ファイルを探す。
///
/// Web Speech API は Chrome が一番素直に動く。Edge の `webkitSpeechRecognition` は
/// v134 の退行以来あてにならないので、Chrome が居るならそちらを優先する。
fn chrome_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    const CANDIDATES: &[&str] = &[
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ];
    #[cfg(target_os = "windows")]
    const CANDIDATES: &[&str] = &[
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    ];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    const CANDIDATES: &[&str] = &[
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ];
    // Windows は管理者権限なしで入れると %LOCALAPPDATA% 側に入る。
    // こちらの方がむしろ普通なので、固定パスより先に見る。
    #[cfg(target_os = "windows")]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local).join(r"Google\Chrome\Application\chrome.exe");
        if p.is_file() {
            return Some(p);
        }
    }
    CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

fn open_external(target: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(target).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(target).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", target])
        .spawn();
}

fn load_pet_texture(ctx: &egui::Context, path: &Path) -> Option<egui::TextureHandle> {
    let bytes = std::fs::read(path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    let mut rgba = img.to_rgba8();
    let (mut w, mut h) = rgba.dimensions();
    let longest = w.max(h);
    if longest > 256 {
        let scale = 256.0 / longest as f32;
        let nw = ((w as f32 * scale) as u32).max(1);
        let nh = ((h as f32 * scale) as u32).max(1);
        rgba = image::imageops::resize(&rgba, nw, nh, image::imageops::FilterType::Triangle);
        w = nw;
        h = nh;
    }
    let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
    Some(ctx.load_texture("zv-pet-image", color, egui::TextureOptions::LINEAR))
}

/// 言語IDに対応する LSP サーバーの起動コマンド。
fn lsp_server_for(lang_id: &str) -> Option<&'static str> {
    match lang_id {
        "rust" => Some("rust-analyzer"),
        "typescript" | "javascript" | "typescriptreact" | "javascriptreact" => {
            Some("typescript-language-server --stdio")
        }
        "python" => Some("pyright-langserver --stdio"),
        "go" => Some("gopls"),
        _ => None,
    }
}

/// which() の否定結果を覚えておく時間。
///
/// 3 秒: 60fps なら約 180 フレーム分の spawn が 1 回に減る一方、人が LSP サーバーを
/// インストールし終える時間(cargo install / npm -g で数十秒〜数分)よりずっと短いので、
/// 「起動中に入れたサーバーがいずれ認識される」性質は保たれる。
/// そもそも egui は再描画要求があるときしかフレームを回さないため、再確認の間隔は
/// 元から不定だった(アイドル中は何分でも確認されない)。TTL はその保証を弱めない。
const WHICH_MISS_TTL: Duration = Duration::from_secs(3);

/// 記録済みの which 結果がまだ有効か(= which() の再実行を省けるか)。
/// `last_checked` が None(未確認)なら常に再確認する。
fn which_result_is_fresh(last_checked: Option<Instant>, now: Instant, ttl: Duration) -> bool {
    match last_checked {
        Some(t) => now.saturating_duration_since(t) < ttl,
        None => false,
    }
}

/// コマンドが PATH 上に存在するか($SHELL -lc 経由で which)。
fn which(bin: &str) -> bool {
    if bin.is_empty() {
        return false;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    std::process::Command::new(shell)
        .arg("-lc")
        .arg(format!("command -v {bin}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// テーマ名を解決する。VS Code テーマJSONへのパスならそれを読み込み、
/// 失敗時・それ以外はビルトインテーマ名として解決する。
fn resolve_theme(name: &str) -> Theme {
    if name.ends_with(".json") || name.contains('/') || name.contains('\\') {
        if let Ok(t) = theme_json::load(Path::new(name)) {
            return t;
        }
    }
    theme::by_name(name)
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates: Vec<&str> = if cfg!(target_os = "macos") {
        vec![
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
            "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
        ]
    } else if cfg!(target_os = "windows") {
        vec![
            "C:/Windows/Fonts/YuGothM.ttc",
            "C:/Windows/Fonts/meiryo.ttc",
            "C:/Windows/Fonts/msgothic.ttc",
        ]
    } else {
        vec![
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJKjp-Regular.otf",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/fonts-japanese-gothic.ttf",
        ]
    };
    for p in candidates {
        if let Ok(bytes) = std::fs::read(p) {
            fonts
                .font_data
                .insert("cjk".into(), egui::FontData::from_owned(bytes));
            for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                if let Some(list) = fonts.families.get_mut(&fam) {
                    list.push("cjk".into());
                }
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gitdir_from_worktree_dot_git_file() {
        let ws = Path::new("/repo/.claude/worktrees/feature");

        // linked worktree の `.git` ファイル (git が書く形式は絶対パス + 末尾改行)
        let abs = "gitdir: /repo/.git/worktrees/feature\n";
        assert_eq!(
            parse_gitdir_file(abs, ws),
            Some(PathBuf::from("/repo/.git/worktrees/feature"))
        );

        // 相対パスは workspace 基準で解決する
        let rel = "gitdir: ../../../.git/worktrees/feature\n";
        assert_eq!(
            parse_gitdir_file(rel, ws),
            Some(ws.join("../../../.git/worktrees/feature"))
        );

        // gitdir 行が無い / 空なら None (通常の .git ディレクトリへフォールバックする)
        assert_eq!(parse_gitdir_file("ref: refs/heads/main\n", ws), None);
        assert_eq!(parse_gitdir_file("gitdir:   \n", ws), None);
        assert_eq!(parse_gitdir_file("", ws), None);
    }

    #[test]
    fn git_head_path_falls_back_to_dot_git_dir() {
        // `.git` が存在しない (=ファイルでない) 場合は従来どおり <ws>/.git/HEAD
        let ws = Path::new("/no/such/workspace");
        assert_eq!(git_head_path(ws), ws.join(".git").join("HEAD"));
    }

    #[test]
    fn which_cache_rechecks_when_never_checked() {
        // 未確認なら必ず which() を実行する(初回は元の挙動と同じ)
        let now = Instant::now();
        assert!(!which_result_is_fresh(None, now, WHICH_MISS_TTL));
    }

    #[test]
    fn which_cache_suppresses_repeat_within_ttl() {
        // TTL 以内の再確認は省く = 毎フレームのサブプロセス生成が消える
        let now = Instant::now();
        let just_now = now - Duration::from_millis(1);
        assert!(which_result_is_fresh(Some(just_now), now, WHICH_MISS_TTL));
    }

    #[test]
    fn which_cache_expires_after_ttl() {
        // TTL を過ぎたら再確認する = 起動後にインストールしても いずれ 認識される
        let now = Instant::now();
        let old = now - WHICH_MISS_TTL - Duration::from_millis(1);
        assert!(!which_result_is_fresh(Some(old), now, WHICH_MISS_TTL));
        // 境界(ちょうど TTL)も再確認側に倒す
        assert!(!which_result_is_fresh(Some(now - WHICH_MISS_TTL), now, WHICH_MISS_TTL));
    }

    #[test]
    fn which_cache_ttl_is_short_enough_to_feel_immediate() {
        assert!(WHICH_MISS_TTL <= Duration::from_secs(5));
    }

    #[test]
    fn joins_japanese_without_spaces() {
        // 息継ぎごとに区切って書き足しても、日本語は分かち書きにならない
        assert!(!needs_space(Some('る'), Some('修')));
        assert!(!needs_space(Some('。'), Some('あ')));
        assert!(!needs_space(Some('た'), Some('。')));
    }

    #[test]
    fn separates_english_words() {
        assert!(needs_space(Some('o'), Some('w')));
        assert!(needs_space(Some('.'), Some('T')));
    }

    #[test]
    fn no_space_at_the_start_or_next_to_existing_space() {
        // 先頭 (まだ何も送っていない)
        assert!(!needs_space(None, Some('a')));
        assert!(!needs_space(Some('a'), None));
        // すでに空白があるところへ重ねない
        assert!(!needs_space(Some(' '), Some('a')));
    }

    #[test]
    fn mixed_scripts_follow_the_japanese_side() {
        // 日本語と英語が隣り合うときは詰める (「Rustで」を割らない)
        assert!(!needs_space(Some('t'), Some('で')));
        assert!(!needs_space(Some('を'), Some('R')));
    }

    #[test]
    fn streaming_appends_only_the_new_tail() {
        // 話し進めているだけの間は、増えたぶんを足すだけで消さない
        assert_eq!(diff_edit("", "こん"), (0, "こん".into()));
        assert_eq!(diff_edit("こん", "こんにちは"), (0, "にちは".into()));
    }

    #[test]
    fn streaming_rewrites_only_what_changed() {
        // 変換が確定して後ろが書き換わったケース。共通する先頭は残す
        // (「きょうは」まで同じ → 「いいてんき」3 文字を消して「良い天気」を書く)
        assert_eq!(diff_edit("きょうはいい", "きょうは良い"), (2, "良い".into()));
        // 文字数は「バイト数」ではなく「文字数」で数える (日本語が壊れないこと)
        let (del, add) = diff_edit("あいうえお", "あい");
        assert_eq!((del, add.as_str()), (3, ""));
    }

    #[test]
    fn streaming_is_a_noop_when_nothing_changed() {
        // 同じ partial が続けて届いても端末へは何も送らない
        assert_eq!(diff_edit("こんにちは", "こんにちは"), (0, String::new()));
    }

    #[test]
    fn streaming_erases_everything_when_the_head_changes() {
        // 先頭から変わったら全部消して書き直す
        assert_eq!(diff_edit("abc", "xyz"), (3, "xyz".into()));
    }

    #[test]
    fn streaming_handles_the_separator_space_as_part_of_the_text() {
        // 区切りの空白も live に含めて数えるので、書き換えても空白が消えない
        assert_eq!(diff_edit(" and", " and then"), (0, " then".into()));
    }

    /// 届け先セッションの id (テスト用の適当な値)
    const DEST: u64 = 1;

    #[test]
    fn second_utterance_continues_in_the_same_field() {
        let mut v = VoiceState::default();

        // 1 回目 — 話しながら partial が伸びていく。増えたぶんだけ書き足す
        let e = v.plan("こん", DEST);
        assert_eq!((e.del, e.add.as_str()), (0, "こん"));
        v.commit(e, false, false, DEST);
        let e = v.plan("こんにちは", DEST);
        assert_eq!((e.del, e.add.as_str()), (0, "にちは"));
        v.commit(e, false, false, DEST);

        // 確定。中身は最後の partial と同じで送るバイトは無いが、
        // ここで追跡を締めないと 2 回目の発話が 1 回目を消してしまう
        let e = v.plan("こんにちは", DEST);
        assert!(e.is_noop());
        v.commit(e, true, false, DEST);
        assert!(v.live.is_empty(), "確定した分は書き換え対象から外れること");

        // 2 回目 — 前の文を 1 文字も消さずに、その後ろへ書き足す
        let e = v.plan("さようなら", DEST);
        assert_eq!((e.del, e.add.as_str()), (0, "さようなら"));
    }

    #[test]
    fn second_utterance_is_spaced_in_english_and_stays_spaced() {
        let mut v = VoiceState::default();
        let e = v.plan("hello", DEST);
        v.commit(e, true, false, DEST);

        // 続きの発話は単語がつながらないよう空白を挟む
        let e = v.plan("world", DEST);
        assert_eq!((e.del, e.add.as_str()), (0, " world"));
        v.commit(e, false, false, DEST);

        // 途中で認識が変わっても区切りの空白は据え置き (" world" → " word")
        let e = v.plan("word", DEST);
        assert_eq!((e.del, e.add.as_str()), (2, "d"));
        assert_eq!(e.want, " word");
    }

    #[test]
    fn submitting_starts_the_next_utterance_from_scratch() {
        let mut v = VoiceState::default();
        let e = v.plan("送ります", DEST);
        v.commit(e, true, true, DEST);
        // Enter を送ったので入力欄は空 — 消す文字も区切りの空白も無い
        assert!(v.live.is_empty());
        assert_eq!(v.last_char, None);
        assert_eq!(v.last_sent_to, None);
        let e = v.plan("次の話", DEST);
        assert_eq!((e.del, e.add.as_str()), (0, "次の話"));
    }

    #[test]
    fn switching_destination_does_not_backspace_the_new_one() {
        let mut v = VoiceState::default();
        let e = v.plan("前の宛先へ", DEST);
        v.commit(e, false, false, DEST);

        // 宛先が変わったら追跡を捨てる (apply_voice_text がやること)
        v.live.clear();
        v.last_char = None;
        // 別セッションへは先頭から書き出す。空白も Backspace も入らない
        let e = v.plan("新しい宛先へ", 2);
        assert_eq!((e.del, e.add.as_str()), (0, "新しい宛先へ"));
    }

    /// テスト用の入力欄シミュレータ。端末へ送ったバイト列を実際に当ててみる。
    /// 0x7f で末尾を 1 文字消し、残りは書き足す (`\r` は送信 = 空になる)。
    fn apply_bytes(field: &mut String, bytes: &[u8]) {
        let del = bytes.iter().take_while(|b| **b == 0x7f).count();
        for _ in 0..del {
            field.pop();
        }
        let rest = &bytes[del..];
        if rest.last() == Some(&b'\r') {
            field.clear();
            return;
        }
        field.push_str(std::str::from_utf8(rest).unwrap());
    }

    #[test]
    fn dictation_lands_in_the_field_as_spoken() {
        // 実際の認識の流れを再現する: 話しながら変換が書き換わり、息継ぎで確定し、
        // 2 回目の発話がその後ろへ続く。入力欄に残る文字列を突き合わせる。
        let mut v = VoiceState::default();
        let mut field = String::new();
        let step = |v: &mut VoiceState, field: &mut String, text: &str, is_final: bool| {
            let e = v.plan(text, DEST);
            apply_bytes(field, &e.bytes(false));
            v.commit(e, is_final, false, DEST);
        };

        // 1 回目 — 「せかい」が「世界」へ変換されても二重にならない
        step(&mut v, &mut field, "こんにちは", false);
        assert_eq!(field, "こんにちは");
        step(&mut v, &mut field, "こんにちはせかい", false);
        assert_eq!(field, "こんにちはせかい");
        step(&mut v, &mut field, "こんにちは世界", false);
        assert_eq!(field, "こんにちは世界");
        // 確定 — 中身は直前と同じなので端末へは何も送らない
        step(&mut v, &mut field, "こんにちは世界", true);
        assert_eq!(field, "こんにちは世界");

        // 2 回目 — 1 回目を 1 文字も消さずに後ろへ続く
        step(&mut v, &mut field, "これは", false);
        assert_eq!(field, "こんにちは世界これは");
        step(&mut v, &mut field, "これは二回目です", false);
        step(&mut v, &mut field, "これは二回目です", true);
        assert_eq!(field, "こんにちは世界これは二回目です");

        // 3 回目まで続けても崩れない
        step(&mut v, &mut field, "さらに三回目", false);
        step(&mut v, &mut field, "さらに三回目も", true);
        assert_eq!(field, "こんにちは世界これは二回目ですさらに三回目も");
    }

    #[test]
    fn english_dictation_keeps_words_apart() {
        let mut v = VoiceState::default();
        let mut field = String::new();
        for (text, is_final) in [("hello", false), ("hello", true), ("world", false), ("world", true)]
        {
            let e = v.plan(text, DEST);
            apply_bytes(&mut field, &e.bytes(false));
            v.commit(e, is_final, false, DEST);
        }
        assert_eq!(field, "hello world");
    }

    #[test]
    fn edit_bytes_are_backspaces_then_text() {
        let e = VoiceEdit {
            del: 2,
            add: "は".into(),
            want: "は".into(),
            space: false,
        };
        let mut want = b"\x7f\x7f".to_vec();
        want.extend_from_slice("は".as_bytes());
        assert_eq!(e.bytes(false), want);
        // 合図キーワードで送信するときだけ Enter が付く
        want.push(b'\r');
        assert_eq!(e.bytes(true), want);
    }

    #[test]
    fn reset_live_forgets_what_was_written() {
        // ユーザーが手で Enter を押した後などに呼ぶ。次は先頭から書き出す
        let mut v = VoiceState {
            live: "書きかけ".into(),
            live_space: true,
            last_char: Some('け'),
            ..Default::default()
        };
        v.reset_live();
        assert!(v.live.is_empty());
        assert!(!v.live_space);
        assert_eq!(v.last_char, None);
        // 追跡を捨てた直後は区切りの空白も入らない
        assert!(!needs_space(v.last_char, Some('a')));
    }
}
