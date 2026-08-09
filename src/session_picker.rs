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
//! # セッションサイドバー (フォルダ見出し + 過去会話) の配線契約
//!
//! 描画は `panels::sessions_sidebar_ui`、状態とキャッシュは本モジュールの
//! [`SidebarState`] が持つ。app.rs 側は次の 1 ブロックだけで済む:
//!
//! ```ignore
//! // 1) 状態は App のフィールドに 1 つ持つ (Default で良い)
//! //    sidebar_sessions: session_picker::SidebarState,
//! //
//! // 2) フォルダ一覧は「いま開いているルートだけ」(重複除去)
//! let folders = session_picker::sidebar_folders(&self.open_roots);
//! //    ※ is_dir() を見るので毎フレームではなく、ルートが変わったときに作り直す
//! //
//! // 3) 左サイドバーの「セッション」タブの中身として呼ぶ
//! match panels::sessions_sidebar_ui(ui, &self.theme, &mut self.sidebar_sessions, &folders) {
//!     SidebarAction::None => {}
//!     SidebarAction::Resume(s) => {
//!         // プリセットのコマンドへ再開指定を足してから通常の起動経路へ
//!         let cmd = session_picker::resume_command(&preset_command, &s);
//!         // agents::merged_env → terminal::SpawnSpec (cwd は s.cwd)
//!     }
//!     SidebarAction::NewConversation(dir) => { /* そのフォルダで新規起動 */ }
//!     SidebarAction::RevealFolder(dir) => { /* OS のファイラで開く */ }
//!     SidebarAction::CloseFolder(dir) => { /* open_roots から外す */ }
//! }
//! ```
//!
//! 走査はバックグラウンドスレッド + mpsc + TTL (git.rs / git_panel.rs と同じ方式)。
//! UI スレッドはキャッシュを読むだけで、フレーム内でファイルシステムに触らない。
//!
//! # 性能上の約束
//!
//! jsonl は 1 本で 100KB〜数 MB になる。**ファイル全体は絶対に読まない**。
//! 並び順は fs のメタデータ (mtime) だけで決め、本文は先頭数十行・数百 KB までを
//! 読んで要約に使うだけ。中身を読むファイル数も [`SCAN_CAP`] 件で頭打ちにする。

// UI 配線 (app.rs) は後続ウェーブ。それまで公開 API は自テストからのみ参照される。
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    // agents.rs のテーブルに載せられなかった保存先 (Antigravity) を後段で足す。
    for st in LOCAL_SESSION_STORES {
        // 将来 agents.rs 側に同じ bin が載ったら、そちらを正として二重計上を避ける。
        if AGENT_CATALOG
            .iter()
            .any(|s| s.bin == st.bin && s.session_store() != SessionStore::None)
        {
            continue;
        }
        match st.kind {
            LocalStoreKind::AntigravitySummaries => {
                for rel in st.rel_paths {
                    let db = rel.iter().fold(home.to_path_buf(), |p, seg| p.join(seg));
                    out.extend(list_antigravity_sessions(&db, workspace, st.bin));
                }
            }
        }
    }
    // **ベンダーが履歴を公開していないエージェントは、アプリ自身の記録で補う。**
    //
    // ここまでで拾えるのは Claude / Codex / Antigravity の 3 つだけ。
    // gemini / droid / cursor-agent / aider / opencode などは
    // 「一覧にすら出ないので再開できない」状態だった。
    // 起動のたびに `~/.zaivern/history/<bin>/<workspace>.jsonl` へ 1 行
    // 積んでいるので、それを同じ形へ変換して足す。
    //
    // **ベンダー側が 1 件でも出している bin は足さない** — 向こうの方が
    // 要約も再開 ID も正確なので、同じ会話が二重に並ぶ方が害になる。
    let covered: HashSet<String> = out.iter().map(|s| s.agent_bin.clone()).collect();
    out.extend(
        zaivern_sessions(workspace)
            .into_iter()
            .filter(|s| !covered.contains(&s.agent_bin)),
    );
    // 新しい順。同時刻は id で安定化させる (テストの再現性のため)。
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    // 同じ会話が複数の保存先に現れても 1 行にする。
    let mut seen: HashSet<(String, String)> = HashSet::new();
    out.retain(|s| seen.insert((s.agent_bin.clone(), s.id.clone())));
    out.truncate(MAX_RESULTS);
    out
}

/// アプリ自身が記録した履歴 ([`crate::history`]) を一覧の形へ変換する。
///
/// `PastSession::id` にはベンダー側の再開 ID を入れる規約なので、
/// 判っていない場合は**空文字**を入れる。`resume_command` はそのとき
/// 「その CLI の "直前の会話を続ける" フラグ」へ落とす。
fn zaivern_sessions(workspace: &Path) -> Vec<PastSession> {
    entries_to_sessions(crate::history::list_all(workspace))
}

/// [`zaivern_sessions`] の変換部だけを切り出した純関数 (ファイルを読まない)。
///
/// 実 `~/.zaivern` を触らずにテストできるよう、I/O と分けてある。
fn entries_to_sessions(entries: Vec<crate::history::Entry>) -> Vec<PastSession> {
    entries
        .into_iter()
        .filter(|e| !e.agent_bin.is_empty())
        .map(|e| {
            let at = |secs: i64| {
                // 負の値や 0 は「不明」。UNIX_EPOCH へ落として最下位に並べる。
                u64::try_from(secs)
                    .ok()
                    .map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s))
                    .unwrap_or(SystemTime::UNIX_EPOCH)
            };
            let started = at(e.started);
            PastSession {
                id: e.vendor_id.clone(),
                agent_bin: e.agent_bin.clone(),
                started,
                // 終了時刻が判らない (まだ開いている / 異常終了) なら開始時刻で並べる。
                modified: if e.ended > 0 { at(e.ended) } else { started },
                // 要約が無ければタイトルを出す (空行を並べない)。
                summary: if e.brief.is_empty() {
                    e.title.clone()
                } else {
                    e.brief.clone()
                },
                cwd: PathBuf::from(&e.cwd),
            }
        })
        .collect()
}

/// 選んだ過去セッションを再開するコマンドを組み立てる。
///
/// `command` は起動に使うプリセットのコマンド (承認モード適用後で構わない)。
/// ID 指定再開に未対応の CLI や未知の bin では、素のコマンドがそのまま返る。
pub fn resume_command(command: &str, session: &PastSession) -> String {
    let via_catalog = match crate::agents::spec_for_bin(&session.agent_bin) {
        Some(spec) => crate::agents::apply_resume_id(command, spec, &session.id),
        None => command.to_string(),
    };
    if via_catalog != command {
        return via_catalog;
    }
    // agents.rs のテーブルに未登録の保存先 (Antigravity) はローカル表で補う。
    let via_local = match local_store_for(&session.agent_bin) {
        Some(st) => apply_local_resume_id(command, st.resume_id_flag, &session.id),
        None => via_catalog,
    };
    if via_local != command {
        return via_local;
    }
    // **ID が判らない相手だけ「直前の会話を続ける」フラグへ落とす。**
    //
    // アプリ自身の履歴から起こした行 ([`zaivern_sessions`]) は、ベンダーが
    // 会話 ID を公開していないので `id` が空になる。ここで諦めると
    // 「一覧には出るのに再開しない」になってしまうので、その CLI が持つ
    // `--continue` / `--resume latest` 相当 (`AgentSpec::resume_flag`) を付ける。
    // フラグを持たない CLI では素のコマンドがそのまま返る (作業フォルダだけは
    // 引き継ぐので、同じ場所で仕切り直せる)。
    //
    // **`id` が空でないときは絶対に落とさない。** ID があるのに上で
    // 変化しなかったのは「もう再開指定が入っている」か「安全でない ID を
    // 弾いた」場合で、そこへ別の再開フラグを重ねると二重指定になる。
    if !session.id.is_empty() {
        return via_local;
    }
    match crate::agents::spec_for_bin(&session.agent_bin) {
        Some(spec) => crate::agents::apply_resume(command, spec),
        None => via_local,
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
pub fn list_claude_sessions(projects_root: &Path, workspace: &Path, bin: &str) -> Vec<PastSession> {
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
pub fn list_codex_sessions(sessions_root: &Path, workspace: &Path, bin: &str) -> Vec<PastSession> {
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
    tag.contains('-')
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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

// ═══════════════════════ Antigravity (Google) の保存先 ═══════════════════════
//
// 【この機械で実際に確認したこと (2026-07 時点 / 読み取りのみ)】
//
// - `~/.gemini/antigravity-cli/conversation_summaries.db`
//   SQLite 3。テーブル `conversation_summaries` の DDL は GORM 由来で、
//   `conversation_id text` `title text` `preview text` `last_modified_time datetime`
//   `workspace_uris text` `agent_name text` `last_user_input_time datetime` ほかを持つ。
//   * `conversation_id` は UUID。`agy --conversation <ID>` でそのまま再開できる
//     (`agy --help` に "Resume a previous conversation by ID" として載っている)。
//   * `workspace_uris` は `["file:///path/to/ws"]` 形式の JSON 配列 (複数可)。
//   * 時刻列は `2026-07-17 06:06:57.519246+00:00` 形式のテキスト。
//     未入力の行は Go のゼロ値 `0001-01-01 00:00:00+00:00` になる (= 無効扱い)。
//   * `title` は空のことが多く、実質の見出しは `preview`。
//
// - `~/.gemini/antigravity/` (IDE 側) と `~/.gemini/antigravity-cli/` の
//   `conversations/<uuid>.db` / `<uuid>.pb`、および IDE 側の
//   `agyhub_summaries_proto.pb` は **protobuf バイナリ** (SQLite の中身も blob 列)。
//   protobuf クレートを足さずには読めないので使わない。IDE 側には
//   `conversation_summaries.db` 相当が存在しない (find で確認済み)。
//   IDE 側も読みたくなったら「`agyhub_summaries_proto.pb` の .proto 定義」が要る。
//
// - `~/.gemini/antigravity-cli/history.jsonl` は
//   `{"display":…, "timestamp":…, "workspace":…}` の行指向 JSON で cwd も時刻も入るが、
//   **会話 ID が無い** ので再開に使えない (入力履歴であってセッション一覧ではない)。
//
// 【既知の制限】読むのは本体ファイルだけなので、`-wal` に留まっている直近の書き込みは
// チェックポイント後に見えるようになる。DB が無い環境では静かに空を返す。

/// agents.rs の `SESSION_STORES` に合流させるまでの **暫定** カタログ。
///
/// agents.rs は他エージェントが編集中で触れないため、同じ「データ表」形式のまま
/// ここへ置いている。合流時は bin / 再開フラグをそのまま移植すればよい。
pub struct LocalSessionStore {
    /// 実行ファイル名 (`AgentSpec::bin` と同じキー)。
    pub bin: &'static str,
    /// 保存先の種類。
    pub kind: LocalStoreKind,
    /// ホームからの相対パス候補 (先に見つかった方ではなく全部を見てマージする)。
    pub rel_paths: &'static [&'static [&'static str]],
    /// ID 指定再開のフラグ。空なら再開不可。
    pub resume_id_flag: &'static str,
}

/// ローカル表で扱う保存先の種類。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LocalStoreKind {
    /// `conversation_summaries` テーブルを持つ SQLite。
    AntigravitySummaries,
}

/// ローカル表の実体。パスは全て `dirs::home_dir()` からの相対で、絶対パスは持たない。
pub const LOCAL_SESSION_STORES: &[LocalSessionStore] = &[LocalSessionStore {
    bin: "agy",
    kind: LocalStoreKind::AntigravitySummaries,
    rel_paths: &[
        &[".gemini", "antigravity-cli", "conversation_summaries.db"],
        // IDE 側は現状この名前のファイルを持たないが、将来置かれたら拾えるようにしておく。
        &[".gemini", "antigravity", "conversation_summaries.db"],
    ],
    resume_id_flag: "--conversation",
}];

fn local_store_for(bin: &str) -> Option<&'static LocalSessionStore> {
    LOCAL_SESSION_STORES.iter().find(|s| s.bin == bin)
}

/// フラグ型の再開指定をコマンド末尾へ足す (agents.rs の `apply_resume_id` と同じ規則)。
fn apply_local_resume_id(command: &str, flag: &str, id: &str) -> String {
    let id = id.trim();
    if flag.is_empty() || id.is_empty() || !is_safe_local_id(id) {
        return command.to_string();
    }
    if command.split_whitespace().any(|t| t == flag) {
        return command.to_string(); // 二重指定を作らない
    }
    format!("{} {flag} {id}", command.trim())
}

/// シェルへ渡して危険にならない ID か (UUID を想定した保守的な判定)。
fn is_safe_local_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `conversation_summaries` の列名。位置ではなく **名前** で引くための表。
struct AgyColumns {
    id: &'static str,
    workspaces: &'static str,
    modified: &'static str,
    started: &'static str,
    title: &'static str,
    preview: &'static str,
}

const AGY_COLUMNS: AgyColumns = AgyColumns {
    id: "conversation_id",
    workspaces: "workspace_uris",
    modified: "last_modified_time",
    started: "last_user_input_time",
    title: "title",
    preview: "preview",
};

/// `conversation_summaries` テーブルの論理名。
const AGY_TABLE: &str = "conversation_summaries";
/// 1 回の読み出しで取り込む最大行数 (全プロジェクト分が 1 ファイルに入っている)。
const AGY_MAX_ROWS: usize = 4_000;

/// Antigravity の会話一覧から `workspace` のものだけを拾う。
///
/// `db` にファイルが無い / 壊れている / テーブルが無い場合は静かに空を返す。
pub fn list_antigravity_sessions(db: &Path, workspace: &Path, bin: &str) -> Vec<PastSession> {
    let Some((cols, rows)) = sqlite_lite::read_table(db, AGY_TABLE, AGY_MAX_ROWS) else {
        return Vec::new();
    };
    let idx = |name: &str| cols.iter().position(|c| c == name);
    let (Some(i_id), Some(i_ws), Some(i_mod)) = (
        idx(AGY_COLUMNS.id),
        idx(AGY_COLUMNS.workspaces),
        idx(AGY_COLUMNS.modified),
    ) else {
        return Vec::new(); // 想定した列が無い = 形式が変わった。推測はしない。
    };
    let (i_started, i_title, i_preview) = (
        idx(AGY_COLUMNS.started),
        idx(AGY_COLUMNS.title),
        idx(AGY_COLUMNS.preview),
    );

    let cell = |row: &Vec<sqlite_lite::Cell>, i: Option<usize>| -> String {
        i.and_then(|i| row.get(i))
            .map(|c| c.text())
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for row in &rows {
        let Some(id) = row.get(i_id).map(|c| c.text()) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let Some(uris) = row.get(i_ws).map(|c| c.text()) else {
            continue;
        };
        if !workspace_uris_match(&uris, workspace) {
            continue;
        }
        let modified = row
            .get(i_mod)
            .and_then(|c| parse_sql_datetime(&c.text()))
            .unwrap_or(UNIX_EPOCH);
        let started = parse_sql_datetime(&cell(row, i_started)).unwrap_or(modified);
        let title = clean_summary(&cell(row, i_title));
        let summary = if title.is_empty() {
            clean_summary(&cell(row, i_preview))
        } else {
            title
        };
        out.push(PastSession {
            id,
            agent_bin: bin.to_string(),
            started,
            modified,
            summary,
            cwd: workspace.to_path_buf(),
        });
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    out.truncate(SCAN_CAP);
    out
}

/// `["file:///a", "file:///b"]` のいずれかが `workspace` を指すか。
fn workspace_uris_match(raw: &str, workspace: &Path) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    let Some(arr) = v.as_array() else {
        // 単一文字列で入っている実装に出会っても壊れないようにする。
        return v
            .as_str()
            .and_then(path_from_file_uri)
            .map(|p| same_dir(&p, workspace))
            .unwrap_or(false);
    };
    arr.iter()
        .filter_map(Value::as_str)
        .filter_map(path_from_file_uri)
        .any(|p| same_dir(&p, workspace))
}

/// `file:///Users/me/ws` → `/Users/me/ws`、`file:///C:/w` → `C:/w`。
/// スキームが違えば None。`%XX` はデコードする。
fn path_from_file_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri
        .strip_prefix("file://")
        .or_else(|| uri.strip_prefix("FILE://"))?;
    // `file://host/path` のホスト部は使わない (ローカルのみ対象)。
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(i) => &rest[i..],
        None if rest.is_empty() => "/",
        None => return None,
    };
    let decoded = percent_decode(path);
    // Windows のドライブ表記 `/C:/…` は先頭のスラッシュを落とす。
    let b = decoded.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        return Some(PathBuf::from(normalize_uri_separators(
            decoded[1..].to_string(),
        )));
    }
    Some(PathBuf::from(normalize_uri_separators(decoded)))
}

/// URI の区切りは常に `/`。Windows ではパス区切りへ直してから比較する。
#[cfg(windows)]
fn normalize_uri_separators(s: String) -> String {
    s.replace('/', "\\")
}
#[cfg(not(windows))]
fn normalize_uri_separators(s: String) -> String {
    s
}

/// `%XX` だけを戻す最小のパーセントデコード (不正な並びはそのまま残す)。
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| -> Option<u8> {
                match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `2026-07-17 06:06:57.519246+00:00` / `…T…Z` の両方を UTC の SystemTime へ。
/// 末尾の `±HH:MM` オフセットは差し引く。Go のゼロ値 (`0001-…`) は None。
fn parse_sql_datetime(s: &str) -> Option<SystemTime> {
    let naive = parse_iso8601_utc(s)?; // 先頭 19 文字だけを見る (区切りは何でもよい)
    let off = tz_offset_secs(s);
    if off == 0 {
        return Some(naive);
    }
    let secs = naive.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64 - off;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// 秒の後ろに続く `Z` / `±HH:MM` / `±HHMM` を秒数へ (無ければ 0)。
fn tz_offset_secs(s: &str) -> i64 {
    let b = s.as_bytes();
    let mut i = 19; // 日付+時刻の固定長部分の直後から探す
    while i < b.len() {
        match b[i] {
            b'+' | b'-' => break,
            b'Z' | b'z' => return 0,
            _ => i += 1,
        }
    }
    if i >= b.len() {
        return 0;
    }
    let sign = if b[i] == b'-' { -1 } else { 1 };
    let digits: Vec<u8> = b[i + 1..]
        .iter()
        .copied()
        .filter(|c| c.is_ascii_digit())
        .collect();
    if digits.len() < 2 {
        return 0;
    }
    let num = |d: &[u8]| -> i64 { d.iter().fold(0i64, |a, c| a * 10 + (c - b'0') as i64) };
    let h = num(&digits[..2]);
    let m = if digits.len() >= 4 {
        num(&digits[2..4])
    } else {
        0
    };
    if h > 23 || m > 59 {
        return 0;
    }
    sign * (h * 3600 + m * 60)
}

// ═══════════════════════ 依存を増やさない最小 SQLite リーダ ═══════════════════
//
// 目的は「1 テーブルを頭から読む」だけ。書き込み・インデックス・WAL は扱わない。
// 仕様は SQLite の公式ファイルフォーマット (b-tree ページ + レコード形式) に従う。
mod sqlite_lite {
    use std::path::Path;

    /// レコード 1 セルの値。
    #[derive(Clone, Debug, PartialEq)]
    pub enum Cell {
        Null,
        Int(i64),
        Real(f64),
        Text(String),
        Blob(Vec<u8>),
    }

    impl Cell {
        /// 表示・比較用のテキスト表現 (数値も文字列化する)。
        pub fn text(&self) -> String {
            match self {
                Cell::Null => String::new(),
                Cell::Int(v) => v.to_string(),
                Cell::Real(v) => v.to_string(),
                Cell::Text(s) => s.clone(),
                Cell::Blob(_) => String::new(),
            }
        }
    }

    const MAGIC: &[u8; 16] = b"SQLite format 3\0";
    /// 丸ごとメモリに載せる上限。これを超えるファイルは読まない。
    const MAX_DB_BYTES: u64 = 32 * 1024 * 1024;
    /// b-tree を歩くページ数の上限 (壊れたファイルで無限ループしない保険)。
    const MAX_PAGES: usize = 20_000;
    /// 1 レコードの最大ペイロード。
    const MAX_PAYLOAD: usize = 4 * 1024 * 1024;
    /// sqlite_master から読む最大行数。
    const MAX_MASTER_ROWS: usize = 2_000;
    /// sqlite_master の列位置 (この 5 列だけは仕様で固定)。
    const MASTER_TYPE: usize = 0;
    const MASTER_NAME: usize = 1;
    const MASTER_ROOT: usize = 3;
    const MASTER_SQL: usize = 4;

    struct Db {
        data: Vec<u8>,
        page_size: usize,
        usable: usize,
    }

    impl Db {
        fn open(path: &Path) -> Option<Self> {
            let meta = std::fs::metadata(path).ok()?;
            if !meta.is_file() || meta.len() > MAX_DB_BYTES {
                return None;
            }
            let data = std::fs::read(path).ok()?;
            if data.len() < 100 || &data[..16] != MAGIC {
                return None;
            }
            let raw = u16::from_be_bytes([data[16], data[17]]) as usize;
            // 1 は 65536 の符牒 (16bit に収まらないため)。
            let page_size = if raw == 1 { 65_536 } else { raw };
            if page_size < 512 || !page_size.is_power_of_two() {
                return None;
            }
            let reserved = data[20] as usize;
            if reserved >= page_size {
                return None;
            }
            Some(Db {
                usable: page_size - reserved,
                page_size,
                data,
            })
        }

        /// 1 始まりのページ番号でスライスを返す。
        fn page(&self, n: usize) -> Option<&[u8]> {
            let start = n.checked_sub(1)?.checked_mul(self.page_size)?;
            self.data.get(start..start.checked_add(self.page_size)?)
        }

        /// テーブル b-tree を根から辿り、葉セルのレコードを集める。
        fn table_rows(&self, root: u32, max_rows: usize) -> Vec<Vec<Cell>> {
            let mut rows = Vec::new();
            let mut stack = vec![root];
            let mut budget = MAX_PAGES;
            while let Some(pn) = stack.pop() {
                if budget == 0 || rows.len() >= max_rows {
                    break;
                }
                budget -= 1;
                let Some(page) = self.page(pn as usize) else {
                    continue;
                };
                // ページ 1 だけは先頭 100 バイトがファイルヘッダ。
                let off = if pn == 1 { 100 } else { 0 };
                let Some(hdr) = page.get(off..off + 12) else {
                    continue;
                };
                let ptype = hdr[0];
                let ncell = u16::from_be_bytes([hdr[3], hdr[4]]) as usize;
                let hdr_len = if ptype == 0x05 || ptype == 0x02 {
                    12
                } else {
                    8
                };
                // セルポインタ配列 (オフセットは「ページ先頭」からの相対)。
                let mut ptrs = Vec::with_capacity(ncell.min(4096));
                for i in 0..ncell {
                    let at = off + hdr_len + i * 2;
                    let Some(b) = page.get(at..at + 2) else { break };
                    ptrs.push(u16::from_be_bytes([b[0], b[1]]) as usize);
                }
                match ptype {
                    0x0d => {
                        // テーブルの葉
                        for p in ptrs {
                            if rows.len() >= max_rows {
                                break;
                            }
                            if let Some(r) = self.leaf_cell(page, p) {
                                rows.push(r);
                            }
                        }
                    }
                    0x05 => {
                        // テーブルの内部ノード: 右端ポインタ + 各セルの左子
                        stack.push(u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]));
                        for p in ptrs {
                            if let Some(b) = page.get(p..p + 4) {
                                stack.push(u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
                            }
                        }
                    }
                    _ => {} // インデックスページは対象外
                }
            }
            rows
        }

        fn leaf_cell(&self, page: &[u8], at: usize) -> Option<Vec<Cell>> {
            let mut pos = at;
            let payload_len = varint(page, &mut pos)?;
            if payload_len < 0 {
                return None;
            }
            let _rowid = varint(page, &mut pos)?;
            let payload = self.read_payload(page, pos, payload_len as usize)?;
            Some(decode_record(&payload))
        }

        /// ローカル領域 + オーバーフローチェーンからペイロードを組み立てる。
        /// 分割位置の式は SQLite のファイルフォーマット仕様そのまま。
        fn read_payload(&self, page: &[u8], start: usize, total: usize) -> Option<Vec<u8>> {
            if total > MAX_PAYLOAD {
                return None;
            }
            let usable = self.usable;
            let max_local = usable.checked_sub(35)?;
            if total <= max_local {
                return page.get(start..start + total).map(<[u8]>::to_vec);
            }
            let min_local = ((usable - 12) * 32 / 255).checked_sub(23)?;
            let k = min_local + (total - min_local) % (usable - 4);
            let local = if k <= max_local { k } else { min_local };
            let mut out = page.get(start..start + local)?.to_vec();
            let ptr = page.get(start + local..start + local + 4)?;
            let mut next = u32::from_be_bytes([ptr[0], ptr[1], ptr[2], ptr[3]]);
            let mut guard = MAX_PAGES;
            while next != 0 && out.len() < total && guard > 0 {
                guard -= 1;
                let p = self.page(next as usize)?;
                next = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
                let take = (total - out.len()).min(usable - 4);
                out.extend_from_slice(p.get(4..4 + take)?);
            }
            (out.len() == total).then_some(out)
        }
    }

    /// テーブルを 1 つ読む。戻り値は (列名, 行)。
    ///
    /// 列名は `sqlite_master.sql` の CREATE 文から取り出すので、呼び出し側は
    /// **位置ではなく名前** で列を引ける (列が増減しても壊れない)。
    pub fn read_table(
        path: &Path,
        table: &str,
        max_rows: usize,
    ) -> Option<(Vec<String>, Vec<Vec<Cell>>)> {
        let db = Db::open(path)?;
        for row in db.table_rows(1, MAX_MASTER_ROWS) {
            if row.len() <= MASTER_SQL {
                continue;
            }
            if row[MASTER_TYPE].text() != "table" || row[MASTER_NAME].text() != table {
                continue;
            }
            let Cell::Int(root) = row[MASTER_ROOT] else {
                continue;
            };
            if root <= 0 {
                continue;
            }
            let cols = column_names(&row[MASTER_SQL].text());
            return Some((cols, db.table_rows(root as u32, max_rows)));
        }
        None
    }

    /// `CREATE TABLE x (a int, b text, PRIMARY KEY(a))` → `["a", "b"]`。
    pub fn column_names(sql: &str) -> Vec<String> {
        let Some(open) = sql.find('(') else {
            return Vec::new();
        };
        let mut depth = 0i32;
        let mut quote: Option<char> = None;
        let mut cur = String::new();
        let mut parts: Vec<String> = Vec::new();
        for ch in sql[open + 1..].chars() {
            if let Some(q) = quote {
                cur.push(ch);
                if ch == q {
                    quote = None;
                }
                continue;
            }
            match ch {
                '`' | '"' | '\'' => {
                    quote = Some(ch);
                    cur.push(ch);
                }
                '[' => {
                    quote = Some(']');
                    cur.push(ch);
                }
                '(' => {
                    depth += 1;
                    cur.push(ch);
                }
                ')' if depth == 0 => break,
                ')' => {
                    depth -= 1;
                    cur.push(ch);
                }
                ',' if depth == 0 => parts.push(std::mem::take(&mut cur)),
                _ => cur.push(ch),
            }
        }
        parts.push(cur);
        parts.iter().filter_map(|p| first_ident(p)).collect()
    }

    /// 列定義の先頭にある識別子。テーブル制約 (PRIMARY KEY 等) なら None。
    fn first_ident(part: &str) -> Option<String> {
        let t = part.trim();
        let mut chars = t.chars();
        let close = match chars.next()? {
            '`' => Some('`'),
            '"' => Some('"'),
            '\'' => Some('\''),
            '[' => Some(']'),
            _ => None,
        };
        if let Some(close) = close {
            let rest: String = chars.collect();
            let end = rest.find(close)?;
            let name = rest[..end].trim().to_string();
            return (!name.is_empty()).then_some(name);
        }
        let name: String = t
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
            .collect();
        if name.is_empty() {
            return None;
        }
        const TABLE_CONSTRAINTS: &[&str] =
            &["primary", "unique", "check", "foreign", "constraint", "key"];
        let lower = name.to_ascii_lowercase();
        if TABLE_CONSTRAINTS.contains(&lower.as_str()) {
            return None;
        }
        Some(name)
    }

    /// SQLite の可変長整数 (最大 9 バイト、上位 7bit ずつ / 9 バイト目は 8bit)。
    fn varint(b: &[u8], pos: &mut usize) -> Option<i64> {
        let mut v: u64 = 0;
        for i in 0..9 {
            let byte = *b.get(*pos)?;
            *pos += 1;
            if i == 8 {
                v = (v << 8) | byte as u64;
            } else {
                v = (v << 7) | (byte & 0x7f) as u64;
                if byte & 0x80 == 0 {
                    break;
                }
            }
        }
        Some(v as i64)
    }

    /// レコード形式 (ヘッダ = serial type の並び、その後ろに本体) をほどく。
    fn decode_record(payload: &[u8]) -> Vec<Cell> {
        let mut pos = 0usize;
        let Some(hdr_size) = varint(payload, &mut pos) else {
            return Vec::new();
        };
        let hdr_end = hdr_size.max(0) as usize;
        if hdr_end > payload.len() {
            return Vec::new();
        }
        let mut types = Vec::new();
        while pos < hdr_end {
            let Some(t) = varint(payload, &mut pos) else {
                break;
            };
            types.push(t);
        }
        let mut body = hdr_end;
        let mut out = Vec::with_capacity(types.len());
        for t in types {
            let (cell, len) = match t {
                0 => (Cell::Null, 0usize),
                1..=4 => (int_at(payload, body, t as usize), t as usize),
                5 => (int_at(payload, body, 6), 6),
                6 => (int_at(payload, body, 8), 8),
                7 => (
                    payload
                        .get(body..body + 8)
                        .and_then(|b| b.try_into().ok())
                        .map(|b| Cell::Real(f64::from_be_bytes(b)))
                        .unwrap_or(Cell::Null),
                    8,
                ),
                8 => (Cell::Int(0), 0),
                9 => (Cell::Int(1), 0),
                n if n >= 12 && n % 2 == 0 => {
                    let len = (n as usize - 12) / 2;
                    (
                        payload
                            .get(body..body + len)
                            .map(|b| Cell::Blob(b.to_vec()))
                            .unwrap_or(Cell::Null),
                        len,
                    )
                }
                n if n >= 13 => {
                    let len = (n as usize - 13) / 2;
                    (
                        payload
                            .get(body..body + len)
                            .map(|b| Cell::Text(String::from_utf8_lossy(b).into_owned()))
                            .unwrap_or(Cell::Null),
                        len,
                    )
                }
                _ => (Cell::Null, 0),
            };
            out.push(cell);
            body = body.saturating_add(len);
        }
        out
    }

    /// ビッグエンディアン・2 の補数の整数を符号拡張して読む。
    fn int_at(payload: &[u8], at: usize, len: usize) -> Cell {
        let Some(b) = payload.get(at..at + len) else {
            return Cell::Null;
        };
        let mut v: i64 = if b.first().is_some_and(|x| x & 0x80 != 0) {
            -1
        } else {
            0
        };
        for &x in b {
            v = (v << 8) | x as i64;
        }
        Cell::Int(v)
    }
}

// ═══════════════════════ サイドバー: 相対時刻・フォルダ整列 ═══════════════════

/// 相対時刻の単位 (表示は [`relative_age`]、判定はここ)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgeUnit {
    /// 1 分未満 (未来の時刻もここへ丸める)。
    Now,
    Minutes,
    Hours,
    Days,
    Years,
}

/// 相対時刻を (数値, 単位) に分ける純粋関数。切り捨て (119 秒 = 1 分)。
///
/// 未来の時刻 (時計のずれ・タイムゾーン誤差) は負にせず [`AgeUnit::Now`] に倒す。
pub fn age_parts(now: SystemTime, then: SystemTime) -> (u64, AgeUnit) {
    let secs = now.duration_since(then).map(|d| d.as_secs()).unwrap_or(0); // then が未来 → 0
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    const YEAR: u64 = 365 * DAY;
    if secs < MIN {
        (secs, AgeUnit::Now)
    } else if secs < HOUR {
        (secs / MIN, AgeUnit::Minutes)
    } else if secs < DAY {
        (secs / HOUR, AgeUnit::Hours)
    } else if secs < YEAR {
        (secs / DAY, AgeUnit::Days)
    } else {
        (secs / YEAR, AgeUnit::Years)
    }
}

/// 一覧の右端に出す短い相対時刻。「3分」「5時間」「2日」「1年」、1 分未満は「今」。
///
/// 英語圏の `2d` / `4d` に相当する日本語の最短表記として、単位は
/// 分 / 時間 / 日 / 年 の 4 段階に絞っている (秒は出さない = 「今」に丸める)。
pub fn relative_age(now: SystemTime, then: SystemTime) -> String {
    let (n, unit) = age_parts(now, then);
    let arg = [("n", n.to_string())];
    match unit {
        AgeUnit::Now => crate::i18n::tr("今"),
        AgeUnit::Minutes => crate::i18n::trf("{n}分", &arg),
        AgeUnit::Hours => crate::i18n::trf("{n}時間", &arg),
        AgeUnit::Days => crate::i18n::trf("{n}日", &arg),
        AgeUnit::Years => crate::i18n::trf("{n}年", &arg),
    }
}

/// 行頭の点を付ける「新しさ」の窓 = 24 時間。
///
/// どの保存先も **既読 / 未読を持たない** ため、点は未読ではなく
/// 「24 時間以内に更新された会話」を意味する (UI 側のツールチップでもそう説明する)。
pub const FRESH_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// その行に点を付けるか。
pub fn is_fresh(now: SystemTime, session: &PastSession) -> bool {
    now.duration_since(session.modified)
        .map(|d| d < FRESH_WINDOW)
        .unwrap_or(true) // 未来 = ついさっき書かれたとみなす
}

/// サイドバーに並べるフォルダの上限。
pub const MAX_SIDEBAR_FOLDERS: usize = 16;

/// サイドバーのフォルダ順を決める。
///
/// 対象は **いま開いているワークスペースのルートだけ**。MRU や、同じリポジトリの
/// 他ブランチ (worktree) は混ぜない — 「開いているフォルダで交わした会話だけが
/// 出る」という一本の規則にするため (VS Code の Claude Code 拡張と同じ切り方)。
///
/// 1. `open_roots` を与えられた順に置く。同じフォルダは 1 度だけ。
/// 2. 実在しないディレクトリは落とす (消えたフォルダの見出しを残さない)。
/// 3. [`MAX_SIDEBAR_FOLDERS`] 件で打ち切る。
///
/// `is_dir` を見るので毎フレームではなく「フォルダ集合が変わったとき」に呼ぶこと。
pub fn sidebar_folders(open_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in open_roots.iter() {
        if out.len() >= MAX_SIDEBAR_FOLDERS {
            break;
        }
        if out.iter().any(|q| same_dir(q, p)) {
            continue;
        }
        if !p.is_dir() {
            continue;
        }
        out.push(p.clone());
    }
    out
}

// ═══════════════════════ サイドバー: 状態とアクション ═══════════════════════

/// 折りたたまずに出す 1 フォルダあたりの行数。残りは「すべて表示」で開く。
pub const FOLDER_ROW_CAP: usize = 6;
/// 走査結果を作り直す間隔。会話は数十秒単位でしか増えないので長めでよい。
pub const SIDEBAR_TTL: Duration = Duration::from_secs(20);

/// サイドバーの 1 フレームぶんの結果。呼び出し側 (app.rs) がこれを実行する。
#[derive(Clone, Debug, PartialEq, Default)]
pub enum SidebarAction {
    /// 何も起きなかった。
    #[default]
    None,
    /// この過去セッションを再開する ([`resume_command`] を通してから起動)。
    Resume(PastSession),
    /// このフォルダで新しい会話を始める。
    NewConversation(PathBuf),
    /// このフォルダを OS のファイラで開く。
    RevealFolder(PathBuf),
    /// このフォルダをサイドバー (= 開いているルート) から外す。
    CloseFolder(PathBuf),
}

/// 1 回の走査の戻り (フォルダ → そのフォルダの過去セッション)。
pub type ScanResults = Vec<(PathBuf, Vec<PastSession>)>;

/// サイドバーの表示状態 + 走査結果のキャッシュ。
///
/// **走査は必ずバックグラウンドスレッドで行う。** UI スレッドからは
/// [`SidebarState::sessions_for`] などキャッシュを読むメソッドしか呼ばない。
pub struct SidebarState {
    /// 折りたたみ中のフォルダ (既定は開いている)。
    collapsed: HashSet<PathBuf>,
    /// 「すべて表示」中のフォルダ。
    show_all: HashSet<PathBuf>,
    /// フォルダ → 過去セッション (新しい順)。
    cache: HashMap<PathBuf, Vec<PastSession>>,
    /// 最後に走査を投げたフォルダ集合。ここが変われば TTL 前でも投げ直す。
    scanned: Vec<PathBuf>,
    /// 最後に走査を **開始** した時刻。
    started_at: Option<Instant>,
    /// 走査中のスレッドからの受け口。
    pending: Option<Receiver<ScanResults>>,
    /// 再走査の間隔 (テストから差し替えられるように公開している)。
    pub ttl: Duration,
    /// リストのスクロール位置 (タブを行き来しても保つ)。
    pub scroll: f32,
    /// 走査に使うホーム。`None` なら `dirs::home_dir()` (本番はこちら)。
    /// テストは [`SidebarState::with_home`] で一時ディレクトリを差し込む。
    home: Option<PathBuf>,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            collapsed: HashSet::new(),
            show_all: HashSet::new(),
            cache: HashMap::new(),
            scanned: Vec::new(),
            started_at: None,
            pending: None,
            ttl: SIDEBAR_TTL,
            scroll: 0.0,
            home: None,
        }
    }
}

/// [`SidebarState::plan_refresh`] の判断結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefreshPlan {
    /// 何もしない (走査中 / TTL 内)。
    Idle,
    /// このフォルダ集合を走査し直す。
    Scan,
}

impl SidebarState {
    /// ホームを差し替えた状態 (テスト / 検証用)。
    pub fn with_home(home: PathBuf) -> Self {
        Self {
            home: Some(home),
            ..Self::default()
        }
    }

    /// 走査を投げるべきかの判断 (純粋。ファイルには一切触らない)。
    pub fn plan_refresh(&self, folders: &[PathBuf]) -> RefreshPlan {
        if self.pending.is_some() {
            return RefreshPlan::Idle; // 二重起動しない
        }
        if self.scanned != folders {
            return RefreshPlan::Scan; // 開いているフォルダが変わった
        }
        match self.started_at {
            Some(t) if t.elapsed() < self.ttl => RefreshPlan::Idle,
            _ => RefreshPlan::Scan,
        }
    }

    /// 毎フレーム呼んでよい入口。完了した走査を取り込み、必要なら次を投げる。
    /// **この関数自身はファイルシステムに触らない** (走査は別スレッド)。
    pub fn refresh_if_stale(&mut self, folders: &[PathBuf]) {
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(results) => {
                    self.apply_scan(results);
                    self.pending = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.plan_refresh(folders) == RefreshPlan::Scan {
            self.spawn_scan(folders.to_vec());
        }
    }

    /// 次の [`SidebarState::refresh_if_stale`] で TTL を無視させる。
    pub fn invalidate(&mut self) {
        self.started_at = None;
    }

    /// 走査結果の取り込み (テストはここへ直接流し込める)。
    pub fn apply_scan(&mut self, results: ScanResults) {
        self.cache = results.into_iter().collect();
    }

    fn spawn_scan(&mut self, folders: Vec<PathBuf>) {
        // 失敗しても時刻は進める (毎フレーム spawn を試みない)。
        self.started_at = Some(Instant::now());
        self.scanned = folders.clone();
        let (tx, rx) = mpsc::channel();
        let home_override = self.home.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-sessions".into())
            .spawn(move || {
                let home = home_override.or_else(dirs::home_dir);
                let out: ScanResults = folders
                    .into_iter()
                    .map(|f| {
                        let list = match &home {
                            Some(h) => list_sessions_from(h, &f),
                            None => Vec::new(),
                        };
                        (f, list)
                    })
                    .collect();
                let _ = tx.send(out);
            });
        if spawned.is_ok() {
            self.pending = Some(rx);
        }
    }

    /// 走査中か (UI の「読み込み中…」表示用)。
    pub fn loading(&self) -> bool {
        self.pending.is_some()
    }

    /// まだ一度も結果が入っていないか。
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// そのフォルダの過去セッション (新しい順)。無ければ空スライス。
    pub fn sessions_for(&self, folder: &Path) -> &[PastSession] {
        self.cache.get(folder).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 実際に描く行と、隠れている件数。
    ///
    /// - 折りたたみ中: 0 行 (隠れ件数は全件)
    /// - 「すべて表示」中: 全行
    /// - それ以外: 先頭 [`FOLDER_ROW_CAP`] 行
    pub fn visible_sessions(&self, folder: &Path) -> (&[PastSession], usize) {
        let all = self.sessions_for(folder);
        if self.is_collapsed(folder) {
            return (&[], all.len());
        }
        if self.is_show_all(folder) || all.len() <= FOLDER_ROW_CAP {
            return (all, 0);
        }
        (&all[..FOLDER_ROW_CAP], all.len() - FOLDER_ROW_CAP)
    }

    pub fn is_collapsed(&self, folder: &Path) -> bool {
        self.collapsed.contains(folder)
    }

    pub fn toggle_collapsed(&mut self, folder: &Path) {
        if !self.collapsed.remove(folder) {
            self.collapsed.insert(folder.to_path_buf());
            // 畳んだら「すべて表示」も解除する (開き直したときに元の高さへ戻す)。
            self.show_all.remove(folder);
        }
    }

    pub fn is_show_all(&self, folder: &Path) -> bool {
        self.show_all.contains(folder)
    }

    pub fn toggle_show_all(&mut self, folder: &Path) {
        if !self.show_all.remove(folder) {
            self.show_all.insert(folder.to_path_buf());
        }
    }
}

#[cfg(test)]
mod tests {
    // ── アプリ自身の履歴からの一覧 (ベンダーが公開していない相手を補う) ──

    /// 履歴 1 件を作る補助。
    fn hist(bin: &str, id: u64, started: i64, ended: i64, brief: &str) -> crate::history::Entry {
        crate::history::Entry {
            id,
            agent_bin: bin.into(),
            preset_name: "テスト".into(),
            title: format!("{bin} #{id}"),
            icon: "🤖".into(),
            command: bin.into(),
            cwd: "/w".into(),
            log_file: String::new(),
            started,
            ended,
            brief: brief.into(),
            vendor_id: String::new(),
        }
    }

    #[test]
    fn 履歴の行を一覧の形へ変換できる() {
        let out = super::entries_to_sessions(vec![hist("gemini", 1, 100, 200, "テストを直して")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].agent_bin, "gemini");
        assert_eq!(out[0].summary, "テストを直して");
        // 終了時刻が入っていれば並び順はそちらで決まる。
        assert_eq!(
            out[0].modified,
            std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(200)
        );
    }

    /// 要約が空なら**タイトルを出す** (空行を並べない)。
    #[test]
    fn 要約が空ならタイトルで埋める() {
        let out = super::entries_to_sessions(vec![hist("droid", 7, 100, 0, "")]);
        assert_eq!(out[0].summary, "droid #7");
    }

    /// まだ終わっていない (ended = 0) 行は開始時刻で並べる。
    /// 0 のまま並べると全部が最下位へ落ちて「新しい会話ほど下」になる。
    #[test]
    fn 終了時刻が無ければ開始時刻で並べる() {
        let out = super::entries_to_sessions(vec![hist("aider", 1, 500, 0, "x")]);
        assert_eq!(
            out[0].modified,
            std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(500)
        );
    }

    /// 壊れた時刻 (負) でも panic しない。
    #[test]
    fn 負の時刻でも落ちない() {
        let out = super::entries_to_sessions(vec![hist("aider", 1, -1, -1, "x")]);
        assert_eq!(out[0].modified, std::time::SystemTime::UNIX_EPOCH);
    }

    /// bin が空の行は捨てる (どのエージェントか判らないと再開できない)。
    #[test]
    fn bin_が空の履歴は捨てる() {
        let mut e = hist("", 1, 1, 2, "x");
        e.agent_bin = String::new();
        assert!(super::entries_to_sessions(vec![e]).is_empty());
    }

    /// **ID が判らない相手は「直前の会話を続ける」フラグへ落とす。**
    /// ここが無いと「一覧には出るのに再開しない」になる。
    #[test]
    fn 再開_id_が無ければ直前の会話を続けるフラグを付ける() {
        let s = super::entries_to_sessions(vec![hist("gemini", 1, 1, 2, "x")])
            .pop()
            .expect("1 件");
        let cmd = resume_command("gemini", &s);
        let spec = crate::agents::spec_for_bin("gemini").expect("カタログにある");
        assert_eq!(cmd, crate::agents::apply_resume("gemini", spec));
        assert_ne!(cmd, "gemini", "素のコマンドのままでは再開にならない");
    }

    /// 再開フラグを持たない CLI では素のコマンドのまま (作業フォルダだけ引き継ぐ)。
    #[test]
    fn 再開フラグが無い_cli_は素のコマンドのまま() {
        let s = super::entries_to_sessions(vec![hist("aider", 1, 1, 2, "x")])
            .pop()
            .expect("1 件");
        assert_eq!(resume_command("aider", &s), "aider");
    }

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
        let got = list_claude_sessions(
            &claude_projects_root(&home),
            Path::new("/no/such/ws"),
            "claude",
        );
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
        let day = codex_sessions_root(&home)
            .join("2026")
            .join("07")
            .join("25");

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
        let day = codex_sessions_root(&home)
            .join("2026")
            .join("07")
            .join("25");
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
        assert_eq!(
            resume_command("codex --yolo", &c),
            "codex resume abc-123 --yolo"
        );
        // 未知の bin は素通し
        let u = PastSession {
            agent_bin: "totally-unknown".into(),
            ..s.clone()
        };
        assert_eq!(resume_command("whatever --x", &u), "whatever --x");
    }

    // ── Antigravity: ローカル表経由の再開 ────────────────────────

    #[test]
    fn resume_command_uses_local_table_for_antigravity() {
        let s = PastSession {
            id: "1d613a7f-31c9-44a1-8028-ab36d2692cd3".into(),
            agent_bin: "agy".into(),
            started: UNIX_EPOCH,
            modified: UNIX_EPOCH,
            summary: String::new(),
            cwd: PathBuf::from("/ws"),
        };
        assert_eq!(
            resume_command("agy", &s),
            "agy --conversation 1d613a7f-31c9-44a1-8028-ab36d2692cd3"
        );
        // 既に指定済みなら二重に付けない
        assert_eq!(
            resume_command("agy --conversation zzz", &s),
            "agy --conversation zzz"
        );
        // 危険な ID は素通し (シェルへ渡らない)
        let bad = PastSession {
            id: "a; rm -rf /".into(),
            ..s.clone()
        };
        assert_eq!(resume_command("agy", &bad), "agy");
    }
}

// ═══════════════════════ テスト: Antigravity 保存先 ═══════════════════════

#[cfg(test)]
mod agy_tests {
    use super::sqlite_lite::Cell;
    use super::*;
    use crate::test_util::unique_temp_dir;

    /// フィクスチャに入れる値。
    #[derive(Clone)]
    pub(crate) enum V {
        T(String),
        I(i64),
    }
    fn t(s: &str) -> V {
        V::T(s.to_string())
    }

    /// 実機の DDL をそのまま縮めたもの (列の順序も実機どおり)。
    const DDL: &str = concat!(
        "CREATE TABLE `conversation_summaries` (",
        "`conversation_id` text,",
        "`title` text NOT NULL DEFAULT \"\",",
        "`preview` text NOT NULL DEFAULT \"\",",
        "`last_modified_time` datetime NOT NULL,",
        "`workspace_uris` text NOT NULL,",
        "`last_user_input_time` datetime NOT NULL,",
        "PRIMARY KEY (`conversation_id`))"
    );

    fn put_varint(out: &mut Vec<u8>, v: u64) {
        if v < 0x80 {
            out.push(v as u8);
            return;
        }
        let mut buf = [0u8; 10];
        let mut n = 0;
        let mut x = v;
        while x > 0 {
            buf[n] = (x & 0x7f) as u8;
            x >>= 7;
            n += 1;
        }
        for i in (0..n).rev() {
            out.push(if i > 0 { buf[i] | 0x80 } else { buf[i] });
        }
    }

    fn varint_len(v: u64) -> usize {
        let mut b = Vec::new();
        put_varint(&mut b, v);
        b.len()
    }

    /// レコード (ヘッダ = serial type 列 + 本体) を組み立てる。
    fn record(vals: &[V]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut body = Vec::new();
        for v in vals {
            match v {
                V::T(s) => {
                    types.push(13 + 2 * s.len() as u64);
                    body.extend_from_slice(s.as_bytes());
                }
                V::I(i) => {
                    types.push(6); // 8 バイト整数で統一する
                    body.extend_from_slice(&i.to_be_bytes());
                }
            }
        }
        let mut tbytes = Vec::new();
        for ty in &types {
            put_varint(&mut tbytes, *ty);
        }
        // ヘッダ長は自分自身の varint も含む → 不動点を求める
        let mut hsize = tbytes.len() + 1;
        loop {
            let want = tbytes.len() + varint_len(hsize as u64);
            if want == hsize {
                break;
            }
            hsize = want;
        }
        let mut out = Vec::new();
        put_varint(&mut out, hsize as u64);
        out.extend_from_slice(&tbytes);
        out.extend_from_slice(&body);
        out
    }

    /// テーブル葉セル。溢れる分はオーバーフローページへ (SQLite の分割式そのまま)。
    fn make_cell(
        payload: Vec<u8>,
        rowid: u64,
        usable: usize,
        ov: &mut Vec<Vec<u8>>,
        ov_first_page: usize,
    ) -> Vec<u8> {
        let total = payload.len();
        let mut out = Vec::new();
        put_varint(&mut out, total as u64);
        put_varint(&mut out, rowid);
        let max_local = usable - 35;
        if total <= max_local {
            out.extend_from_slice(&payload);
            return out;
        }
        let min_local = ((usable - 12) * 32 / 255) - 23;
        let k = min_local + (total - min_local) % (usable - 4);
        let local = if k <= max_local { k } else { min_local };
        out.extend_from_slice(&payload[..local]);
        let first = ov_first_page + ov.len();
        let chunks: Vec<&[u8]> = payload[local..].chunks(usable - 4).collect();
        let n = chunks.len();
        for (i, ch) in chunks.into_iter().enumerate() {
            let next = if i + 1 < n { (first + i + 1) as u32 } else { 0 };
            let mut p = vec![0u8; usable];
            p[..4].copy_from_slice(&next.to_be_bytes());
            p[4..4 + ch.len()].copy_from_slice(ch);
            ov.push(p);
        }
        out.extend_from_slice(&(first as u32).to_be_bytes());
        out
    }

    fn leaf_page(page_size: usize, hdr_off: usize, cells: &[Vec<u8>]) -> Vec<u8> {
        let mut page = vec![0u8; page_size];
        let mut content = page_size;
        let mut ptrs = Vec::new();
        for c in cells {
            content -= c.len();
            page[content..content + c.len()].copy_from_slice(c);
            ptrs.push(content as u16);
        }
        let h = hdr_off;
        page[h] = 0x0d; // テーブルの葉
        page[h + 3..h + 5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
        page[h + 5..h + 7].copy_from_slice(&(content as u16).to_be_bytes());
        for (i, p) in ptrs.iter().enumerate() {
            let at = h + 8 + i * 2;
            page[at..at + 2].copy_from_slice(&p.to_be_bytes());
        }
        page
    }

    /// 最小の SQLite ファイルを組み立てる (ページ 1 = sqlite_master、ページ 2 = 本体)。
    pub(crate) fn build_db(page_size: usize, table: &str, ddl: &str, rows: &[Vec<V>]) -> Vec<u8> {
        let usable = page_size; // reserved = 0
        let mut ov: Vec<Vec<u8>> = Vec::new();
        let data_cells: Vec<Vec<u8>> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| make_cell(record(r), i as u64 + 1, usable, &mut ov, 3))
            .collect();

        let master = record(&[
            t("table"),
            t(table),
            t(table),
            V::I(2),
            V::T(ddl.to_string()),
        ]);
        let mut master_ov = Vec::new();
        let master_cell = make_cell(master, 1, usable, &mut master_ov, 3 + ov.len());
        assert!(master_ov.is_empty(), "テストの DDL は 1 ページに収める");

        let mut page1 = leaf_page(page_size, 100, &[master_cell]);
        let page2 = leaf_page(page_size, 0, &data_cells);
        let total_pages = 2 + ov.len();

        // ファイルヘッダ (先頭 100 バイト)
        page1[..16].copy_from_slice(b"SQLite format 3\0");
        let ps = if page_size == 65_536 {
            1u16
        } else {
            page_size as u16
        };
        page1[16..18].copy_from_slice(&ps.to_be_bytes());
        page1[18] = 1;
        page1[19] = 1;
        page1[20] = 0; // reserved
        page1[21] = 64;
        page1[22] = 32;
        page1[23] = 32;
        page1[28..32].copy_from_slice(&(total_pages as u32).to_be_bytes());
        page1[56..60].copy_from_slice(&1u32.to_be_bytes()); // UTF-8

        let mut out = page1;
        out.extend_from_slice(&page2);
        for p in ov {
            out.extend_from_slice(&p);
        }
        out
    }

    fn file_uri(p: &Path) -> String {
        let s = p.display().to_string().replace('\\', "/");
        if s.starts_with('/') {
            format!("file://{s}")
        } else {
            format!("file:///{s}")
        }
    }

    fn row(id: &str, title: &str, preview: &str, modified: &str, ws: &[String]) -> Vec<V> {
        vec![
            t(id),
            t(title),
            t(preview),
            t(modified),
            V::T(serde_json::to_string(ws).unwrap()),
            t("0001-01-01 00:00:00+00:00"), // Go のゼロ値 (= 無効)
        ]
    }

    fn write_db(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    // ── 列挙 ─────────────────────────────────────────────────────

    #[test]
    fn antigravity_filters_by_workspace_and_sorts_newest_first() {
        let home = unique_temp_dir("zaivern-picker", "agy-basic");
        let ws = home.join("ws");
        let other = home.join("other");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&other).unwrap();

        let rows = vec![
            row(
                "aaa-1111",
                "",
                "プレビュー由来の見出し",
                "2026-07-17 06:06:57.519246+00:00",
                &[file_uri(&ws)],
            ),
            row(
                "bbb-2222",
                "タイトルが優先される",
                "こちらは使われない",
                "2026-07-18 06:06:57.000000+00:00",
                &[file_uri(&ws)],
            ),
            row(
                "ccc-3333",
                "",
                "別ワークスペース",
                "2026-07-19 00:00:00.000000+00:00",
                &[file_uri(&other)],
            ),
            // 複数ワークスペースのうち 1 つが一致すれば拾う
            row(
                "ddd-4444",
                "",
                "マルチルート",
                "2026-07-16 00:00:00.000000+00:00",
                &[file_uri(&other), file_uri(&ws)],
            ),
            // ID が空の行は捨てる
            row(
                "",
                "",
                "壊れた行",
                "2026-07-20 00:00:00.000000+00:00",
                &[file_uri(&ws)],
            ),
        ];
        let db = home.join("store.db");
        write_db(&db, &build_db(4096, "conversation_summaries", DDL, &rows));

        let got = list_antigravity_sessions(&db, &ws, "agy");
        let ids: Vec<&str> = got.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["bbb-2222", "aaa-1111", "ddd-4444"]);
        assert_eq!(got[0].summary, "タイトルが優先される");
        assert_eq!(got[1].summary, "プレビュー由来の見出し");
        assert_eq!(got[0].agent_bin, "agy");
        assert_eq!(got[0].cwd, ws);
        // 時刻列は +00:00 付きのテキスト
        assert_eq!(
            got[1].modified,
            parse_sql_datetime("2026-07-17 06:06:57+00:00").unwrap()
        );
        // last_user_input_time がゼロ値なので started は modified に落ちる
        assert_eq!(got[1].started, got[1].modified);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn antigravity_percent_encoded_and_multiroot_uris() {
        let home = unique_temp_dir("zaivern-picker", "agy-uri");
        let ws = home.join("my ws");
        std::fs::create_dir_all(&ws).unwrap();
        let encoded = file_uri(&ws).replace(' ', "%20");
        assert!(encoded.contains("%20"));
        let rows = vec![row(
            "eee-5555",
            "",
            "空白入りパス",
            "2026-07-17 06:06:57+00:00",
            &[encoded],
        )];
        let db = home.join("store.db");
        write_db(&db, &build_db(4096, "conversation_summaries", DDL, &rows));
        let got = list_antigravity_sessions(&db, &ws, "agy");
        assert_eq!(got.len(), 1, "パーセントエンコードが戻せていない");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn antigravity_missing_or_broken_store_is_empty() {
        let home = unique_temp_dir("zaivern-picker", "agy-none");
        std::fs::create_dir_all(&home).unwrap();
        let ws = home.join("ws");

        // ファイルが無い
        assert!(list_antigravity_sessions(&home.join("nope.db"), &ws, "agy").is_empty());
        // SQLite ですらない
        let junk = home.join("junk.db");
        std::fs::write(&junk, b"not a database at all, really").unwrap();
        assert!(list_antigravity_sessions(&junk, &ws, "agy").is_empty());
        // 目的のテーブルが無い
        let other = home.join("other.db");
        write_db(
            &other,
            &build_db(
                4096,
                "something_else",
                "CREATE TABLE `something_else` (`a` text)",
                &[vec![t("x")]],
            ),
        );
        assert!(list_antigravity_sessions(&other, &ws, "agy").is_empty());
        // 必須列が欠けている (形式が変わったら推測せず空を返す)
        let thin = home.join("thin.db");
        write_db(
            &thin,
            &build_db(
                4096,
                "conversation_summaries",
                "CREATE TABLE `conversation_summaries` (`conversation_id` text,`preview` text)",
                &[vec![t("id-1"), t("見出し")]],
            ),
        );
        assert!(list_antigravity_sessions(&thin, &ws, "agy").is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    // ── SQLite リーダ単体 ────────────────────────────────────────

    #[test]
    fn sqlite_reads_overflow_payloads() {
        let home = unique_temp_dir("zaivern-picker", "agy-overflow");
        std::fs::create_dir_all(&home).unwrap();
        // 512 バイトページなら maxLocal = 477。1500 バイトの本文は必ず溢れる。
        let long: String = "あ".repeat(500); // UTF-8 で 1500 バイト
        assert!(long.len() > 512 - 35);
        let rows = vec![vec![t("id-1"), V::T(long.clone()), V::I(-42)]];
        let db = home.join("ov.db");
        write_db(
            &db,
            &build_db(
                512,
                "t",
                "CREATE TABLE `t` (`id` text, `body` text, `n` integer)",
                &rows,
            ),
        );
        let (cols, got) = sqlite_lite::read_table(&db, "t", 100).expect("読めない");
        assert_eq!(cols, vec!["id", "body", "n"]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0][1], Cell::Text(long));
        assert_eq!(got[0][2], Cell::Int(-42), "負数の符号拡張");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn sqlite_column_names_from_ddl() {
        let cases: &[(&str, &[&str])] = &[
            (
                DDL,
                &[
                    "conversation_id",
                    "title",
                    "preview",
                    "last_modified_time",
                    "workspace_uris",
                    "last_user_input_time",
                ],
            ),
            ("CREATE TABLE t (a INT, b TEXT)", &["a", "b"]),
            (
                "CREATE TABLE t (a NUMERIC(10, 2), \"b c\" TEXT, PRIMARY KEY (a), UNIQUE (a))",
                &["a", "b c"],
            ),
            ("CREATE TABLE t ([x y] TEXT, `z` TEXT)", &["x y", "z"]),
            (
                "CREATE TABLE t (a TEXT DEFAULT '(,)', b TEXT, FOREIGN KEY (b) REFERENCES u(b))",
                &["a", "b"],
            ),
            ("CREATE TABLE t", &[]),
        ];
        for (sql, want) in cases {
            let got = sqlite_lite::column_names(sql);
            assert_eq!(got, *want, "sql={sql}");
        }
    }

    // ── file:// URI とタイムスタンプ ─────────────────────────────

    #[test]
    fn file_uri_decoding_table() {
        let cases: &[(&str, Option<&str>)] = &[
            ("file:///Users/me/ws", Some("/Users/me/ws")),
            ("file:///Users/me/my%20ws", Some("/Users/me/my ws")),
            (
                "file:///Users/me/%E9%96%8B%E7%99%BA",
                Some("/Users/me/開発"),
            ),
            // 壊れたエスケープはそのまま残す
            ("file:///a/%zz", Some("/a/%zz")),
            ("vscode://file/x", None),
            ("/no/scheme", None),
        ];
        for (uri, want) in cases {
            let got = path_from_file_uri(uri);
            match want {
                Some(w) => assert_eq!(
                    got,
                    Some(PathBuf::from(normalize_uri_separators(w.to_string()))),
                    "uri={uri}"
                ),
                None => assert!(got.is_none(), "uri={uri}"),
            }
        }
        // Windows のドライブ表記は先頭スラッシュを落とす
        let win = path_from_file_uri("file:///C:/work/proj").unwrap();
        assert!(win.to_string_lossy().starts_with("C:"), "{win:?}");
    }

    #[test]
    fn sql_datetime_parsing_table() {
        // 空白区切り + オフセット
        assert_eq!(
            parse_sql_datetime("1970-01-01 00:00:00.000000+00:00").unwrap(),
            UNIX_EPOCH
        );
        // +09:00 は 9 時間ぶん引いて UTC にする
        assert_eq!(
            parse_sql_datetime("1970-01-02 09:00:00+09:00").unwrap(),
            UNIX_EPOCH + Duration::from_secs(86_400)
        );
        // -05:00 は足す
        assert_eq!(
            parse_sql_datetime("1970-01-01 19:00:00-05:00").unwrap(),
            UNIX_EPOCH + Duration::from_secs(86_400)
        );
        // T 区切り / Z も同じ経路で通る
        assert_eq!(
            parse_sql_datetime("2001-09-09T01:46:40Z").unwrap(),
            UNIX_EPOCH + Duration::from_secs(1_000_000_000)
        );
        // Go のゼロ値と壊れた値は None
        assert!(parse_sql_datetime("0001-01-01 00:00:00+00:00").is_none());
        assert!(parse_sql_datetime("").is_none());
        assert!(parse_sql_datetime("いつか").is_none());
        // オフセット単体
        assert_eq!(tz_offset_secs("2026-07-17 06:06:57+09:30"), 34_200);
        assert_eq!(tz_offset_secs("2026-07-17 06:06:57.5Z"), 0);
        assert_eq!(tz_offset_secs("2026-07-17 06:06:57"), 0);
    }

    // ── マージ (claude / codex / agy) ────────────────────────────

    #[test]
    fn merged_list_includes_all_three_agents_in_time_order() {
        let home = unique_temp_dir("zaivern-picker", "merge3");
        let ws = home.join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        // claude: mtime 300 / 100
        let cdir = claude_projects_root(&home).join(encode_claude_project_dir(&ws));
        std::fs::create_dir_all(&cdir).unwrap();
        for (name, epoch) in [("c-new", 300u64), ("c-old", 100)] {
            let p = cdir.join(format!("{name}.jsonl"));
            std::fs::write(&p, "{\"type\":\"user\",\"timestamp\":\"1970-01-01T00:00:00.000Z\",\"message\":{\"content\":\"x\"}}\n").unwrap();
            File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(epoch))
                .unwrap();
        }
        // codex: mtime 200
        let day = codex_sessions_root(&home)
            .join("1970")
            .join("01")
            .join("01");
        std::fs::create_dir_all(&day).unwrap();
        let cx = day.join("rollout-x.jsonl");
        let meta = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"x-mid\",\"cwd\":\"{}\"}}}}\n",
            ws.to_string_lossy().replace('\\', "\\\\")
        );
        std::fs::write(&cx, meta).unwrap();
        File::options()
            .write(true)
            .open(&cx)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(200))
            .unwrap();
        // agy: last_modified_time = epoch 250 (00:04:10)
        let agy_db = LOCAL_SESSION_STORES[0].rel_paths[0]
            .iter()
            .fold(home.clone(), |p, seg| p.join(seg));
        write_db(
            &agy_db,
            &build_db(
                4096,
                "conversation_summaries",
                DDL,
                &[row(
                    "a-mid2",
                    "",
                    "アンチグラビティの会話",
                    "1970-01-01 00:04:10+00:00",
                    &[file_uri(&ws)],
                )],
            ),
        );

        let got = list_sessions_from(&home, &ws);
        let ids: Vec<&str> = got.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["c-new", "a-mid2", "x-mid", "c-old"]);
        let bins: Vec<&str> = got.iter().map(|s| s.agent_bin.as_str()).collect();
        assert_eq!(bins, vec!["claude", "agy", "codex", "claude"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn merged_list_is_empty_when_no_store_exists() {
        let home = unique_temp_dir("zaivern-picker", "merge-empty");
        std::fs::create_dir_all(&home).unwrap();
        assert!(list_sessions_from(&home, &home.join("ws")).is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 実機の Antigravity ストアに対する読み取り専用の煙テスト。
    /// 環境依存なので既定では走らせない (`cargo test -- --ignored agy_real`)。
    /// 会話の中身は一切表示せず、件数と ID の形だけを確認する。
    #[test]
    #[ignore = "実機の ~/.gemini が要る環境依存テスト"]
    fn agy_real_store_smoke() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let db = LOCAL_SESSION_STORES[0].rel_paths[0]
            .iter()
            .fold(home, |p, seg| p.join(seg));
        if !db.is_file() {
            eprintln!("スキップ: 保存先が無い");
            return;
        }
        let (cols, rows) = sqlite_lite::read_table(&db, AGY_TABLE, AGY_MAX_ROWS).expect("読めない");
        eprintln!("列数={} 行数={}", cols.len(), rows.len());
        assert!(cols.iter().any(|c| c == AGY_COLUMNS.id));
        assert!(cols.iter().any(|c| c == AGY_COLUMNS.workspaces));
        assert!(!rows.is_empty(), "行が 1 つも読めていない");
        let id_at = cols.iter().position(|c| c == AGY_COLUMNS.id).unwrap();
        let ok = rows
            .iter()
            .filter(|r| r.get(id_at).map(|c| is_safe_local_id(&c.text())) == Some(true))
            .count();
        eprintln!("UUID 形式の行={ok}");
        assert!(ok * 2 > rows.len(), "ID が UUID として読めていない");
    }
}

// ═══════════════════════ テスト: サイドバー ═══════════════════════

#[cfg(test)]
mod sidebar_tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    fn session(id: &str, bin: &str, epoch: u64) -> PastSession {
        PastSession {
            id: id.to_string(),
            agent_bin: bin.to_string(),
            started: UNIX_EPOCH + Duration::from_secs(epoch),
            modified: UNIX_EPOCH + Duration::from_secs(epoch),
            summary: format!("要約 {id}"),
            cwd: PathBuf::from("/ws"),
        }
    }

    // ── 相対時刻 ─────────────────────────────────────────────────

    #[test]
    fn relative_age_boundary_table() {
        let base = UNIX_EPOCH + Duration::from_secs(10_000_000_000);
        let ago = |s: u64| base - Duration::from_secs(s);
        let cases: &[(u64, u64, AgeUnit, &str)] = &[
            (0, 0, AgeUnit::Now, "今"),
            (59, 59, AgeUnit::Now, "今"),
            (60, 1, AgeUnit::Minutes, "1分"),
            (119, 1, AgeUnit::Minutes, "1分"),
            (180, 3, AgeUnit::Minutes, "3分"),
            (3_599, 59, AgeUnit::Minutes, "59分"),
            (3_600, 1, AgeUnit::Hours, "1時間"),
            (5 * 3_600, 5, AgeUnit::Hours, "5時間"),
            (86_399, 23, AgeUnit::Hours, "23時間"),
            (86_400, 1, AgeUnit::Days, "1日"),
            (2 * 86_400, 2, AgeUnit::Days, "2日"),
            (9 * 86_400, 9, AgeUnit::Days, "9日"),
            (364 * 86_400, 364, AgeUnit::Days, "364日"),
            (365 * 86_400, 1, AgeUnit::Years, "1年"),
            (800 * 86_400, 2, AgeUnit::Years, "2年"),
        ];
        for (secs, n, unit, text) in cases {
            let then = ago(*secs);
            assert_eq!(age_parts(base, then), (*n, *unit), "secs={secs}");
            assert_eq!(relative_age(base, then), *text, "secs={secs}");
        }
        // 未来の時刻 (時計のずれ) は負にせず「今」へ丸める
        let future = base + Duration::from_secs(9_999);
        assert_eq!(age_parts(base, future), (0, AgeUnit::Now));
        assert_eq!(relative_age(base, future), "今");
    }

    #[test]
    fn fresh_dot_marks_last_24h_only() {
        let base = UNIX_EPOCH + Duration::from_secs(10_000_000_000);
        let at = |s: u64| PastSession {
            modified: base - Duration::from_secs(s),
            ..session("x", "claude", 0)
        };
        assert!(is_fresh(base, &at(0)));
        assert!(is_fresh(base, &at(86_399)));
        assert!(!is_fresh(base, &at(86_400)));
        assert!(!is_fresh(base, &at(10 * 86_400)));
        // 未来 = ついさっき書かれた扱い
        assert!(is_fresh(
            base,
            &PastSession {
                modified: base + Duration::from_secs(60),
                ..session("x", "claude", 0)
            }
        ));
    }

    // ── フォルダの並び ───────────────────────────────────────────

    #[test]
    fn sidebar_folders_keeps_open_roots_only_dedup_and_drop_missing() {
        let tmp = unique_temp_dir("zaivern-sidebar", "folders");
        let mk = |n: &str| {
            let p = tmp.join(n);
            std::fs::create_dir_all(&p).unwrap();
            p
        };
        let (a, b) = (mk("a"), mk("b"));
        let gone = tmp.join("gone");

        // 開いているルートを与えられた順にそのまま
        assert_eq!(
            sidebar_folders(&[a.clone(), b.clone()]),
            vec![a.clone(), b.clone()]
        );

        // 実在しないディレクトリは落ちる
        assert_eq!(sidebar_folders(&[gone.clone(), a.clone()]), vec![a.clone()]);

        // 重複と末尾区切り違いは同一視する
        let a_slash = PathBuf::from(format!("{}{}", a.display(), std::path::MAIN_SEPARATOR));
        assert_eq!(sidebar_folders(&[a.clone(), a_slash]), vec![a.clone()]);

        // 上限
        let many: Vec<PathBuf> = (0..MAX_SIDEBAR_FOLDERS + 5)
            .map(|i| mk(&format!("m{i}")))
            .collect();
        assert_eq!(sidebar_folders(&many).len(), MAX_SIDEBAR_FOLDERS);

        // 全部空
        assert!(sidebar_folders(&[]).is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 走査結果は **フォルダごとに分かれて** キャッシュされる。
    /// 別 worktree のグループを引いても、そのフォルダの会話しか出ない。
    #[test]
    fn cache_keeps_each_worktree_separate() {
        let main = PathBuf::from("/ws/repo");
        let night = PathBuf::from("/ws/repo/.wt/night");
        let mut st = SidebarState::default();
        st.apply_scan(vec![
            (
                main.clone(),
                vec![PastSession {
                    cwd: main.clone(),
                    ..session("m1", "claude", 10)
                }],
            ),
            (
                night.clone(),
                vec![
                    PastSession {
                        cwd: night.clone(),
                        ..session("n1", "codex", 20)
                    },
                    PastSession {
                        cwd: night.clone(),
                        ..session("n2", "claude", 30)
                    },
                ],
            ),
        ]);
        assert_eq!(st.sessions_for(&main).len(), 1);
        assert_eq!(st.sessions_for(&night).len(), 2);
        // 再開に使う cwd は「その会話が走っていた worktree」のまま
        assert!(st.sessions_for(&night).iter().all(|s| s.cwd == night));
        assert!(st.sessions_for(&PathBuf::from("/ws/nope")).is_empty());
    }

    // ── 行数上限と「すべて表示」 ─────────────────────────────────

    #[test]
    fn row_cap_and_show_all_state_machine() {
        let f = PathBuf::from("/ws/proj");
        let mut st = SidebarState::default();
        let all: Vec<PastSession> = (0..FOLDER_ROW_CAP + 28)
            .map(|i| session(&format!("id-{i:02}"), "claude", 1_000 - i as u64))
            .collect();
        st.apply_scan(vec![(f.clone(), all.clone())]);
        assert_eq!(st.sessions_for(&f).len(), FOLDER_ROW_CAP + 28);

        // 既定は上限まで + 残り件数
        let (rows, hidden) = st.visible_sessions(&f);
        assert_eq!(rows.len(), FOLDER_ROW_CAP);
        assert_eq!(hidden, 28);
        assert_eq!(rows[0].id, "id-00", "新しい順のまま先頭から出す");

        // 「すべて表示」で全件
        st.toggle_show_all(&f);
        assert!(st.is_show_all(&f));
        let (rows, hidden) = st.visible_sessions(&f);
        assert_eq!((rows.len(), hidden), (FOLDER_ROW_CAP + 28, 0));

        // もう一度押すと畳む
        st.toggle_show_all(&f);
        assert_eq!(st.visible_sessions(&f).0.len(), FOLDER_ROW_CAP);

        // 折りたたみ中は 0 行 (隠れ件数は全件)
        st.toggle_show_all(&f);
        st.toggle_collapsed(&f);
        assert!(st.is_collapsed(&f));
        let (rows, hidden) = st.visible_sessions(&f);
        assert_eq!((rows.len(), hidden), (0, FOLDER_ROW_CAP + 28));
        // 畳むと「すべて表示」は解除される (開き直したとき元の高さに戻す)
        assert!(!st.is_show_all(&f));
        st.toggle_collapsed(&f);
        assert_eq!(st.visible_sessions(&f).0.len(), FOLDER_ROW_CAP);

        // 上限ちょうど以下なら「すべて表示」は出さない (hidden = 0)
        let few: Vec<PastSession> = (0..FOLDER_ROW_CAP)
            .map(|i| session(&format!("s{i}"), "codex", i as u64))
            .collect();
        st.apply_scan(vec![(f.clone(), few)]);
        assert_eq!(st.visible_sessions(&f), (st.sessions_for(&f), 0));

        // 知らないフォルダは空
        assert_eq!(
            st.visible_sessions(Path::new("/ws/unknown")),
            (&[][..], 0usize)
        );
    }

    // ── TTL / 再走査の状態機械 ───────────────────────────────────

    #[test]
    fn refresh_plan_never_touches_filesystem_on_ui_path() {
        // 実在しないパスばかりを渡す。UI 経路が fs を触るなら結果が空になるはず。
        let a = PathBuf::from("/definitely/not/here/a");
        let b = PathBuf::from("/definitely/not/here/b");
        let mut st = SidebarState {
            ttl: Duration::from_secs(3_600),
            ..Default::default()
        };

        // 1) 初回は走査したい
        assert_eq!(st.plan_refresh(std::slice::from_ref(&a)), RefreshPlan::Scan);

        // 2) 結果が入って時刻が進めば TTL 内は Idle
        st.started_at = Some(Instant::now());
        st.scanned = vec![a.clone()];
        st.apply_scan(vec![(a.clone(), vec![session("s1", "claude", 10)])]);
        assert_eq!(st.plan_refresh(std::slice::from_ref(&a)), RefreshPlan::Idle);
        // キャッシュはディスクを見ずに読める (パスは実在しない)
        assert_eq!(st.sessions_for(&a).len(), 1);
        assert_eq!(st.sessions_for(&a)[0].id, "s1");

        // 3) フォルダ集合が変われば TTL 内でも走査する
        assert_eq!(st.plan_refresh(&[a.clone(), b.clone()]), RefreshPlan::Scan);

        // 4) TTL 切れなら走査する。ここまで一度もディスクを見ていない。
        st.ttl = Duration::ZERO;
        assert_eq!(st.plan_refresh(std::slice::from_ref(&a)), RefreshPlan::Scan);
        // 走査を投げていないので、実在しないフォルダのキャッシュは残ったまま。
        assert_eq!(st.sessions_for(&a).len(), 1);
        assert!(st.sessions_for(&b).is_empty());
    }

    #[test]
    fn ttl_expiry_and_invalidate_trigger_rescan() {
        let a = PathBuf::from("/definitely/not/here/a");
        let mut st = SidebarState {
            scanned: vec![a.clone()],
            started_at: Some(Instant::now()),
            ..Default::default()
        };

        st.ttl = Duration::from_secs(3_600);
        assert_eq!(st.plan_refresh(std::slice::from_ref(&a)), RefreshPlan::Idle);
        // TTL 0 = 常に走査
        st.ttl = Duration::ZERO;
        assert_eq!(st.plan_refresh(std::slice::from_ref(&a)), RefreshPlan::Scan);
        // invalidate() でも走査に倒れる
        st.ttl = Duration::from_secs(3_600);
        st.invalidate();
        assert_eq!(st.plan_refresh(std::slice::from_ref(&a)), RefreshPlan::Scan);
        // 走査中は二重に投げない
        st.started_at = Some(Instant::now());
        let (_tx, rx) = mpsc::channel();
        st.pending = Some(rx);
        assert!(st.loading());
        assert_eq!(
            st.plan_refresh(&[PathBuf::from("/other")]),
            RefreshPlan::Idle
        );
    }

    #[test]
    fn background_scan_fills_cache_without_blocking_ui() {
        // ホームは一時ディレクトリへ差し替える (実ホームを走査しない)。
        // 「UI 経路 = refresh_if_stale を呼んでも即座に返る」ことを確かめる。
        let tmp = unique_temp_dir("zaivern-sidebar", "bg");
        let ws = tmp.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        // claude の保存先に 1 本だけ置いて、走査結果が届くことも見る。
        let cdir = claude_projects_root(&tmp).join(encode_claude_project_dir(&ws));
        std::fs::create_dir_all(&cdir).unwrap();
        std::fs::write(
            cdir.join("bg-1.jsonl"),
            "{\"type\":\"user\",\"timestamp\":\"2026-07-20T00:00:00.000Z\",\"message\":{\"content\":\"背景走査\"}}\n",
        )
        .unwrap();
        let mut st = SidebarState::with_home(tmp.clone());
        let folders = vec![ws.clone()];
        st.refresh_if_stale(&folders);
        assert!(st.loading(), "走査はバックグラウンドで走っているはず");
        // 走査中に何度呼んでもスレッドは 1 本のまま
        for _ in 0..5 {
            st.refresh_if_stale(&folders);
        }
        // 完了を待って取り込む
        for _ in 0..200 {
            st.refresh_if_stale(&folders);
            if !st.loading() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!st.loading(), "走査が終わらない");
        let got = st.sessions_for(&ws);
        assert_eq!(got.len(), 1, "バックグラウンド走査の結果が入っていない");
        assert_eq!(got[0].summary, "背景走査");
        // 取り込み後は TTL 内なので投げ直さない
        assert_eq!(st.plan_refresh(&folders), RefreshPlan::Idle);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sidebar_action_default_is_none() {
        assert_eq!(SidebarAction::default(), SidebarAction::None);
        let s = session("id", "agy", 1);
        assert_ne!(SidebarAction::Resume(s.clone()), SidebarAction::None);
        assert_eq!(
            SidebarAction::NewConversation(PathBuf::from("/a")),
            SidebarAction::NewConversation(PathBuf::from("/a"))
        );
        assert_ne!(
            SidebarAction::RevealFolder(PathBuf::from("/a")),
            SidebarAction::CloseFolder(PathBuf::from("/a"))
        );
    }
}
