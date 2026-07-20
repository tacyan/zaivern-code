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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// SyntaxSet / ThemeSet のロードは重いので、テスト全体で 1 個だけ作って共有する。
    /// (syntect の SyntaxSet は Send + Sync なので static に置ける)
    fn hl() -> &'static Highlighter {
        static HL: OnceLock<Highlighter> = OnceLock::new();
        HL.get_or_init(Highlighter::new)
    }

    /// 実プロダクトでも使われているテーマ名 (theme.rs のダークテーマ既定値)。
    const THEME: &str = "base16-ocean.dark";

    fn font() -> FontId {
        FontId::monospace(12.0)
    }

    /// どのテーマ色とも被りにくい番兵色。これが出たら「ハイライトせず素通し」の証拠。
    fn fallback() -> Color32 {
        Color32::from_rgb(1, 2, 3)
    }

    fn job_of(text: &str, lang: &str) -> LayoutJob {
        hl().layout_job(text, lang, THEME, font(), fallback())
    }

    /// スパン列の健全性: 入力文字列を完全に復元し、単調・非重複・境界内・
    /// かつ char 境界を割っていないこと。マルチバイト崩れの検出器。
    fn assert_spans_ok(job: &LayoutJob, text: &str) {
        assert_eq!(job.text, text, "layout job must reproduce the input text");

        let mut prev_end = 0usize;
        for (i, s) in job.sections.iter().enumerate() {
            assert!(
                s.byte_range.start <= s.byte_range.end,
                "section {i} has an inverted range {:?}",
                s.byte_range
            );
            assert_eq!(
                s.byte_range.start, prev_end,
                "section {i} must start where the previous one ended (no gap / no overlap)"
            );
            assert!(
                s.byte_range.end <= job.text.len(),
                "section {i} range {:?} exceeds text length {}",
                s.byte_range,
                job.text.len()
            );
            assert!(
                job.text.is_char_boundary(s.byte_range.start)
                    && job.text.is_char_boundary(s.byte_range.end),
                "section {i} range {:?} splits a UTF-8 char boundary",
                s.byte_range
            );
            prev_end = s.byte_range.end;
        }

        if !text.is_empty() {
            assert_eq!(
                prev_end,
                text.len(),
                "sections must cover the whole text, ending at its length"
            );
        }
    }

    fn color_at(job: &LayoutJob, byte_idx: usize) -> Color32 {
        job.sections
            .iter()
            .find(|s| s.byte_range.start <= byte_idx && byte_idx < s.byte_range.end)
            .map(|s| s.format.color)
            .unwrap_or_else(|| panic!("no section covers byte {byte_idx}"))
    }

    /// `needle` の最初の出現位置の色を返す。
    fn color_of(job: &LayoutJob, needle: &str) -> Color32 {
        let i = job
            .text
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not found in laid out text"));
        color_at(job, i)
    }

    // ---- lang_for -------------------------------------------------------

    #[test]
    fn lang_for_resolves_known_extension() {
        assert_eq!(hl().lang_for(Some(Path::new("a.rs")), "fn main() {}"), "Rust");
    }

    #[test]
    fn lang_for_unknown_extension_falls_back_to_plain_text() {
        assert_eq!(
            hl().lang_for(Some(Path::new("notes.zzqqxx")), "hello world"),
            "Plain Text"
        );
    }

    #[test]
    fn lang_for_uses_whole_file_name_when_there_is_no_extension() {
        // 拡張子なしファイル (Makefile 等) は file_name 経由で解決される分岐。
        assert_ne!(
            hl().lang_for(Some(Path::new("/proj/Makefile")), "all:\n\techo hi\n"),
            "Plain Text"
        );
    }

    #[test]
    fn lang_for_falls_back_to_first_line_when_path_is_none() {
        // シェバンによる判定 (path が無いケース)。
        assert_ne!(
            hl().lang_for(None, "#!/usr/bin/env python3\nprint(1)\n"),
            "Plain Text"
        );
    }

    #[test]
    fn lang_for_prefers_extension_over_first_line() {
        // 拡張子が勝つこと。シェバンに引きずられて Python にならない。
        assert_eq!(
            hl().lang_for(Some(Path::new("a.rs")), "#!/usr/bin/env python3\n"),
            "Rust"
        );
    }

    #[test]
    fn lang_for_handles_empty_text_without_path() {
        assert_eq!(hl().lang_for(None, ""), "Plain Text");
    }

    #[test]
    fn lang_for_handles_multibyte_first_line() {
        // 日本語だけの 1 行目で first_line 判定に入っても panic しない。
        let lang = hl().lang_for(None, "日本語のテキストです\n2行目\n");
        assert!(!lang.is_empty());
    }

    // ---- lang_for_fence -------------------------------------------------

    #[test]
    fn lang_for_fence_resolves_name_token() {
        assert_eq!(hl().lang_for_fence("rust"), "Rust");
    }

    #[test]
    fn lang_for_fence_resolves_extension_token() {
        assert_eq!(hl().lang_for_fence("py"), "Python");
    }

    #[test]
    fn lang_for_fence_empty_token_is_plain_text() {
        assert_eq!(hl().lang_for_fence(""), "Plain Text");
    }

    #[test]
    fn lang_for_fence_unknown_token_is_plain_text() {
        assert_eq!(hl().lang_for_fence("no-such-language-xyz"), "Plain Text");
    }

    // ---- トークン分類 ---------------------------------------------------

    #[test]
    fn keyword_and_function_name_get_different_colors() {
        let job = job_of("fn main() {}\n", "Rust");
        assert_ne!(
            color_of(&job, "fn"),
            color_of(&job, "main"),
            "keyword and function name must not share a color"
        );
    }

    #[test]
    fn number_literal_differs_from_identifier() {
        let job = job_of("let n = 42;\n", "Rust");
        assert_ne!(color_of(&job, "42"), color_of(&job, "n ="));
    }

    #[test]
    fn comment_and_string_get_different_colors() {
        let comment = job_of("// alpha\n", "Rust");
        let string = job_of("let s = \"alpha\";\n", "Rust");
        assert_ne!(
            color_of(&comment, "alpha"),
            color_of(&string, "alpha"),
            "a comment and a string literal must not share a color"
        );
    }

    // ---- 文字列内の誤分類 -----------------------------------------------

    #[test]
    fn keyword_inside_string_literal_is_not_colored_as_keyword() {
        let text = "let s = \"fn abc\";\n";
        let job = job_of(text, "Rust");
        let inside_fn = job.text.find("\"fn").expect("quote") + 1;
        let inside_abc = job.text.find("abc").expect("abc");

        assert_eq!(
            color_at(&job, inside_fn),
            color_at(&job, inside_abc),
            "`fn` inside a string must be colored like the rest of the string"
        );
        assert_ne!(
            color_at(&job, inside_fn),
            color_of(&job, "let"),
            "`fn` inside a string must not be colored as a keyword"
        );
    }

    #[test]
    fn comment_marker_inside_string_does_not_start_a_comment() {
        let with_marker = job_of("let s = \"// not a comment\";\nlet n = 1;\n", "Rust");
        let baseline = job_of("let n = 1;\n", "Rust");
        assert_eq!(
            color_of(&with_marker, "1"),
            color_of(&baseline, "1"),
            "code after a string containing `//` must still be highlighted as code"
        );
    }

    #[test]
    fn block_comment_marker_inside_string_does_not_open_a_comment() {
        let with_marker = job_of("let s = \"/* open\";\nlet n = 7;\n", "Rust");
        let baseline = job_of("let n = 7;\n", "Rust");
        assert_eq!(color_of(&with_marker, "7"), color_of(&baseline, "7"));
    }

    // ---- 未終端トークン -------------------------------------------------

    #[test]
    fn unterminated_string_is_laid_out_without_panicking() {
        let text = "let s = \"never closed\nlet t = 2;\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn unterminated_block_comment_is_laid_out_without_panicking() {
        let text = "/* open block\nstill inside\nand still\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn unterminated_string_at_eof_without_newline_is_laid_out() {
        let text = "let s = \"dangling";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    // ---- マルチバイト ---------------------------------------------------

    #[test]
    fn japanese_comment_does_not_panic_and_preserves_text() {
        let text = "// 日本語のコメント\nfn main() {}\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn japanese_string_literal_does_not_panic_and_preserves_text() {
        let text = "let s = \"日本語\";\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn unterminated_japanese_string_does_not_panic() {
        // 未終端 × マルチバイトの合わせ技。byte index スライスが char 境界を
        // 割るなら、ここが最初に落ちる。
        let text = "let s = \"日本語のまま閉じない\nlet t = 3;\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn emoji_and_combining_characters_are_preserved() {
        let text = "let s = \"🎌 とれ́ま\"; // 絵文字\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn japanese_survives_the_plain_text_fallback_path() {
        let text = "日本語のプレーンテキスト\n2行目\n";
        let job = job_of(text, "Plain Text");
        assert_spans_ok(&job, text);
    }

    // ---- 空・境界・フォールバック ---------------------------------------

    #[test]
    fn empty_text_produces_no_visible_content() {
        let job = job_of("", "Rust");
        assert_eq!(job.text, "");
    }

    #[test]
    fn empty_text_in_plain_mode_produces_no_visible_content() {
        let job = job_of("", "Plain Text");
        assert_eq!(job.text, "");
    }

    #[test]
    fn blank_and_whitespace_only_lines_are_preserved() {
        let text = "fn a() {}\n\n   \n\nfn b() {}\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn text_without_trailing_newline_is_fully_covered() {
        let text = "fn main() { let x = 1; }";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn crlf_line_endings_are_preserved_verbatim() {
        let text = "fn a() {}\r\n// コメント\r\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn unknown_language_uses_the_fallback_color_in_one_section() {
        let text = "fn main() {}\n";
        let job = job_of(text, "No Such Language 12345");
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, fallback());
        assert_eq!(job.text, text);
    }

    #[test]
    fn unknown_theme_falls_back_to_unhighlighted_text() {
        let text = "fn main() {}\n";
        let job = hl().layout_job(text, "Rust", "no-such-theme-xyz", font(), fallback());
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, fallback());
        assert_eq!(job.text, text);
    }

    #[test]
    fn plain_text_language_skips_highlighting() {
        let text = "fn main() {}\n";
        let job = job_of(text, "Plain Text");
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, fallback());
    }

    #[test]
    fn oversized_text_skips_highlighting() {
        let unit = "fn main() { let x = 1; }\n";
        let text = unit.repeat(MAX_HIGHLIGHT_BYTES / unit.len() + 2);
        assert!(text.len() > MAX_HIGHLIGHT_BYTES);

        let job = job_of(&text, "Rust");
        assert_eq!(job.sections.len(), 1, "large files must be laid out in one plain span");
        assert_eq!(job.sections[0].format.color, fallback());
    }

    #[test]
    fn requested_font_is_applied_to_every_section() {
        let job = job_of("// コメント\nfn main() { let x = \"s\"; }\n", "Rust");
        assert!(job.sections.iter().all(|s| s.format.font_id == font()));
    }

    #[test]
    fn wrapping_is_disabled_so_the_editor_can_scroll_horizontally() {
        let job = job_of("fn main() {}\n", "Rust");
        assert_eq!(job.wrap.max_width, f32::INFINITY);
    }

    #[test]
    fn mixed_token_document_keeps_spans_well_formed() {
        let text = concat!(
            "// 日本語のコメント: \"fn\" や // を含む\n",
            "/* block\n",
            "   still block */\n",
            "fn main() {\n",
            "    let s = \"fn // /* 日本語 \\\" escaped\";\n",
            "    let n = 0x1F + 42.5;\n",
            "\n",
            "    println!(\"{}\", s);\n",
            "}\n",
        );
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }
}
