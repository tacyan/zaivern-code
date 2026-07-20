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

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use serde::Deserialize;

// ─── マニフェスト ────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmdInput {
    None,
    Selection,
    File,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmdOutput {
    Replace,
    Insert,
    NewTab,
    Notify,
    Silent,
}

#[derive(Clone, Debug)]
pub struct PluginCommand {
    /// マニフェスト仕様の一部 (コマンドの安定識別子。現状 UI では未使用)
    #[allow(dead_code)]
    pub id: String,
    pub title: String,
    pub icon: String,
    pub run: String,
    pub input: CmdInput,
    pub output: CmdOutput,
    /// 空 = 全言語。要素は snippets::lang_id_for 形式の言語ID (小文字)。
    pub langs: Vec<String>,
    pub keybind: Option<String>,
    pub on_save: bool,
    pub timeout_secs: u64,
}

impl PluginCommand {
    /// 言語フィルタの判定 (空リストは全言語にマッチ)。
    pub fn lang_matches(&self, lang_id: &str) -> bool {
        self.langs.is_empty() || self.langs.iter().any(|l| l.eq_ignore_ascii_case(lang_id))
    }
}

#[derive(Debug)]
pub struct Plugin {
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub dir: PathBuf,
    pub commands: Vec<PluginCommand>,
    pub themes: Vec<(String, PathBuf)>,        // (label, json path)
    pub snippet_files: Vec<(String, PathBuf)>, // (language, path)
    /// マニフェストが壊れている場合の理由 (一覧に ⚠ 表示するため)。
    pub error: Option<String>,
}

// serde 用の生マニフェスト。検証は validate() で行う。
#[derive(Deserialize)]
struct RawManifest {
    plugin: RawPlugin,
    #[serde(default, rename = "command")]
    commands: Vec<RawCommand>,
    #[serde(default, rename = "theme")]
    themes: Vec<RawTheme>,
    #[serde(default, rename = "snippet")]
    snippets: Vec<RawSnippet>,
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
                dir: path,
                commands: Vec::new(),
                themes: Vec::new(),
                snippet_files: Vec::new(),
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
        let output = match c.output.trim() {
            "replace" => CmdOutput::Replace,
            "insert" => CmdOutput::Insert,
            "new_tab" => CmdOutput::NewTab,
            "" | "notify" => CmdOutput::Notify,
            "silent" => CmdOutput::Silent,
            other => return Err(format!("command[{i}].output が不正: {other:?}")),
        };
        // 保存時フックは「ファイル全体を整形して置き換える」動作に限定する
        if c.on_save && (input != CmdInput::File || output != CmdOutput::Replace) {
            return Err(format!(
                "command[{i}]: on_save = true には input = \"file\", output = \"replace\" が必要です"
            ));
        }
        let id = if c.id.trim().is_empty() {
            format!("cmd{i}")
        } else {
            c.id.trim().to_string()
        };
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

    Ok(Plugin {
        name,
        version: some_or(&m.plugin.version, "0.1.0"),
        author: m.plugin.author.trim().to_string(),
        description: m.plugin.description.trim().to_string(),
        dir: dir.to_path_buf(),
        commands,
        themes,
        snippet_files,
        error: None,
    })
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

# ─── コマンド ─────────────────────────────────────────────
# 任意のシェルコマンドを実行し、結果をエディタへ反映します。
#   input:  none | selection | file   … stdin に渡すもの
#   output: replace | insert | new_tab | notify | silent
#           replace = 選択範囲(input=selection)/ファイル全体(input=file)を stdout で置換
#   環境変数: ZV_FILE (フルパス) / ZV_LANG (言語ID) / ZV_WORKSPACE / ZV_PLUGIN_DIR

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
    pub title: String,
    pub output: CmdOutput,
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
        title: req.command.title.clone(),
        output: req.command.output,
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
            title: req.command.title.clone(),
            output: req.command.output,
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
            langs: Vec::new(),
            keybind: None,
            on_save: false,
            timeout_secs: timeout,
        }
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
