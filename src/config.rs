use crate::agents;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// ファイル索引 (⌘P) の既定の上限。`.gitignore` を尊重すれば
/// 数万ファイル規模のモノレポでも届かない値にしてある
/// (以前は 8000 件 / 深さ 12 で**無音**に打ち切っていた)。
pub const DEFAULT_INDEX_MAX_FILES: usize = 50_000;
/// ファイル索引が潜る既定の深さ。node_modules を除いた実在のツリーは
/// 20 段も無いが、生成物混じりのリポジトリでも足りるよう余裕を持たせる。
pub const DEFAULT_INDEX_MAX_DEPTH: usize = 32;
/// ツリーの 1 ディレクトリで一度に描く既定の行数。
/// 画面に入るのは数十行なので、これを超えたぶんは「さらに N 件」に畳む。
pub const DEFAULT_TREE_DIR_PAGE: usize = 300;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: String,
    pub editor_font_size: f32,
    pub terminal_font_size: f32,
    /// 画面全体のズーム倍率 (VS Code の `window.zoomLevel` 相当)。
    ///
    /// egui の `zoom_factor` へそのまま渡す = **UI の全部** (サイドバー・タブ・
    /// メニュー・端末・エディタ) が一緒に拡大縮小する。段は `crate::zoom::STEPS`。
    /// ファイル単位のズームは倍率をバッファ側が持つので、ここには入らない。
    pub ui_zoom: f32,
    pub show_hidden_files: bool,

    /// `.gitignore` (+ `.git/info/exclude` + `core.excludesFile`) を尊重して
    /// ファイルツリーとファイル索引 (⌘P) から除外するか。**既定はオン**。
    /// これが無いと `node_modules` / `target` がツリーと索引を埋め尽くす。
    pub respect_gitignore: bool,

    /// 無視されたファイルを隠さず**薄く**表示するか (VS Code と同じ見せ方)。
    /// `respect_gitignore = false` のときは意味を持たない。既定はオフ。
    pub dim_ignored_files: bool,

    /// ファイル索引 (⌘P) に載せる最大件数。上限に達したらパレットに
    /// 「N 件で打ち切りました」と出す (無音で切らない)。
    /// `.gitignore` を尊重していれば数万件のリポジトリでも届かない。
    pub index_max_files: usize,

    /// ファイル索引が潜る最大の深さ (ルート直下 = 1)。
    pub index_max_depth: usize,

    /// ツリーの 1 ディレクトリで一度に描く行数の上限。
    /// 超えたぶんは「さらに N 件」を押すと同じ数だけ伸びる
    /// (巨大ディレクトリで数万行を描いてフレームを落とさないため)。
    pub tree_dir_page: usize,

    /// エディタ本文の折り返し (VS Code の Word Wrap 相当)。既定はオフ。
    pub word_wrap: bool,

    /// 空白文字の可視化 (スペース「·」/ タブ「→」)。既定はオフ。
    pub show_whitespace: bool,

    /// カーソル下のシンボルと同じものを本文で薄くハイライトするか
    /// (LSP の textDocument/documentHighlight)。既定はオン。
    /// オフにすると要求自体を送らないので、サーバーへの往復もゼロになる。
    pub lsp_highlight_occurrences: bool,

    /// 診断メッセージを本文の**行末**に淡色で出すか (VS Code の Error Lens 相当)。
    /// **既定はオン。ただし出すのはキャレット行だけ** — 全行に出すと本文の
    /// 右側が文章で埋まり、コードより診断の方が目立ってしまう。
    /// オフにしても波線とホバーは残る (消えるのは行末の文字だけ)。
    pub inline_diagnostics: bool,

    /// エディタ右端のミニマップ (VS Code の遠景ビュー相当)。
    /// **既定はオフ** — 本文の横幅を 64px 奪うので、欲しい人だけが払う。
    /// 幅が足りない画面では設定が ON でも自動的に隠れる。
    pub minimap: bool,

    /// 取り消し履歴: 連続した文字入力を 1 段へまとめる時間しきい値 (ミリ秒)。
    /// これを越えて間が空いたら別の段になる (VS Code / Zed と同じ考え方)。
    pub undo_merge_ms: u64,

    /// 取り消し履歴の最大段数 (タブ 1 枚あたり)。古いものから捨てる。
    pub undo_max_steps: usize,

    /// 取り消し履歴が抱える差分の合計バイト上限 (タブ 1 枚あたり)。
    /// 巨大な一括置換を何度もやっても、ここで頭打ちになる。
    pub undo_max_bytes: usize,

    /// エディタ上部のブレッドクラム (`ワークスペース › フォルダ › ファイル › シンボル`)。
    /// **既定はオン** — 高さ 1 行ぶんで、どの言語でも (LSP 無しでも) 必ず出せる。
    pub breadcrumbs: bool,
    /// 差分ビューの既定の表示: `"side_by_side"` (左右 2 列) | `"inline"` (1 列)。
    ///
    /// **既定は並列**。ただし幅が足りないときは `diff::diff_layout` が
    /// 自動で 1 列へ縮退させるので、狭いウィンドウでも見切れない。
    /// 値の解釈は [`crate::diff::DiffMode::from_config_str`] に集約してある。
    pub diff_view: String,
    /// エディタ左端のガターに git blame (著者 · 相対日時) を出す。既定はオフ。
    /// オンの間だけ可視範囲ぶんの `git blame` を非同期で取る。
    pub git_blame: bool,

    // ── 保存時の整形 (VS Code の files.* / editor.formatOnSave 相当) ──
    //
    // **どれも既定はオフ** — VS Code の既定と同じ。保存しただけで差分が
    // 増えるのは事故なので、明示的に選んだ人だけが払う。
    /// 保存時に各行の末尾空白を落とす (`files.trimTrailingWhitespace`)。
    /// 全角スペース U+3000 と NBSP は落とさない
    /// ([`crate::editor_ops::trim_trailing_whitespace`] を参照)。
    pub trim_trailing_whitespace: bool,
    /// 保存時に末尾の余分な空行を落とす (`files.trimFinalNewlines`)。
    pub trim_final_newlines: bool,
    /// 保存時に最終行へ改行を入れる (`files.insertFinalNewline`)。
    pub insert_final_newline: bool,
    /// 保存時に LSP の整形をかける (`editor.formatOnSave`)。
    /// 整形の経路は「ドキュメントを整形」と同じ 1 本。
    pub format_on_save: bool,

    // ── エディタの見た目・インデント ──────────────────────────────
    /// 括弧を入れ子の深さごとに色分けする
    /// (`editor.bracketPairColorization.enabled`)。**既定はオン**。
    /// 色はテーマの ANSI 表から採る ([`crate::theme::Theme::bracket_colors`])。
    pub bracket_colorization: bool,
    /// 縦のルーラーを引く桁 (`editor.rulers`)。既定は空 = 1 本も引かない。
    /// 例: `rulers = [80, 120]`。桁は等幅の**桁数**で数える (東アジア文字幅ではない)。
    pub rulers: Vec<usize>,
    /// 開いたファイルの中身からインデントを推定する
    /// (`editor.detectIndentation`)。**既定はオン**。
    /// オフにすると `tab_size` / `insert_spaces` をそのまま使う。
    pub detect_indentation: bool,
    /// インデント 1 段の桁数 (`editor.tabSize`)。既定 4。
    pub tab_size: usize,
    /// インデントにスペースを使う (`editor.insertSpaces`)。既定オン。
    pub insert_spaces: bool,
    /// タブ切替 (Ctrl+Tab) を **MRU (最近使った順)** で回すか。
    ///
    /// **既定はオン** — VS Code / Zed と同じで、押しっぱなしの間に候補一覧を
    /// 出し、離したところで確定する。2 回押せば直前のファイルへ戻れる。
    /// オフにすると従来どおりの**位置巡回** (タブの並び順) になる。
    /// どちらでも `[keybindings]` の `switch_tab` / `switch_tab_back` で
    /// 打鍵を付け替えられる。
    pub tab_switch_mru: bool,

    /// **プレビュータブ** (VS Code の斜体タブ) を使うか。
    ///
    /// **既定はオン** — ツリーやパレットからシングルクリックで開いたタブは
    /// 使い捨てになり、次のプレビューで置き換わるのでタブが無限に増えない。
    /// もう一度クリック / 編集 / ピン留め / ドラッグで確定タブへ昇格する。
    /// オフにすると開いたタブは常に確定タブになる。
    pub preview_tabs: bool,

    /// 既定の権限モード: "ask"(毎回ユーザー承認) | "auto"(全て自動YES) |
    /// "agent"(Agent欄優先: プリセットのコマンドに書かれたフラグをそのまま使う)
    pub approval_mode: String,
    /// フォルダを開き直したとき、前回のエージェントタブを復元して会話を再開するか。
    /// **既定は false** — 起動しただけで前回の会話が勝手に走り出さない。
    /// 過去の会話へ戻る口は「💬 セッション」タブ (明示的に選んで再開) の 1 本に絞る。
    /// true にすると前回スクロールバックを再生し、対応 CLI (claude / codex) は
    /// 再開指定付きで起動する。
    pub restore_agents: bool,
    pub show_pet: bool,
    /// state.toml へ書き戻すグローバル値の控え (プロジェクト overlay 適用前)。
    /// save_state はこちらを書くので、.zaivern.toml のプロジェクト値が
    /// グローバル state.toml へ漏れて永続化されることはない。
    /// 設定ファイルには読み書きしない (メモリ上だけ)。
    #[serde(skip)]
    pub global_theme: String,
    #[serde(skip)]
    pub global_approval_mode: String,
    #[serde(skip)]
    pub global_show_pet: bool,
    #[serde(skip)]
    pub global_word_wrap: bool,
    #[serde(skip)]
    pub global_show_whitespace: bool,
    #[serde(skip)]
    pub global_ui_zoom: f32,
    #[serde(skip)]
    pub global_minimap: bool,
    #[serde(skip)]
    pub global_breadcrumbs: bool,
    pub global_git_blame: bool,
    /// overlay を重ねる前のグローバルなプラグイン設定の控え。
    /// save_plugins_section はこちらを書く — セッション中の値を書くと
    /// プロジェクトの .zaivern.toml 由来の無効化・設定値がグローバル
    /// config.toml へ漏れて永続化されてしまう。
    #[serde(skip)]
    pub global_plugins: PluginsConfig,
    /// ペット画像のフルパス(None なら内蔵ドット絵)
    pub pet_image: Option<String>,
    /// ペットの固定位置(None なら右下うろうろ)
    pub pet_x: Option<f32>,
    pub pet_y: Option<f32>,
    /// ペットの見た目: "blocky" | "crab" | "cat" | "cloud"
    pub pet_variant: String,
    /// ペットの大きさ (0.75=小 / 1.0=中 / 1.4=大)
    pub pet_scale: f32,
    /// うろうろ散歩するか
    pub pet_free_roam: bool,
    /// 無操作で睡眠するか
    pub pet_sleep: bool,
    /// 効果音を鳴らすか
    pub pet_sounds: bool,
    /// 承認バブルを表示するか
    pub pet_bubbles: bool,
    /// 承認プロンプトへ自動で YES を送るか (オフ=ユーザー承認必須)
    pub pet_auto_yes: bool,
    /// 承認時に PTY へ送るキー (既定は Enter)
    pub pet_approve_keys: String,
    /// 拒否時に PTY へ送るキー (既定は ESC)
    pub pet_deny_keys: String,
    /// 自動YESの追加ルール (ユーザー定義)。
    ///
    /// CLI 側が承認プロンプトの文言を変えると、同梱の応答表 (src/agents.rs) が
    /// 一致しなくなり自動YESが素通りする。そのとき**再ビルドせずに**
    /// config.toml だけで直せるようにするための逃げ道。
    /// 組み込みの表より先に評価されるので、既定の判断を上書きもできる。
    pub auto_yes_rules: Vec<AutoYesRule>,
    /// 統合承認キューのポリシー (`[[approval_policies]]`)。
    ///
    /// 「この種別の承認は、この範囲では常に許可/拒否」を宣言する表。
    /// 空 (既定) なら従来どおり全部ユーザーに聞く。**このキーが無い
    /// 既存の config.toml でも `Config` の `#[serde(default)]` により
    /// 空として読み込まれる**ので、古い設定を壊さない。
    pub approval_policies: Vec<ApprovalPolicy>,
    /// 音声認識エンジン: "auto" | "mac" | "powershell" | "browser" | "command" | "off"
    /// auto = macOS は内蔵、voice_command 設定済みならそれ、Windows は標準の
    /// 音声認識、残りはブラウザの /voice ページ (src/voice.rs の resolve_engine)
    pub voice_engine: String,
    /// 音声入力の既定の届け先: "active"(アクティブなエージェント) | "broadcast"(全員)
    pub voice_target: String,
    /// 認識言語 (BCP-47)
    pub voice_lang: String,
    /// 外部音声認識コマンド (mac 以外 / 独自エンジン用)。
    /// 標準出力に 1 行ずつテキストを吐き、stdin の "q" で停止する実装を想定。
    /// {lang} は voice_lang に置換される。
    pub voice_command: String,
    /// 話すと自動で Enter まで送るキーワード (空文字 = 常に手動 Enter)
    pub voice_keyword: String,
    /// SSH リモート接続の踏み台 (`user@host` / `user@host:port`。空 = 未設定)。
    ///
    /// **鍵やパスフレーズは絶対に保存しない** — 認証は OS の `ssh` と
    /// `~/.ssh/config` / ssh-agent に丸投げする。ここに置くのは接続先だけ。
    pub ssh_tunnel_host: String,
    /// 外部通知の Webhook URL (空 = 無効)。承認待ち・終了・レート制限を
    /// curl で POST する。ntfy トピック URL / Slack / Discord の Incoming Webhook に対応。
    pub webhook_url: String,
    pub agents: Vec<AgentPreset>,
    /// キーバインドの上書き: action名 → "cmd+shift+p" 形式 (src/keybinds.rs 参照)
    pub keybindings: HashMap<String, String>,
    /// プラグインの有効/無効と設定値。
    pub plugins: PluginsConfig,
    /// エージェント監視 (スーパーバイザー) の設定。
    /// `[supervisor]` セクションが無い既存の config.toml でも、
    /// `SupervisorConfig` 側の `#[serde(default)]` により既定値で読み込まれる。
    pub supervisor: crate::supervisor::SupervisorConfig,
    /// 監視役 LLM (スーパーエージェント) の設定。
    /// `[super_agent]` セクションが無い既存の config.toml でも、
    /// `SuperAgentConfig` 側の `#[serde(default)]` により既定値 (= なし) で読み込まれる。
    pub super_agent: SuperAgentConfig,
    /// レート制限時のアカウント自動フェイルオーバー (`[failover]`)。
    /// **既定は無効** — ユーザーが明示的に有効化したときだけ働く。
    pub failover: crate::failover::FailoverConfig,

    /// コマンドパレットの MRU (最近実行したコマンド。先頭が直近)。
    ///
    /// UI の操作から溜まる値なので手書きの config.toml には**書かない** —
    /// state.toml 側 (`[[palette_recent]]`) に置く。`save_state` が
    /// この控えをそのまま書き戻すので、テーマ変更などで消えることはない。
    #[serde(skip)]
    pub palette_recent: Vec<PaletteRecent>,
}

/// パレット MRU の永続化 1 件ぶん。**アクションは保存しない** —
/// `Cmd` にはパスや添字が入り、次の起動では別物を指しうるため。
/// ラベルから引き直せなければ順位付けにだけ使う (palette.rs)。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaletteRecent {
    pub label: String,
    pub icon: String,
    pub uses: u32,
}

/// `[super_agent]` セクション。**どのエージェントに他のエージェントを見張らせるか**。
///
/// 既定は「なし」。決定論的な監視 (supervisor) は この設定に関わらず常に働くので、
/// ここが空でも見張り自体は成立する。LLM はあくまで助言役。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SuperAgentConfig {
    /// 監視役に使うプリセットのコマンド。空文字 = なし (LLM には相談しない)。
    pub command: String,
    /// 指揮官に指名したセッションのタイトル (例: `Claude Code (全自動) #3`)。
    /// 空文字 = 指名なし (旧来どおり `command` に一致する最初のセッションを使う)。
    /// セッション ID は再起動で変わるため、再起動をまたいでも追従できる
    /// タイトルで持つ。
    pub session_title: String,
    /// 指揮を有効にするか。指名 (`session_title` / `command`) が空なら
    /// この値によらず指揮しない。
    pub enabled: bool,
    /// (廃止) かつての状況フィードの最短間隔 (秒)。状況フィードは廃止された
    /// (指揮官の端末へも他エージェントへも自動注入しない) ため、いまは
    /// どこからも参照されない。既存の state.toml を壊さないよう残している。
    pub timeout_secs: u64,
}

impl Default for SuperAgentConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            session_title: String::new(),
            enabled: false,
            timeout_secs: 60,
        }
    }
}

impl SuperAgentConfig {
    /// 監視役として実際に動かす対象のコマンド。無効・未選択なら `None`。
    ///
    /// 「有効フラグが立っている」だけでは足りない。コマンドが空なら誰も選ばれて
    /// いないので、ここで必ず弾く。
    pub fn active_command(&self) -> Option<&str> {
        if !self.enabled {
            return None;
        }
        let c = self.command.trim();
        if c.is_empty() {
            None
        } else {
            Some(c)
        }
    }
}

/// `[plugins]` セクション。
///
/// - `disabled`: 無効にするプラグイン名。未記載のものは有効。
/// - `settings`: プラグインごとの設定値 (`[plugins.settings.<名前>]`)。
///   キーはマニフェストの `[[setting]] key`、値は文字列として保持する。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    pub disabled: Vec<String>,
    pub settings: HashMap<String, HashMap<String, String>>,
}

impl PluginsConfig {
    /// 指定プラグインが有効か。
    pub fn is_enabled(&self, name: &str) -> bool {
        !self.disabled.iter().any(|d| d == name)
    }

    /// 有効/無効を切り替える。
    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        if enabled {
            self.disabled.retain(|d| d != name);
        } else if !self.disabled.iter().any(|d| d == name) {
            self.disabled.push(name.to_string());
        }
    }

    /// プラグインの設定値を取り出す (未設定なら None)。
    /// `set_setting` と対になる読み出し口として公開しておく。
    #[allow(dead_code)]
    pub fn setting(&self, plugin: &str, key: &str) -> Option<&str> {
        self.settings.get(plugin)?.get(key).map(|s| s.as_str())
    }

    /// プラグインの設定値を書き込む。
    pub fn set_setting(&mut self, plugin: &str, key: &str, value: &str) {
        self.settings
            .entry(plugin.to_string())
            .or_default()
            .insert(key.to_string(), value.to_string());
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "zaivern-dark".into(),
            editor_font_size: 15.0,
            terminal_font_size: 13.0,
            ui_zoom: crate::zoom::DEFAULT,
            show_hidden_files: true,
            respect_gitignore: true,
            dim_ignored_files: false,
            index_max_files: DEFAULT_INDEX_MAX_FILES,
            index_max_depth: DEFAULT_INDEX_MAX_DEPTH,
            tree_dir_page: DEFAULT_TREE_DIR_PAGE,
            word_wrap: false,
            show_whitespace: false,
            lsp_highlight_occurrences: true,
            inline_diagnostics: true,
            minimap: false,
            undo_merge_ms: crate::editor::UNDO_MERGE_MS,
            undo_max_steps: crate::editor::UNDO_MAX_STEPS,
            undo_max_bytes: crate::editor::UNDO_MAX_BYTES,
            breadcrumbs: true,
            diff_view: crate::diff::DiffMode::default().config_str().into(),
            git_blame: false,
            // 保存時の整形は VS Code と同じく全部オフから始める
            trim_trailing_whitespace: false,
            trim_final_newlines: false,
            insert_final_newline: false,
            format_on_save: false,
            bracket_colorization: true,
            rulers: Vec::new(),
            detect_indentation: true,
            tab_size: crate::editor_ops::IndentStyle::DEFAULT_WIDTH,
            insert_spaces: true,
            tab_switch_mru: true,
            preview_tabs: true,
            approval_mode: "ask".into(),
            restore_agents: false,
            show_pet: true,
            global_theme: "zaivern-dark".into(),
            global_approval_mode: "ask".into(),
            global_show_pet: true,
            global_word_wrap: false,
            global_show_whitespace: false,
            global_ui_zoom: 1.0,
            global_minimap: false,
            global_breadcrumbs: true,
            global_git_blame: false,
            global_plugins: PluginsConfig::default(),
            pet_image: None,
            pet_x: None,
            pet_y: None,
            pet_variant: "blocky".into(),
            pet_scale: 1.0,
            pet_free_roam: true,
            pet_sleep: true,
            pet_sounds: true,
            pet_bubbles: true,
            pet_auto_yes: false,
            pet_approve_keys: "\r".into(),
            pet_deny_keys: "\u{1b}".into(),
            auto_yes_rules: Vec::new(),
            approval_policies: Vec::new(),
            voice_engine: "auto".into(),
            voice_target: "active".into(),
            voice_lang: "ja-JP".into(),
            voice_command: String::new(),
            voice_keyword: String::new(),
            ssh_tunnel_host: String::new(),
            webhook_url: String::new(),
            agents: default_agents(),
            keybindings: HashMap::new(),
            supervisor: crate::supervisor::SupervisorConfig::default(),
            super_agent: SuperAgentConfig::default(),
            plugins: PluginsConfig::default(),
            failover: crate::failover::FailoverConfig::default(),
            palette_recent: Vec::new(),
        }
    }
}

impl Config {
    /// 取り消し履歴のしきい値と上限。**呼び出し側で直書きしないための唯一の口**。
    pub fn history_limits(&self) -> crate::editor::HistoryLimits {
        crate::editor::HistoryLimits {
            merge_ms: self.undo_merge_ms,
            // 0 を書かれても 1 段は残す (`History::trim` 側でも守っている)
            max_steps: self.undo_max_steps.max(1),
            max_bytes: self.undo_max_bytes,
        }
    }
}

/// 自動YESのユーザー定義ルール 1 件 (`[[auto_yes_rules]]`)。
///
/// ```toml
/// [[auto_yes_rules]]
/// pattern = "Allow access to this file?"   # 画面に出る目印
/// reply   = "\r"                            # PTY へ送るキー ("\r"=Enter)
/// agent   = "agy"                           # 省略/空なら全エージェント
/// ```
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct AutoYesRule {
    /// 画面に含まれていたら一致とみなす文字列。空の行は無視される。
    pub pattern: String,
    /// 一致したとき PTY へ送るキー列。TOML のエスケープがそのまま使える
    /// (`"\r"` = Enter / `"y\r"` = y と Enter / `"1"` = 番号キー)。
    pub reply: String,
    /// 対象エージェントの実行ファイル名 (`agy` / `claude` …)。
    /// 空なら全エージェント。別名で書いても正規化されないので正規名で書く。
    pub agent: String,
}

impl Default for AutoYesRule {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            reply: "\r".into(),
            agent: String::new(),
        }
    }
}

/// 統合承認キューのポリシー 1 件 (`[[approval_policies]]`)。
///
/// ```toml
/// [[approval_policies]]
/// kind     = "file_read"     # 承認の種別 (src/approvals.rs の ApprovalKind)
/// scope    = "agent"         # 適用範囲: "global" | "agent" | "session" | "path"
/// target   = "claude"        # scope の対象 (global は空)
/// decision = "allow_always"  # "ask" | "allow_once" | "allow_always" | "deny_always"
/// ```
///
/// 値はすべて**安定 ID (英小文字)** で持つ。列挙型を直接 serde に載せず
/// 文字列にしているのは、将来 kind が増えても古い Zaivern が
/// 「未知の行」として読み飛ばせるようにするため
/// ([`approval_policies_from_config`] が未知の値の行を捨てる)。
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct ApprovalPolicy {
    /// 承認の種別 ID (`file_read` / `file_write` / `file_delete` /
    /// `shell_command` / `network_access` / `git_operation` /
    /// `package_install` / `privilege` / `other`)。
    pub kind: String,
    /// 適用範囲 (`global` / `agent` / `session` / `path`)。省略で `global`。
    pub scope: String,
    /// `scope` の対象値。`agent` なら実行ファイル名、`session` なら数値 ID、
    /// `path` ならパス接頭辞。`global` では空。
    pub target: String,
    /// 判断 (`ask` / `allow_once` / `allow_always` / `deny_always`)。
    pub decision: String,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            kind: String::new(),
            scope: "global".into(),
            target: String::new(),
            decision: "ask".into(),
        }
    }
}

/// `[[approval_policies]]` を承認エンジンの [`crate::agents::approvals::Policy`] へ変換する。
///
/// **未知の種別 / 範囲 / 判断の行は黙って捨てる**。書き間違いや、
/// 新しい Zaivern が書いた行を古いバイナリで読んだときに、意図しない
/// 種別へ丸めて自動承認してしまう事故を防ぐため (「推測しない」の原則)。
pub fn approval_policies_from_config(cfg: &Config) -> Vec<crate::agents::approvals::Policy> {
    use crate::agents::approvals::{ApprovalKind, Decision, Policy, Scope};
    cfg.approval_policies
        .iter()
        .filter_map(|p| {
            Some(Policy {
                kind: ApprovalKind::from_id(p.kind.trim())?,
                scope: Scope::from_toml(p.scope.trim(), p.target.trim())?,
                decision: Decision::from_id(p.decision.trim())?,
            })
        })
        .collect()
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentPreset {
    pub name: String,
    /// Shell command line. Empty string launches a plain login shell.
    pub command: String,
    pub icon: String,
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
}

impl Default for AgentPreset {
    fn default() -> Self {
        Self {
            name: "Shell".into(),
            command: String::new(),
            icon: "🖥".into(),
            cwd: None,
            env: HashMap::new(),
        }
    }
}

/// 既定のプリセット一覧。
///
/// **エージェント固有の値をここへ直書きしない**: 並びは
/// [`agents::DEFAULT_PRESET_BINS`]、中身は [`agents::AGENT_CATALOG`] の
/// エントリから組み立てる(プリセットの作り方はピッカーで「追加」したときと
/// 完全に同じ経路 = `agent_picker::plain_preset` / `auto_preset`)。
/// カタログに CLI を足せば、ここを触らずに既定へ載せられる。
fn default_agents() -> Vec<AgentPreset> {
    let mut out = Vec::new();
    for bin in agents::DEFAULT_PRESET_BINS {
        // 別名でも引けるよう spec_for_bin を通す(bin が変わっても壊れない)。
        let Some(spec) = agents::spec_for_bin(bin) else {
            continue;
        };
        out.push(crate::agent_picker::plain_preset(spec));
        if let Some(auto) = crate::agent_picker::auto_preset(spec) {
            out.push(auto);
        }
    }
    // 素のログインシェルは常に最後(コマンド空 = シェル起動)。
    out.push(AgentPreset {
        name: "Shell".into(),
        command: String::new(),
        icon: "🖥".into(),
        ..Default::default()
    });
    out
}

/// Project-local overlay (<workspace>/.zaivern.toml): every field optional.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Overlay {
    theme: Option<String>,
    editor_font_size: Option<f32>,
    terminal_font_size: Option<f32>,
    ui_zoom: Option<f32>,
    show_hidden_files: Option<bool>,
    respect_gitignore: Option<bool>,
    dim_ignored_files: Option<bool>,
    index_max_files: Option<usize>,
    index_max_depth: Option<usize>,
    tree_dir_page: Option<usize>,
    word_wrap: Option<bool>,
    show_whitespace: Option<bool>,
    minimap: Option<bool>,
    breadcrumbs: Option<bool>,
    git_blame: Option<bool>,
    approval_mode: Option<String>,
    show_pet: Option<bool>,
    agents: Vec<AgentPreset>,
    keybindings: HashMap<String, String>,
    /// プロジェクト単位でプラグインを切る / 設定を上書きする。
    plugins: Option<PluginsConfig>,
}

/// UI 上での選択を保持する軽量ステート (~/.zaivern/state.toml)。
/// config.toml はユーザーのコメント付き手書きファイルなので上書きしない。
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct UiState {
    theme: Option<String>,
    approval_mode: Option<String>,
    show_pet: Option<bool>,
    word_wrap: Option<bool>,
    show_whitespace: Option<bool>,
    /// 画面全体のズーム倍率。⌘+ / ⌘- は UI からの操作なので、
    /// 手書きの config.toml ではなく state 側に覚える
    /// (config.toml はアプリが書き換えない方針)。
    ui_zoom: Option<f32>,
    minimap: Option<bool>,
    breadcrumbs: Option<bool>,
    /// 差分ビューの表示モード ("side_by_side" | "inline")。
    diff_view: Option<String>,
    git_blame: Option<bool>,
    /// タブ切替を MRU で回すか (パレットから切り替えるので state 側に置く)。
    tab_switch_mru: Option<bool>,
    /// プレビュータブを使うか (同上)。
    preview_tabs: Option<bool>,
    pet_image: Option<String>,
    pet_x: Option<f32>,
    pet_y: Option<f32>,
    pet_variant: Option<String>,
    pet_scale: Option<f32>,
    pet_free_roam: Option<bool>,
    pet_sleep: Option<bool>,
    pet_sounds: Option<bool>,
    pet_bubbles: Option<bool>,
    pet_auto_yes: Option<bool>,
    pet_approve_keys: Option<String>,
    pet_deny_keys: Option<String>,
    voice_engine: Option<String>,
    voice_target: Option<String>,
    voice_lang: Option<String>,
    voice_command: Option<String>,
    voice_keyword: Option<String>,
    /// SSH リモート接続の踏み台 (接続先だけ。鍵は保存しない)。
    ssh_tunnel_host: Option<String>,
    /// 監視役 LLM の選択。UI から選ぶものなので、手書きの config.toml ではなく
    /// state 側に置く (config.toml をアプリが書き換えない方針に合わせる)。
    super_agent_command: Option<String>,
    super_agent_session_title: Option<String>,
    super_agent_enabled: Option<bool>,
    super_agent_timeout_secs: Option<u64>,
    /// レート制限の自動フェイルオーバーの有効/無効。UI (パレット / Cockpit) から
    /// 切り替えるものなので、手書きの config.toml ではなく state 側に置く。
    /// 上限やクールダウンの数値は `[failover]` (config.toml) のまま。
    failover_enabled: Option<bool>,
    /// コマンドパレットの MRU。**配列のテーブルなので必ず最後の項目に置く**
    /// (TOML は値をテーブルより先に書く必要があり、途中に置くと
    /// `toml::to_string_pretty` が state.toml 全体を書けなくなる)。
    palette_recent: Option<Vec<PaletteRecent>>,
}

pub fn config_path() -> PathBuf {
    zaivern_dir().join("config.toml")
}

// 実体は save_state_to_dir() が dir から組むため、現在はテストからのみ参照。
#[allow(dead_code)]
pub fn state_path() -> PathBuf {
    zaivern_dir().join("state.toml")
}

/// `~/.zaivern` の場所。home が取れない場合は `./.zaivern` にフォールバック。
/// ディレクトリの作成 (create_dir_all) は行わない — 呼び出し側の責務。
pub(crate) fn zaivern_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".zaivern")
}

pub const DEFAULT_CONFIG: &str = r#"# ══════════════════════════════════════════════════
#  Zaivern Code 設定ファイル
#  場所: ~/.zaivern/config.toml
#  プロジェクトごとの上書き: <workspace>/.zaivern.toml
#  変更後はコマンドパレット (⌘⇧P) の「設定を再読み込み」で反映されます
# ══════════════════════════════════════════════════

# テーマ (ダーク): "zaivern-dark" | "zaivern-midnight" | "zaivern-nordic"
#                 | "zaivern-ember" | "zaivern-forest" | "zaivern-ocean" | "zaivern-carbon"
# テーマ (ライト): "zaivern-light" | "zaivern-paper" | "zaivern-daylight" | "zaivern-frost"
# カラーテーマJSON (VS Code 互換形式) へのフルパスも指定できます
# (~/.zaivern/themes とプラグイン同梱のテーマは 🎨 メニューに自動で並びます)
theme = "zaivern-dark"
editor_font_size = 15.0
terminal_font_size = 13.0
show_hidden_files = true

# .gitignore (+ .git/info/exclude + core.excludesFile) を尊重して
# ファイルツリーとファイル検索 (⌘P) から除外します。既定は true。
# false にすると node_modules / target なども全部並びます。
# respect_gitignore = true
# 無視されたファイルを隠さず「薄く」表示する (VS Code と同じ見せ方)。既定は false。
# respect_gitignore = false のときは効きません。
# dim_ignored_files = false

# ファイル検索 (⌘P) の索引の上限。上限に達したらパレットにその旨を出します
# (黙って切り捨てません)。索引はバックグラウンドで作るので UI は止まりません。
# index_max_files = 50000
# index_max_depth = 32

# ツリーの 1 フォルダで一度に描く行数。超えたぶんは「さらに N 件」に畳みます
# (巨大フォルダで数万行を描いてカクつかせないため)。
# tree_dir_page = 300

# 画面全体のズーム (0.5〜3.0)。UI の全部が一緒に拡大縮小します。
# ⌘+ / ⌘- / ⌘0 で変えた値は ~/.zaivern/state.toml に覚えるので、
# ここに書くのは「起動時の初期値を固定したい」ときだけで構いません。
# ファイル単位のズーム (⌘⌥+ / ⌘⌥- / ⌘⌥0) はタブごとの一時的な値で、保存しません。
# ui_zoom = 1.0

# エディタ本文の折り返しと空白文字 (·/→) の可視化
# (表示メニュー・コマンドパレットの「折り返し切替」「空白文字表示切替」でも変更できます)
# word_wrap = false
# show_whitespace = false

# カーソル下のシンボルと同じものを本文で薄くハイライトする (LSP documentHighlight)
# (コマンドパレットの「同一シンボルのハイライト切替」でも変更できます)
# lsp_highlight_occurrences = true

# 診断メッセージを本文の行末に淡色で出す (VS Code の Error Lens 相当)
# 出るのは**キャレット行だけ**です。オフにしても波線とホバーは残ります
# (コマンドパレットの「行末の診断メッセージ切替」でも変更できます)
# inline_diagnostics = true
# 取り消し (Undo) 履歴の粒度とメモリ上限。タブ 1 枚あたりの値です。
#   undo_merge_ms  = 続けて打った文字を 1 段にまとめる時間しきい値 (ミリ秒)。
#                    これを超えて間が空くと別の段になります。0 にすると 1 打鍵 = 1 段。
#   undo_max_steps = 保持する最大段数。超えたぶんは古い方から捨てます。
#   undo_max_bytes = 履歴が抱える差分の合計バイト上限。巨大な一括置換を
#                    繰り返してもここで頭打ちになります。
# undo_merge_ms = 400
# undo_max_steps = 400
# undo_max_bytes = 4194304

# ミニマップ (エディタ右端の遠景) とブレッドクラム (上部のパンくず)
# (表示メニュー・コマンドパレットの「ミニマップの表示切替」「ブレッドクラムの表示切替」でも変更できます)
# ミニマップは本文の幅を 64px 使うため既定はオフ。狭い画面では自動的に隠れます
# minimap = false
# breadcrumbs = true
# ガターに git blame (著者 · 相対日時) を出す。既定はオフ
# (表示メニュー・コマンドパレットの「Git blame の表示切替」でも変更できます)
# git_blame = false

# ── 保存時の整形 (VS Code の files.* / editor.formatOnSave 相当) ──
# どれも既定はオフ。保存しただけで差分が増えないようにするためで、
# コマンドパレットの「保存時に…」からも個別に切り替えられます。
# trim_trailing_whitespace = false   # 各行の末尾空白を落とす (全角スペースは残す)
# trim_final_newlines      = false   # 末尾の余分な空行を落とす
# insert_final_newline     = false   # 最終行に改行が無ければ足す
# format_on_save           = false   # LSP の整形をかけてから保存する

# ── 括弧の色分け / 縦のルーラー / インデント ──
# 括弧を入れ子の深さごとに色分けする (色はテーマの ANSI 表から採ります)
# bracket_colorization = true
# 縦のルーラーを引く桁 (等幅の桁数)。既定は空 = 1 本も引きません
# rulers = [80, 120]
# 開いたファイルの中身からインデントを推定する (オフなら下の 2 つをそのまま使う)
# detect_indentation = true
# tab_size = 4
# insert_spaces = true
# タブ切替 (Ctrl+Tab / Ctrl+Shift+Tab) を最近使った順 (MRU) で回すか。
# 既定はオン = 押している間に候補一覧が出て、離したところで確定します
# (2 回押せば直前のファイルへ戻る)。false にすると並び順の巡回になります。
# (コマンドパレットの「タブ切替を最近使った順/並び順にする」でも変更できます)
# tab_switch_mru = true
# プレビュータブ (斜体の使い捨てタブ)。既定はオン = ツリーやパレットから
# 1 回クリックして開いたタブは次のプレビューで置き換わるので増え続けません。
# もう一度クリック / 編集 / ピン留め / ドラッグで確定タブになります。
# (コマンドパレットの「プレビュータブの切替」でも変更できます)
# preview_tabs = true

# 既定の権限モード (claude / codex / agy に自動適用)
#   "ask"   = 毎回ユーザー承認が必要（安全・デフォルト）
#   "auto"  = すべて自動YES（各CLIの bypass フラグを自動付与）
#   "agent" = Agent欄優先（プリセットのコマンドに書かれたフラグをそのまま使う。
#             「(全自動)」プリセットと通常プリセットを使い分けたい場合はこれ）
# ツールバーの 🛡/⚡/👾 ボタンでも切替できます
approval_mode = "ask"

# フォルダを開き直したとき、前回のエージェントタブを復元して会話を再開する
# 既定は false — 起動しただけでは何も立ち上がりません。過去の会話は
# 「💬 セッション」タブから選んで再開します。
# true にすると前回のスクロールバックが見える状態で、claude は --continue /
# codex は resume --last 付きで起動します。
# restore_agents = false

# デスクトップペット (🐾) の表示
show_pet = true

# ── 外出先への通知 (Webhook) ──────────────
# 承認待ち・終了・レート制限のイベントを外部サービスへ POST します (curl 使用)。
# ntfy ならスマホアプリを入れてトピックを購読するだけでプッシュ通知になります。
# Slack / Discord の Incoming Webhook URL はドメインから自動判別して JSON で送ります。
# webhook_url = "https://ntfy.sh/あなたのトピック名"
# webhook_url = "https://hooks.slack.com/services/XXX/YYY/ZZZ"
# webhook_url = "https://discord.com/api/webhooks/XXX/YYY"

# ── ペットの好み設定 ──────────────
# pet_variant = "blocky"   # 見た目: "blocky" | "crab" | "cat" | "cloud"
# pet_scale = 1.0          # 大きさ: 0.75=小 / 1.0=中 / 1.4=大
# pet_free_roam = true     # うろうろ散歩
# pet_sleep = true         # 無操作で睡眠
# pet_sounds = true        # 効果音
# pet_bubbles = true       # 承認バブル
# pet_auto_yes = false    # 承認プロンプトへ自動でYES (オフ=ユーザー承認必須)
# pet_approve_keys = "\r"    # 承認時にPTYへ送るキー (Enter)
# pet_deny_keys = "\u001B"   # 拒否時にPTYへ送るキー (ESC)

# ── 自動YESの追加ルール ──────────────
# 自動YES は、CLI ごとの承認プロンプトの文言を Zaivern 内部の表と突き合わせて
# 「どのキーを送れば YES になるか」を決めています。CLI 側の更新で文言が変わると
# 表に当たらなくなり、自動YES が素通りします。そのときは Zaivern を入れ直さなくても
# ここへ自分のルールを足せば直せます (組み込みの表より先に評価されます)。
#
# 画面に pattern が含まれていたら reply のキー列を送ります。
#   reply = "\r"    Enter (矢印キー選択メニューの確定。既定)
#   reply = "y\r"   y と Enter ((y/n) 形式)
#   reply = "1"     番号キー (「1. Yes」形式で、カーソルが Yes に無いとき)
#   reply = "3\r"   番号 + Enter (「番号を入力してください」形式のメニュー)
#   agent = "agy"   そのエージェントのタブだけに効かせる (省略すると全部)
#                   agy=Antigravity / claude / codex / gemini … (実行ファイル名)
#
# [[auto_yes_rules]]
# pattern = "Allow access to this file?"
# reply = "\r"
# agent = "agy"
#
# [[auto_yes_rules]]
# pattern = "Continue with this plan?"
# reply = "y\r"
#
# 番号入力メニュー (アンケート等) の既定の選び方を上書きしたいとき。
# 組み込みは「スキップ肢 → 肯定肢 → (評点しか無ければ) 肯定側の端」の順に
# 選びますが、この行があればそちらが優先されます。
#
# [[auto_yes_rules]]
# pattern = "How would you rate"
# reply = "3\r"

# ── 承認ポリシー (統合承認キュー) ──────────────
# すべてのエージェントの承認要求を 1 本のキューへ集め、種別ごとに
# 「常に許可 / 常に拒否 / 毎回聞く」を決められます。全自動YES と違って
# **種別と範囲を絞れる**のと、判断が ~/.zaivern/approvals.jsonl に
# 追記されて後から監査できるのが違いです。
#
# kind     … file_read / file_write / file_delete / shell_command /
#            network_access / git_operation / package_install /
#            privilege / other
# scope    … global (全部) / agent (実行ファイル名) /
#            session (セッションID) / path (パス接頭辞)
# target   … scope の対象値 (global のときは不要)
# decision … ask / allow_once / allow_always / deny_always
#
# 具体的な範囲が優先されます: session > path > agent > global
# (path 同士は深い方が勝つ)。同じ具体性なら後に書いた方が勝ちます。
#
# ★ privilege (管理者権限の昇格) だけは allow_always を書いても効きません。
#   自動承認は決して行わず、必ず本人に聞きます。
#
# 例1: Claude のファイル読み取りは黙って通す
# [[approval_policies]]
# kind = "file_read"
# scope = "agent"
# target = "claude"
# decision = "allow_always"
#
# 例2: ネットワークアクセスはどのエージェントでも必ず拒否
# [[approval_policies]]
# kind = "network_access"
# scope = "global"
# decision = "deny_always"
#
# 例3: このフォルダ配下の書き込みだけ自動許可
# [[approval_policies]]
# kind = "file_write"
# scope = "path"
# target = "/Users/me/work/sandbox"
# decision = "allow_always"

# ── 音声入力 (🎤) ──────────────
# 🎤 を押すと録音が始まり、⏹ を押すまで話した内容がエージェントの入力欄へ
# 流れ込み続けます。Enter は送られないので、内容を確認して自分で Enter を
# 押すまで送信されません。Enter で入力欄が空になっても録音は続いたままなので、
# そのまま次の指示を話せます。ツールバーの 🎤 メニューからも変更できます。
#
# voice_engine = "auto"    # "auto" | "mac" | "powershell" | "browser" | "command" | "off"
# voice_target = "active"  # 届け先: "active"(アクティブなエージェント) | "broadcast"(全員)
# voice_lang = "ja-JP"     # 認識する言語
# voice_keyword = ""       # このキーワードを話すと Enter まで自動送信 ("" = 常に手動)
#
# "auto" は上から順に:
#   macOS                     → "mac"        内蔵の Swift ヘルパー
#   voice_command が設定済み  → "command"    下記の外部コマンド
#   Windows (対応言語あり)    → "powershell" Windows 標準の音声認識 (オフライン)
#   それ以外                  → "browser"    ブラウザの音声入力ページを開く
#
# "browser" はスマホリモートの /voice を 127.0.0.1 で開き、ブラウザの音声認識に
# 喋らせます。マイクはブラウザ側なので、ページを閉じれば止まります。
# Chrome が入っていれば Chrome で開きます (Edge の音声認識は不安定なため)。
#
# 独自の認識エンジンを使う場合は voice_command を設定します。標準出力へ 1 行ずつ
# 認識テキストを吐き、標準入力に "q" が来たら終了するコマンドを想定しています
# ({lang} は言語に置換)。auto のままでも、設定されていれば mac 以外では優先されます。
# voice_engine = "command"
# voice_command = "my-stt --lang {lang} --stream"

# ── AIエージェント / ターミナルのプリセット ──────────────
# command はログインシェル (-lc) 経由で実行されます。
# 空文字 "" は普通のシェルを起動します。
# env でプリセット固有の環境変数を設定できます。
# カタログ登録済みの CLI (claude / codex / gemini / agy / cursor-agent / droid …)
# で始まるコマンドには承認モードが自動適用されます
# (approval_mode = "agent" ならコマンドをそのまま尊重します)。
# ここに無い CLI も「エージェント追加」から選べます (対応 CLI は 30 種類以上)。

[[agents]]
name = "Claude Code"
icon = "👾"
command = "claude"

[[agents]]
name = "Claude Code (全自動)"
icon = "⚡"
command = "claude --dangerously-skip-permissions"

[[agents]]
name = "Codex"
icon = "💡"
command = "codex"

[[agents]]
name = "Codex (全自動)"
icon = "⚡"
command = "codex --dangerously-bypass-approvals-and-sandbox"

[[agents]]
name = "Gemini CLI"
icon = "✨"
command = "gemini"

[[agents]]
name = "Gemini CLI (全自動)"
icon = "⚡"
command = "gemini --yolo"

[[agents]]
name = "Antigravity"
icon = "🚀"
command = "agy"

[[agents]]
name = "Antigravity (全自動)"
icon = "⚡"
command = "agy --dangerously-skip-permissions"

[[agents]]
name = "Cursor"
icon = "🖱"
command = "cursor-agent"

[[agents]]
name = "Cursor (全自動)"
icon = "⚡"
command = "cursor-agent -f"

[[agents]]
name = "Droid"
icon = "👾"
command = "droid"

[[agents]]
name = "Droid (全自動)"
icon = "⚡"
command = "droid --skip-permissions-unsafe"

[[agents]]
name = "Shell"
icon = "🖥"
command = ""

# [[agents]]
# name = "Claude (Opus 明示)"
# icon = "💡"
# command = "claude --model claude-opus-4-8"
# env = { MAX_THINKING_TOKENS = "31999" }

# ── アカウント/プロファイル切替 ──────────────
# env に設定ディレクトリを指定すると、同じ CLI を別アカウント (別サブスク) で
# 並列起動できます。片方の制限に当たっても、もう片方はそのまま走り続けます。
# [[agents]]
# name = "Claude (仕事用アカウント)"
# icon = "🏢"
# command = "claude"
# env = { CLAUDE_CONFIG_DIR = "~/.claude-work" }
#
# [[agents]]
# name = "Codex (サブ垢)"
# icon = "🅾"
# command = "codex"
# env = { CODEX_HOME = "~/.codex-alt" }

# ── レート制限時の自動フェイルオーバー ──────────
# 上限に当たったら、同じ CLI の別プロファイル → 別 CLI の順で切替先を選び、
# 新しいセッションを立てて続きを渡します。上限に当たった側のセッションは
# **そのまま残します** (終了させません)。
# **既定は無効** — 有効にするのは ⌘⇧P →「自動フェイルオーバーを有効化」か、
# 📊 プラン使用量ウィンドウのチェックボックス、あるいは下の enabled = true。
# 上の [[agents]] に別プロファイルを 2 つ以上書いておかないと切替先がありません。
# [failover]
# enabled = false           # 自動で切り替えるか
# max_switches = 3          # 1 セッションあたりの連鎖切替の上限
# max_attempts = 2          # 同じ枠を試し直す上限
# cooldown_secs = 300       # 枯れた枠を寝かせる基準時間 (失敗のたびに倍)
# max_cooldown_secs = 3600  # クールダウンの上限
# verify_secs = 90          # 切替先が動いていると見なすまでの観察時間
# min_screen_hits = 2       # 画面由来の検知を信じるまでの連続一致回数

# [[agents]]
# name = "Gemini CLI"
# icon = "✨"
# command = "gemini"

# ── キーバインド上書き(例)──────────────
# [keybindings]
# save = "cmd+s"
# toggle_terminal = "ctrl+`"
# toggle_comment = "cmd+/"

# ── プラグイン ──────────────────────
# 標準プラグインは初回起動時に ~/.zaivern/plugins/ へ展開され、
# 何も書かなくてもすべて有効です。切りたいものだけここに並べます。
# [plugins]
# disabled = ["usage-meter"]

# プラグインごとの設定 (マニフェストの [[setting]] key に対応)
# [plugins.settings.worktrees]
# parallel_count = "3"
#
# [plugins.settings.remote-host]
# host = "user@example.com"
# remote_path = "/home/user/work"
"#;

/// Write the default config template if none exists yet.
pub fn ensure_default() {
    let path = config_path();
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, DEFAULT_CONFIG);
    }
}

/// Load global config merged with each root's project overlay.
/// `with_state`: UI 選択 (state.toml) を最後に適用するか。
/// 起動時は true、「設定を再読み込み」では false (config.toml を正とする)。
///
/// マルチルート時のマージ規則: `roots` の順に `<root>/.zaivern.toml` を適用する。
/// つまり **後のルートが前のルートを上書きする (last wins)**。
/// これは「後から追加したフォルダの設定が効く」という直感に沿い、また
/// 単一ルート時の挙動と完全に一致する。
/// ただし `agents` は上書きではなく順に追加、`keybindings` はキー単位で
/// 上書きマージ (last wins) — いずれも従来の単一ルート時の規則そのまま。
pub fn load(roots: &[PathBuf], with_state: bool) -> Config {
    ensure_default();
    load_from_dir(&zaivern_dir(), roots, with_state)
}

/// `load()` の実体。テストから一時ディレクトリを差し込めるよう分離している。
fn load_from_dir(dir: &Path, roots: &[PathBuf], with_state: bool) -> Config {
    let mut cfg: Config = match std::fs::read_to_string(dir.join("config.toml")) {
        Ok(s) => match toml::from_str(&s) {
            Ok(c) => c,
            Err(e) => {
                // 手書きの 1 文字ミスで全設定 (自作プリセット・キーバインド等)
                // が黙って既定値に戻ると「全部消えた」ように見える。
                // 原因を stderr に出し、復旧用に .broken へ控えてから既定値で
                // 起動する (以後の保存で壊れたファイルが上書きされても戻せる)。
                eprintln!("zaivern: config.toml のパースに失敗: {e}");
                let _ = std::fs::copy(dir.join("config.toml"), dir.join("config.toml.broken"));
                eprintln!("zaivern: 壊れた config.toml を config.toml.broken に控えました");
                Config::default()
            }
        },
        Err(_) => Config::default(),
    };

    if cfg.agents.is_empty() {
        cfg.agents = default_agents();
    }

    if with_state {
        if let Ok(s) = std::fs::read_to_string(dir.join("state.toml")) {
            if let Ok(st) = toml::from_str::<UiState>(&s) {
                if let Some(t) = st.theme {
                    cfg.theme = t;
                }
                if let Some(a) = st.approval_mode {
                    cfg.approval_mode = a;
                }
                if let Some(p) = st.show_pet {
                    cfg.show_pet = p;
                }
                if let Some(v) = st.word_wrap {
                    cfg.word_wrap = v;
                }
                if let Some(v) = st.show_whitespace {
                    cfg.show_whitespace = v;
                }
                if let Some(v) = st.ui_zoom {
                    cfg.ui_zoom = v;
                }
                if let Some(v) = st.minimap {
                    cfg.minimap = v;
                }
                if let Some(v) = st.breadcrumbs {
                    cfg.breadcrumbs = v;
                }
                if let Some(v) = st.diff_view {
                    cfg.diff_view = v;
                }
                if let Some(v) = st.git_blame {
                    cfg.git_blame = v;
                }
                if let Some(v) = st.tab_switch_mru {
                    cfg.tab_switch_mru = v;
                }
                if let Some(v) = st.preview_tabs {
                    cfg.preview_tabs = v;
                }
                if st.pet_image.is_some() {
                    cfg.pet_image = st.pet_image;
                }
                if st.pet_x.is_some() {
                    cfg.pet_x = st.pet_x;
                }
                if st.pet_y.is_some() {
                    cfg.pet_y = st.pet_y;
                }
                if let Some(v) = st.pet_variant {
                    cfg.pet_variant = v;
                }
                if let Some(v) = st.pet_scale {
                    cfg.pet_scale = v;
                }
                if let Some(v) = st.pet_free_roam {
                    cfg.pet_free_roam = v;
                }
                if let Some(v) = st.pet_sleep {
                    cfg.pet_sleep = v;
                }
                if let Some(v) = st.pet_sounds {
                    cfg.pet_sounds = v;
                }
                if let Some(v) = st.pet_bubbles {
                    cfg.pet_bubbles = v;
                }
                if let Some(v) = st.pet_auto_yes {
                    cfg.pet_auto_yes = v;
                }
                if let Some(v) = st.pet_approve_keys {
                    cfg.pet_approve_keys = v;
                }
                if let Some(v) = st.pet_deny_keys {
                    cfg.pet_deny_keys = v;
                }
                if let Some(v) = st.voice_engine {
                    cfg.voice_engine = v;
                }
                if let Some(v) = st.voice_target {
                    cfg.voice_target = v;
                }
                if let Some(v) = st.voice_lang {
                    cfg.voice_lang = v;
                }
                if let Some(v) = st.voice_command {
                    cfg.voice_command = v;
                }
                if let Some(v) = st.voice_keyword {
                    cfg.voice_keyword = v;
                }
                if let Some(v) = st.ssh_tunnel_host {
                    cfg.ssh_tunnel_host = v;
                }
                if let Some(v) = st.super_agent_command {
                    cfg.super_agent.command = v;
                }
                if let Some(v) = st.super_agent_session_title {
                    cfg.super_agent.session_title = v;
                }
                if let Some(v) = st.super_agent_enabled {
                    cfg.super_agent.enabled = v;
                }
                if let Some(v) = st.super_agent_timeout_secs {
                    cfg.super_agent.timeout_secs = v;
                }
                if let Some(v) = st.palette_recent {
                    cfg.palette_recent = v;
                }
                if let Some(v) = st.failover_enabled {
                    cfg.failover.enabled = v;
                }
            }
        }
    }

    // overlay を重ねる前のグローバル値を控える。save_state はこの控えを書く。
    cfg.global_theme = cfg.theme.clone();
    cfg.global_approval_mode = cfg.approval_mode.clone();
    cfg.global_show_pet = cfg.show_pet;
    cfg.global_word_wrap = cfg.word_wrap;
    cfg.global_show_whitespace = cfg.show_whitespace;
    cfg.global_ui_zoom = crate::zoom::clamp(cfg.ui_zoom);
    cfg.global_minimap = cfg.minimap;
    cfg.global_breadcrumbs = cfg.breadcrumbs;
    cfg.global_git_blame = cfg.git_blame;
    cfg.global_plugins = cfg.plugins.clone();

    for root in roots {
        apply_overlay(&mut cfg, root);
    }

    if cfg.approval_mode != "auto" && cfg.approval_mode != "agent" {
        cfg.approval_mode = "ask".into();
    }
    if cfg.global_approval_mode != "auto" && cfg.global_approval_mode != "agent" {
        cfg.global_approval_mode = "ask".into();
    }
    cfg.editor_font_size = cfg.editor_font_size.clamp(8.0, 32.0);
    cfg.terminal_font_size = cfg.terminal_font_size.clamp(7.0, 28.0);
    // ここを外すと `ui_zoom = 0` で UI が 1 ピクセルに潰れて操作不能になる
    // (設定を戻す口も潰れるので、必ず範囲へ収めてから返す)。
    cfg.ui_zoom = crate::zoom::clamp(cfg.ui_zoom);
    cfg.pet_scale = cfg.pet_scale.clamp(0.5, 2.0);
    // 期限が 0 だと診断側で毎回丸められて分かりにくいので、ここで下限を揃える。
    cfg.super_agent.timeout_secs = cfg.super_agent.timeout_secs.clamp(5, 600);
    // LLM 相談の ON/OFF は「監視役が選ばれているか」から導く。
    // `[supervisor] llm_escalation` を単独で立てても、相談相手が居なければ
    // 何も起きない (request_diagnosis が no-op になる) ため、UI の見え方と
    // 実挙動がずれないようここで一本化する。
    cfg.supervisor.llm_escalation = cfg.super_agent.active_command().is_some();
    // 自動YESのユーザー定義ルールを応答エンジンへ渡す。
    // ここで配るので、設定を読み直せば再起動なしで反映される。
    publish_auto_yes_rules(&cfg);
    // 承認ポリシーと承認/拒否キーも同じタイミングで配る。
    publish_approval_policies(&cfg);
    cfg
}

/// `[[auto_yes_rules]]` を自動YESの応答エンジン (src/agents.rs) へ登録する。
pub fn publish_auto_yes_rules(cfg: &Config) {
    let rules: Vec<(String, String, String)> = cfg
        .auto_yes_rules
        .iter()
        .map(|r| (r.pattern.clone(), r.reply.clone(), r.agent.clone()))
        .collect();
    crate::agents::set_user_prompt_rules(&rules);
}

/// `[[approval_policies]]` と承認/拒否キーを統合承認キュー
/// (src/approvals.rs) へ登録する。設定を読み直せば再起動なしで反映される。
pub fn publish_approval_policies(cfg: &Config) {
    crate::agents::approvals::set_policies(approval_policies_from_config(cfg));
    crate::agents::approvals::set_reply_keys(&cfg.pet_approve_keys, &cfg.pet_deny_keys);
}

/// `<root>/.zaivern.toml` を 1 枚 `cfg` に重ねる。無ければ何もしない。
fn apply_overlay(cfg: &mut Config, root: &Path) {
    let overlay_path = root.join(".zaivern.toml");
    if let Ok(s) = std::fs::read_to_string(&overlay_path) {
        if let Ok(o) = toml::from_str::<Overlay>(&s) {
            if let Some(t) = o.theme {
                cfg.theme = t;
            }
            if let Some(v) = o.editor_font_size {
                cfg.editor_font_size = v;
            }
            if let Some(v) = o.terminal_font_size {
                cfg.terminal_font_size = v;
            }
            if let Some(v) = o.ui_zoom {
                cfg.ui_zoom = v;
            }
            if let Some(v) = o.show_hidden_files {
                cfg.show_hidden_files = v;
            }
            if let Some(v) = o.respect_gitignore {
                cfg.respect_gitignore = v;
            }
            if let Some(v) = o.dim_ignored_files {
                cfg.dim_ignored_files = v;
            }
            if let Some(v) = o.index_max_files {
                cfg.index_max_files = v;
            }
            if let Some(v) = o.index_max_depth {
                cfg.index_max_depth = v;
            }
            if let Some(v) = o.tree_dir_page {
                cfg.tree_dir_page = v;
            }
            if let Some(v) = o.word_wrap {
                cfg.word_wrap = v;
            }
            if let Some(v) = o.show_whitespace {
                cfg.show_whitespace = v;
            }
            if let Some(v) = o.minimap {
                cfg.minimap = v;
            }
            if let Some(v) = o.breadcrumbs {
                cfg.breadcrumbs = v;
            }
            if let Some(v) = o.git_blame {
                cfg.git_blame = v;
            }
            if let Some(v) = o.approval_mode {
                cfg.approval_mode = v;
            }
            if let Some(v) = o.show_pet {
                cfg.show_pet = v;
            }
            cfg.agents.extend(o.agents);
            // extend ではなくキー単位の上書きマージ
            for (k, v) in o.keybindings {
                cfg.keybindings.insert(k, v);
            }
            if let Some(p) = o.plugins {
                // 無効リストは追記 (プロジェクト側で追加で切れる)
                for name in p.disabled {
                    cfg.plugins.set_enabled(&name, false);
                }
                for (plugin, kv) in p.settings {
                    for (k, v) in kv {
                        cfg.plugins.set_setting(&plugin, &k, &v);
                    }
                }
            }
        }
    }
}

/// config.toml の `[plugins]` 区画だけを現在の設定で書き直す。
///
/// プラグインの有効/無効と設定値は「config.toml が唯一の正」とする。
/// state.toml と二重管理にすると、ユーザーが config.toml を編集しても
/// 効かない状況が生まれて混乱するため。
///
/// `[plugins]` と `[plugins.settings.*]` 以外の行は 1 行も触らないので、
/// ユーザーのコメントや並び順は保たれる (区画内のコメントは失われる)。
pub fn save_plugins_section(cfg: &Config) -> Result<(), String> {
    // セッション中の値 (overlay 適用済み) ではなくグローバルの控えを書く。
    // UI から変更したときは呼び出し側が控えも更新している。
    save_plugins_config(&cfg.global_plugins)
}

/// `[[agents]]` ブロック 1 件分の TOML テキストを作る。
///
/// 手で組み立てずに toml クレートへ通すのは、名前やコマンドに `"` や `\` が
/// 入っていても壊れた config.toml を書かないため。
/// env はインラインテーブルにする。追記位置に関係なく 1 行で閉じるので、
/// 後からさらに `[[agents]]` を足しても前のブロックに吸われる事故が起きない。
fn render_agent_preset(p: &AgentPreset) -> String {
    let mut s = String::from("\n[[agents]]\n");
    let kv = |k: &str, v: &str| format!("{k} = {}\n", toml::Value::String(v.to_string()));
    s.push_str(&kv("name", &p.name));
    s.push_str(&kv("icon", &p.icon));
    s.push_str(&kv("command", &p.command));
    if let Some(cwd) = &p.cwd {
        s.push_str(&kv("cwd", cwd));
    }
    if !p.env.is_empty() {
        // 並びを固定して、書き出しを決定的にする。
        let mut keys: Vec<&String> = p.env.keys().collect();
        keys.sort();
        let body: Vec<String> = keys
            .iter()
            .map(|k| {
                format!(
                    "{} = {}",
                    toml::Value::String((*k).clone()),
                    toml::Value::String(p.env[*k].clone())
                )
            })
            .collect();
        s.push_str(&format!("env = {{ {} }}\n", body.join(", ")));
    }
    s
}

/// config.toml の末尾に `[[agents]]` を 1 件書き足す。
///
/// 既存の行は 1 文字も触らない。カタログは「そこから足す元ネタ」であって
/// 利用者のプリセット一覧の置き換えではないので、手書きのコメントも並び順も
/// そのまま残さなければならない。
pub fn append_agent_preset(preset: &AgentPreset) -> Result<(), String> {
    let path = config_path();
    ensure_default();
    let mut raw = std::fs::read_to_string(&path).unwrap_or_default();
    if !raw.is_empty() && !raw.ends_with('\n') {
        raw.push('\n');
    }
    raw.push_str(&render_agent_preset(preset));
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, raw).map_err(|e| format!("config.toml を書けません: {e}"))
}

/// config.toml から `[plugins]` 区画だけを読む (GUI を起動せずに使える)。
pub fn load_plugins_config() -> PluginsConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| toml::from_str::<Config>(&s).ok())
        .map(|c| c.plugins)
        .unwrap_or_default()
}

/// `[plugins]` 区画だけを書き戻す。CLI と GUI の両方がここを通る。
pub fn save_plugins_config(plugins: &PluginsConfig) -> Result<(), String> {
    let path = config_path();
    ensure_default();
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = rewrite_plugins_section(&raw, plugins);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, updated).map_err(|e| format!("config.toml を書けません: {e}"))
}

/// 既存の `[plugins]` / `[plugins.settings.*]` 区画を取り除き、
/// 末尾に現在の内容を書き足した文字列を返す。
fn rewrite_plugins_section(raw: &str, plugins: &PluginsConfig) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skipping = false;

    for line in raw.lines() {
        let t = line.trim();
        // セクション見出しかどうか (コメント行は見出しではない)
        let is_header = t.starts_with('[') && t.ends_with(']');
        if is_header {
            let name = t.trim_start_matches('[').trim_end_matches(']');
            let name = name.trim_start_matches('[').trim_end_matches(']');
            skipping = name == "plugins" || name.starts_with("plugins.");
        }
        if !skipping {
            out.push(line);
        }
    }

    // 末尾の空行を整理してから追記する
    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    let mut text = out.join("\n");

    let block = render_plugins_section(plugins);
    if !block.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&block);
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// `[plugins]` 区画の本文を組み立てる (空設定なら空文字列)。
fn render_plugins_section(plugins: &PluginsConfig) -> String {
    let has_settings = plugins.settings.values().any(|kv| !kv.is_empty());
    if plugins.disabled.is_empty() && !has_settings {
        return String::new();
    }

    let quote = |s: &str| toml::Value::String(s.to_string()).to_string();
    // TOML の裸キーとして安全な形か。マニフェスト由来の名前は検証済みだが、
    // 手書き config.toml 由来の値 (空白や . 入り) を裸で書き戻すと
    // 不正な TOML になり、次回起動でファイル全体が読めなくなる。
    let bare_ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    let key_str = |s: &str| {
        if bare_ok(s) {
            s.to_string()
        } else {
            quote(s)
        }
    };

    let mut s = String::from("[plugins]\n");
    let items: Vec<String> = plugins.disabled.iter().map(|d| quote(d)).collect();
    s.push_str(&format!("disabled = [{}]\n", items.join(", ")));

    // HashMap の順序は不定なので、書くたびに差分が出ないよう名前で並べる
    let mut names: Vec<&String> = plugins.settings.keys().collect();
    names.sort();
    for name in names {
        let kv = &plugins.settings[name];
        if kv.is_empty() {
            continue;
        }
        s.push_str(&format!("\n[plugins.settings.{}]\n", key_str(name)));
        let mut keys: Vec<&String> = kv.keys().collect();
        keys.sort();
        for k in keys {
            s.push_str(&format!("{} = {}\n", key_str(k), quote(&kv[k])));
        }
    }
    s
}

/// Persist the current UI choices (theme / approval mode / pet) without
/// touching the user's hand-written config.toml.
///
/// theme / approval_mode / show_pet はプロジェクト overlay で上書きされ得るため、
/// セッション中の値ではなく `global_*` の控えを書く。UI から変更したときは
/// 呼び出し側が控えも更新するので、本当の変更はちゃんと永続化される。
pub fn save_state(cfg: &Config) {
    save_state_to_dir(&zaivern_dir(), cfg);
}

/// `save_state()` の実体。テストから一時ディレクトリを差し込めるよう分離している。
fn save_state_to_dir(dir: &Path, cfg: &Config) {
    let st = UiState {
        theme: Some(cfg.global_theme.clone()),
        approval_mode: Some(cfg.global_approval_mode.clone()),
        show_pet: Some(cfg.global_show_pet),
        word_wrap: Some(cfg.global_word_wrap),
        show_whitespace: Some(cfg.global_show_whitespace),
        ui_zoom: Some(crate::zoom::clamp(cfg.global_ui_zoom)),
        minimap: Some(cfg.global_minimap),
        breadcrumbs: Some(cfg.global_breadcrumbs),
        diff_view: Some(cfg.diff_view.clone()),
        git_blame: Some(cfg.global_git_blame),
        tab_switch_mru: Some(cfg.tab_switch_mru),
        preview_tabs: Some(cfg.preview_tabs),
        pet_image: cfg.pet_image.clone(),
        pet_x: cfg.pet_x,
        pet_y: cfg.pet_y,
        pet_variant: Some(cfg.pet_variant.clone()),
        pet_scale: Some(cfg.pet_scale),
        pet_free_roam: Some(cfg.pet_free_roam),
        pet_sleep: Some(cfg.pet_sleep),
        pet_sounds: Some(cfg.pet_sounds),
        pet_bubbles: Some(cfg.pet_bubbles),
        pet_auto_yes: Some(cfg.pet_auto_yes),
        pet_approve_keys: Some(cfg.pet_approve_keys.clone()),
        pet_deny_keys: Some(cfg.pet_deny_keys.clone()),
        voice_engine: Some(cfg.voice_engine.clone()),
        voice_target: Some(cfg.voice_target.clone()),
        voice_lang: Some(cfg.voice_lang.clone()),
        voice_command: Some(cfg.voice_command.clone()),
        voice_keyword: Some(cfg.voice_keyword.clone()),
        ssh_tunnel_host: Some(cfg.ssh_tunnel_host.clone()),
        super_agent_command: Some(cfg.super_agent.command.clone()),
        super_agent_session_title: Some(cfg.super_agent.session_title.clone()),
        super_agent_enabled: Some(cfg.super_agent.enabled),
        super_agent_timeout_secs: Some(cfg.super_agent.timeout_secs),
        failover_enabled: Some(cfg.failover.enabled),
        palette_recent: Some(cfg.palette_recent.clone()),
    };
    if let Ok(s) = toml::to_string_pretty(&st) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(dir.join("state.toml"), s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // load() / ensure_default() / save_state() は実ユーザーの ~/.zaivern を
    // 読み書きするためテストしない。実体の load_from_dir() / save_state_to_dir()
    // は state_overlay_tests で一時ディレクトリを差し込んで検証する。

    // ---- Config / AgentPreset の既定値 ----

    #[test]
    fn default_config_has_expected_values() {
        let c = Config::default();
        assert_eq!(c.theme, "zaivern-dark");
        assert_eq!(c.editor_font_size, 15.0);
        assert_eq!(c.terminal_font_size, 13.0);
        assert!(c.show_hidden_files);
        assert_eq!(c.approval_mode, "ask", "既定は必ず安全側 (ask)");
        assert!(c.show_pet);
        assert_eq!(c.pet_image, None);
        assert_eq!(c.pet_x, None);
        assert_eq!(c.pet_y, None);
        assert_eq!(c.pet_variant, "blocky");
        assert_eq!(c.pet_scale, 1.0);
        assert!(c.pet_free_roam);
        assert!(c.pet_sleep);
        assert!(c.pet_sounds);
        assert!(c.pet_bubbles);
        assert!(!c.pet_auto_yes, "自動YESは既定でオフ (ユーザー承認必須)");
        assert_eq!(c.pet_approve_keys, "\r", "承認は Enter");
        assert_eq!(c.pet_deny_keys, "\u{1b}", "拒否は ESC");
        assert_eq!(c.voice_engine, "auto");
        assert_eq!(c.voice_target, "active");
        assert_eq!(c.voice_lang, "ja-JP");
        assert_eq!(c.voice_command, "");
        assert_eq!(c.voice_keyword, "", "空 = 常に手動 Enter");
        assert!(c.keybindings.is_empty());
        assert!(!c.agents.is_empty());
    }

    /// エディタ細部の既定値は **VS Code の既定に合わせる**。
    ///
    /// 保存時の整形は全部オフ (保存しただけで差分が増えないため)、
    /// 括弧の色分けとインデント推定はオン、ルーラーは 1 本も引かない。
    #[test]
    fn 保存時の整形とエディタ表示の既定はvscodeに合わせる() {
        let c = Config::default();
        assert!(!c.trim_trailing_whitespace, "files.trimTrailingWhitespace");
        assert!(!c.trim_final_newlines, "files.trimFinalNewlines");
        assert!(!c.insert_final_newline, "files.insertFinalNewline");
        assert!(!c.format_on_save, "editor.formatOnSave");
        assert!(c.bracket_colorization, "editor.bracketPairColorization");
        assert!(c.rulers.is_empty(), "editor.rulers は既定で空");
        assert!(c.detect_indentation, "editor.detectIndentation");
        assert_eq!(c.tab_size, 4, "editor.tabSize");
        assert!(c.insert_spaces, "editor.insertSpaces");
        // 同梱テンプレートを読んでも同じ既定になる (コメントアウト済み)
        let t: Config = toml::from_str(DEFAULT_CONFIG).expect("同梱テンプレは常にパースできる");
        assert!(!t.trim_trailing_whitespace);
        assert!(!t.trim_final_newlines);
        assert!(!t.insert_final_newline);
        assert!(!t.format_on_save);
        assert!(t.bracket_colorization);
        assert!(t.rulers.is_empty());
        assert!(t.detect_indentation);
        assert_eq!(t.tab_size, 4);
        assert!(t.insert_spaces);
        // 実際に書けば読める (設定として機能する)
        let on: Config = toml::from_str(
            "trim_trailing_whitespace = true\n\
             trim_final_newlines = true\n\
             insert_final_newline = true\n\
             format_on_save = true\n\
             bracket_colorization = false\n\
             rulers = [80, 120]\n\
             detect_indentation = false\n\
             tab_size = 2\n\
             insert_spaces = false\n",
        )
        .expect("個別にオンオフできる");
        assert!(on.trim_trailing_whitespace);
        assert!(on.trim_final_newlines);
        assert!(on.insert_final_newline);
        assert!(on.format_on_save);
        assert!(!on.bracket_colorization);
        assert_eq!(on.rulers, vec![80, 120]);
        assert!(!on.detect_indentation);
        assert_eq!(on.tab_size, 2);
        assert!(!on.insert_spaces);
    }

    #[test]
    fn default_agent_preset_is_plain_shell() {
        let a = AgentPreset::default();
        assert_eq!(a.name, "Shell");
        assert_eq!(a.command, "", "空コマンド = ログインシェル");
        assert_eq!(a.icon, "🖥");
        assert_eq!(a.cwd, None);
        assert!(a.env.is_empty());
    }

    #[test]
    fn default_agents_cover_every_cli() {
        let agents = default_agents();
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Claude Code",
                "Claude Code (全自動)",
                "Codex",
                "Codex (全自動)",
                "Gemini CLI",
                "Gemini CLI (全自動)",
                "Antigravity",
                "Antigravity (全自動)",
                "Cursor",
                "Cursor (全自動)",
                "Droid",
                "Droid (全自動)",
                "Shell",
            ]
        );
    }

    /// 既定プリセットは **カタログから導出** される。おすすめ表に足した CLI が
    /// そのまま既定へ載ること、素のシェルが必ず最後に来ることを固定する。
    #[test]
    fn default_agents_are_derived_from_the_catalog() {
        let agents = default_agents();
        for bin in agents::DEFAULT_PRESET_BINS {
            let spec = agents::spec_for_bin(bin).expect("おすすめ表の bin はカタログにある");
            assert!(
                agents.iter().any(|a| a.command == spec.launch_command()),
                "{bin} の素のプリセットが既定に無い"
            );
            if crate::agent_picker::auto_preset(spec).is_some() {
                assert!(
                    agents
                        .iter()
                        .any(|a| a.name
                            == format!("{}{}", spec.label, crate::agent_picker::AUTO_SUFFIX)),
                    "{bin} の全自動プリセットが既定に無い"
                );
            }
        }
        let last = agents.last().expect("空にはならない");
        assert_eq!(last.command, "", "最後は素のシェル");
    }

    #[test]
    fn default_agents_auto_presets_carry_bypass_flags() {
        let agents = default_agents();
        for a in &agents {
            if a.name.contains("全自動") {
                // フラグ名は CLI ごとに違うので、判定は agents.rs の表に任せる
                assert!(
                    agents::command_is_bypass(&a.command),
                    "{} が bypass と判定されない: {:?}",
                    a.name,
                    a.command
                );
            }
        }
        // 通常プリセットは素の起動コマンドのまま (bypass フラグが混ざらない)
        for a in agents.iter().filter(|a| !a.name.contains("全自動")) {
            assert!(
                !agents::command_is_bypass(&a.command),
                "{} に bypass フラグが混ざっている: {:?}",
                a.name,
                a.command
            );
        }
        assert_eq!(
            agents
                .iter()
                .filter(|a| !a.name.contains("全自動"))
                .map(|a| a.command.as_str())
                .collect::<Vec<_>>(),
            vec![
                "claude",
                "codex",
                "gemini",
                "agy",
                "cursor-agent",
                "droid",
                ""
            ]
        );
    }

    #[test]
    fn default_agents_all_have_icon_and_name() {
        for a in default_agents() {
            assert!(!a.name.is_empty(), "名前が空のプリセットがある");
            assert!(!a.icon.is_empty(), "{} のアイコンが空", a.name);
        }
    }

    // ---- [[agents]] の追記 ----

    #[test]
    fn rendered_agent_preset_parses_back_unchanged() {
        let mut env = HashMap::new();
        env.insert("GOOSE_MODE".to_string(), "auto".to_string());
        let p = AgentPreset {
            name: "Goose (全自動)".into(),
            command: "goose".into(),
            icon: "⚡".into(),
            cwd: None,
            env,
        };
        let text = render_agent_preset(&p);
        let back: Config = toml::from_str(&text).expect("追記したブロックは読み戻せる");
        let a = back.agents.last().expect("agents が空");
        assert_eq!(a.name, p.name);
        assert_eq!(a.command, p.command);
        assert_eq!(a.icon, p.icon);
        assert_eq!(a.env.get("GOOSE_MODE").map(String::as_str), Some("auto"));
    }

    #[test]
    fn rendered_agent_preset_escapes_quotes_and_backslashes() {
        let p = AgentPreset {
            name: "変な \"名前\"".into(),
            command: r#"foo --msg "a\b""#.into(),
            icon: "👾".into(),
            cwd: Some(r"C:\tmp".into()),
            env: HashMap::new(),
        };
        let text = render_agent_preset(&p);
        let back: Config = toml::from_str(&text).expect("引用符が入っても壊れない");
        let a = back.agents.last().unwrap();
        assert_eq!(a.name, p.name);
        assert_eq!(a.command, p.command);
        assert_eq!(a.cwd.as_deref(), Some(r"C:\tmp"));
    }

    #[test]
    fn appending_a_preset_keeps_every_existing_one() {
        // 既存の config.toml を書き換えない ＝ 追記後も元のプリセットが全部残る。
        let base = DEFAULT_CONFIG.to_string();
        let before: Config = toml::from_str(&base).unwrap();
        let p = AgentPreset {
            name: "Qwen Code".into(),
            command: "qwen".into(),
            icon: "🐉".into(),
            cwd: None,
            env: HashMap::new(),
        };
        let after_text = format!("{base}{}", render_agent_preset(&p));
        // 元の本文は 1 文字も変わっていない
        assert!(after_text.starts_with(&base));
        let after: Config = toml::from_str(&after_text).expect("追記後もパースできる");
        assert_eq!(after.agents.len(), before.agents.len() + 1);
        for (i, a) in before.agents.iter().enumerate() {
            assert_eq!(after.agents[i].name, a.name, "既存プリセットの順序が崩れた");
            assert_eq!(after.agents[i].command, a.command);
        }
        assert_eq!(after.agents.last().unwrap().command, "qwen");
    }

    #[test]
    fn appending_twice_does_not_swallow_the_previous_block() {
        // env をインラインテーブルにしている理由の回帰テスト。
        // ヘッダ形式 ([agents.env]) だと、次に足した [[agents]] との間で
        // 所属が壊れやすい。
        let mut env = HashMap::new();
        env.insert("A".to_string(), "1".to_string());
        let first = AgentPreset {
            name: "First".into(),
            command: "goose".into(),
            icon: "🐦".into(),
            cwd: None,
            env,
        };
        let second = AgentPreset {
            name: "Second".into(),
            command: "qwen".into(),
            icon: "🐉".into(),
            cwd: None,
            env: HashMap::new(),
        };
        let text = format!(
            "{}{}{}",
            DEFAULT_CONFIG,
            render_agent_preset(&first),
            render_agent_preset(&second)
        );
        let cfg: Config = toml::from_str(&text).expect("2 回追記してもパースできる");
        let names: Vec<&str> = cfg.agents.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"First") && names.contains(&"Second"));
        let f = cfg.agents.iter().find(|a| a.name == "First").unwrap();
        assert_eq!(f.env.get("A").map(String::as_str), Some("1"));
        let s = cfg.agents.iter().find(|a| a.name == "Second").unwrap();
        assert!(s.env.is_empty(), "後続ブロックが前の env を吸い込んだ");
    }

    // ---- DEFAULT_CONFIG テンプレート ----

    #[test]
    fn default_config_template_parses_into_config() {
        let c: Config = toml::from_str(DEFAULT_CONFIG).expect("同梱テンプレは常にパースできる");
        assert_eq!(c.theme, "zaivern-dark");
        assert_eq!(c.editor_font_size, 15.0);
        assert_eq!(c.terminal_font_size, 13.0);
        assert!(c.show_hidden_files);
        assert_eq!(c.approval_mode, "ask");
        assert!(c.show_pet);
        // コメントアウトされている項目は Default から埋まる
        assert_eq!(c.voice_engine, "auto");
        assert_eq!(c.pet_variant, "blocky");
        assert!(c.keybindings.is_empty(), "keybindings 例はコメントアウト");
    }

    #[test]
    fn default_config_template_agents_match_default_agents() {
        let c: Config = toml::from_str(DEFAULT_CONFIG).expect("parse ok");
        let from_template: Vec<(&str, &str)> = c
            .agents
            .iter()
            .map(|a| (a.name.as_str(), a.command.as_str()))
            .collect();
        let builtin = default_agents();
        let from_code: Vec<(&str, &str)> = builtin
            .iter()
            .map(|a| (a.name.as_str(), a.command.as_str()))
            .collect();
        assert_eq!(
            from_template, from_code,
            "テンプレートと default_agents() がずれている"
        );
    }

    /// **既存ユーザーの config.toml を壊さない**こと。
    ///
    /// 新しい CLI を既定プリセットへ足しても、[[agents]] を書いている設定は
    /// その内容だけが残る (新既定が勝手に混ざらない)。逆に [[agents]] が
    /// 1 つも無い古い設定では、新しい既定一式が入る。
    #[test]
    fn old_config_without_new_presets_still_loads() {
        // 新プリセット導入前の設定 (claude / codex / agy の 3 件だけ)
        let old = r#"
theme = "zaivern-dark"
editor_font_size = 15.0

[[agents]]
name = "Claude Code"
icon = "👾"
command = "claude"

[[agents]]
name = "Codex"
icon = "💡"
command = "codex"

[[agents]]
name = "Antigravity"
icon = "🚀"
command = "agy"
"#;
        let cfg: Config = toml::from_str(old).expect("旧設定が読めなくなった");
        assert_eq!(cfg.theme, "zaivern-dark");
        assert_eq!(cfg.agents.len(), 3, "旧設定のプリセットに新既定が混ざった");
        assert_eq!(cfg.agents[0].command, "claude");
        assert_eq!(cfg.agents[2].command, "agy");
        // 旧設定に無い新フィールドは既定値で埋まる
        assert!(cfg
            .agents
            .iter()
            .all(|a| a.cwd.is_none() && a.env.is_empty()));

        // [[agents]] を書いていない古い設定は、新しい既定一式を受け取る
        let bare: Config = toml::from_str("theme = \"zaivern-dark\"").expect("読める");
        assert_eq!(bare.agents.len(), default_agents().len());
        assert!(
            bare.agents.iter().any(|a| a.command == "gemini"),
            "新しく足した CLI が既定に出てこない"
        );

        // 新しい既定はそのまま書き出して読み戻せる (往復で欠けない)
        for p in default_agents() {
            let text = render_agent_preset(&p);
            let back: Config = toml::from_str(&text).expect("往復で壊れた");
            assert_eq!(back.agents.len(), 1, "{}", p.name);
            assert_eq!(back.agents[0].name, p.name);
            assert_eq!(back.agents[0].command, p.command);
            assert_eq!(back.agents[0].icon, p.icon);
            assert_eq!(back.agents[0].env, p.env);
        }
    }

    // ---- Config のデシリアライズ (正常系) ----

    #[test]
    fn config_from_empty_toml_equals_defaults() {
        let c: Config = toml::from_str("").expect("空 TOML は既定値");
        let d = Config::default();
        assert_eq!(c.theme, d.theme);
        assert_eq!(c.approval_mode, d.approval_mode);
        assert_eq!(c.editor_font_size, d.editor_font_size);
        assert_eq!(c.agents.len(), d.agents.len(), "agents も既定が入る");
    }

    #[test]
    fn config_partial_toml_keeps_other_defaults() {
        let c: Config = toml::from_str("theme = \"zaivern-light\"\n").expect("parse ok");
        assert_eq!(c.theme, "zaivern-light");
        assert_eq!(c.approval_mode, "ask", "書かれていない項目は既定のまま");
        assert_eq!(c.terminal_font_size, 13.0);
    }

    #[test]
    fn config_ignores_unknown_fields() {
        // deny_unknown_fields を付けていないので、将来削除された項目が
        // 残っていても設定全体が壊れない
        let c: Config =
            toml::from_str("theme = \"x\"\nlegacy_option = 42\n").expect("未知のキーは無視される");
        assert_eq!(c.theme, "x");
    }

    #[test]
    fn config_accepts_optional_pet_position() {
        let c: Config = toml::from_str("pet_x = 12.5\npet_y = -3.0\npet_image = \"/tmp/p.png\"\n")
            .expect("parse ok");
        assert_eq!(c.pet_x, Some(12.5));
        assert_eq!(c.pet_y, Some(-3.0));
        assert_eq!(c.pet_image, Some("/tmp/p.png".to_string()));
    }

    #[test]
    fn config_parses_keybindings_table() {
        let c: Config = toml::from_str("[keybindings]\nsave = \"cmd+s\"\n").expect("parse ok");
        assert_eq!(c.keybindings.get("save").map(String::as_str), Some("cmd+s"));
        assert_eq!(c.keybindings.len(), 1);
    }

    #[test]
    fn agent_preset_parses_env_and_cwd() {
        let c: Config = toml::from_str(
            "[[agents]]\nname = \"X\"\ncommand = \"x --go\"\ncwd = \"/tmp\"\nenv = { A = \"1\" }\n",
        )
        .expect("parse ok");
        assert_eq!(c.agents.len(), 1, "書かれた agents が既定を置き換える");
        let a = &c.agents[0];
        assert_eq!(a.name, "X");
        assert_eq!(a.command, "x --go");
        assert_eq!(a.cwd, Some("/tmp".to_string()));
        assert_eq!(a.env.get("A").map(String::as_str), Some("1"));
        assert_eq!(a.icon, "🖥", "icon 省略時は既定アイコン");
    }

    #[test]
    fn agent_preset_allows_all_fields_omitted() {
        let c: Config = toml::from_str("[[agents]]\n").expect("空の agents 要素も既定で埋まる");
        assert_eq!(c.agents.len(), 1);
        assert_eq!(c.agents[0].name, "Shell");
        assert_eq!(c.agents[0].command, "");
    }

    // ---- Config のデシリアライズ (境界値・異常系) ----

    #[test]
    fn config_empty_strings_survive_parsing() {
        // load() 側で正規化されるので、パース段階では空文字がそのまま通る
        let c: Config = toml::from_str("theme = \"\"\napproval_mode = \"\"\nvoice_lang = \"\"\n")
            .expect("parse ok");
        assert_eq!(c.theme, "");
        assert_eq!(c.approval_mode, "");
        assert_eq!(c.voice_lang, "");
    }

    #[test]
    fn config_extreme_font_sizes_parse_unclamped() {
        // clamp は load() の中でのみ行われる (パース自体は素通し)
        let c: Config = toml::from_str("editor_font_size = 999.0\nterminal_font_size = -5.0\n")
            .expect("parse ok");
        assert_eq!(c.editor_font_size, 999.0);
        assert_eq!(c.terminal_font_size, -5.0);
        assert_eq!(c.editor_font_size.clamp(8.0, 32.0), 32.0);
        assert_eq!(c.terminal_font_size.clamp(7.0, 28.0), 7.0);
    }

    #[test]
    fn config_pet_scale_clamp_boundaries() {
        let c: Config = toml::from_str("pet_scale = 0.0\n").expect("parse ok");
        assert_eq!(c.pet_scale.clamp(0.5, 2.0), 0.5);
        let c: Config = toml::from_str("pet_scale = 5.0\n").expect("parse ok");
        assert_eq!(c.pet_scale.clamp(0.5, 2.0), 2.0);
        let c: Config = toml::from_str("pet_scale = 1.4\n").expect("parse ok");
        assert_eq!(c.pet_scale.clamp(0.5, 2.0), 1.4, "範囲内はそのまま");
    }

    #[test]
    fn config_empty_agents_list_parses_as_empty() {
        // load() は空なら default_agents() を入れ直す
        let c: Config = toml::from_str("agents = []\n").expect("parse ok");
        assert!(c.agents.is_empty());
    }

    #[test]
    fn config_rejects_malformed_toml() {
        assert!(toml::from_str::<Config>("theme = ").is_err(), "値が無い");
        assert!(
            toml::from_str::<Config>("[[agents\n").is_err(),
            "括弧が閉じていない"
        );
        assert!(toml::from_str::<Config>("= \"x\"\n").is_err(), "キーが無い");
    }

    #[test]
    fn config_rejects_wrong_field_types() {
        assert!(
            toml::from_str::<Config>("editor_font_size = \"big\"\n").is_err(),
            "f32 に文字列"
        );
        assert!(
            toml::from_str::<Config>("show_hidden_files = 3\n").is_err(),
            "bool に整数"
        );
        assert!(
            toml::from_str::<Config>("theme = true\n").is_err(),
            "String に真偽値"
        );
        assert!(
            toml::from_str::<Config>("agents = \"claude\"\n").is_err(),
            "配列に文字列"
        );
        assert!(
            toml::from_str::<Config>("keybindings = 1\n").is_err(),
            "テーブルに整数"
        );
    }

    // ---- Overlay (<workspace>/.zaivern.toml) ----

    #[test]
    fn overlay_empty_is_all_none() {
        let o: Overlay = toml::from_str("").expect("空でも成立する");
        assert_eq!(o.theme, None);
        assert_eq!(o.editor_font_size, None);
        assert_eq!(o.terminal_font_size, None);
        assert_eq!(o.show_hidden_files, None);
        assert_eq!(o.approval_mode, None);
        assert_eq!(o.show_pet, None);
        assert!(o.agents.is_empty(), "overlay の agents は既定を持たない");
        assert!(o.keybindings.is_empty());
    }

    #[test]
    fn overlay_parses_only_present_fields() {
        let o: Overlay =
            toml::from_str("theme = \"zaivern-midnight\"\nshow_pet = false\n").expect("parse ok");
        assert_eq!(o.theme, Some("zaivern-midnight".to_string()));
        assert_eq!(o.show_pet, Some(false));
        assert_eq!(o.approval_mode, None, "未指定はグローバル設定を残す");
        assert_eq!(o.editor_font_size, None);
    }

    #[test]
    fn overlay_agents_are_appended_not_replaced() {
        // load() は cfg.agents.extend(o.agents) するので、overlay 側は追加分だけ
        let o: Overlay =
            toml::from_str("[[agents]]\nname = \"Proj\"\ncommand = \"make\"\n").expect("parse ok");
        assert_eq!(o.agents.len(), 1);
        assert_eq!(o.agents[0].name, "Proj");

        let mut merged = default_agents();
        let before = merged.len();
        merged.extend(o.agents);
        assert_eq!(merged.len(), before + 1);
        assert_eq!(merged.last().map(|a| a.name.as_str()), Some("Proj"));
    }

    #[test]
    fn overlay_keybindings_merge_per_key() {
        let o: Overlay =
            toml::from_str("[keybindings]\nsave = \"ctrl+s\"\nrun = \"f5\"\n").expect("parse ok");
        let mut base: HashMap<String, String> = HashMap::new();
        base.insert("save".into(), "cmd+s".into());
        base.insert("quit".into(), "cmd+q".into());
        for (k, v) in o.keybindings {
            base.insert(k, v);
        }
        assert_eq!(
            base.get("save").map(String::as_str),
            Some("ctrl+s"),
            "上書き"
        );
        assert_eq!(base.get("run").map(String::as_str), Some("f5"), "追加");
        assert_eq!(base.get("quit").map(String::as_str), Some("cmd+q"), "温存");
        assert_eq!(base.len(), 3);
    }

    #[test]
    fn overlay_rejects_wrong_types_and_malformed_toml() {
        assert!(toml::from_str::<Overlay>("show_pet = \"yes\"\n").is_err());
        assert!(toml::from_str::<Overlay>("editor_font_size = \"big\"\n").is_err());
        assert!(toml::from_str::<Overlay>("theme = \n").is_err());
    }

    #[test]
    fn overlay_ignores_fields_it_does_not_own() {
        // pet_* や voice_* はプロジェクト overlay の対象外だが、書かれていても壊れない
        let o: Overlay = toml::from_str("theme = \"x\"\nvoice_lang = \"en-US\"\npet_scale = 2.0\n")
            .expect("未知キーは無視");
        assert_eq!(o.theme, Some("x".to_string()));
    }

    // ---- UiState (~/.zaivern/state.toml) ----

    #[test]
    fn ui_state_roundtrip_preserves_values() {
        let st = UiState {
            theme: Some("zaivern-light".into()),
            approval_mode: Some("auto".into()),
            ui_zoom: Some(1.25),
            show_pet: Some(false),
            word_wrap: Some(true),
            show_whitespace: Some(true),
            tab_switch_mru: Some(false),
            preview_tabs: Some(false),
            minimap: Some(true),
            breadcrumbs: Some(false),
            diff_view: Some("inline".into()),
            git_blame: Some(true),
            pet_image: Some("/tmp/p.png".into()),
            pet_x: Some(10.0),
            pet_y: Some(20.5),
            pet_variant: Some("cat".into()),
            pet_scale: Some(1.4),
            pet_free_roam: Some(false),
            pet_sleep: Some(false),
            pet_sounds: Some(true),
            pet_bubbles: Some(true),
            pet_auto_yes: Some(true),
            pet_approve_keys: Some("\r".into()),
            pet_deny_keys: Some("\u{1b}".into()),
            voice_engine: Some("command".into()),
            voice_target: Some("broadcast".into()),
            voice_lang: Some("en-US".into()),
            voice_command: Some("my-stt --lang {lang}".into()),
            voice_keyword: Some("送信".into()),
            ssh_tunnel_host: Some("user@bastion.example:2222".into()),
            super_agent_command: Some("claude".into()),
            super_agent_session_title: Some("Claude Code (全自動) #3".into()),
            super_agent_enabled: Some(true),
            super_agent_timeout_secs: Some(45),
            failover_enabled: Some(true),
            palette_recent: Some(vec![PaletteRecent {
                label: "保存".into(),
                icon: "💾".into(),
                uses: 3,
            }]),
        };
        let s = toml::to_string_pretty(&st).expect("UiState は TOML 化できる");
        let back: UiState = toml::from_str(&s).expect("読み戻せる");
        assert_eq!(back.theme, Some("zaivern-light".to_string()));
        assert_eq!(back.approval_mode, Some("auto".to_string()));
        assert_eq!(back.ui_zoom, Some(1.25));
        assert_eq!(back.show_pet, Some(false));
        assert_eq!(back.word_wrap, Some(true));
        assert_eq!(back.show_whitespace, Some(true));
        assert_eq!(back.minimap, Some(true));
        assert_eq!(back.breadcrumbs, Some(false));
        assert_eq!(back.git_blame, Some(true));
        assert_eq!(back.pet_image, Some("/tmp/p.png".to_string()));
        assert_eq!(back.pet_x, Some(10.0));
        assert_eq!(back.pet_y, Some(20.5));
        assert_eq!(back.pet_variant, Some("cat".to_string()));
        assert_eq!(back.pet_scale, Some(1.4));
        assert_eq!(back.pet_free_roam, Some(false));
        assert_eq!(back.pet_auto_yes, Some(true));
        assert_eq!(back.voice_keyword, Some("送信".to_string()));
        assert_eq!(
            back.ssh_tunnel_host,
            Some("user@bastion.example:2222".to_string()),
            "接続先は残す (鍵・パスフレーズは保存しない)"
        );
        // エスケープが必要な制御文字も往復する
        assert_eq!(back.pet_approve_keys, Some("\r".to_string()));
        assert_eq!(back.pet_deny_keys, Some("\u{1b}".to_string()));
        // 監視役 LLM の選択も state に残る (指名セッションのタイトル含む)
        assert_eq!(back.super_agent_command, Some("claude".to_string()));
        assert_eq!(
            back.super_agent_session_title,
            Some("Claude Code (全自動) #3".to_string())
        );
        assert_eq!(back.super_agent_enabled, Some(true));
        assert_eq!(back.super_agent_timeout_secs, Some(45));
        // 自動フェイルオーバーの有効/無効も state に残る
        assert_eq!(back.failover_enabled, Some(true));
        // パレットの MRU も state に残る (アクションは保存しない)
        assert_eq!(
            back.palette_recent,
            Some(vec![PaletteRecent {
                label: "保存".into(),
                icon: "💾".into(),
                uses: 3,
            }])
        );
    }

    #[test]
    fn ui_state_skips_none_fields() {
        let st = UiState {
            theme: Some("zaivern-dark".into()),
            ..Default::default()
        };
        let s = toml::to_string_pretty(&st).expect("None 混じりでも TOML 化できる");
        assert!(s.contains("theme"));
        assert!(!s.contains("pet_image"), "None は書き出されない: {s}");
        let back: UiState = toml::from_str(&s).expect("読み戻せる");
        assert_eq!(back.theme, Some("zaivern-dark".to_string()));
        assert_eq!(back.pet_image, None);
    }

    #[test]
    fn ui_state_empty_toml_is_all_none() {
        let st: UiState = toml::from_str("").expect("空でも成立する");
        assert_eq!(st.theme, None);
        assert_eq!(st.approval_mode, None);
        assert_eq!(st.pet_scale, None);
        assert_eq!(st.voice_engine, None);
    }

    #[test]
    fn ui_state_rejects_wrong_types() {
        assert!(toml::from_str::<UiState>("pet_scale = \"big\"\n").is_err());
        assert!(toml::from_str::<UiState>("show_pet = 1\n").is_err());
    }

    // ---- approval_mode 正規化 (load() 末尾のロジックと同じ規則) ----

    #[test]
    fn approval_mode_normalization_rules() {
        let normalize = |m: &str| -> String {
            if m != "auto" && m != "agent" {
                "ask".to_string()
            } else {
                m.to_string()
            }
        };
        assert_eq!(normalize("auto"), "auto");
        assert_eq!(normalize("agent"), "agent");
        assert_eq!(normalize("ask"), "ask");
        assert_eq!(normalize(""), "ask", "空文字は安全側へ");
        assert_eq!(normalize("AUTO"), "ask", "大文字は認識されない (現仕様)");
        assert_eq!(normalize(" auto "), "ask", "前後の空白は許容されない");
        assert_eq!(normalize("yolo"), "ask", "未知の値は安全側へ");
    }

    // ---- パス解決 ----

    #[test]
    fn config_and_state_paths_share_zaivern_dir() {
        let c = config_path();
        let s = state_path();
        assert_eq!(c.file_name().and_then(|f| f.to_str()), Some("config.toml"));
        assert_eq!(s.file_name().and_then(|f| f.to_str()), Some("state.toml"));
        assert_eq!(c.parent(), s.parent(), "同じ ~/.zaivern に置かれる");
        assert!(c.parent().is_some_and(|p| p.ends_with(".zaivern")));
        assert!(
            c.is_absolute() || c.starts_with("."),
            "home 不明時は ./.zaivern"
        );
    }

    #[test]
    fn zaivern_dir_is_home_or_dot_fallback() {
        let d = zaivern_dir();
        assert!(d.ends_with(".zaivern"));
        match dirs::home_dir() {
            Some(h) => assert_eq!(d, h.join(".zaivern")),
            None => assert_eq!(d, PathBuf::from(".").join(".zaivern")),
        }
    }

    #[test]
    fn overlay_path_is_workspace_local() {
        let ws = Path::new("/tmp/some-workspace");
        let p: PathBuf = ws.join(".zaivern.toml");
        assert_eq!(p, PathBuf::from("/tmp/some-workspace/.zaivern.toml"));
        assert!(p.starts_with(ws));
    }
}

#[cfg(test)]
mod supervisor_field_tests {
    use super::*;

    /// `[supervisor]` セクションが無い既存の config.toml が、
    /// これまでどおり読めて既定値が入ることを確かめる。
    #[test]
    fn config_without_supervisor_section_still_loads() {
        assert!(
            !DEFAULT_CONFIG.contains("[supervisor]"),
            "この検証は [supervisor] を書いていない設定を前提にしている"
        );
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("既定の設定が読めなくなった");
        assert_eq!(cfg.theme, "zaivern-dark");
        // プリセット件数はテンプレートと default_agents() で必ず一致する
        // (`default_config_template_agents_match_default_agents` が担保)
        assert_eq!(cfg.agents.len(), default_agents().len());
        // supervisor は SupervisorConfig の既定値で埋まる
        let d = crate::supervisor::SupervisorConfig::default();
        assert_eq!(cfg.supervisor.enabled, d.enabled);
        assert_eq!(cfg.supervisor.sample_interval_ms, d.sample_interval_ms);
        assert_eq!(cfg.supervisor.allow_auto_restart, d.allow_auto_restart);
    }

    /// 手元の `~/.zaivern/config.toml` があるなら、それも読めることを確かめる。
    /// 無い環境では何もしない (CI で落とさない)。
    #[test]
    fn existing_user_config_still_loads() {
        let Ok(s) = std::fs::read_to_string(config_path()) else {
            return;
        };
        let cfg: Config = toml::from_str(&s).expect("既存の config.toml が読めなくなった");
        assert!(!cfg.theme.is_empty());
        // 新しく生えた [super_agent] を書いていない既存ファイルでも既定値で埋まる
        assert_eq!(cfg.super_agent, SuperAgentConfig::default());
    }
}

#[cfg(test)]
mod failover_field_tests {
    use super::*;

    /// `[failover]` を書いていない設定は **必ず無効** で読み込まれる。
    /// (勝手に別アカウントへ移る = 課金先が変わる。既定で入ってはいけない)
    #[test]
    fn failoverセクションが無ければ既定で無効() {
        assert!(
            !DEFAULT_CONFIG.contains("\n[failover]"),
            "テンプレートは [failover] をコメントのままにしておく (既定は無効)"
        );
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("既定の設定が読める");
        assert_eq!(cfg.failover, crate::failover::FailoverConfig::default());
        assert!(!cfg.failover.enabled, "既定は必ず無効");
    }

    /// テンプレートに書いた `[failover]` の例 (コメントを外した形) が実際に読める。
    /// 書き方の見本がそのままでは動かない、を防ぐ。
    #[test]
    fn テンプレートのfailover例はコメントを外せば読める() {
        let uncommented: String = DEFAULT_CONFIG
            .lines()
            .skip_while(|l| !l.starts_with("# [failover]"))
            .take_while(|l| l.starts_with('#'))
            .map(|l| l.trim_start_matches("# ").trim_start_matches('#'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            uncommented.starts_with("[failover]"),
            "テンプレートに [failover] の例が無い: {uncommented:?}"
        );
        let cfg: Config = toml::from_str(&uncommented).expect("例がそのまま読める");
        assert!(!cfg.failover.enabled, "例の既定値も無効のまま");
        assert_eq!(cfg.failover.max_switches, 3);
        assert_eq!(cfg.failover.min_screen_hits, 2);
        // 有効化した形も読める。
        let on = uncommented.replace("enabled = false", "enabled = true");
        let cfg: Config = toml::from_str(&on).expect("有効化した形も読める");
        assert!(cfg.failover.enabled);
    }
}

#[cfg(test)]
mod super_agent_field_tests {
    use super::*;

    /// DoD: `[super_agent]` セクションが無い既存の config.toml が、
    /// これまでどおり読めて「なし」の既定値が入ること。
    #[test]
    fn super_agentセクションが無い設定も読める() {
        assert!(
            !DEFAULT_CONFIG.contains("[super_agent]"),
            "この検証は [super_agent] を書いていない設定を前提にしている"
        );
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("既定の設定が読めなくなった");
        assert_eq!(cfg.super_agent.command, "");
        assert!(!cfg.super_agent.enabled);
        assert_eq!(cfg.super_agent.timeout_secs, 60);
        assert_eq!(cfg.super_agent.active_command(), None);
    }

    /// 何も書かれていない TOML でも既定値どおりに読める。
    #[test]
    fn 空のtomlでも既定の監視役はなし() {
        let cfg: Config = toml::from_str("").expect("空の TOML");
        assert_eq!(cfg.super_agent, SuperAgentConfig::default());
    }

    /// 部分指定 (コマンドだけ) でも他は既定値のまま。
    #[test]
    fn 監視役の部分指定が読める() {
        let cfg: Config =
            toml::from_str("[super_agent]\ncommand = \"claude\"\nenabled = true\n").expect("TOML");
        assert_eq!(cfg.super_agent.command, "claude");
        assert!(cfg.super_agent.enabled);
        assert_eq!(cfg.super_agent.timeout_secs, 60);
        assert_eq!(cfg.super_agent.active_command(), Some("claude"));
    }

    /// DoD: 有効フラグだけ立っていてコマンドが空なら、監視役は居ない扱い。
    /// ここを取り違えると「誰も選ばれていないのに相談モードが ON」になる。
    #[test]
    fn コマンドが空なら有効フラグだけでは動かない() {
        let c = SuperAgentConfig {
            command: "   ".into(),
            enabled: true,
            timeout_secs: 30,
            ..Default::default()
        };
        assert_eq!(c.active_command(), None);
    }

    /// 無効化されていれば、コマンドが入っていても動かない。
    #[test]
    fn 無効化されていれば監視役は居ない() {
        let c = SuperAgentConfig {
            command: "claude".into(),
            enabled: false,
            timeout_secs: 30,
            ..Default::default()
        };
        assert_eq!(c.active_command(), None);
    }

    /// 前後の空白は落として渡す (診断側のカタログ照合が空白で失敗しないように)。
    #[test]
    fn コマンドの前後空白は落ちる() {
        let c = SuperAgentConfig {
            command: "  codex  ".into(),
            enabled: true,
            timeout_secs: 30,
            ..Default::default()
        };
        assert_eq!(c.active_command(), Some("codex"));
    }
}

#[cfg(test)]
mod plugins_config_tests {
    use super::*;

    #[test]
    fn 未記載のプラグインは有効() {
        let p = PluginsConfig::default();
        assert!(p.is_enabled("worktrees"));
    }

    #[test]
    fn 無効化と再有効化が往復する() {
        let mut p = PluginsConfig::default();
        p.set_enabled("worktrees", false);
        assert!(!p.is_enabled("worktrees"));
        assert_eq!(p.disabled, vec!["worktrees".to_string()]);

        // 二重に無効化しても重複しない
        p.set_enabled("worktrees", false);
        assert_eq!(p.disabled.len(), 1);

        p.set_enabled("worktrees", true);
        assert!(p.is_enabled("worktrees"));
        assert!(p.disabled.is_empty());
    }

    #[test]
    fn 設定値の読み書き() {
        let mut p = PluginsConfig::default();
        assert_eq!(p.setting("remote-host", "host"), None);
        p.set_setting("remote-host", "host", "user@example.com");
        assert_eq!(p.setting("remote-host", "host"), Some("user@example.com"));
        // 別キーを足しても既存キーは残る
        p.set_setting("remote-host", "remote_path", "/srv/work");
        assert_eq!(p.setting("remote-host", "host"), Some("user@example.com"));
        assert_eq!(p.setting("remote-host", "remote_path"), Some("/srv/work"));
        // 未知のプラグインは None
        assert_eq!(p.setting("nope", "host"), None);
    }

    #[test]
    fn toml_を往復できる() {
        let mut p = PluginsConfig::default();
        p.set_enabled("usage-meter", false);
        p.set_setting("worktrees", "parallel_count", "5");
        let s = toml::to_string_pretty(&p).expect("serialize");
        let back: PluginsConfig = toml::from_str(&s).expect("deserialize");
        assert!(!back.is_enabled("usage-meter"));
        assert_eq!(back.setting("worktrees", "parallel_count"), Some("5"));
    }

    #[test]
    fn plugins_セクションを省略した設定も読める() {
        // 既存ユーザーの config.toml には [plugins] が無い
        let cfg: Config = toml::from_str("theme = \"dark\"\n").expect("parse");
        assert!(cfg.plugins.disabled.is_empty());
        assert!(cfg.plugins.is_enabled("worktrees"));
    }

    #[test]
    fn plugins区画の書き換えでコメントが残る() {
        let raw = "# 大事なメモ\ntheme = \"dark\"\n\n[plugins]\ndisabled = [\"old\"]\n\n[plugins.settings.foo]\na = \"1\"\n\n[keybindings]\nsave = \"cmd+s\"\n";
        let mut p = PluginsConfig::default();
        p.set_enabled("new-one", false);
        p.set_setting("bar", "host", "example.com");
        let out = rewrite_plugins_section(raw, &p);

        assert!(out.contains("# 大事なメモ"), "区画外のコメントが消えた");
        assert!(out.contains("save = \"cmd+s\""), "他セクションが消えた");
        assert!(!out.contains("\"old\""), "古い disabled が残っている");
        assert!(
            !out.contains("[plugins.settings.foo]"),
            "古い設定テーブルが残っている"
        );
        assert!(out.contains("[plugins.settings.bar]"));

        let back: Config = toml::from_str(&out).expect("書き戻した config.toml が壊れている");
        assert!(!back.plugins.is_enabled("new-one"));
        assert_eq!(back.plugins.setting("bar", "host"), Some("example.com"));
    }

    #[test]
    fn 設定が空なら区画を書かない() {
        let raw = "theme = \"dark\"\n\n[plugins]\ndisabled = [\"x\"]\n";
        let out = rewrite_plugins_section(raw, &PluginsConfig::default());
        assert!(!out.contains("[plugins]"), "空なら区画ごと消えるべき");
        assert!(out.contains("theme"));
        let back: Config = toml::from_str(&out).expect("parse");
        assert!(back.plugins.is_enabled("x"));
    }

    #[test]
    fn 既存区画が無くても追記できる() {
        let out = rewrite_plugins_section("theme = \"dark\"\n", &{
            let mut p = PluginsConfig::default();
            p.set_enabled("z", false);
            p
        });
        let back: Config = toml::from_str(&out).expect("parse");
        assert!(!back.plugins.is_enabled("z"));
    }

    #[test]
    fn コメントアウトされた見出しは区画扱いしない() {
        // 既定テンプレートは "# [plugins]" を含む。これを本物の見出しと
        // 誤認すると、以降の行が丸ごと消えてしまう。
        let out = rewrite_plugins_section(DEFAULT_CONFIG, &PluginsConfig::default());
        assert!(out.contains("# [plugins]"), "コメント行が消えた");
        assert!(out.contains("[[agents]]"), "エージェント定義が消えた");
        let back: Config = toml::from_str(&out).expect("既定テンプレートが壊れた");
        assert!(!back.agents.is_empty());
    }

    #[test]
    fn 既定テンプレートがそのまま読める() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("既定 config.toml が壊れている");
        assert!(cfg.plugins.is_enabled("worktrees"));
        assert!(!cfg.agents.is_empty());
    }

    // ---- 自動YESのユーザー定義ルール ([[auto_yes_rules]]) ----

    #[test]
    fn 自動yesルールが書いて読んで往復する() {
        let src = r#"
theme = "dark"

[[auto_yes_rules]]
pattern = "Allow access to this file?"
reply = "\r"
agent = "agy"

[[auto_yes_rules]]
pattern = "Continue with this plan?"
reply = "y\r"
"#;
        let cfg: Config = toml::from_str(src).expect("auto_yes_rules が読めない");
        assert_eq!(cfg.auto_yes_rules.len(), 2);
        assert_eq!(cfg.auto_yes_rules[0].pattern, "Allow access to this file?");
        assert_eq!(cfg.auto_yes_rules[0].reply, "\r", "TOML のエスケープが効く");
        assert_eq!(cfg.auto_yes_rules[0].agent, "agy");
        // agent 省略時は「全エージェント」を表す空文字。
        assert_eq!(cfg.auto_yes_rules[1].reply, "y\r");
        assert_eq!(cfg.auto_yes_rules[1].agent, "");

        // 書き戻して読み直しても同じ (シリアライズ側の欠落よけ)。
        let out = toml::to_string(&cfg).expect("書き戻せない");
        let back: Config = toml::from_str(&out).expect("書き戻したものが読めない");
        assert_eq!(back.auto_yes_rules, cfg.auto_yes_rules);
    }

    #[test]
    fn キーの無い古い設定ファイルもそのまま読める() {
        // 既存ユーザーの config.toml には auto_yes_rules が無い。
        // 追加したキーのせいで設定全体が既定へ戻る事故を防ぐ。
        let old = "theme = \"dark\"\npet_auto_yes = true\n";
        let cfg: Config = toml::from_str(old).expect("古い設定が読めなくなった");
        assert_eq!(cfg.theme, "dark");
        assert!(cfg.pet_auto_yes, "既存のキーが読めている");
        assert!(
            cfg.auto_yes_rules.is_empty(),
            "未指定なら空 (既定の表だけ使う)"
        );
    }

    #[test]
    fn 既定テンプレートの自動yesルール例はコメントアウトされている() {
        // 例をそのまま有効にすると、書いた覚えの無いルールが効いてしまう。
        assert!(
            DEFAULT_CONFIG.contains("# [[auto_yes_rules]]"),
            "DEFAULT_CONFIG に auto_yes_rules の記入例が無い"
        );
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("既定 config.toml が壊れている");
        assert!(
            cfg.auto_yes_rules.is_empty(),
            "記入例が有効になっている (コメントアウトのはず)"
        );
    }

    #[test]
    fn 番号入力メニューのユーザールールが書ける() {
        // 「アンケートを数字で入力しないと進まない」画面の選び方を、
        // 再ビルド無しで config.toml から上書きできること。
        let src = r#"
theme = "dark"

[[auto_yes_rules]]
pattern = "How would you rate"
reply = "3\r"
"#;
        let cfg: Config = toml::from_str(src).expect("番号メニューのルールが読めない");
        assert_eq!(cfg.auto_yes_rules.len(), 1);
        assert_eq!(
            cfg.auto_yes_rules[0].reply, "3\r",
            "番号 + Enter が書けない"
        );
        assert!(
            cfg.auto_yes_rules[0].agent.is_empty(),
            "省略時は全エージェント"
        );
        // 記入例が既定 config.toml に載っていること (ユーザーが真似できる)
        assert!(
            DEFAULT_CONFIG.contains("reply = \"3\\r\""),
            "DEFAULT_CONFIG に番号入力メニューの記入例が無い"
        );
    }

    // ---- 承認ポリシー ([[approval_policies]]) ----

    #[test]
    fn 承認ポリシーが書いて読んで往復する() {
        use crate::agents::approvals::{ApprovalKind, Decision, Scope};
        let src = r#"
theme = "dark"

[[approval_policies]]
kind = "file_read"
scope = "agent"
target = "claude"
decision = "allow_always"

[[approval_policies]]
kind = "network_access"
decision = "deny_always"

[[approval_policies]]
kind = "file_write"
scope = "path"
target = "/repo/src"
decision = "allow_once"
"#;
        let cfg: Config = toml::from_str(src).expect("approval_policies が読めない");
        assert_eq!(cfg.approval_policies.len(), 3);
        // scope 省略時は既定の "global"。
        assert_eq!(cfg.approval_policies[1].scope, "global");

        let ps = approval_policies_from_config(&cfg);
        assert_eq!(ps.len(), 3);
        assert_eq!(ps[0].kind, ApprovalKind::FileRead);
        assert_eq!(ps[0].scope, Scope::Agent("claude".into()));
        assert_eq!(ps[0].decision, Decision::AllowAlways);
        assert_eq!(ps[1].scope, Scope::Global);
        assert_eq!(ps[1].decision, Decision::DenyAlways);
        assert_eq!(ps[2].scope, Scope::PathPrefix("/repo/src".into()));
        assert_eq!(ps[2].decision, Decision::AllowOnce);

        // 書き戻して読み直しても同じ (シリアライズ側の欠落よけ)。
        let out = toml::to_string(&cfg).expect("書き戻せない");
        let back: Config = toml::from_str(&out).expect("書き戻したものが読めない");
        assert_eq!(back.approval_policies, cfg.approval_policies);
        assert_eq!(approval_policies_from_config(&back), ps);
    }

    #[test]
    fn 未知の値の承認ポリシー行は捨てる() {
        // 新しい Zaivern が書いた種別を古いバイナリで読んだときに、
        // 近い種別へ丸めて自動承認してしまう事故を防ぐ。
        let src = r#"
[[approval_policies]]
kind = "quantum_teleport"
decision = "allow_always"

[[approval_policies]]
kind = "file_read"
scope = "brainwave"
decision = "allow_always"

[[approval_policies]]
kind = "file_read"
decision = "yolo"

[[approval_policies]]
kind = "file_read"
decision = "allow_always"
"#;
        let cfg: Config = toml::from_str(src).expect("読めない");
        assert_eq!(cfg.approval_policies.len(), 4);
        let ps = approval_policies_from_config(&cfg);
        assert_eq!(ps.len(), 1, "妥当な 1 行だけ残るはず");
        assert_eq!(ps[0].kind, crate::agents::approvals::ApprovalKind::FileRead);
    }

    #[test]
    fn 承認ポリシーキーの無い古い設定ファイルもそのまま読める() {
        let old = "theme = \"dark\"\npet_auto_yes = true\n";
        let cfg: Config = toml::from_str(old).expect("古い設定が読めなくなった");
        assert_eq!(cfg.theme, "dark");
        assert!(cfg.pet_auto_yes);
        assert!(
            cfg.approval_policies.is_empty(),
            "未指定なら空 (= 従来どおり全部ユーザーに聞く)"
        );
        assert!(approval_policies_from_config(&cfg).is_empty());
    }

    #[test]
    fn 既定テンプレートの承認ポリシー例はコメントアウトされている() {
        assert!(
            DEFAULT_CONFIG.contains("# [[approval_policies]]"),
            "DEFAULT_CONFIG に approval_policies の記入例が無い"
        );
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("既定 config.toml が壊れている");
        assert!(
            cfg.approval_policies.is_empty(),
            "記入例が有効になっている (コメントアウトのはず)"
        );
    }
}

#[cfg(test)]
mod state_overlay_tests {
    use super::*;

    // プロジェクト overlay とグローバル state.toml の分離。
    // 実ユーザーの ~/.zaivern に触れないよう、実体の load_from_dir() /
    // save_state_to_dir() に一時ディレクトリを差し込んで検証する。

    #[test]
    fn プロジェクトoverlayの値はsave_stateでグローバルに漏れない() {
        let home = crate::test_util::unique_temp_dir("zaivern-config-test", "no-leak-home");
        let root = crate::test_util::unique_temp_dir("zaivern-config-test", "no-leak-root");
        std::fs::write(
            home.join("state.toml"),
            "theme = \"global-theme\"\napproval_mode = \"auto\"\nshow_pet = false\n",
        )
        .expect("write state.toml");
        std::fs::write(
            root.join(".zaivern.toml"),
            "theme = \"project-theme\"\napproval_mode = \"agent\"\nshow_pet = true\n",
        )
        .expect("write .zaivern.toml");

        let cfg = load_from_dir(&home, std::slice::from_ref(&root), true);
        // セッション中はプロジェクトの値が効く
        assert_eq!(cfg.theme, "project-theme");
        assert_eq!(cfg.approval_mode, "agent");
        assert!(cfg.show_pet);

        // プロジェクトを開いただけで保存されても、グローバルは壊れない
        save_state_to_dir(&home, &cfg);
        let raw = std::fs::read_to_string(home.join("state.toml")).expect("re-read state.toml");
        let st: UiState = toml::from_str(&raw).expect("parse state.toml");
        assert_eq!(st.theme.as_deref(), Some("global-theme"));
        assert_eq!(st.approval_mode.as_deref(), Some("auto"));
        assert_eq!(st.show_pet, Some(false));

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 折り返しと空白表示はstateへ永続化される() {
        let home = crate::test_util::unique_temp_dir("zaivern-config-test", "wrap-ws-home");
        let mut cfg = Config::default();
        assert!(!cfg.word_wrap && !cfg.show_whitespace, "既定はどちらもオフ");

        // UI からの切替相当 (Cmd::ToggleWordWrap 等): 控えも一緒に更新する
        cfg.word_wrap = true;
        cfg.global_word_wrap = true;
        cfg.show_whitespace = true;
        cfg.global_show_whitespace = true;
        save_state_to_dir(&home, &cfg);

        let loaded = load_from_dir(&home, &[], true);
        assert!(loaded.word_wrap, "折り返しが state.toml から復元される");
        assert!(
            loaded.show_whitespace,
            "空白表示が state.toml から復元される"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn パレットのmruは保存と読み込みで順序が保たれる() {
        let home = crate::test_util::unique_temp_dir("zaivern-config-test", "palette-mru");
        let mut cfg = Config::default();
        assert!(cfg.palette_recent.is_empty(), "既定は空");
        cfg.palette_recent = vec![
            PaletteRecent {
                label: "ターミナル表示切替".into(),
                icon: "🖥".into(),
                uses: 5,
            },
            PaletteRecent {
                label: "保存".into(),
                icon: "💾".into(),
                uses: 2,
            },
            PaletteRecent {
                label: "絵文字🎨と日本語".into(),
                icon: String::new(),
                uses: 1,
            },
        ];
        save_state_to_dir(&home, &cfg);
        let loaded = load_from_dir(&home, &[], true);
        assert_eq!(
            loaded.palette_recent, cfg.palette_recent,
            "MRU が往復で変わった (順序・回数・アイコン)"
        );

        // 別の UI 操作 (テーマ変更) で save_state しても MRU は消えない
        let mut next = loaded;
        next.global_theme = "zaivern-light".into();
        save_state_to_dir(&home, &next);
        let again = load_from_dir(&home, &[], true);
        assert_eq!(again.palette_recent, cfg.palette_recent, "MRU が消えた");
        assert_eq!(again.theme, "zaivern-light");

        // config.toml (手書き) には書かない = state.toml 側だけに現れる
        let state = std::fs::read_to_string(home.join("state.toml")).expect("state.toml");
        assert!(state.contains("palette_recent"), "state.toml に無い");

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn 壊れたstateファイルでもmruはパニックせず既定に戻る() {
        for (tag, body) in [
            ("broken-toml", "palette_recent = [[[\n"),
            ("wrong-type", "palette_recent = 42\n"),
            (
                "missing-fields",
                "theme = \"zaivern-dark\"\n[[palette_recent]]\n",
            ),
            ("empty", ""),
        ] {
            let home = crate::test_util::unique_temp_dir("zaivern-config-test", tag);
            std::fs::write(home.join("state.toml"), body).expect("write state.toml");
            // 壊れていても load は成立し、MRU は既定 (空 or 既定値) に落ちる
            let cfg = load_from_dir(&home, &[], true);
            for r in &cfg.palette_recent {
                // 欠けたフィールドは serde の default (空文字 / 0) で埋まる
                assert!(r.uses < u32::MAX, "tag={tag}");
            }
            if tag != "missing-fields" {
                assert!(cfg.palette_recent.is_empty(), "tag={tag} は空に戻るはず");
            }
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn ミニマップとブレッドクラムの既定と永続化() {
        let home = crate::test_util::unique_temp_dir("zaivern-config-test", "mm-bc-home");
        let root = crate::test_util::unique_temp_dir("zaivern-config-test", "mm-bc-root");
        let cfg = Config::default();
        // 既定: ミニマップはオフ (本文の幅を奪うので使う人だけが払う)
        //       ブレッドクラムはオン (高さ 1 行・LSP 無しでも必ず出せる)
        assert!(!cfg.minimap, "ミニマップの既定はオフ");
        assert!(cfg.breadcrumbs, "ブレッドクラムの既定はオン");

        // UI からの切替相当: 控えも一緒に更新する
        let mut cfg = cfg;
        cfg.minimap = true;
        cfg.global_minimap = true;
        cfg.breadcrumbs = false;
        cfg.global_breadcrumbs = false;
        save_state_to_dir(&home, &cfg);
        let loaded = load_from_dir(&home, &[], true);
        assert!(loaded.minimap, "ミニマップが state.toml から復元される");
        assert!(
            !loaded.breadcrumbs,
            "ブレッドクラムが state.toml から復元される"
        );

        // プロジェクト overlay で上書きでき、グローバルの控えへは漏れない
        std::fs::write(
            root.join(".zaivern.toml"),
            "minimap = false\nbreadcrumbs = true\n",
        )
        .expect("write .zaivern.toml");
        let cfg = load_from_dir(&home, std::slice::from_ref(&root), true);
        assert!(!cfg.minimap && cfg.breadcrumbs, "プロジェクト値が効く");
        assert!(
            cfg.global_minimap && !cfg.global_breadcrumbs,
            "控えは overlay 前のまま"
        );

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 折り返しはプロジェクトoverlayでも上書きできる() {
        let home = crate::test_util::unique_temp_dir("zaivern-config-test", "wrap-ov-home");
        let root = crate::test_util::unique_temp_dir("zaivern-config-test", "wrap-ov-root");
        std::fs::write(
            root.join(".zaivern.toml"),
            "word_wrap = true\nshow_whitespace = true\n",
        )
        .expect("write .zaivern.toml");

        let cfg = load_from_dir(&home, std::slice::from_ref(&root), true);
        assert!(cfg.word_wrap && cfg.show_whitespace, "プロジェクト値が効く");
        // グローバルの控えは overlay 適用前の値のまま → state.toml へ漏れない
        assert!(!cfg.global_word_wrap && !cfg.global_show_whitespace);
        save_state_to_dir(&home, &cfg);
        let raw = std::fs::read_to_string(home.join("state.toml")).expect("re-read");
        let st: UiState = toml::from_str(&raw).expect("parse");
        assert_eq!(st.word_wrap, Some(false));
        assert_eq!(st.show_whitespace, Some(false));

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ユーザーが変えた値はoverlay下でも永続化される() {
        let home = crate::test_util::unique_temp_dir("zaivern-config-test", "persist-home");
        let root = crate::test_util::unique_temp_dir("zaivern-config-test", "persist-root");
        std::fs::write(home.join("state.toml"), "theme = \"global-theme\"\n")
            .expect("write state.toml");
        std::fs::write(root.join(".zaivern.toml"), "theme = \"project-theme\"\n")
            .expect("write .zaivern.toml");

        let mut cfg = load_from_dir(&home, std::slice::from_ref(&root), true);
        // UI からの変更相当 (Cmd::SetTheme / Cmd::TogglePet): 控えも一緒に更新する
        cfg.theme = "user-picked".into();
        cfg.global_theme = "user-picked".into();
        cfg.show_pet = false;
        cfg.global_show_pet = false;

        save_state_to_dir(&home, &cfg);
        let raw = std::fs::read_to_string(home.join("state.toml")).expect("re-read state.toml");
        let st: UiState = toml::from_str(&raw).expect("parse state.toml");
        assert_eq!(st.theme.as_deref(), Some("user-picked"));
        assert_eq!(st.show_pet, Some(false));

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
    }
}
