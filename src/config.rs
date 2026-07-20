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
    /// 音声認識エンジン: "auto" | "mac" | "command" | "off"
    /// auto = macOS なら内蔵 (mac)、その他 OS は voice_command
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
    pub agents: Vec<AgentPreset>,
    /// キーバインドの上書き: action名 → "cmd+shift+p" 形式 (src/keybinds.rs 参照)
    pub keybindings: HashMap<String, String>,
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
            agents: default_agents(),
            keybindings: HashMap::new(),
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
            icon: "🤖".into(),
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
            icon: "🧠".into(),
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
# ツールバーの 🛡/⚡/🤖 ボタンでも切替できます
approval_mode = "ask"

# デスクトップペット (🦀) の表示
show_pet = true

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
# voice_engine = "auto"    # "auto" | "mac"(内蔵) | "command"(外部) | "off"
# voice_target = "active"  # 届け先: "active"(アクティブなエージェント) | "broadcast"(全員)
# voice_lang = "ja-JP"     # 認識する言語
# voice_keyword = ""       # このキーワードを話すと Enter まで自動送信 ("" = 常に手動)
#
# macOS 以外、または独自の認識エンジンを使う場合は "command" にして
# voice_command を設定します。標準出力へ 1 行ずつ認識テキストを吐き、
# 標準入力に "q" が来たら終了するコマンドを想定しています ({lang} は言語に置換)。
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
icon = "🤖"
command = "claude"

[[agents]]
name = "Claude Code (全自動)"
icon = "⚡"
command = "claude --dangerously-skip-permissions"

[[agents]]
name = "Codex"
icon = "🧠"
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
# icon = "🧠"
# command = "claude --model claude-opus-4-8"
# env = { MAX_THINKING_TOKENS = "31999" }

# [[agents]]
# name = "Gemini CLI"
# icon = "✨"
# command = "gemini"

# ── キーバインド上書き(例)──────────────
# [keybindings]
# save = "cmd+s"
# toggle_terminal = "ctrl+`"
# toggle_comment = "cmd+/"
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

/// Load global config merged with the project overlay.
/// `with_state`: UI 選択 (state.toml) を最後に適用するか。
/// 起動時は true、「設定を再読み込み」では false (config.toml を正とする)。
pub fn load(workspace: &Path, with_state: bool) -> Config {
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
            }
        }
    }

    let overlay_path = workspace.join(".zaivern.toml");
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
        }
    }

    if cfg.approval_mode != "auto" && cfg.approval_mode != "agent" {
        cfg.approval_mode = "ask".into();
    }
    cfg.editor_font_size = cfg.editor_font_size.clamp(8.0, 32.0);
    cfg.terminal_font_size = cfg.terminal_font_size.clamp(7.0, 28.0);
    cfg.pet_scale = cfg.pet_scale.clamp(0.5, 2.0);
    cfg
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
    };
    if let Ok(s) = toml::to_string_pretty(&st) {
        let _ = std::fs::create_dir_all(zaivern_dir());
        let _ = std::fs::write(state_path(), s);
    }
}
