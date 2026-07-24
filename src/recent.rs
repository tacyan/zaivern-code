//! 「最近使用した項目」とメニューバー付随の軽量永続化。
//!
//! config.toml (手書き・コメント保護) や state.toml (UI 選択) とは独立に、
//! `~/.zaivern/menu_state.toml` へ保存する。既存ファイルのフォーマットを
//! 巻き込まないため、壊れていても黙って既定値に戻る。

use crate::config::zaivern_dir;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_RECENT: usize = 12;

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
}
