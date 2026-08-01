//! MCP (Model Context Protocol) サーバ設定の**発見・解析・有効/無効の切り替え**。
//!
//! 各 CLI エージェント (Claude Code / Codex / Gemini CLI / Cursor / VS Code) は
//! それぞれ別のファイルに MCP サーバを書く。利用者から見ると「同じサーバを
//! どこに書いたか分からない」「どのエージェントに効いているのか分からない」
//! という 1 つの問題なので、ここで**1 枚の表**に畳む。
//!
//! 設計上の要点:
//!
//! - **env / headers の値は構造体にすら入れない。** 持たないものは漏れない。
//!   `SecretKey` はキー名と「設定済みか」だけを持ち、URL も
//!   [`redact_url`] でクエリと userinfo を落としてから表示する。
//!   表示に出る文字列は全て [`detail_lines`] が作るので、
//!   「値が出ていないこと」はその 1 関数のテストで固定できる。
//! - **壊れた JSON で panic しない。** 読めない理由は [`FileState::Broken`]
//!   という状態として持ち、パネルに理由ごと出す (握り潰さない)。
//! - **書き戻しは既存ファイルを壊さない。** serde で往復させるとキー順も
//!   コメントも消えるので、[`toggle_disabled`] は生テキストへの**外科的な
//!   差し替え**を行い、結果を再解析して確かめてから返す。TOML は
//!   書式保存つき編集器を持たないので**読み取り専用**に倒す。
//! - **走査は要求されたときだけ。** `~/.claude.json` は 100KB 級になるので、
//!   毎フレーム読むことは決してしない (設計原則 3: アイドルのコストはゼロ)。

use std::path::{Path, PathBuf};

use eframe::egui::{self, RichText};

use crate::i18n::{tr, trf};
use crate::jsonc::strip_jsonc;
use crate::panels::space;
use crate::theme::Theme;

/// 無効化に使うキー名。Cursor / Cline 系が使う事実上の標準。
const DISABLED_KEY: &str = "disabled";

/// 設定ファイルとして読み込む上限。これを超えるものは「設定ファイルではない」
/// と見なして読まない (壊れた巨大ファイルで UI を止めないため)。
const MAX_CONFIG_BYTES: u64 = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// 出典 (どのファイルの、どのエージェント向けの設定か)
// ---------------------------------------------------------------------------

/// 設定ファイルの記法。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigFormat {
    /// JSON / JSONC (コメント・末尾カンマ可)
    Json,
    /// TOML
    Toml,
}

/// この設定が効くエージェント。
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum AgentKind {
    Claude,
    Codex,
    Gemini,
    Cursor,
    VsCode,
}

impl AgentKind {
    /// チップに出す短い名前。**絵文字を使わない** — 環境によって豆腐になる
    /// アイコンで「どのエージェントに効くか」を表すと意味が消えるため。
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::Claude => "Claude",
            AgentKind::Codex => "Codex",
            AgentKind::Gemini => "Gemini",
            AgentKind::Cursor => "Cursor",
            AgentKind::VsCode => "VS Code",
        }
    }
}

/// MCP 設定の出典。
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum ConfigSource {
    /// ワークスペース直下の `.mcp.json` (Claude Code のプロジェクト設定)
    ProjectMcpJson,
    /// ワークスペースの `.cursor/mcp.json`
    ProjectCursor,
    /// ワークスペースの `.vscode/mcp.json`
    ProjectVscode,
    /// ホームの `.claude.json` の `mcpServers`
    UserClaude,
    /// ホームの `.codex/config.toml` の `mcp_servers`
    UserCodex,
    /// ホームの `.gemini/settings.json`
    UserGemini,
    /// ホームの `.cursor/mcp.json`
    UserCursor,
}

/// ワークスペース直下を見る出典。
pub const PROJECT_SOURCES: &[ConfigSource] = &[
    ConfigSource::ProjectMcpJson,
    ConfigSource::ProjectCursor,
    ConfigSource::ProjectVscode,
];

/// ホーム直下を見る出典。
pub const USER_SOURCES: &[ConfigSource] = &[
    ConfigSource::UserClaude,
    ConfigSource::UserCodex,
    ConfigSource::UserGemini,
    ConfigSource::UserCursor,
];

impl ConfigSource {
    /// 記法。
    pub fn format(self) -> ConfigFormat {
        match self {
            ConfigSource::UserCodex => ConfigFormat::Toml,
            _ => ConfigFormat::Json,
        }
    }

    /// 有効/無効の書き戻しに対応しているか。
    ///
    /// TOML はキー順とコメントを保ったまま書き戻す手段を持たないので
    /// **読み取り専用**にする (安全側)。
    pub fn editable(self) -> bool {
        self.format() == ConfigFormat::Json
    }

    /// サーバの表が置かれているキーの**候補**。先に見つかったものを使う。
    /// 実ファイルを確認した結果に基づく:
    ///   `.mcp.json` / `.cursor/mcp.json` / `.claude.json` → `mcpServers`
    ///   `.vscode/mcp.json` → `servers` (旧 `mcp.servers` も見る)
    ///   `.codex/config.toml` → `mcp_servers`
    pub fn container_paths(self) -> &'static [&'static [&'static str]] {
        match self {
            ConfigSource::ProjectVscode => &[&["servers"], &["mcp", "servers"], &["mcpServers"]],
            ConfigSource::UserGemini => &[&["mcpServers"], &["mcp", "servers"]],
            ConfigSource::UserCodex => &[&["mcp_servers"]],
            _ => &[&["mcpServers"]],
        }
    }

    /// この出典が効くエージェント。
    pub fn agents(self) -> &'static [AgentKind] {
        match self {
            ConfigSource::ProjectMcpJson | ConfigSource::UserClaude => &[AgentKind::Claude],
            ConfigSource::ProjectCursor | ConfigSource::UserCursor => &[AgentKind::Cursor],
            ConfigSource::ProjectVscode => &[AgentKind::VsCode],
            ConfigSource::UserCodex => &[AgentKind::Codex],
            ConfigSource::UserGemini => &[AgentKind::Gemini],
        }
    }

    /// 一覧に出す短い出典名 (日本語原文。表示側で `tr` を通す)。
    pub fn label(self) -> &'static str {
        match self {
            ConfigSource::ProjectMcpJson => "プロジェクト .mcp.json",
            ConfigSource::ProjectCursor => "プロジェクト .cursor",
            ConfigSource::ProjectVscode => "プロジェクト .vscode",
            ConfigSource::UserClaude => "ユーザー .claude.json",
            ConfigSource::UserCodex => "ユーザー .codex",
            ConfigSource::UserGemini => "ユーザー .gemini",
            ConfigSource::UserCursor => "ユーザー .cursor",
        }
    }

    /// ワークスペース `root` 配下での実ファイルパス。
    ///
    /// 区切り文字は書かず `Path::join` だけで組む (Windows / Unix 共通)。
    pub fn project_path(self, root: &Path) -> Option<PathBuf> {
        Some(match self {
            ConfigSource::ProjectMcpJson => root.join(".mcp.json"),
            ConfigSource::ProjectCursor => root.join(".cursor").join("mcp.json"),
            ConfigSource::ProjectVscode => root.join(".vscode").join("mcp.json"),
            _ => return None,
        })
    }

    /// ホーム配下での実ファイルパス。ホームが取れない環境では `None`。
    pub fn user_path(self) -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        Some(match self {
            ConfigSource::UserClaude => home.join(".claude.json"),
            ConfigSource::UserCodex => home.join(".codex").join("config.toml"),
            ConfigSource::UserGemini => home.join(".gemini").join("settings.json"),
            ConfigSource::UserCursor => home.join(".cursor").join("mcp.json"),
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// サーバ記述子
// ---------------------------------------------------------------------------

/// トランスポート種別。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Transport {
    /// 子プロセスを起動して標準入出力で話す
    Stdio {
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
    },
    /// HTTP (streamable-http)。URL は [`redact_url`] を**通した後**の値
    /// (クエリや userinfo にトークンが入るため、生のまま持たない)
    Http { url: String },
    /// Server-Sent Events。URL は [`redact_url`] を通した後の値
    Sse { url: String },
    /// 判定できない (`command` も `url` も無い等)
    Unknown,
}

impl Transport {
    /// 種別バッジの文字。
    pub fn badge(&self) -> &'static str {
        match self {
            Transport::Stdio { .. } => "stdio",
            Transport::Http { .. } => "http",
            Transport::Sse { .. } => "sse",
            Transport::Unknown => "?",
        }
    }
}

/// **値を持たない**秘密キーの記述子。
///
/// `env` / `headers` はトークンそのものが入る場所なので、
/// 値はパースの時点で捨てる。持たない値は漏れない。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SecretKey {
    /// キー名 (これは出してよい)
    pub name: String,
    /// 値が入っているか。中身は保持しない
    pub set: bool,
}

/// MCP サーバ 1 つ。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpServer {
    /// 設定上の名前
    pub name: String,
    pub transport: Transport,
    /// env のキー名だけ (値は保持しない)
    pub env: Vec<SecretKey>,
    /// HTTP ヘッダのキー名だけ (値は保持しない)
    pub headers: Vec<SecretKey>,
    /// `"disabled": true` が付いているか
    pub disabled: bool,
    pub source: ConfigSource,
    /// 出典ファイル。[`parse_mcp_config`] は空のまま返し、
    /// ファイルから読む [`load_file`] が埋める
    pub path: PathBuf,
}

impl McpServer {
    /// この設定が効くエージェント。
    pub fn agents(&self) -> &'static [AgentKind] {
        self.source.agents()
    }
}

/// ファイル 1 枚の読み取り状態。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FileState {
    /// ファイルが無い (**エラーではない**)
    Missing,
    /// 読めた
    Ok,
    /// 読めない / 壊れている。理由を持つ
    Broken(String),
}

/// ファイル 1 枚の走査結果。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ConfigFile {
    pub source: ConfigSource,
    pub path: PathBuf,
    pub state: FileState,
    pub servers: Vec<McpServer>,
}

/// 全出典を横断した一覧。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Inventory {
    pub files: Vec<ConfigFile>,
}

impl Inventory {
    /// 名前順に並べたサーバ一覧 (同名は出典順)。
    pub fn servers(&self) -> Vec<&McpServer> {
        let mut v: Vec<&McpServer> = self.files.iter().flat_map(|f| f.servers.iter()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.source.cmp(&b.source)));
        v
    }

    /// サーバ件数。**0 のときバッジを出さない**判断に使う。
    pub fn count(&self) -> usize {
        self.files.iter().map(|f| f.servers.len()).sum()
    }

    /// 読めなかったファイル (パス, 理由)。
    pub fn broken(&self) -> Vec<(&Path, &str)> {
        self.files
            .iter()
            .filter_map(|f| match &f.state {
                FileState::Broken(why) => Some((f.path.as_path(), why.as_str())),
                _ => None,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 解析 (純関数)
// ---------------------------------------------------------------------------

/// 設定テキストから MCP サーバ一覧を取り出す (純関数)。
///
/// 読めない場合は**空を返す** (panic しない)。理由まで要るときは
/// [`parse_config`] を使う。`path` は空のままなので、ファイルから読む側が埋める。
pub fn parse_mcp_config(text: &str, source: ConfigSource) -> Vec<McpServer> {
    parse_config(text, source).0
}

/// [`parse_mcp_config`] に「読めない理由」を添えた版。
pub fn parse_config(text: &str, source: ConfigSource) -> (Vec<McpServer>, FileState) {
    let root = match source.format() {
        ConfigFormat::Json => match serde_json::from_str::<serde_json::Value>(&strip_jsonc(text)) {
            Ok(v) => v,
            Err(e) => {
                return (
                    Vec::new(),
                    FileState::Broken(trf(
                        "JSON として読めません: {why}",
                        &[("why", e.to_string())],
                    )),
                )
            }
        },
        ConfigFormat::Toml => match text.parse::<toml::Value>() {
            Ok(v) => toml_to_json(&v),
            Err(e) => {
                return (
                    Vec::new(),
                    FileState::Broken(trf(
                        "TOML として読めません: {why}",
                        &[("why", e.to_string())],
                    )),
                )
            }
        },
    };

    let Some(map) = find_container(&root, source) else {
        // サーバの表が無いのは**エラーではない** (単に未設定)。
        return (Vec::new(), FileState::Ok);
    };
    let mut out: Vec<McpServer> = map
        .iter()
        .map(|(name, v)| server_from_value(name, v, source))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    (out, FileState::Ok)
}

/// サーバの表を持つオブジェクトを、出典ごとの候補キーから探す。
fn find_container(
    root: &serde_json::Value,
    source: ConfigSource,
) -> Option<&serde_json::Map<String, serde_json::Value>> {
    for path in source.container_paths() {
        let mut cur = root;
        let mut ok = true;
        for key in *path {
            match cur.get(*key) {
                Some(next) => cur = next,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            if let Some(m) = cur.as_object() {
                return Some(m);
            }
        }
    }
    None
}

/// サーバ 1 件を組み立てる。**env / headers の値はここで捨てる。**
fn server_from_value(name: &str, v: &serde_json::Value, source: ConfigSource) -> McpServer {
    let obj = v.as_object();
    let get_str = |k: &str| -> Option<String> {
        obj.and_then(|o| o.get(k))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
    };
    let kind = get_str("type").unwrap_or_default().to_ascii_lowercase();
    let url = get_str("url");
    let command = get_str("command");
    let cwd = get_str("cwd");
    let args: Vec<String> = obj
        .and_then(|o| o.get("args"))
        .and_then(|x| x.as_array())
        .map(|a| a.iter().map(json_scalar_to_string).collect())
        .unwrap_or_default();

    let stdio = |command: Option<String>| match command {
        Some(c) => Transport::Stdio {
            command: c,
            args: args.clone(),
            cwd: cwd.clone(),
        },
        None => Transport::Unknown,
    };
    // URL は**保持する前に**秘密を落とす。持たない値は漏れない
    // (`?token=…` や `user:pass@` は Debug 出力にすら出してはいけない)。
    let remote = |sse: bool| match url.as_deref().map(redact_url) {
        Some(u) if sse => Transport::Sse { url: u },
        Some(u) => Transport::Http { url: u },
        None => Transport::Unknown,
    };
    let transport = match kind.as_str() {
        "stdio" | "local" => stdio(command.clone()),
        "sse" => remote(true),
        "http" | "streamable-http" | "streamablehttp" | "remote" => remote(false),
        // `type` が無い書き方 (Claude Code / Cursor の実ファイルで多数派)
        _ if url.is_some() => remote(false),
        _ if command.is_some() => stdio(command.clone()),
        _ => Transport::Unknown,
    };

    McpServer {
        name: name.to_string(),
        transport,
        env: secret_keys(obj, "env"),
        headers: secret_keys(obj, "headers"),
        disabled: obj
            .and_then(|o| o.get(DISABLED_KEY))
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        source,
        path: PathBuf::new(),
    }
}

/// `args` の要素を文字列にする (数値・真偽値もそのまま渡されることがある)。
fn json_scalar_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `env` / `headers` の**キー名だけ**を取り出す。値は返り値に載せない。
fn secret_keys(
    obj: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
) -> Vec<SecretKey> {
    let Some(m) = obj.and_then(|o| o.get(key)).and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut v: Vec<SecretKey> = m
        .iter()
        .map(|(k, val)| SecretKey {
            name: k.clone(),
            set: match val {
                serde_json::Value::Null => false,
                serde_json::Value::String(s) => !s.trim().is_empty(),
                _ => true,
            },
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// `toml::Value` を `serde_json::Value` へ写す。以降の解析を 1 本にまとめるため。
fn toml_to_json(v: &toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::from(*i),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(a) => serde_json::Value::Array(a.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => serde_json::Value::Object(
            t.iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}

// ---------------------------------------------------------------------------
// 表示文字列 (秘密が出ない唯一の出口)
// ---------------------------------------------------------------------------

/// URL から**秘密になり得る部分を落とす** (純関数)。
///
/// クエリ (`?token=…`) とフラグメント、`user:pass@` の userinfo を捨てる。
/// 捨てたことは `?***` として明示する (黙って消さない)。
pub fn redact_url(url: &str) -> String {
    let (head, had_extra) = match url.find(['?', '#']) {
        Some(i) => (&url[..i], true),
        None => (url, false),
    };
    // scheme://userinfo@host/path の userinfo を落とす
    let cleaned = match head.find("://") {
        Some(i) => {
            let (scheme, rest) = head.split_at(i + 3);
            match rest.find('@') {
                // '@' が最初の '/' より後ろならパスの一部なので触らない
                Some(at) if rest[..at].find('/').is_none() => {
                    format!("{scheme}***@{}", &rest[at + 1..])
                }
                _ => head.to_string(),
            }
        }
        None => head.to_string(),
    };
    if had_extra {
        format!("{cleaned}?***")
    } else {
        cleaned
    }
}

/// 詳細行を組み立てる (純関数)。
///
/// **パネルが詳細として描く文字列はここが全て。** env / headers の値は
/// 構造体が持っていないので出しようがなく、URL は [`redact_url`] を通る。
/// 「値が漏れないこと」はこの関数のテストで固定する。
pub fn detail_lines(s: &McpServer) -> Vec<String> {
    let mut out = Vec::new();
    match &s.transport {
        Transport::Stdio { command, args, cwd } => {
            out.push(trf("command: {v}", &[("v", command.clone())]));
            if !args.is_empty() {
                out.push(trf("args: {v}", &[("v", args.join(" "))]));
            }
            if let Some(c) = cwd {
                out.push(trf("cwd: {v}", &[("v", c.clone())]));
            }
        }
        Transport::Http { url } | Transport::Sse { url } => {
            out.push(trf("url: {v}", &[("v", redact_url(url))]));
        }
        Transport::Unknown => {
            out.push(tr("種別を判定できません (command も url もありません)"));
        }
    }
    for k in &s.env {
        out.push(trf(
            "env: {k} = *** ({state})",
            &[
                ("k", k.name.clone()),
                (
                    "state",
                    if k.set {
                        tr("設定済み")
                    } else {
                        tr("未設定")
                    },
                ),
            ],
        ));
    }
    for k in &s.headers {
        out.push(trf(
            "header: {k} = *** ({state})",
            &[
                ("k", k.name.clone()),
                (
                    "state",
                    if k.set {
                        tr("設定済み")
                    } else {
                        tr("未設定")
                    },
                ),
            ],
        ));
    }
    out.push(trf("出典: {p}", &[("p", s.path.display().to_string())]));
    if !s.source.editable() {
        out.push(tr("この形式は編集非対応です (読み取り専用)"));
    }
    out
}

/// 長い文字列を末尾省略する (純関数・文字境界で切る)。
pub fn ellipsize(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let n = s.chars().count();
    if n <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// 書き戻し (生テキストへの外科的な差し替え)
// ---------------------------------------------------------------------------

/// 書き戻しが断られた理由。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EditError {
    /// この記法は編集非対応 (TOML など)
    Unsupported,
    /// 対象が見つからない
    NotFound(String),
    /// 書き換えると壊れる — 何も書かずに諦めた
    WouldCorrupt,
}

impl EditError {
    /// 表示用の理由 (日本語原文)。
    pub fn message(&self) -> String {
        match self {
            EditError::Unsupported => tr("この形式は編集非対応です (読み取り専用)"),
            EditError::NotFound(what) => trf(
                "設定の中に {what} が見つかりません",
                &[("what", what.clone())],
            ),
            EditError::WouldCorrupt => tr("書き換えると設定が壊れるため中止しました"),
        }
    }
}

/// 空白と JSONC コメントを読み飛ばした位置を返す。
fn skip_trivia(b: &[u8], mut i: usize) -> usize {
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        return i;
    }
}

/// `b[i]` が `"` のとき、文字列リテラルの終端の**次**の位置。
fn end_of_string(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    let mut j = i + 1;
    let mut esc = false;
    while j < b.len() {
        let c = b[j];
        if esc {
            esc = false;
        } else if c == b'\\' {
            esc = true;
        } else if c == b'"' {
            return Some(j + 1);
        }
        j += 1;
    }
    None
}

/// 値の終端の次の位置。`i` は値の先頭 (trivia は飛ばし済み)。
fn end_of_value(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => end_of_string(b, i),
        open @ (b'{' | b'[') => {
            let close = if open == b'{' { b'}' } else { b']' };
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                let c = b[j];
                if c == b'"' {
                    j = end_of_string(b, j)?;
                    continue;
                }
                if c == b'/' && j + 1 < b.len() && (b[j + 1] == b'/' || b[j + 1] == b'*') {
                    let k = skip_trivia(b, j);
                    if k == j {
                        return None;
                    }
                    j = k;
                    continue;
                }
                if c == open {
                    depth += 1;
                } else if c == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some(j + 1);
                    }
                }
                j += 1;
            }
            None
        }
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']') && !b[j].is_ascii_whitespace()
            {
                j += 1;
            }
            if j == i {
                None
            } else {
                Some(j)
            }
        }
    }
}

/// オブジェクト (`obj` は `{` の位置) の直下から `key` の要素を探す。
/// 返り値は `(値の開始, 値の終端)`。
fn find_member(text: &str, obj: usize, key: &str) -> Option<(usize, usize)> {
    let b = text.as_bytes();
    if b.get(obj) != Some(&b'{') {
        return None;
    }
    let mut i = skip_trivia(b, obj + 1);
    while i < b.len() && b[i] != b'}' {
        if b[i] != b'"' {
            return None;
        }
        let kend = end_of_string(b, i)?;
        let raw = text.get(i..kend)?;
        let k: String = serde_json::from_str(raw).ok()?;
        let mut j = skip_trivia(b, kend);
        if b.get(j) != Some(&b':') {
            return None;
        }
        j = skip_trivia(b, j + 1);
        let vend = end_of_value(b, j)?;
        if k == key {
            return Some((j, vend));
        }
        i = skip_trivia(b, vend);
        if b.get(i) == Some(&b',') {
            i = skip_trivia(b, i + 1);
        }
    }
    None
}

/// 対象サーバへ `"disabled": <flag>` を書き戻したテキストを返す (純関数)。
///
/// serde で往復させずに**元のバイト列をそのまま温存**し、値 1 つの差し替え
/// (または `disabled` 要素 1 つの挿入) だけを行う。よってキー順・コメント・
/// インデント・改行コードは保たれる。仕上げに解析し直して、
/// 「壊れていない」「意図どおり切り替わった」ことを確かめてから返す。
pub fn toggle_disabled(
    text: &str,
    source: ConfigSource,
    server: &str,
    flag: bool,
) -> Result<String, EditError> {
    if !source.editable() {
        return Err(EditError::Unsupported);
    }
    // **読めない設定には触らない。** 壊れたファイルへ書き足して更に壊す方が
    // 「切り替えられません」と断るより遥かに悪い (安全側へ倒す)。
    if parse_config(text, source).1 != FileState::Ok {
        return Err(EditError::WouldCorrupt);
    }
    let b = text.as_bytes();
    let root = skip_trivia(b, 0);
    if b.get(root) != Some(&b'{') {
        return Err(EditError::NotFound(tr("設定の本体 (オブジェクト)")));
    }

    // サーバが実際に居るコンテナを選ぶ (候補は出典ごとに複数ある)
    let mut found: Option<(usize, usize)> = None;
    'outer: for path in source.container_paths() {
        let mut obj = root;
        for key in *path {
            let Some((vs, _)) = find_member(text, obj, key) else {
                continue 'outer;
            };
            if b.get(vs) != Some(&b'{') {
                continue 'outer;
            }
            obj = vs;
        }
        if let Some(hit) = find_member(text, obj, server) {
            found = Some(hit);
            break;
        }
    }
    let (vs, _ve) = found.ok_or_else(|| EditError::NotFound(server.to_string()))?;
    if b.get(vs) != Some(&b'{') {
        // サーバの中身がオブジェクトでない書き方には触らない
        return Err(EditError::Unsupported);
    }

    let val = if flag { "true" } else { "false" };
    let out = match find_member(text, vs, DISABLED_KEY) {
        // 既にある → 値だけ差し替え
        Some((dvs, dve)) => {
            let mut s = String::with_capacity(text.len() + 8);
            s.push_str(text.get(..dvs).ok_or(EditError::WouldCorrupt)?);
            s.push_str(val);
            s.push_str(text.get(dve..).ok_or(EditError::WouldCorrupt)?);
            s
        }
        // 無い → 先頭要素として挿入。`{` 直後の空白 (改行 + インデント) を
        // そのまま真似るので、既存の整形を崩さない
        None => {
            let mut w = vs + 1;
            while w < b.len() && b[w].is_ascii_whitespace() {
                w += 1;
            }
            let lead = text.get(vs + 1..w).ok_or(EditError::WouldCorrupt)?;
            let empty = b.get(w) == Some(&b'}');
            let comma = if empty { "" } else { "," };
            let mut s = String::with_capacity(text.len() + 32);
            s.push_str(text.get(..vs + 1).ok_or(EditError::WouldCorrupt)?);
            s.push_str(lead);
            s.push_str(&format!("\"{DISABLED_KEY}\": {val}{comma}"));
            s.push_str(text.get(vs + 1..).ok_or(EditError::WouldCorrupt)?);
            s
        }
    };

    // 仕上げの検算: 壊れていないか / 狙ったサーバだけが切り替わったか
    let before = parse_mcp_config(text, source);
    let (after, state) = parse_config(&out, source);
    if state != FileState::Ok {
        return Err(EditError::WouldCorrupt);
    }
    if after.len() != before.len() {
        return Err(EditError::WouldCorrupt);
    }
    let ok = after
        .iter()
        .zip(before.iter())
        .all(|(a, b)| a.name == b.name && (a.name == server || a.disabled == b.disabled));
    if !ok || !after.iter().any(|s| s.name == server && s.disabled == flag) {
        return Err(EditError::WouldCorrupt);
    }
    Ok(out)
}

/// 書き戻し前に取る控えのパス (`<元のファイル名>.zaivern.bak`)。
///
/// 拡張子を差し替えるのではなく**足す**ので、`config.toml` でも
/// `.mcp.json` でも元の名前が残って何の控えか分かる。
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("mcp-config"));
    name.push(".zaivern.bak");
    path.with_file_name(name)
}

/// ファイルへ有効/無効を書き戻す。**控えを取ってから**書く。
pub fn write_toggle(
    path: &Path,
    source: ConfigSource,
    server: &str,
    flag: bool,
) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| trf("読めません: {why}", &[("why", e.to_string())]))?;
    let out = toggle_disabled(&text, source, server, flag).map_err(|e| e.message())?;
    if out == text {
        return Ok(());
    }
    let bak = backup_path(path);
    std::fs::write(&bak, text.as_bytes())
        .map_err(|e| trf("控えを作れません: {why}", &[("why", e.to_string())]))?;
    std::fs::write(path, out.as_bytes())
        .map_err(|e| trf("書き込めません: {why}", &[("why", e.to_string())]))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 走査 (I/O)
// ---------------------------------------------------------------------------

/// ファイル 1 枚を読んで解析する。**panic しない。**
pub fn load_file(source: ConfigSource, path: PathBuf) -> ConfigFile {
    let meta = match std::fs::metadata(&path) {
        Ok(m) if m.is_file() => m,
        _ => {
            return ConfigFile {
                source,
                path,
                state: FileState::Missing,
                servers: Vec::new(),
            }
        }
    };
    if meta.len() > MAX_CONFIG_BYTES {
        return ConfigFile {
            source,
            path,
            state: FileState::Broken(tr("大きすぎるため読み込みません")),
            servers: Vec::new(),
        };
    }
    match std::fs::read_to_string(&path) {
        Err(e) => ConfigFile {
            source,
            path,
            state: FileState::Broken(trf("読めません: {why}", &[("why", e.to_string())])),
            servers: Vec::new(),
        },
        Ok(text) => {
            let (mut servers, state) = parse_config(&text, source);
            for s in &mut servers {
                s.path = path.clone();
            }
            ConfigFile {
                source,
                path,
                state,
                servers,
            }
        }
    }
}

/// ワークスペースのルート群とホームを走査する。
///
/// **要求されたときだけ呼ぶこと。** `~/.claude.json` は 100KB 級になる。
pub fn scan(roots: &[PathBuf]) -> Inventory {
    let mut files: Vec<ConfigFile> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    let push = |source: ConfigSource,
                path: PathBuf,
                files: &mut Vec<ConfigFile>,
                seen: &mut Vec<PathBuf>| {
        if seen.contains(&path) {
            return;
        }
        seen.push(path.clone());
        files.push(load_file(source, path));
    };
    for root in roots {
        for src in PROJECT_SOURCES {
            if let Some(p) = src.project_path(root) {
                push(*src, p, &mut files, &mut seen);
            }
        }
    }
    for src in USER_SOURCES {
        if let Some(p) = src.user_path() {
            push(*src, p, &mut files, &mut seen);
        }
    }
    Inventory { files }
}

// ---------------------------------------------------------------------------
// レイアウト (純関数)
// ---------------------------------------------------------------------------

/// 列の間隔。
const GAP: f32 = space::SM;
/// 種別バッジの幅 ("stdio" が入る)。
const BADGE_W: f32 = 46.0;
/// 効き先エージェント列の幅 ("VS Code" が入る)。
const AGENTS_W: f32 = 96.0;
/// 出典列の最小幅。
const SOURCE_W: f32 = 130.0;
/// 名前列の最小幅 (これを割ると名前が読めない)。
const NAME_MIN_W: f32 = 100.0;
/// 操作列 (開く / 切替) をラベル付きで並べる幅。
const ACTIONS_FULL_W: f32 = 74.0;
/// 操作列をアイコンだけに縮めた幅。
const ACTIONS_ICON_W: f32 = 40.0;
/// 操作列にラベルを付けられる行幅の下限。
const ACTIONS_LABEL_MIN_ROW_W: f32 = 420.0;

/// 一覧 1 行の列幅。**幅 0 の列は描かない。**
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowLayout {
    pub badge_w: f32,
    pub name_w: f32,
    pub agents_w: f32,
    pub source_w: f32,
    pub actions_w: f32,
    /// 操作列をアイコンだけに縮退させるか
    pub compact_actions: bool,
}

impl RowLayout {
    /// 描く列の合計幅 (列間の間隔込み)。**必ず可用幅以下**になる。
    pub fn total(&self) -> f32 {
        let cols = [
            self.badge_w,
            self.name_w,
            self.agents_w,
            self.source_w,
            self.actions_w,
        ];
        let n = cols.iter().filter(|w| **w > 0.0).count();
        if n == 0 {
            return 0.0;
        }
        cols.iter().sum::<f32>() + GAP * (n as f32 - 1.0)
    }
}

/// 行の列幅を決める (純関数)。
///
/// 優先順は **操作 > 名前 > 種別 > 効き先 > 出典**。
/// 操作列は「切り替えに到達できなくなる」ので最後まで落とさず、
/// 狭いところではアイコンだけに縮退させる。
pub fn row_layout(avail_w: f32) -> RowLayout {
    let avail = if avail_w.is_finite() {
        avail_w.max(0.0)
    } else {
        0.0
    };
    let compact_actions = avail < ACTIONS_LABEL_MIN_ROW_W;
    let actions_w = if compact_actions {
        ACTIONS_ICON_W
    } else {
        ACTIONS_FULL_W
    }
    .min(avail);
    let mut rest = (avail - actions_w - GAP).max(0.0);
    let mut badge_w = 0.0;
    let mut agents_w = 0.0;
    let mut source_w = 0.0;
    if rest >= NAME_MIN_W + BADGE_W + GAP {
        badge_w = BADGE_W;
        rest -= BADGE_W + GAP;
    }
    if rest >= NAME_MIN_W + AGENTS_W + GAP {
        agents_w = AGENTS_W;
        rest -= AGENTS_W + GAP;
    }
    if rest >= NAME_MIN_W + SOURCE_W + GAP {
        source_w = SOURCE_W;
        rest -= SOURCE_W + GAP;
    }
    RowLayout {
        badge_w,
        name_w: rest,
        agents_w,
        source_w,
        actions_w,
        compact_actions,
    }
}

/// 空状態カードの最大幅。
const EMPTY_CARD_MAX_W: f32 = 460.0;
/// 空状態カードの高さ (アイコン + 見出し + ヒント 2 行)。
const EMPTY_CARD_H: f32 = 168.0;

/// 空状態カードの矩形 (純関数)。**常に `avail` の中央 1 枚**で、必ず収まる。
pub fn empty_card(avail: egui::Rect) -> egui::Rect {
    let aw = avail.width().max(0.0);
    let ah = avail.height().max(0.0);
    let w = (aw - space::LG * 2.0).clamp(0.0, EMPTY_CARD_MAX_W).min(aw);
    let h = EMPTY_CARD_H.min(ah);
    let x = avail.left() + (aw - w) * 0.5;
    let y = avail.top() + (ah - h) * 0.5;
    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))
}

// ---------------------------------------------------------------------------
// パネル (状態 + 描画)
// ---------------------------------------------------------------------------

/// パネルの表示状態。app が所有する。
#[derive(Default)]
pub struct McpPanel {
    /// 走査結果
    pub inventory: Inventory,
    /// 展開中のサーバ (出典, 名前)
    pub expanded: Option<(ConfigSource, String)>,
    /// 走査済みか。**false の間だけ**走査する (毎フレーム I/O にしない)
    pub scanned: bool,
    /// 直近の書き戻し結果 (本文, 成功か)
    pub notice: Option<(String, bool)>,
}

impl McpPanel {
    /// タブに添える件数。**0 のときは `None`** (常に 0 を出すバッジを作らない)。
    pub fn badge(&self) -> Option<usize> {
        match self.inventory.count() {
            0 => None,
            n => Some(n),
        }
    }

    /// 次の描画で走査し直す。
    pub fn invalidate(&mut self) {
        self.scanned = false;
    }
}

/// パネルが app へ返す要求。I/O は app 側 (描画の外) で行う。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum McpAction {
    #[default]
    None,
    /// 出典ファイルをエディタで開く
    Open(PathBuf),
    /// 有効/無効を切り替える
    Toggle {
        path: PathBuf,
        source: ConfigSource,
        name: String,
        /// true にすると無効化
        disable: bool,
    },
    /// 走査し直す
    Rescan,
}

/// MCP サーバ管理パネルを描く。
pub fn ui(ui: &mut egui::Ui, theme: &Theme, panel: &mut McpPanel) -> McpAction {
    let mut action = McpAction::None;

    // ── 見出し行 ──
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(tr("🔌 MCP サーバ"))
                .size(13.0)
                .color(theme.text),
        );
        let n = panel.inventory.count();
        if n > 0 {
            ui.label(
                RichText::new(trf("{n} 件", &[("n", n.to_string())]))
                    .size(11.5)
                    .color(theme.text_dim),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button("⟳")
                .on_hover_text(tr("設定ファイルを読み直す"))
                .clicked()
            {
                action = McpAction::Rescan;
            }
        });
    });

    // ── 直近の書き戻し結果 ──
    if let Some((msg, ok)) = &panel.notice {
        ui.label(RichText::new(msg.clone()).size(11.0).color(if *ok {
            theme.ok
        } else {
            theme.err
        }));
    }

    // ── 読めなかったファイル (握り潰さず理由ごと出す) ──
    let broken: Vec<String> = panel
        .inventory
        .broken()
        .iter()
        .map(|(p, why)| {
            trf(
                "⚠ {p}: {why}",
                &[("p", p.display().to_string()), ("why", (*why).to_string())],
            )
        })
        .collect();
    for line in &broken {
        ui.label(
            RichText::new(ellipsize(line, 200))
                .size(11.0)
                .color(theme.warn),
        )
        .on_hover_text(line.clone());
    }

    let servers: Vec<McpServer> = panel.inventory.servers().into_iter().cloned().collect();
    if servers.is_empty() {
        empty_state(ui, theme);
        return action;
    }

    ui.add_space(space::XS);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // 行の枠には左右 `space::SM` の内側余白があるので、その分を
            // 差し引いた実効幅で列を決める (差し引かないと右端が見切れる)。
            let l = row_layout(ui.available_width() - space::SM * 2.0);
            for s in &servers {
                // 可変長リストの中で永続 ID を作るウィジェットは使わないが、
                // 行内の `interact` の ID が名前で分かれるよう名前を混ぜる。
                ui.push_id(&s.name, |ui| {
                    if server_row(ui, theme, s, &l, panel, &mut action) {
                        let key = (s.source, s.name.clone());
                        panel.expanded = if panel.expanded.as_ref() == Some(&key) {
                            None
                        } else {
                            Some(key)
                        };
                    }
                });
            }
        });
    action
}

/// 1 行を描く。行そのものがクリックされたら `true` (詳細の開閉)。
fn server_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    s: &McpServer,
    l: &RowLayout,
    panel: &McpPanel,
    action: &mut McpAction,
) -> bool {
    let open = panel.expanded.as_ref() == Some(&(s.source, s.name.clone()));
    let dim = s.disabled;
    let name_color = if dim { theme.text_dim } else { theme.text };
    let mut row_clicked = false;

    let frame = egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(space::SM, 5.0))
        .rounding(6.0)
        .fill(if open { theme.panel_alt } else { theme.bg })
        .show(ui, |ui| {
            // 列の合計は必ず可用幅以下 (`RowLayout::total` の不変条件)。
            // これで行がどの幅でも見切れない。
            ui.set_width(l.total().min(ui.available_width()));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                if l.badge_w > 0.0 {
                    ui.add_sized(
                        [l.badge_w, 18.0],
                        egui::Label::new(
                            RichText::new(s.transport.badge())
                                .size(10.5)
                                .monospace()
                                .color(theme.accent),
                        )
                        .selectable(false),
                    );
                }
                if l.name_w > 0.0 {
                    let mark = if dim { "○ " } else { "● " };
                    let label = format!("{mark}{}", s.name);
                    ui.add_sized(
                        [l.name_w, 18.0],
                        egui::Label::new(
                            RichText::new(ellipsize(&label, name_chars(l.name_w)))
                                .size(12.0)
                                .color(name_color),
                        )
                        .selectable(false),
                    )
                    .on_hover_text(&s.name);
                }
                if l.agents_w > 0.0 {
                    let names: Vec<&str> = s.agents().iter().map(|a| a.label()).collect();
                    let text = names.join(" / ");
                    ui.add_sized(
                        [l.agents_w, 18.0],
                        egui::Label::new(
                            RichText::new(ellipsize(&text, 12))
                                .size(10.5)
                                .color(theme.text_dim),
                        )
                        .selectable(false),
                    )
                    .on_hover_text(text);
                }
                if l.source_w > 0.0 {
                    let text = tr(s.source.label());
                    ui.add_sized(
                        [l.source_w, 18.0],
                        egui::Label::new(
                            RichText::new(ellipsize(&text, 18))
                                .size(10.5)
                                .color(theme.text_dim),
                        )
                        .selectable(false),
                    )
                    .on_hover_text(s.path.display().to_string());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let toggle_label = if l.compact_actions {
                        if dim {
                            "▶".to_string()
                        } else {
                            "■".to_string()
                        }
                    } else if dim {
                        tr("有効化")
                    } else {
                        tr("無効化")
                    };
                    let editable = s.source.editable();
                    let btn = ui.add_enabled(
                        editable,
                        egui::Button::new(RichText::new(toggle_label).size(11.0)),
                    );
                    let hint = if editable {
                        if dim {
                            tr("このサーバを有効に戻す (設定ファイルを書き換えます)")
                        } else {
                            tr("このサーバを無効にする (設定ファイルを書き換えます)")
                        }
                    } else {
                        tr("この形式は編集非対応です (読み取り専用)")
                    };
                    if btn.on_hover_text(hint).clicked() {
                        *action = McpAction::Toggle {
                            path: s.path.clone(),
                            source: s.source,
                            name: s.name.clone(),
                            disable: !dim,
                        };
                    }
                    let open_label = if l.compact_actions {
                        "📂".to_string()
                    } else {
                        tr("📂 出典")
                    };
                    if ui
                        .button(RichText::new(open_label).size(11.0))
                        .on_hover_text(trf(
                            "出典ファイルを開く: {p}",
                            &[("p", s.path.display().to_string())],
                        ))
                        .clicked()
                    {
                        *action = McpAction::Open(s.path.clone());
                    }
                });
            });
            if open {
                ui.add_space(space::XS);
                for line in detail_lines(s) {
                    ui.label(
                        RichText::new(ellipsize(&line, 160))
                            .size(10.5)
                            .monospace()
                            .color(theme.text_dim),
                    )
                    .on_hover_text(line);
                }
            }
        });

    let hit = ui.interact(
        frame.response.rect,
        ui.id().with("zv-mcp-row"),
        egui::Sense::click(),
    );
    if hit.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if hit.on_hover_text(tr("クリックで詳細を開閉")).clicked() {
        row_clicked = true;
    }
    row_clicked
}

/// 名前列に入るおおよその文字数 (等幅でない前提で 7px/文字と見積もる)。
fn name_chars(w: f32) -> usize {
    ((w / 7.0).floor() as usize).max(4)
}

/// 空状態 — 利用可能領域の**中央に 1 枚**のカード。
fn empty_state(ui: &mut egui::Ui, theme: &Theme) {
    let avail = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    let card = empty_card(avail);
    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(card), |ui| {
        egui::Frame::none()
            .fill(theme.panel_alt)
            .stroke(egui::Stroke::new(1.0_f32, theme.border))
            .rounding(egui::Rounding::same(10.0))
            .inner_margin(egui::Margin::same(space::MD))
            .show(ui, |ui| {
                ui.set_width((card.width() - space::MD * 2.0).max(0.0));
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("🔌").size(40.0));
                    ui.label(
                        RichText::new(tr("MCP サーバが未設定です"))
                            .size(16.0)
                            .color(theme.text),
                    );
                    ui.label(
                        RichText::new(tr(
                            "ワークスペース直下の .mcp.json / .cursor/mcp.json / .vscode/mcp.json、\
                             またはホームの .claude.json・.codex/config.toml・.gemini/settings.json \
                             に書くとここに並びます",
                        ))
                        .size(11.0)
                        .color(theme.text_dim),
                    );
                });
            });
    });
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 本物のトークンに見える文字列。**どの表示経路にも出てはいけない。**
    const SECRET: &str = "sk-live-DO-NOT-LEAK-0123456789";

    // ---- parse_mcp_config: テーブルテスト --------------------------------

    #[test]
    fn 解析のテーブル() {
        // (説明, 入力, 出典, 期待する (名前, バッジ) の並び)
        let cases: Vec<(&str, String, ConfigSource, Vec<(&str, &str)>)> = vec![
            (
                "stdio (type 無し)",
                r#"{"mcpServers": {"a": {"command": "node", "args": ["x.js"]}}}"#.into(),
                ConfigSource::ProjectMcpJson,
                vec![("a", "stdio")],
            ),
            (
                "stdio (type あり)",
                r#"{"mcpServers": {"a": {"type": "stdio", "command": "zzz", "args": []}}}"#.into(),
                ConfigSource::UserClaude,
                vec![("a", "stdio")],
            ),
            (
                "http",
                r#"{"mcpServers": {"h": {"type": "http", "url": "https://e.test/mcp"}}}"#.into(),
                ConfigSource::UserClaude,
                vec![("h", "http")],
            ),
            (
                "sse",
                r#"{"mcpServers": {"s": {"type": "sse", "url": "https://e.test/sse"}}}"#.into(),
                ConfigSource::UserCursor,
                vec![("s", "sse")],
            ),
            (
                "url だけ (type 無し) は http",
                r#"{"mcpServers": {"u": {"url": "https://e.test/mcp"}}}"#.into(),
                ConfigSource::ProjectCursor,
                vec![("u", "http")],
            ),
            (
                "env 付き",
                format!(
                    r#"{{"mcpServers": {{"e": {{"command": "c", "env": {{"TOKEN": "{SECRET}", "EMPTY": ""}}}}}}}}"#
                ),
                ConfigSource::UserClaude,
                vec![("e", "stdio")],
            ),
            (
                "VS Code は servers キー",
                r#"{"servers": {"v": {"type": "stdio", "command": "c"}}, "inputs": []}"#.into(),
                ConfigSource::ProjectVscode,
                vec![("v", "stdio")],
            ),
            (
                "空オブジェクト",
                "{}".into(),
                ConfigSource::ProjectMcpJson,
                vec![],
            ),
            (
                "サーバ表が空",
                r#"{"mcpServers": {}}"#.into(),
                ConfigSource::ProjectMcpJson,
                vec![],
            ),
            (
                "壊れた JSON",
                r#"{"mcpServers": {"a": }"#.into(),
                ConfigSource::ProjectMcpJson,
                vec![],
            ),
            (
                "空文字列",
                String::new(),
                ConfigSource::ProjectMcpJson,
                vec![],
            ),
            (
                "コメント + 末尾カンマ (JSONC)",
                "{\n // MCP\n \"mcpServers\": {\n \"c\": {\"command\": \"c\",},\n },\n}".into(),
                ConfigSource::ProjectVscode,
                vec![("c", "stdio")],
            ),
            (
                "command も url も無い",
                r#"{"mcpServers": {"x": {"note": "todo"}}}"#.into(),
                ConfigSource::ProjectMcpJson,
                vec![("x", "?")],
            ),
            (
                "サーバの値がオブジェクトでない",
                r#"{"mcpServers": {"x": "node x.js"}}"#.into(),
                ConfigSource::ProjectMcpJson,
                vec![("x", "?")],
            ),
            (
                "TOML (codex)",
                "[mcp_servers.t]\ncommand = \"c\"\nargs = [\"a\"]\n\n[mcp_servers.t.env]\nTOKEN = \"x\"\n".into(),
                ConfigSource::UserCodex,
                vec![("t", "stdio")],
            ),
            (
                "壊れた TOML",
                "[mcp_servers.t\ncommand =".into(),
                ConfigSource::UserCodex,
                vec![],
            ),
            (
                "disabled 付き",
                r#"{"mcpServers": {"d": {"command": "c", "disabled": true}}}"#.into(),
                ConfigSource::UserCursor,
                vec![("d", "stdio")],
            ),
        ];
        for (why, text, source, want) in cases {
            let got = parse_mcp_config(&text, source);
            let got_pairs: Vec<(&str, &str)> = got
                .iter()
                .map(|s| (s.name.as_str(), s.transport.badge()))
                .collect();
            assert_eq!(got_pairs, want, "{why}");
        }
    }

    #[test]
    fn 壊れた入力は理由を持つ状態になる() {
        let (servers, state) = parse_config(r#"{"mcpServers": {"a": }"#, ConfigSource::UserClaude);
        assert!(servers.is_empty());
        assert!(matches!(state, FileState::Broken(_)), "{state:?}");
        let (_, state) = parse_config("[mcp_servers.t\n", ConfigSource::UserCodex);
        assert!(matches!(state, FileState::Broken(_)), "{state:?}");
        // サーバ表が無いのはエラーではない
        let (servers, state) = parse_config("{}", ConfigSource::UserClaude);
        assert!(servers.is_empty());
        assert_eq!(state, FileState::Ok);
    }

    #[test]
    fn disabled_を読み取る() {
        let s = parse_mcp_config(
            r#"{"mcpServers": {"a": {"command": "c", "disabled": true},
                               "b": {"command": "c"}}}"#,
            ConfigSource::UserCursor,
        );
        assert_eq!(s.len(), 2);
        assert!(s[0].disabled, "a は無効");
        assert!(!s[1].disabled, "b は有効");
    }

    #[test]
    fn env_は_キー名と設定済みかだけを持つ() {
        let text = format!(
            r#"{{"mcpServers": {{"e": {{"command": "c",
               "env": {{"TOKEN": "{SECRET}", "EMPTY": "", "NULLED": null}},
               "headers": {{"Authorization": "Bearer {SECRET}"}}}}}}}}"#
        );
        let s = parse_mcp_config(&text, ConfigSource::UserClaude);
        assert_eq!(s.len(), 1);
        let names: Vec<&str> = s[0].env.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, vec!["EMPTY", "NULLED", "TOKEN"]);
        assert_eq!(
            s[0].env.iter().map(|k| k.set).collect::<Vec<_>>(),
            vec![false, false, true]
        );
        assert_eq!(s[0].headers.len(), 1);
        assert_eq!(s[0].headers[0].name, "Authorization");
        assert!(s[0].headers[0].set);
    }

    // ---- 秘密が漏れないこと (要件そのもの) ------------------------------

    #[test]
    fn 秘密の値はどの表示経路にも出ない() {
        let text = format!(
            r#"{{"mcpServers": {{
                 "e": {{"command": "c", "args": ["--x"],
                        "env": {{"TOKEN": "{SECRET}"}}}},
                 "h": {{"type": "http", "url": "https://u:{SECRET}@e.test/mcp?key={SECRET}#f={SECRET}",
                        "headers": {{"Authorization": "Bearer {SECRET}"}}}}
               }}}}"#
        );
        let servers = parse_mcp_config(&text, ConfigSource::UserClaude);
        assert_eq!(servers.len(), 2);

        // 1. 構造体そのものが値を保持していない (Debug 出力にも出ない = ログにも出ない)
        let dbg = format!("{servers:?}");
        assert!(!dbg.contains(SECRET), "Debug 出力に秘密が載っている");

        // 2. パネルが描く文字列 (detail_lines が唯一の出口) にも出ない
        for s in &servers {
            let lines = detail_lines(s);
            for line in &lines {
                assert!(!line.contains(SECRET), "詳細行に秘密が載っている: {line}");
            }
            // マスクは出す (「何も無い」と区別が付かないと困る)
            if !s.env.is_empty() || !s.headers.is_empty() {
                assert!(
                    lines.iter().any(|l| l.contains("***")),
                    "マスク表示が無い: {lines:?}"
                );
            }
        }

        // 3. キー名は出てよい (どれを設定すべきか分からないと使えない)
        let e = detail_lines(&servers[0]).join("\n");
        assert!(e.contains("TOKEN"), "キー名が出ていない: {e}");
    }

    #[test]
    fn urlのクエリとuserinfoを落とす() {
        for (input, want) in [
            ("https://e.test/mcp", "https://e.test/mcp"),
            ("https://e.test/mcp?token=abc", "https://e.test/mcp?***"),
            ("https://e.test/mcp#tok", "https://e.test/mcp?***"),
            ("https://u:p@e.test/mcp", "https://***@e.test/mcp"),
            ("https://e.test/a@b/c", "https://e.test/a@b/c"),
            ("", ""),
        ] {
            assert_eq!(redact_url(input), want, "入力: {input}");
        }
    }

    // ---- 書き戻し --------------------------------------------------------

    #[test]
    fn 無効化はキー順とコメントを保つ() {
        let src = "{\n  // 先頭コメント\n  \"mcpServers\": {\n    \"zeta\": {\n      \"command\": \"c\",\n      \"args\": [\"--x\"] // 引数\n    },\n    \"alpha\": {\"command\": \"d\"}\n  }\n}\n";
        let out =
            toggle_disabled(src, ConfigSource::ProjectMcpJson, "zeta", true).expect("書き戻せる");
        assert!(out.contains("// 先頭コメント"), "コメントが消えた:\n{out}");
        assert!(out.contains("// 引数"), "行末コメントが消えた:\n{out}");
        assert!(
            out.find("\"zeta\"").unwrap() < out.find("\"alpha\"").unwrap(),
            "キー順が変わった:\n{out}"
        );
        assert!(out.contains("\"disabled\": true"), "{out}");
        // 元の行は 1 行だけ増える (整形が総取っ替えになっていない)
        assert_eq!(
            out.lines().count(),
            src.lines().count() + 1,
            "整形が変わっている:\n{out}"
        );
        let after = parse_mcp_config(&out, ConfigSource::ProjectMcpJson);
        assert_eq!(after.len(), 2);
        assert!(after.iter().any(|s| s.name == "zeta" && s.disabled));
        assert!(after.iter().any(|s| s.name == "alpha" && !s.disabled));
    }

    #[test]
    fn 既にある_disabled_は値だけ差し替える() {
        let src = r#"{"mcpServers": {"a": {"command": "c", "disabled": true, "args": []}}}"#;
        let out = toggle_disabled(src, ConfigSource::UserCursor, "a", false).expect("戻せる");
        assert_eq!(
            out,
            r#"{"mcpServers": {"a": {"command": "c", "disabled": false, "args": []}}}"#
        );
        // 往復して元に戻る
        let back = toggle_disabled(&out, ConfigSource::UserCursor, "a", true).expect("戻せる");
        assert_eq!(back, src);
    }

    #[test]
    fn 書き戻しのテーブル() {
        // (説明, 入力, 出典, サーバ名, 期待)
        type Want = Result<(), EditError>;
        let cases: Vec<(&str, String, ConfigSource, &str, Want)> = vec![
            (
                "普通の JSON",
                r#"{"mcpServers": {"a": {"command": "c"}}}"#.into(),
                ConfigSource::ProjectMcpJson,
                "a",
                Ok(()),
            ),
            (
                "空オブジェクトのサーバ",
                r#"{"mcpServers": {"a": {}}}"#.into(),
                ConfigSource::ProjectMcpJson,
                "a",
                Ok(()),
            ),
            (
                "VS Code の servers",
                r#"{"servers": {"a": {"command": "c"}}}"#.into(),
                ConfigSource::ProjectVscode,
                "a",
                Ok(()),
            ),
            (
                "TOML は編集非対応",
                "[mcp_servers.a]\ncommand = \"c\"\n".into(),
                ConfigSource::UserCodex,
                "a",
                Err(EditError::Unsupported),
            ),
            (
                "居ないサーバ",
                r#"{"mcpServers": {"a": {"command": "c"}}}"#.into(),
                ConfigSource::ProjectMcpJson,
                "b",
                Err(EditError::NotFound("b".into())),
            ),
            (
                "サーバ表が無い",
                "{}".into(),
                ConfigSource::ProjectMcpJson,
                "a",
                Err(EditError::NotFound("a".into())),
            ),
            (
                "値がオブジェクトでない",
                r#"{"mcpServers": {"a": "node"}}"#.into(),
                ConfigSource::ProjectMcpJson,
                "a",
                Err(EditError::Unsupported),
            ),
            (
                "壊れた JSON",
                r#"{"mcpServers": {"a": }"#.into(),
                ConfigSource::ProjectMcpJson,
                "a",
                Err(EditError::WouldCorrupt),
            ),
            (
                "本体がオブジェクトでない",
                "[]".into(),
                ConfigSource::ProjectMcpJson,
                "a",
                Err(EditError::NotFound("設定の本体 (オブジェクト)".into())),
            ),
        ];
        for (why, text, source, name, want) in cases {
            let got = toggle_disabled(&text, source, name, true);
            match (&got, &want) {
                (Ok(out), Ok(())) => {
                    let after = parse_mcp_config(out, source);
                    assert!(
                        after.iter().any(|s| s.name == name && s.disabled),
                        "{why}: 切り替わっていない\n{out}"
                    );
                }
                (Err(a), Err(b)) => assert_eq!(a, b, "{why}"),
                _ => panic!("{why}: 期待 {want:?} / 実際 {got:?}"),
            }
        }
    }

    #[test]
    fn 他のサーバの状態は巻き込まない() {
        let src =
            r#"{"mcpServers": {"a": {"command": "c", "disabled": true}, "b": {"command": "d"}}}"#;
        let out = toggle_disabled(src, ConfigSource::UserCursor, "b", true).expect("書ける");
        let after = parse_mcp_config(&out, ConfigSource::UserCursor);
        assert!(
            after.iter().any(|s| s.name == "a" && s.disabled),
            "a を巻き込んだ"
        );
        assert!(after.iter().any(|s| s.name == "b" && s.disabled));
    }

    #[test]
    fn 名前に紛らわしい文字が入っていても壊さない() {
        // 値の中に `}` や `"disabled"` の**文字列**が入っていても誤爆しない
        let src = r#"{"mcpServers": {"a": {"command": "echo }", "args": ["\"disabled\": true"]}}}"#;
        let out = toggle_disabled(src, ConfigSource::ProjectMcpJson, "a", true).expect("書ける");
        let after = parse_mcp_config(&out, ConfigSource::ProjectMcpJson);
        assert_eq!(after.len(), 1);
        assert!(after[0].disabled);
        match &after[0].transport {
            Transport::Stdio { command, args, .. } => {
                assert_eq!(command, "echo }");
                assert_eq!(args, &vec!["\"disabled\": true".to_string()]);
            }
            other => panic!("stdio のはず: {other:?}"),
        }
    }

    #[test]
    fn 控えのパスは元の名前を残す() {
        let base = std::path::Path::new("dir");
        assert_eq!(
            backup_path(&base.join(".mcp.json")).file_name().unwrap(),
            std::ffi::OsStr::new(".mcp.json.zaivern.bak")
        );
        assert_eq!(
            backup_path(&base.join("config.toml")).file_name().unwrap(),
            std::ffi::OsStr::new("config.toml.zaivern.bak")
        );
        // 親ディレクトリは変わらない
        assert_eq!(backup_path(&base.join("x.json")).parent(), Some(base));
    }

    #[test]
    fn ファイルへの書き戻しは控えを残す() {
        let dir = crate::test_util::unique_temp_dir("zaivern-mcp-test", "write");
        let path = dir.join(".mcp.json");
        let src = "{\n  \"mcpServers\": {\n    \"a\": {\"command\": \"c\"}\n  }\n}\n";
        std::fs::write(&path, src).expect("書ける");
        write_toggle(&path, ConfigSource::ProjectMcpJson, "a", true).expect("切り替えられる");
        let after = std::fs::read_to_string(&path).expect("読める");
        assert!(after.contains("\"disabled\": true"), "{after}");
        let bak = std::fs::read_to_string(backup_path(&path)).expect("控えがある");
        assert_eq!(bak, src, "控えが元の中身と違う");
        // 読み取り専用の出典は書かずに断る
        let toml_path = dir.join("config.toml");
        std::fs::write(&toml_path, "[mcp_servers.a]\ncommand = \"c\"\n").expect("書ける");
        assert!(write_toggle(&toml_path, ConfigSource::UserCodex, "a", true).is_err());
        assert!(!backup_path(&toml_path).exists(), "断ったのに控えを作った");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 走査 -----------------------------------------------------------

    #[test]
    fn 走査はプロジェクトの3か所を見る() {
        let dir = crate::test_util::unique_temp_dir("zaivern-mcp-test", "scan");
        std::fs::write(
            dir.join(".mcp.json"),
            r#"{"mcpServers": {"p": {"command": "c"}}}"#,
        )
        .expect("書ける");
        std::fs::create_dir_all(dir.join(".cursor")).expect("作れる");
        std::fs::write(
            dir.join(".cursor").join("mcp.json"),
            r#"{"mcpServers": {"q": {"command": "c"}}}"#,
        )
        .expect("書ける");
        std::fs::create_dir_all(dir.join(".vscode")).expect("作れる");
        std::fs::write(dir.join(".vscode").join("mcp.json"), "{ not json").expect("書ける");

        let inv = scan(std::slice::from_ref(&dir));
        let names: Vec<&str> = inv.servers().iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"p"), "{names:?}");
        assert!(names.contains(&"q"), "{names:?}");
        // 壊れたファイルは理由付きで残る (握り潰さない)
        let broken = inv.broken();
        assert!(
            broken.iter().any(|(p, _)| p.ends_with("mcp.json")),
            "壊れた .vscode/mcp.json が報告されていない"
        );
        // 出典パスが埋まっている
        for s in inv.servers() {
            assert!(s.path.is_absolute() || s.path.exists(), "{:?}", s.path);
        }
        // 同じルートを 2 回渡しても二重に数えない
        let dup = scan(&[dir.clone(), dir.clone()]);
        assert_eq!(dup.count(), inv.count(), "重複して数えている");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 存在しないファイルはエラーではない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-mcp-test", "missing");
        let f = load_file(ConfigSource::ProjectMcpJson, dir.join(".mcp.json"));
        assert_eq!(f.state, FileState::Missing);
        assert!(f.servers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 出典ごとのパスは区切り文字を直書きしない() {
        let root = std::path::Path::new("root");
        assert_eq!(
            ConfigSource::ProjectCursor.project_path(root),
            Some(root.join(".cursor").join("mcp.json"))
        );
        assert_eq!(ConfigSource::UserClaude.project_path(root), None);
        // ユーザー階層はホームから導く (取れない環境では None)
        for s in USER_SOURCES {
            match dirs::home_dir() {
                Some(h) => assert!(
                    s.user_path().is_some_and(|p| p.starts_with(&h)),
                    "{s:?} がホーム配下でない"
                ),
                None => assert_eq!(s.user_path(), None),
            }
        }
        for s in PROJECT_SOURCES {
            assert_eq!(s.user_path(), None, "{s:?}");
        }
    }

    #[test]
    fn 件数バッジは0のとき出さない() {
        let mut p = McpPanel::default();
        assert_eq!(p.badge(), None, "0 件でバッジを出している");
        p.inventory.files.push(ConfigFile {
            source: ConfigSource::ProjectMcpJson,
            path: PathBuf::new(),
            state: FileState::Ok,
            servers: parse_mcp_config(
                r#"{"mcpServers": {"a": {"command": "c"}}}"#,
                ConfigSource::ProjectMcpJson,
            ),
        });
        assert_eq!(p.badge(), Some(1));
    }

    // ---- レイアウト: テーブルテスト --------------------------------------

    #[test]
    fn 行レイアウトのテーブル() {
        // (可用幅, バッジを出すか, 効き先を出すか, 出典を出すか, 操作を縮退させるか)
        let cases: [(f32, bool, bool, bool, bool); 9] = [
            (0.0, false, false, false, true),
            (60.0, false, false, false, true),
            (200.0, false, false, false, true),
            (300.0, true, false, false, true),
            (419.0, true, true, false, true),
            (420.0, true, true, false, false),
            (700.0, true, true, true, false),
            (900.0, true, true, true, false),
            (2000.0, true, true, true, false),
        ];
        for (w, badge, agents, source, compact) in cases {
            let l = row_layout(w);
            assert_eq!(l.badge_w > 0.0, badge, "幅 {w}: バッジ {l:?}");
            assert_eq!(l.agents_w > 0.0, agents, "幅 {w}: 効き先 {l:?}");
            assert_eq!(l.source_w > 0.0, source, "幅 {w}: 出典 {l:?}");
            assert_eq!(l.compact_actions, compact, "幅 {w}: 操作 {l:?}");
        }
    }

    #[test]
    fn 行はどの幅でも見切れない() {
        let mut w = -50.0_f32;
        while w <= 2400.0 {
            let l = row_layout(w);
            let avail = w.max(0.0);
            assert!(
                l.total() <= avail + 0.001,
                "幅 {w}: 合計 {} が可用幅を超えた {l:?}",
                l.total()
            );
            for v in [l.badge_w, l.name_w, l.agents_w, l.source_w, l.actions_w] {
                assert!(v >= 0.0 && v.is_finite(), "幅 {w}: 負/非有限の列 {l:?}");
            }
            // 操作列は最後まで落とさない (切り替えに到達できなくなるため)
            if avail >= 1.0 {
                assert!(l.actions_w > 0.0, "幅 {w}: 操作列が消えた {l:?}");
            }
            w += 7.0;
        }
        // 非有限入力でも壊れない
        let l = row_layout(f32::NAN);
        assert!(l.total() <= 0.001, "{l:?}");
    }

    #[test]
    fn 空状態カードは常に可用領域の中に収まる() {
        let sizes = [
            (900.0_f32, 700.0_f32),
            (1200.0, 300.0),
            (320.0, 120.0),
            (200.0, 40.0),
            (0.0, 0.0),
        ];
        for (w, h) in sizes {
            let avail = egui::Rect::from_min_size(egui::pos2(11.0, 23.0), egui::vec2(w, h));
            let card = empty_card(avail);
            assert!(
                avail.contains_rect(card),
                "{w}x{h}: カードがはみ出した {card:?} / {avail:?}"
            );
            assert!(card.width() >= 0.0 && card.height() >= 0.0, "{card:?}");
            // 中央に置く (左右・上下の余りが等しい)
            assert!(
                ((card.left() - avail.left()) - (avail.right() - card.right())).abs() < 0.01,
                "{w}x{h}: 水平中央でない"
            );
            assert!(
                ((card.top() - avail.top()) - (avail.bottom() - card.bottom())).abs() < 0.01,
                "{w}x{h}: 垂直中央でない"
            );
        }
    }

    #[test]
    fn 省略は文字境界で切る() {
        assert_eq!(ellipsize("abc", 5), "abc");
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        assert_eq!(ellipsize("日本語テスト", 3), "日本…");
        assert_eq!(ellipsize("abc", 0), "");
        assert_eq!(ellipsize("", 5), "");
    }

    // ---- ハードコーディングの番人 ----------------------------------------

    #[test]
    fn ソースに絶対パスを直書きしていない() {
        let src = include_str!("mcp.rs").replace("\r\n", "\n");
        // 検出語をそのまま書くと**この番人自身**が引っかかるので、組み立てる。
        let q = "\u{22}"; // "
        let sl = "\u{2f}"; // /
        let bs = "\u{5c}"; // \
        let bad = [
            format!("{q}{sl}tmp"),
            format!("{sl}Users{sl}"),
            format!("{sl}home{sl}"),
            format!("C:{bs}"),
        ];
        for b in &bad {
            assert!(!src.contains(b.as_str()), "絶対パスの直書きがある: {b}");
        }
        // 区切り文字入りのパス文字列も書かない (必ず `Path::join` で組む)
        for name in ["mcp.json", "config.toml", "settings.json"] {
            let joined = format!("{sl}{name}{q}");
            assert!(
                !src.contains(joined.as_str()),
                "区切り文字入りのパス文字列がある: {joined}"
            );
        }
    }
}
