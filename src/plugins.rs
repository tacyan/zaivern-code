//! Zaivern 独自プラグインシステム。
//!
//! プラグインは `~/.zaivern/plugins/<name>/` に置かれた 1 ディレクトリで、
//! ルートの `plugin.toml` がマニフェスト。VS Code 拡張 (Node ランタイム前提) とは
//! 異なり、Zaivern プラグインは以下だけで完結する:
//!
//! - **コマンド**: 任意のシェルコマンドを実行し、結果をエディタへ反映する
//!   (選択範囲/ファイルを stdin へ、stdout を置換/挿入/新規タブ/通知へ)。
//!   コマンドパレット・プラグインタブ・キーバインド・保存時フックから起動できる。
//! - **テーマ**: カラーテーマ JSON (VS Code 互換形式) を同梱できる。
//! - **スニペット**: スニペット JSON (VS Code 互換形式) を同梱できる。
//!
//! 配布は「📤 エクスポート」で作る 1 ファイル (`<name>-<version>.zvplug` = ZIP)。
//! 受け取った人は「📦 インストール」で取り込むだけ。自作は「➕ 新規作成」で
//! テンプレート一式が生成され、そのまま編集して使える。
//!
//! ```toml
//! [plugin]
//! name = "my-plugin"          # 小文字英数と - _ のみ。ディレクトリ名になる
//! version = "0.1.0"
//! author = "you"
//! description = "何をするプラグインか"
//!
//! [[command]]
//! id = "upper"
//! title = "選択範囲を大文字化"
//! icon = "🔠"                  # 省略可
//! run = "tr '[:lower:]' '[:upper:]'"
//! input = "selection"          # none | selection | file (stdin に渡すもの)
//! output = "replace"           # replace | insert | new_tab | notify | silent
//! langs = []                   # 空 = 全言語。例: ["rust", "python"]
//! keybind = "cmd+alt+u"        # 省略可
//! on_save = false              # true: 対象言語のファイル保存時に自動実行(整形など)
//! timeout_secs = 30            # 暴走防止 (1〜600)
//! ```


use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// このビルドが解釈できるマニフェスト API 世代の上限。
pub const API_VERSION: u32 = 2;

/// `interval` 系の最小間隔 (秒)。
pub const MIN_INTERVAL_SECS: u64 = 5;

// ─── マニフェスト ────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmdInput {
    None,
    Selection,
    File,
}

/// v1 の出力先。既存の適用経路の識別子であり、意味は変えない。
/// v2 で増えた出力先は [`CmdSink`] 側で表現し、こちらでは `Silent` に落として
/// 旧経路が誤って反応しないようにしている。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmdOutput {
    Replace,
    Insert,
    NewTab,
    Notify,
    Silent,
}

/// v2 の出力先 (v1 の 5 種 + 追加の 3 種)。こちらが正となる値。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmdSink {
    Replace,
    Insert,
    NewTab,
    Notify,
    Silent,
    /// stdout をエージェント入力欄へ差し込む。
    AgentPrompt,
    /// stdout を `panel` で指定したパネルへ表示する。
    Panel,
    /// stdout を JSON Lines のアクション列として解釈する。
    Actions,
}

impl CmdSink {
    /// 旧経路へ渡す値。v2 専用の出力先は Silent (旧経路では何もしない)。
    pub fn legacy(self) -> CmdOutput {
        match self {
            CmdSink::Replace => CmdOutput::Replace,
            CmdSink::Insert => CmdOutput::Insert,
            CmdSink::NewTab => CmdOutput::NewTab,
            CmdSink::Notify => CmdOutput::Notify,
            _ => CmdOutput::Silent,
        }
    }

    /// マニフェストに書く文字列表現。丸ごとの対応表をコード側に残しておく
    /// ため、現時点で呼び出しが無くても保持する。
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            CmdSink::Replace => "replace",
            CmdSink::Insert => "insert",
            CmdSink::NewTab => "new_tab",
            CmdSink::Notify => "notify",
            CmdSink::Silent => "silent",
            CmdSink::AgentPrompt => "agent_prompt",
            CmdSink::Panel => "panel",
            CmdSink::Actions => "actions",
        }
    }

    /// 空文字は notify (v1 の既定) を返す。未知の値は None。
    pub fn parse(s: &str) -> Option<CmdSink> {
        Some(match s.trim() {
            "replace" => CmdSink::Replace,
            "insert" => CmdSink::Insert,
            "new_tab" => CmdSink::NewTab,
            "" | "notify" => CmdSink::Notify,
            "silent" => CmdSink::Silent,
            "agent_prompt" => CmdSink::AgentPrompt,
            "panel" => CmdSink::Panel,
            "actions" => CmdSink::Actions,
            _ => return None,
        })
    }
}

/// フックの起動契機。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HookEvent {
    Startup,
    FileOpen,
    FileSave,
    AgentFinish,
    AgentAttention,
    GitChange,
    Interval,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::Startup => "startup",
            HookEvent::FileOpen => "file_open",
            HookEvent::FileSave => "file_save",
            HookEvent::AgentFinish => "agent_finish",
            HookEvent::AgentAttention => "agent_attention",
            HookEvent::GitChange => "git_change",
            HookEvent::Interval => "interval",
        }
    }

    pub fn parse(s: &str) -> Option<HookEvent> {
        Some(match s.trim() {
            "startup" => HookEvent::Startup,
            "file_open" => HookEvent::FileOpen,
            "file_save" => HookEvent::FileSave,
            "agent_finish" => HookEvent::AgentFinish,
            "agent_attention" => HookEvent::AgentAttention,
            "git_change" => HookEvent::GitChange,
            "interval" => HookEvent::Interval,
            _ => return None,
        })
    }
}

/// パネルの更新契機。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelRefresh {
    Manual,
    OnOpen,
    Interval,
}

/// パネル本文の描画形式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelFormat {
    Text,
    Markdown,
}

/// 設定値の型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingType {
    Str,
    Bool,
    Int,
}

impl SettingType {
    pub fn as_str(self) -> &'static str {
        match self {
            SettingType::Str => "string",
            SettingType::Bool => "bool",
            SettingType::Int => "int",
        }
    }

    pub fn parse(s: &str) -> Option<SettingType> {
        Some(match s.trim() {
            "" | "string" => SettingType::Str,
            "bool" => SettingType::Bool,
            "int" => SettingType::Int,
            _ => return None,
        })
    }

    /// 文字列がこの型として妥当か。
    pub fn accepts(self, v: &str) -> bool {
        match self {
            SettingType::Str => true,
            SettingType::Bool => matches!(v.trim(), "true" | "false"),
            SettingType::Int => v.trim().parse::<i64>().is_ok(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PluginCommand {
    /// 安定識別子。省略時は title から slug 生成 (重複時は連番付き)。
    pub id: String,
    pub title: String,
    pub icon: String,
    pub run: String,
    pub input: CmdInput,
    /// v1 互換の出力先。v2 専用の出力先では Silent になる。
    pub output: CmdOutput,
    /// v2 の出力先 (こちらが正)。
    pub sink: CmdSink,
    /// `sink = Panel` のときの出力先パネルID。
    pub panel: Option<String>,
    /// 空 = 全言語。要素は snippets::lang_id_for 形式の言語ID (小文字)。
    pub langs: Vec<String>,
    pub keybind: Option<String>,
    pub on_save: bool,
    pub timeout_secs: u64,
    /// フック由来の擬似コマンドのとき、その契機。
    /// どのフックから起動したか。ログ・デバッグ用に載せる。
    #[allow(dead_code)]
    pub event: Option<HookEvent>,
}

impl PluginCommand {
    /// 言語フィルタの判定 (空リストは全言語にマッチ)。
    pub fn lang_matches(&self, lang_id: &str) -> bool {
        self.langs.is_empty() || self.langs.iter().any(|l| l.eq_ignore_ascii_case(lang_id))
    }
}

/// イベント駆動で走るフック。
#[derive(Clone, Debug)]
pub struct PluginHook {
    pub event: HookEvent,
    pub run: String,
    /// `event = Interval` のときの間隔 (秒、5 以上)。それ以外では 0。
    pub interval_secs: u64,
    pub sink: CmdSink,
    pub panel: Option<String>,
    pub timeout_secs: u64,
}

impl PluginHook {
    /// 実行系 (RunRequest) へ渡すための擬似コマンドに変換する。
    pub fn as_command(&self, plugin: &str) -> PluginCommand {
        PluginCommand {
            id: format!("hook:{}", self.event.as_str()),
            title: format!("{plugin} / {}", self.event.as_str()),
            icon: "🪝".to_string(),
            run: self.run.clone(),
            input: CmdInput::None,
            output: self.sink.legacy(),
            sink: self.sink,
            panel: self.panel.clone(),
            langs: Vec::new(),
            keybind: None,
            on_save: false,
            timeout_secs: self.timeout_secs,
            event: Some(self.event),
        }
    }
}

/// サイドバーに追加される独自パネル。
#[derive(Clone, Debug)]
pub struct PluginPanel {
    pub id: String,
    pub title: String,
    pub icon: String,
    /// 空可。空ならアクション経由でのみ更新される。
    pub run: String,
    pub refresh: PanelRefresh,
    /// `refresh = Interval` のときの間隔 (秒、5 以上)。それ以外では 0。
    pub interval_secs: u64,
    pub format: PanelFormat,
    pub timeout_secs: u64,
}

impl PluginPanel {
    /// 実行系 (RunRequest) へ渡すための擬似コマンドに変換する。
    pub fn as_command(&self) -> PluginCommand {
        PluginCommand {
            id: format!("panel:{}", self.id),
            title: self.title.clone(),
            icon: self.icon.clone(),
            run: self.run.clone(),
            input: CmdInput::None,
            output: CmdOutput::Silent,
            sink: CmdSink::Panel,
            panel: Some(self.id.clone()),
            langs: Vec::new(),
            keybind: None,
            on_save: false,
            timeout_secs: self.timeout_secs,
            event: None,
        }
    }
}

/// プラグイン設定項目の宣言。
#[derive(Clone, Debug)]
pub struct PluginSetting {
    pub key: String,
    pub kind: SettingType,
    /// 既定値を文字列化したもの (環境変数へはこの形で渡る)。
    pub default: String,
    pub label: String,
    pub secret: bool,
}

impl PluginSetting {
    /// 環境変数名 `ZV_CFG_<KEY大文字>`。
    pub fn env_key(&self) -> String {
        format!("ZV_CFG_{}", env_ident(&self.key))
    }
}

/// 設定キーを環境変数の識別子へ (英数字以外は `_`、大文字化)。
fn env_ident(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Debug)]
pub struct Plugin {
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    /// マニフェスト API 世代 (省略時 1)。
    pub api: u32,
    pub dir: PathBuf,
    pub commands: Vec<PluginCommand>,
    pub hooks: Vec<PluginHook>,
    pub panels: Vec<PluginPanel>,
    pub settings: Vec<PluginSetting>,
    /// 設定の現在値 (既定値で初期化し、apply_settings で上書き)。
    pub setting_values: HashMap<String, String>,
    pub themes: Vec<(String, PathBuf)>,        // (label, json path)
    pub snippet_files: Vec<(String, PathBuf)>, // (language, path)
    /// UI 言語パック (`[language]`)。有効なプラグインのものが i18n へ入る。
    pub language: Option<PluginLanguage>,
    /// 初回インストール時に有効で始めるか (マニフェストの `default_enabled`)。
    /// 既定 true。false のものは初回シード時に無効リストへ入れる。
    pub default_enabled: bool,
    /// 無効化されていないか。無効なら一切登録しない (一覧には残す)。
    pub enabled: bool,
    /// マニフェストが壊れている場合の理由 (一覧に ⚠ 表示するため)。
    pub error: Option<String>,
}

/// UI 言語パックの宣言。
#[derive(Clone, Debug)]
pub struct PluginLanguage {
    /// 言語ID (例: "en")。
    #[allow(dead_code)]
    pub id: String,
    /// 表示名 (例: "English")。
    #[allow(dead_code)]
    pub name: String,
    /// 辞書のパス (解決済み)。ファイルまたはディレクトリ。
    pub dict: PathBuf,
}

impl Plugin {
    /// 実際に機能を登録してよいか (有効かつマニフェストが健全)。
    pub fn active(&self) -> bool {
        self.enabled && self.error.is_none()
    }

    /// 設定の現在値 (未設定なら宣言された既定値、宣言も無ければ空)。
    pub fn setting(&self, key: &str) -> String {
        if let Some(v) = self.setting_values.get(key) {
            return v.clone();
        }
        self.settings
            .iter()
            .find(|s| s.key == key)
            .map(|s| s.default.clone())
            .unwrap_or_default()
    }

    /// 保存された値を取り込む。型に合わない値は既定値のまま無視する。
    pub fn apply_settings(&mut self, values: &HashMap<String, String>) {
        for s in &self.settings {
            let v = match values.get(&s.key) {
                Some(v) if s.kind.accepts(v) => v.trim().to_string(),
                _ => s.default.clone(),
            };
            self.setting_values.insert(s.key.clone(), v);
        }
    }
}

/// `Vec<Plugin>` / `&[Plugin]` に対する ID ベースの参照 API。
/// 呼び出し側が配列インデックスに依存しないための入口。
pub trait PluginList {
    fn find_plugin(&self, plugin: &str) -> Option<&Plugin>;
    /// 可変で引く版。プラグイン状態を書き換える拡張のために用意してある。
    #[allow(dead_code)]
    fn find_plugin_mut(&mut self, plugin: &str) -> Option<&mut Plugin>;
    /// (プラグイン名, コマンドID) でコマンドを引く。
    fn find_command(&self, plugin: &str, cmd_id: &str) -> Option<(&Plugin, &PluginCommand)>;
    /// (プラグイン名, パネルID) でパネルを引く。
    fn find_panel(&self, plugin: &str, panel_id: &str) -> Option<(&Plugin, &PluginPanel)>;
    /// 有効なプラグインのコマンドだけを (プラグイン名, コマンド) で列挙する。
    /// 有効なプラグインのコマンドを列挙する (拡張・外部呼び出し用)。
    #[allow(dead_code)]
    fn active_commands(&self) -> Vec<(&Plugin, &PluginCommand)>;
    /// 有効なプラグインのフックだけを列挙する。
    fn active_hooks(&self, event: HookEvent) -> Vec<(&Plugin, &PluginHook)>;
    /// 有効なプラグインのパネルだけを列挙する。
    fn active_panels(&self) -> Vec<(&Plugin, &PluginPanel)>;
    /// 無効化リストを適用する (未記載＝有効)。
    fn apply_disabled(&mut self, disabled: &[String]);
    /// プラグイン名 → 設定マップ をまとめて適用する。
    fn apply_all_settings(&mut self, settings: &HashMap<String, HashMap<String, String>>);
}

impl PluginList for [Plugin] {
    fn find_plugin(&self, plugin: &str) -> Option<&Plugin> {
        self.iter().find(|p| p.name == plugin)
    }

    fn find_plugin_mut(&mut self, plugin: &str) -> Option<&mut Plugin> {
        self.iter_mut().find(|p| p.name == plugin)
    }

    fn find_command(&self, plugin: &str, cmd_id: &str) -> Option<(&Plugin, &PluginCommand)> {
        let p = self.find_plugin(plugin)?;
        let c = p.commands.iter().find(|c| c.id == cmd_id)?;
        Some((p, c))
    }

    fn find_panel(&self, plugin: &str, panel_id: &str) -> Option<(&Plugin, &PluginPanel)> {
        let p = self.find_plugin(plugin)?;
        let panel = p.panels.iter().find(|x| x.id == panel_id)?;
        Some((p, panel))
    }

    fn active_commands(&self) -> Vec<(&Plugin, &PluginCommand)> {
        self.iter()
            .filter(|p| p.active())
            .flat_map(|p| p.commands.iter().map(move |c| (p, c)))
            .collect()
    }

    fn active_hooks(&self, event: HookEvent) -> Vec<(&Plugin, &PluginHook)> {
        self.iter()
            .filter(|p| p.active())
            .flat_map(|p| p.hooks.iter().map(move |h| (p, h)))
            .filter(|(_, h)| h.event == event)
            .collect()
    }

    fn active_panels(&self) -> Vec<(&Plugin, &PluginPanel)> {
        self.iter()
            .filter(|p| p.active())
            .flat_map(|p| p.panels.iter().map(move |x| (p, x)))
            .collect()
    }

    fn apply_disabled(&mut self, disabled: &[String]) {
        for p in self.iter_mut() {
            p.enabled = !disabled.iter().any(|d| d.trim().eq_ignore_ascii_case(&p.name));
        }
    }

    fn apply_all_settings(&mut self, settings: &HashMap<String, HashMap<String, String>>) {
        for p in self.iter_mut() {
            let empty = HashMap::new();
            let values = settings.get(&p.name).unwrap_or(&empty);
            p.apply_settings(values);
        }
    }
}

// serde 用の生マニフェスト。検証は validate() で行う。
#[derive(Deserialize)]
struct RawManifest {
    plugin: RawPlugin,
    #[serde(default, rename = "command")]
    commands: Vec<RawCommand>,
    #[serde(default, rename = "hook")]
    hooks: Vec<RawHook>,
    #[serde(default, rename = "panel")]
    panels: Vec<RawPanel>,
    #[serde(default, rename = "setting")]
    settings: Vec<RawSetting>,
    #[serde(default, rename = "theme")]
    themes: Vec<RawTheme>,
    #[serde(default, rename = "snippet")]
    snippets: Vec<RawSnippet>,
    #[serde(default)]
    language: Option<RawLanguage>,
}

#[derive(Deserialize)]
struct RawPlugin {
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    api: Option<u32>,
    /// 初回インストール時に有効で始めるか (省略時 true)。
    /// UI 言語のように「入れただけで挙動が変わる」プラグインは false にする。
    #[serde(default)]
    default_enabled: Option<bool>,
}

/// `[language]` セクション。UI 言語パック。
#[derive(Deserialize)]
struct RawLanguage {
    /// 言語ID (例: "en")。
    id: String,
    /// 表示名 (例: "English")。省略時は id。
    #[serde(default)]
    name: String,
    /// 辞書のパス (プラグインディレクトリ相対)。ファイルまたはディレクトリ。
    dict: String,
}

#[derive(Deserialize)]
struct RawCommand {
    #[serde(default)]
    id: String,
    title: String,
    #[serde(default)]
    icon: String,
    run: String,
    #[serde(default)]
    input: String,
    #[serde(default)]
    output: String,
    #[serde(default)]
    langs: Vec<String>,
    #[serde(default)]
    keybind: Option<String>,
    #[serde(default)]
    on_save: bool,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    panel: String,
}

#[derive(Deserialize)]
struct RawHook {
    #[serde(default)]
    event: String,
    run: String,
    #[serde(default)]
    interval_secs: Option<u64>,
    #[serde(default)]
    output: String,
    #[serde(default)]
    panel: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
struct RawPanel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    icon: String,
    #[serde(default)]
    run: String,
    #[serde(default)]
    refresh: String,
    #[serde(default)]
    interval_secs: Option<u64>,
    #[serde(default)]
    format: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
struct RawSetting {
    #[serde(default)]
    key: String,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    default: Option<toml::Value>,
    #[serde(default)]
    label: String,
    #[serde(default)]
    secret: bool,
}

#[derive(Deserialize)]
struct RawTheme {
    #[serde(default)]
    label: String,
    path: String,
}

#[derive(Deserialize)]
struct RawSnippet {
    #[serde(default)]
    language: String,
    path: String,
}

/// プラグイン名として妥当か (小文字英数と - _ のみ、1〜64 文字)。
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

pub fn plugins_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".zaivern").join("plugins"))
}

/// ~/.zaivern/plugins/*/plugin.toml をスキャンする。
/// 壊れたマニフェストも error 付きで一覧へ含める (作者がすぐ気づけるように)。
pub fn scan_installed() -> Vec<Plugin> {
    let Some(root) = plugins_root() else {
        return Vec::new();
    };
    scan_root(&root)
}

fn scan_root(root: &Path) -> Vec<Plugin> {
    let mut out: Vec<Plugin> = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || !path.is_dir() {
            continue;
        }
        if !path.join("plugin.toml").is_file() {
            continue;
        }
        match parse_manifest(&path) {
            Ok(p) => out.push(p),
            Err(e) => out.push(Plugin {
                name,
                version: String::new(),
                author: String::new(),
                description: String::new(),
                api: 1,
                dir: path,
                commands: Vec::new(),
                hooks: Vec::new(),
                panels: Vec::new(),
                settings: Vec::new(),
                setting_values: HashMap::new(),
                themes: Vec::new(),
                snippet_files: Vec::new(),
                language: None,
                default_enabled: true,
                enabled: true,
                error: Some(e),
            }),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// dir/plugin.toml を読み Plugin を構築する。
pub fn parse_manifest(dir: &Path) -> Result<Plugin, String> {
    let raw = std::fs::read_to_string(dir.join("plugin.toml"))
        .map_err(|e| format!("plugin.toml を読めません: {e}"))?;
    let m: RawManifest =
        toml::from_str(&raw).map_err(|e| format!("plugin.toml の解析に失敗: {e}"))?;

    let name = m.plugin.name.trim().to_lowercase();
    if !valid_name(&name) {
        return Err(format!(
            "プラグイン名が不正です: {:?} (小文字英数と - _ のみ)",
            m.plugin.name
        ));
    }

    let api = m.plugin.api.unwrap_or(1);
    if api == 0 || api > API_VERSION {
        return Err(format!(
            "api = {api} はこの版では扱えません (対応: 1〜{API_VERSION})"
        ));
    }

    // パネルはコマンド/フックの出力先として参照されるので先に確定させる。
    let mut panels: Vec<PluginPanel> = Vec::new();
    for (i, p) in m.panels.into_iter().enumerate() {
        let id = p.id.trim().to_lowercase();
        if !valid_ident(&id) {
            return Err(format!(
                "panel[{i}].id が不正: {:?} (小文字英数と - _ のみ)",
                p.id
            ));
        }
        if panels.iter().any(|x: &PluginPanel| x.id == id) {
            return Err(format!("panel[{i}].id が重複しています: {id:?}"));
        }
        let refresh = match p.refresh.trim() {
            "" | "manual" => PanelRefresh::Manual,
            "on_open" => PanelRefresh::OnOpen,
            "interval" => PanelRefresh::Interval,
            other => return Err(format!("panel[{i}].refresh が不正: {other:?}")),
        };
        let format = match p.format.trim() {
            "" | "text" => PanelFormat::Text,
            "markdown" => PanelFormat::Markdown,
            other => return Err(format!("panel[{i}].format が不正: {other:?}")),
        };
        let interval_secs = if refresh == PanelRefresh::Interval {
            let secs = p.interval_secs.unwrap_or(0);
            if secs < MIN_INTERVAL_SECS {
                return Err(format!(
                    "panel[{i}]: refresh = \"interval\" には interval_secs = {MIN_INTERVAL_SECS} 以上が必要です"
                ));
            }
            secs
        } else {
            0
        };
        if refresh != PanelRefresh::Manual && p.run.trim().is_empty() {
            return Err(format!(
                "panel[{i}]: refresh = {:?} には run が必要です",
                p.refresh.trim()
            ));
        }
        let title = if p.title.trim().is_empty() {
            id.clone()
        } else {
            p.title.trim().to_string()
        };
        panels.push(PluginPanel {
            id,
            title,
            icon: if p.icon.trim().is_empty() {
                "📋".to_string()
            } else {
                p.icon.trim().to_string()
            },
            run: p.run.trim().to_string(),
            refresh,
            interval_secs,
            format,
            timeout_secs: p.timeout_secs.unwrap_or(30).clamp(1, 600),
        });
    }

    let mut commands: Vec<PluginCommand> = Vec::new();
    for (i, c) in m.commands.into_iter().enumerate() {
        if c.title.trim().is_empty() || c.run.trim().is_empty() {
            return Err(format!("command[{i}] に title / run が必要です"));
        }
        let input = match c.input.trim() {
            "" | "none" => CmdInput::None,
            "selection" => CmdInput::Selection,
            "file" => CmdInput::File,
            other => return Err(format!("command[{i}].input が不正: {other:?}")),
        };
        let sink = match CmdSink::parse(&c.output) {
            Some(s) => s,
            None => return Err(format!("command[{i}].output が不正: {:?}", c.output.trim())),
        };
        let output = sink.legacy();
        // 保存時フックは「ファイル全体を整形して置き換える」動作に限定する
        if c.on_save && (input != CmdInput::File || sink != CmdSink::Replace) {
            return Err(format!(
                "command[{i}]: on_save = true には input = \"file\", output = \"replace\" が必要です"
            ));
        }
        let panel = check_panel_ref(&panels, sink, &c.panel, &format!("command[{i}]"))?;
        let id = unique_id(&mut commands, &c.id, &c.title, i);
        commands.push(PluginCommand {
            id,
            title: c.title.trim().to_string(),
            icon: if c.icon.trim().is_empty() {
                "🔌".to_string()
            } else {
                c.icon.trim().to_string()
            },
            run: c.run.trim().to_string(),
            input,
            output,
            sink,
            panel,
            langs: c.langs.iter().map(|l| l.trim().to_lowercase()).collect(),
            keybind: c.keybind.and_then(|k| {
                let k = k.trim().to_string();
                if k.is_empty() {
                    None
                } else {
                    Some(k)
                }
            }),
            on_save: c.on_save,
            timeout_secs: c.timeout_secs.unwrap_or(30).clamp(1, 600),
            event: None,
        });
    }

    let mut hooks: Vec<PluginHook> = Vec::new();
    for (i, h) in m.hooks.into_iter().enumerate() {
        if h.run.trim().is_empty() {
            return Err(format!("hook[{i}] に run が必要です"));
        }
        let Some(event) = HookEvent::parse(&h.event) else {
            return Err(format!("hook[{i}].event が不正: {:?}", h.event.trim()));
        };
        let sink = match CmdSink::parse(if h.output.trim().is_empty() {
            "silent"
        } else {
            &h.output
        }) {
            Some(s @ (CmdSink::Silent | CmdSink::Notify | CmdSink::Actions | CmdSink::Panel)) => s,
            _ => {
                return Err(format!(
                    "hook[{i}].output が不正: {:?} (silent | notify | actions | panel)",
                    h.output.trim()
                ))
            }
        };
        let interval_secs = if event == HookEvent::Interval {
            let secs = h.interval_secs.unwrap_or(0);
            if secs < MIN_INTERVAL_SECS {
                return Err(format!(
                    "hook[{i}]: event = \"interval\" には interval_secs = {MIN_INTERVAL_SECS} 以上が必要です"
                ));
            }
            secs
        } else {
            0
        };
        let panel = check_panel_ref(&panels, sink, &h.panel, &format!("hook[{i}]"))?;
        hooks.push(PluginHook {
            event,
            run: h.run.trim().to_string(),
            interval_secs,
            sink,
            panel,
            timeout_secs: h.timeout_secs.unwrap_or(30).clamp(1, 600),
        });
    }

    let mut settings: Vec<PluginSetting> = Vec::new();
    for (i, s) in m.settings.into_iter().enumerate() {
        let key = s.key.trim().to_string();
        if !valid_ident(&key.to_lowercase()) {
            return Err(format!(
                "setting[{i}].key が不正: {:?} (英数と - _ のみ)",
                s.key
            ));
        }
        if settings.iter().any(|x: &PluginSetting| x.key == key) {
            return Err(format!("setting[{i}].key が重複しています: {key:?}"));
        }
        let Some(kind) = SettingType::parse(&s.kind) else {
            return Err(format!("setting[{i}].type が不正: {:?}", s.kind.trim()));
        };
        let default = match (&s.default, kind) {
            (None, SettingType::Str) => String::new(),
            (None, SettingType::Bool) => "false".to_string(),
            (None, SettingType::Int) => "0".to_string(),
            (Some(toml::Value::String(v)), SettingType::Str) => v.clone(),
            (Some(toml::Value::Boolean(v)), SettingType::Bool) => v.to_string(),
            (Some(toml::Value::Integer(v)), SettingType::Int) => v.to_string(),
            (Some(v), _) => {
                return Err(format!(
                    "setting[{i}].default が type = {:?} と一致しません: {v}",
                    kind.as_str()
                ))
            }
        };
        settings.push(PluginSetting {
            key: key.clone(),
            kind,
            default,
            label: if s.label.trim().is_empty() {
                key
            } else {
                s.label.trim().to_string()
            },
            secret: s.secret,
        });
    }

    let themes = m
        .themes
        .into_iter()
        .map(|t| {
            let p = resolve_rel(dir, &t.path);
            let label = if t.label.trim().is_empty() {
                p.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "theme".into())
            } else {
                t.label.trim().to_string()
            };
            (label, p)
        })
        .collect();

    let snippet_files = m
        .snippets
        .into_iter()
        .map(|s| {
            let lang = if s.language.trim().is_empty() {
                "global".to_string()
            } else {
                s.language.trim().to_lowercase()
            };
            (lang, resolve_rel(dir, &s.path))
        })
        .collect();

    let language = match m.language {
        None => None,
        Some(l) => {
            let id = l.id.trim().to_lowercase();
            if id.is_empty() {
                return Err("[language] に id が必要です (例: \"en\")".into());
            }
            if l.dict.trim().is_empty() {
                return Err("[language] に dict (辞書のパス) が必要です".into());
            }
            Some(PluginLanguage {
                name: if l.name.trim().is_empty() {
                    id.clone()
                } else {
                    l.name.trim().to_string()
                },
                id,
                dict: resolve_rel(dir, &l.dict),
            })
        }
    };

    let mut p = Plugin {
        name,
        version: some_or(&m.plugin.version, "0.1.0"),
        author: m.plugin.author.trim().to_string(),
        description: m.plugin.description.trim().to_string(),
        api,
        dir: dir.to_path_buf(),
        commands,
        hooks,
        panels,
        settings,
        setting_values: HashMap::new(),
        themes,
        snippet_files,
        language,
        default_enabled: m.plugin.default_enabled.unwrap_or(true),
        enabled: true,
        error: None,
    };
    p.apply_settings(&HashMap::new()); // 既定値で初期化
    Ok(p)
}

/// `output = "panel"` のときだけパネル参照を要求し、実在するIDか検証する。
fn check_panel_ref(
    panels: &[PluginPanel],
    sink: CmdSink,
    raw: &str,
    what: &str,
) -> Result<Option<String>, String> {
    let id = raw.trim().to_lowercase();
    if sink != CmdSink::Panel {
        return Ok(if id.is_empty() { None } else { Some(id) });
    }
    if id.is_empty() {
        return Err(format!("{what}: output = \"panel\" には panel の指定が必要です"));
    }
    if !panels.iter().any(|p| p.id == id) {
        return Err(format!(
            "{what}: panel = {id:?} に対応する [[panel]] がありません"
        ));
    }
    Ok(Some(id))
}

/// title から安定IDを生成する。英数字以外は `-` に畳み、前後の `-` を落とす。
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if c == '_' || c == '-' || c.is_whitespace() {
            if !out.ends_with('-') {
                out.push('-');
            }
        }
        // それ以外 (日本語など) は落とす
    }
    let s = out.trim_matches('-').to_string();
    if s.len() > 64 {
        s.chars().take(64).collect::<String>().trim_matches('-').to_string()
    } else {
        s
    }
}

/// 明示IDが無ければ title から slug を作り、既存と衝突したら連番を足す。
fn unique_id(existing: &mut [PluginCommand], raw_id: &str, title: &str, i: usize) -> String {
    let base = {
        let e = raw_id.trim();
        if !e.is_empty() {
            e.to_string()
        } else {
            let s = slug(title);
            if s.is_empty() {
                format!("cmd{i}")
            } else {
                s
            }
        }
    };
    if !existing.iter().any(|c| c.id == base) {
        return base;
    }
    for n in 2..1000u32 {
        let cand = format!("{base}-{n}");
        if !existing.iter().any(|c| c.id == cand) {
            return cand;
        }
    }
    format!("cmd{i}")
}

/// パネルID / 設定キーとして妥当か (英数と - _ のみ、1〜64 文字)。
fn valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn some_or(s: &str, default: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        default.to_string()
    } else {
        t.to_string()
    }
}

/// "./themes/x.json" 等をプラグインディレクトリ相対で解決。
fn resolve_rel(dir: &Path, rel: &str) -> PathBuf {
    dir.join(rel.trim_start_matches("./"))
}

// ─── アクションプロトコル (プラグイン → アプリ) ──────────────────

/// 通知の重要度。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotifyLevel {
    Info,
    Warn,
    Error,
}

impl NotifyLevel {
    pub fn parse(s: &str) -> NotifyLevel {
        match s.trim() {
            "warn" => NotifyLevel::Warn,
            "error" => NotifyLevel::Error,
            _ => NotifyLevel::Info,
        }
    }
}

/// `output = "actions"` のとき stdout の 1 行が 1 つのこれになる。
#[derive(Clone, PartialEq, Debug)]
pub enum PluginAction {
    OpenFile { path: String, line: Option<u32> },
    Notify { message: String, level: NotifyLevel },
    InsertText { text: String },
    ReplaceBuffer { text: String },
    NewTab { title: String, text: String },
    AgentPrompt { agent: Option<String>, text: String, submit: bool },
    RunTerminal { command: String, cwd: Option<String> },
    OpenUrl { url: String },
    SetPanel { panel: String, text: String },
    SetStatus { text: String },
    RefreshFiles,
    SetSetting { key: String, value: String },
}

/// stdout を JSON Lines として解釈する。
/// 解釈できない行は黙って読み飛ばす (プラグインを落とさない)。
pub fn parse_actions(stdout: &str) -> Vec<PluginAction> {
    stdout.lines().filter_map(parse_action_line).collect()
}

fn parse_action_line(line: &str) -> Option<PluginAction> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
    let req = |k: &str| s(k).filter(|x| !x.is_empty());
    let opt = |k: &str| s(k).filter(|x| !x.trim().is_empty());
    Some(match v.get("action").and_then(|a| a.as_str())?.trim() {
        "open_file" => PluginAction::OpenFile {
            path: req("path")?,
            line: v
                .get("line")
                .and_then(|x| x.as_u64().or_else(|| x.as_str()?.parse().ok()))
                .map(|n| n.max(1) as u32),
        },
        "notify" => PluginAction::Notify {
            message: s("message").unwrap_or_default(),
            level: NotifyLevel::parse(&s("level").unwrap_or_default()),
        },
        "insert_text" => PluginAction::InsertText { text: s("text")? },
        "replace_buffer" => PluginAction::ReplaceBuffer { text: s("text")? },
        "new_tab" => PluginAction::NewTab {
            title: s("title").filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "結果".into()),
            text: s("text").unwrap_or_default(),
        },
        "agent_prompt" => PluginAction::AgentPrompt {
            agent: opt("agent"),
            text: s("text")?,
            submit: v.get("submit").and_then(|x| x.as_bool()).unwrap_or(false),
        },
        "run_terminal" => PluginAction::RunTerminal {
            command: req("command")?,
            cwd: opt("cwd"),
        },
        "open_url" => PluginAction::OpenUrl { url: req("url")? },
        "set_panel" => PluginAction::SetPanel {
            panel: req("panel")?.to_lowercase(),
            text: s("text").unwrap_or_default(),
        },
        "set_status" => PluginAction::SetStatus {
            text: s("text").unwrap_or_default(),
        },
        "refresh_files" => PluginAction::RefreshFiles,
        "set_setting" => PluginAction::SetSetting {
            key: req("key")?,
            value: s("value").unwrap_or_default(),
        },
        _ => return None,
    })
}

// ─── 環境変数 ────────────────────────────────────────────────────

/// プラグインプロセスへ渡す実行文脈。
pub struct EnvContext<'a> {
    /// 対象ファイルの絶対パス (無ければ None)。
    pub file: Option<&'a Path>,
    /// 言語ID (snippets::lang_id_for 形式)。
    pub lang: &'a str,
    pub workspace: &'a Path,
    /// 選択テキスト (無選択なら空)。
    pub selection: &'a str,
    /// カーソル位置 (1 始まり)。0 を渡すと 1 に丸める。
    pub line: usize,
    pub column: usize,
    /// アクティブなエージェント名 (無ければ空)。
    pub agent: &'a str,
    /// フック起動時のイベント (コマンド起動時は None)。
    pub event: Option<HookEvent>,
    /// 現在のブランチ名 (git 管理外なら空)。
    pub git_branch: &'a str,
}

impl Default for EnvContext<'_> {
    fn default() -> Self {
        EnvContext {
            file: None,
            lang: "",
            workspace: Path::new(""),
            selection: "",
            line: 1,
            column: 1,
            agent: "",
            event: None,
            git_branch: "",
        }
    }
}

/// 仕様 3 章の環境変数一式を組み立てる。`RunRequest.envs` へそのまま渡せる。
pub fn command_env(plugin: &Plugin, ctx: &EnvContext) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut push = |k: &str, v: String| env.push((k.to_string(), v));

    // 既存 (v1) — 意味も名前も変えない
    push(
        "ZV_FILE",
        ctx.file.map(|p| p.display().to_string()).unwrap_or_default(),
    );
    push("ZV_LANG", ctx.lang.to_string());
    push("ZV_WORKSPACE", ctx.workspace.display().to_string());
    push("ZV_PLUGIN_DIR", plugin.dir.display().to_string());

    // v2 追加分
    push("ZV_API", API_VERSION.to_string());
    push(
        "ZV_BIN",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    );
    push(
        "ZV_PLUGIN_DATA",
        plugin_data_dir(&plugin.name)
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    );
    push("ZV_SELECTION", ctx.selection.to_string());
    push("ZV_LINE", ctx.line.max(1).to_string());
    push("ZV_COLUMN", ctx.column.max(1).to_string());
    push("ZV_AGENT", ctx.agent.to_string());
    push(
        "ZV_EVENT",
        ctx.event.map(|e| e.as_str().to_string()).unwrap_or_default(),
    );
    push("ZV_GIT_BRANCH", ctx.git_branch.to_string());

    for s in &plugin.settings {
        env.push((s.env_key(), plugin.setting(&s.key)));
    }
    env
}

/// `~/.zaivern/plugin-data/<name>/` (存在しなければ作る)。
pub fn plugin_data_dir(name: &str) -> Option<PathBuf> {
    let dir = dirs::home_dir()?
        .join(".zaivern")
        .join("plugin-data")
        .join(name);
    let _ = std::fs::create_dir_all(&dir);
    Some(dir)
}

// ─── バンドル標準プラグイン ──────────────────────────────────────

/// ビルド時に埋め込む標準プラグイン。
/// 形式: `(プラグイン名, &[(プラグインディレクトリ相対パス, 内容)])`
///
/// `.sh` は展開時に実行権限が付く。バージョンは plugin.toml の `version` を見る。
///
/// この表は `assets/plugins/` を走査して機械的に生成している。
/// プラグインを足したら、同じ形式で 1 ブロック追記すること。
const BUNDLED: &[(&str, &[(&str, &str)])] = &[
    (
        "agent-compare",
        &[
            ("compare.sh", include_str!("../assets/plugins/agent-compare/compare.sh")),
            ("lib.sh", include_str!("../assets/plugins/agent-compare/lib.sh")),
            ("pick.sh", include_str!("../assets/plugins/agent-compare/pick.sh")),
            ("plugin.toml", include_str!("../assets/plugins/agent-compare/plugin.toml")),
        ],
    ),
    (
        "diff-review",
        &[
            ("clear.sh", include_str!("../assets/plugins/diff-review/clear.sh")),
            ("comment.sh", include_str!("../assets/plugins/diff-review/comment.sh")),
            ("lib.sh", include_str!("../assets/plugins/diff-review/lib.sh")),
            ("pending.sh", include_str!("../assets/plugins/diff-review/pending.sh")),
            ("plugin.toml", include_str!("../assets/plugins/diff-review/plugin.toml")),
            ("review.sh", include_str!("../assets/plugins/diff-review/review.sh")),
            ("send.sh", include_str!("../assets/plugins/diff-review/send.sh")),
        ],
    ),
    (
        "element-capture",
        &[
            ("plugin.toml", include_str!("../assets/plugins/element-capture/plugin.toml")),
            ("scripts/build-prompt.py", include_str!("../assets/plugins/element-capture/scripts/build-prompt.py")),
            ("scripts/capture-common.sh", include_str!("../assets/plugins/element-capture/scripts/capture-common.sh")),
            ("scripts/common.sh", include_str!("../assets/plugins/element-capture/scripts/common.sh")),
            ("scripts/find-browser.applescript", include_str!("../assets/plugins/element-capture/scripts/find-browser.applescript")),
            ("scripts/paste.sh", include_str!("../assets/plugins/element-capture/scripts/paste.sh")),
            ("scripts/pick.sh", include_str!("../assets/plugins/element-capture/scripts/pick.sh")),
            ("scripts/picker.js", include_str!("../assets/plugins/element-capture/scripts/picker.js")),
            ("scripts/poll.js", include_str!("../assets/plugins/element-capture/scripts/poll.js")),
            ("scripts/region.sh", include_str!("../assets/plugins/element-capture/scripts/region.sh")),
        ],
    ),
    (
        "english-mode",
        &[
            ("lang/10-common.toml", include_str!("../assets/plugins/english-mode/lang/10-common.toml")),
            ("lang/20-app.toml", include_str!("../assets/plugins/english-mode/lang/20-app.toml")),
            ("lang/30-cockpit.toml", include_str!("../assets/plugins/english-mode/lang/30-cockpit.toml")),
            ("lang/40-panels.toml", include_str!("../assets/plugins/english-mode/lang/40-panels.toml")),
            ("lang/50-editor.toml", include_str!("../assets/plugins/english-mode/lang/50-editor.toml")),
            ("plugin.toml", include_str!("../assets/plugins/english-mode/plugin.toml")),
        ],
    ),
    (
        "quick-actions",
        &[
            ("plugin.toml", include_str!("../assets/plugins/quick-actions/plugin.toml")),
            ("scripts/common.sh", include_str!("../assets/plugins/quick-actions/scripts/common.sh")),
            ("scripts/detect.sh", include_str!("../assets/plugins/quick-actions/scripts/detect.sh")),
            ("scripts/panel.sh", include_str!("../assets/plugins/quick-actions/scripts/panel.sh")),
            ("scripts/render.sh", include_str!("../assets/plugins/quick-actions/scripts/render.sh")),
            ("scripts/run.sh", include_str!("../assets/plugins/quick-actions/scripts/run.sh")),
            ("scripts/startup.sh", include_str!("../assets/plugins/quick-actions/scripts/startup.sh")),
        ],
    ),
    (
        "remote-host",
        &[
            ("agent.sh", include_str!("../assets/plugins/remote-host/agent.sh")),
            ("exec.sh", include_str!("../assets/plugins/remote-host/exec.sh")),
            ("lib.sh", include_str!("../assets/plugins/remote-host/lib.sh")),
            ("plugin.toml", include_str!("../assets/plugins/remote-host/plugin.toml")),
            ("pull.sh", include_str!("../assets/plugins/remote-host/pull.sh")),
            ("push.sh", include_str!("../assets/plugins/remote-host/push.sh")),
            ("remote.sh", include_str!("../assets/plugins/remote-host/remote.sh")),
            ("worktree.sh", include_str!("../assets/plugins/remote-host/worktree.sh")),
        ],
    ),
    (
        "tasks",
        &[
            ("plugin.toml", include_str!("../assets/plugins/tasks/plugin.toml")),
            ("scripts/common.sh", include_str!("../assets/plugins/tasks/scripts/common.sh")),
            ("scripts/gh-common.sh", include_str!("../assets/plugins/tasks/scripts/gh-common.sh")),
            ("scripts/issue-branch.sh", include_str!("../assets/plugins/tasks/scripts/issue-branch.sh")),
            ("scripts/issue-list.sh", include_str!("../assets/plugins/tasks/scripts/issue-list.sh")),
            ("scripts/panel.sh", include_str!("../assets/plugins/tasks/scripts/panel.sh")),
            ("scripts/pr-diff.sh", include_str!("../assets/plugins/tasks/scripts/pr-diff.sh")),
            ("scripts/pr-list.sh", include_str!("../assets/plugins/tasks/scripts/pr-list.sh")),
            ("scripts/pr-review.sh", include_str!("../assets/plugins/tasks/scripts/pr-review.sh")),
        ],
    ),
    (
        "usage-meter",
        &[
            ("plugin.toml", include_str!("../assets/plugins/usage-meter/plugin.toml")),
            ("scripts/common.sh", include_str!("../assets/plugins/usage-meter/scripts/common.sh")),
            ("scripts/panel.sh", include_str!("../assets/plugins/usage-meter/scripts/panel.sh")),
            ("scripts/refresh.sh", include_str!("../assets/plugins/usage-meter/scripts/refresh.sh")),
            ("scripts/report.sh", include_str!("../assets/plugins/usage-meter/scripts/report.sh")),
            ("scripts/scan.py", include_str!("../assets/plugins/usage-meter/scripts/scan.py")),
        ],
    ),
    (
        "worktrees",
        &[
            ("create.sh", include_str!("../assets/plugins/worktrees/create.sh")),
            ("lib.sh", include_str!("../assets/plugins/worktrees/lib.sh")),
            ("list.sh", include_str!("../assets/plugins/worktrees/list.sh")),
            ("merge.sh", include_str!("../assets/plugins/worktrees/merge.sh")),
            ("parallel.sh", include_str!("../assets/plugins/worktrees/parallel.sh")),
            ("plugin.toml", include_str!("../assets/plugins/worktrees/plugin.toml")),
            ("remove.sh", include_str!("../assets/plugins/worktrees/remove.sh")),
        ],
    ),
];

/// 標準プラグインを `plugins_root` へ展開する。展開したプラグイン名を返す。
/// 既に同じかより新しい版が入っている場合は触らない (ユーザーの編集を潰さない)。
pub fn seed_bundled(plugins_root: &Path) -> Vec<String> {
    seed_bundled_from(plugins_root, BUNDLED)
}

fn seed_bundled_from(root: &Path, table: &[(&str, &[(&str, &str)])]) -> Vec<String> {
    let mut seeded = Vec::new();
    for (name, files) in table {
        if !valid_name(name) {
            continue;
        }
        let bundled_ver = files
            .iter()
            .find(|(rel, _)| *rel == "plugin.toml")
            .map(|(_, body)| manifest_version(body))
            .unwrap_or_else(|| "0.0.0".to_string());
        let dir = root.join(name);
        let stamp = dir.join(".bundled");
        let installed = std::fs::read_to_string(&stamp)
            .unwrap_or_default()
            .trim()
            .to_string();
        // 展開済みで、バンドル版が新しくないなら何もしない
        if dir.is_dir() && !installed.is_empty() && !version_newer(&bundled_ver, &installed) {
            continue;
        }
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let mut ok = true;
        for (rel, body) in *files {
            // ディレクトリ脱出を防ぐ
            if rel.contains("..") || rel.starts_with('/') {
                ok = false;
                continue;
            }
            let dest = dir.join(rel);
            if let Some(parent) = dest.parent() {
                if std::fs::create_dir_all(parent).is_err() {
                    ok = false;
                    continue;
                }
            }
            if std::fs::write(&dest, body).is_err() {
                ok = false;
                continue;
            }
            if rel.ends_with(".sh") {
                make_executable(&dest);
            }
        }
        if ok && std::fs::write(&stamp, &bundled_ver).is_ok() {
            seeded.push((*name).to_string());
        }
    }
    seeded
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(perm.mode() | 0o755);
            let _ = std::fs::set_permissions(path, perm);
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// plugin.toml 本文から `version = "..."` を素朴に拾う (解析失敗時は 0.0.0)。
fn manifest_version(body: &str) -> String {
    for line in body.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("version") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let v = rest.trim().trim_matches('"').trim_matches('\'').trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    "0.0.0".to_string()
}

/// `a` が `b` より新しいか。数値成分を左から比較し、非数値成分は 0 とみなす。
fn version_newer(a: &str, b: &str) -> bool {
    let part = |s: &str| -> Vec<u64> {
        s.split(['.', '-', '+'])
            .map(|c| c.trim().parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (va, vb) = (part(a), part(b));
    let n = va.len().max(vb.len());
    for i in 0..n {
        let x = va.get(i).copied().unwrap_or(0);
        let y = vb.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

// ─── テンプレート生成 (自作の入口) ────────────────────────────────

/// ~/.zaivern/plugins/<name>/ にテンプレート一式を生成し、そのパスを返す。
pub fn create_template(name: &str) -> Result<PathBuf, String> {
    let root = plugins_root().ok_or_else(|| "ホームディレクトリを特定できません".to_string())?;
    create_template_at(&root, name)
}

fn create_template_at(root: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim().to_lowercase();
    if !valid_name(&name) {
        return Err("プラグイン名は小文字英数と - _ のみで指定してください".into());
    }
    let dir = root.join(&name);
    if dir.exists() {
        return Err(format!("{} は既に存在します", dir.display()));
    }
    std::fs::create_dir_all(dir.join("themes"))
        .and_then(|_| std::fs::create_dir_all(dir.join("snippets")))
        .map_err(|e| format!("ディレクトリを作成できません: {e}"))?;

    let manifest = format!(
        r#"# Zaivern プラグイン: {name}
# 保存後、プラグインタブの ⟳ (再スキャン) で反映されます。

[plugin]
name = "{name}"
version = "0.1.0"
author = ""
description = "説明をここに書く"
api = 2

# ─── コマンド ─────────────────────────────────────────────
# 任意のシェルコマンドを実行し、結果をエディタへ反映します。
#   id:     省略可。省略すると title から自動生成されます
#   input:  none | selection | file   … stdin に渡すもの
#   output: replace | insert | new_tab | notify | silent
#           | agent_prompt (エージェント入力欄へ差し込む)
#           | panel (下の [[panel]] へ表示。panel = "<id>" が必要)
#           | actions (stdout を JSON Lines のアクション列として解釈)
#           replace = 選択範囲(input=selection)/ファイル全体(input=file)を stdout で置換
#   環境変数: ZV_FILE / ZV_LANG / ZV_WORKSPACE / ZV_PLUGIN_DIR / ZV_API / ZV_BIN
#             ZV_PLUGIN_DATA / ZV_SELECTION / ZV_LINE / ZV_COLUMN / ZV_AGENT
#             ZV_EVENT / ZV_GIT_BRANCH / ZV_CFG_<設定キー大文字>

[[command]]
id = "upper"
title = "選択範囲を大文字化"
icon = "🔠"
run = "tr '[:lower:]' '[:upper:]'"
input = "selection"
output = "replace"

[[command]]
id = "wc"
title = "ファイルの行数・文字数を表示"
icon = "🧮"
run = "wc -lm"
input = "file"
output = "notify"

# 保存時に自動整形する例 (対象言語のファイルを保存すると実行):
# [[command]]
# id = "fmt-json"
# title = "JSON を整形"
# run = "python3 -m json.tool"
# input = "file"
# output = "replace"
# langs = ["json"]
# on_save = true
# keybind = "cmd+alt+f"

# ─── フック / パネル / 設定 (api = 2) ──────────────────────
# フック: イベント発生時に自動実行します。
#   event:  startup | file_open | file_save | agent_finish
#           | agent_attention | git_change | interval
#   output: silent | notify | actions | panel
# [[hook]]
# event = "file_save"
# run = "echo '{{\"action\":\"notify\",\"message\":\"保存しました\"}}'"
# output = "actions"

# 独自パネル: サイドバーに自前の表示欄を追加します。
#   refresh: manual | on_open | interval   (interval は interval_secs >= 5)
#   format:  text | markdown
# [[panel]]
# id = "tasks"
# title = "タスク"
# icon = "📋"
# run = "cat TODO.md"
# refresh = "on_open"
# format = "markdown"

# 設定: 値は ZV_CFG_<キー大文字> で渡ります。secret = true でUIをマスクします。
# [[setting]]
# key = "token"
# type = "string"          # string | bool | int
# default = ""
# label = "APIトークン"
# secret = true

# ─── テーマ / スニペット (VS Code 互換 JSON) ─────────────────

[[theme]]
label = "{name} dark"
path = "themes/sample.json"

[[snippet]]
language = "rust"
path = "snippets/sample.json"
"#
    );
    std::fs::write(dir.join("plugin.toml"), manifest)
        .map_err(|e| format!("plugin.toml を書けません: {e}"))?;

    std::fs::write(
        dir.join("themes").join("sample.json"),
        r##"{
  "name": "Sample Dark",
  "type": "dark",
  "colors": {
    "editor.background": "#101418",
    "editor.foreground": "#d8dee9",
    "sideBar.background": "#161b22",
    "focusBorder": "#7aa2f7",
    "terminal.background": "#101418",
    "terminal.foreground": "#d8dee9"
  }
}
"##,
    )
    .map_err(|e| format!("テーマを書けません: {e}"))?;

    std::fs::write(
        dir.join("snippets").join("sample.json"),
        r#"{
  "Debug print": {
    "prefix": "dbgp",
    "body": ["println!(\"{}: {:?}\", \"${1:label}\", ${2:value});$0"],
    "description": "println! デバッグ"
  }
}
"#,
    )
    .map_err(|e| format!("スニペットを書けません: {e}"))?;

    std::fs::write(
        dir.join("README.md"),
        format!(
            r#"# {name}

Zaivern Code のプラグインです。`plugin.toml` を編集し、プラグインタブの ⟳ で再読み込みしてください。

## 配布するには
プラグインタブの 📤 エクスポートで `{name}-<version>.zvplug` (ZIP) が作られます。
受け取った人は Zaivern の「📦 プラグインをインストール…」でそのファイルを選ぶだけです。
"#
        ),
    )
    .map_err(|e| format!("README を書けません: {e}"))?;

    Ok(dir)
}

// ─── インストール / エクスポート / アンインストール (配布の入口) ──

/// .zvplug / .zip またはプラグインディレクトリをインストールする。
pub fn install(src: &Path) -> Result<Plugin, String> {
    let root = plugins_root().ok_or_else(|| "ホームディレクトリを特定できません".to_string())?;
    install_at(&root, src)
}

fn install_at(root: &Path, src: &Path) -> Result<Plugin, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("{} を作成できません: {e}", root.display()))?;

    if src.is_dir() {
        let manifest = parse_manifest(src)?;
        let dest = root.join(&manifest.name);
        if src.canonicalize().ok() == dest.canonicalize().ok() {
            return Err("インストール先と同じディレクトリです".into());
        }
        if dest.exists() {
            std::fs::remove_dir_all(&dest)
                .map_err(|e| format!("既存の {} を削除できません: {e}", dest.display()))?;
        }
        copy_dir(src, &dest)?;
        return parse_manifest(&dest);
    }
    if !src.is_file() {
        return Err(format!("見つかりません: {}", src.display()));
    }

    // 一意な一時展開先
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = root.join(format!(".tmp-install-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("一時ディレクトリを作成できません: {e}"))?;

    if let Err(e) = extract_zip(src, &tmp) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }

    // plugin.toml はルート直下、または単一サブディレクトリ直下を許容
    let payload = if tmp.join("plugin.toml").is_file() {
        tmp.clone()
    } else {
        let mut found: Option<PathBuf> = None;
        if let Ok(rd) = std::fs::read_dir(&tmp) {
            for e in rd.flatten() {
                if e.path().is_dir() && e.path().join("plugin.toml").is_file() {
                    found = Some(e.path());
                    break;
                }
            }
        }
        match found {
            Some(p) => p,
            None => {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err("アーカイブ内に plugin.toml が見つかりません".into());
            }
        }
    };

    let manifest = match parse_manifest(&payload) {
        Ok(m) => m,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };
    let dest = root.join(&manifest.name);
    if dest.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dest) {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!("既存の {} を削除できません: {e}", dest.display()));
        }
    }
    if let Err(e) = std::fs::rename(&payload, &dest) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("{} への配置に失敗: {e}", dest.display()));
    }
    let _ = std::fs::remove_dir_all(&tmp);
    parse_manifest(&dest)
}

/// プラグインを dest_dir/<name>-<version>.zvplug (ZIP) へエクスポートする。
pub fn export(plugin: &Plugin, dest_dir: &Path) -> Result<PathBuf, String> {
    let parent = plugin
        .dir
        .parent()
        .ok_or_else(|| "プラグインの親ディレクトリを特定できません".to_string())?;
    let folder = plugin
        .dir
        .file_name()
        .ok_or_else(|| "プラグインのディレクトリ名を特定できません".to_string())?
        .to_string_lossy()
        .to_string();
    let dest = dest_dir.join(format!("{}-{}.zvplug", plugin.name, plugin.version));
    if dest.exists() {
        std::fs::remove_file(&dest).map_err(|e| format!("既存ファイルを削除できません: {e}"))?;
    }

    let zip = Command::new("zip")
        .arg("-r")
        .arg("-q")
        .arg(&dest)
        .arg(&folder)
        .current_dir(parent)
        .output();
    if let Ok(out) = &zip {
        if out.status.success() {
            return Ok(dest);
        }
    }
    // macOS 標準の ditto へフォールバック
    let ditto = Command::new("ditto")
        .arg("-c")
        .arg("-k")
        .arg(&plugin.dir)
        .arg(&dest)
        .output();
    match ditto {
        Ok(out) if out.status.success() => Ok(dest),
        Ok(out) => Err(format!(
            "zip / ditto の両方でエクスポートに失敗: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(format!("zip / ditto を起動できません: {e}")),
    }
}

/// ~/.zaivern/plugins 配下のみアンインストールを許可。
pub fn uninstall(plugin_dir: &Path) -> Result<(), String> {
    let root = plugins_root().ok_or_else(|| "ホームディレクトリを特定できません".to_string())?;
    uninstall_at(&root, plugin_dir)
}

fn uninstall_at(root: &Path, plugin_dir: &Path) -> Result<(), String> {
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("{} を解決できません: {e}", root.display()))?;
    let canon_dir = plugin_dir
        .canonicalize()
        .map_err(|e| format!("{} を解決できません: {e}", plugin_dir.display()))?;
    if !canon_dir.starts_with(&canon_root) || canon_dir == canon_root {
        return Err(format!(
            "{} は plugins ディレクトリ配下ではないため削除できません",
            plugin_dir.display()
        ));
    }
    if !canon_dir.is_dir() {
        return Err(format!("{} はディレクトリではありません", plugin_dir.display()));
    }
    std::fs::remove_dir_all(&canon_dir).map_err(|e| format!("削除に失敗: {e}"))
}

fn copy_dir(src: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("{} を作成できません: {e}", dest.display()))?;
    let rd = std::fs::read_dir(src).map_err(|e| format!("{} を読めません: {e}", src.display()))?;
    for e in rd.flatten() {
        let from = e.path();
        let name = e.file_name();
        // 隠しファイル (.git 等) はコピーしない
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let to = dest.join(&name);
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|e| format!("{} をコピーできません: {e}", from.display()))?;
        }
    }
    Ok(())
}

/// unzip -o、失敗時 tar -xf (bsdtar) フォールバック。
fn extract_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let unzip = Command::new("unzip")
        .arg("-o")
        .arg("-q")
        .arg(archive)
        .arg("-d")
        .arg(dest)
        .output();
    if let Ok(out) = unzip {
        if out.status.success() {
            return Ok(());
        }
    }
    let tar = Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .output();
    match tar {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!(
            "unzip / tar の両方で解凍に失敗: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(format!("unzip / tar を起動できません: {e}")),
    }
}

// ─── コマンド実行 ────────────────────────────────────────────────

/// コマンド完了時に UI スレッドへ返す結果。
pub struct RunOutcome {
    pub plugin: String,
    /// 起動したコマンド (またはフック/パネル) の安定ID。
    /// 失敗時のログ・デバッグ用に必ず載せる (現状 UI では読んでいない)。
    #[allow(dead_code)]
    pub command_id: String,
    pub title: String,
    /// v1 互換の出力先。v2 専用の出力先では Silent。
    /// 実際の分岐は `sink` を見る。v1 の意味を失わないために残す。
    #[allow(dead_code)]
    pub output: CmdOutput,
    /// v2 の出力先 (こちらが正)。
    pub sink: CmdSink,
    /// `sink = Panel` のときの出力先パネルID。
    pub panel: Option<String>,
    /// フック由来ならその契機。ログ・デバッグ用に載せる。
    #[allow(dead_code)]
    pub event: Option<HookEvent>,
    /// `sink = Actions` のときに stdout から解釈したアクション列。
    pub actions: Vec<PluginAction>,
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    /// 反映先バッファ (Replace/Insert 用)。
    pub buffer_id: Option<u64>,
    /// input=selection のときの選択 char 範囲。None = ファイル全体。
    pub replace_range: Option<(usize, usize)>,
    /// 実行時に stdin へ渡したテキスト。適用前の照合に使う
    /// (実行中にバッファが編集されていたら黙って上書きしない)。
    pub original: String,
    /// 保存時フック由来: 置換後にファイルへ再保存する。
    pub resave: bool,
}

/// 実行要求 (UI スレッドで組み立ててワーカースレッドへ渡す)。
pub struct RunRequest {
    pub plugin: String,
    pub command: PluginCommand,
    pub stdin_text: String,
    pub envs: Vec<(String, String)>,
    pub workdir: PathBuf,
    pub buffer_id: Option<u64>,
    pub replace_range: Option<(usize, usize)>,
    pub resave: bool,
}

/// バックグラウンドスレッドでシェルコマンドを実行し、完了時に tx へ結果を送る。
/// タイムアウトすると kill して失敗として報告する。
pub fn run_async(req: RunRequest, tx: Sender<RunOutcome>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let outcome = run_blocking(&req);
        let _ = tx.send(outcome);
        ctx.request_repaint();
    });
}

fn run_blocking(req: &RunRequest) -> RunOutcome {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let fail = |msg: String| RunOutcome {
        plugin: req.plugin.clone(),
        command_id: req.command.id.clone(),
        title: req.command.title.clone(),
        output: req.command.output,
        sink: req.command.sink,
        panel: req.command.panel.clone(),
        event: req.command.event,
        actions: Vec::new(),
        ok: false,
        stdout: String::new(),
        stderr: msg,
        buffer_id: req.buffer_id,
        replace_range: req.replace_range,
        original: req.stdin_text.clone(),
        resave: req.resave,
    };

    let mut cmd = Command::new(&shell);
    cmd.arg("-lc")
        .arg(&req.command.run)
        .current_dir(&req.workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &req.envs {
        cmd.env(k, v);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail(format!("{shell} を起動できません: {e}")),
    };

    // stdin 書き込みは別スレッドへ (子が stdin を読まない場合に本スレッドが
    // write_all でブロックし、タイムアウト監視が止まるのを防ぐ)
    if let Some(mut si) = child.stdin.take() {
        let text = req.stdin_text.clone();
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = si.write_all(text.as_bytes());
        });
    }
    let out_reader = child.stdout.take().map(spawn_reader);
    let err_reader = child.stderr.take().map(spawn_reader);

    let deadline = Instant::now() + Duration::from_secs(req.command.timeout_secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(format!(
                        "{} 秒でタイムアウトしたため中断しました",
                        req.command.timeout_secs
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => break Err(format!("実行状態を取得できません: {e}")),
        }
    };

    let stdout = out_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = err_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    match status {
        Ok(st) => RunOutcome {
            plugin: req.plugin.clone(),
            command_id: req.command.id.clone(),
            title: req.command.title.clone(),
            output: req.command.output,
            sink: req.command.sink,
            panel: req.command.panel.clone(),
            event: req.command.event,
            actions: if req.command.sink == CmdSink::Actions && st.success() {
                parse_actions(&stdout)
            } else {
                Vec::new()
            },
            ok: st.success(),
            stdout,
            stderr,
            buffer_id: req.buffer_id,
            replace_range: req.replace_range,
            original: req.stdin_text.clone(),
            resave: req.resave,
        },
        Err(msg) => fail(if stderr.trim().is_empty() {
            msg
        } else {
            format!("{msg}: {}", stderr.trim())
        }),
    }
}

fn spawn_reader<R: std::io::Read + Send + 'static>(
    mut r: R,
) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!(
            "zaivern-plugins-test-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cmd_available(name: &str, arg: &str) -> bool {
        Command::new(name)
            .arg(arg)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn template_generates_valid_plugin() {
        let root = temp_dir("tpl");
        let dir = create_template_at(&root, "My-Plugin").expect("template ok");
        assert!(dir.ends_with("my-plugin"), "名前は小文字化される");
        let p = parse_manifest(&dir).expect("parse ok");
        assert_eq!(p.name, "my-plugin");
        assert_eq!(p.version, "0.1.0");
        assert_eq!(p.commands.len(), 2);
        assert_eq!(p.commands[0].input, CmdInput::Selection);
        assert_eq!(p.commands[0].output, CmdOutput::Replace);
        assert_eq!(p.commands[1].output, CmdOutput::Notify);
        assert_eq!(p.themes.len(), 1);
        assert!(p.themes[0].1.is_file(), "テンプレのテーマ JSON が実在する");
        assert_eq!(p.snippet_files.len(), 1);
        assert!(p.snippet_files[0].1.is_file());
        assert!(p.error.is_none());

        // 同名は拒否
        assert!(create_template_at(&root, "my-plugin").is_err());
        // 不正名は拒否
        assert!(create_template_at(&root, "日本語").is_err());
        assert!(create_template_at(&root, "").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 言語パックのマニフェストを読める() {
        let d = temp_dir("lang");
        std::fs::write(
            d.join("plugin.toml"),
            r#"
[plugin]
name = "my-lang"
default_enabled = false
[language]
id = "EN"
dict = "lang"
"#,
        )
        .unwrap();
        let p = parse_manifest(&d).expect("parse ok");
        assert!(!p.default_enabled, "default_enabled = false が読める");
        let l = p.language.expect("language section");
        assert_eq!(l.id, "en", "id は小文字化される");
        assert_eq!(l.name, "en", "name 省略時は id");
        assert_eq!(l.dict, d.join("lang"), "dict はプラグイン相対で解決される");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn 言語パックのidとdictは必須() {
        let d = temp_dir("langbad");
        std::fs::write(
            d.join("plugin.toml"),
            "[plugin]\nname = \"x\"\n[language]\nid = \"\"\ndict = \"lang\"\n",
        )
        .unwrap();
        assert!(parse_manifest(&d).unwrap_err().contains("id"));
        std::fs::write(
            d.join("plugin.toml"),
            "[plugin]\nname = \"x\"\n[language]\nid = \"en\"\ndict = \"\"\n",
        )
        .unwrap();
        assert!(parse_manifest(&d).unwrap_err().contains("dict"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 既存プラグイン (指定なし) の挙動が変わらないこと。
    #[test]
    fn default_enabledの既定はtrue() {
        let d = temp_dir("defen");
        std::fs::write(d.join("plugin.toml"), "[plugin]\nname = \"x\"\n").unwrap();
        let p = parse_manifest(&d).expect("parse ok");
        assert!(p.default_enabled);
        assert!(p.language.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// DoD: 同梱の english-mode がそのまま健全で、辞書が実際に読めること。
    /// ここが通らないと「プラグインは入るが英語にならない」という壊れ方をする。
    #[test]
    fn 同梱english_modeが健全で辞書が読める() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/plugins/english-mode");
        let p = parse_manifest(&dir).expect("english-mode manifest parses");
        assert!(!p.default_enabled, "初回は無効で入る (勝手に英語にしない)");
        let l = p.language.as_ref().expect("language section");
        assert_eq!(l.id, "en");
        let dict = crate::i18n::load_dict(&l.dict).expect("辞書が読める");
        assert!(
            dict.len() >= 20,
            "主要ラベルの訳が入っている (現在 {} 件)",
            dict.len()
        );
        assert_eq!(dict.get("設定").map(String::as_str), Some("Settings"));
    }

    #[test]
    fn parse_manifest_validation() {
        // on_save には input=file / output=replace が必要
        let d = temp_dir("val");
        std::fs::write(
            d.join("plugin.toml"),
            r#"
[plugin]
name = "bad"
[[command]]
title = "x"
run = "cat"
input = "selection"
output = "replace"
on_save = true
"#,
        )
        .unwrap();
        assert!(parse_manifest(&d).unwrap_err().contains("on_save"));

        // 不正な input 値
        std::fs::write(
            d.join("plugin.toml"),
            r#"
[plugin]
name = "bad"
[[command]]
title = "x"
run = "cat"
input = "clipboard"
"#,
        )
        .unwrap();
        assert!(parse_manifest(&d).unwrap_err().contains("input"));

        // 最小マニフェスト (デフォルト適用)
        std::fs::write(
            d.join("plugin.toml"),
            "[plugin]\nname = \"mini\"\n[[command]]\ntitle = \"t\"\nrun = \"true\"\n",
        )
        .unwrap();
        let p = parse_manifest(&d).expect("parse ok");
        assert_eq!(p.version, "0.1.0");
        assert_eq!(p.commands[0].input, CmdInput::None);
        assert_eq!(p.commands[0].output, CmdOutput::Notify);
        assert_eq!(p.commands[0].timeout_secs, 30);
        assert!(p.commands[0].lang_matches("rust"), "langs 空 = 全言語");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn scan_reports_broken_manifest() {
        let root = temp_dir("scan");
        let ok = create_template_at(&root, "good").unwrap();
        let broken = root.join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("plugin.toml"), "this is not toml [").unwrap();
        // plugin.toml の無いディレクトリは無視される
        std::fs::create_dir_all(root.join("not-a-plugin")).unwrap();

        let list = scan_root(&root);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "broken");
        assert!(list[0].error.is_some());
        assert_eq!(list[1].name, "good");
        assert!(list[1].error.is_none());
        assert_eq!(list[1].dir, ok);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_export_uninstall_roundtrip() {
        if !cmd_available("zip", "-v") || !cmd_available("unzip", "-v") {
            eprintln!("zip/unzip 不在のためスキップ");
            return;
        }
        let root = temp_dir("inst");
        let stage = temp_dir("stage");
        let src = create_template_at(&stage, "roundtrip").unwrap();

        // ディレクトリからインストール
        let p = install_at(&root, &src).expect("dir install ok");
        assert_eq!(p.name, "roundtrip");
        assert!(p.dir.starts_with(&root));
        assert!(p.themes[0].1.is_file(), "テーマもコピーされる");

        // エクスポート → zip インストール
        let exported = export(&p, &stage).expect("export ok");
        assert!(exported.is_file());
        assert!(exported.to_string_lossy().ends_with("roundtrip-0.1.0.zvplug"));
        uninstall_at(&root, &p.dir).expect("uninstall ok");
        assert!(!p.dir.exists());
        let p2 = install_at(&root, &exported).expect("zip install ok");
        assert_eq!(p2.name, "roundtrip");
        assert!(p2.themes[0].1.is_file());

        // plugins ディレクトリ外は削除拒否
        assert!(uninstall_at(&root, &stage).is_err());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&stage);
    }

    fn run_sync(req: RunRequest) -> RunOutcome {
        run_blocking(&req)
    }

    fn basic_cmd(run: &str, timeout: u64) -> PluginCommand {
        PluginCommand {
            id: "t".into(),
            title: "test".into(),
            icon: "🔌".into(),
            run: run.into(),
            input: CmdInput::Selection,
            output: CmdOutput::Replace,
            sink: CmdSink::Replace,
            panel: None,
            langs: Vec::new(),
            keybind: None,
            on_save: false,
            timeout_secs: timeout,
            event: None,
        }
    }

    // ─── v2: 後方互換 ────────────────────────────────────────────

    #[test]
    fn v1_manifest_parses_identically() {
        let d = temp_dir("v1compat");
        std::fs::write(
            d.join("plugin.toml"),
            r#"
[plugin]
name = "legacy"
version = "1.2.3"
author = "me"
description = "説明"

[[command]]
id = "upper"
title = "大文字化"
icon = "🔠"
run = "tr a-z A-Z"
input = "selection"
output = "replace"
langs = ["Rust"]
keybind = "cmd+alt+u"
timeout_secs = 5

[[command]]
title = "wc"
run = "wc -l"
input = "file"
output = "notify"

[[theme]]
label = "dark"
path = "themes/x.json"

[[snippet]]
language = "Rust"
path = "snippets/r.json"
"#,
        )
        .unwrap();
        let p = parse_manifest(&d).expect("v1 は無改造で通る");
        assert_eq!(p.api, 1, "api 省略時は 1");
        assert!(p.enabled);
        assert_eq!(p.version, "1.2.3");
        assert_eq!(p.commands.len(), 2);
        assert_eq!(p.commands[0].id, "upper");
        assert_eq!(p.commands[0].input, CmdInput::Selection);
        assert_eq!(p.commands[0].output, CmdOutput::Replace);
        assert_eq!(p.commands[0].sink, CmdSink::Replace);
        assert_eq!(p.commands[0].langs, vec!["rust"]);
        assert_eq!(p.commands[0].keybind.as_deref(), Some("cmd+alt+u"));
        assert_eq!(p.commands[0].timeout_secs, 5);
        assert!(p.commands[0].panel.is_none());
        assert!(p.commands[0].event.is_none());
        // v1 に無かったセクションは空
        assert!(p.hooks.is_empty() && p.panels.is_empty() && p.settings.is_empty());
        assert_eq!(p.themes.len(), 1);
        assert_eq!(p.themes[0].0, "dark");
        assert_eq!(p.snippet_files[0].0, "rust");
        // id 省略時は title の slug
        assert_eq!(p.commands[1].id, "wc");
        let _ = std::fs::remove_dir_all(&d);
    }

    // ─── v2: 新セクション ────────────────────────────────────────

    #[test]
    fn v2_manifest_parses() {
        let d = temp_dir("v2");
        std::fs::write(
            d.join("plugin.toml"),
            r#"
[plugin]
name = "v2plug"
version = "0.2.0"
api = 2

[[command]]
id = "fmt"
title = "整形"
run = "cat"
input = "file"
output = "replace"

[[command]]
title = "Send To Agent"
run = "echo hi"
output = "agent_prompt"

[[command]]
title = "タスク一覧"
run = "echo x"
output = "panel"
panel = "tasks"

[[command]]
title = "アクション"
run = "echo x"
output = "actions"

[[hook]]
event = "file_save"
run = "echo saved"
output = "actions"

[[hook]]
event = "interval"
run = "echo tick"
interval_secs = 60
output = "panel"
panel = "tasks"

[[panel]]
id = "tasks"
title = "タスク"
icon = "📋"
run = ""
refresh = "manual"
format = "markdown"

[[setting]]
key = "token"
type = "string"
default = ""
label = "APIトークン"
secret = true

[[setting]]
key = "retries"
type = "int"
default = 3

[[setting]]
key = "verbose"
type = "bool"
default = true
"#,
        )
        .unwrap();
        let p = parse_manifest(&d).expect("v2 parse ok");
        assert_eq!(p.api, 2);
        assert_eq!(p.commands.len(), 4);
        assert_eq!(p.commands[0].id, "fmt");
        // 新出力先は v1 経路では Silent に落ちる (誤発火を防ぐ)
        assert_eq!(p.commands[1].id, "send-to-agent", "title から slug 生成");
        assert_eq!(p.commands[1].sink, CmdSink::AgentPrompt);
        assert_eq!(p.commands[1].output, CmdOutput::Silent);
        assert_eq!(p.commands[2].sink, CmdSink::Panel);
        assert_eq!(p.commands[2].panel.as_deref(), Some("tasks"));
        // 日本語のみの title は slug が空 → cmd{i} へフォールバック
        assert_eq!(p.commands[2].id, "cmd2");
        assert_eq!(p.commands[3].sink, CmdSink::Actions);

        assert_eq!(p.hooks.len(), 2);
        assert_eq!(p.hooks[0].event, HookEvent::FileSave);
        assert_eq!(p.hooks[0].sink, CmdSink::Actions);
        assert_eq!(p.hooks[0].interval_secs, 0);
        assert_eq!(p.hooks[1].event, HookEvent::Interval);
        assert_eq!(p.hooks[1].interval_secs, 60);
        assert_eq!(p.hooks[1].panel.as_deref(), Some("tasks"));

        assert_eq!(p.panels.len(), 1);
        assert_eq!(p.panels[0].id, "tasks");
        assert_eq!(p.panels[0].refresh, PanelRefresh::Manual);
        assert_eq!(p.panels[0].format, PanelFormat::Markdown);

        assert_eq!(p.settings.len(), 3);
        assert_eq!(p.settings[0].kind, SettingType::Str);
        assert!(p.settings[0].secret);
        assert_eq!(p.setting("retries"), "3", "既定値で初期化される");
        assert_eq!(p.setting("verbose"), "true");
        assert_eq!(p.settings[1].env_key(), "ZV_CFG_RETRIES");

        // ID 引きが効く
        let list = vec![p];
        assert!(list.find_command("v2plug", "fmt").is_some());
        assert!(list.find_command("v2plug", "nope").is_none());
        assert!(list.find_command("other", "fmt").is_none());
        assert!(list.find_panel("v2plug", "tasks").is_some());
        assert_eq!(list.active_commands().len(), 4);
        assert_eq!(list.active_hooks(HookEvent::FileSave).len(), 1);
        assert_eq!(list.active_hooks(HookEvent::Startup).len(), 0);
        assert_eq!(list.active_panels().len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn v2_validation_rejects_bad_values() {
        let d = temp_dir("v2bad");
        let write = |body: &str| std::fs::write(d.join("plugin.toml"), body).unwrap();
        let head = "[plugin]\nname = \"bad\"\napi = 2\n";

        // 未対応の api
        write("[plugin]\nname = \"bad\"\napi = 99\n");
        assert!(parse_manifest(&d).unwrap_err().contains("api"));

        // 未知の output
        write(&format!(
            "{head}[[command]]\ntitle = \"t\"\nrun = \"x\"\noutput = \"teleport\"\n"
        ));
        assert!(parse_manifest(&d).unwrap_err().contains("output"));

        // output = panel なのに panel 未指定
        write(&format!(
            "{head}[[command]]\ntitle = \"t\"\nrun = \"x\"\noutput = \"panel\"\n"
        ));
        assert!(parse_manifest(&d).unwrap_err().contains("panel"));

        // 存在しないパネルID を参照
        write(&format!(
            "{head}[[panel]]\nid = \"a\"\n[[command]]\ntitle = \"t\"\nrun = \"x\"\noutput = \"panel\"\npanel = \"zzz\"\n"
        ));
        assert!(parse_manifest(&d).unwrap_err().contains("[[panel]]"));

        // 未知の event
        write(&format!("{head}[[hook]]\nevent = \"lunch\"\nrun = \"x\"\n"));
        assert!(parse_manifest(&d).unwrap_err().contains("event"));

        // interval は interval_secs >= 5
        write(&format!(
            "{head}[[hook]]\nevent = \"interval\"\nrun = \"x\"\ninterval_secs = 1\n"
        ));
        assert!(parse_manifest(&d).unwrap_err().contains("interval_secs"));
        write(&format!(
            "{head}[[hook]]\nevent = \"interval\"\nrun = \"x\"\n"
        ));
        assert!(parse_manifest(&d).unwrap_err().contains("interval_secs"));
        write(&format!(
            "{head}[[hook]]\nevent = \"interval\"\nrun = \"x\"\ninterval_secs = 5\n"
        ));
        assert_eq!(parse_manifest(&d).unwrap().hooks[0].interval_secs, 5);

        // hook の output に replace は許さない
        write(&format!(
            "{head}[[hook]]\nevent = \"startup\"\nrun = \"x\"\noutput = \"replace\"\n"
        ));
        assert!(parse_manifest(&d).unwrap_err().contains("output"));

        // setting の型と default の不一致
        write(&format!(
            "{head}[[setting]]\nkey = \"n\"\ntype = \"int\"\ndefault = \"three\"\n"
        ));
        assert!(parse_manifest(&d).unwrap_err().contains("default"));
        write(&format!("{head}[[setting]]\nkey = \"n\"\ntype = \"色\"\n"));
        assert!(parse_manifest(&d).unwrap_err().contains("type"));

        // パネルID の重複 / 不正
        write(&format!("{head}[[panel]]\nid = \"a\"\n[[panel]]\nid = \"a\"\n"));
        assert!(parse_manifest(&d).unwrap_err().contains("重複"));
        write(&format!("{head}[[panel]]\nid = \"タスク\"\n"));
        assert!(parse_manifest(&d).unwrap_err().contains("id"));

        // 不正な設定でもプラグイン一覧からは消えず、error 付きで残る
        let root = temp_dir("v2bad-root");
        let inner = root.join("bad");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("plugin.toml"), "[plugin]\nname = \"bad\"\napi = 99\n").unwrap();
        let list = scan_root(&root);
        assert_eq!(list.len(), 1);
        assert!(list[0].error.as_deref().unwrap_or_default().contains("api"));
        assert!(!list[0].active(), "壊れていれば有効でも登録しない");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn duplicate_command_ids_get_suffixed() {
        let d = temp_dir("dupid");
        std::fs::write(
            d.join("plugin.toml"),
            r#"
[plugin]
name = "dup"
[[command]]
title = "Run"
run = "a"
[[command]]
title = "run"
run = "b"
[[command]]
id = "run"
title = "three"
run = "c"
"#,
        )
        .unwrap();
        let p = parse_manifest(&d).unwrap();
        assert_eq!(p.commands[0].id, "run");
        assert_eq!(p.commands[1].id, "run-2");
        assert_eq!(p.commands[2].id, "run-3");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn slug_generation() {
        assert_eq!(slug("Send To Agent"), "send-to-agent");
        assert_eq!(slug("  Format JSON!  "), "format-json");
        assert_eq!(slug("a__b--c"), "a-b-c");
        assert_eq!(slug("整形する"), "", "非 ASCII のみなら空");
        assert_eq!(slug("整形 fmt"), "fmt");
        assert_eq!(slug(""), "");
        assert!(slug(&"x".repeat(200)).len() <= 64);
    }

    // ─── v2: アクションプロトコル ────────────────────────────────

    #[test]
    fn parse_actions_json_lines() {
        let out = r#"
{"action":"open_file","path":"src/main.rs","line":42}
{"action":"notify","message":"完了","level":"warn"}
{"action":"notify","message":"既定は info"}
{"action":"insert_text","text":"abc"}
{"action":"replace_buffer","text":"whole"}
{"action":"new_tab","title":"結果","text":"body"}
{"action":"agent_prompt","agent":"claude","text":"やって","submit":true}
{"action":"agent_prompt","text":"送信しない"}
{"action":"run_terminal","command":"cargo test","cwd":"."}
{"action":"open_url","url":"https://example.com"}
{"action":"set_panel","panel":"Tasks","text":"t"}
{"action":"set_status","text":"s"}
{"action":"refresh_files"}
{"action":"set_setting","key":"token","value":"xxx"}
"#;
        let a = parse_actions(out);
        assert_eq!(a.len(), 14);
        assert_eq!(
            a[0],
            PluginAction::OpenFile { path: "src/main.rs".into(), line: Some(42) }
        );
        assert_eq!(
            a[1],
            PluginAction::Notify { message: "完了".into(), level: NotifyLevel::Warn }
        );
        assert!(matches!(&a[2], PluginAction::Notify { level: NotifyLevel::Info, .. }));
        assert_eq!(a[3], PluginAction::InsertText { text: "abc".into() });
        assert_eq!(a[4], PluginAction::ReplaceBuffer { text: "whole".into() });
        assert_eq!(a[5], PluginAction::NewTab { title: "結果".into(), text: "body".into() });
        assert_eq!(
            a[6],
            PluginAction::AgentPrompt {
                agent: Some("claude".into()),
                text: "やって".into(),
                submit: true
            }
        );
        assert_eq!(
            a[7],
            PluginAction::AgentPrompt { agent: None, text: "送信しない".into(), submit: false },
            "submit の既定は false"
        );
        assert_eq!(
            a[8],
            PluginAction::RunTerminal { command: "cargo test".into(), cwd: Some(".".into()) }
        );
        assert_eq!(a[9], PluginAction::OpenUrl { url: "https://example.com".into() });
        assert_eq!(
            a[10],
            PluginAction::SetPanel { panel: "tasks".into(), text: "t".into() },
            "パネルIDは小文字化される"
        );
        assert_eq!(a[11], PluginAction::SetStatus { text: "s".into() });
        assert_eq!(a[12], PluginAction::RefreshFiles);
        assert_eq!(a[13], PluginAction::SetSetting { key: "token".into(), value: "xxx".into() });
    }

    #[test]
    fn parse_actions_skips_malformed_lines() {
        let out = concat!(
            "これはJSONではない\n",
            "{\"action\":\"open_file\"}\n",            // path 欠落
            "{\"action\":\"unknown_thing\"}\n",        // 未知アクション
            "{\"message\":\"action キーが無い\"}\n",
            "{broken json\n",
            "[1,2,3]\n",                                // オブジェクトでない
            "\n",
            "   \n",
            "{\"action\":\"set_status\",\"text\":\"ok\"}\n",
            "{\"action\":\"run_terminal\"}\n",         // command 欠落
        );
        let a = parse_actions(out);
        assert_eq!(a, vec![PluginAction::SetStatus { text: "ok".into() }]);
        assert!(parse_actions("").is_empty());
    }

    // ─── v2: 有効/無効・設定 ─────────────────────────────────────

    #[test]
    fn disabled_filtering_and_settings() {
        let root = temp_dir("disable");
        for n in ["alpha", "beta"] {
            let d = root.join(n);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("plugin.toml"),
                format!(
                    "[plugin]\nname = \"{n}\"\napi = 2\n\
                     [[command]]\nid = \"go\"\ntitle = \"go\"\nrun = \"x\"\n\
                     [[panel]]\nid = \"p\"\nrun = \"y\"\n\
                     [[hook]]\nevent = \"startup\"\nrun = \"z\"\n\
                     [[setting]]\nkey = \"token\"\ntype = \"string\"\ndefault = \"d\"\n\
                     [[setting]]\nkey = \"n\"\ntype = \"int\"\ndefault = 1\n"
                ),
            )
            .unwrap();
        }
        let mut list = scan_root(&root);
        assert_eq!(list.len(), 2);
        assert_eq!(list.active_commands().len(), 2);

        list.apply_disabled(&["beta".to_string()]);
        assert!(list.find_plugin("alpha").unwrap().enabled);
        assert!(!list.find_plugin("beta").unwrap().enabled);
        // 無効側はコマンド・フック・パネルを一切登録しない
        assert_eq!(list.active_commands().len(), 1);
        assert_eq!(list.active_hooks(HookEvent::Startup).len(), 1);
        assert_eq!(list.active_panels().len(), 1);
        // 一覧には残るので再有効化できる
        assert!(list.find_command("beta", "go").is_some());
        list.apply_disabled(&[]);
        assert_eq!(list.active_commands().len(), 2);

        // 設定の適用: 型に合わない値は既定値のまま
        let mut map: HashMap<String, HashMap<String, String>> = HashMap::new();
        map.insert(
            "alpha".into(),
            HashMap::from([
                ("token".to_string(), "secret".to_string()),
                ("n".to_string(), "not-a-number".to_string()),
                ("unknown".to_string(), "ignored".to_string()),
            ]),
        );
        list.apply_all_settings(&map);
        let a = list.find_plugin("alpha").unwrap();
        assert_eq!(a.setting("token"), "secret");
        assert_eq!(a.setting("n"), "1", "不正値は既定値に戻す");
        assert_eq!(a.setting("missing"), "");
        assert_eq!(list.find_plugin("beta").unwrap().setting("token"), "d");

        // 環境変数一式
        let ws = PathBuf::from("/ws");
        let env = command_env(
            a,
            &EnvContext {
                file: Some(Path::new("/ws/src/main.rs")),
                lang: "rust",
                workspace: &ws,
                selection: "sel",
                line: 0,
                column: 7,
                agent: "claude",
                event: Some(HookEvent::FileSave),
                git_branch: "main",
            },
        );
        let get = |k: &str| {
            env.iter()
                .find(|(a, _)| a == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("ZV_FILE"), "/ws/src/main.rs");
        assert_eq!(get("ZV_LANG"), "rust");
        assert_eq!(get("ZV_WORKSPACE"), "/ws");
        assert_eq!(get("ZV_PLUGIN_DIR"), a.dir.display().to_string());
        assert_eq!(get("ZV_API"), "2");
        assert!(!get("ZV_BIN").is_empty());
        assert!(get("ZV_PLUGIN_DATA").ends_with("plugin-data/alpha"));
        assert_eq!(get("ZV_SELECTION"), "sel");
        assert_eq!(get("ZV_LINE"), "1", "0 は 1 に丸める");
        assert_eq!(get("ZV_COLUMN"), "7");
        assert_eq!(get("ZV_AGENT"), "claude");
        assert_eq!(get("ZV_EVENT"), "file_save");
        assert_eq!(get("ZV_GIT_BRANCH"), "main");
        assert_eq!(get("ZV_CFG_TOKEN"), "secret");
        assert_eq!(get("ZV_CFG_N"), "1");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ─── v2: バンドル展開 ────────────────────────────────────────

    fn bundle(ver: &str) -> Vec<(String, Vec<(String, String)>)> {
        vec![(
            "std-demo".to_string(),
            vec![
                (
                    "plugin.toml".to_string(),
                    format!("[plugin]\nname = \"std-demo\"\nversion = \"{ver}\"\napi = 2\n"),
                ),
                ("run.sh".to_string(), format!("#!/bin/sh\necho {ver}\n")),
            ],
        )]
    }

    fn as_table(b: &[(String, Vec<(String, String)>)]) -> Vec<(&str, Vec<(&str, &str)>)> {
        b.iter()
            .map(|(n, f)| {
                (
                    n.as_str(),
                    f.iter().map(|(a, c)| (a.as_str(), c.as_str())).collect(),
                )
            })
            .collect()
    }

    fn seed(root: &Path, b: &[(String, Vec<(String, String)>)]) -> Vec<String> {
        let owned = as_table(b);
        let table: Vec<(&str, &[(&str, &str)])> =
            owned.iter().map(|(n, f)| (*n, f.as_slice())).collect();
        seed_bundled_from(root, &table)
    }

    #[test]
    fn bundled_seeding_respects_version_stamp() {
        let root = temp_dir("bundle");
        let dir = root.join("std-demo");

        // 初回: 展開される
        let v1 = bundle("1.0.0");
        assert_eq!(seed(&root, &v1), vec!["std-demo".to_string()]);
        assert!(dir.join("plugin.toml").is_file());
        assert_eq!(std::fs::read_to_string(dir.join(".bundled")).unwrap(), "1.0.0");
        assert!(parse_manifest(&dir).is_ok(), "展開物がそのまま解析できる");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("run.sh")).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, ".sh に実行権限が付く");
        }

        // 同版の再実行: ユーザーの編集を潰さない
        std::fs::write(dir.join("run.sh"), "# ユーザーの編集\n").unwrap();
        assert!(seed(&root, &v1).is_empty(), "同版なら何もしない");
        assert_eq!(
            std::fs::read_to_string(dir.join("run.sh")).unwrap(),
            "# ユーザーの編集\n"
        );

        // 古い版は上書きしない
        assert!(seed(&root, &bundle("0.9.0")).is_empty());
        assert_eq!(std::fs::read_to_string(dir.join(".bundled")).unwrap(), "1.0.0");

        // 新しい版なら再展開
        let v2 = bundle("1.0.1");
        assert_eq!(seed(&root, &v2), vec!["std-demo".to_string()]);
        assert!(std::fs::read_to_string(dir.join("run.sh")).unwrap().contains("1.0.1"));
        assert_eq!(std::fs::read_to_string(dir.join(".bundled")).unwrap(), "1.0.1");

        // .bundled を消したら再展開される (取り込み漏れの復旧)
        std::fs::remove_file(dir.join(".bundled")).unwrap();
        assert_eq!(seed(&root, &v2), vec!["std-demo".to_string()]);

        // 標準プラグインは scan にも載る
        assert!(scan_root(&root).iter().any(|p| p.name == "std-demo"));

        // 出荷テーブルは (空でも) 破綻しない
        let empty = temp_dir("bundle-empty");
        let _ = seed_bundled(&empty);
        let _ = std::fs::remove_dir_all(&empty);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn version_compare_and_extraction() {
        assert!(version_newer("1.0.1", "1.0.0"));
        assert!(version_newer("1.1.0", "1.0.9"));
        assert!(version_newer("2.0", "1.9.9"));
        assert!(!version_newer("1.0.0", "1.0.0"));
        assert!(!version_newer("1.0.0", "1.0.1"));
        assert!(!version_newer("0.1", "0.1.0"));
        assert_eq!(manifest_version("[plugin]\nversion = \"3.4.5\"\n"), "3.4.5");
        assert_eq!(manifest_version("version='0.2.0'"), "0.2.0");
        assert_eq!(manifest_version("[plugin]\nname = \"x\"\n"), "0.0.0");
    }

    #[test]
    fn run_pipes_stdin_to_stdout() {
        let out = run_sync(RunRequest {
            plugin: "p".into(),
            command: basic_cmd("tr '[:lower:]' '[:upper:]'", 10),
            stdin_text: "hello".into(),
            envs: vec![("ZV_LANG".into(), "rust".into())],
            workdir: std::env::temp_dir(),
            buffer_id: Some(1),
            replace_range: Some((0, 5)),
            resave: false,
        });
        assert!(out.ok, "stderr: {}", out.stderr);
        assert_eq!(out.stdout.trim(), "HELLO");
        assert_eq!(out.original, "hello");
        assert_eq!(out.replace_range, Some((0, 5)));
    }

    #[test]
    fn run_reports_failure_and_timeout() {
        let out = run_sync(RunRequest {
            plugin: "p".into(),
            command: basic_cmd("echo boom >&2; exit 3", 10),
            stdin_text: String::new(),
            envs: Vec::new(),
            workdir: std::env::temp_dir(),
            buffer_id: None,
            replace_range: None,
            resave: false,
        });
        assert!(!out.ok);
        assert!(out.stderr.contains("boom"));

        let started = Instant::now();
        let out = run_sync(RunRequest {
            plugin: "p".into(),
            command: basic_cmd("sleep 30", 1),
            stdin_text: String::new(),
            envs: Vec::new(),
            workdir: std::env::temp_dir(),
            buffer_id: None,
            replace_range: None,
            resave: false,
        });
        assert!(!out.ok);
        assert!(out.stderr.contains("タイムアウト"));
        assert!(started.elapsed() < Duration::from_secs(10), "kill が効いている");
    }
}
