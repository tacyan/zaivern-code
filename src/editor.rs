use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use eframe::egui::text::LayoutJob;

use crate::highlight::Highlighter;

pub fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// ディスク上の最終更新時刻(外部変更検知用)。
pub fn disk_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// 外部(エージェント・他ツール)によるファイル変更の検知結果。
pub enum ExternalEvent {
    /// 未保存の編集が無かったのでディスクの内容へ読み直した
    Reloaded { index: usize, title: String },
    /// 未保存の編集があるため読み直さなかった(上書き注意)
    Conflict { title: String },
}

pub struct Buffer {
    pub id: u64,
    pub path: Option<PathBuf>,
    pub title: String,
    pub text: String,
    pub saved_hash: u64,
    pub lang: String,
    /// (cache key, layout job) — recomputed only when text/theme/font change.
    pub cache: Option<(u64, LayoutJob)>,
    /// (cache key, gutter layout job) — 行番号 + git 差分マーク色。
    pub gutter: Option<(u64, LayoutJob)>,
    /// 読み込み/保存時点のディスク上の mtime。外部変更はこれとの差分で検知する。
    pub disk_mtime: Option<SystemTime>,
    /// 警告済みの外部変更 mtime(同じ競合を連続通知しないため)。
    pub conflict_notified: Option<SystemTime>,
}

impl Buffer {
    pub fn dirty(&self) -> bool {
        hash_str(&self.text) != self.saved_hash
    }
}

pub struct Editor {
    pub buffers: Vec<Buffer>,
    pub active: Option<usize>,
    next_id: u64,
    /// (line, col) of the active buffer's cursor, 1-based.
    pub cursor: (usize, usize),
    untitled_count: u64,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            buffers: Vec::new(),
            active: None,
            next_id: 1,
            cursor: (1, 1),
            untitled_count: 0,
        }
    }

    pub fn new_untitled(&mut self) {
        self.untitled_count += 1;
        let id = self.next_id;
        self.next_id += 1;
        self.buffers.push(Buffer {
            id,
            path: None,
            title: format!("untitled-{}", self.untitled_count),
            text: String::new(),
            saved_hash: hash_str(""),
            lang: "Plain Text".into(),
            cache: None,
            gutter: None,
            disk_mtime: None,
            conflict_notified: None,
        });
        self.active = Some(self.buffers.len() - 1);
    }

    /// Open a file (or focus it if already open).
    /// 既に開いていたタブをディスクから読み直したときだけ Ok(true)。
    pub fn open(&mut self, path: &Path, hl: &Highlighter) -> Result<bool, String> {
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(i) = self
            .buffers
            .iter()
            .position(|b| b.path.as_deref() == Some(canon.as_path()))
        {
            self.active = Some(i);
            // 外部(エージェント等)がファイルを書き換えていたら、
            // 未保存の編集が無い場合に限りディスクの内容へ読み直す
            return Ok(self.reload_from_disk(i));
        }

        let text = std::fs::read_to_string(&canon)
            .map_err(|e| format!("開けませんでした: {e}"))?;
        let lang = hl.lang_for(Some(&canon), &text);
        let title = canon
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "???".into());

        let id = self.next_id;
        self.next_id += 1;
        let mtime = disk_mtime(&canon);
        self.buffers.push(Buffer {
            id,
            path: Some(canon),
            title,
            saved_hash: hash_str(&text),
            text,
            lang,
            cache: None,
            gutter: None,
            disk_mtime: mtime,
            conflict_notified: None,
        });
        self.active = Some(self.buffers.len() - 1);
        Ok(false)
    }

    /// バッファをディスクの内容で読み直す。読み直したときだけ true。
    /// 未保存の編集があるバッファには触らない。読めない場合(削除等)も何もしない。
    pub fn reload_from_disk(&mut self, i: usize) -> bool {
        let Some(b) = self.buffers.get_mut(i) else {
            return false;
        };
        let Some(path) = b.path.clone() else {
            return false;
        };
        let m = disk_mtime(&path);
        let Ok(text) = std::fs::read_to_string(&path) else {
            b.disk_mtime = m;
            return false;
        };
        if text == b.text {
            // 内容は同じ(自前の保存・touch 等)。保存済み扱いに同期するだけ
            b.disk_mtime = m;
            b.conflict_notified = None;
            b.saved_hash = hash_str(&text);
            return false;
        }
        if b.dirty() {
            // 未保存の編集は守る。mtime も据え置き、ポーリング側が競合を警告できるようにする
            return false;
        }
        b.disk_mtime = m;
        b.conflict_notified = None;
        b.saved_hash = hash_str(&text);
        b.text = text;
        b.cache = None;
        b.gutter = None;
        true
    }

    /// 全バッファの外部変更を確認する。クリーンなバッファは自動で読み直し、
    /// 未保存の編集と競合したバッファは一度だけ Conflict を報告する。
    pub fn check_external(&mut self) -> Vec<ExternalEvent> {
        let mut events = Vec::new();
        for i in 0..self.buffers.len() {
            let Some(path) = self.buffers[i].path.clone() else {
                continue;
            };
            let m = disk_mtime(&path);
            if m == self.buffers[i].disk_mtime {
                continue;
            }
            if self.buffers[i].dirty() {
                let b = &mut self.buffers[i];
                if b.conflict_notified != m {
                    b.conflict_notified = m;
                    events.push(ExternalEvent::Conflict {
                        title: b.title.clone(),
                    });
                }
                continue;
            }
            if self.reload_from_disk(i) {
                events.push(ExternalEvent::Reloaded {
                    index: i,
                    title: self.buffers[i].title.clone(),
                });
            }
        }
        events
    }

    pub fn close(&mut self, i: usize) {
        if i >= self.buffers.len() {
            return;
        }
        self.buffers.remove(i);
        self.active = if self.buffers.is_empty() {
            None
        } else {
            Some(match self.active {
                Some(a) if a > i => a - 1,
                Some(a) => a.min(self.buffers.len() - 1),
                None => 0,
            })
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::Highlighter;
    use crate::test_util::unique_temp_dir;

    /// 外部変更を mtime 差として確実に検知させる（同一秒内の書き換え対策）。
    fn bump_mtime(path: &Path) {
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        let f = std::fs::File::options()
            .append(true)
            .open(path)
            .expect("open for mtime bump");
        f.set_modified(future).expect("set mtime");
    }

    fn open_one(dir: &Path, name: &str, content: &str) -> (Editor, PathBuf, Highlighter) {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write initial file");
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false));
        (ed, path, hl)
    }

    #[test]
    fn external_change_reloads_clean_buffer() {
        let dir = unique_temp_dir("zaivern-editor-test", "reload");
        let (mut ed, path, _hl) = open_one(&dir, "a.md", "old");

        std::fs::write(&path, "new").expect("external write");
        bump_mtime(&path);

        let events = ed.check_external();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ExternalEvent::Reloaded { .. }));
        assert_eq!(ed.buffers[0].text, "new");
        assert!(!ed.buffers[0].dirty());

        // 変化が無ければ以後イベントは出ない
        assert!(ed.check_external().is_empty());
    }

    #[test]
    fn external_change_keeps_dirty_buffer_and_warns_once() {
        let dir = unique_temp_dir("zaivern-editor-test", "conflict");
        let (mut ed, path, _hl) = open_one(&dir, "a.md", "old");
        ed.buffers[0].text = "my unsaved edit".into();

        std::fs::write(&path, "agent wrote this").expect("external write");
        bump_mtime(&path);

        let events = ed.check_external();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ExternalEvent::Conflict { .. }));
        assert_eq!(ed.buffers[0].text, "my unsaved edit");

        // 同じ外部変更で二度は警告しない
        assert!(ed.check_external().is_empty());
    }

    #[test]
    fn reopen_reloads_from_disk() {
        let dir = unique_temp_dir("zaivern-editor-test", "reopen");
        let (mut ed, path, hl) = open_one(&dir, "a.md", "old");

        std::fs::write(&path, "new").expect("external write");
        bump_mtime(&path);

        // 既に開いているファイルを開き直す → ディスクの内容へ読み直される
        assert_eq!(ed.open(&path, &hl), Ok(true));
        assert_eq!(ed.buffers.len(), 1);
        assert_eq!(ed.buffers[0].text, "new");
    }

    #[test]
    fn identical_disk_content_syncs_without_event() {
        let dir = unique_temp_dir("zaivern-editor-test", "touch");
        let (mut ed, path, _hl) = open_one(&dir, "a.md", "same");

        // 内容は同じで mtime だけ変わった（touch 相当）→ イベント無し
        bump_mtime(&path);
        assert!(ed.check_external().is_empty());
        assert_eq!(ed.buffers[0].text, "same");
    }
}
