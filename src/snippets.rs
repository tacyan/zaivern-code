//! VS Code 互換スニペットエンジン + Emmet 略記展開。
//!
//! * スニペット JSON (`{"名前": {"prefix": …, "body": […], "description": …}}`)
//!   の読み込み (JSONC 許容)、`~/.zaivern/snippets/` のユーザースニペット、
//!   組み込みスニペットのマージ。
//! * テンプレート展開 — `$1` / `${1:既定値}` / `${1|a,b,c|}` / `$0` /
//!   `${TM_FILENAME}` などの変数 / ネスト / ミラー / `\$` エスケープ。
//!   展開結果は「本文 + タブストップ (ミラーを含む複数レンジ)」を返す。
//! * Emmet 略記 — `div.cls#id` / `ul>li*3` / `a[href=#]{text}` / `+` / `^` /
//!   `$` 連番 / 暗黙タグ / 空要素。解釈できなければ **None** を返し、
//!   呼び出し側は通常の Tab 動作へ落とす (バッファを壊さない)。
//!
//! 言語ごとの知識 (Emmet の可否・コメント記法・既定インデント・継承する
//! スニペット集合) は **データ表** `LANGS` に集約する。判定を各所へ散らさない。
//! 時刻は `Clock` として呼び出し側から注入する — 純粋関数のままに保ち、
//! テストを決定的にするため、この中で現在時刻を読むことはしない。

// UI 配線 (app.rs 側) から順次呼ばれる公開 API 群。bin クレートなので
// 未配線の pub は dead_code 警告になるが、意図した公開面なので許可する。
#![allow(dead_code)]

use crate::i18n::trf;
use crate::jsonc::strip_jsonc;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// ===========================================================================
// 言語データ表
// ===========================================================================

/// Emmet の方言。`None` の言語では略記展開を試みない。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EmmetKind {
    /// Emmet 無効。
    None,
    /// HTML/XML 系のタグ略記。
    Markup,
    /// CSS 系のプロパティ略記。
    Style,
}

/// 言語ごとの展開規則。`LANGS` の 1 行 = 1 言語。
#[derive(Clone, Copy)]
pub struct LangSpec {
    /// VS Code の language id。
    pub id: &'static str,
    pub emmet: EmmetKind,
    /// 空要素を XML 流儀 (`<br />`) で閉じるか。HTML は false (`<br>`)。
    pub xml_close: bool,
    /// 行コメント記号 (無い言語は空文字)。
    pub line_comment: &'static str,
    /// ブロックコメント (開始, 終了)。無い言語は空文字。
    pub block_comment: (&'static str, &'static str),
    /// 既定インデント (幅, タブか)。
    pub indent: (usize, bool),
    /// スニペットを継承する言語 ID。tsx → ts → js のように連鎖する。
    pub inherits: &'static [&'static str],
}

/// 未知の言語に使うフォールバック (プレーンテキスト相当)。
static FALLBACK_SPEC: LangSpec = LangSpec {
    id: "plaintext",
    emmet: EmmetKind::None,
    xml_close: false,
    line_comment: "",
    block_comment: ("", ""),
    indent: (4, false),
    inherits: &[],
};

/// 言語データ表 — Emmet 可否・コメント記法・既定インデント・スニペット継承。
pub static LANGS: &[LangSpec] = &[
    LangSpec {
        id: "rust",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (4, false),
        inherits: &[],
    },
    LangSpec {
        id: "c",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (4, false),
        inherits: &[],
    },
    LangSpec {
        id: "cpp",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (4, false),
        inherits: &["c"],
    },
    LangSpec {
        id: "csharp",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (4, false),
        inherits: &[],
    },
    LangSpec {
        id: "objective-c",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (4, false),
        inherits: &["c"],
    },
    LangSpec {
        id: "objective-cpp",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (4, false),
        inherits: &["objective-c", "c"],
    },
    LangSpec {
        id: "go",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (1, true),
        inherits: &[],
    },
    LangSpec {
        id: "java",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (4, false),
        inherits: &[],
    },
    LangSpec {
        id: "kotlin",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (4, false),
        inherits: &[],
    },
    LangSpec {
        id: "swift",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (4, false),
        inherits: &[],
    },
    LangSpec {
        id: "dart",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "scala",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "javascript",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "typescript",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (2, false),
        inherits: &["javascript"],
    },
    LangSpec {
        id: "javascriptreact",
        emmet: EmmetKind::Markup,
        xml_close: true,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (2, false),
        inherits: &["javascript"],
    },
    LangSpec {
        id: "typescriptreact",
        emmet: EmmetKind::Markup,
        xml_close: true,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (2, false),
        inherits: &["typescript", "javascript"],
    },
    LangSpec {
        id: "vue",
        emmet: EmmetKind::Markup,
        xml_close: false,
        line_comment: "//",
        block_comment: ("<!--", "-->"),
        indent: (2, false),
        inherits: &["html", "javascript"],
    },
    LangSpec {
        id: "svelte",
        emmet: EmmetKind::Markup,
        xml_close: false,
        line_comment: "//",
        block_comment: ("<!--", "-->"),
        indent: (2, false),
        inherits: &["html", "javascript"],
    },
    LangSpec {
        id: "html",
        emmet: EmmetKind::Markup,
        xml_close: false,
        line_comment: "",
        block_comment: ("<!--", "-->"),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "xml",
        emmet: EmmetKind::Markup,
        xml_close: true,
        line_comment: "",
        block_comment: ("<!--", "-->"),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "php",
        emmet: EmmetKind::Markup,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (4, false),
        inherits: &["html"],
    },
    LangSpec {
        id: "handlebars",
        emmet: EmmetKind::Markup,
        xml_close: false,
        line_comment: "",
        block_comment: ("{{!--", "--}}"),
        indent: (2, false),
        inherits: &["html"],
    },
    LangSpec {
        id: "astro",
        emmet: EmmetKind::Markup,
        xml_close: false,
        line_comment: "//",
        block_comment: ("<!--", "-->"),
        indent: (2, false),
        inherits: &["html", "typescript"],
    },
    LangSpec {
        id: "css",
        emmet: EmmetKind::Style,
        xml_close: false,
        line_comment: "",
        block_comment: ("/*", "*/"),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "scss",
        emmet: EmmetKind::Style,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (2, false),
        inherits: &["css"],
    },
    LangSpec {
        id: "sass",
        emmet: EmmetKind::Style,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (2, false),
        inherits: &["css"],
    },
    LangSpec {
        id: "less",
        emmet: EmmetKind::Style,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (2, false),
        inherits: &["css"],
    },
    LangSpec {
        id: "python",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "#",
        block_comment: ("\"\"\"", "\"\"\""),
        indent: (4, false),
        inherits: &[],
    },
    LangSpec {
        id: "ruby",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "#",
        block_comment: ("=begin", "=end"),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "perl",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "#",
        block_comment: ("", ""),
        indent: (4, false),
        inherits: &[],
    },
    LangSpec {
        id: "lua",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "--",
        block_comment: ("--[[", "]]"),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "r",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "#",
        block_comment: ("", ""),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "haskell",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "--",
        block_comment: ("{-", "-}"),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "elixir",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "#",
        block_comment: ("", ""),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "erlang",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "%",
        block_comment: ("", ""),
        indent: (4, false),
        inherits: &[],
    },
    LangSpec {
        id: "shellscript",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "#",
        block_comment: ("", ""),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "bat",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "REM",
        block_comment: ("", ""),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "powershell",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "#",
        block_comment: ("<#", "#>"),
        indent: (4, false),
        inherits: &[],
    },
    LangSpec {
        id: "makefile",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "#",
        block_comment: ("", ""),
        indent: (1, true),
        inherits: &[],
    },
    LangSpec {
        id: "dockerfile",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "#",
        block_comment: ("", ""),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "sql",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "--",
        block_comment: ("/*", "*/"),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "yaml",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "#",
        block_comment: ("", ""),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "toml",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "#",
        block_comment: ("", ""),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "json",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "markdown",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "",
        block_comment: ("<!--", "-->"),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "latex",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "%",
        block_comment: ("", ""),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "dot",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "//",
        block_comment: ("/*", "*/"),
        indent: (2, false),
        inherits: &[],
    },
    LangSpec {
        id: "plaintext",
        emmet: EmmetKind::None,
        xml_close: false,
        line_comment: "",
        block_comment: ("", ""),
        indent: (4, false),
        inherits: &[],
    },
];

/// 言語 ID から規則を引く。未知の ID はプレーンテキスト相当へフォールバック。
pub fn lang_spec(lang_id: &str) -> &'static LangSpec {
    LANGS
        .iter()
        .find(|l| l.id == lang_id)
        .unwrap_or(&FALLBACK_SPEC)
}

/// その言語で Emmet を試すか (データ表が唯一の判断材料)。
pub fn emmet_kind(lang_id: &str) -> EmmetKind {
    lang_spec(lang_id).emmet
}

/// 既定インデント 1 段の文字列 ("\t" か空白 N 個)。
pub fn default_indent(lang_id: &str) -> String {
    let (w, tabs) = lang_spec(lang_id).indent;
    if tabs {
        "\t".repeat(w.max(1))
    } else {
        " ".repeat(w.max(1))
    }
}

/// その言語へ適用するスニペット集合の言語 ID を優先順に並べる。
/// 例: typescriptreact → ["typescriptreact", "typescript", "javascript"]。
pub fn snippet_langs(lang_id: &str) -> Vec<String> {
    let mut out = vec![lang_id.to_string()];
    let mut seen: HashSet<String> = out.iter().cloned().collect();
    let mut queue: Vec<&'static str> = lang_spec(lang_id).inherits.to_vec();
    while let Some(next) = queue.first().copied() {
        queue.remove(0);
        if seen.insert(next.to_string()) {
            out.push(next.to_string());
            for p in lang_spec(next).inherits {
                queue.push(p);
            }
        }
    }
    out
}

/// 拡張子 → 言語 ID のデータ表 (ファイル名だけから言語を決めるとき用)。
static EXT_LANG: &[(&str, &str)] = &[
    ("rs", "rust"),
    ("c", "c"),
    ("h", "c"),
    ("cc", "cpp"),
    ("cpp", "cpp"),
    ("cxx", "cpp"),
    ("hpp", "cpp"),
    ("cs", "csharp"),
    ("m", "objective-c"),
    ("mm", "objective-cpp"),
    ("go", "go"),
    ("java", "java"),
    ("kt", "kotlin"),
    ("kts", "kotlin"),
    ("swift", "swift"),
    ("dart", "dart"),
    ("scala", "scala"),
    ("js", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("jsx", "javascriptreact"),
    ("ts", "typescript"),
    ("tsx", "typescriptreact"),
    ("vue", "vue"),
    ("svelte", "svelte"),
    ("html", "html"),
    ("htm", "html"),
    ("xhtml", "html"),
    ("xml", "xml"),
    ("svg", "xml"),
    ("php", "php"),
    ("hbs", "handlebars"),
    ("astro", "astro"),
    ("css", "css"),
    ("scss", "scss"),
    ("sass", "sass"),
    ("less", "less"),
    ("py", "python"),
    ("rb", "ruby"),
    ("pl", "perl"),
    ("lua", "lua"),
    ("r", "r"),
    ("hs", "haskell"),
    ("ex", "elixir"),
    ("exs", "elixir"),
    ("erl", "erlang"),
    ("sh", "shellscript"),
    ("bash", "shellscript"),
    ("zsh", "shellscript"),
    ("bat", "bat"),
    ("cmd", "bat"),
    ("ps1", "powershell"),
    ("sql", "sql"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("toml", "toml"),
    ("json", "json"),
    ("md", "markdown"),
    ("markdown", "markdown"),
    ("tex", "latex"),
    ("dot", "dot"),
    ("txt", "plaintext"),
];

/// ファイル名/パスの拡張子から言語 ID を推定する。
pub fn lang_id_for_path(path: &str) -> &'static str {
    let name = Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name == "makefile" {
        return "makefile";
    }
    if name.starts_with("dockerfile") {
        return "dockerfile";
    }
    let ext = match name.rsplit_once('.') {
        Some((_, e)) => e.to_string(),
        None => return "plaintext",
    };
    EXT_LANG
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, l)| *l)
        .unwrap_or("plaintext")
}

// ===========================================================================
// スニペット本体と JSON 読み込み
// ===========================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    pub name: String,
    pub prefix: String,
    pub body: String,
    pub description: String,
    /// 言語 ID。`*` = 全言語。VS Code の `scope` はカンマ区切りで入る。
    pub language: String,
}

fn json_str_or_join(v: &serde_json::Value, sep: &str) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(a) => Some(
            a.iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(sep),
        ),
        _ => None,
    }
}

/// スニペット JSON 文字列を解析する (JSONC 許容)。`default_lang` は
/// ファイル名から決めた言語 ID で、エントリに `scope` があればそちらが勝つ。
/// 解析できないときは人が読めるエラー文字列を返す (panic しない)。
pub fn parse_str(src: &str, default_lang: &str) -> Result<Vec<Snippet>, String> {
    let clean = strip_jsonc(src);
    let val: serde_json::Value =
        serde_json::from_str(&clean).map_err(|e| format!("JSON 構文エラー: {e}"))?;
    let obj = val
        .as_object()
        .ok_or_else(|| "最上位がオブジェクトではありません".to_string())?;
    let mut result = Vec::new();
    for (name, entry) in obj {
        let e = match entry.as_object() {
            Some(e) => e,
            None => continue,
        };
        // prefix: 文字列 or 文字列配列 (先頭が採用される)
        let prefix = match e.get("prefix") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str())
                .next()
                .unwrap_or("")
                .to_string(),
            _ => continue,
        };
        if prefix.is_empty() {
            continue;
        }
        // body: 文字列 or 行配列 (\n 連結)
        let body = match e.get("body").and_then(|v| json_str_or_join(v, "\n")) {
            Some(b) => b,
            None => continue,
        };
        let description = e
            .get("description")
            .and_then(|v| json_str_or_join(v, " "))
            .unwrap_or_default();
        // scope があれば言語を上書き (未知フィールドはすべて無視する)
        let language = e
            .get("scope")
            .and_then(|v| json_str_or_join(v, ","))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| default_lang.to_string());
        result.push(Snippet {
            name: name.clone(),
            prefix,
            body,
            description,
            language,
        });
    }
    Ok(result)
}

/// 1 ファイルを読み込む。読めない/壊れている場合は理由付きの Err。
pub fn parse_file_checked(path: &Path, language: &str) -> Result<Vec<Snippet>, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("読み込めません: {e}"))?;
    parse_str(&src, language)
}

/// 1 ファイルを読み込む (エラーは黙って空扱い。プラグイン読み込み経路互換)。
pub fn parse_file(path: &Path, language: &str) -> Vec<Snippet> {
    parse_file_checked(path, language).unwrap_or_default()
}

// ===========================================================================
// 展開コンテキスト (時刻は注入 — 純粋関数を保つ)
// ===========================================================================

/// 呼び出し側が渡す時刻。`Clock::from_unix` で UNIX 秒から組み立てる。
/// この中で現在時刻を読まないので、テストは完全に決定的になる。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Clock {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 0 = 日曜。
    pub weekday: u32,
    pub unix: i64,
}

impl Clock {
    /// UNIX 秒 + 秒単位のオフセット (ローカル時刻にしたい場合) から組む。
    /// 暦の計算は Howard Hinnant の civil_from_days 相当。
    pub fn from_unix_offset(unix: i64, offset_secs: i64) -> Clock {
        let t = unix + offset_secs;
        let days = t.div_euclid(86_400);
        let rem = t.rem_euclid(86_400);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        Clock {
            year: (y + if m <= 2 { 1 } else { 0 }) as i32,
            month: m as u32,
            day: d as u32,
            hour: (rem / 3_600) as u32,
            minute: ((rem % 3_600) / 60) as u32,
            second: (rem % 60) as u32,
            weekday: (days + 4).rem_euclid(7) as u32,
            unix,
        }
    }

    /// UTC として組み立てる。
    pub fn from_unix(unix: i64) -> Clock {
        Clock::from_unix_offset(unix, 0)
    }
}

static MONTH_NAMES: &[(&str, &str)] = &[
    ("January", "Jan"),
    ("February", "Feb"),
    ("March", "Mar"),
    ("April", "Apr"),
    ("May", "May"),
    ("June", "Jun"),
    ("July", "Jul"),
    ("August", "Aug"),
    ("September", "Sep"),
    ("October", "Oct"),
    ("November", "Nov"),
    ("December", "Dec"),
];

static DAY_NAMES: &[(&str, &str)] = &[
    ("Sunday", "Sun"),
    ("Monday", "Mon"),
    ("Tuesday", "Tue"),
    ("Wednesday", "Wed"),
    ("Thursday", "Thu"),
    ("Friday", "Fri"),
    ("Saturday", "Sat"),
];

/// スニペット変数を解決するための入力一式。すべて呼び出し側が用意する。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExpandCtx {
    /// 編集中ファイルのパス (無ければ空)。
    pub file_path: String,
    /// ワークスペースのルートパス。
    pub workspace_root: String,
    /// 言語 ID (コメント記号の解決に使う)。
    pub language: String,
    /// キャレット行 (1 始まり)。
    pub line_number: usize,
    /// 選択テキスト。
    pub selected_text: String,
    /// キャレット行の全文。
    pub current_line: String,
    /// キャレット直前の単語。
    pub current_word: String,
    /// クリップボード内容。
    pub clipboard: String,
    /// インデント 1 段。空なら言語データ表の既定値。
    pub indent: String,
    /// 現在時刻 (未指定なら日付系変数は空文字になる)。
    pub clock: Option<Clock>,
    /// RANDOM / UUID の種。未指定なら空文字。
    pub random_seed: Option<u64>,
}

impl ExpandCtx {
    /// パスから最低限の文脈を組む (言語は拡張子から推定)。
    pub fn for_path(file_path: &str) -> ExpandCtx {
        let language = lang_id_for_path(file_path).to_string();
        ExpandCtx {
            indent: default_indent(&language),
            file_path: file_path.to_string(),
            language,
            line_number: 1,
            ..Default::default()
        }
    }
    pub fn with_language(mut self, lang: &str) -> Self {
        self.language = lang.to_string();
        if self.indent.is_empty() {
            self.indent = default_indent(lang);
        }
        self
    }
    pub fn with_clock(mut self, c: Clock) -> Self {
        self.clock = Some(c);
        self
    }
    pub fn with_selection(mut self, s: &str) -> Self {
        self.selected_text = s.to_string();
        self
    }
    pub fn with_line_number(mut self, n: usize) -> Self {
        self.line_number = n;
        self
    }
    pub fn with_indent(mut self, s: &str) -> Self {
        self.indent = s.to_string();
        self
    }
    pub fn with_workspace(mut self, root: &str) -> Self {
        self.workspace_root = root.to_string();
        self
    }
    pub fn with_random_seed(mut self, seed: u64) -> Self {
        self.random_seed = Some(seed);
        self
    }
    fn indent_unit(&self) -> String {
        if self.indent.is_empty() {
            default_indent(&self.language)
        } else {
            self.indent.clone()
        }
    }
}

/// 種から決定的な擬似乱数 (xorshift64*) を作る。時計と同様、注入前提。
fn rand_from(seed: u64, salt: u64) -> u64 {
    let mut x = seed ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// 変数名を解決する。`None` = 未知の変数 (VS Code は名前をそのまま出さず
/// 空にするが、既定値 `${VAR:既定}` を活かすため None と空文字を区別する)。
fn resolve_var(name: &str, ctx: &ExpandCtx) -> Option<String> {
    let path = Path::new(&ctx.file_path);
    let base = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let spec = lang_spec(&ctx.language);
    let clock = ctx.clock;
    let two = |v: u32| format!("{v:02}");
    match name {
        "TM_FILENAME" => Some(base),
        "TM_FILENAME_BASE" => Some(match base.rfind('.') {
            Some(0) | None => base,
            Some(i) => base[..i].to_string(),
        }),
        "TM_FILEPATH" => Some(ctx.file_path.clone()),
        "TM_DIRECTORY" => Some(
            path.parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        "RELATIVE_FILEPATH" => Some(
            ctx.file_path
                .strip_prefix(&ctx.workspace_root)
                .map(|s| s.trim_start_matches(['/', '\\']).to_string())
                .unwrap_or_else(|| ctx.file_path.clone()),
        ),
        "WORKSPACE_FOLDER" => Some(ctx.workspace_root.clone()),
        "WORKSPACE_NAME" => Some(
            Path::new(&ctx.workspace_root)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        "TM_LINE_INDEX" => Some(ctx.line_number.saturating_sub(1).to_string()),
        "TM_LINE_NUMBER" => Some(ctx.line_number.max(1).to_string()),
        "TM_SELECTED_TEXT" => Some(ctx.selected_text.clone()),
        "TM_CURRENT_LINE" => Some(ctx.current_line.clone()),
        "TM_CURRENT_WORD" => Some(ctx.current_word.clone()),
        "CLIPBOARD" => Some(ctx.clipboard.clone()),
        "LINE_COMMENT" => Some(spec.line_comment.to_string()),
        "BLOCK_COMMENT_START" => Some(spec.block_comment.0.to_string()),
        "BLOCK_COMMENT_END" => Some(spec.block_comment.1.to_string()),
        "CURRENT_YEAR" => Some(clock.map(|c| c.year.to_string()).unwrap_or_default()),
        "CURRENT_YEAR_SHORT" => Some(
            clock
                .map(|c| two((c.year.rem_euclid(100)) as u32))
                .unwrap_or_default(),
        ),
        "CURRENT_MONTH" => Some(clock.map(|c| two(c.month)).unwrap_or_default()),
        "CURRENT_MONTH_NAME" => Some(
            clock
                .and_then(|c| MONTH_NAMES.get(c.month.saturating_sub(1) as usize))
                .map(|m| m.0.to_string())
                .unwrap_or_default(),
        ),
        "CURRENT_MONTH_NAME_SHORT" => Some(
            clock
                .and_then(|c| MONTH_NAMES.get(c.month.saturating_sub(1) as usize))
                .map(|m| m.1.to_string())
                .unwrap_or_default(),
        ),
        "CURRENT_DATE" => Some(clock.map(|c| two(c.day)).unwrap_or_default()),
        "CURRENT_DAY_NAME" => Some(
            clock
                .and_then(|c| DAY_NAMES.get(c.weekday as usize))
                .map(|d| d.0.to_string())
                .unwrap_or_default(),
        ),
        "CURRENT_DAY_NAME_SHORT" => Some(
            clock
                .and_then(|c| DAY_NAMES.get(c.weekday as usize))
                .map(|d| d.1.to_string())
                .unwrap_or_default(),
        ),
        "CURRENT_HOUR" => Some(clock.map(|c| two(c.hour)).unwrap_or_default()),
        "CURRENT_MINUTE" => Some(clock.map(|c| two(c.minute)).unwrap_or_default()),
        "CURRENT_SECOND" => Some(clock.map(|c| two(c.second)).unwrap_or_default()),
        "CURRENT_SECONDS_UNIX" => Some(clock.map(|c| c.unix.to_string()).unwrap_or_default()),
        "CURRENT_TIMEZONE_OFFSET" => Some(String::new()),
        "RANDOM" => Some(
            ctx.random_seed
                .map(|s| format!("{:06}", rand_from(s, 1) % 1_000_000))
                .unwrap_or_default(),
        ),
        "RANDOM_HEX" => Some(
            ctx.random_seed
                .map(|s| format!("{:06x}", rand_from(s, 2) & 0xFF_FFFF))
                .unwrap_or_default(),
        ),
        "UUID" => Some(
            ctx.random_seed
                .map(|s| {
                    let a = rand_from(s, 3);
                    let b = rand_from(s, 4);
                    format!(
                        "{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
                        a as u32,
                        (a >> 32) as u16,
                        (a >> 48) & 0xFFF,
                        b & 0xFFF,
                        (b >> 12) & 0xFFFF_FFFF_FFFF
                    )
                })
                .unwrap_or_default(),
        ),
        _ => None,
    }
}

// ===========================================================================
// テンプレート構文解析 (VS Code 互換)
// ===========================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    Text(String),
    /// `$1` / `${1}` / `${1:既定}` / `${1|a,b|}` — children と choices は排他。
    Stop {
        index: u32,
        children: Vec<Node>,
        choices: Vec<String>,
    },
    /// `$VAR` / `${VAR}` / `${VAR:既定}`
    Var {
        name: String,
        default: Vec<Node>,
    },
}

/// ネストの上限。壊れた入力でスタックを食い潰さないための保険。
const MAX_DEPTH: usize = 32;

fn is_var_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `${…}` 内の未エスケープ `}` まで読み飛ばす (入れ子ブレース対応)。
fn skip_brace(cs: &[char], mut i: usize) -> usize {
    let mut depth = 0usize;
    while i < cs.len() {
        let c = cs[i];
        if c == '\\' && i + 1 < cs.len() {
            i += 2;
            continue;
        }
        if c == '{' {
            depth += 1;
        } else if c == '}' {
            if depth == 0 {
                return i + 1;
            }
            depth -= 1;
        }
        i += 1;
    }
    i
}

fn push_text(out: &mut Vec<Node>, s: &str) {
    if s.is_empty() {
        return;
    }
    if let Some(Node::Text(t)) = out.last_mut() {
        t.push_str(s);
    } else {
        out.push(Node::Text(s.to_string()));
    }
}

/// `i` から 1 セグメントを解析する。`stop_at_brace` なら現在の `${…}` を
/// 閉じる未エスケープ `}` を消費して戻る。警告は `warn` へ積む。
fn parse_nodes(
    cs: &[char],
    i: &mut usize,
    stop_at_brace: bool,
    depth: usize,
    warn: &mut Vec<String>,
) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::new();
    while *i < cs.len() {
        let c = cs[*i];
        if c == '\\' && *i + 1 < cs.len() {
            let n = cs[*i + 1];
            if n == '$' || n == '\\' || n == '}' {
                push_text(&mut out, &n.to_string());
                *i += 2;
                continue;
            }
        }
        if c == '}' && stop_at_brace {
            *i += 1;
            return out;
        }
        if c == '$' && *i + 1 < cs.len() {
            if depth >= MAX_DEPTH {
                warn.push("スニペットのネストが深すぎます".to_string());
                push_text(&mut out, "$");
                *i += 1;
                continue;
            }
            parse_dollar(cs, i, &mut out, depth, warn);
            continue;
        }
        push_text(&mut out, &c.to_string());
        *i += 1;
    }
    if stop_at_brace {
        warn.push("`${` が閉じられていません".to_string());
    }
    out
}

fn read_number(cs: &[char], i: &mut usize) -> u32 {
    let mut n: u32 = 0;
    while *i < cs.len() && cs[*i].is_ascii_digit() {
        n = n
            .saturating_mul(10)
            .saturating_add(cs[*i] as u32 - '0' as u32);
        *i += 1;
    }
    n
}

/// `$…` を解析する (`i` は `$` を指している)。
fn parse_dollar(
    cs: &[char],
    i: &mut usize,
    out: &mut Vec<Node>,
    depth: usize,
    warn: &mut Vec<String>,
) {
    let start = *i;
    let next = cs[start + 1];
    // $1 / $12 / $0
    if next.is_ascii_digit() {
        *i = start + 1;
        let index = read_number(cs, i);
        out.push(Node::Stop {
            index,
            children: Vec::new(),
            choices: Vec::new(),
        });
        return;
    }
    // $TM_FILENAME など
    if is_var_char(next) {
        let mut j = start + 1;
        while j < cs.len() && is_var_char(cs[j]) {
            j += 1;
        }
        let name: String = cs[start + 1..j].iter().collect();
        *i = j;
        out.push(Node::Var {
            name,
            default: Vec::new(),
        });
        return;
    }
    if next != '{' {
        push_text(out, "$");
        *i = start + 1;
        return;
    }
    // ${…}
    let mut j = start + 2;
    if j >= cs.len() {
        warn.push("`${` が閉じられていません".to_string());
        push_text(out, "${");
        *i = j;
        return;
    }
    if cs[j].is_ascii_digit() {
        let index = read_number(cs, &mut j);
        if j >= cs.len() {
            warn.push("`${` が閉じられていません".to_string());
            out.push(Node::Stop {
                index,
                children: Vec::new(),
                choices: Vec::new(),
            });
            *i = j;
            return;
        }
        match cs[j] {
            '}' => {
                out.push(Node::Stop {
                    index,
                    children: Vec::new(),
                    choices: Vec::new(),
                });
                *i = j + 1;
            }
            ':' => {
                let mut k = j + 1;
                let children = parse_nodes(cs, &mut k, true, depth + 1, warn);
                out.push(Node::Stop {
                    index,
                    children,
                    choices: Vec::new(),
                });
                *i = k;
            }
            '|' => {
                let mut k = j + 1;
                let mut choices: Vec<String> = Vec::new();
                let mut cur = String::new();
                let mut closed = false;
                while k < cs.len() {
                    let c = cs[k];
                    if c == '\\' && k + 1 < cs.len() {
                        cur.push(cs[k + 1]);
                        k += 2;
                        continue;
                    }
                    if c == ',' {
                        choices.push(std::mem::take(&mut cur));
                        k += 1;
                        continue;
                    }
                    if c == '|' {
                        closed = true;
                        k += 1;
                        break;
                    }
                    cur.push(c);
                    k += 1;
                }
                choices.push(cur);
                if !closed {
                    warn.push("選択肢 `${n|…|}` が閉じられていません".to_string());
                }
                if k < cs.len() && cs[k] == '}' {
                    k += 1;
                } else if closed {
                    warn.push("選択肢の後に `}` がありません".to_string());
                }
                out.push(Node::Stop {
                    index,
                    children: Vec::new(),
                    choices,
                });
                *i = k;
            }
            '/' => {
                // ${1/正規表現/置換/} — 変換は非対応。素のタブストップ扱い。
                warn.push("`${n/正規表現/置換/}` の変換は未対応です".to_string());
                out.push(Node::Stop {
                    index,
                    children: Vec::new(),
                    choices: Vec::new(),
                });
                *i = skip_brace(cs, j + 1);
            }
            _ => {
                warn.push("`${n…}` の書式が不正です".to_string());
                push_text(out, "${");
                *i = j;
            }
        }
        return;
    }
    if is_var_char(cs[j]) {
        let name_start = j;
        while j < cs.len() && is_var_char(cs[j]) {
            j += 1;
        }
        let name: String = cs[name_start..j].iter().collect();
        if j >= cs.len() {
            warn.push("`${` が閉じられていません".to_string());
            out.push(Node::Var {
                name,
                default: Vec::new(),
            });
            *i = j;
            return;
        }
        match cs[j] {
            '}' => {
                out.push(Node::Var {
                    name,
                    default: Vec::new(),
                });
                *i = j + 1;
            }
            ':' => {
                let mut k = j + 1;
                let default = parse_nodes(cs, &mut k, true, depth + 1, warn);
                out.push(Node::Var { name, default });
                *i = k;
            }
            '/' => {
                warn.push("`${VAR/正規表現/置換/}` の変換は未対応です".to_string());
                out.push(Node::Var {
                    name,
                    default: Vec::new(),
                });
                *i = skip_brace(cs, j + 1);
            }
            _ => {
                warn.push("`${VAR…}` の書式が不正です".to_string());
                push_text(out, "${");
                *i = name_start;
            }
        }
        return;
    }
    // `${` の直後が数字でも変数名でもない — 書式不正。素の文字として残す。
    warn.push("`${…}` の中身が不正です".to_string());
    push_text(out, "$");
    *i = start + 1;
}

// ===========================================================================
// 展開結果モデル
// ===========================================================================

/// タブストップ 1 つ。同じ添字が複数出た場合はミラーとして `ranges` に並ぶ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabStop {
    pub index: u32,
    /// (開始, 終了) の char 位置。長さ 0 = キャレットのみ。
    pub ranges: Vec<(usize, usize)>,
}

impl TabStop {
    /// 先頭レンジ (UI が最初に選択すべき範囲)。
    pub fn first(&self) -> (usize, usize) {
        self.ranges.first().copied().unwrap_or((0, 0))
    }
}

/// 展開結果。`stops` は「$0 最後・それ以外は昇順」に整列済み。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Expansion {
    pub text: String,
    pub stops: Vec<TabStop>,
}

impl Expansion {
    /// 最初にキャレットを置くべき char 位置。
    pub fn cursor(&self) -> usize {
        self.stops
            .first()
            .map(|s| s.first().0)
            .unwrap_or_else(|| self.text.chars().count())
    }
    /// すべてのレンジを `offset` char ずらす (挿入位置へ写す)。
    pub fn shifted(mut self, offset: usize) -> Expansion {
        for s in &mut self.stops {
            for r in &mut s.ranges {
                r.0 += offset;
                r.1 += offset;
            }
        }
        self
    }
    /// `caret` より後ろにある最初のタブストップ (Tab での前進)。
    pub fn next_stop(&self, caret: usize) -> Option<&TabStop> {
        self.stops.iter().find(|s| s.first().0 > caret)
    }
    /// `caret` より前にある最後のタブストップ (Shift+Tab での後退)。
    pub fn prev_stop(&self, caret: usize) -> Option<&TabStop> {
        self.stops.iter().rev().find(|s| s.first().0 < caret)
    }
}

struct Render<'a> {
    out: String,
    len: usize,
    ctx: &'a ExpandCtx,
    values: &'a HashMap<u32, String>,
    order: Vec<u32>,
    ranges: HashMap<u32, Vec<(usize, usize)>>,
}

impl<'a> Render<'a> {
    fn new(ctx: &'a ExpandCtx, values: &'a HashMap<u32, String>) -> Self {
        Render {
            out: String::new(),
            len: 0,
            ctx,
            values,
            order: Vec::new(),
            ranges: HashMap::new(),
        }
    }
    fn push_str(&mut self, s: &str) {
        self.out.push_str(s);
        self.len += s.chars().count();
    }
    fn run(&mut self, nodes: &[Node]) {
        for n in nodes {
            match n {
                Node::Text(s) => self.push_str(s),
                Node::Var { name, default } => match resolve_var(name, self.ctx) {
                    Some(v) if !v.is_empty() => self.push_str(&v),
                    _ => self.run(default),
                },
                Node::Stop {
                    index,
                    children,
                    choices,
                } => {
                    let start = self.len;
                    let first_time = !self.ranges.contains_key(index);
                    if first_time && !children.is_empty() {
                        self.run(children);
                    } else if first_time && !choices.is_empty() {
                        let c = choices[0].clone();
                        self.push_str(&c);
                    } else if let Some(v) = self.values.get(index).cloned() {
                        // ミラー: 同じ添字の最初の定義と必ず同じ文字列にする
                        self.push_str(&v);
                    }
                    let end = self.len;
                    if first_time {
                        self.order.push(*index);
                    }
                    self.ranges.entry(*index).or_default().push((start, end));
                }
            }
        }
    }
}

/// ミラー用に「各添字の最初の定義が生む文字列」を先に決める。
/// `$1 … ${1:foo}` のように参照が先行しても同期させるための前段。
fn collect_values(nodes: &[Node], ctx: &ExpandCtx, values: &mut HashMap<u32, String>) {
    for n in nodes {
        match n {
            Node::Text(_) => {}
            Node::Var { default, .. } => collect_values(default, ctx, values),
            Node::Stop {
                index,
                children,
                choices,
            } => {
                if !children.is_empty() {
                    collect_values(children, ctx, values);
                    if !values.contains_key(index) {
                        let snapshot = values.clone();
                        let mut r = Render::new(ctx, &snapshot);
                        r.run(children);
                        if !r.out.is_empty() {
                            values.insert(*index, r.out);
                        }
                    }
                } else if let Some(c) = choices.first() {
                    values.entry(*index).or_insert_with(|| c.clone());
                }
            }
        }
    }
}

/// スニペット本文を展開する **純粋関数**。時刻・ファイル名・選択テキストは
/// すべて `ctx` から取るので、同じ入力なら常に同じ結果になる。
pub fn expand(body: &str, ctx: &ExpandCtx) -> Expansion {
    let cs: Vec<char> = body.chars().collect();
    let mut i = 0usize;
    let mut warn = Vec::new();
    let nodes = parse_nodes(&cs, &mut i, false, 0, &mut warn);
    let mut values: HashMap<u32, String> = HashMap::new();
    collect_values(&nodes, ctx, &mut values);
    let mut r = Render::new(ctx, &values);
    r.run(&nodes);
    let mut stops: Vec<TabStop> = r
        .order
        .iter()
        .map(|ix| TabStop {
            index: *ix,
            ranges: r.ranges.get(ix).cloned().unwrap_or_default(),
        })
        .collect();
    // $0 は最後、それ以外は昇順 (VS Code のタブ順)
    stops.sort_by_key(|s| if s.index == 0 { u32::MAX } else { s.index });
    Expansion { text: r.out, stops }
}

/// 本文の書式チェック。展開自体は決して panic しないので、これは
/// 「ユーザーへ見せる警告」を得るための別口。空 Vec = 問題なし。
pub fn lint(body: &str) -> Vec<String> {
    let cs: Vec<char> = body.chars().collect();
    let mut i = 0usize;
    let mut warn = Vec::new();
    let nodes = parse_nodes(&cs, &mut i, false, 0, &mut warn);
    let ctx = ExpandCtx::default();
    fn walk(nodes: &[Node], ctx: &ExpandCtx, warn: &mut Vec<String>) {
        for n in nodes {
            match n {
                Node::Text(_) => {}
                Node::Stop { children, .. } => walk(children, ctx, warn),
                Node::Var { name, default } => {
                    if resolve_var(name, ctx).is_none() {
                        warn.push(format!("未知の変数 ${name} は空文字になります"));
                    }
                    walk(default, ctx, warn);
                }
            }
        }
    }
    walk(&nodes, &ctx, &mut warn);
    warn.dedup();
    warn
}

// ===========================================================================
// キャレット位置でのスニペット展開
// ===========================================================================

/// 展開の当たり判定つき結果。`start` は置換を始めた char 位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpandHit {
    /// 置換開始位置 (入力 text 内の char 位置)。
    pub start: usize,
    /// 置換後の全文。
    pub text: String,
    /// キャレット位置 (text 内の char 位置)。
    pub cursor: usize,
    /// タブストップ (text 内の絶対 char 位置)。
    pub stops: Vec<TabStop>,
}

fn word_start_before(chars: &[char], cursor: usize) -> usize {
    let mut start = cursor;
    while start > 0 {
        let c = chars[start - 1];
        if c.is_ascii_alphanumeric() || c == '_' {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

/// キャレット直前の単語が prefix と一致すればスニペットへ置き換える。
/// タブストップつきの詳細版。
pub fn try_expand_ctx(
    text: &str,
    cursor_char: usize,
    snippets: &[Snippet],
    ctx: &ExpandCtx,
) -> Option<ExpandHit> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor_char.min(chars.len());
    let start = word_start_before(&chars, cursor);
    if start == cursor {
        return None;
    }
    let word: String = chars[start..cursor].iter().collect();
    let sn = snippets.iter().find(|s| s.prefix == word)?;
    let ex = expand(&sn.body, ctx);
    let mut new_text: String = chars[..start].iter().collect();
    new_text.push_str(&ex.text);
    new_text.extend(chars[cursor..].iter());
    let cursor_abs = start + ex.cursor();
    let shifted = ex.shifted(start);
    Some(ExpandHit {
        start,
        text: new_text,
        cursor: cursor_abs,
        stops: shifted.stops,
    })
}

/// キャレット直前の単語が prefix と一致すればスニペットへ置き換える。
/// 返り値は (置換後の全文, キャレットの char 位置)。
pub fn try_expand_at(
    text: &str,
    cursor_char: usize,
    snippets: &[Snippet],
    filename: &str,
) -> Option<(String, usize)> {
    let ctx = ExpandCtx::for_path(filename);
    try_expand_ctx(text, cursor_char, snippets, &ctx).map(|h| (h.text, h.cursor))
}

// ===========================================================================
// 組み込みスニペット (データ表)
// ===========================================================================

struct Builtin {
    lang: &'static str,
    name: &'static str,
    prefix: &'static str,
    body: &'static str,
    desc: &'static str,
}

/// 組み込みスニペット。ユーザー定義 (`~/.zaivern/snippets`) が同じ prefix を
/// 持つ場合はユーザー側が勝つ。`*` は全言語共通。
static BUILTINS: &[Builtin] = &[
    Builtin { lang: "*", name: "TODO", prefix: "todo", body: "$LINE_COMMENT TODO: $0", desc: "TODO コメント" },
    Builtin { lang: "*", name: "FIXME", prefix: "fixme", body: "$LINE_COMMENT FIXME: $0", desc: "FIXME コメント" },
    Builtin { lang: "rust", name: "Function", prefix: "fn", body: "fn ${1:name}(${2}) {\n\t$0\n}", desc: "関数" },
    Builtin { lang: "rust", name: "Public function", prefix: "pfn", body: "pub fn ${1:name}(${2}) {\n\t$0\n}", desc: "公開関数" },
    Builtin { lang: "rust", name: "Test", prefix: "test", body: "#[test]\nfn ${1:name}() {\n\t$0\n}", desc: "テスト関数" },
    Builtin { lang: "rust", name: "Match", prefix: "match", body: "match ${1:expr} {\n\t${2:_} => $0,\n}", desc: "match 式" },
    Builtin { lang: "rust", name: "Impl", prefix: "impl", body: "impl ${1:Type} {\n\t$0\n}", desc: "impl ブロック" },
    Builtin { lang: "rust", name: "Println", prefix: "pr", body: "println!(\"$1\");\n$0", desc: "標準出力" },
    Builtin { lang: "javascript", name: "Console log", prefix: "log", body: "console.log($1);\n$0", desc: "ログ出力" },
    Builtin { lang: "javascript", name: "Function", prefix: "fn", body: "function ${1:name}(${2}) {\n\t$0\n}", desc: "関数" },
    Builtin { lang: "javascript", name: "Arrow function", prefix: "af", body: "const ${1:name} = (${2}) => {\n\t$0\n};", desc: "アロー関数" },
    Builtin { lang: "javascript", name: "Import", prefix: "imp", body: "import ${1:name} from \"${2:module}\";\n$0", desc: "import 文" },
    Builtin { lang: "typescript", name: "Interface", prefix: "int", body: "interface ${1:Name} {\n\t$0\n}", desc: "インターフェース" },
    Builtin { lang: "python", name: "Function", prefix: "def", body: "def ${1:name}(${2}):\n\t$0", desc: "関数" },
    Builtin { lang: "python", name: "Class", prefix: "class", body: "class ${1:Name}:\n\tdef __init__(self${2}):\n\t\t$0", desc: "クラス" },
    Builtin { lang: "python", name: "Main guard", prefix: "main", body: "if __name__ == \"__main__\":\n\t$0", desc: "main ガード" },
    Builtin { lang: "go", name: "Function", prefix: "func", body: "func ${1:name}(${2}) ${3:error} {\n\t$0\n}", desc: "関数" },
    Builtin { lang: "go", name: "Error check", prefix: "iferr", body: "if err != nil {\n\treturn ${1:err}\n}\n$0", desc: "エラー処理" },
    Builtin { lang: "markdown", name: "Link", prefix: "link", body: "[${1:text}](${2:url})$0", desc: "リンク" },
    Builtin { lang: "markdown", name: "Code fence", prefix: "code", body: "```${1:lang}\n$0\n```", desc: "コードブロック" },
    Builtin { lang: "html", name: "HTML5 document", prefix: "doc", body: "<!DOCTYPE html>\n<html lang=\"${1:ja}\">\n<head>\n\t<meta charset=\"UTF-8\">\n\t<title>${2:Document}</title>\n</head>\n<body>\n\t$0\n</body>\n</html>", desc: "HTML5 雛形" },
    Builtin { lang: "css", name: "Media query", prefix: "mq", body: "@media (max-width: ${1:768}px) {\n\t$0\n}", desc: "メディアクエリ" },
];

/// 言語 (と継承元) の組み込みスニペットを優先順で返す。
pub fn builtin_for_lang(lang_id: &str) -> Vec<Snippet> {
    let mut out = Vec::new();
    let mut langs = snippet_langs(lang_id);
    langs.push("*".to_string());
    for l in langs {
        for b in BUILTINS.iter().filter(|b| b.lang == l) {
            out.push(Snippet {
                name: b.name.to_string(),
                prefix: b.prefix.to_string(),
                body: b.body.to_string(),
                description: b.desc.to_string(),
                language: b.lang.to_string(),
            });
        }
    }
    out
}

// ===========================================================================
// ユーザースニペット (~/.zaivern/snippets)
// ===========================================================================

/// ユーザースニペットの置き場所 (`<zaivern_dir>/snippets`)。
/// パスをここで決め打ちせず、設定側の `zaivern_dir()` に従う。
pub fn user_snippets_dir() -> PathBuf {
    crate::config::zaivern_dir().join("snippets")
}

/// ディレクトリを丸ごと読む。返り値は (言語 ID → スニペット, 診断メッセージ)。
/// ディレクトリが無い場合は空 + 診断なし (エラーではない)。
/// ファイル名が言語 ID なら言語別、それ以外 (`global.json` /
/// `*.code-snippets`) は全言語 (`*`) 扱い。エントリの `scope` が優先。
pub fn load_dir(dir: &Path) -> (HashMap<String, Vec<Snippet>>, Vec<String>) {
    let mut by_lang: HashMap<String, Vec<Snippet>> = HashMap::new();
    let mut diags: Vec<String> = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return (by_lang, diags), // 未作成は正常系
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|x| x.to_str()),
                Some("json") | Some("code-snippets")
            )
        })
        .collect();
    files.sort(); // 読み込み順を安定させる (結果の決定性)
    for path in files {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let file_lang = if LANGS.iter().any(|l| l.id == stem) {
            stem.clone()
        } else {
            "*".to_string()
        };
        match parse_file_checked(&path, &file_lang) {
            Ok(snips) => {
                for s in snips {
                    for l in s.language.split(',') {
                        let l = l.trim();
                        if l.is_empty() {
                            continue;
                        }
                        by_lang.entry(l.to_string()).or_default().push(Snippet {
                            language: l.to_string(),
                            ..s.clone()
                        });
                    }
                }
            }
            Err(e) => diags.push(trf(
                "⚠ スニペット {file} を読めません — {err}",
                &[
                    (
                        "file",
                        path.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    ),
                    ("err", e),
                ],
            )),
        }
    }
    (by_lang, diags)
}

/// ユーザースニペットの保管庫。読み込みは `reload()` を呼んだときだけ行う
/// (毎フレーム走らせない)。
#[derive(Clone, Debug, Default)]
pub struct SnippetStore {
    dir: PathBuf,
    by_lang: HashMap<String, Vec<Snippet>>,
    diagnostics: Vec<String>,
}

impl SnippetStore {
    /// ディスクには触れない。実際の読み込みは `reload()`。
    pub fn new(dir: PathBuf) -> SnippetStore {
        SnippetStore {
            dir,
            by_lang: HashMap::new(),
            diagnostics: Vec::new(),
        }
    }
    /// 既定の置き場所 (`~/.zaivern/snippets`) を使う。
    pub fn default_dir() -> SnippetStore {
        SnippetStore::new(user_snippets_dir())
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    /// 読み直す。壊れたファイルは診断に積んで残りを活かす (panic しない)。
    pub fn reload(&mut self) {
        let (by_lang, diags) = load_dir(&self.dir);
        self.by_lang = by_lang;
        self.diagnostics = diags;
    }
    /// 直近の読み込みで出た警告 (UI から見せる用)。
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }
    /// ユーザー定義の総数。
    pub fn user_count(&self) -> usize {
        self.by_lang.values().map(|v| v.len()).sum()
    }
    /// その言語で使えるスニペットを優先順に返す。
    /// ユーザー (言語別 → 継承元 → 全言語) → 組み込み、prefix 重複は先勝ち。
    pub fn for_lang(&self, lang_id: &str) -> Vec<Snippet> {
        let mut out: Vec<Snippet> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut langs = snippet_langs(lang_id);
        langs.push("*".to_string());
        for l in &langs {
            if let Some(v) = self.by_lang.get(l) {
                for s in v {
                    if seen.insert(s.prefix.clone()) {
                        out.push(s.clone());
                    }
                }
            }
        }
        for s in builtin_for_lang(lang_id) {
            if seen.insert(s.prefix.clone()) {
                out.push(s);
            }
        }
        out
    }
}

// ===========================================================================
// syntect 名 → スニペット言語 ID
// ===========================================================================

/// エディタの syntect シンタックス名 ("Rust", "JavaScript", …) を
/// VS Code の言語 ID ("rust", "javascript", …) へ写す。
pub fn lang_id_for(syntect_name: &str) -> &'static str {
    match syntect_name {
        "Rust" => "rust",
        "JavaScript" | "JavaScript (Babel)" => "javascript",
        "TypeScript" => "typescript",
        "TypeScriptReact" | "TSX" => "typescriptreact",
        "JSX" | "JavaScriptReact" => "javascriptreact",
        "Python" => "python",
        "C" => "c",
        "C++" => "cpp",
        "C#" => "csharp",
        "Go" => "go",
        "Java" => "java",
        "Kotlin" => "kotlin",
        "Swift" => "swift",
        "Objective-C" => "objective-c",
        "Objective-C++" => "objective-cpp",
        "Ruby" => "ruby",
        "PHP" => "php",
        "Perl" => "perl",
        "Lua" => "lua",
        "R" => "r",
        "Scala" => "scala",
        "Haskell" => "haskell",
        "Erlang" => "erlang",
        "Elixir" => "elixir",
        "Dart" => "dart",
        "HTML" | "HTML (ASP)" => "html",
        "CSS" => "css",
        "SCSS" => "scss",
        "Sass" => "sass",
        "Less" => "less",
        "JSON" => "json",
        "XML" => "xml",
        "YAML" => "yaml",
        "TOML" => "toml",
        "Markdown" => "markdown",
        "SQL" => "sql",
        "Shell-Unix-Generic" | "Bourne Again Shell (bash)" | "Shell Script (Bash)" => "shellscript",
        "Batch File" => "bat",
        "PowerShell" => "powershell",
        "Makefile" => "makefile",
        "Dockerfile" => "dockerfile",
        "Graphviz (DOT)" => "dot",
        "LaTeX" => "latex",
        "Vue Component" | "Vue" => "vue",
        "Svelte" => "svelte",
        _ => "plaintext",
    }
}

// ===========================================================================
// Emmet 略記展開
// ===========================================================================

/// 空要素 (子を持たないタグ)。
static VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// 親タグ → 省略時に補う子タグ (`ul>.item` が `li.item` になる規則)。
static IMPLICIT_CHILD: &[(&str, &str)] = &[
    ("ul", "li"),
    ("ol", "li"),
    ("dl", "dt"),
    ("table", "tr"),
    ("tbody", "tr"),
    ("thead", "tr"),
    ("tfoot", "tr"),
    ("tr", "td"),
    ("select", "option"),
    ("optgroup", "option"),
    ("datalist", "option"),
    ("map", "area"),
    ("audio", "source"),
    ("video", "source"),
    ("picture", "source"),
    ("nav", "a"),
    ("figure", "img"),
];

/// タグに自動で付く属性 (明示指定があればそちらが勝つ)。
static IMPLICIT_ATTRS: &[(&str, &[(&str, &str)])] = &[
    ("a", &[("href", "")]),
    ("img", &[("src", ""), ("alt", "")]),
    ("input", &[("type", "text")]),
    ("link", &[("rel", "stylesheet"), ("href", "")]),
    ("form", &[("action", "")]),
    ("script", &[("src", "")]),
];

/// 既知の HTML タグ。1 要素だけの略記 (`div`, `p`) はこの表に載っている
/// ときだけ展開する — 散文の単語を Tab で壊さないための歯止め。
static HTML_TAGS: &[&str] = &[
    "a",
    "abbr",
    "address",
    "area",
    "article",
    "aside",
    "audio",
    "b",
    "base",
    "bdi",
    "bdo",
    "blockquote",
    "body",
    "br",
    "button",
    "canvas",
    "caption",
    "cite",
    "code",
    "col",
    "colgroup",
    "data",
    "datalist",
    "dd",
    "del",
    "details",
    "dfn",
    "dialog",
    "div",
    "dl",
    "dt",
    "em",
    "embed",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hgroup",
    "hr",
    "html",
    "i",
    "iframe",
    "img",
    "input",
    "ins",
    "kbd",
    "label",
    "legend",
    "li",
    "link",
    "main",
    "map",
    "mark",
    "menu",
    "meta",
    "meter",
    "nav",
    "noscript",
    "object",
    "ol",
    "optgroup",
    "option",
    "output",
    "p",
    "param",
    "picture",
    "pre",
    "progress",
    "q",
    "rp",
    "rt",
    "ruby",
    "s",
    "samp",
    "script",
    "section",
    "select",
    "slot",
    "small",
    "source",
    "span",
    "strong",
    "style",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "template",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "time",
    "title",
    "tr",
    "track",
    "u",
    "ul",
    "var",
    "video",
    "wbr",
];

/// 記号入りの定型略記 (`!` = HTML5 雛形)。`\t` は要求インデントへ置換する。
static MARKUP_SNIPPETS: &[(&str, &str)] = &[
    ("!", "<!DOCTYPE html>\n<html lang=\"ja\">\n<head>\n\t<meta charset=\"UTF-8\">\n\t<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n\t<title>Document</title>\n</head>\n<body>\n\t\n</body>\n</html>"),
    ("html:5", "<!DOCTYPE html>\n<html lang=\"ja\">\n<head>\n\t<meta charset=\"UTF-8\">\n\t<title>Document</title>\n</head>\n<body>\n\t\n</body>\n</html>"),
    ("link:css", "<link rel=\"stylesheet\" href=\"style.css\">"),
    ("script:src", "<script src=\"\"></script>"),
    ("meta:utf", "<meta charset=\"UTF-8\">"),
    ("meta:vp", "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">"),
];

/// CSS の定型略記 (値を取らないもの)。
static CSS_SNIPPETS: &[(&str, &str)] = &[
    ("df", "display: flex;"),
    ("dib", "display: inline-block;"),
    ("db", "display: block;"),
    ("dn", "display: none;"),
    ("dg", "display: grid;"),
    ("posr", "position: relative;"),
    ("posa", "position: absolute;"),
    ("posf", "position: fixed;"),
    ("poss", "position: sticky;"),
    ("fll", "float: left;"),
    ("flr", "float: right;"),
    ("tac", "text-align: center;"),
    ("tal", "text-align: left;"),
    ("tar", "text-align: right;"),
    ("ma", "margin: auto;"),
    ("mt-a", "margin-top: auto;"),
    ("ovh", "overflow: hidden;"),
    ("ova", "overflow: auto;"),
    ("fwb", "font-weight: bold;"),
    ("fwn", "font-weight: normal;"),
    ("curp", "cursor: pointer;"),
    ("aic", "align-items: center;"),
    ("jcc", "justify-content: center;"),
    ("jcsb", "justify-content: space-between;"),
    ("fxdc", "flex-direction: column;"),
    ("bdn", "border: none;"),
    ("bxsbb", "box-sizing: border-box;"),
];

/// CSS の「プロパティ略記 → プロパティ名」。値は数値部分から組む。
static CSS_PROPS: &[(&str, &str)] = &[
    ("m", "margin"),
    ("mt", "margin-top"),
    ("mr", "margin-right"),
    ("mb", "margin-bottom"),
    ("ml", "margin-left"),
    ("p", "padding"),
    ("pt", "padding-top"),
    ("pr", "padding-right"),
    ("pb", "padding-bottom"),
    ("pl", "padding-left"),
    ("w", "width"),
    ("h", "height"),
    ("maw", "max-width"),
    ("mah", "max-height"),
    ("miw", "min-width"),
    ("mih", "min-height"),
    ("fz", "font-size"),
    ("lh", "line-height"),
    ("bdrs", "border-radius"),
    ("op", "opacity"),
    ("z", "z-index"),
    ("t", "top"),
    ("r", "right"),
    ("b", "bottom"),
    ("l", "left"),
    ("fxg", "flex-grow"),
    ("fxsh", "flex-shrink"),
    ("gap", "gap"),
    ("bdw", "border-width"),
];

/// Emmet 展開の設定。言語データ表から `for_lang` で組むのが基本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmmetOpts {
    /// インデント 1 段 ("\t" / "  " など)。
    pub indent: String,
    pub kind: EmmetKind,
    /// 空要素を `<br />` と閉じるか。
    pub xml_close: bool,
    /// キャレット位置の親タグ (暗黙タグ名の推定に使う。空なら div)。
    pub parent_tag: String,
}

impl EmmetOpts {
    pub fn for_lang(lang_id: &str, indent: &str) -> EmmetOpts {
        let spec = lang_spec(lang_id);
        EmmetOpts {
            indent: if indent.is_empty() {
                default_indent(lang_id)
            } else {
                indent.to_string()
            },
            kind: spec.emmet,
            xml_close: spec.xml_close,
            parent_tag: String::new(),
        }
    }
    pub fn with_parent(mut self, tag: &str) -> Self {
        self.parent_tag = tag.to_string();
        self
    }
}

#[derive(Default, Clone, Debug)]
struct EmNode {
    tag: String,
    /// 出現順の属性。class は 1 エントリへ連結。
    attrs: Vec<(String, String)>,
    text: Option<String>,
    mult: usize,
    parent: usize,
    children: Vec<usize>,
}

/// 繰り返しの上限 (`li*99999` で巨大出力を作らせない)。
const MAX_REPEAT: usize = 1000;
/// 略記の最大長。
const MAX_ABBR: usize = 240;
/// ツリーの最大ノード数。
const MAX_NODES: usize = 500;

fn em_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '$' || c == ':' || c == '.'
}

/// `$` 連番を適用する。`$` の連続数 = ゼロ埋め桁数 (`$$` → 01)。
/// `\$` は素の `$`。
fn apply_numbering(s: &str, idx: usize) -> String {
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < cs.len() {
        if cs[i] == '\\' && i + 1 < cs.len() && cs[i + 1] == '$' {
            out.push('$');
            i += 2;
            continue;
        }
        if cs[i] == '$' {
            let mut n = 0usize;
            while i < cs.len() && cs[i] == '$' {
                n += 1;
                i += 1;
            }
            out.push_str(&format!("{idx:0width$}", width = n));
            continue;
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// 1 要素 (`div.a#b[href=x]{text}*3`) を読む。読めなければ None。
fn parse_em_element(cs: &[char], i: &mut usize) -> Option<EmNode> {
    let mut n = EmNode {
        mult: 1,
        ..Default::default()
    };
    // タグ名 (英字始まり)
    if *i < cs.len() && cs[*i].is_ascii_alphabetic() {
        let start = *i;
        while *i < cs.len() && (cs[*i].is_ascii_alphanumeric() || cs[*i] == '-' || cs[*i] == '$') {
            *i += 1;
        }
        n.tag = cs[start..*i].iter().collect();
    }
    let mut saw_any = !n.tag.is_empty();
    loop {
        if *i >= cs.len() {
            break;
        }
        match cs[*i] {
            '#' => {
                *i += 1;
                let start = *i;
                while *i < cs.len() && em_name_char(cs[*i]) && cs[*i] != '.' {
                    *i += 1;
                }
                if start == *i {
                    return None;
                }
                let id: String = cs[start..*i].iter().collect();
                set_attr(&mut n.attrs, "id", &id);
                saw_any = true;
            }
            '.' => {
                *i += 1;
                let start = *i;
                while *i < cs.len() && em_name_char(cs[*i]) && cs[*i] != '.' {
                    *i += 1;
                }
                if start == *i {
                    return None;
                }
                let cls: String = cs[start..*i].iter().collect();
                append_class(&mut n.attrs, &cls);
                saw_any = true;
            }
            '[' => {
                *i += 1;
                loop {
                    while *i < cs.len() && cs[*i] == ' ' {
                        *i += 1;
                    }
                    if *i < cs.len() && cs[*i] == ']' {
                        *i += 1;
                        break;
                    }
                    let start = *i;
                    while *i < cs.len()
                        && (cs[*i].is_ascii_alphanumeric()
                            || cs[*i] == '-'
                            || cs[*i] == '_'
                            || cs[*i] == ':'
                            || cs[*i] == '@')
                    {
                        *i += 1;
                    }
                    if start == *i {
                        return None; // 属性名が読めない
                    }
                    let key: String = cs[start..*i].iter().collect();
                    let mut val = String::new();
                    if *i < cs.len() && cs[*i] == '=' {
                        *i += 1;
                        if *i < cs.len() && (cs[*i] == '"' || cs[*i] == '\'') {
                            let q = cs[*i];
                            *i += 1;
                            while *i < cs.len() && cs[*i] != q {
                                val.push(cs[*i]);
                                *i += 1;
                            }
                            if *i >= cs.len() {
                                return None;
                            }
                            *i += 1;
                        } else {
                            while *i < cs.len() && cs[*i] != ' ' && cs[*i] != ']' {
                                val.push(cs[*i]);
                                *i += 1;
                            }
                        }
                    }
                    set_attr(&mut n.attrs, &key, &val);
                    if *i >= cs.len() {
                        return None; // ] が来ない
                    }
                }
                saw_any = true;
            }
            '{' => {
                *i += 1;
                let mut depth = 0usize;
                let mut t = String::new();
                let mut closed = false;
                while *i < cs.len() {
                    let c = cs[*i];
                    if c == '\\' && *i + 1 < cs.len() {
                        // `\$` は連番置換まで生かす (apply_numbering が外す)
                        if cs[*i + 1] == '$' {
                            t.push('\\');
                        }
                        t.push(cs[*i + 1]);
                        *i += 2;
                        continue;
                    }
                    if c == '{' {
                        depth += 1;
                    } else if c == '}' {
                        if depth == 0 {
                            *i += 1;
                            closed = true;
                            break;
                        }
                        depth -= 1;
                    }
                    t.push(c);
                    *i += 1;
                }
                if !closed {
                    return None;
                }
                n.text = Some(t);
                saw_any = true;
            }
            '*' => {
                *i += 1;
                let start = *i;
                while *i < cs.len() && cs[*i].is_ascii_digit() {
                    *i += 1;
                }
                if start == *i {
                    return None;
                }
                let num: String = cs[start..*i].iter().collect();
                n.mult = num.parse::<usize>().unwrap_or(1).min(MAX_REPEAT);
                // `*` だけでは要素にならない (`*3` 単体は不正)
            }
            _ => break,
        }
    }
    if !saw_any {
        return None;
    }
    Some(n)
}

fn set_attr(attrs: &mut Vec<(String, String)>, key: &str, val: &str) {
    if let Some(a) = attrs.iter_mut().find(|a| a.0 == key) {
        a.1 = val.to_string();
    } else {
        attrs.push((key.to_string(), val.to_string()));
    }
}

fn append_class(attrs: &mut Vec<(String, String)>, cls: &str) {
    if let Some(a) = attrs.iter_mut().find(|a| a.0 == "class") {
        if !a.1.is_empty() {
            a.1.push(' ');
        }
        a.1.push_str(cls);
    } else {
        attrs.push(("class".to_string(), cls.to_string()));
    }
}

/// 略記全体をツリー (arena, 0 = 根) へ。解釈できなければ None。
fn parse_emmet_tree(abbr: &str) -> Option<Vec<EmNode>> {
    let cs: Vec<char> = abbr.chars().collect();
    if cs.is_empty() || cs.len() > MAX_ABBR {
        return None;
    }
    let mut arena: Vec<EmNode> = vec![EmNode {
        mult: 1,
        ..Default::default()
    }];
    let mut parent = 0usize;
    let mut i = 0usize;
    loop {
        let node = parse_em_element(&cs, &mut i)?;
        if arena.len() >= MAX_NODES {
            return None;
        }
        let idx = arena.len();
        arena.push(EmNode { parent, ..node });
        arena[parent].children.push(idx);
        if i >= cs.len() {
            break;
        }
        match cs[i] {
            '>' => {
                parent = idx;
                i += 1;
            }
            '+' => {
                i += 1;
            }
            '^' => {
                let mut p = parent;
                while i < cs.len() && cs[i] == '^' {
                    p = arena[p].parent;
                    i += 1;
                }
                parent = p;
            }
            _ => return None,
        }
        if i >= cs.len() {
            return None; // 演算子で終わる略記は不正
        }
    }
    Some(arena)
}

fn implicit_tag(parent_tag: &str) -> &'static str {
    IMPLICIT_CHILD
        .iter()
        .find(|(p, _)| *p == parent_tag)
        .map(|(_, c)| *c)
        .unwrap_or("div")
}

fn render_attrs(attrs: &[(String, String)], tag: &str, idx: usize) -> String {
    let mut all: Vec<(String, String)> = attrs
        .iter()
        .map(|(k, v)| (k.clone(), apply_numbering(v, idx)))
        .collect();
    if let Some((_, imp)) = IMPLICIT_ATTRS.iter().find(|(t, _)| *t == tag) {
        for (k, v) in imp.iter() {
            if !all.iter().any(|a| a.0 == *k) {
                all.push((k.to_string(), v.to_string()));
            }
        }
    }
    let mut s = String::new();
    for (k, v) in all {
        s.push_str(&format!(" {k}=\"{v}\""));
    }
    s
}

fn render_em(
    arena: &[EmNode],
    idx: usize,
    depth: usize,
    inherited_num: usize,
    parent_tag: &str,
    opts: &EmmetOpts,
    out: &mut String,
) {
    let node = &arena[idx];
    let count = node.mult.min(MAX_REPEAT);
    if count == 0 {
        return;
    }
    for k in 1..=count {
        let num = if count > 1 { k } else { inherited_num };
        render_em_one(arena, idx, depth, num, parent_tag, opts, out);
    }
}

fn render_em_one(
    arena: &[EmNode],
    idx: usize,
    depth: usize,
    num: usize,
    parent_tag: &str,
    opts: &EmmetOpts,
    out: &mut String,
) {
    let node = &arena[idx];
    let tag = if node.tag.is_empty() {
        implicit_tag(parent_tag).to_string()
    } else {
        apply_numbering(&node.tag, num)
    };
    let pad = opts.indent.repeat(depth);
    let attrs = render_attrs(&node.attrs, &tag, num);
    let text = node.text.as_ref().map(|t| apply_numbering(t, num));
    if VOID_TAGS.contains(&tag.as_str()) {
        let close = if opts.xml_close { " />" } else { ">" };
        out.push_str(&format!("{pad}<{tag}{attrs}{close}\n"));
        return;
    }
    if node.children.is_empty() {
        out.push_str(&format!(
            "{pad}<{tag}{attrs}>{}</{tag}>\n",
            text.unwrap_or_default()
        ));
        return;
    }
    out.push_str(&format!("{pad}<{tag}{attrs}>\n"));
    if let Some(t) = text {
        if !t.is_empty() {
            out.push_str(&format!("{pad}{}{t}\n", opts.indent));
        }
    }
    for c in &node.children {
        render_em(arena, *c, depth + 1, num, &tag, opts, out);
    }
    out.push_str(&format!("{pad}</{tag}>\n"));
}

/// CSS 略記 1 個を展開する。
fn expand_css_one(abbr: &str) -> Option<String> {
    if let Some((_, v)) = CSS_SNIPPETS.iter().find(|(k, _)| *k == abbr) {
        return Some(v.to_string());
    }
    let cs: Vec<char> = abbr.chars().collect();
    let mut i = 0usize;
    while i < cs.len() && cs[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == 0 || i == cs.len() {
        return None;
    }
    let key: String = cs[..i].iter().collect();
    let (_, prop) = CSS_PROPS.iter().find(|(k, _)| *k == key)?;
    let rest: String = cs[i..].iter().collect();
    // `-` は値の区切り。先頭の `-` だけは負値の符号 (`m-10` → margin: -10px)。
    let (neg, rest) = match rest.strip_prefix('-') {
        Some(r) => (true, r.to_string()),
        None => (false, rest),
    };
    let mut values: Vec<String> = Vec::new();
    for (n, part) in rest.split('-').enumerate() {
        if part.is_empty() {
            return None;
        }
        values.push(css_value(part, neg && n == 0)?);
    }
    if values.is_empty() {
        return None;
    }
    Some(format!("{prop}: {};", values.join(" ")))
}

/// 数値 + 単位記号を CSS 値へ。単位なしは px、`0` はそのまま、
/// `p` = %、`e` = em、`r` = rem。
fn css_value(part: &str, neg: bool) -> Option<String> {
    let mut num = String::new();
    let mut unit = String::new();
    for c in part.chars() {
        if c.is_ascii_digit() || c == '.' {
            if !unit.is_empty() {
                return None;
            }
            num.push(c);
        } else if c.is_ascii_alphabetic() {
            unit.push(c);
        } else {
            return None;
        }
    }
    if num.is_empty() {
        return None;
    }
    let sign = if neg { "-" } else { "" };
    let u = match unit.as_str() {
        "" => {
            if num == "0" {
                return Some(format!("{sign}0"));
            }
            "px"
        }
        "p" => "%",
        "e" => "em",
        "r" => "rem",
        "v" => "vh",
        "vw" => "vw",
        "vh" => "vh",
        "px" => "px",
        "em" => "em",
        "rem" => "rem",
        "s" => "s",
        "ms" => "ms",
        _ => return None,
    };
    Some(format!("{sign}{num}{u}"))
}

/// Emmet 略記を展開する。解釈できない入力は **None** (呼び出し側は
/// 通常の Tab 動作へ落とす)。
pub fn expand_emmet(abbr: &str, opts: &EmmetOpts) -> Option<String> {
    let a = abbr.trim();
    if a.is_empty() || a.chars().count() > MAX_ABBR {
        return None;
    }
    match opts.kind {
        EmmetKind::None => None,
        EmmetKind::Style => {
            let mut lines = Vec::new();
            for p in a.split('+') {
                lines.push(expand_css_one(p.trim())?);
            }
            Some(lines.join("\n"))
        }
        EmmetKind::Markup => {
            if let Some((_, body)) = MARKUP_SNIPPETS.iter().find(|(k, _)| *k == a) {
                return Some(body.replace('\t', &opts.indent));
            }
            let arena = parse_emmet_tree(a)?;
            // 単独の裸タグは既知の HTML タグのときだけ展開する (散文保護)
            if arena.len() == 2 {
                let n = &arena[1];
                if n.attrs.is_empty()
                    && n.text.is_none()
                    && n.mult == 1
                    && !n.tag.is_empty()
                    && !HTML_TAGS.contains(&n.tag.as_str())
                {
                    return None;
                }
            }
            let mut out = String::new();
            let root_children = arena[0].children.clone();
            for c in root_children {
                render_em(&arena, c, 0, 1, &opts.parent_tag, opts, &mut out);
            }
            let out = out.trim_end_matches('\n').to_string();
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
    }
}

/// Emmet の当たり判定結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmmetHit {
    /// 置換開始の行内 char 位置。
    pub start: usize,
    /// 展開後テキスト (2 行目以降は元行のインデントを継いでいる)。
    pub text: String,
    /// text 内でのキャレット char 位置。
    pub cursor: usize,
}

/// 行 `line` のキャレット位置 (行内 char 位置) の直前にある略記を展開する。
/// 空白で区切った最長の候補から順に試し、どれも解釈できなければ None。
pub fn try_emmet_at(line: &str, caret: usize, lang_id: &str, indent: &str) -> Option<EmmetHit> {
    let opts = EmmetOpts::for_lang(lang_id, indent);
    if opts.kind == EmmetKind::None {
        return None;
    }
    let cs: Vec<char> = line.chars().collect();
    let caret = caret.min(cs.len());
    if caret == 0 || cs[caret - 1].is_whitespace() {
        return None; // 空白直後の Tab は通常のインデント動作
    }
    // 候補の開始位置: 行頭、または空白の直後 (空白そのものは含めない)
    let mut starts: Vec<usize> = vec![0];
    for (n, c) in cs[..caret].iter().enumerate() {
        if c.is_whitespace() {
            starts.push(n + 1);
        }
    }
    starts.retain(|s| *s < caret && !cs[*s].is_whitespace());
    let lead: String = cs
        .iter()
        .take_while(|c| **c == ' ' || **c == '\t')
        .collect();
    for s in starts {
        let abbr: String = cs[s..caret].iter().collect();
        if let Some(body) = expand_emmet(&abbr, &opts) {
            let text = body
                .split('\n')
                .enumerate()
                .map(|(n, l)| {
                    if n == 0 || l.is_empty() {
                        l.to_string()
                    } else {
                        format!("{lead}{l}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let chars: Vec<char> = text.chars().collect();
            // 最初の `></` の内側 (空のテキスト位置) にキャレットを置く
            let cursor = chars
                .windows(3)
                .position(|w| w == ['>', '<', '/'])
                .map(|p| p + 1)
                .unwrap_or(chars.len());
            return Some(EmmetHit {
                start: s,
                text,
                cursor,
            });
        }
    }
    None
}

// ===========================================================================
// テスト
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    fn ctx(f: &str) -> ExpandCtx {
        ExpandCtx::for_path(f)
    }

    /// 2023-11-14 22:13:20 UTC (火曜)。日付変数の期待値はすべてこの時刻基準。
    const T: i64 = 1_700_000_000;

    // ---- 展開: 構文 (表駆動) ----

    #[test]
    fn expand_syntax_table() {
        // (本文, 期待テキスト, 期待キャレット)
        let cases: &[(&str, &str, usize)] = &[
            ("hello", "hello", 5),                       // タブストップ無し = 末尾
            ("${1:default}", "default", 0),              // 既定値つき
            ("ab$0cd", "abcd", 2),                       // $0 のみ
            ("a$0b$1c", "abc", 2),                       // $1 が $0 より先
            ("${1}x", "x", 0),                           // ${1} 形式
            (r"\$1 \\ \}", r"$1 \ }", 6),                // エスケープ
            ("${1|red,green,blue|}!", "red!", 0),        // 選択肢は先頭が入る
            (r"${1|a\,b,c|}", "a,b", 0),                 // 選択肢内のエスケープカンマ
            ("こんにちは$1世界", "こんにちは世界", 5),   // char 単位 (バイトでない)
            ("${1:outer ${2:inner}}", "outer inner", 0), // ネスト
            ("if $1 {\n\t$0\n}", "if  {\n\t\n}", 3),     // 複数行
            ("$", "$", 1),                               // 裸の $
            ("100$", "100$", 4),
        ];
        for (body, want, cur) in cases {
            let ex = expand(body, &ctx("/tmp/a.rs"));
            assert_eq!(ex.text, *want, "本文: {body}");
            assert_eq!(ex.cursor(), *cur, "本文: {body}");
        }
    }

    #[test]
    fn expand_nested_placeholder_ranges() {
        let ex = expand("${1:outer ${2:inner}}", &ctx("/tmp/a.rs"));
        assert_eq!(ex.text, "outer inner");
        assert_eq!(ex.stops.len(), 2);
        assert_eq!(ex.stops[0].index, 1);
        assert_eq!(ex.stops[0].ranges, vec![(0, 11)]); // 外側は全体を覆う
        assert_eq!(ex.stops[1].index, 2);
        assert_eq!(ex.stops[1].ranges, vec![(6, 11)]);
    }

    #[test]
    fn expand_mirror_keeps_ranges_in_sync() {
        let ex = expand("${1:foo} = $1;", &ctx("/tmp/a.rs"));
        assert_eq!(ex.text, "foo = foo;");
        assert_eq!(ex.stops.len(), 1);
        let r = &ex.stops[0].ranges;
        assert_eq!(r.len(), 2, "ミラーは 2 レンジ");
        // 長さも中身も一致していること (同期の実体)
        let cs: Vec<char> = ex.text.chars().collect();
        let a: String = cs[r[0].0..r[0].1].iter().collect();
        let b: String = cs[r[1].0..r[1].1].iter().collect();
        assert_eq!(a, b);
        assert_eq!(r[0].1 - r[0].0, r[1].1 - r[1].0);
    }

    #[test]
    fn expand_mirror_works_before_definition() {
        // 参照が定義より前に出ても同じ文字列になる (前方ミラー)
        let ex = expand("$1 = ${1:x};", &ctx("/tmp/a.rs"));
        assert_eq!(ex.text, "x = x;");
        assert_eq!(ex.stops[0].ranges, vec![(0, 1), (4, 5)]);
    }

    #[test]
    fn expand_mirror_of_choice_and_second_default_ignored() {
        assert_eq!(expand("${1|a,b|} $1", &ctx("/tmp/a.rs")).text, "a a");
        // 2 つめの既定値は無視され、最初の定義にそろう (VS Code と同じ)
        let ex = expand("${1:foo} ${1:bar}", &ctx("/tmp/a.rs"));
        assert_eq!(ex.text, "foo foo");
        assert_eq!(ex.stops[0].ranges, vec![(0, 3), (4, 7)]);
    }

    #[test]
    fn expand_tabstop_ordering_zero_last_ascending_otherwise() {
        let ex = expand("$3 $1 $0 $2", &ctx("/tmp/a.rs"));
        let idx: Vec<u32> = ex.stops.iter().map(|s| s.index).collect();
        assert_eq!(idx, vec![1, 2, 3, 0], "$0 は最後、他は昇順");
        assert_eq!(ex.text, "   "); // タブストップは幅 0
        assert_eq!(ex.cursor(), 1); // 本文中の $1 の位置
    }

    #[test]
    fn expand_duplicate_indices_merge_into_one_stop() {
        let ex = expand("$1-$1-$1", &ctx("/tmp/a.rs"));
        assert_eq!(ex.stops.len(), 1);
        assert_eq!(ex.stops[0].ranges, vec![(0, 0), (1, 1), (2, 2)]);
    }

    #[test]
    fn expand_stop_navigation_helpers() {
        let ex = expand("${1:a} ${2:b} $0", &ctx("/tmp/a.rs"));
        assert_eq!(ex.text, "a b ");
        assert_eq!(ex.next_stop(0).map(|s| s.index), Some(2));
        assert_eq!(ex.next_stop(2).map(|s| s.index), Some(0));
        assert_eq!(ex.next_stop(9), None);
        assert_eq!(ex.prev_stop(4).map(|s| s.index), Some(2));
        assert_eq!(ex.prev_stop(0), None);
    }

    #[test]
    fn expand_shifted_moves_all_ranges() {
        let ex = expand("${1:a}$1", &ctx("/tmp/a.rs")).shifted(10);
        assert_eq!(ex.stops[0].ranges, vec![(10, 11), (11, 12)]);
    }

    #[test]
    fn expand_is_deterministic() {
        let c = ctx("/tmp/a.rs")
            .with_clock(Clock::from_unix(T))
            .with_random_seed(7);
        let body = "${1:x} $1 ${TM_FILENAME} $CURRENT_YEAR $UUID $RANDOM ${2|a,b|} $0";
        let a = expand(body, &c);
        let b = expand(body, &c);
        assert_eq!(a, b, "同じ入力は常に同じ結果");
    }

    // ---- 展開: 変数 ----

    #[test]
    fn expand_variable_table_with_injected_clock() {
        let c = ctx("/work/app/src/foo.rs")
            .with_workspace("/work/app")
            .with_clock(Clock::from_unix(T))
            .with_selection("SEL")
            .with_line_number(42);
        // (本文, 期待)
        let cases: &[(&str, &str)] = &[
            ("$TM_FILENAME", "foo.rs"),
            ("${TM_FILENAME_BASE}", "foo"),
            ("$TM_DIRECTORY", "/work/app/src"),
            ("$TM_FILEPATH", "/work/app/src/foo.rs"),
            ("$RELATIVE_FILEPATH", "src/foo.rs"),
            ("$WORKSPACE_NAME", "app"),
            ("$WORKSPACE_FOLDER", "/work/app"),
            ("$TM_LINE_NUMBER", "42"),
            ("$TM_LINE_INDEX", "41"),
            ("$TM_SELECTED_TEXT", "SEL"),
            ("$LINE_COMMENT", "//"),
            ("$BLOCK_COMMENT_START$BLOCK_COMMENT_END", "/**/"),
            ("$CURRENT_YEAR", "2023"),
            ("$CURRENT_YEAR_SHORT", "23"),
            ("$CURRENT_MONTH", "11"),
            ("$CURRENT_MONTH_NAME", "November"),
            ("$CURRENT_MONTH_NAME_SHORT", "Nov"),
            ("$CURRENT_DATE", "14"),
            ("$CURRENT_DAY_NAME", "Tuesday"),
            ("$CURRENT_DAY_NAME_SHORT", "Tue"),
            ("$CURRENT_HOUR", "22"),
            ("$CURRENT_MINUTE", "13"),
            ("$CURRENT_SECOND", "20"),
            ("$CURRENT_SECONDS_UNIX", "1700000000"),
        ];
        for (body, want) in cases {
            assert_eq!(expand(body, &c).text, *want, "本文: {body}");
        }
    }

    #[test]
    fn expand_clock_offset_shifts_local_time() {
        // JST (+9h) にすると日付が翌日へ繰り上がる
        let c = ctx("/tmp/a.rs").with_clock(Clock::from_unix_offset(T, 9 * 3600));
        assert_eq!(expand("$CURRENT_DATE $CURRENT_HOUR", &c).text, "15 07");
    }

    #[test]
    fn expand_without_clock_date_vars_are_empty() {
        let c = ctx("/tmp/a.rs");
        assert_eq!(expand("[$CURRENT_YEAR]", &c).text, "[]");
        // 既定値つきなら既定値が効く
        assert_eq!(expand("${CURRENT_YEAR:不明}", &c).text, "不明");
    }

    #[test]
    fn expand_line_comment_comes_from_lang_table() {
        let py = ExpandCtx::for_path("/tmp/a.py");
        assert_eq!(expand("$LINE_COMMENT x", &py).text, "# x");
        let rs = ExpandCtx::for_path("/tmp/a.rs");
        assert_eq!(expand("$LINE_COMMENT x", &rs).text, "// x");
        let html = ExpandCtx::for_path("/tmp/a.html");
        assert_eq!(
            expand("$BLOCK_COMMENT_START$BLOCK_COMMENT_END", &html).text,
            "<!---->"
        );
    }

    #[test]
    fn expand_unknown_variable_uses_default_or_empty() {
        let c = ctx("/tmp/a.rs");
        assert_eq!(expand("[$NO_SUCH_VAR]", &c).text, "[]");
        assert_eq!(expand("${NO_SUCH_VAR:fallback}", &c).text, "fallback");
    }

    #[test]
    fn expand_random_is_deterministic_with_seed_and_empty_without() {
        let c = ctx("/tmp/a.rs");
        assert_eq!(expand("$RANDOM$UUID$RANDOM_HEX", &c).text, "");
        let s = c.clone().with_random_seed(1234);
        let a = expand("$RANDOM $RANDOM_HEX $UUID", &s).text;
        let b = expand("$RANDOM $RANDOM_HEX $UUID", &s).text;
        assert_eq!(a, b);
        let parts: Vec<&str> = a.split(' ').collect();
        assert_eq!(parts[0].len(), 6);
        assert_eq!(parts[1].len(), 6);
        assert_eq!(parts[2].len(), 36); // UUID の桁組み
    }

    #[test]
    fn expand_nested_variable_in_placeholder() {
        let ex = expand("class ${1:$TM_FILENAME_BASE} {}", &ctx("/dir/MyMod.rs"));
        assert_eq!(ex.text, "class MyMod {}");
        assert_eq!(ex.cursor(), 6);
        assert_eq!(ex.stops[0].ranges, vec![(6, 11)]);
    }

    #[test]
    fn expand_var_default_used_when_selection_empty() {
        let ex = expand("${TM_SELECTED_TEXT:done}", &ctx("/tmp/a.rs"));
        assert_eq!(ex.text, "done");
        let ex2 = expand(
            "${TM_SELECTED_TEXT:done}",
            &ctx("/tmp/a.rs").with_selection("sel"),
        );
        assert_eq!(ex2.text, "sel");
    }

    // ---- 展開: 壊れた入力 ----

    #[test]
    fn expand_malformed_never_panics_and_lint_reports() {
        let bad = [
            "${1:unclosed",
            "${",
            "${1|a,b",
            "${1",
            "${!!}",
            "${VAR",
            "${1:${2:x}",
        ];
        for b in bad {
            let ex = expand(b, &ctx("/tmp/a.rs")); // panic しないこと
            assert!(ex.text.chars().count() <= b.chars().count() + 8, "{b}");
            assert!(!lint(b).is_empty(), "警告が出るべき: {b}");
        }
        // 正常な本文は警告なし
        assert!(lint("fn ${1:name}() {\n\t$0\n}").is_empty());
        // 未知の変数は警告される
        assert!(lint("$NO_SUCH_VAR")
            .iter()
            .any(|m| m.contains("未知の変数")));
    }

    #[test]
    fn expand_deep_nesting_is_capped() {
        let body = "${1:".repeat(200);
        let ex = expand(&body, &ctx("/tmp/a.rs")); // スタックを溢れさせない
        assert!(!ex.text.is_empty() || ex.stops.is_empty());
        let _ = lint(&body);
    }

    #[test]
    fn expand_transform_syntax_degrades_to_plain_tabstop() {
        // ${1/…/…/} の変換は未対応 — タブストップとして残し本文は壊さない
        let ex = expand("${1/.*/\\u$0/}x", &ctx("/tmp/a.rs"));
        assert_eq!(ex.text, "x");
        assert_eq!(ex.stops[0].index, 1);
        assert!(lint("${1/.*/x/}").iter().any(|m| m.contains("未対応")));
    }

    // ---- try_expand_at / try_expand_ctx ----

    fn test_snippets() -> Vec<Snippet> {
        vec![Snippet {
            name: "For".to_string(),
            prefix: "fo".to_string(),
            body: "for $1 {}$0".to_string(),
            description: String::new(),
            language: "rust".to_string(),
        }]
    }

    #[test]
    fn try_expand_match() {
        let sn = test_snippets();
        let (t, c) = try_expand_at("let fo", 6, &sn, "/tmp/a.rs").unwrap();
        assert_eq!(t, "let for  {}");
        assert_eq!(c, 8); // "let " (4) + "for " (4) => $1 は char 8
    }

    #[test]
    fn try_expand_no_match() {
        let sn = test_snippets();
        assert!(try_expand_at("let foo", 7, &sn, "/tmp/a.rs").is_none());
        assert!(try_expand_at("fo ", 3, &sn, "/tmp/a.rs").is_none()); // 直前が空白
    }

    #[test]
    fn try_expand_prefix_after_japanese() {
        let sn = test_snippets();
        let (t, c) = try_expand_at("値はfo", 4, &sn, "/tmp/a.rs").unwrap();
        assert_eq!(t, "値はfor  {}");
        assert_eq!(c, 6);
    }

    #[test]
    fn try_expand_ctx_returns_absolute_stops() {
        let sn = vec![Snippet {
            name: "M".into(),
            prefix: "m".into(),
            body: "${1:a}-$1-$0".into(),
            description: String::new(),
            language: "rust".into(),
        }];
        let hit = try_expand_ctx("xx m", 4, &sn, &ctx("/tmp/a.rs")).unwrap();
        assert_eq!(hit.start, 3);
        assert_eq!(hit.text, "xx a-a-");
        assert_eq!(hit.cursor, 3);
        assert_eq!(hit.stops[0].ranges, vec![(3, 4), (5, 6)]); // 挿入位置へ写っている
        assert_eq!(hit.stops[1].index, 0);
    }

    // ---- ファイル読み込み ----

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn parse_file_plain_json() {
        let d = unique_temp_dir("zaivern-snippets-test", "plain");
        write(
            &d,
            "rust.json",
            r#"{"Print":{"prefix":"pr","body":["println!(\"$1\");","$0"],"description":"print macro"}}"#,
        );
        let v = parse_file(&d.join("rust.json"), "rust");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "Print");
        assert_eq!(v[0].prefix, "pr");
        assert_eq!(v[0].body, "println!(\"$1\");\n$0"); // 行配列は \n 連結
        assert_eq!(v[0].description, "print macro");
        assert_eq!(v[0].language, "rust");
    }

    #[test]
    fn parse_file_jsonc_with_comments_and_trailing_commas() {
        let d = unique_temp_dir("zaivern-snippets-test", "jsonc");
        write(
            &d,
            "rust.json",
            r#"// file comment
{
    /* block comment */
    "For Loop": {
        "prefix": ["for", "forloop"], // 複数 prefix: 先頭が勝つ
        "body": "for ${1:x} in ${2:xs} { $0 } // see https://example.com",
        "description": "For loop",
    },
}"#,
        );
        let v = parse_file(&d.join("rust.json"), "rust");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].prefix, "for");
        assert!(v[0].body.contains("${1:x}"));
        assert!(v[0].body.contains("https://example.com")); // 文字列内の // は残る
    }

    #[test]
    fn parse_str_ignores_unknown_fields_and_bad_entries() {
        let src = r#"{
            "Good": {"prefix":"g","body":"G","isFileTemplate":true,"weird":{"a":1}},
            "NoPrefix": {"body":"x"},
            "NoBody": {"prefix":"n"},
            "NotAnObject": 42
        }"#;
        let v = parse_str(src, "rust").unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].prefix, "g"); // 未知フィールドは黙って無視
    }

    #[test]
    fn parse_str_scope_overrides_file_language() {
        let v = parse_str(r#"{"S":{"prefix":"s","body":"x","scope":"rust,go"}}"#, "*").unwrap();
        assert_eq!(v[0].language, "rust,go");
    }

    #[test]
    fn parse_corrupt_json_is_readable_error_not_panic() {
        let d = unique_temp_dir("zaivern-snippets-test", "corrupt");
        write(&d, "rust.json", "{ this is not json ");
        let e = parse_file_checked(&d.join("rust.json"), "rust").unwrap_err();
        assert!(e.contains("JSON 構文エラー"), "{e}");
        assert!(parse_file(&d.join("rust.json"), "rust").is_empty()); // 互換 API は空
                                                                      // 最上位が配列 → 別のエラー文
        write(&d, "arr.json", "[1,2,3]");
        let e2 = parse_file_checked(&d.join("arr.json"), "rust").unwrap_err();
        assert!(e2.contains("オブジェクト"), "{e2}");
        // 存在しないファイル
        assert!(parse_file_checked(&d.join("nope.json"), "rust").is_err());
    }

    // ---- SnippetStore ----

    #[test]
    fn store_missing_dir_degrades_to_builtins() {
        let d = unique_temp_dir("zaivern-snippets-test", "missing");
        let mut st = SnippetStore::new(d.join("no-such-dir"));
        st.reload();
        assert!(st.diagnostics().is_empty(), "未作成は警告なし");
        assert_eq!(st.user_count(), 0);
        let rs = st.for_lang("rust");
        assert!(rs.iter().any(|s| s.prefix == "fn")); // 組み込みは残る
        assert!(rs.iter().any(|s| s.prefix == "todo")); // 全言語共通も
    }

    #[test]
    fn store_loads_per_language_and_global_files() {
        let d = unique_temp_dir("zaivern-snippets-test", "load");
        write(&d, "rust.json", r#"{"R":{"prefix":"rr","body":"RUST"}}"#);
        write(
            &d,
            "global.code-snippets",
            r#"{"G":{"prefix":"gg","body":"GLOBAL"}}"#,
        );
        write(&d, "notes.txt", "無視される");
        let mut st = SnippetStore::new(d.clone());
        st.reload();
        assert!(st.diagnostics().is_empty());
        assert_eq!(st.user_count(), 2);
        let rs = st.for_lang("rust");
        assert!(rs.iter().any(|s| s.prefix == "rr"));
        assert!(rs.iter().any(|s| s.prefix == "gg"));
        let py = st.for_lang("python");
        assert!(
            !py.iter().any(|s| s.prefix == "rr"),
            "言語別は他言語へ漏れない"
        );
        assert!(py.iter().any(|s| s.prefix == "gg"), "global は全言語へ");
    }

    #[test]
    fn store_precedence_language_over_global_over_builtin() {
        let d = unique_temp_dir("zaivern-snippets-test", "prec");
        write(&d, "rust.json", r#"{"A":{"prefix":"p","body":"LANG"}}"#);
        write(
            &d,
            "global.json",
            r#"{"B":{"prefix":"p","body":"GLOBAL"},"C":{"prefix":"fn","body":"USERFN"}}"#,
        );
        let mut st = SnippetStore::new(d.clone());
        st.reload();
        let rs = st.for_lang("rust");
        let p: Vec<&Snippet> = rs.iter().filter(|s| s.prefix == "p").collect();
        assert_eq!(p.len(), 1, "同じ prefix は 1 つに畳まれる");
        assert_eq!(p[0].body, "LANG", "言語別がグローバルより強い");
        let f: Vec<&Snippet> = rs.iter().filter(|s| s.prefix == "fn").collect();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].body, "USERFN", "ユーザー定義が組み込みより強い");
    }

    #[test]
    fn store_inherited_language_snippets_apply() {
        let d = unique_temp_dir("zaivern-snippets-test", "inherit");
        write(
            &d,
            "javascript.json",
            r#"{"J":{"prefix":"jj","body":"JS"}}"#,
        );
        write(
            &d,
            "typescript.json",
            r#"{"T":{"prefix":"tt","body":"TS"}}"#,
        );
        let mut st = SnippetStore::new(d.clone());
        st.reload();
        let tsx = st.for_lang("typescriptreact");
        assert!(tsx.iter().any(|s| s.prefix == "tt"));
        assert!(tsx.iter().any(|s| s.prefix == "jj"), "tsx → ts → js を継承");
        let js = st.for_lang("javascript");
        assert!(!js.iter().any(|s| s.prefix == "tt"), "継承は一方向");
    }

    #[test]
    fn store_scope_field_routes_to_languages() {
        let d = unique_temp_dir("zaivern-snippets-test", "scope");
        write(
            &d,
            "mine.code-snippets",
            r#"{"S":{"prefix":"ss","body":"X","scope":"rust,go"}}"#,
        );
        let mut st = SnippetStore::new(d.clone());
        st.reload();
        assert!(st.for_lang("rust").iter().any(|s| s.prefix == "ss"));
        assert!(st.for_lang("go").iter().any(|s| s.prefix == "ss"));
        assert!(!st.for_lang("python").iter().any(|s| s.prefix == "ss"));
    }

    #[test]
    fn store_corrupt_file_is_reported_and_others_survive() {
        let d = unique_temp_dir("zaivern-snippets-test", "broken");
        write(&d, "rust.json", "{ broken");
        write(&d, "python.json", r#"{"P":{"prefix":"pp","body":"PY"}}"#);
        let mut st = SnippetStore::new(d.clone());
        st.reload();
        assert_eq!(st.diagnostics().len(), 1);
        assert!(
            st.diagnostics()[0].contains("rust.json"),
            "{:?}",
            st.diagnostics()
        );
        assert!(st.for_lang("python").iter().any(|s| s.prefix == "pp"));
        // 壊れた言語も組み込みへ落ちるだけ (panic しない)
        assert!(st.for_lang("rust").iter().any(|s| s.prefix == "fn"));
    }

    #[test]
    fn store_reload_picks_up_changes() {
        let d = unique_temp_dir("zaivern-snippets-test", "reload");
        let mut st = SnippetStore::new(d.clone());
        st.reload();
        assert_eq!(st.user_count(), 0);
        write(&d, "rust.json", r#"{"A":{"prefix":"aa","body":"A"}}"#);
        assert_eq!(st.user_count(), 0, "reload するまで反映しない");
        st.reload();
        assert_eq!(st.user_count(), 1);
    }

    #[test]
    fn user_snippets_dir_is_under_zaivern_dir() {
        let p = user_snippets_dir();
        assert!(p.ends_with("snippets"));
        assert_eq!(p.parent(), Some(crate::config::zaivern_dir().as_path()));
    }

    // ---- 言語データ表 ----

    #[test]
    fn lang_table_lookup() {
        assert_eq!(lang_spec("rust").line_comment, "//");
        assert_eq!(lang_spec("python").line_comment, "#");
        assert_eq!(lang_spec("go").indent, (1, true));
        assert_eq!(default_indent("go"), "\t");
        assert_eq!(default_indent("html"), "  ");
        assert_eq!(default_indent("rust"), "    ");
        // 未知の言語はフォールバック
        assert_eq!(lang_spec("no-such-lang").id, "plaintext");
        assert_eq!(default_indent("no-such-lang"), "    ");
    }

    #[test]
    fn lang_table_emmet_gating() {
        for l in [
            "html",
            "xml",
            "javascriptreact",
            "typescriptreact",
            "vue",
            "svelte",
            "php",
        ] {
            assert_eq!(emmet_kind(l), EmmetKind::Markup, "{l} は Emmet(Markup)");
        }
        for l in ["css", "scss", "sass", "less"] {
            assert_eq!(emmet_kind(l), EmmetKind::Style, "{l} は Emmet(Style)");
        }
        for l in [
            "rust",
            "python",
            "markdown",
            "json",
            "javascript",
            "typescript",
            "plaintext",
        ] {
            assert_eq!(emmet_kind(l), EmmetKind::None, "{l} は Emmet 無効");
        }
    }

    #[test]
    fn snippet_langs_inheritance_order() {
        assert_eq!(snippet_langs("rust"), vec!["rust"]);
        assert_eq!(
            snippet_langs("typescript"),
            vec!["typescript", "javascript"]
        );
        assert_eq!(
            snippet_langs("typescriptreact"),
            vec!["typescriptreact", "typescript", "javascript"]
        );
        assert_eq!(snippet_langs("scss"), vec!["scss", "css"]);
        assert_eq!(snippet_langs("vue"), vec!["vue", "html", "javascript"]);
    }

    #[test]
    fn lang_id_for_path_table() {
        let cases: &[(&str, &str)] = &[
            ("/a/b/main.rs", "rust"),
            ("x.tsx", "typescriptreact"),
            ("x.jsx", "javascriptreact"),
            ("/tmp/index.html", "html"),
            ("style.SCSS", "scss"),
            ("Makefile", "makefile"),
            ("Dockerfile.dev", "dockerfile"),
            ("noext", "plaintext"),
            ("a.unknownext", "plaintext"),
        ];
        for (p, want) in cases {
            assert_eq!(lang_id_for_path(p), *want, "{p}");
        }
    }

    #[test]
    fn lang_id_for_common_languages() {
        assert_eq!(lang_id_for("Rust"), "rust");
        assert_eq!(lang_id_for("Python"), "python");
        assert_eq!(lang_id_for("C++"), "cpp");
        assert_eq!(lang_id_for("JavaScript (Babel)"), "javascript");
        assert_eq!(lang_id_for("Bourne Again Shell (bash)"), "shellscript");
        assert_eq!(lang_id_for("Zig"), "plaintext");
        assert_eq!(lang_id_for("rust"), "plaintext"); // 大文字小文字は区別する
    }

    #[test]
    fn every_lang_table_id_is_unique() {
        let mut ids: Vec<&str> = LANGS.iter().map(|l| l.id).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "言語 ID が重複している");
        // lang_id_for が返す ID はすべて表に載っていること
        for name in [
            "Rust",
            "HTML",
            "Vue Component",
            "Svelte",
            "TSX",
            "Graphviz (DOT)",
        ] {
            let id = lang_id_for(name);
            assert!(LANGS.iter().any(|l| l.id == id), "{id} が LANGS に無い");
        }
    }

    // ---- 変数解決の単体 ----

    #[test]
    fn resolve_var_filename_variants() {
        assert_eq!(
            resolve_var("TM_FILENAME_BASE", &ctx("/home/u/.bashrc")),
            Some(".bashrc".to_string()) // 先頭ドットは拡張子扱いしない
        );
        assert_eq!(
            resolve_var("TM_FILENAME_BASE", &ctx("/tmp/Makefile")),
            Some("Makefile".to_string())
        );
        assert_eq!(
            resolve_var("TM_FILENAME_BASE", &ctx("/tmp/a.b.c")),
            Some("a.b".to_string()) // 最後の拡張子だけ落とす
        );
        assert_eq!(resolve_var("TM_NO_SUCH_VAR", &ctx("/tmp/a.rs")), None);
        assert_eq!(resolve_var("", &ctx("/tmp/a.rs")), None);
    }

    #[test]
    fn clock_from_unix_known_instant() {
        let c = Clock::from_unix(T);
        assert_eq!(
            (c.year, c.month, c.day, c.hour, c.minute, c.second, c.weekday),
            (2023, 11, 14, 22, 13, 20, 2)
        );
        let epoch = Clock::from_unix(0);
        assert_eq!(
            (epoch.year, epoch.month, epoch.day, epoch.weekday),
            (1970, 1, 1, 4)
        );
        let leap = Clock::from_unix(951_782_400); // 2000-02-29
        assert_eq!((leap.year, leap.month, leap.day), (2000, 2, 29));
    }

    // ---- Emmet: Markup ----

    fn html_opts() -> EmmetOpts {
        EmmetOpts::for_lang("html", "  ")
    }

    #[test]
    fn emmet_markup_table() {
        // (略記, 期待出力)
        let cases: &[(&str, &str)] = &[
            ("div", "<div></div>"),
            ("div.cls#id", "<div class=\"cls\" id=\"id\"></div>"),
            ("p.a.b", "<p class=\"a b\"></p>"),
            (".foo", "<div class=\"foo\"></div>"),
            ("#main", "<div id=\"main\"></div>"),
            ("a[href=#]{text}", "<a href=\"#\">text</a>"),
            ("a[href=\"/x\" title=\"y\"]", "<a href=\"/x\" title=\"y\"></a>"),
            ("span{こんにちは}", "<span>こんにちは</span>"),
            ("ul>li*3", "<ul>\n  <li></li>\n  <li></li>\n  <li></li>\n</ul>"),
            ("div>p+span", "<div>\n  <p></p>\n  <span></span>\n</div>"),
            ("div>ul>li^p", "<div>\n  <ul>\n    <li></li>\n  </ul>\n  <p></p>\n</div>"),
            ("div>ul>li^^section", "<div>\n  <ul>\n    <li></li>\n  </ul>\n</div>\n<section></section>"),
            ("ul>.foo", "<ul>\n  <li class=\"foo\"></li>\n</ul>"),
            ("table>tr>td", "<table>\n  <tr>\n    <td></td>\n  </tr>\n</table>"),
            ("ul>li.item$*3", "<ul>\n  <li class=\"item1\"></li>\n  <li class=\"item2\"></li>\n  <li class=\"item3\"></li>\n</ul>"),
            ("ul>li.item$$*2", "<ul>\n  <li class=\"item01\"></li>\n  <li class=\"item02\"></li>\n</ul>"),
            ("ul>li*2>a{link$}", "<ul>\n  <li>\n    <a href=\"\">link1</a>\n  </li>\n  <li>\n    <a href=\"\">link2</a>\n  </li>\n</ul>"),
            ("br", "<br>"),
            ("img", "<img src=\"\" alt=\"\">"),
            ("input[type=checkbox]", "<input type=\"checkbox\">"),
            ("div>div>div>span{deep}", "<div>\n  <div>\n    <div>\n      <span>deep</span>\n    </div>\n  </div>\n</div>"),
        ];
        for (abbr, want) in cases {
            assert_eq!(
                expand_emmet(abbr, &html_opts()).as_deref(),
                Some(*want),
                "略記: {abbr}"
            );
        }
    }

    #[test]
    fn emmet_html_boilerplate() {
        let out = expand_emmet("!", &html_opts()).unwrap();
        assert!(out.starts_with("<!DOCTYPE html>"));
        assert!(out.contains("  <meta charset=\"UTF-8\">"), "{out}");
        assert!(out.ends_with("</html>"));
        assert!(expand_emmet("link:css", &html_opts())
            .unwrap()
            .contains("stylesheet"));
    }

    #[test]
    fn emmet_indent_unit_follows_request() {
        let tabs = EmmetOpts {
            indent: "\t".into(),
            ..html_opts()
        };
        assert_eq!(
            expand_emmet("ul>li", &tabs).unwrap(),
            "<ul>\n\t<li></li>\n</ul>"
        );
        let four = EmmetOpts {
            indent: "    ".into(),
            ..html_opts()
        };
        assert_eq!(
            expand_emmet("ul>li", &four).unwrap(),
            "<ul>\n    <li></li>\n</ul>"
        );
        // 深いネストでも要求単位のまま積み上がる
        assert_eq!(
            expand_emmet("a>b>i", &four).unwrap(),
            "<a href=\"\">\n    <b>\n        <i></i>\n    </b>\n</a>"
        );
    }

    #[test]
    fn emmet_self_closing_style_follows_language() {
        assert_eq!(
            expand_emmet("br", &EmmetOpts::for_lang("html", "  ")).unwrap(),
            "<br>"
        );
        assert_eq!(
            expand_emmet("br", &EmmetOpts::for_lang("xml", "  ")).unwrap(),
            "<br />"
        );
        assert_eq!(
            expand_emmet("br", &EmmetOpts::for_lang("typescriptreact", "  ")).unwrap(),
            "<br />"
        );
    }

    #[test]
    fn emmet_parent_tag_drives_implicit_tag() {
        let o = html_opts().with_parent("ul");
        assert_eq!(expand_emmet(".foo", &o).unwrap(), "<li class=\"foo\"></li>");
        let o2 = html_opts().with_parent("tr");
        assert_eq!(expand_emmet(".c", &o2).unwrap(), "<td class=\"c\"></td>");
        // 未知の親なら div
        assert_eq!(
            expand_emmet(".c", &html_opts().with_parent("main")).unwrap(),
            "<div class=\"c\"></div>"
        );
    }

    #[test]
    fn emmet_repeat_zero_and_huge_counts_are_capped() {
        assert_eq!(
            expand_emmet("ul>li*0", &html_opts()).unwrap(),
            "<ul>\n</ul>"
        );
        let big = expand_emmet("li*99999", &html_opts()).unwrap();
        assert_eq!(big.lines().count(), MAX_REPEAT, "繰り返しは上限で頭打ち");
    }

    #[test]
    fn emmet_unparseable_returns_none() {
        let bad = [
            "",
            "   ",
            "hello world",   // 空白入りの散文
            "div>",          // 演算子で終わる
            "div..",         // 空のクラス名
            "div#",          // 空の id
            "ul>li*",        // 個数がない
            "a[href",        // ] が来ない
            "span{unclosed", // } が来ない
            "こんにちは",    // 非 ASCII の語
            "foo",           // 未知の単独タグ (散文保護)
            "div>>p",        // 演算子の連続
            "*3",            // 要素がない
        ];
        for b in bad {
            assert_eq!(expand_emmet(b, &html_opts()), None, "略記: {b:?}");
        }
        // 長すぎる入力も拒否
        assert_eq!(expand_emmet(&"div>".repeat(200), &html_opts()), None);
    }

    #[test]
    fn emmet_unknown_tag_allowed_only_in_compound_abbr() {
        // 単独の未知タグは None、構造があればカスタム要素として通す
        assert_eq!(expand_emmet("my-widget", &html_opts()), None);
        assert_eq!(
            expand_emmet("my-widget>div", &html_opts()).unwrap(),
            "<my-widget>\n  <div></div>\n</my-widget>"
        );
        assert_eq!(
            expand_emmet("my-widget.x", &html_opts()).unwrap(),
            "<my-widget class=\"x\"></my-widget>"
        );
    }

    #[test]
    fn emmet_escaped_dollar_stays_literal() {
        assert_eq!(
            expand_emmet(r"span{price \$5}", &html_opts()).unwrap(),
            "<span>price $5</span>"
        );
    }

    #[test]
    fn emmet_is_deterministic() {
        let a = expand_emmet("ul>li.item$*3>a[href=#]{x$}", &html_opts());
        let b = expand_emmet("ul>li.item$*3>a[href=#]{x$}", &html_opts());
        assert_eq!(a, b);
    }

    // ---- Emmet: CSS ----

    #[test]
    fn emmet_css_table() {
        let o = EmmetOpts::for_lang("css", "  ");
        let cases: &[(&str, &str)] = &[
            ("m10", "margin: 10px;"),
            ("m0", "margin: 0;"),
            ("m-10", "margin: -10px;"),
            ("p10-20", "padding: 10px 20px;"),
            ("w100p", "width: 100%;"),
            ("fz1.5e", "font-size: 1.5em;"),
            ("mah50r", "max-height: 50rem;"),
            ("df", "display: flex;"),
            ("posa", "position: absolute;"),
            ("bdrs4", "border-radius: 4px;"),
            ("m10+p5", "margin: 10px;\npadding: 5px;"),
        ];
        for (abbr, want) in cases {
            assert_eq!(
                expand_emmet(abbr, &o).as_deref(),
                Some(*want),
                "略記: {abbr}"
            );
        }
        for bad in ["zzz9", "m", "10", "m10-", "mq!!", ""] {
            assert_eq!(expand_emmet(bad, &o), None, "略記: {bad:?}");
        }
    }

    #[test]
    fn emmet_disabled_language_returns_none() {
        for lang in ["rust", "python", "markdown", "plaintext"] {
            let o = EmmetOpts::for_lang(lang, "    ");
            assert_eq!(expand_emmet("ul>li*3", &o), None, "{lang}");
            assert_eq!(try_emmet_at("ul>li*3", 7, lang, "    "), None, "{lang}");
        }
    }

    // ---- Emmet: キャレット位置での展開 ----

    #[test]
    fn try_emmet_at_uses_line_indent_and_finds_abbr() {
        let hit = try_emmet_at("  ul>li*2", 9, "html", "  ").unwrap();
        assert_eq!(hit.start, 2, "行頭の空白は置換しない");
        assert_eq!(hit.text, "<ul>\n    <li></li>\n    <li></li>\n  </ul>");
        // キャレットは最初の空要素の内側
        let cs: Vec<char> = hit.text.chars().collect();
        assert_eq!(cs[hit.cursor], '<');
        assert_eq!(cs[hit.cursor - 1], '>');
    }

    #[test]
    fn try_emmet_at_picks_last_token() {
        let hit = try_emmet_at("text div.a", 10, "html", "  ").unwrap();
        assert_eq!(hit.start, 5);
        assert_eq!(hit.text, "<div class=\"a\"></div>");
        // 空白直後の Tab では展開しない
        assert_eq!(try_emmet_at("div ", 4, "html", "  "), None);
        assert_eq!(try_emmet_at("", 0, "html", "  "), None);
    }

    #[test]
    fn try_emmet_at_css_language() {
        let hit = try_emmet_at("  m10", 5, "css", "  ").unwrap();
        assert_eq!(hit.start, 2);
        assert_eq!(hit.text, "margin: 10px;");
    }

    // ---- 組み込みスニペット ----

    #[test]
    fn builtins_expand_without_panic_and_are_lint_clean() {
        for b in BUILTINS {
            let lang = if b.lang == "*" { "rust" } else { b.lang };
            let c = ExpandCtx::for_path("/tmp/x").with_language(lang);
            let ex = expand(b.body, &c);
            assert!(!ex.text.is_empty(), "空展開: {}", b.prefix);
            assert!(
                lint(b.body).is_empty(),
                "警告: {} {:?}",
                b.prefix,
                lint(b.body)
            );
        }
        // 全言語共通スニペットは言語のコメント記号に追従する
        let c = ExpandCtx::for_path("/tmp/a.py");
        let todo = BUILTINS.iter().find(|b| b.prefix == "todo").unwrap();
        assert!(expand(todo.body, &c).text.starts_with("# TODO"));
    }

    #[test]
    fn builtin_prefixes_are_unique_per_language() {
        for lang in [
            "rust",
            "javascript",
            "typescript",
            "python",
            "go",
            "markdown",
            "html",
            "css",
        ] {
            let v = builtin_for_lang(lang);
            let mut p: Vec<&str> = v.iter().map(|s| s.prefix.as_str()).collect();
            p.sort_unstable();
            let n = p.len();
            p.dedup();
            assert_eq!(p.len(), n, "{lang} で prefix が重複");
        }
    }
}
