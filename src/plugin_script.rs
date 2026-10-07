//! プラグインのスクリプトが呼ぶ道具 (`zai plugin emit` / `json` / `usage-scan`)。
//!
//! # なぜ Rust に置くのか
//!
//! 同梱プラグインは JSON の組み立てと読み取りを **`python3`** に頼っていた。
//! これが 2 つの意味で壊れていた:
//!
//! 1. **Windows には python3 が居ない** — しかも居ないと言ってくれない。
//!    Microsoft Store の「アプリ実行エイリアス」が `python3.exe` として
//!    PATH (`%LOCALAPPDATA%\Microsoft\WindowsApps`) に**実在する**ので
//!    `command -v python3` は成功し、実行すると `Python` の 1 行を出して
//!    **exit 49** で終わる。存在チェックを通り抜けて失敗する、いちばん
//!    質の悪い壊れ方 (実測: Windows 11 / Git for Windows 環境)
//! 2. リポジトリの約束 (`CLAUDE.md`) に反する — 「道具は Rust に置く。
//!    Python は置かない」。第 2 の実行環境が増えると、どの python か・
//!    依存はあるか・Windows で動くかを全部背負うことになる
//!
//! そこで**プラグインが python3 を呼んでいた箇所だけ**を `zai` の
//! サブコマンドへ移した。スクリプトは `"$ZV_BIN" plugin <...>` と書く
//! (`ZV_BIN` は仕様 3 章の環境変数で、実行中の `zai` の実体を指す)。
//!
//! # 入口
//!
//! | サブコマンド | 役目 | 置き換えた python |
//! |---|---|---|
//! | `zai plugin emit <キー> <値> …` | アクション 1 行 (JSON Lines) を出す | 4 つの `common.sh` の `zv_emit` |
//! | `zai plugin json <ファイル> keys\|get\|rows …` | JSON の読み取り | `detect.sh` / `gh-common.sh` / `issue-branch.sh` |
//! | `zai plugin usage-scan [追加ディレクトリ]` | 使用量の目安を Markdown で出す | `usage-meter/scripts/scan.py` |

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// 画面に出る文字列は **日本語の原文を鍵**にして引く (ID ではなく)。
// `zai` の CLI 経路は `cli::init_cli_locale` が「原文言語 (日本語) なら
// 辞書を読まない」ので、**ID を渡すと日本語の利用者には ID がそのまま出る**。
// ここは GUI ではなく `zai plugin usage-scan` として子プロセスで動く層なので、
// 既存 3300 箇所と同じ原文キー方式にしてある (訳は `locales/*.json` の
// `plugin.usage.*` にあり、日本語以外では逆引きで当たる)。
use crate::i18n::{tr, trf};

// ───────────────────────── emit (JSON Lines) ─────────────────────────

/// `zai plugin emit <キー> <値> [<キー> <値> …]` — アクション 1 行を組み立てる。
///
/// 値が `@@` で始まるときは、続くパスの**ファイル内容**を値にする
/// (パネル本文のように長い値を、引数の長さ制限を避けて渡すため)。
/// キーの型は仕様に合わせて寄せる: `submit` は真偽、`line` / `column` は整数、
/// それ以外は文字列。キーが奇数個余ったときは、余りを捨てる (python 版と同じ)。
pub fn emit(args: &[String]) -> Result<String, String> {
    if args.is_empty() {
        return Err(
            "使い方: zai plugin emit <キー> <値> [<キー> <値> …]  (例: emit action notify level info message 完了)"
                .into(),
        );
    }
    let mut obj = serde_json::Map::new();
    for pair in args.chunks(2) {
        if pair.len() < 2 {
            break; // 余った 1 個は捨てる
        }
        let (key, raw) = (pair[0].trim(), pair[1].as_str());
        if key.is_empty() {
            continue;
        }
        let val = match raw.strip_prefix("@@") {
            Some(path) => {
                std::fs::read_to_string(path).map_err(|e| format!("{path} を読めません: {e}"))?
            }
            None => raw.to_string(),
        };
        obj.insert(key.to_string(), typed_value(key, &val));
    }
    serde_json::to_string(&serde_json::Value::Object(obj))
        .map_err(|e| format!("JSON にできません: {e}"))
}

/// キー名から値の型を決める (仕様 4 章のアクション定義に合わせる)。
fn typed_value(key: &str, val: &str) -> serde_json::Value {
    match key {
        "submit" => {
            let t = val.trim().to_ascii_lowercase();
            serde_json::Value::Bool(matches!(t.as_str(), "1" | "true" | "yes"))
        }
        "line" | "column" => match val.trim().parse::<i64>() {
            Ok(n) => serde_json::Value::from(n),
            // 数えられない値を 0 に化かすと「1 行目へ飛ぶ」等の嘘になるので、
            // 文字列のまま渡す (受け側 parse_action_line は文字列も解する)。
            Err(_) => serde_json::Value::String(val.to_string()),
        },
        _ => serde_json::Value::String(val.to_string()),
    }
}

// ───────────────────────── json (読み取り) ─────────────────────────

/// `zai plugin json <ファイル> <モード> [パス …]` — JSON を読む最小の道具。
///
/// * `keys <パス>` — そこにあるオブジェクトのキーを 1 行 1 件
/// * `get <パス>` — そこにある値を 1 つ (無ければ空。改行は空白へ畳む)
/// * `rows <パス> …` — 配列の各要素から、指定パスの値を TSV で 1 行ずつ
///
/// パスは `.` 区切り。`labels[].name` のように `[]` を挟むと配列を辿って
/// `, ` で連結する。読めないファイル・無いパスは**空を返して成功する**
/// (プラグイン側は `set -eu` で走っているので、無い物を探しただけで
/// スクリプト全体を落とさない)。
pub fn json_query(args: &[String]) -> Result<String, String> {
    let file = args
        .first()
        .ok_or("使い方: zai plugin json <ファイル> keys|get|rows <パス> …")?;
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let paths: Vec<&str> = args.iter().skip(2).map(|s| s.as_str()).collect();
    let Ok(text) = std::fs::read_to_string(file) else {
        return Ok(String::new()); // 無いファイルは「空」として扱う
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(String::new()); // 壊れた JSON も「空」
    };
    match mode {
        "keys" => {
            let at = dig(&v, paths.first().copied().unwrap_or(""));
            Ok(match at {
                Some(serde_json::Value::Object(o)) => {
                    o.keys().cloned().collect::<Vec<_>>().join("\n")
                }
                _ => String::new(),
            })
        }
        "get" => Ok(dig(&v, paths.first().copied().unwrap_or(""))
            .map(flatten_scalar)
            .unwrap_or_default()),
        "rows" => {
            if paths.is_empty() {
                return Err("rows には取り出すパスを 1 つ以上指定してください".into());
            }
            let empty = Vec::new();
            let rows = v.as_array().unwrap_or(&empty);
            Ok(rows
                .iter()
                .map(|row| {
                    paths
                        .iter()
                        .map(|p| dig(row, p).map(flatten_scalar).unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("\t")
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "" => Err("モードを指定してください: keys / get / rows".into()),
        other => Err(format!("不明な json モードです: {other}")),
    }
}

/// `a.b[].c` 形式のパスで値を辿る。`[]` は「配列を辿って `, ` で連結する」印。
fn dig(v: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let path = path.trim();
    if path.is_empty() {
        return Some(v.clone());
    }
    let (seg, rest) = match path.split_once('.') {
        Some((a, b)) => (a, b),
        None => (path, ""),
    };
    let (name, spread) = match seg.strip_suffix("[]") {
        Some(n) => (n, true),
        None => (seg, false),
    };
    let here = if name.is_empty() {
        v.clone()
    } else {
        v.get(name)?.clone()
    };
    if spread {
        let items: Vec<String> = here
            .as_array()?
            .iter()
            .filter_map(|it| dig(it, rest).map(flatten_scalar))
            .filter(|s| !s.is_empty())
            .collect();
        return Some(serde_json::Value::String(items.join(", ")));
    }
    if rest.is_empty() {
        Some(here)
    } else {
        dig(&here, rest)
    }
}

/// 値を 1 行のテキストへ畳む (TSV / 行指向の出力へ載せるため)。
fn flatten_scalar(v: serde_json::Value) -> String {
    let s = match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s,
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    };
    // タブと改行は区切りを壊すので空白へ寄せる
    s.replace(['\t', '\r', '\n'], " ").trim().to_string()
}

// ───────────────────────── usage-scan (使用量の目安) ─────────────────────────

/// 利用量とみなす数値キー (見つかった分だけ合算する)。
const TOKEN_KEYS: &[&str] = &[
    "input_tokens",
    "output_tokens",
    "total_tokens",
    "prompt_tokens",
    "completion_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
];
const COST_KEYS: &[&str] = &["cost", "cost_usd", "total_cost", "total_cost_usd"];
/// 会話・セッション記録が置かれがちなディレクトリ名。製品固有の名前は含めない。
const SESSION_DIR_HINTS: &[&str] = &[
    "sessions",
    "session",
    "history",
    "conversations",
    "conversation",
    "chats",
    "chat",
    "projects",
    "transcripts",
    "threads",
    "usage",
];
const SKIP_DIR_NAMES: &[&str] = &[
    "node_modules",
    ".git",
    "cache",
    "Cache",
    "tmp",
    "bin",
    "lib",
];

const MAX_FILES_PER_DIR: usize = 400;
const MAX_PARSE_FILES: usize = 60;
const MAX_BYTES_PER_FILE: u64 = 2 * 1024 * 1024;
const MAX_DEPTH: usize = 4;
const MAX_DIRS_PER_ROOT: usize = 600;
const MAX_ARRAY_ITEMS: usize = 200;
const MAX_JSON_DEPTH: u32 = 8;
const MAX_REPORTS: usize = 12;

/// 走査で見つけたセッション記録ファイル 1 件。
struct Found {
    size: u64,
    mtime: std::time::SystemTime,
    path: PathBuf,
}

/// 記録元 1 つぶんの集計。
struct Report {
    name: String,
    path: String,
    sessions: usize,
    bytes: u64,
    tokens: BTreeMap<String, f64>,
    costs: BTreeMap<String, f64>,
    parsed: usize,
}

/// `zai plugin usage-scan [追加ディレクトリ(コンマ区切り)]`
///
/// ローカルに残っているエージェントのセッション記録を走査し、使用量の目安を
/// Markdown で出す。**スキーマは決め打ちにしない** — ホーム直下の隠し
/// ディレクトリのうち会話記録らしい構造を持つものを探し、その中に現れる
/// 利用量らしき数値キーだけを拾う。見つからない項目は必ず「取得不可」と書き、
/// 値を作らない (推定した数字は、無いより悪い)。
pub fn usage_scan(args: &[String]) -> Result<String, String> {
    let extra: Vec<String> = args
        .first()
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let home = dirs::home_dir().unwrap_or_default();
    let mut reports: Vec<Report> = Vec::new();
    for (name, dirs) in discover_roots(&home, &extra) {
        let mut files: Vec<Found> = Vec::new();
        for one in &dirs {
            files.extend(walk_sessions(one));
        }
        if files.is_empty() {
            continue;
        }
        let (tokens, costs, parsed) = parse_usage(&mut files);
        if tokens.is_empty() && costs.is_empty() && files.len() < 3 {
            continue;
        }
        reports.push(Report {
            name,
            path: dirs
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            sessions: files.len(),
            bytes: files.iter().map(|f| f.size).sum(),
            tokens,
            costs,
            parsed,
        });
    }
    Ok(render_usage(&mut reports))
}

/// 集計結果を Markdown へ (表示だけを担う — 走査とは分けてテストできるように)。
fn render_usage(reports: &mut [Report]) -> String {
    let mut out: Vec<String> = vec![format!("## {}", tr("使用量の目安")), String::new()];
    if reports.is_empty() {
        out.push(tr(
            "ローカルにセッション記録が見つかりませんでした: **取得不可**",
        ));
        out.push(String::new());
        out.push(tr(
            "設定「追加の探索ディレクトリ」に記録の置き場所を指定すると集計できます。",
        ));
        return out.join("\n") + "\n";
    }
    reports.sort_by(|a, b| b.sessions.cmp(&a.sessions));
    let na = tr("取得不可");
    out.push(format!(
        "| {} | {} | {} | {} | {} | {} |",
        tr("記録元"),
        tr("セッション数"),
        tr("記録サイズ"),
        tr("入力トークン"),
        tr("出力トークン"),
        tr("費用"),
    ));
    out.push("| --- | ---: | ---: | ---: | ---: | ---: |".to_string());
    for rep in reports.iter().take(MAX_REPORTS) {
        let pick = |a: &str, b: &str| rep.tokens.get(a).or_else(|| rep.tokens.get(b)).copied();
        let cost: f64 = rep.costs.values().sum();
        out.push(format!(
            "| {} | {} | {} | {} | {} | {} |",
            rep.name,
            rep.sessions,
            human(rep.bytes),
            pick("input_tokens", "prompt_tokens").map_or(na.clone(), |n| thousands(n as i64)),
            pick("output_tokens", "completion_tokens").map_or(na.clone(), |n| thousands(n as i64)),
            if rep.costs.is_empty() {
                na.clone()
            } else {
                format!("${cost:.2}")
            },
        ));
    }
    let total_sessions: usize = reports.iter().map(|r| r.sessions).sum();
    let total_bytes: u64 = reports.iter().map(|r| r.bytes).sum();
    out.push(String::new());
    out.push(trf(
        "合計 {sessions} セッション / {size}",
        &[
            ("sessions", total_sessions.to_string()),
            ("size", human(total_bytes)),
        ],
    ));
    out.push(String::new());
    out.push(format!("### {}", tr("内訳")));
    for rep in reports.iter().take(MAX_REPORTS) {
        let mut detail: Vec<String> = Vec::new();
        for k in TOKEN_KEYS {
            if let Some(v) = rep.tokens.get(*k) {
                detail.push(format!("{k}={}", thousands(*v as i64)));
            }
        }
        for (k, v) in &rep.costs {
            detail.push(format!("{k}={v:.4}"));
        }
        out.push(format!(
            "- **{}** (`{}`) — {}: {}",
            rep.name,
            rep.path,
            trf(
                "解析 {files} ファイル",
                &[("files", rep.parsed.to_string())]
            ),
            if detail.is_empty() {
                tr("利用量の数値は 取得不可")
            } else {
                detail.join(", ")
            },
        ));
    }
    out.push(String::new());
    out.push(tr(
        "※ 数値は記録ファイルに実際に書かれていた値のみを合算しています。",
    ));
    out.push(tr(
        "※ 記録が無い項目は「取得不可」と表示し、推定は行いません。",
    ));
    out.join("\n") + "\n"
}

/// 記録の置き場所を探す。設定で指定された場所はそのまま、ホーム直下の隠し
/// ディレクトリは会話記録らしい構造を持つものだけを対象にする。
fn discover_roots(home: &Path, extra: &[String]) -> Vec<(String, Vec<PathBuf>)> {
    let mut roots: Vec<(String, Vec<PathBuf>)> = Vec::new();
    for entry in extra {
        let p = expand_home(entry, home);
        if p.is_dir() {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| p.display().to_string());
            roots.push((name, vec![p]));
        }
    }
    let Ok(rd) = std::fs::read_dir(home) else {
        return roots;
    };
    let mut names: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    names.sort();
    for path in names {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string());
        let Some(name) = name.filter(|n| n.starts_with('.')) else {
            continue;
        };
        // symlink を辿ると、同じ記録を 2 度数えたり外へ出たりする
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !md.is_dir() {
            continue;
        }
        let hits = session_store_hits(&path);
        if !hits.is_empty() {
            roots.push((name, hits));
        }
    }
    roots
}

/// 先頭の `~` をホームへ広げる (プラグイン設定へ `~/...` と書けるように)。
fn expand_home(raw: &str, home: &Path) -> PathBuf {
    let raw = raw.trim();
    match raw.strip_prefix('~') {
        Some(rest) => home.join(rest.trim_start_matches(['/', '\\'])),
        None => PathBuf::from(raw),
    }
}

/// 直下に会話記録らしいディレクトリがあるか (あればそれらを返す)。
fn session_store_hits(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut hits: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            SESSION_DIR_HINTS.contains(&n.as_str()) && e.path().is_dir()
        })
        .map(|e| e.path())
        .collect();
    hits.sort();
    hits
}

/// セッションらしいファイルを集める。深さと訪問ディレクトリ数に上限を設け、
/// 巨大なキャッシュを掘り続けないようにする。
fn walk_sessions(root: &Path) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    let mut queue: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = queue.pop() {
        visited += 1;
        if visited > MAX_DIRS_PER_ROOT || found.len() >= MAX_FILES_PER_DIR {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.filter_map(|e| e.ok()) {
            let path = e.path();
            // symlink は辿らない (旧 scan.py の os.walk は followlinks=False)。
            // DirEntry::file_type はリンクそのものを見るので、リンク先の
            // metadata で directory と誤判定して外へ出ることはない。
            let Ok(ft) = e.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                let name = e.file_name().to_string_lossy().to_string();
                if depth < MAX_DEPTH && !SKIP_DIR_NAMES.contains(&name.as_str()) {
                    queue.push((path, depth + 1));
                }
                continue;
            }
            let is_session = path
                .extension()
                .map(|x| {
                    let x = x.to_string_lossy().to_lowercase();
                    x == "jsonl" || x == "json"
                })
                .unwrap_or(false);
            if !is_session {
                continue;
            }
            // ここに来るのは実ファイルだけ (symlink は上で除いた)。
            let Ok(md) = e.metadata() else {
                continue;
            };
            found.push(Found {
                size: md.len(),
                mtime: md.modified().unwrap_or(std::time::UNIX_EPOCH),
                path,
            });
            if found.len() >= MAX_FILES_PER_DIR {
                break;
            }
        }
    }
    found
}

/// 新しい記録から順に開いて、利用量らしき数値だけを合算する。
fn parse_usage(files: &mut [Found]) -> (BTreeMap<String, f64>, BTreeMap<String, f64>, usize) {
    let mut tokens: BTreeMap<String, f64> = BTreeMap::new();
    let mut costs: BTreeMap<String, f64> = BTreeMap::new();
    let mut parsed = 0usize;
    files.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    for f in files.iter() {
        if parsed >= MAX_PARSE_FILES {
            break;
        }
        if f.size == 0 || f.size > MAX_BYTES_PER_FILE {
            continue;
        }
        let Ok(bytes) = std::fs::read(&f.path) else {
            continue;
        };
        parsed += 1;
        // 旧 scan.py は errors="replace" で読んでいた。1 バイトでも不正
        // UTF-8 があると read_to_string ではファイルごと捨てるので、
        // 置換文字に直して正常な行を救う。
        let text = String::from_utf8_lossy(&bytes);
        let stripped = text.trim_start();
        // 1 ファイル 1 JSON (整形済み) か、JSON Lines かを内容で見分ける
        if stripped.starts_with('{') && !stripped.contains("\n{") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                collect_numbers(&v, &mut tokens, &mut costs, 0);
            }
            continue;
        }
        for line in text.lines() {
            let line = line.trim();
            if !line.starts_with('{') {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                collect_numbers(&v, &mut tokens, &mut costs, 0);
            }
        }
    }
    (tokens, costs, parsed)
}

/// 入れ子の JSON を辿り、利用量らしき数値キーだけを拾う。
fn collect_numbers(
    v: &serde_json::Value,
    tokens: &mut BTreeMap<String, f64>,
    costs: &mut BTreeMap<String, f64>,
    depth: u32,
) {
    if depth > MAX_JSON_DEPTH {
        return;
    }
    match v {
        serde_json::Value::Object(o) => {
            for (k, val) in o {
                let lowered = k.to_lowercase();
                match val.as_f64() {
                    Some(n) if TOKEN_KEYS.contains(&lowered.as_str()) => {
                        *tokens.entry(lowered).or_insert(0.0) += n;
                    }
                    Some(n) if COST_KEYS.contains(&lowered.as_str()) => {
                        *costs.entry(lowered).or_insert(0.0) += n;
                    }
                    _ => collect_numbers(val, tokens, costs, depth + 1),
                }
            }
        }
        serde_json::Value::Array(a) => {
            for it in a.iter().take(MAX_ARRAY_ITEMS) {
                collect_numbers(it, tokens, costs, depth + 1);
            }
        }
        _ => {}
    }
}

/// バイト数を人が読める形へ (1 桁小数、GB 止め)。
fn human(bytes: u64) -> String {
    let mut size = bytes as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if size < 1024.0 || unit == "GB" {
            return format!("{size:.1} {unit}");
        }
        size /= 1024.0;
    }
    format!("{size:.1} GB")
}

/// 3 桁区切り (python の `"{:,}".format` と同じ見え方にする)。
fn thousands(n: i64) -> String {
    let neg = n < 0;
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// python 版 `zv_emit` と同じ JSON を出すこと (受け側 `parse_actions` が
    /// 解せる形 — `submit` は真偽、`line` は数値)。
    #[test]
    fn emitは型を仕様に合わせる() {
        let args: Vec<String> = ["action", "agent_prompt", "text", "やって", "submit", "yes"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let line = emit(&args).expect("組める");
        let acts = crate::plugins::parse_actions(&line);
        assert_eq!(acts.len(), 1, "1 行 1 アクション: {line}");
        match &acts[0] {
            crate::plugins::PluginAction::AgentPrompt { text, submit, .. } => {
                assert_eq!(text, "やって", "非 ASCII を \\u で潰さない");
                assert!(*submit, "submit yes は真");
            }
            other => panic!("agent_prompt にならない: {other:?}"),
        }
        // line は数値として載る (文字列だと行番号として扱えない受け側がある)
        let l = emit(&[
            "action".into(),
            "open_file".into(),
            "path".into(),
            "a.rs".into(),
            "line".into(),
            "42".into(),
        ])
        .expect("組める");
        assert!(l.contains("\"line\":42"), "数値で載っていない: {l}");
    }

    /// `@@パス` はファイル内容を値にする (長いパネル本文を引数長の制限を
    /// 避けて渡すための仕様)。
    #[test]
    fn emitはアットマーク2つでファイルを読む() {
        let dir = crate::test_util::unique_temp_dir("zaivern-plugin-script", "emit");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let body = dir.join("panel.md");
        std::fs::write(&body, "## 見出し\n本文\n").expect("write");
        let line = emit(&[
            "action".into(),
            "set_panel".into(),
            "panel".into(),
            "runner".into(),
            "text".into(),
            format!("@@{}", body.display()),
        ])
        .expect("組める");
        match &crate::plugins::parse_actions(&line)[0] {
            crate::plugins::PluginAction::SetPanel { panel, text } => {
                assert_eq!(panel, "runner");
                assert!(text.contains("## 見出し"), "本文が入っていない: {text}");
            }
            other => panic!("set_panel にならない: {other:?}"),
        }
        // 読めないパスは失敗として返す (黙って空の本文を出すと、パネルが
        // 何も言わずに空になり原因が分からない)
        assert!(emit(&["text".into(), "@@/no/such/file".into()]).is_err());
    }

    /// `json keys` は package.json の scripts 一覧 (detect.sh が使う形)。
    /// 無いファイル・壊れた JSON・無いパスは**空で成功**すること —
    /// `set -eu` のスクリプトを、探しただけで落とさないため。
    #[test]
    fn jsonは読めないものを空で返す() {
        let dir = crate::test_util::unique_temp_dir("zaivern-plugin-script", "json");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let pkg = dir.join("package.json");
        std::fs::write(
            &pkg,
            r#"{"scripts":{"dev":"vite","test":"vitest"},"name":"x"}"#,
        )
        .expect("write");
        let q = |file: &Path, a: &[&str]| {
            let mut v = vec![file.display().to_string()];
            v.extend(a.iter().map(|s| s.to_string()));
            json_query(&v).expect("成功する")
        };
        assert_eq!(q(&pkg, &["keys", "scripts"]), "dev\ntest");
        assert_eq!(q(&pkg, &["get", "name"]), "x");
        assert_eq!(q(&pkg, &["keys", "nope"]), "", "無いパスは空");
        assert_eq!(q(Path::new("/no/such.json"), &["keys", "scripts"]), "");
        let broken = dir.join("broken.json");
        std::fs::write(&broken, "{ not json").expect("write");
        assert_eq!(q(&broken, &["keys", "scripts"]), "");
    }

    /// `json rows` は gh の JSON 配列を TSV へ (gh-common.sh が使う形)。
    /// `labels[].name` の連結と、タブ/改行の畳み込みまで見る。
    #[test]
    fn jsonrowsはgh配列をtsvにする() {
        let dir = crate::test_util::unique_temp_dir("zaivern-plugin-script", "rows");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let f = dir.join("prs.json");
        std::fs::write(
            &f,
            r#"[{"number":7,"title":"直す\tやつ","author":{"login":"octocat"},
                 "labels":[{"name":"bug"},{"name":"win"}],"state":"OPEN","headRefName":"fix/x"},
                {"number":8,"title":"もう 1 つ","author":{},"labels":[],"state":"MERGED"}]"#,
        )
        .expect("write");
        let out = json_query(&[
            f.display().to_string(),
            "rows".into(),
            "number".into(),
            "title".into(),
            "author.login".into(),
            "labels[].name".into(),
            "state".into(),
            "headRefName".into(),
        ])
        .expect("成功する");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0], "7\t直す やつ\toctocat\tbug, win\tOPEN\tfix/x",
            "タブは空白へ畳み、labels は連結する"
        );
        assert_eq!(
            lines[1], "8\tもう 1 つ\t\t\tMERGED\t",
            "無い値は空欄で、列数は揃える"
        );
    }

    /// 表示の作りを固定する: 記録が無ければ数字を作らず「取得不可」と言う。
    #[test]
    fn usage表示は無いものを取得不可と書く() {
        let mut none: Vec<Report> = Vec::new();
        let out = render_usage(&mut none);
        assert!(
            out.contains("取得不可"),
            "無いものを取得不可と書いていない: {out}"
        );
        assert!(out.starts_with("## "), "見出しから始まる: {out}");
        // 数字を捏造していない (合計行も内訳も出さない)
        assert!(
            !out.contains("###"),
            "記録が無いのに内訳を出している: {out}"
        );
    }

    /// 合算した数値と件数が、そのまま表に出ること。
    #[test]
    fn usage表示は拾えた数値だけを出す() {
        let mut reps = vec![Report {
            name: ".claude".into(),
            path: "/h/.claude/projects".into(),
            sessions: 3,
            bytes: 2048,
            tokens: BTreeMap::from([
                ("input_tokens".to_string(), 1234567.0),
                ("output_tokens".to_string(), 8901.0),
            ]),
            costs: BTreeMap::from([("cost_usd".to_string(), 1.5)]),
            parsed: 2,
        }];
        let out = render_usage(&mut reps);
        assert!(out.contains("1,234,567"), "3 桁区切りで出ない: {out}");
        assert!(out.contains("8,901"), "{out}");
        assert!(out.contains("$1.50"), "費用が出ない: {out}");
        assert!(out.contains("2.0 KB"), "サイズが人向けでない: {out}");
        assert!(out.contains("cost_usd=1.5000"), "内訳が出ない: {out}");
    }

    #[test]
    fn humanとthousandsの境目() {
        assert_eq!(human(0), "0.0 B");
        assert_eq!(human(1023), "1023.0 B");
        assert_eq!(human(1024), "1.0 KB");
        assert_eq!(human(1024 * 1024), "1.0 MB");
        // GB で止める (TB を作らない = python 版と同じ)
        assert!(human(5 * 1024 * 1024 * 1024 * 1024).ends_with(" GB"));
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(-1234567), "-1,234,567");
    }

    /// 走査は深さと件数の上限を守る (ホーム全体を掘り続けない)。
    /// ルートを深さ 0 とし、深さ MAX_DEPTH のディレクトリにあるファイルまで
    /// 拾い、それより深いものは拾わない (旧 scan.py の os.walk と同じ境界)。
    #[test]
    fn walkは深さ上限を守る() {
        let dir = crate::test_util::unique_temp_dir("zaivern-plugin-script", "walk");
        let at_limit = dir.join("a").join("b").join("c").join("d");
        let over_limit = at_limit.join("e");
        std::fs::create_dir_all(&over_limit).expect("mkdir");
        std::fs::write(dir.join("top.jsonl"), "{}\n").expect("write");
        std::fs::write(at_limit.join("mid.jsonl"), "{}\n").expect("write");
        std::fs::write(over_limit.join("deep.jsonl"), "{}\n").expect("write");
        let found = walk_sessions(&dir);
        let names: Vec<String> = found
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"top.jsonl".to_string()));
        assert!(
            names.contains(&"mid.jsonl".to_string()),
            "深さ {MAX_DEPTH} のファイルを拾えていない: {names:?}"
        );
        assert!(
            !names.contains(&"deep.jsonl".to_string()),
            "深さ {MAX_DEPTH} を超えて掘っている: {names:?}"
        );
    }

    /// directory symlink を再帰しない (旧 scan.py の os.walk は
    /// followlinks=False)。セッション置き場の外へ出ると、同じ記録を
    /// 2 度数えたり関係ないツリー全体を掘ったりする。
    #[cfg(unix)]
    #[test]
    fn walkはdirectory_symlinkを辿らない() {
        use std::os::unix::fs::symlink;
        let dir = crate::test_util::unique_temp_dir("zaivern-plugin-script", "walk-link");
        let sessions = dir.join("sessions");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&sessions).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(sessions.join("local.json"), "{}\n").expect("write");
        std::fs::write(outside.join("secret.json"), "{}\n").expect("write");
        // sessions/outside -> ../outside (走査ルートの外を指す directory symlink)
        symlink(&outside, sessions.join("outside")).expect("symlink");
        // 実体側からルートへ戻る輪も作る (辿ると visited 上限まで延々と回る)
        symlink(&sessions, outside.join("back")).expect("symlink");

        let found = walk_sessions(&sessions);
        let names: Vec<String> = found
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"local.json".to_string()), "{names:?}");
        assert!(
            !names.contains(&"secret.json".to_string()),
            "symlink 越しに走査ルートの外へ出ている: {names:?}"
        );
        assert!(
            !found
                .iter()
                .any(|f| f.path.to_string_lossy().contains("outside")),
            "symlink 経由のパスが混入している: {names:?}"
        );
    }

    /// 不正 UTF-8 を含む記録でもファイルごと捨てず、読める行は集計する
    /// (旧 scan.py の errors="replace" と同じ見え方にする)。
    #[test]
    fn 不正utf8の混じる記録も読める行は集計する() {
        let dir = crate::test_util::unique_temp_dir("zaivern-plugin-script", "bad-utf8");
        let file = dir.join("session.jsonl");
        // 正常行 / 壊れたバイトを含む行 / 正常行、の順で書く
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(b"{\"input_tokens\": 10}\n");
        body.extend_from_slice(b"{\"output_tokens\": \"\xff\xfe\"}\n");
        body.extend_from_slice(b"{\"output_tokens\": 4}\n");
        std::fs::write(&file, &body).expect("write");
        let md = std::fs::metadata(&file).expect("metadata");
        let mut files = vec![Found {
            size: md.len(),
            mtime: md.modified().expect("mtime"),
            path: file,
        }];
        let (tokens, _, parsed) = parse_usage(&mut files);
        assert_eq!(parsed, 1, "ファイル自体は数える");
        assert_eq!(tokens.get("input_tokens"), Some(&10.0), "{tokens:?}");
        assert_eq!(tokens.get("output_tokens"), Some(&4.0), "{tokens:?}");
    }

    /// 利用量らしき数値だけを拾い、関係ない数値は拾わないこと。
    #[test]
    fn collectは利用量キーだけ拾う() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"usage":{"input_tokens":10,"output_tokens":4,"latency_ms":900},
                "rows":[{"usage":{"input_tokens":5}},{"cost_usd":0.25}]}"#,
        )
        .expect("parse");
        let mut t = BTreeMap::new();
        let mut c = BTreeMap::new();
        collect_numbers(&v, &mut t, &mut c, 0);
        assert_eq!(t.get("input_tokens"), Some(&15.0), "入れ子を合算する");
        assert_eq!(t.get("output_tokens"), Some(&4.0));
        assert!(!t.contains_key("latency_ms"), "関係ない数値を拾っている");
        assert_eq!(c.get("cost_usd"), Some(&0.25));
    }
}
