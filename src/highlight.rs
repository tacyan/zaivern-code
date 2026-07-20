use std::path::Path;

use eframe::egui::{text::LayoutJob, Color32, FontId, TextFormat};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Files larger than this are laid out without highlighting to stay snappy.
const MAX_HIGHLIGHT_BYTES: usize = 400_000;

pub struct Highlighter {
    ps: SyntaxSet,
    ts: ThemeSet,
}

impl Highlighter {
    pub fn new() -> Self {
        Self {
            ps: SyntaxSet::load_defaults_newlines(),
            ts: ThemeSet::load_defaults(),
        }
    }

    pub fn lang_for(&self, path: Option<&Path>, text: &str) -> String {
        if let Some(p) = path {
            if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                if let Some(s) = self.ps.find_syntax_by_extension(ext) {
                    return s.name.clone();
                }
            }
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if let Some(s) = self.ps.find_syntax_by_extension(name) {
                    return s.name.clone();
                }
            }
        }
        if let Some(line) = text.lines().next() {
            if let Some(s) = self.ps.find_syntax_by_first_line(line) {
                return s.name.clone();
            }
        }
        "Plain Text".into()
    }

    /// フェンスコードの言語トークン ("rust", "py" など) から syntect の言語名を引く。
    pub fn lang_for_fence(&self, token: &str) -> String {
        if token.is_empty() {
            return "Plain Text".into();
        }
        self.ps
            .find_syntax_by_token(token)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "Plain Text".into())
    }

    pub fn layout_job(
        &self,
        text: &str,
        lang: &str,
        theme_name: &str,
        font: FontId,
        fallback: Color32,
    ) -> LayoutJob {
        let plain = |job: &mut LayoutJob| {
            job.append(
                text,
                0.0,
                TextFormat {
                    font_id: font.clone(),
                    color: fallback,
                    ..Default::default()
                },
            );
        };

        let mut job = LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;

        let syntax = self
            .ps
            .find_syntax_by_name(lang)
            .unwrap_or_else(|| self.ps.find_syntax_plain_text());

        if text.len() > MAX_HIGHLIGHT_BYTES || syntax.name == "Plain Text" {
            plain(&mut job);
            return job;
        }

        let Some(theme) = self.ts.themes.get(theme_name) else {
            plain(&mut job);
            return job;
        };

        let mut h = HighlightLines::new(syntax, theme);
        for line in LinesWithEndings::from(text) {
            match h.highlight_line(line, &self.ps) {
                Ok(regions) => {
                    for (style, piece) in regions {
                        let fg = style.foreground;
                        let mut fmt = TextFormat {
                            font_id: font.clone(),
                            color: Color32::from_rgb(fg.r, fg.g, fg.b),
                            ..Default::default()
                        };
                        if style.font_style.contains(FontStyle::ITALIC) {
                            fmt.italics = true;
                        }
                        if style.font_style.contains(FontStyle::UNDERLINE) {
                            fmt.underline = eframe::egui::Stroke::new(1.0_f32, fmt.color);
                        }
                        job.append(piece, 0.0, fmt);
                    }
                }
                Err(_) => {
                    job.append(
                        line,
                        0.0,
                        TextFormat {
                            font_id: font.clone(),
                            color: fallback,
                            ..Default::default()
                        },
                    );
                }
            }
        }
        job
    }
}
