use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use eframe::egui::text::LayoutJob;

use crate::highlight::Highlighter;

pub fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
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
        });
        self.active = Some(self.buffers.len() - 1);
    }

    /// Open a file (or focus it if already open).
    pub fn open(&mut self, path: &Path, hl: &Highlighter) -> Result<(), String> {
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(i) = self
            .buffers
            .iter()
            .position(|b| b.path.as_deref() == Some(canon.as_path()))
        {
            self.active = Some(i);
            return Ok(());
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
        self.buffers.push(Buffer {
            id,
            path: Some(canon),
            title,
            saved_hash: hash_str(&text),
            text,
            lang,
            cache: None,
            gutter: None,
        });
        self.active = Some(self.buffers.len() - 1);
        Ok(())
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
