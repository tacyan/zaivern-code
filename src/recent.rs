//! 「最近使用した項目」とメニューバー付随の軽量永続化。
//!
//! config.toml (手書き・コメント保護) や state.toml (UI 選択) とは独立に、
//! `~/.zaivern/menu_state.toml` へ保存する。既存ファイルのフォーマットを
//! 巻き込まないため、壊れていても黙って既定値に戻る。

use crate::config::zaivern_dir;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 「最近使用した項目」に覚えておく件数 (フォルダ / ファイルそれぞれ)。
/// ⌘P の「最近開いた順」の加点もこの件数を前提に設計してある (app.rs)。
pub const MAX_RECENT: usize = 12;

#[derive(Default, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct MenuState {
    pub recent_folders: Vec<String>,
    pub recent_files: Vec<String>,
    /// ファイルの自動保存 (VS Code の afterDelay 相当)
    pub auto_save: bool,
}

impl MenuState {
    /// フォルダを先頭に記録 (重複は先頭へ移動、上限あり)。
    pub fn touch_folder(&mut self, p: &Path) {
        touch(&mut self.recent_folders, p);
    }

    /// ファイルを先頭に記録 (重複は先頭へ移動、上限あり)。
    pub fn touch_file(&mut self, p: &Path) {
        touch(&mut self.recent_files, p);
    }

    pub fn clear_recent(&mut self) {
        self.recent_folders.clear();
        self.recent_files.clear();
    }

    /// 実在するフォルダだけを PathBuf で返す。
    pub fn folders(&self) -> Vec<PathBuf> {
        self.recent_folders
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .collect()
    }

    /// 実在するファイルだけを PathBuf で返す。
    pub fn files(&self) -> Vec<PathBuf> {
        self.recent_files
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .collect()
    }
}

/// 起動時のフォルダ復元を無効化する環境変数。
/// `1` / `true` / `yes` など「空でも 0/false/no/off でもない値」で無効化。
pub const NO_RESTORE_ENV: &str = "ZAIVERN_NO_RESTORE";

/// 同じことをするコマンドラインフラグ。
/// cli.rs のサブコマンド表には載せない (載せると GUI 起動ではなく CLI 実行になる)。
pub const NO_RESTORE_FLAG: &str = "--no-restore";

/// 前回フォルダの復元が無効化されているか。
///
/// `env_value` は `std::env::var(NO_RESTORE_ENV).ok()` を渡す想定
/// (テストからプロセス環境を汚さずに検証できるよう引数で受け取る)。
pub fn restore_disabled(args: &[String], env_value: Option<&str>) -> bool {
    if args.iter().any(|a| a == NO_RESTORE_FLAG) {
        return true;
    }
    match env_value {
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && !matches!(v.as_str(), "0" | "false" | "no" | "off")
        }
        None => false,
    }
}

/// 起動時に開き直すフォルダを決める (純粋関数)。
///
/// 【ルール】
/// 1. 引数でディレクトリが 1 つでも指定されていれば復元しない — 明示指定が常に勝つ。
/// 2. `--no-restore` / `ZAIVERN_NO_RESTORE` で無効化されていれば復元しない。
/// 3. それ以外 (= 引数無しの素の起動) は MRU の先頭から順に見て、
///    **今も実在する最初のフォルダ** を返す。
///    「カレントディレクトリが意味あるワークスペースかどうか」は判定材料にしない —
///    Finder/Dock からの起動ではカレントが `/` などになるうえ、ターミナルから
///    引数無しで叩いた場合も「前に開いていた続き」を期待する方が自然なため。
/// 4. MRU が空/壊れている/全部消えている場合は `None`。
///    呼び出し側は従来どおりカレントディレクトリへフォールバックする。
pub fn startup_folder(arg_dirs: &[PathBuf], st: &MenuState, disabled: bool) -> Option<PathBuf> {
    if !arg_dirs.is_empty() || disabled {
        return None;
    }
    st.folders().into_iter().next()
}

/// main.rs から呼ぶ実環境版 (`~/.zaivern/menu_state.toml` + プロセス環境)。
/// ファイルが無い/壊れていても `MenuState::default()` に落ちるので失敗しない。
pub fn startup_folder_for_launch(arg_dirs: &[PathBuf], args: &[String]) -> Option<PathBuf> {
    let disabled = restore_disabled(args, std::env::var(NO_RESTORE_ENV).ok().as_deref());
    startup_folder(arg_dirs, &load(), disabled)
}

fn touch(list: &mut Vec<String>, p: &Path) {
    let s = p.display().to_string();
    list.retain(|x| x != &s);
    list.insert(0, s);
    list.truncate(MAX_RECENT);
}

fn state_file(dir: &Path) -> PathBuf {
    dir.join("menu_state.toml")
}

pub fn load() -> MenuState {
    load_from(&zaivern_dir())
}

pub fn save(st: &MenuState) {
    save_to(&zaivern_dir(), st);
}

fn load_from(dir: &Path) -> MenuState {
    std::fs::read_to_string(state_file(dir))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_to(dir: &Path, st: &MenuState) {
    if let Ok(s) = toml::to_string_pretty(st) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(state_file(dir), s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zv-recent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn touch_moves_duplicates_to_front_and_caps() {
        let mut st = MenuState::default();
        for i in 0..20 {
            st.touch_folder(Path::new(&format!("/f{i}")));
        }
        assert_eq!(st.recent_folders.len(), MAX_RECENT);
        assert_eq!(st.recent_folders[0], "/f19");
        // 既存項目を触ると先頭へ移動するだけで数は増えない
        st.touch_folder(Path::new("/f10"));
        assert_eq!(st.recent_folders[0], "/f10");
        assert_eq!(st.recent_folders.len(), MAX_RECENT);
    }

    #[test]
    fn roundtrip_persists_and_broken_file_falls_back() {
        let dir = tmp("rt");
        let mut st = MenuState::default();
        st.touch_file(Path::new("/tmp/a.txt"));
        st.auto_save = true;
        save_to(&dir, &st);
        let got = load_from(&dir);
        assert_eq!(got.recent_files, vec!["/tmp/a.txt".to_string()]);
        assert!(got.auto_save);

        // 壊れた TOML は黙って既定値
        std::fs::write(state_file(&dir), "not { valid").unwrap();
        let broken = load_from(&dir);
        assert!(broken.recent_files.is_empty() && !broken.auto_save);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folders_files_filter_nonexistent() {
        let dir = tmp("filter");
        let real = dir.join("real.txt");
        std::fs::write(&real, "x").unwrap();
        let mut st = MenuState::default();
        st.touch_file(&real);
        st.touch_file(Path::new("/nonexistent/file.txt"));
        st.touch_folder(&dir);
        st.touch_folder(Path::new("/nonexistent-dir"));
        assert_eq!(st.files(), vec![real.clone()]);
        assert_eq!(st.folders(), vec![dir.clone()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 起動時のフォルダ復元 ─────────────────────────────────────

    #[test]
    fn restore_disabled_honors_flag_and_env() {
        let none: Vec<String> = Vec::new();
        let flag = vec!["--no-restore".to_string()];
        let table: &[(&[String], Option<&str>, bool)] = &[
            (&[], None, false),
            (&[], Some(""), false),
            (&[], Some("0"), false),
            (&[], Some("false"), false),
            (&[], Some(" No "), false),
            (&[], Some("off"), false),
            (&[], Some("1"), true),
            (&[], Some("true"), true),
            (&[], Some("yes"), true),
        ];
        for (args, env, want) in table {
            assert_eq!(restore_disabled(args, *env), *want, "env={env:?}");
        }
        assert!(restore_disabled(&flag, None));
        // フラグは環境変数より強い (無効化側に倒す)
        assert!(restore_disabled(&flag, Some("0")));
        assert!(!restore_disabled(&none, None));
    }

    #[test]
    fn startup_folder_decision_table() {
        let dir = tmp("startup");
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let gone = dir.join("gone");

        // MRU 先頭が実在 → それを開く
        let mut st = MenuState::default();
        st.touch_folder(&b);
        st.touch_folder(&a); // 先頭は a
        assert_eq!(startup_folder(&[], &st, false), Some(a.clone()));

        // 引数でディレクトリ指定あり → 復元しない (引数が勝つ)
        assert_eq!(startup_folder(&[b.clone()], &st, false), None);

        // 無効化されていれば復元しない
        assert_eq!(startup_folder(&[], &st, true), None);

        // MRU 先頭が消えている → 次に実在するものへ
        let mut st2 = MenuState::default();
        st2.touch_folder(&b);
        st2.touch_folder(&gone); // 先頭は存在しない
        assert_eq!(startup_folder(&[], &st2, false), Some(b.clone()));

        // MRU が空 → None (呼び出し側がカレントへフォールバック)
        assert_eq!(startup_folder(&[], &MenuState::default(), false), None);

        // 全部消えている → None
        let mut st3 = MenuState::default();
        st3.touch_folder(&gone);
        assert_eq!(startup_folder(&[], &st3, false), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_folder_survives_missing_and_broken_state_file() {
        let dir = tmp("startup-broken");
        // ファイルが無い
        assert!(startup_folder(&[], &load_from(&dir), false).is_none());
        // 壊れている
        std::fs::write(state_file(&dir), "!!! not toml").unwrap();
        assert!(startup_folder(&[], &load_from(&dir), false).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
