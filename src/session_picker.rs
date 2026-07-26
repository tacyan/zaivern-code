//! 過去 AI セッションの列挙 —「あのときの会話の続きから再開する」ためのデータ層。
//!
//! 各 CLI が自前で持っているセッション保存先 (claude なら
//! `~/.claude/projects/<エンコード済み cwd>/<uuid>.jsonl`、codex なら
//! `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`) を走査し、
//! 「このワークスペースで過去に走らせた会話」を新しい順に並べて返す。
//! どの CLI がどこに保存するかは agents.rs のカタログ (`AgentSpec::session_store()`)
//! が唯一の真実源で、このモジュールには CLI 固有の名前を直書きしない。
//!
//! # UI 配線の想定 (app.rs 側は後続ウェーブで実装する)
//!
//! 1. フォルダを開いた直後 / 「過去セッションを開く」を押したとき:
//!    `session_picker::list_sessions(&workspace_root)` を呼ぶ。
//!    戻り値は更新時刻の新しい順・最大 [`MAX_RESULTS`] 件。
//!    ホーム配下の I/O が走るので UI スレッドで毎フレーム呼ばないこと
//!    (開いた瞬間に一度だけ取り、`Vec<PastSession>` をそのまま保持する)。
//! 2. 一覧の 1 行は `summary` (本文の先頭 1 行) + `modified`/`started` +
//!    `agent_bin` (アイコンは `agents::spec_for_bin(&s.agent_bin)` から引ける)。
//!    `summary` は空になり得る (要約に足る発言が先頭付近に無かった場合) ので、
//!    その場合は時刻表示だけにフォールバックすること。
//! 3. ユーザーが 1 件選んだら、起動したいプリセットのコマンドへ
//!    [`resume_command`] を通してから、通常のエージェント起動経路
//!    (`agents::merged_env` → `terminal::SpawnSpec`) へ渡す。
//!    cwd には `PastSession::cwd` ではなく現在のワークスペースを使ってよい
//!    (claude は保存先がプロジェクト単位、codex は ID で一意に決まるため)。
//!
//! # 性能上の約束
//!
//! jsonl は 1 本で 100KB〜数 MB になる。**ファイル全体は絶対に読まない**。
//! 並び順は fs のメタデータ (mtime) だけで決め、本文は先頭数十行・数百 KB までを
//! 読んで要約に使うだけ。中身を読むファイル数も [`SCAN_CAP`] 件で頭打ちにする。

// UI 配線 (app.rs) は後続ウェーブ。それまで公開 API は自テストからのみ参照される。
#![allow(dead_code)]

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::agents::{SessionStore, AGENT_CATALOG};

/// 一覧に載せる過去セッション 1 本分。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PastSession {
    /// CLI へ渡す再開用 ID (claude/codex とも uuid)。
    pub id: String,
    /// 実行ファイル名。`agents::spec_for_bin` の引数にそのまま使える。
    pub agent_bin: String,
    /// 会話の開始時刻 (保存ファイル中のタイムスタンプ。取れなければ `modified`)。
    pub started: SystemTime,
    /// 最終更新時刻 (fs の mtime)。並び順はこれで決める。
    pub modified: SystemTime,
    /// 一覧に出す要約 (最初のユーザー発言の先頭。取れなければ空)。
    pub summary: String,
    /// そのセッションが走っていた作業ディレクトリ。
    pub cwd: PathBuf,
}

/// 一覧として返す最大件数。
pub const MAX_RESULTS: usize = 30;
/// 要約のために **中身を開く** ファイル数の上限 (エージェントごと)。
const SCAN_CAP: usize = 30;
/// 先頭から読む行数の上限 (claude: 冒頭にメタ行が数行入るだけ)。
const CLAUDE_HEAD_LINES: usize = 30;
/// 同上 (codex: 実ユーザー発言の前に長い前置き行が並ぶので少し多め)。
const CODEX_HEAD_LINES: usize = 80;
/// 1 ファイルから読む総バイト数の上限。
const HEAD_BYTES: usize = 512 * 1024;
/// 1 行の長さの上限。これを超える行は要約候補から捨てる (巨大な前置き行対策)。
const MAX_LINE_BYTES: usize = 64 * 1024;
/// cwd 判定のために **1 行目だけ** 開く codex ロールアウトの上限数。
const CODEX_META_SCAN_CAP: usize = 400;
/// 要約の最大文字数 (文字単位。マルチバイトでも壊さない)。
const SUMMARY_CHARS: usize = 140;

/// 実ホーム配下の保存先を走査する通常経路。
pub fn list_sessions(workspace: &Path) -> Vec<PastSession> {
    match dirs::home_dir() {
        Some(home) => list_sessions_from(&home, workspace),
        None => Vec::new(),
    }
}

/// ホームディレクトリを注入できる版 (テストはこちらを叩く)。
///
/// カタログで保存先を宣言しているエージェントだけを順に問い合わせ、
/// 結果をマージして更新時刻の新しい順に並べ、[`MAX_RESULTS`] 件で切る。
pub fn list_sessions_from(home: &Path, workspace: &Path) -> Vec<PastSession> {
    let mut out: Vec<PastSession> = Vec::new();
    for spec in AGENT_CATALOG {
        match spec.session_store() {
            SessionStore::None => {}
            SessionStore::ClaudeProjects => out.extend(list_claude_sessions(
                &claude_projects_root(home),
                workspace,
                spec.bin,
            )),
            SessionStore::CodexRollouts => out.extend(list_codex_sessions(
                &codex_sessions_root(home),
                workspace,
                spec.bin,
            )),
        }
    }
    // 新しい順。同時刻は id で安定化させる (テストの再現性のため)。
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    out.truncate(MAX_RESULTS);
    out
}

/// 選んだ過去セッションを再開するコマンドを組み立てる。
///
/// `command` は起動に使うプリセットのコマンド (承認モード適用後で構わない)。
/// ID 指定再開に未対応の CLI や未知の bin では、素のコマンドがそのまま返る。
pub fn resume_command(command: &str, session: &PastSession) -> String {
    match crate::agents::spec_for_bin(&session.agent_bin) {
        Some(spec) => crate::agents::apply_resume_id(command, spec, &session.id),
        None => command.to_string(),
    }
}

/// `<home>/.claude/projects`
pub fn claude_projects_root(home: &Path) -> PathBuf {
    home.join(".claude").join("projects")
}

/// `<home>/.codex/sessions`
pub fn codex_sessions_root(home: &Path) -> PathBuf {
    home.join(".codex").join("sessions")
}

// ───────────────────────── claude: プロジェクト単位 ─────────────────────────

/// 絶対パス → claude のプロジェクトディレクトリ名。
///
/// claude 本体は「英数字以外を全て `-` に置換」する (JS の
/// `/[^a-zA-Z0-9]/g` 相当なので **ASCII 英数のみ** が残る)。
/// `/Users/me/dev/my app` → `-Users-me-dev-my-app`、
/// `C:\work\proj` → `C--work-proj`。
pub fn encode_claude_project_dir(workspace: &Path) -> String {
    workspace
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// 非 ASCII を残す別解釈 (将来 claude 側が Unicode を保持する実装に変わっても
/// 一覧が空にならないようにする保険。実ディレクトリが在る方を採用する)。
fn encode_claude_project_dir_unicode(workspace: &Path) -> String {
    workspace
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect()
}

/// `projects_root` 配下から、`workspace` のセッションを列挙する。
pub fn list_claude_sessions(
    projects_root: &Path,
    workspace: &Path,
    bin: &str,
) -> Vec<PastSession> {
    let mut names = vec![encode_claude_project_dir(workspace)];
    let uni = encode_claude_project_dir_unicode(workspace);
    if uni != names[0] {
        names.push(uni);
    }
    let Some(dir) = names
        .into_iter()
        .map(|n| projects_root.join(n))
        .find(|p| p.is_dir())
    else {
        return Vec::new();
    };

    // 同名の「サイドカーディレクトリ」が混ざるので、必ずファイルだけを拾う。
    let mut files = jsonl_files(&dir, |name| name.ends_with(".jsonl"));
    files.sort_by(|a, b| b.1.cmp(&a.1));
    files.truncate(SCAN_CAP);

    files
        .into_iter()
        .filter_map(|(path, mtime)| {
            let id = path.file_stem()?.to_string_lossy().to_string();
            if id.is_empty() {
                return None;
            }
            let (summary, started) = claude_summary(&path);
            Some(PastSession {
                id,
                agent_bin: bin.to_string(),
                started: started.unwrap_or(mtime),
                modified: mtime,
                summary,
                cwd: workspace.to_path_buf(),
            })
        })
        .collect()
}

/// 先頭数十行から「最初の本物のユーザー発言」を取り出す。
fn claude_summary(path: &Path) -> (String, Option<SystemTime>) {
    for line in head_lines(path, CLAUDE_HEAD_LINES, HEAD_BYTES) {
        // 巨大行を serde_json に食わせないための安価な事前判定。
        if !line.contains("\"user\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        // サブエージェント (sidechain) の発言はユーザーの会話ではない。
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(ts) = v.get("timestamp").and_then(Value::as_str) else {
            continue;
        };
        let Some(text) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(content_text)
        else {
            continue;
        };
        let text = clean_summary(&text);
        if text.is_empty() || is_envelope(&text) {
            continue;
        }
        return (text, parse_iso8601_utc(ts));
    }
    (String::new(), None)
}

// ───────────────────────── codex: 全プロジェクト混在 ─────────────────────────

/// `sessions_root` (= `~/.codex/sessions`) 配下から `workspace` のものだけ拾う。
///
/// codex はプロジェクト別ディレクトリを持たず日付で掘るので、新しい日付から順に
/// ファイルを集め、**1 行目 (session_meta) だけ** を読んで cwd で絞り込む。
pub fn list_codex_sessions(
    sessions_root: &Path,
    workspace: &Path,
    bin: &str,
) -> Vec<PastSession> {
    let mut files = codex_rollout_files(sessions_root, CODEX_META_SCAN_CAP);
    files.sort_by(|a, b| b.1.cmp(&a.1));

    let mut out = Vec::new();
    for (path, mtime) in files {
        if out.len() >= SCAN_CAP {
            break;
        }
        let Some(first) = head_lines(&path, 1, MAX_LINE_BYTES).into_iter().next() else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&first) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(payload) = v.get("payload") else {
            continue;
        };
        let Some(id) = payload.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(cwd) = payload.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        let cwd = PathBuf::from(cwd);
        if !same_dir(&cwd, workspace) {
            continue;
        }
        let started = payload
            .get("timestamp")
            .or_else(|| v.get("timestamp"))
            .and_then(Value::as_str)
            .and_then(parse_iso8601_utc);
        out.push(PastSession {
            id: id.to_string(),
            agent_bin: bin.to_string(),
            started: started.unwrap_or(mtime),
            modified: mtime,
            summary: codex_summary(&path),
            cwd,
        });
    }
    out
}

/// `YYYY/MM/DD/rollout-*.jsonl` を新しい日付から順に集める (件数上限あり)。
/// ディレクトリ名はゼロ埋めなので、辞書順の降順 = 日付の降順。
fn codex_rollout_files(root: &Path, cap: usize) -> Vec<(PathBuf, SystemTime)> {
    let mut out = Vec::new();
    for year in sorted_subdirs(root) {
        for month in sorted_subdirs(&year) {
            for day in sorted_subdirs(&month) {
                out.extend(jsonl_files(&day, |name| {
                    name.starts_with("rollout-") && name.ends_with(".jsonl")
                }));
                if out.len() >= cap {
                    out.truncate(cap);
                    return out;
                }
            }
        }
    }
    out
}

/// codex の要約は `event_msg` / `user_message` の `payload.message` を最優先で使う
/// (前置きの developer メッセージや AGENTS.md 取り込みを踏まない)。
fn codex_summary(path: &Path) -> String {
    let lines = head_lines(path, CODEX_HEAD_LINES, HEAD_BYTES);
    let mut fallback = String::new();
    for line in lines {
        if !line.contains("\"user") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(payload) = v.get("payload") else {
            continue;
        };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let ptype = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "event_msg" && ptype == "user_message" {
            let text = payload
                .get("message")
                .and_then(Value::as_str)
                .map(clean_summary)
                .unwrap_or_default();
            if !text.is_empty() {
                return text;
            }
            continue;
        }
        // 保険: role=user の response_item (前置きの envelope は弾く)
        if fallback.is_empty()
            && kind == "response_item"
            && payload.get("role").and_then(Value::as_str) == Some("user")
        {
            if let Some(text) = payload.get("content").and_then(content_text) {
                let text = clean_summary(&text);
                if !text.is_empty() && !is_envelope(&text) && !text.starts_with('#') {
                    fallback = text;
                }
            }
        }
    }
    fallback
}

// ───────────────────────── 共通ヘルパ ─────────────────────────

/// ディレクトリ直下のファイルを (パス, mtime) で集める。ディレクトリは必ず除外する。
fn jsonl_files(dir: &Path, keep: impl Fn(&str) -> bool) -> Vec<(PathBuf, SystemTime)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_file() {
            continue; // 同名のサイドカーディレクトリを踏まない
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !keep(&name) {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        out.push((entry.path(), mtime));
    }
    out
}

/// 直下のサブディレクトリを名前の降順で返す。
fn sorted_subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    out.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    out
}

/// ファイル先頭を「行数・総バイト数・1 行の長さ」の三重の上限つきで読む。
/// 上限を超えた長い行は捨てる (要約には使えないし、持ち歩くと重い)。
fn head_lines(path: &Path, max_lines: usize, max_bytes: usize) -> Vec<String> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut reader = BufReader::new(file.take(max_bytes as u64));
    let mut out = Vec::new();
    let mut buf = Vec::new();
    for _ in 0..max_lines {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buf.len() > MAX_LINE_BYTES {
            continue;
        }
        out.push(String::from_utf8_lossy(&buf).into_owned());
    }
    out
}

/// `message.content` から表示用テキストを取り出す。
/// 文字列そのままの形式と、ブロック配列 (`[{"type":"text","text":...}]`) の両方に対応。
fn content_text(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let arr = content.as_array()?;
    let mut parts = Vec::new();
    for block in arr {
        let ty = block.get("type").and_then(Value::as_str).unwrap_or("");
        // text / input_text / output_text いずれも本文は "text" フィールド
        if ty.is_empty() || ty.ends_with("text") {
            if let Some(t) = block.get("text").and_then(Value::as_str) {
                if !t.trim().is_empty() {
                    parts.push(t.trim().to_string());
                }
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(" "))
}

/// 空白を潰して 1 行にし、[`SUMMARY_CHARS`] 文字で切る。
fn clean_summary(raw: &str) -> String {
    let flat: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= SUMMARY_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(SUMMARY_CHARS).collect();
    format!("{cut}…")
}

/// CLI が本文に差し込む擬似タグ (`<command-name>` `<local-command-stdout>`
/// `<system-reminder>` など) かどうか。
///
/// タグ名にハイフンを含むものだけを弾く — ユーザーが貼り付けた普通の HTML
/// (`<div>` `<p>`) を要約から落とさないための線引き。
fn is_envelope(text: &str) -> bool {
    let t = text.trim_start();
    let Some(rest) = t.strip_prefix('<') else {
        return false;
    };
    let Some(end) = rest.find('>') else {
        return false;
    };
    if end > 40 {
        return false;
    }
    let tag = rest[..end].trim_end_matches('/');
    tag.contains('-') && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// パスが同じディレクトリを指すか (末尾の区切りを無視。Windows は大小無視)。
fn same_dir(a: &Path, b: &Path) -> bool {
    fn norm(p: &Path) -> String {
        let s = p.to_string_lossy().to_string();
        let trimmed = s.trim_end_matches(['/', '\\']);
        if trimmed.is_empty() {
            s
        } else {
            trimmed.to_string()
        }
    }
    let (x, y) = (norm(a), norm(b));
    if cfg!(windows) {
        x.eq_ignore_ascii_case(&y)
    } else {
        x == y
    }
}

/// `2026-07-25T03:06:23.019Z` 形式を SystemTime へ。依存クレートを増やさない最小実装。
/// 解釈できなければ None (呼び出し側は mtime にフォールバックする)。
fn parse_iso8601_utc(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> { s.get(from..to)?.parse::<i64>().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Howard Hinnant の civil_from_days の逆関数 (1970-01-01 からの日数)。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    /// mtime を固定して書く (並び順のテストを sleep 無しで決定的にする)。
    fn write_at(path: &Path, body: &str, epoch: u64) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
        let f = File::options().write(true).open(path).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(epoch))
            .unwrap();
    }

    /// メタ行 → envelope → sidechain → 本物のユーザー発言 (文字列本文)
    const CLAUDE_META_FIRST: &str = concat!(
        r#"{"type":"mode","mode":"default"}"#,
        "\n",
        r#"{"type":"permission-mode","mode":"acceptEdits"}"#,
        "\n",
        r#"{"type":"file-history-snapshot","messageId":"x"}"#,
        "\n",
        r#"{"type":"user","timestamp":"2026-07-20T01:02:03.000Z","message":{"content":"<command-name>/clear</command-name>"}}"#,
        "\n",
        r#"{"type":"user","isSidechain":true,"timestamp":"2026-07-20T01:03:00.000Z","message":{"content":"サブエージェントの発言"}}"#,
        "\n",
        r#"{"type":"user","timestamp":"2026-07-20T01:04:05.000Z","message":{"content":"文字列本文の\n最初の発言"}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-07-20T01:04:09.000Z","message":{"content":"返事"}}"#,
        "\n",
    );

    /// content がブロック配列の形式
    const CLAUDE_BLOCK_LIST: &str = concat!(
        r#"{"type":"summary","summary":"無視される"}"#,
        "\n",
        r#"{"type":"user","timestamp":"2026-07-21T09:00:00.000Z","isSidechain":false,"message":{"role":"user","content":[{"type":"text","text":"  ブロック形式の本文  "},{"type":"image"}]}}"#,
        "\n",
    );

    fn codex_rollout(id: &str, cwd: &Path, msg: &str) -> String {
        let cwd = cwd.to_string_lossy().replace('\\', "\\\\");
        format!(
            concat!(
                r#"{{"timestamp":"2026-07-25T03:06:23.000Z","type":"session_meta","payload":{{"id":"{id}","timestamp":"2026-07-25T03:06:23.000Z","cwd":"{cwd}","cli_version":"0.56.0"}}}}"#,
                "\n",
                r#"{{"type":"response_item","payload":{{"type":"message","role":"developer","content":[{{"type":"input_text","text":"<permissions instructions>前置き</permissions instructions>"}}]}}}}"#,
                "\n",
                // `"#` を含むので r##".."## が要る (raw 文字列が途中で閉じてしまう)
                r##"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"# AGENTS.md instructions"}}]}}}}"##,
                "\n",
                r#"{{"type":"event_msg","payload":{{"type":"user_message","message":"{msg}","images":null}}}}"#,
                "\n",
            ),
            id = id,
            cwd = cwd,
            msg = msg
        )
    }

    // ── claude 列挙 ──────────────────────────────────────────────

    #[test]
    fn claude_enumerates_ids_summaries_and_newest_first() {
        let home = unique_temp_dir("zaivern-picker", "claude-basic");
        let ws = home.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let dir = claude_projects_root(&home).join(encode_claude_project_dir(&ws));
        std::fs::create_dir_all(&dir).unwrap();

        write_at(&dir.join("aaa-1111.jsonl"), CLAUDE_META_FIRST, 1_000);
        write_at(&dir.join("bbb-2222.jsonl"), CLAUDE_BLOCK_LIST, 2_000);
        // サイドカー: 同じ uuid 名のディレクトリ / .jsonl 名のディレクトリ
        std::fs::create_dir_all(dir.join("aaa-1111")).unwrap();
        std::fs::create_dir_all(dir.join("ccc-3333.jsonl")).unwrap();

        let got = list_claude_sessions(&claude_projects_root(&home), &ws, "claude");
        assert_eq!(got.len(), 2, "ディレクトリを数えていないか");
        // 新しい順
        assert_eq!(got[0].id, "bbb-2222");
        assert_eq!(got[0].summary, "ブロック形式の本文");
        assert_eq!(got[1].id, "aaa-1111");
        // メタ行 / envelope / sidechain を飛ばして最初の本物の発言を拾う
        assert_eq!(got[1].summary, "文字列本文の 最初の発言");
        assert_eq!(got[1].agent_bin, "claude");
        assert_eq!(got[1].cwd, ws);
        assert_eq!(got[1].modified, UNIX_EPOCH + Duration::from_secs(1_000));
        // started は本文行の timestamp 由来 (mtime とは別物)
        assert_eq!(
            got[1].started,
            parse_iso8601_utc("2026-07-20T01:04:05.000Z").unwrap()
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn claude_missing_project_dir_is_empty_not_panic() {
        let home = unique_temp_dir("zaivern-picker", "claude-none");
        let got = list_claude_sessions(&claude_projects_root(&home), Path::new("/no/such/ws"), "claude");
        assert!(got.is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn claude_scan_is_capped() {
        let home = unique_temp_dir("zaivern-picker", "claude-cap");
        let ws = home.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let dir = claude_projects_root(&home).join(encode_claude_project_dir(&ws));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..(SCAN_CAP + 12) {
            write_at(
                &dir.join(format!("id-{i:04}.jsonl")),
                CLAUDE_BLOCK_LIST,
                10_000 + i as u64,
            );
        }
        let got = list_claude_sessions(&claude_projects_root(&home), &ws, "claude");
        assert_eq!(got.len(), SCAN_CAP);
        // 上限で切っても「新しい方」が残る
        assert_eq!(got[0].id, format!("id-{:04}", SCAN_CAP + 11));
        let _ = std::fs::remove_dir_all(&home);
    }

    // ── codex 列挙 ───────────────────────────────────────────────

    #[test]
    fn codex_filters_by_cwd_and_reads_user_message() {
        let home = unique_temp_dir("zaivern-picker", "codex-basic");
        let ws = home.join("ws");
        let other = home.join("other");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let day = codex_sessions_root(&home).join("2026").join("07").join("25");

        write_at(
            &day.join("rollout-2026-07-25T03-06-23-aaa-1111.jsonl"),
            &codex_rollout("aaa-1111", &ws, "コーデックスの最初の指示"),
            3_000,
        );
        write_at(
            &day.join("rollout-2026-07-25T04-00-00-bbb-2222.jsonl"),
            &codex_rollout("bbb-2222", &other, "別プロジェクトの指示"),
            4_000,
        );

        let got = list_codex_sessions(&codex_sessions_root(&home), &ws, "codex");
        assert_eq!(got.len(), 1, "cwd の違うロールアウトが混ざっている");
        assert_eq!(got[0].id, "aaa-1111");
        assert_eq!(got[0].agent_bin, "codex");
        assert_eq!(got[0].summary, "コーデックスの最初の指示");
        assert_eq!(got[0].cwd, ws);
        assert_eq!(
            got[0].started,
            parse_iso8601_utc("2026-07-25T03:06:23.000Z").unwrap()
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_missing_root_is_empty() {
        let home = unique_temp_dir("zaivern-picker", "codex-none");
        assert!(list_codex_sessions(&codex_sessions_root(&home), &home, "codex").is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    // ── マージ / 並び / 上限 ─────────────────────────────────────

    #[test]
    fn merged_list_is_sorted_by_mtime_desc_and_capped() {
        let home = unique_temp_dir("zaivern-picker", "merge");
        let ws = home.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let cdir = claude_projects_root(&home).join(encode_claude_project_dir(&ws));
        std::fs::create_dir_all(&cdir).unwrap();
        write_at(&cdir.join("c-old.jsonl"), CLAUDE_BLOCK_LIST, 100);
        write_at(&cdir.join("c-new.jsonl"), CLAUDE_BLOCK_LIST, 300);
        let day = codex_sessions_root(&home).join("2026").join("07").join("25");
        write_at(
            &day.join("rollout-x-x-mid.jsonl"),
            &codex_rollout("x-mid", &ws, "まんなか"),
            200,
        );

        let got = list_sessions_from(&home, &ws);
        let ids: Vec<&str> = got.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["c-new", "x-mid", "c-old"]);
        assert!(got.len() <= MAX_RESULTS);

        // 総数の上限
        for i in 0..MAX_RESULTS + 10 {
            write_at(
                &cdir.join(format!("bulk-{i:04}.jsonl")),
                CLAUDE_BLOCK_LIST,
                1_000 + i as u64,
            );
        }
        assert_eq!(list_sessions_from(&home, &ws).len(), MAX_RESULTS);
        let _ = std::fs::remove_dir_all(&home);
    }

    // ── パスのエンコード表 ───────────────────────────────────────

    #[test]
    fn path_encoding_table() {
        let cases: &[(&str, &str)] = &[
            ("/Users/me/dev/proj", "-Users-me-dev-proj"),
            ("/Users/me/my app", "-Users-me-my-app"),
            ("/Users/me/dev/Fable5-mcp", "-Users-me-dev-Fable5-mcp"),
            ("/home/u/.config/x", "-home-u--config-x"),
            // 非 ASCII は 1 文字 = 1 ハイフン
            ("/Users/me/開発", "-Users-me---"),
            ("/tmp/a_b.c", "-tmp-a-b-c"),
            // Windows 形式 (区切りが `\` でも、パス文字列としてそのまま潰される)
            (r"C:\work\proj", "C--work-proj"),
            (r"D:\a b\c-d", "D--a-b-c-d"),
        ];
        for (input, want) in cases {
            assert_eq!(
                encode_claude_project_dir(Path::new(input)),
                *want,
                "input={input}"
            );
        }
        // 実パス由来でも往復して同じディレクトリを指す
        let ws = Path::new("/Users/me/dev/proj");
        assert_eq!(
            claude_projects_root(Path::new("/h")).join(encode_claude_project_dir(ws)),
            Path::new("/h/.claude/projects/-Users-me-dev-proj")
        );
    }

    #[test]
    fn same_dir_ignores_trailing_separator() {
        assert!(same_dir(Path::new("/a/b/"), Path::new("/a/b")));
        assert!(!same_dir(Path::new("/a/b"), Path::new("/a/bc")));
        #[cfg(windows)]
        assert!(same_dir(Path::new(r"C:\A\B"), Path::new(r"c:\a\b")));
    }

    // ── 小物 ─────────────────────────────────────────────────────

    #[test]
    fn envelope_detection_keeps_plain_html() {
        assert!(is_envelope("<command-name>/clear</command-name>"));
        assert!(is_envelope("<local-command-stdout>out"));
        assert!(is_envelope("<system-reminder>note"));
        assert!(!is_envelope("<div>普通の貼り付け</div>"));
        assert!(!is_envelope("これは <command-name> ではない"));
        assert!(!is_envelope("普通の文章"));
    }

    #[test]
    fn summary_is_flattened_and_truncated() {
        assert_eq!(clean_summary("  a\n b\tc "), "a b c");
        let long: String = "あ".repeat(SUMMARY_CHARS + 20);
        let cut = clean_summary(&long);
        assert_eq!(cut.chars().count(), SUMMARY_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn iso8601_parses_and_rejects_garbage() {
        assert_eq!(
            parse_iso8601_utc("1970-01-01T00:00:00.000Z").unwrap(),
            UNIX_EPOCH
        );
        assert_eq!(
            parse_iso8601_utc("2001-09-09T01:46:40Z").unwrap(),
            UNIX_EPOCH + Duration::from_secs(1_000_000_000)
        );
        assert_eq!(
            parse_iso8601_utc("2024-02-29T00:00:00.000Z").unwrap(),
            UNIX_EPOCH + Duration::from_secs(1_709_164_800)
        );
        assert!(parse_iso8601_utc("").is_none());
        assert!(parse_iso8601_utc("not-a-timestamp!!!").is_none());
        assert!(parse_iso8601_utc("2026-13-01T00:00:00Z").is_none());
    }

    #[test]
    fn head_lines_drops_oversized_lines() {
        let dir = unique_temp_dir("zaivern-picker", "head");
        let p = dir.join("x.jsonl");
        let huge = "x".repeat(MAX_LINE_BYTES + 10);
        std::fs::write(&p, format!("first\n{huge}\nthird\n")).unwrap();
        let got = head_lines(&p, 10, HEAD_BYTES);
        assert_eq!(got.len(), 2);
        assert!(got[0].starts_with("first") && got[1].starts_with("third"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_command_uses_catalog() {
        let s = PastSession {
            id: "abc-123".into(),
            agent_bin: "claude".into(),
            started: UNIX_EPOCH,
            modified: UNIX_EPOCH,
            summary: String::new(),
            cwd: PathBuf::from("/ws"),
        };
        assert_eq!(resume_command("claude", &s), "claude --resume abc-123");
        let c = PastSession {
            agent_bin: "codex".into(),
            ..s.clone()
        };
        assert_eq!(resume_command("codex --yolo", &c), "codex resume abc-123 --yolo");
        // 未知の bin は素通し
        let u = PastSession {
            agent_bin: "totally-unknown".into(),
            ..s.clone()
        };
        assert_eq!(resume_command("whatever --x", &u), "whatever --x");
    }
}
