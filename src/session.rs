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
}

/// `~/.zaivern/sessions/<workspaceハッシュhex>.toml` から読む。無ければ None。
pub fn load(workspace: &Path) -> Option<SessionData> {
    load_from(&sessions_dir(), workspace)
}

/// 同パスへ書く（ディレクトリは自動作成、失敗は無視）。
pub fn save(workspace: &Path, data: &SessionData) {
    save_to(&sessions_dir(), workspace, data);
}

/// 既定の保存先ディレクトリ: `~/.zaivern/sessions`
fn sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".zaivern")
        .join("sessions")
}

/// 内部: 既定ディレクトリ配下のセッションファイルパス。
fn session_file(workspace: &Path) -> PathBuf {
    session_file_in(&sessions_dir(), workspace)
}

/// 内部: 指定ディレクトリ配下のセッションファイルパス（テストで差し替え可能）。
fn session_file_in(dir: &Path, workspace: &Path) -> PathBuf {
    dir.join(format!("{}.toml", workspace_hash(workspace)))
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
fn load_from(dir: &Path, workspace: &Path) -> Option<SessionData> {
    let path = session_file_in(dir, workspace);
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

/// 内部: 指定ディレクトリへ書く（dirは自動作成、失敗は無視）。
fn save_to(dir: &Path, workspace: &Path, data: &SessionData) {
    let Ok(text) = toml::to_string_pretty(data) else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(session_file_in(dir, workspace), text);
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
            ..Default::default()
        };

        save_to(&dir, &workspace, &data);
        let loaded = load_from(&dir, &workspace).expect("session should load");

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
            ..Default::default()
        };

        save_to(&dir, &workspace, &data);
        let loaded = load_from(&dir, &workspace).expect("session should load");

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
                workspace.join("設計/仕様書.md").to_string_lossy().into_owned(),
            ],
            active: Some(0),
            sidebar_open: true,
            panel_open: true,
            ..Default::default()
        };

        save_to(&dir, &workspace, &data);
        let loaded = load_from(&dir, &workspace).expect("japanese session should load");

        assert_eq!(loaded.open_files, data.open_files);
        assert_eq!(loaded.active, Some(0));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_session_returns_none() {
        let dir = unique_temp_dir("zaivern-session-test", "missing");
        let workspace = dir.join("never-saved-workspace");

        assert!(load_from(&dir, &workspace).is_none());
        // 保存先ディレクトリ自体が無い場合も None
        let ghost_dir = dir.join("no-such-dir");
        assert!(load_from(&ghost_dir, &workspace).is_none());

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
        save_to(&dir, &workspace, &data);
        let loaded = load_from(&dir, &workspace).expect("session should load");
        assert_eq!(loaded.sidebar_tab, "git");

        // 旧バージョンのセッション (sidebar_tab フィールド無し) も読めること
        let old = "open_files = []\nsidebar_open = true\npanel_open = false\n";
        std::fs::write(session_file_in(&dir, &workspace), old).expect("write old session");
        let loaded = load_from(&dir, &workspace).expect("old session should still load");
        assert_eq!(loaded.sidebar_tab, "");
        assert!(loaded.sidebar_open);

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

        let file = session_file_in(Path::new("/x"), &base.join("a"));
        assert_eq!(file, PathBuf::from(format!("/x/{a1}.toml")));
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
            ..Default::default()
        };
        save_to(&dir, &workspace, &first);

        let second = SessionData {
            open_files: vec!["/new.rs".into(), "/new2.rs".into()],
            active: Some(1),
            sidebar_open: true,
            panel_open: true,
            ..Default::default()
        };
        save_to(&dir, &workspace, &second);

        let loaded = load_from(&dir, &workspace).expect("session should load");
        assert_eq!(loaded.open_files, vec!["/new.rs", "/new2.rs"]);
        assert_eq!(loaded.active, Some(1));

        std::fs::remove_dir_all(&dir).ok();
    }
}
