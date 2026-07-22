use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: String,
    pub editor_font_size: f32,
    pub terminal_font_size: f32,
    pub show_hidden_files: bool,
    /// 既定の権限モード: "ask"(毎回ユーザー承認) | "auto"(全て自動YES) |
    /// "agent"(Agent欄優先: プリセットのコマンドに書かれたフラグをそのまま使う)
    pub approval_mode: String,
    pub show_pet: bool,
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
    /// 承認時に PTY へ送るキー (既定は Enter)
    pub pet_approve_keys: String,
    /// 拒否時に PTY へ送るキー (既定は ESC)
    pub pet_deny_keys: String,
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
    /// LLM への相談を有効にするか。`command` が空ならこの値によらず相談しない。
    pub enabled: bool,
    /// 診断 1 回あたりのハード期限 (秒)。5 秒未満は診断側で 5 秒に丸められる。
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
            show_hidden_files: true,
            approval_mode: "ask".into(),
            show_pet: true,
            pet_image: None,
            pet_x: None,
            pet_y: None,
            pet_variant: "blocky".into(),
            pet_scale: 1.0,
            pet_free_roam: true,
            pet_sleep: true,
            pet_sounds: true,
            pet_bubbles: true,
            pet_approve_keys: "\r".into(),
            pet_deny_keys: "\u{1b}".into(),
            voice_engine: "auto".into(),
            voice_target: "active".into(),
            voice_lang: "ja-JP".into(),
            voice_command: String::new(),
            voice_keyword: String::new(),
            webhook_url: String::new(),
            agents: default_agents(),
            keybindings: HashMap::new(),
            supervisor: crate::supervisor::SupervisorConfig::default(),
            super_agent: SuperAgentConfig::default(),
            plugins: PluginsConfig::default(),
        }
    }
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

fn default_agents() -> Vec<AgentPreset> {
    vec![
        AgentPreset {
            name: "Claude Code".into(),
            command: "claude".into(),
            icon: "👾".into(),
            ..Default::default()
        },
        AgentPreset {
            name: "Claude Code (全自動)".into(),
            command: "claude --dangerously-skip-permissions".into(),
            icon: "⚡".into(),
            ..Default::default()
        },
        AgentPreset {
            name: "Codex".into(),
            command: "codex".into(),
            icon: "💡".into(),
            ..Default::default()
        },
        AgentPreset {
            name: "Codex (全自動)".into(),
            command: "codex --dangerously-bypass-approvals-and-sandbox".into(),
            icon: "⚡".into(),
            ..Default::default()
        },
        AgentPreset {
            name: "Antigravity".into(),
            command: "agy".into(),
            icon: "🚀".into(),
            ..Default::default()
        },
        AgentPreset {
            name: "Antigravity (全自動)".into(),
            command: "agy --dangerously-skip-permissions".into(),
            icon: "⚡".into(),
            ..Default::default()
        },
        AgentPreset {
            name: "Shell".into(),
            command: String::new(),
            icon: "🖥".into(),
            ..Default::default()
        },
    ]
}

/// Project-local overlay (<workspace>/.zaivern.toml): every field optional.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Overlay {
    theme: Option<String>,
    editor_font_size: Option<f32>,
    terminal_font_size: Option<f32>,
    show_hidden_files: Option<bool>,
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
    pet_image: Option<String>,
    pet_x: Option<f32>,
    pet_y: Option<f32>,
    pet_variant: Option<String>,
    pet_scale: Option<f32>,
    pet_free_roam: Option<bool>,
    pet_sleep: Option<bool>,
    pet_sounds: Option<bool>,
    pet_bubbles: Option<bool>,
    pet_approve_keys: Option<String>,
    pet_deny_keys: Option<String>,
    voice_engine: Option<String>,
    voice_target: Option<String>,
    voice_lang: Option<String>,
    voice_command: Option<String>,
    voice_keyword: Option<String>,
    /// 監視役 LLM の選択。UI から選ぶものなので、手書きの config.toml ではなく
    /// state 側に置く (config.toml をアプリが書き換えない方針に合わせる)。
    super_agent_command: Option<String>,
    super_agent_session_title: Option<String>,
    super_agent_enabled: Option<bool>,
    super_agent_timeout_secs: Option<u64>,
}

pub fn config_path() -> PathBuf {
    zaivern_dir().join("config.toml")
}

pub fn state_path() -> PathBuf {
    zaivern_dir().join("state.toml")
}

fn zaivern_dir() -> PathBuf {
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

# テーマ: "zaivern-dark" | "zaivern-midnight" | "zaivern-light"
# カラーテーマJSON (VS Code 互換形式) へのフルパスも指定できます
# (~/.zaivern/themes とプラグイン同梱のテーマは 🎨 メニューに自動で並びます)
theme = "zaivern-dark"
editor_font_size = 15.0
terminal_font_size = 13.0
show_hidden_files = true

# 既定の権限モード (claude / codex / agy に自動適用)
#   "ask"   = 毎回ユーザー承認が必要（安全・デフォルト）
#   "auto"  = すべて自動YES（各CLIの bypass フラグを自動付与）
#   "agent" = Agent欄優先（プリセットのコマンドに書かれたフラグをそのまま使う。
#             「(全自動)」プリセットと通常プリセットを使い分けたい場合はこれ）
# ツールバーの 🛡/⚡/👾 ボタンでも切替できます
approval_mode = "ask"

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
# pet_approve_keys = "\r"    # 承認時にPTYへ送るキー (Enter)
# pet_deny_keys = "\u001B"   # 拒否時にPTYへ送るキー (ESC)

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
# claude / codex / agy で始まるコマンドには承認モードが自動適用されます
# (approval_mode = "agent" ならコマンドをそのまま尊重します)。

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
name = "Antigravity"
icon = "🚀"
command = "agy"

[[agents]]
name = "Antigravity (全自動)"
icon = "⚡"
command = "agy --dangerously-skip-permissions"

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

    let mut cfg: Config = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();

    if cfg.agents.is_empty() {
        cfg.agents = default_agents();
    }

    if with_state {
        if let Ok(s) = std::fs::read_to_string(state_path()) {
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
            }
        }
    }

    for root in roots {
        apply_overlay(&mut cfg, root);
    }

    if cfg.approval_mode != "auto" && cfg.approval_mode != "agent" {
        cfg.approval_mode = "ask".into();
    }
    cfg.editor_font_size = cfg.editor_font_size.clamp(8.0, 32.0);
    cfg.terminal_font_size = cfg.terminal_font_size.clamp(7.0, 28.0);
    cfg.pet_scale = cfg.pet_scale.clamp(0.5, 2.0);
    // 期限が 0 だと診断側で毎回丸められて分かりにくいので、ここで下限を揃える。
    cfg.super_agent.timeout_secs = cfg.super_agent.timeout_secs.clamp(5, 600);
    // LLM 相談の ON/OFF は「監視役が選ばれているか」から導く。
    // `[supervisor] llm_escalation` を単独で立てても、相談相手が居なければ
    // 何も起きない (request_diagnosis が no-op になる) ため、UI の見え方と
    // 実挙動がずれないようここで一本化する。
    cfg.supervisor.llm_escalation = cfg.super_agent.active_command().is_some();
    cfg
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
            if let Some(v) = o.show_hidden_files {
                cfg.show_hidden_files = v;
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
    save_plugins_config(&cfg.plugins)
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
        s.push_str(&format!("\n[plugins.settings.{name}]\n"));
        let mut keys: Vec<&String> = kv.keys().collect();
        keys.sort();
        for k in keys {
            s.push_str(&format!("{k} = {}\n", quote(&kv[k])));
        }
    }
    s
}

/// Persist the current UI choices (theme / approval mode / pet) without
/// touching the user's hand-written config.toml.
pub fn save_state(cfg: &Config) {
    let st = UiState {
        theme: Some(cfg.theme.clone()),
        approval_mode: Some(cfg.approval_mode.clone()),
        show_pet: Some(cfg.show_pet),
        pet_image: cfg.pet_image.clone(),
        pet_x: cfg.pet_x,
        pet_y: cfg.pet_y,
        pet_variant: Some(cfg.pet_variant.clone()),
        pet_scale: Some(cfg.pet_scale),
        pet_free_roam: Some(cfg.pet_free_roam),
        pet_sleep: Some(cfg.pet_sleep),
        pet_sounds: Some(cfg.pet_sounds),
        pet_bubbles: Some(cfg.pet_bubbles),
        pet_approve_keys: Some(cfg.pet_approve_keys.clone()),
        pet_deny_keys: Some(cfg.pet_deny_keys.clone()),
        voice_engine: Some(cfg.voice_engine.clone()),
        voice_target: Some(cfg.voice_target.clone()),
        voice_lang: Some(cfg.voice_lang.clone()),
        voice_command: Some(cfg.voice_command.clone()),
        voice_keyword: Some(cfg.voice_keyword.clone()),
        super_agent_command: Some(cfg.super_agent.command.clone()),
        super_agent_session_title: Some(cfg.super_agent.session_title.clone()),
        super_agent_enabled: Some(cfg.super_agent.enabled),
        super_agent_timeout_secs: Some(cfg.super_agent.timeout_secs),
    };
    if let Ok(s) = toml::to_string_pretty(&st) {
        let _ = std::fs::create_dir_all(zaivern_dir());
        let _ = std::fs::write(state_path(), s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // load() / ensure_default() / save_state() は実ユーザーの ~/.zaivern を
    // 読み書きするためテストしない。ここでは純粋なパース・既定値・マージ対象の
    // データ構造だけを検証する。

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
                "Antigravity",
                "Antigravity (全自動)",
                "Shell",
            ]
        );
    }

    #[test]
    fn default_agents_auto_presets_carry_bypass_flags() {
        let agents = default_agents();
        for a in &agents {
            if a.name.contains("全自動") {
                assert!(
                    a.command.contains("--dangerously"),
                    "{} に bypass フラグが無い: {:?}",
                    a.name,
                    a.command
                );
            }
        }
        // 通常プリセットは素のコマンドのまま
        let plain: Vec<&str> = agents
            .iter()
            .filter(|a| !a.name.contains("全自動"))
            .map(|a| a.command.as_str())
            .collect();
        assert_eq!(plain, vec!["claude", "codex", "agy", ""]);
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
        let c: Config = toml::from_str("theme = \"x\"\nlegacy_option = 42\n")
            .expect("未知のキーは無視される");
        assert_eq!(c.theme, "x");
    }

    #[test]
    fn config_accepts_optional_pet_position() {
        let c: Config =
            toml::from_str("pet_x = 12.5\npet_y = -3.0\npet_image = \"/tmp/p.png\"\n")
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
        assert!(toml::from_str::<Config>("[[agents\n").is_err(), "括弧が閉じていない");
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
        let o: Overlay = toml::from_str("theme = \"zaivern-midnight\"\nshow_pet = false\n")
            .expect("parse ok");
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
        let o: Overlay = toml::from_str("[keybindings]\nsave = \"ctrl+s\"\nrun = \"f5\"\n")
            .expect("parse ok");
        let mut base: HashMap<String, String> = HashMap::new();
        base.insert("save".into(), "cmd+s".into());
        base.insert("quit".into(), "cmd+q".into());
        for (k, v) in o.keybindings {
            base.insert(k, v);
        }
        assert_eq!(base.get("save").map(String::as_str), Some("ctrl+s"), "上書き");
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
            show_pet: Some(false),
            pet_image: Some("/tmp/p.png".into()),
            pet_x: Some(10.0),
            pet_y: Some(20.5),
            pet_variant: Some("cat".into()),
            pet_scale: Some(1.4),
            pet_free_roam: Some(false),
            pet_sleep: Some(false),
            pet_sounds: Some(true),
            pet_bubbles: Some(true),
            pet_approve_keys: Some("\r".into()),
            pet_deny_keys: Some("\u{1b}".into()),
            voice_engine: Some("command".into()),
            voice_target: Some("broadcast".into()),
            voice_lang: Some("en-US".into()),
            voice_command: Some("my-stt --lang {lang}".into()),
            voice_keyword: Some("送信".into()),
            super_agent_command: Some("claude".into()),
            super_agent_session_title: Some("Claude Code (全自動) #3".into()),
            super_agent_enabled: Some(true),
            super_agent_timeout_secs: Some(45),
        };
        let s = toml::to_string_pretty(&st).expect("UiState は TOML 化できる");
        let back: UiState = toml::from_str(&s).expect("読み戻せる");
        assert_eq!(back.theme, Some("zaivern-light".to_string()));
        assert_eq!(back.approval_mode, Some("auto".to_string()));
        assert_eq!(back.show_pet, Some(false));
        assert_eq!(back.pet_image, Some("/tmp/p.png".to_string()));
        assert_eq!(back.pet_x, Some(10.0));
        assert_eq!(back.pet_y, Some(20.5));
        assert_eq!(back.pet_variant, Some("cat".to_string()));
        assert_eq!(back.pet_scale, Some(1.4));
        assert_eq!(back.pet_free_roam, Some(false));
        assert_eq!(back.voice_keyword, Some("送信".to_string()));
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
        assert!(c.is_absolute() || c.starts_with("."), "home 不明時は ./.zaivern");
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
        assert_eq!(cfg.agents.len(), 7);
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
        assert!(!out.contains("[plugins.settings.foo]"), "古い設定テーブルが残っている");
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
}
