//! 外部 GUI IDE への受け渡し (ハンドオフ) モジュール。
//!
//! 「いま開いているファイルを、いまのカーソル行で VS Code / Cursor / Zed …で開く」
//! 「ワークスペースフォルダを外部 IDE で開く」を担当する。
//!
//! 設計方針:
//! - 引数の組み立て (`build_open_file_args` / `build_open_folder_args` / `url_for`) は
//!   純関数として切り出し、ユニットテストで argv を厳密に検証する。
//!   プロセス起動 (`launch`) と検出 (`detect_installed`) だけが副作用を持つ。
//! - 検出は必ずワーカースレッドで行う (`detect_async`)。UI スレッドを止めない。
//!   GUI アプリはログインシェルの PATH を継承しないので、
//!   src/app.rs の `which` と同じ手口で `$SHELL -lc 'command -v <bin>'` を使う。
//! - 結果はキャッシュする。毎フレーム shell out するのは論外。
//!
//! # 行番号・列番号の契約 (重要)
//!
//! **`line` / `col` はすべて 1 始まり (1-based)。** ここに載っている IDE は
//! 例外なく 1 始まりの座標系を取る。エディタ内部が 0 始まりなら、
//! **呼び出し側が +1 してから渡すこと。** 0 を渡してもこのモジュールは
//! 1 に丸めて事故を防ぐが、それは保険であって仕様ではない。
//!
//! # 検出の罠 (このマシンで実測・2026-07)
//!
//! `/usr/local/bin/code` は **Cursor のシム**であって VS Code ではない
//! (実体: `/Applications/Cursor.app/Contents/Resources/app/bin/code`)。
//! つまり `command -v code` がパスを返しても VS Code があるとは限らない。
//!
//! さらに厄介なことに、VS Code 系 (VS Code / Cursor / Trae / Kiro) の
//! `--version` 出力は製品名を含まない素のバージョン 3 行だけで、
//! Cursor と VS Code を **文字列マーカーでは区別できない**:
//!
//! ```text
//! $ code --version        $ cursor --version
//! 3.12.17                 3.12.17
//! 0fb762053c...           0fb762053c...
//! x64                     x64
//! ```
//!
//! そこで本モジュールは **実体パス (シンボリックリンク解決後) のマーカー照合**を
//! 一次判定に使う。他 IDE のマーカーに当たったら「別 IDE のシム」として棄却する。
//! `--version` マーカーは Zed / Xcode のように製品名を名乗るツール向けの二次判定。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

/// ファイルを「指定行で開く」ときの引数の形。IDE ごとに流儀が違う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileArgStyle {
    /// `-g file:LINE:COL` (VS Code 系: code / cursor / trae / kiro)
    GotoColon,
    /// `--goto file:LINE:COL` (`-g` の長い別名。Windsurf 向け)
    GotoSpaceColon,
    /// `--goto=file:LINE:COL` (JetBrains Fleet)
    GotoEquals,
    /// `file:LINE:COL` をそのまま渡す (Zed / Sublime Text)
    BareColon,
    /// `--line N --column C /abs/file` (JetBrains IDE 群)
    LineFlags,
    /// `--line N /abs/file` — **列を受け付けない** (Xcode xed / Android Studio)
    LineOnly,
    /// `+LINE file` (vim 系: Neovide)。列は指定できない。
    PlusLine,
    /// `+LINE:COL file` (Emacs emacsclient)
    PlusLineCol,
}

/// 1 つの外部 IDE の起動仕様。
#[derive(Debug, Clone, Copy)]
pub struct IdeSpec {
    /// 内部識別子 (設定ファイルに保存する安定キー)。
    pub key: &'static str,
    /// UI 表示名。
    pub label: &'static str,
    /// UI アイコン。
    pub icon: &'static str,
    /// PATH 上で探す実行ファイル名。
    pub bin: &'static str,
    /// `--version` 出力に含まれるはずの小文字マーカー。
    /// 空文字列 = そのツールは製品名を名乗らないので判定に使えない
    /// (VS Code 系がまさにこれ。詳細はモジュール冒頭のコメント)。
    pub version_marker: &'static str,
    /// 実体パス (シンボリックリンク解決後) を小文字化した文字列に
    /// 含まれるはずのマーカー群。いずれか 1 つ当たれば本人と確定する。
    /// 他 IDE のマーカーに当たった場合は「別 IDE のシム」として棄却する。
    pub path_markers: &'static [&'static str],
    /// ファイルを行指定で開くときの引数の形。
    pub file_arg: FileArgStyle,
    /// ファイル引数より前に置く固定引数 (`--` や `-c` など)。
    pub file_pre_args: &'static [&'static str],
    /// フォルダを開くときにディレクトリより前に置く固定引数 (Xcode の `-p` など)。
    pub folder_args: &'static [&'static str],
    /// 「既存ウィンドウにフォルダを追加」するフラグ。無ければ None。
    pub add_folder_arg: Option<&'static str>,
    /// CLI が無いときのフォールバック URL スキーム (`vscode` など)。
    /// URL は `scheme://file/<絶対パス>:LINE:COL` の形で組み立てる。
    pub url_scheme: Option<&'static str>,
    /// UI に出す補足説明。
    pub note: &'static str,
    /// この仕様を実機で検証済みか。false = ベストエフォート
    /// (UI では「動く保証なし」として見せること)。
    pub confirmed: bool,
}

/// 対応 IDE カタログ。
///
/// `confirmed: false` は「フォークの継承や公式ドキュメントからの推定で、
/// 実機検証はできていない」という意味。UI ではそう見せること。
pub const CATALOG: &[IdeSpec] = &[
    IdeSpec {
        key: "vscode",
        label: "VS Code",
        icon: "🟦",
        bin: "code",
        // VS Code 系の --version は製品名を出さないので照合不能。実体パスで判定する。
        version_marker: "",
        path_markers: &["visual studio code", "microsoft vs code", "/share/code/"],
        file_arg: FileArgStyle::GotoColon,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: Some("-a"),
        url_scheme: Some("vscode"),
        note: "code -g file:行:列 / code フォルダ。-n 新規, -r 再利用",
        confirmed: true,
    },
    IdeSpec {
        key: "vscode-insiders",
        label: "VS Code Insiders",
        icon: "🟩",
        bin: "code-insiders",
        version_marker: "",
        path_markers: &["visual studio code - insiders", "code - insiders"],
        file_arg: FileArgStyle::GotoColon,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: Some("-a"),
        url_scheme: Some("vscode-insiders"),
        note: "VS Code と同じ引数体系",
        confirmed: true,
    },
    IdeSpec {
        key: "cursor",
        label: "Cursor",
        icon: "🖱",
        bin: "cursor",
        version_marker: "",
        path_markers: &["cursor.app", "/cursor/", "cursor.exe"],
        file_arg: FileArgStyle::GotoColon,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: Some("-a"),
        url_scheme: Some("cursor"),
        note: "VS Code フォーク。-g / -a / -n / -r 実機確認済み",
        confirmed: true,
    },
    IdeSpec {
        key: "windsurf",
        label: "Windsurf",
        icon: "🌊",
        bin: "windsurf",
        version_marker: "",
        path_markers: &["windsurf.app", "/windsurf/", "windsurf.exe"],
        // VS Code フォークからの継承。実機未確認。
        file_arg: FileArgStyle::GotoSpaceColon,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: Some("-a"),
        url_scheme: None,
        note: "未検証: VS Code フォーク由来の推定 (--goto file:行:列)",
        confirmed: false,
    },
    IdeSpec {
        key: "zed",
        label: "Zed",
        icon: "⚡",
        bin: "zed",
        version_marker: "zed",
        path_markers: &["zed.app", "zed-editor", "/bin/zed"],
        file_arg: FileArgStyle::BareColon,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: Some("--add"),
        url_scheme: Some("zed"),
        note: "zed file:行:列 / zed フォルダ。--add 追加, --new 新規",
        confirmed: true,
    },
    IdeSpec {
        key: "sublime",
        label: "Sublime Text",
        icon: "📙",
        bin: "subl",
        version_marker: "sublime",
        path_markers: &["sublime text", "sublime_text"],
        file_arg: FileArgStyle::BareColon,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: Some("-a"),
        url_scheme: None,
        note: "subl file:行:列 / subl フォルダ。URL スキームなし",
        confirmed: true,
    },
    IdeSpec {
        key: "intellij",
        label: "IntelliJ IDEA",
        icon: "🧠",
        bin: "idea",
        version_marker: "",
        path_markers: &["intellij", "idea.app", "/idea"],
        file_arg: FileArgStyle::LineFlags,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: None,
        url_scheme: None, // jetbrains:// はプロジェクト名が必要でパスだけでは組めない
        note: "idea --line 行 --column 列 /abs/file。フォルダ指定は起動中の窓を再利用",
        confirmed: true,
    },
    IdeSpec {
        key: "pycharm",
        label: "PyCharm",
        icon: "🐍",
        bin: "pycharm",
        version_marker: "",
        path_markers: &["pycharm"],
        file_arg: FileArgStyle::LineFlags,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: None,
        url_scheme: None,
        note: "JetBrains 共通の引数体系",
        confirmed: true,
    },
    IdeSpec {
        key: "webstorm",
        label: "WebStorm",
        icon: "🌐",
        bin: "webstorm",
        version_marker: "",
        path_markers: &["webstorm"],
        file_arg: FileArgStyle::LineFlags,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: None,
        url_scheme: None,
        note: "JetBrains 共通の引数体系",
        confirmed: true,
    },
    IdeSpec {
        key: "rustrover",
        label: "RustRover",
        icon: "🦀",
        bin: "rustrover",
        version_marker: "",
        path_markers: &["rustrover"],
        file_arg: FileArgStyle::LineFlags,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: None,
        url_scheme: None,
        note: "JetBrains 共通の引数体系",
        confirmed: true,
    },
    IdeSpec {
        key: "goland",
        label: "GoLand",
        icon: "🐹",
        bin: "goland",
        version_marker: "",
        path_markers: &["goland"],
        file_arg: FileArgStyle::LineFlags,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: None,
        url_scheme: None,
        note: "JetBrains 共通の引数体系",
        confirmed: true,
    },
    IdeSpec {
        key: "fleet",
        label: "JetBrains Fleet",
        icon: "🚀",
        bin: "fleet",
        version_marker: "fleet",
        path_markers: &["fleet"],
        file_arg: FileArgStyle::GotoEquals,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: None,
        url_scheme: None,
        note: "fleet --goto=file:行:列 / fleet フォルダ",
        confirmed: true,
    },
    IdeSpec {
        key: "xcode",
        label: "Xcode",
        icon: "🔨",
        bin: "xed",
        version_marker: "xed",
        path_markers: &["/usr/bin/xed", "xcode.app"],
        // xed は列を受け付けない。col は捨てる。
        file_arg: FileArgStyle::LineOnly,
        file_pre_args: &[],
        folder_args: &["-p"],
        add_folder_arg: None,
        url_scheme: None,
        note: "xed --line 行 file (列指定は非対応) / xed -p フォルダ",
        confirmed: true,
    },
    IdeSpec {
        key: "android-studio",
        label: "Android Studio",
        icon: "🤖",
        bin: "studio",
        // studio --version は何も出力しない (実測)。実体パスで判定する。
        version_marker: "",
        path_markers: &["android studio.app", "android-studio"],
        file_arg: FileArgStyle::LineOnly,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: None,
        url_scheme: None,
        note: "未検証: studio --line 行 file (JetBrains 系からの推定)",
        confirmed: false,
    },
    IdeSpec {
        key: "trae",
        label: "Trae",
        icon: "🎯",
        bin: "trae",
        version_marker: "",
        path_markers: &["trae.app", "/trae/", "trae.exe"],
        file_arg: FileArgStyle::GotoColon,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: Some("-a"),
        url_scheme: None,
        note: "VS Code フォーク。-g / -a / -n / -r 実機確認済み",
        confirmed: true,
    },
    IdeSpec {
        key: "kiro",
        label: "Kiro",
        icon: "🪄",
        bin: "kiro",
        version_marker: "",
        path_markers: &["kiro.app", "/kiro/", "kiro.exe"],
        file_arg: FileArgStyle::GotoColon,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: Some("-a"),
        url_scheme: None,
        note: "VS Code フォーク。-g / -a / -n / -r 実機確認済み",
        confirmed: true,
    },
    IdeSpec {
        key: "antigravity",
        label: "Antigravity",
        icon: "🛸",
        bin: "agy-ide",
        version_marker: "",
        path_markers: &["antigravity"],
        file_arg: FileArgStyle::GotoColon,
        file_pre_args: &[],
        folder_args: &[],
        add_folder_arg: Some("-a"),
        // CLI シムがアプリバンドルにも PATH にも無い。URL スキームだけが頼り。
        url_scheme: Some("antigravity"),
        note: "未検証: CLI シム無し。antigravity:// URL 経由で開く (パス形式も未確認)",
        confirmed: false,
    },
    IdeSpec {
        key: "neovide",
        label: "Neovide",
        icon: "📝",
        bin: "neovide",
        version_marker: "neovide",
        path_markers: &["neovide"],
        // vim の +LINE は列を取れない。
        file_arg: FileArgStyle::PlusLine,
        file_pre_args: &["--"],
        folder_args: &["--"],
        add_folder_arg: None,
        url_scheme: None,
        note: "neovide -- +行 file (列指定は非対応)",
        confirmed: true,
    },
    IdeSpec {
        key: "emacs",
        label: "Emacs",
        icon: "🅴",
        bin: "emacsclient",
        version_marker: "emacs",
        path_markers: &["emacs"],
        file_arg: FileArgStyle::PlusLineCol,
        file_pre_args: &["-c"],
        folder_args: &["-c"],
        add_folder_arg: None,
        url_scheme: None,
        note: "emacsclient -c +行:列 file。フォルダを渡すと dired で開く",
        confirmed: true,
    },
];

/// キーから仕様を引く。
pub fn spec_by_key(key: &str) -> Option<&'static IdeSpec> {
    CATALOG.iter().find(|s| s.key == key)
}

/// 行・列を 1 始まりに正規化する。
///
/// 契約上は呼び出し側が 1 始まりで渡す。0 が来たら呼び出し側のバグだが、
/// 「1 行目が開けない」より「黙って 1 行目を開く」方がマシなので丸める。
fn one_based(v: usize) -> usize {
    v.max(1)
}

/// パスを文字列に落とす。非 UTF-8 パスは lossy 変換する
/// (argv に載せる以上どのみち OS 側で解釈されるので実害はない)。
fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// 可能なら絶対パスにする。カレントディレクトリが取れない場合は元のまま返す。
fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p),
        Err(_) => p.to_path_buf(),
    }
}

/// ファイルを指定行・指定列で開くための argv を組み立てる (実行ファイル名は含まない)。
///
/// **`line` / `col` は 1 始まり。** 0 始まりの値を渡すのは呼び出し側のバグ
/// (安全のため 1 に丸めるが、それに依存しないこと)。
///
/// パスは絶対パスに正規化して渡す。空白を含むパスでも 1 つの argv 要素として
/// 渡るのでクォートは不要 (シェルを経由しないため)。
pub fn build_open_file_args(spec: &IdeSpec, path: &Path, line: usize, col: usize) -> Vec<String> {
    let line = one_based(line);
    let col = one_based(col);
    let abs = path_string(&absolutize(path));

    let mut args: Vec<String> = spec.file_pre_args.iter().map(|s| s.to_string()).collect();
    match spec.file_arg {
        FileArgStyle::GotoColon => {
            args.push("-g".into());
            args.push(format!("{abs}:{line}:{col}"));
        }
        FileArgStyle::GotoSpaceColon => {
            args.push("--goto".into());
            args.push(format!("{abs}:{line}:{col}"));
        }
        FileArgStyle::GotoEquals => {
            args.push(format!("--goto={abs}:{line}:{col}"));
        }
        FileArgStyle::BareColon => {
            args.push(format!("{abs}:{line}:{col}"));
        }
        FileArgStyle::LineFlags => {
            args.push("--line".into());
            args.push(line.to_string());
            args.push("--column".into());
            args.push(col.to_string());
            args.push(abs);
        }
        FileArgStyle::LineOnly => {
            // 列は捨てる (xed 等は --column を受け付けない)。
            args.push("--line".into());
            args.push(line.to_string());
            args.push(abs);
        }
        FileArgStyle::PlusLine => {
            args.push(format!("+{line}"));
            args.push(abs);
        }
        FileArgStyle::PlusLineCol => {
            args.push(format!("+{line}:{col}"));
            args.push(abs);
        }
    }
    args
}

/// フォルダを開くための argv を組み立てる。
///
/// `add = true` かつ `add_folder_arg` を持つ IDE では、既存ウィンドウに
/// フォルダを追加する。対応していない IDE では黙って通常の「開く」に落とす。
pub fn build_open_folder_args(spec: &IdeSpec, dir: &Path, add: bool) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if add {
        if let Some(flag) = spec.add_folder_arg {
            args.push(flag.to_string());
        }
    }
    args.extend(spec.folder_args.iter().map(|s| s.to_string()));
    args.push(path_string(&absolutize(dir)));
    args
}

/// URL の path 部分をパーセントエンコードする。
/// `/` と unreserved 文字はそのまま残す (依存クレートを増やさない方針)。
fn percent_encode_path(s: &str) -> String {
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

/// CLI シムが無いときのフォールバック用 URL を組み立てる。
///
/// 形式は `scheme://file/<絶対パス>:行:列`。**行・列は 1 始まり。**
/// URL スキームを持たない IDE では None を返す。
pub fn url_for(spec: &IdeSpec, path: &Path, line: usize, col: usize) -> Option<String> {
    let scheme = spec.url_scheme?;
    let line = one_based(line);
    let col = one_based(col);
    let abs = path_string(&absolutize(path));
    // 先頭の / は scheme://file/ の / と重ねない。
    let trimmed = abs.strip_prefix('/').unwrap_or(&abs);
    Some(format!(
        "{scheme}://file/{}:{line}:{col}",
        percent_encode_path(trimmed)
    ))
}

/// フォルダ用の URL (行・列なし)。
pub fn folder_url_for(spec: &IdeSpec, dir: &Path) -> Option<String> {
    let scheme = spec.url_scheme?;
    let abs = path_string(&absolutize(dir));
    let trimmed = abs.strip_prefix('/').unwrap_or(&abs);
    Some(format!(
        "{scheme}://file/{}",
        percent_encode_path(trimmed)
    ))
}

/// 検出結果 1 件。
#[derive(Debug, Clone)]
pub struct DetectedIde {
    /// 対応する `IdeSpec::key`。
    pub key: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    /// `command -v` が返した実行ファイルのパス。
    pub bin_path: String,
    /// `--version` の 1 行目 (取れた場合)。
    pub version: Option<String>,
    /// 実体パスまたは `--version` マーカーで本人確認が取れたか。
    /// false = 名前が一致しただけのベストエフォート。
    pub identity_verified: bool,
    /// 起動仕様そのものが実機検証済みか (`IdeSpec::confirmed` のコピー)。
    pub confirmed: bool,
}

impl DetectedIde {
    pub fn spec(&self) -> &'static IdeSpec {
        spec_by_key(self.key).expect("DetectedIde は CATALOG 由来なので必ず引ける")
    }
}

fn cache() -> &'static Mutex<Option<Vec<DetectedIde>>> {
    static CACHE: OnceLock<Mutex<Option<Vec<DetectedIde>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// キャッシュを捨てる。IDE を新しくインストールした直後などに呼ぶ。
pub fn invalidate_cache() {
    if let Ok(mut c) = cache().lock() {
        *c = None;
    }
}

/// キャッシュ済みの検出結果があれば返す (ブロックしない)。UI スレッドから呼んでよい。
pub fn cached() -> Option<Vec<DetectedIde>> {
    cache().lock().ok().and_then(|c| c.clone())
}

/// `$SHELL -lc 'command -v <bin>'` で実行ファイルの絶対パスを得る。
///
/// GUI アプリはログインシェルの PATH を継承しないため、素の `Command` では
/// `/usr/local/bin` などが見えない。src/app.rs の `which` と同じ手口。
fn which_path(bin: &str) -> Option<String> {
    if bin.is_empty() {
        return None;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let out = Command::new(shell)
        .arg("-lc")
        .arg(format!("command -v {bin}"))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() {
        None
    } else {
        Some(p)
    }
}

/// `<bin> --version` の 1 行目を取る。出さないツールもあるので None を許す。
fn version_line(bin_path: &str) -> Option<String> {
    let out = Command::new(bin_path)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    if text.trim().is_empty() {
        text = String::from_utf8_lossy(&out.stderr).to_string();
    }
    let first = text.lines().next()?.trim().to_string();
    if first.is_empty() {
        None
    } else {
        Some(first)
    }
}

/// 実体パス (シンボリックリンク解決後) を小文字で返す。解決できなければ元のパス。
fn canonical_lower(bin_path: &str) -> String {
    std::fs::canonicalize(bin_path)
        .map(|p| p.to_string_lossy().to_lowercase())
        .unwrap_or_else(|_| bin_path.to_lowercase())
}

/// 実体パスのマーカー照合で本人確認する。
///
/// 戻り値: `Some(true)` = 本人確定 / `Some(false)` = 別 IDE のシムなので棄却 /
/// `None` = マーカーが 1 つも当たらず判定不能 (`--version` へフォールバック)。
///
/// `/usr/local/bin/code` → `/Applications/Cursor.app/.../bin/code` のような
/// 「他 IDE のシム」を確実に弾くのがこの関数の存在意義。
fn identify_by_path(spec: &IdeSpec, canon: &str) -> Option<bool> {
    if spec.path_markers.iter().any(|m| canon.contains(m)) {
        return Some(true);
    }
    // 自分のマーカーには当たらないが、他 IDE のマーカーには当たる → 別 IDE のシム。
    let stolen = CATALOG
        .iter()
        .filter(|other| other.key != spec.key)
        .any(|other| other.path_markers.iter().any(|m| canon.contains(m)));
    if stolen {
        Some(false)
    } else {
        None
    }
}

/// 1 つの IDE を検出する。見つからない/別 IDE のシムだった場合は None。
fn detect_one(spec: &IdeSpec) -> Option<DetectedIde> {
    let bin_path = which_path(spec.bin)?;
    let canon = canonical_lower(&bin_path);

    let mut identity_verified = false;
    match identify_by_path(spec, &canon) {
        // 実体パスが他 IDE のバンドルを指している → これは別 IDE のシム。棄却。
        Some(false) => return None,
        Some(true) => identity_verified = true,
        None => {}
    }

    // 実体パスで確定できなかった場合だけ --version を叩く (プロセス起動を節約)。
    let version = if identity_verified && spec.version_marker.is_empty() {
        None
    } else {
        version_line(&bin_path)
    };

    if !identity_verified && !spec.version_marker.is_empty() {
        let v = version.as_deref().unwrap_or("").to_lowercase();
        if !v.contains(spec.version_marker) {
            return None;
        }
        identity_verified = true;
    }

    Some(DetectedIde {
        key: spec.key,
        label: spec.label,
        icon: spec.icon,
        bin_path,
        version,
        identity_verified,
        confirmed: spec.confirmed,
    })
}

/// インストール済み IDE を検出する (ブロックする)。
///
/// **UI スレッドから直接呼ばないこと。** シェルを IDE の数だけ起動するので
/// 数百 ms〜数秒かかる。UI からは `detect_async` を使う。
/// 2 回目以降はキャッシュを返すので安い。
pub fn detect_installed() -> Vec<DetectedIde> {
    if let Some(hit) = cached() {
        return hit;
    }
    let found: Vec<DetectedIde> = CATALOG.iter().filter_map(detect_one).collect();
    if let Ok(mut c) = cache().lock() {
        *c = Some(found.clone());
    }
    found
}

/// ワーカースレッドで検出し、結果を tx へ送って再描画を要求する。
/// src/plugins.rs の `run_async` と同じ作法。
pub fn detect_async(tx: Sender<Vec<DetectedIde>>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let found = detect_installed();
        let _ = tx.send(found);
        ctx.request_repaint();
    });
}

/// OS の標準ハンドラで URL を開く (URL スキームによるフォールバック)。
fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    spawn_detached(&mut cmd).map_err(|e| format!("URL を開けませんでした ({url}): {e}"))
}

/// 子プロセスを切り離して起動する。
///
/// - stdin/stdout/stderr は null に落とす (エディタの出力を汚さない)。
/// - 待たないがゾンビも残さない: 別スレッドで `wait` だけさせて回収する。
fn spawn_detached(cmd: &mut Command) -> Result<(), std::io::Error> {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // wait しないと Unix ではゾンビが残る。UI は止めたくないのでスレッドで回収する。
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

/// IDE を起動する。UI スレッドから呼んでよい (待たずに即座に返る)。
///
/// CLI が PATH に無く URL スキームがある場合は、そちらへフォールバックしたい。
/// その判断には開きたい対象が必要なので、フォールバックが要るときは
/// `launch_file` / `launch_folder` を使うこと。
pub fn launch(spec: &IdeSpec, args: &[String]) -> Result<(), String> {
    let bin = match which_path(spec.bin) {
        Some(p) => p,
        None => {
            return Err(format!(
                "{} が見つかりません ({} が PATH にありません)",
                spec.label, spec.bin
            ))
        }
    };
    let mut cmd = Command::new(&bin);
    cmd.args(args);
    spawn_detached(&mut cmd).map_err(|e| format!("{} の起動に失敗しました: {e}", spec.label))
}

/// ファイルを指定行・指定列で開く。**行・列は 1 始まり。**
///
/// CLI が無い場合は URL スキームへフォールバックする
/// (Antigravity のように CLI シムが存在しない IDE のための経路)。
pub fn launch_file(spec: &IdeSpec, path: &Path, line: usize, col: usize) -> Result<(), String> {
    if which_path(spec.bin).is_some() {
        return launch(spec, &build_open_file_args(spec, path, line, col));
    }
    if let Some(url) = url_for(spec, path, line, col) {
        return open_url(&url);
    }
    Err(format!(
        "{} が見つかりません ({} が PATH に無く、URL スキームもありません)",
        spec.label, spec.bin
    ))
}

/// フォルダを開く。`add = true` なら既存ウィンドウへ追加する (対応 IDE のみ)。
pub fn launch_folder(spec: &IdeSpec, dir: &Path, add: bool) -> Result<(), String> {
    if which_path(spec.bin).is_some() {
        return launch(spec, &build_open_folder_args(spec, dir, add));
    }
    if let Some(url) = folder_url_for(spec, dir) {
        return open_url(&url);
    }
    Err(format!(
        "{} が見つかりません ({} が PATH に無く、URL スキームもありません)",
        spec.label, spec.bin
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(key: &str) -> &'static IdeSpec {
        spec_by_key(key).unwrap()
    }

    /// テスト内では絶対パスだけを使い、cwd 依存を排除する。
    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn goto_colon_vscode_family() {
        let args = build_open_file_args(spec("vscode"), &p("/tmp/a.rs"), 42, 5);
        assert_eq!(args, vec!["-g", "/tmp/a.rs:42:5"]);
        // フォークも同じ形
        for key in ["cursor", "trae", "kiro", "vscode-insiders"] {
            let a = build_open_file_args(spec(key), &p("/tmp/a.rs"), 7, 3);
            assert_eq!(a, vec!["-g", "/tmp/a.rs:7:3"], "{key}");
        }
    }

    #[test]
    fn goto_space_colon_windsurf() {
        let args = build_open_file_args(spec("windsurf"), &p("/tmp/a.rs"), 10, 2);
        assert_eq!(args, vec!["--goto", "/tmp/a.rs:10:2"]);
    }

    #[test]
    fn goto_equals_fleet() {
        let args = build_open_file_args(spec("fleet"), &p("/tmp/a.rs"), 10, 5);
        assert_eq!(args, vec!["--goto=/tmp/a.rs:10:5"]);
    }

    #[test]
    fn bare_colon_zed_and_sublime() {
        assert_eq!(
            build_open_file_args(spec("zed"), &p("/tmp/a.rs"), 12, 9),
            vec!["/tmp/a.rs:12:9"]
        );
        assert_eq!(
            build_open_file_args(spec("sublime"), &p("/tmp/a.rs"), 12, 9),
            vec!["/tmp/a.rs:12:9"]
        );
    }

    #[test]
    fn line_flags_jetbrains() {
        let args = build_open_file_args(spec("intellij"), &p("/src/main.rs"), 42, 5);
        assert_eq!(
            args,
            vec!["--line", "42", "--column", "5", "/src/main.rs"],
            "JetBrains は --line と --column を別々に取り、パスは最後"
        );
        for key in ["pycharm", "webstorm", "rustrover", "goland"] {
            let a = build_open_file_args(spec(key), &p("/src/main.rs"), 1, 1);
            assert_eq!(a, vec!["--line", "1", "--column", "1", "/src/main.rs"], "{key}");
        }
    }

    #[test]
    fn line_only_xcode_drops_column() {
        let args = build_open_file_args(spec("xcode"), &p("/src/App.swift"), 99, 40);
        assert_eq!(
            args,
            vec!["--line", "99", "/src/App.swift"],
            "xed は列を受け付けないので col は捨てる"
        );
        assert!(!args.iter().any(|a| a == "--column"));
        assert!(!args.iter().any(|a| a == "40"));
    }

    #[test]
    fn line_only_android_studio() {
        let args = build_open_file_args(spec("android-studio"), &p("/src/Main.kt"), 8, 2);
        assert_eq!(args, vec!["--line", "8", "/src/Main.kt"]);
    }

    #[test]
    fn plus_line_neovide_has_separator_and_no_column() {
        let args = build_open_file_args(spec("neovide"), &p("/tmp/a.rs"), 33, 7);
        assert_eq!(args, vec!["--", "+33", "/tmp/a.rs"]);
    }

    #[test]
    fn plus_line_col_emacs() {
        let args = build_open_file_args(spec("emacs"), &p("/tmp/a.rs"), 33, 7);
        assert_eq!(args, vec!["-c", "+33:7", "/tmp/a.rs"]);
    }

    #[test]
    fn path_with_spaces_stays_one_argv_element() {
        let path = p("/Users/me/My Projects/hello world.rs");
        let args = build_open_file_args(spec("cursor"), &path, 3, 4);
        assert_eq!(args.len(), 2, "クォート不要: シェルを経由しないので 1 要素のまま");
        assert_eq!(args[1], "/Users/me/My Projects/hello world.rs:3:4");

        let jb = build_open_file_args(spec("intellij"), &path, 3, 4);
        assert_eq!(jb.last().unwrap(), "/Users/me/My Projects/hello world.rs");

        let folder = build_open_folder_args(spec("zed"), &p("/Users/me/My Projects"), false);
        assert_eq!(folder, vec!["/Users/me/My Projects"]);
    }

    #[test]
    fn zero_based_input_is_clamped_to_one() {
        // 契約は 1 始まり。0 は呼び出し側のバグだが 1 に丸めて事故を防ぐ。
        let args = build_open_file_args(spec("cursor"), &p("/tmp/a.rs"), 0, 0);
        assert_eq!(args, vec!["-g", "/tmp/a.rs:1:1"]);
        let jb = build_open_file_args(spec("intellij"), &p("/tmp/a.rs"), 0, 0);
        assert_eq!(jb, vec!["--line", "1", "--column", "1", "/tmp/a.rs"]);
    }

    #[test]
    fn relative_path_is_absolutized() {
        let args = build_open_file_args(spec("zed"), &p("src/main.rs"), 2, 1);
        assert!(
            args[0].starts_with('/') || args[0].contains(":\\"),
            "相対パスは絶対パスへ正規化する: {}",
            args[0]
        );
        assert!(args[0].ends_with("src/main.rs:2:1"));
    }

    #[test]
    fn folder_args_plain_and_add() {
        assert_eq!(
            build_open_folder_args(spec("vscode"), &p("/work/proj"), false),
            vec!["/work/proj"]
        );
        assert_eq!(
            build_open_folder_args(spec("vscode"), &p("/work/proj"), true),
            vec!["-a", "/work/proj"]
        );
        assert_eq!(
            build_open_folder_args(spec("zed"), &p("/work/proj"), true),
            vec!["--add", "/work/proj"]
        );
        // Xcode はフォルダに -p が要る。追加フラグは無いので add は無視される。
        assert_eq!(
            build_open_folder_args(spec("xcode"), &p("/work/proj"), true),
            vec!["-p", "/work/proj"]
        );
    }

    #[test]
    fn url_scheme_fallback() {
        assert_eq!(
            url_for(spec("vscode"), &p("/tmp/a.rs"), 4, 2).unwrap(),
            "vscode://file/tmp/a.rs:4:2"
        );
        assert_eq!(
            url_for(spec("vscode-insiders"), &p("/tmp/a.rs"), 4, 2).unwrap(),
            "vscode-insiders://file/tmp/a.rs:4:2"
        );
        assert_eq!(
            url_for(spec("antigravity"), &p("/tmp/a.rs"), 4, 2).unwrap(),
            "antigravity://file/tmp/a.rs:4:2"
        );
        // スキームを持たない IDE は None
        assert!(url_for(spec("sublime"), &p("/tmp/a.rs"), 1, 1).is_none());
        assert!(url_for(spec("intellij"), &p("/tmp/a.rs"), 1, 1).is_none());
    }

    #[test]
    fn url_percent_encodes_spaces() {
        let url = url_for(spec("cursor"), &p("/Users/me/My Projects/a b.rs"), 1, 1).unwrap();
        assert_eq!(url, "cursor://file/Users/me/My%20Projects/a%20b.rs:1:1");
        assert!(!url.contains(' '));
    }

    #[test]
    fn every_file_arg_style_is_covered_by_catalog() {
        use FileArgStyle::*;
        for style in [
            GotoColon,
            GotoSpaceColon,
            GotoEquals,
            BareColon,
            LineFlags,
            LineOnly,
            PlusLine,
            PlusLineCol,
        ] {
            assert!(
                CATALOG.iter().any(|s| s.file_arg == style),
                "{style:?} を使う IDE がカタログに無い"
            );
        }
    }

    #[test]
    fn catalog_keys_are_unique() {
        let mut keys: Vec<&str> = CATALOG.iter().map(|s| s.key).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "key が重複している");
    }

    #[test]
    fn cursor_shim_named_code_is_rejected_as_vscode() {
        // このマシンの実測: /usr/local/bin/code の実体は
        // /Applications/Cursor.app/Contents/Resources/app/bin/code
        let canon = "/applications/cursor.app/contents/resources/app/bin/code";
        assert_eq!(
            identify_by_path(spec("vscode"), canon),
            Some(false),
            "Cursor のシムを VS Code と誤検出してはいけない"
        );
        assert_eq!(identify_by_path(spec("cursor"), canon), Some(true));
    }

    #[test]
    fn kiro_shim_is_also_named_code_internally() {
        // Kiro のシムも内部ファイル名は code だが、バンドルは Kiro.app。
        let canon = "/applications/kiro.app/contents/resources/app/bin/code";
        assert_eq!(identify_by_path(spec("kiro"), canon), Some(true));
        assert_eq!(identify_by_path(spec("vscode"), canon), Some(false));
    }

    #[test]
    fn real_vscode_path_is_accepted() {
        let canon = "/applications/visual studio code.app/contents/resources/app/bin/code";
        assert_eq!(identify_by_path(spec("vscode"), canon), Some(true));
        assert_eq!(identify_by_path(spec("cursor"), canon), Some(false));
    }

    #[test]
    fn unknown_path_is_inconclusive_not_a_rejection() {
        // Linux の素朴な配置などでマーカーが当たらない場合は --version へ回す。
        assert_eq!(identify_by_path(spec("fleet"), "/opt/custom/bin/myeditor"), None);
    }
}
