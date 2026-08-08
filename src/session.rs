//! ワークスペースセッション永続化
//!
//! アプリ再起動時に「開いていたタブ」「アクティブタブ」「サイドバー/パネルの開閉」を
//! 復元するための保存層。ワークスペース絶対パスごとに
//! `~/.zaivern/sessions/<ハッシュhex>.toml` へ保存する。
#![allow(dead_code)]

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// 1ワークスペース分のセッション情報。
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SessionData {
    /// 開いていたファイルの絶対パス（存在しないパスもそのまま保存してよい）
    pub open_files: Vec<String>,
    /// アクティブタブの index
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<usize>,
    pub sidebar_open: bool,
    pub panel_open: bool,
    /// サイドバーのタブ ("files"|"agents"|"plugins"|"git")。
    /// 旧バージョンのセッションファイルには無いので空文字なら既定タブ扱い。
    pub sidebar_tab: String,
    /// ワークスペースのルート一覧(絶対パス)。再起動時に全フォルダを復元する。
    /// 旧形式(単一ルート)のファイルでは空 — その場合は起動時のルートを使う。
    pub roots: Vec<String>,
    /// エディタの分割レイアウト ([`crate::editor_split::EditorPanesRec::to_line`])。
    ///
    /// 端末分割 ([`AgentSessionRec::split`]) と**同じ流儀**: プレーンな文字列 1 本。
    /// TOML はテーブル / 配列を単純値より後ろにしか置けないので、ここへ構造体や
    /// `Vec` を足してはいけない。空 = 分割なし (1 ペインで開く)。
    /// リーフはバッファ ID ではなく**ファイルの絶対パス**で指す (再起動で ID は変わる)。
    pub editor_split: String,
    /// **ピン留めされていたタブ**のファイル絶対パス (ピン順)。
    ///
    /// ペイン ID ではなくパスで持つ理由は [`Self::editor_split`] と同じ
    /// (バッファ ID は再起動で必ず変わる)。復元時は、そのファイルを開いている
    /// 全ペインでピン留めし直す。旧形式のファイルには無いので空 = ピン留めなし。
    /// TOML の制約でテーブル配列 (`agents`) より**前**に置くこと。
    pub pinned_files: Vec<String>,
    /// 走らせていたエージェントタブの記録 (チャット履歴のフォルダ別保存)。
    /// フォルダを開き直したときに、タブ + 前回スクロールバックを復元し、
    /// 対応 CLI (claude / codex) は会話を再開する。旧ファイルには無いので空。
    /// TOML の制約 (テーブル配列は単純値の後) があるため必ず最後のフィールドに置く。
    pub agents: Vec<AgentSessionRec>,
}

/// 復元用に保存するエージェントセッション 1 本分の記録。
///
/// env は保存しない — プリセットの env にはシークレットが入り得るので、
/// 復元時に `preset_name` で現在の設定から引き直す (config.rs 側が真実源)。
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AgentSessionRec {
    /// 起動元プリセット名 (復元時に env を再構成するための照合キー)。
    pub preset_name: String,
    /// タブのタイトル ("Claude Code #2" など)。
    pub title: String,
    pub icon: String,
    /// 実際に起動したコマンド (承認モード適用済み)。復元時はこれに再開指定を足す。
    pub command: String,
    /// 起動ディレクトリ (絶対パス)。消えていたら現在の作業フォルダで代替する。
    pub cwd: String,
    /// 生ログの書き出し先 (絶対パス)。復元後も同じファイルへ追記する。
    pub log_file: String,
    /// このタイルの端末分割レイアウト ([`crate::terminal::SplitLayoutRec::to_line`])。
    ///
    /// **必ずプレーンな文字列 1 本**にしておく: TOML はテーブル / 配列を
    /// 単純値より後ろにしか置けないため、ここへ構造体や `Vec` を足すと
    /// 既存フィールドの並び順に依存した壊れ方をする。空 = 分割なし。
    /// リーフはセッション ID ではなく**生ログのパス**で指す (再起動で ID は変わる)。
    pub split: String,
}

/// `~/.zaivern/sessions/<ルート集合ハッシュhex>.toml` から読む。無ければ None。
pub fn load(roots: &[PathBuf]) -> Option<SessionData> {
    load_from(&sessions_dir(), roots)
}

/// 同パスへ書く（ディレクトリは自動作成、失敗は無視）。
pub fn save(roots: &[PathBuf], data: &SessionData) {
    save_to(&sessions_dir(), roots, data);
}

/// 既定の保存先ディレクトリ: `~/.zaivern/sessions`
/// (`zai session list` からも走査するので pub)
pub fn sessions_dir() -> PathBuf {
    crate::config::zaivern_dir().join("sessions")
}

/// 内部: 既定ディレクトリ配下のセッションファイルパス。
fn session_file(roots: &[PathBuf]) -> PathBuf {
    session_file_in(&sessions_dir(), roots)
}

/// 内部: 指定ディレクトリ配下のセッションファイルパス（テストで差し替え可能）。
fn session_file_in(dir: &Path, roots: &[PathBuf]) -> PathBuf {
    dir.join(format!("{}.toml", roots_hash(roots)))
}

/// 内部: ルート「集合」→ 安定ハッシュhex文字列。
///
/// 順序非依存にするため、canonicalize → 文字列化 → ソート → 重複除去 してから
/// まとめてハッシュする。つまり `[A, B]` と `[B, A]` は同じセッションを指す。
///
/// 注意: `DefaultHasher` は Rust バージョン間での安定性が保証されていない。
/// 値が変わった場合はセッションファイルが見つからなくなるだけで、
/// クラッシュもデータ破壊も起きない（次回保存で新しいキーに書かれる）。
fn roots_hash(roots: &[PathBuf]) -> String {
    let mut keys: Vec<String> = roots
        .iter()
        .map(|p| {
            p.canonicalize()
                .unwrap_or_else(|_| p.to_path_buf())
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    keys.sort();
    keys.dedup();
    let mut hasher = DefaultHasher::new();
    keys.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// ターミナル生ログの置き場: `~/.zaivern/term_logs/<ワークスペースハッシュ>/`。
///
/// ワークスペース単位で分けるので、同じプロジェクトを開き直せば前回のログが
/// そのまま並ぶ (スクロールバック永続化)。
pub fn term_log_dir(workspace: &Path) -> PathBuf {
    crate::config::zaivern_dir()
        .join("term_logs")
        .join(workspace_hash(workspace))
}

/// セッション 1 本分のログファイルパス。ファイル名にタイトルを含めて
/// 一覧で見分けられるようにする (パスに使えない文字は `_` へ)。
pub fn term_log_path(workspace: &Path, session_id: u64, title: &str) -> PathBuf {
    let safe: String = title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    term_log_dir(workspace).join(format!("{safe}-{session_id}.log"))
}

/// セッション復元時にスクロールバックへ流し込む前回ログの上限バイト数。
/// vt100 のスクロールバックは 5000 行なので、これで十分に埋まる
/// (ログ全量 (最大 8MB) を流すと起動が重くなるだけで見える行は増えない)。
pub const REPLAY_TAIL_CAP: usize = 1024 * 1024;

/// 前回ログの末尾を読み出す (復元時のスクロールバック再生用)。
///
/// `open_term_log` と同じく `.log.old` (ローテート退避分) → `.log` の順で繋ぎ、
/// 後ろ `cap` バイトへ切り詰める。切り口がエスケープ列や UTF-8 の途中に
/// かからないよう、次の改行まで進めてから切る。ログが無ければ空。
pub fn read_term_log_tail(path: &Path, cap: usize) -> Vec<u8> {
    let mut raw = std::fs::read(path.with_extension("log.old")).unwrap_or_default();
    if let Ok(cur) = std::fs::read(path) {
        raw.extend_from_slice(&cur);
    }
    if raw.len() > cap {
        let from = raw.len() - cap;
        let cut = raw[from..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| from + i + 1)
            .unwrap_or(from);
        raw.drain(..cut);
    }
    raw
}

/// 古いターミナルログの掃除。新しい方から `keep` 本を残して削除する
/// (`.old` ローテート分は本体と対で消す)。起動時に一度呼ぶ想定。
pub fn prune_term_logs(workspace: &Path, keep: usize) {
    let mut logs = list_term_logs(workspace);
    for p in logs.split_off(keep.min(logs.len())) {
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("log.old"));
    }
}

/// ワークスペースの既存ログ一覧 (新しい順)。「📜 前回ログ」メニューの素材。
pub fn list_term_logs(workspace: &Path) -> Vec<PathBuf> {
    let dir = term_log_dir(workspace);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "log").unwrap_or(false))
        .filter_map(|e| {
            let t = e.metadata().ok()?.modified().ok()?;
            Some((t, e.path()))
        })
        .collect();
    logs.sort_by_key(|b| std::cmp::Reverse(b.0));
    logs.into_iter().map(|(_, p)| p).collect()
}

/// 内部: ワークスペース絶対パス → 安定ハッシュhex文字列（DefaultHasher）。
/// canonicalize できる場合は正規化してシンボリックリンク差を吸収する。
fn workspace_hash(workspace: &Path) -> String {
    let canonical = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// 内部: 指定ディレクトリから読む（テスト用に保存先を差し替え可能）。
///
/// マルチルートキーで見つからず、かつルートが 1 件だけの場合は、
/// 旧形式（単一ワークスペースパスのハッシュ）のファイルへフォールバックする。
/// これで v0.1.3 以前のセッションもそのまま復元できる。
fn load_from(dir: &Path, roots: &[PathBuf]) -> Option<SessionData> {
    let read = |p: PathBuf| -> Option<SessionData> {
        toml::from_str(&std::fs::read_to_string(p).ok()?).ok()
    };
    if let Some(d) = read(session_file_in(dir, roots)) {
        return Some(d);
    }
    if roots.len() == 1 {
        return read(dir.join(format!("{}.toml", workspace_hash(&roots[0]))));
    }
    None
}

/// 内部: 指定ディレクトリへ書く（dirは自動作成、失敗は無視）。
fn save_to(dir: &Path, roots: &[PathBuf], data: &SessionData) {
    let Ok(text) = toml::to_string_pretty(data) else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(session_file_in(dir, roots), text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    #[test]
    fn roundtrip_save_then_load() {
        let dir = unique_temp_dir("zaivern-session-test", "roundtrip");
        let workspace = dir.join("my-workspace");
        let data = SessionData {
            open_files: vec![
                "/Users/alice/project/src/main.rs".into(),
                "/Users/alice/project/README.md".into(),
                "/does/not/exist.rs".into(), // 存在しないパスもそのまま保存される
            ],
            active: Some(1),
            sidebar_open: true,
            panel_open: false,
            roots: Vec::new(),
            ..Default::default()
        };

        save_to(&dir, std::slice::from_ref(&workspace), &data);
        let loaded =
            load_from(&dir, std::slice::from_ref(&workspace)).expect("session should load");

        assert_eq!(loaded.open_files, data.open_files);
        assert_eq!(loaded.active, Some(1));
        assert!(loaded.sidebar_open);
        assert!(!loaded.panel_open);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn roundtrip_with_active_none() {
        let dir = unique_temp_dir("zaivern-session-test", "none-active");
        let workspace = dir.join("ws-no-active");
        let data = SessionData {
            open_files: vec![],
            active: None,
            sidebar_open: false,
            panel_open: true,
            roots: Vec::new(),
            ..Default::default()
        };

        save_to(&dir, std::slice::from_ref(&workspace), &data);
        let loaded =
            load_from(&dir, std::slice::from_ref(&workspace)).expect("session should load");

        assert!(loaded.open_files.is_empty());
        assert_eq!(loaded.active, None);
        assert!(!loaded.sidebar_open);
        assert!(loaded.panel_open);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn roundtrip_with_japanese_paths() {
        let dir = unique_temp_dir("zaivern-session-test", "japanese");
        // ワークスペース自体も日本語パス（実在させて canonicalize 経路も通す）
        let workspace = dir.join("日本語ワークスペース").join("プロジェクト");
        std::fs::create_dir_all(&workspace).expect("create japanese workspace dir");
        let data = SessionData {
            open_files: vec![
                workspace.join("メモ帳.txt").to_string_lossy().into_owned(),
                workspace
                    .join("設計/仕様書.md")
                    .to_string_lossy()
                    .into_owned(),
            ],
            active: Some(0),
            sidebar_open: true,
            panel_open: true,
            roots: Vec::new(),
            ..Default::default()
        };

        save_to(&dir, std::slice::from_ref(&workspace), &data);
        let loaded = load_from(&dir, std::slice::from_ref(&workspace))
            .expect("japanese session should load");

        assert_eq!(loaded.open_files, data.open_files);
        assert_eq!(loaded.active, Some(0));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_session_returns_none() {
        let dir = unique_temp_dir("zaivern-session-test", "missing");
        let workspace = dir.join("never-saved-workspace");

        assert!(load_from(&dir, std::slice::from_ref(&workspace)).is_none());
        // 保存先ディレクトリ自体が無い場合も None
        let ghost_dir = dir.join("no-such-dir");
        assert!(load_from(&ghost_dir, std::slice::from_ref(&workspace)).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sidebar_tab_roundtrips_and_old_file_without_it_still_loads() {
        let dir = unique_temp_dir("zaivern-session-test", "sidebar-tab");
        let workspace = dir.join("ws-tab");

        let data = SessionData {
            sidebar_tab: "git".into(),
            ..Default::default()
        };
        let roots = std::slice::from_ref(&workspace);
        save_to(&dir, roots, &data);
        let loaded = load_from(&dir, roots).expect("session should load");
        assert_eq!(loaded.sidebar_tab, "git");

        // 旧バージョンのセッション (sidebar_tab フィールド無し) も読めること
        let old = "open_files = []\nsidebar_open = true\npanel_open = false\n";
        std::fs::write(session_file_in(&dir, roots), old).expect("write old session");
        let loaded = load_from(&dir, roots).expect("old session should still load");
        assert_eq!(loaded.sidebar_tab, "");
        assert!(loaded.sidebar_open);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn agents_roundtrip_and_old_file_without_agents_still_loads() {
        let dir = unique_temp_dir("zaivern-session-test", "agents");
        let workspace = dir.join("ws-agents");
        let roots = std::slice::from_ref(&workspace);

        let data = SessionData {
            open_files: vec!["/p/main.rs".into()],
            agents: vec![
                AgentSessionRec {
                    preset_name: "Claude Code".into(),
                    title: "Claude Code".into(),
                    icon: "👾".into(),
                    command: "claude --dangerously-skip-permissions".into(),
                    cwd: "/p".into(),
                    log_file: "/logs/Claude_Code-1.log".into(),
                    // 分割レイアウト (リーフ = 生ログのパス)。
                    split: String::new(),
                },
                AgentSessionRec {
                    preset_name: "Codex".into(),
                    title: "Codex #2".into(),
                    icon: "💡".into(),
                    command: "codex".into(),
                    cwd: "/p/サブ".into(),
                    log_file: "/logs/Codex__2-2.log".into(),
                    split: String::new(),
                },
            ],
            ..Default::default()
        };
        save_to(&dir, roots, &data);
        let loaded = load_from(&dir, roots).expect("session should load");
        assert_eq!(loaded.agents.len(), 2);
        assert_eq!(loaded.agents[0].preset_name, "Claude Code");
        assert_eq!(
            loaded.agents[0].command,
            "claude --dangerously-skip-permissions"
        );
        assert_eq!(loaded.agents[1].title, "Codex #2");
        assert_eq!(loaded.agents[1].cwd, "/p/サブ");
        assert_eq!(loaded.agents[1].log_file, "/logs/Codex__2-2.log");
        // open_files などの既存フィールドと共存できる (テーブル配列は最後)
        assert_eq!(loaded.open_files, vec!["/p/main.rs"]);

        // 旧バージョンのセッション ([[agents]] 無し) も読めること
        let old = "open_files = [\"/a.rs\"]\nsidebar_open = true\npanel_open = false\n";
        std::fs::write(session_file_in(&dir, roots), old).expect("write old session");
        let loaded = load_from(&dir, roots).expect("old session should still load");
        assert!(loaded.agents.is_empty(), "旧ファイルでは空の agents になる");
        assert_eq!(loaded.open_files, vec!["/a.rs"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// エディタ分割の 1 行が `[[agents]]` (テーブル配列) と共存でき、
    /// この欄を持たない旧セッションも読めること。
    #[test]
    fn editor_split_line_roundtrips_and_old_file_without_it_still_loads() {
        let dir = unique_temp_dir("zaivern-session-test", "editor-split");
        let roots = &[dir.join("ws")];
        let line = format!(
            "1{gs}0{fs}p0{fs}H:0.5{rs}L:p0{rs}L:p1{gs}p0{fs}0{fs}/p/a.rs{gs}p1{fs}0{fs}/p/b.rs",
            gs = '\u{1d}',
            fs = '\u{1f}',
            rs = '\u{1e}',
        );
        let data = SessionData {
            open_files: vec!["/p/a.rs".into(), "/p/b.rs".into()],
            editor_split: line.clone(),
            agents: vec![AgentSessionRec {
                preset_name: "Claude Code".into(),
                title: "Claude Code #1".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        save_to(&dir, roots, &data);
        let loaded = load_from(&dir, roots).expect("session should load");
        assert_eq!(loaded.editor_split, line, "分割の 1 行がそのまま戻る");
        assert_eq!(loaded.agents.len(), 1, "テーブル配列と共存できる");

        // この欄を持たない旧ファイルは空文字 (= 分割なし) になる
        let old = "open_files = [\"/a.rs\"]\nsidebar_open = true\npanel_open = false\n";
        std::fs::write(session_file_in(&dir, roots), old).expect("write old session");
        let loaded = load_from(&dir, roots).expect("old session should still load");
        assert_eq!(loaded.editor_split, "");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// ピン留めは**再起動をまたいで残る**。旧セッションファイル (この欄が無い)
    /// でも空 = ピン留めなしとして読める。
    #[test]
    fn ピン留めは往復し古いセッションでも読める() {
        let dir = unique_temp_dir("zaivern-session-test", "pinned");
        let roots = &[dir.join("ws")];
        // パスはハードコードせず一時ディレクトリから組む (どの OS でも通る)
        let a = dir.join("a.rs").to_string_lossy().into_owned();
        let b = dir.join("b.rs").to_string_lossy().into_owned();
        let data = SessionData {
            open_files: vec![a.clone(), b.clone()],
            pinned_files: vec![b.clone()],
            agents: vec![AgentSessionRec {
                preset_name: "Claude Code".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        save_to(&dir, roots, &data);
        let loaded = load_from(&dir, roots).expect("session should load");
        assert_eq!(loaded.pinned_files, vec![b], "ピン留めがそのまま戻る");
        assert_eq!(loaded.open_files.len(), 2);
        assert_eq!(loaded.agents.len(), 1, "テーブル配列と共存できる");

        // この欄を持たない旧ファイルは空 (= ピン留めなし)
        let old = "open_files = [\"/a.rs\"]\nsidebar_open = true\npanel_open = false\n";
        std::fs::write(session_file_in(&dir, roots), old).expect("write old session");
        let loaded = load_from(&dir, roots).expect("old session should still load");
        assert!(loaded.pinned_files.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_term_log_tail_concats_old_then_current() {
        let dir = unique_temp_dir("zaivern-termlog-test", "tail-concat");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("a-1.log");
        std::fs::write(log.with_extension("log.old"), b"old-part\n").unwrap();
        std::fs::write(&log, b"new-part\n").unwrap();
        // .old が先、本体が後 (時系列順)
        assert_eq!(read_term_log_tail(&log, 1024), b"old-part\nnew-part\n");
        // ログが無ければ空 (復元側はこれで resume を諦める)
        assert!(read_term_log_tail(&dir.join("ghost.log"), 1024).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_term_log_tail_caps_at_line_boundary() {
        let dir = unique_temp_dir("zaivern-termlog-test", "tail-cap");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("b-1.log");
        std::fs::write(&log, b"line-one\nline-two\nline-three\n").unwrap();
        // cap 位置が行の途中なら、次の改行まで捨ててから返す
        let tail = read_term_log_tail(&log, 14); // "-two\nline-three\n" の途中
        assert_eq!(tail, b"line-three\n");
        // cap がファイルより大きければ全量そのまま
        let all = read_term_log_tail(&log, 4096);
        assert_eq!(all, b"line-one\nline-two\nline-three\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hash_is_stable_and_distinguishes_workspaces() {
        let base = PathBuf::from("/tmp/zaivern-hash-check");
        let a1 = workspace_hash(&base.join("a"));
        let a2 = workspace_hash(&base.join("a"));
        let b = workspace_hash(&base.join("b"));

        assert_eq!(a1, a2, "same workspace must map to the same file");
        assert_ne!(a1, b, "different workspaces should map to different files");
        assert!(a1.chars().all(|c| c.is_ascii_hexdigit()));

        let roots = [base.join("a")];
        let h = roots_hash(&roots);
        let file = session_file_in(Path::new("/x"), &roots);
        assert_eq!(file, PathBuf::from(format!("/x/{h}.toml")));
    }

    #[test]
    fn roots_hash_is_order_independent() {
        let a = PathBuf::from("/tmp/zaivern-roots/a");
        let b = PathBuf::from("/tmp/zaivern-roots/b");
        let c = PathBuf::from("/tmp/zaivern-roots/c");

        let ab = roots_hash(&[a.clone(), b.clone()]);
        let ba = roots_hash(&[b.clone(), a.clone()]);
        assert_eq!(ab, ba, "ルート集合が同じなら順序が違っても同じセッション");

        // 重複は畳まれる
        assert_eq!(roots_hash(&[a.clone(), b.clone(), a.clone()]), ab);
        // 集合が違えば別キー
        assert_ne!(roots_hash(&[a.clone(), b, c]), ab);
        assert_ne!(roots_hash(std::slice::from_ref(&a)), ab);
        assert!(ab.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn multi_root_session_roundtrip_ignores_order() {
        let dir = unique_temp_dir("zaivern-session-test", "multi");
        let a = dir.join("proj-a");
        let b = dir.join("proj-b");
        std::fs::create_dir_all(&a).expect("mkdir a");
        std::fs::create_dir_all(&b).expect("mkdir b");

        let data = SessionData {
            open_files: vec![
                a.join("src/main.rs").to_string_lossy().into_owned(),
                b.join("index.js").to_string_lossy().into_owned(),
            ],
            active: Some(1),
            sidebar_open: true,
            panel_open: true,
            roots: vec![
                a.to_string_lossy().into_owned(),
                b.to_string_lossy().into_owned(),
            ],
            ..Default::default()
        };

        save_to(&dir, &[a.clone(), b.clone()], &data);
        // 順序を入れ替えても同じセッションが読める
        let loaded = load_from(&dir, &[b.clone(), a.clone()]).expect("session should load");
        assert_eq!(loaded.open_files, data.open_files);
        assert_eq!(loaded.roots.len(), 2, "ルート一覧そのものも永続化される");

        // 片方だけのワークスペースは別セッション扱い
        assert!(load_from(&dir, std::slice::from_ref(&a)).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_single_root_session_still_loads() {
        let dir = unique_temp_dir("zaivern-session-test", "legacy");
        let workspace = dir.join("old-ws");
        std::fs::create_dir_all(&workspace).expect("mkdir ws");

        // v0.1.3 以前の形式で書かれたファイルを手で置く
        let legacy = SessionData {
            open_files: vec!["/old/a.rs".into()],
            active: Some(0),
            sidebar_open: true,
            panel_open: false,
            roots: Vec::new(),
            ..Default::default()
        };
        std::fs::create_dir_all(&dir).ok();
        std::fs::write(
            dir.join(format!("{}.toml", workspace_hash(&workspace))),
            toml::to_string_pretty(&legacy).expect("serialize"),
        )
        .expect("write legacy session");

        let loaded =
            load_from(&dir, std::slice::from_ref(&workspace)).expect("legacy session should load");
        assert_eq!(loaded.open_files, vec!["/old/a.rs"]);
        assert!(loaded.roots.is_empty(), "旧形式に roots は無い");

        // 複数ルートになると旧形式へはフォールバックしない（別ワークスペース扱い）
        assert!(load_from(&dir, &[workspace, dir.join("other")]).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_overwrites_existing_session() {
        let dir = unique_temp_dir("zaivern-session-test", "overwrite");
        let workspace = dir.join("ws");

        let first = SessionData {
            open_files: vec!["/old.rs".into()],
            active: Some(0),
            sidebar_open: false,
            panel_open: false,
            roots: Vec::new(),
            ..Default::default()
        };
        save_to(&dir, std::slice::from_ref(&workspace), &first);

        let second = SessionData {
            open_files: vec!["/new.rs".into(), "/new2.rs".into()],
            active: Some(1),
            sidebar_open: true,
            panel_open: true,
            roots: Vec::new(),
            ..Default::default()
        };
        save_to(&dir, std::slice::from_ref(&workspace), &second);

        let loaded =
            load_from(&dir, std::slice::from_ref(&workspace)).expect("session should load");
        assert_eq!(loaded.open_files, vec!["/new.rs", "/new2.rs"]);
        assert_eq!(loaded.active, Some(1));

        std::fs::remove_dir_all(&dir).ok();
    }

    // ── ターミナルログのパスと掃除 ──────────────────────────────────

    #[test]
    fn term_log_path_sanitizes_title() {
        let p = term_log_path(Path::new("/tmp"), 7, "Claude Code #2 (全自動)/危険");
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        // パス区切りや空白は _ に置き換わり、id で一意になる
        assert!(name.ends_with("-7.log"), "{name}");
        assert!(!name.contains('/') && !name.contains(' '), "{name}");
    }

    #[test]
    fn prune_term_logs_keeps_newest() {
        use std::time::Duration;
        let ws = crate::test_util::unique_temp_dir("zaivern-termlog-test", "prune");
        std::fs::create_dir_all(&ws).unwrap();
        let dir = term_log_dir(&ws);
        std::fs::create_dir_all(&dir).unwrap();
        // mtime 差を付けて 4 本作る
        for i in 0..4u64 {
            let p = dir.join(format!("t-{i}.log"));
            std::fs::write(&p, format!("log {i}")).unwrap();
            let t = std::time::SystemTime::now() - Duration::from_secs((4 - i) * 10);
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(t).unwrap();
        }
        prune_term_logs(&ws, 2);
        let left = list_term_logs(&ws);
        assert_eq!(left.len(), 2, "新しい 2 本だけ残る");
        let names: Vec<String> = left
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"t-3.log".to_string()), "{names:?}");
        assert!(names.contains(&"t-2.log".to_string()), "{names:?}");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&ws).ok();
    }
}
