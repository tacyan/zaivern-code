//! カラーテーマ JSON (VS Code 互換形式) を Zaivern の Theme に変換して取り込む。
//! ~/.zaivern/themes/*.json とプラグイン同梱テーマがこの形式を使う。

use std::path::Path;

use eframe::egui::Color32;

use crate::theme::{self, Theme};

/// ~/.zaivern/themes/*.json を列挙する: (label, full path)。
/// プラグイン同梱テーマは app 側でこの一覧へマージされる。
pub fn discover_user_themes() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        scan_flat(&home.join(".zaivern").join("themes"), &mut out);
    }
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    out.dedup_by(|a, b| a.1 == b.1);
    out
}

fn scan_flat(dir: &Path, out: &mut Vec<(String, String)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) == Some("json") {
            let label = p
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
                .trim_end_matches("-color-theme")
                .to_string();
            if !label.is_empty() {
                out.push((label, p.to_string_lossy().to_string()));
            }
        }
    }
}

/// カラーテーマ JSON を読み込み Zaivern Theme へマップする。
pub fn load(path: &Path) -> Result<Theme, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("テーマを読めません ({}): {e}", path.display()))?;
    let clean = strip_jsonc(&raw);
    let v: serde_json::Value =
        serde_json::from_str(&clean).map_err(|e| format!("テーマJSONの解析に失敗: {e}"))?;

    let colors = v.get("colors").and_then(|c| c.as_object());
    let get = |keys: &[&str]| -> Option<Color32> {
        let colors = colors?;
        for k in keys {
            if let Some(c) = colors.get(*k).and_then(|x| x.as_str()).and_then(parse_color) {
                return Some(c);
            }
        }
        None
    };

    let bg = get(&["editor.background"]).unwrap_or(Color32::from_rgb(0x1e, 0x1e, 0x1e));
    let dark = v
        .get("type")
        .and_then(|t| t.as_str())
        .map(|t| t != "light" && t != "hc-light")
        .unwrap_or_else(|| luminance(bg) < 0.5);

    let mut t = theme::by_name(if dark { "zaivern-dark" } else { "zaivern-light" });
    t.name = path.to_string_lossy().to_string();
    t.label = v
        .get("name")
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
                .trim_end_matches("-color-theme")
                .to_string()
        });
    t.dark = dark;
    t.bg = bg;
    t.panel = get(&["sideBar.background", "activityBar.background", "panel.background"])
        .unwrap_or_else(|| shift(bg, if dark { 12 } else { -8 }));
    t.panel_alt = get(&[
        "editorGroupHeader.tabsBackground",
        "tab.inactiveBackground",
        "titleBar.activeBackground",
    ])
    .unwrap_or_else(|| shift(bg, if dark { 7 } else { -5 }));
    if let Some(c) = get(&[
        "focusBorder",
        "activityBarBadge.background",
        "button.background",
        "progressBar.background",
    ]) {
        t.accent = c;
    }
    t.accent_soft = get(&["list.activeSelectionBackground", "editor.selectionBackground"])
        .unwrap_or_else(|| t.accent.gamma_multiply(0.25));
    if let Some(c) = get(&["editor.foreground", "foreground"]) {
        t.text = c;
    }
    t.text_dim = get(&[
        "descriptionForeground",
        "tab.inactiveForeground",
        "editorLineNumber.foreground",
    ])
    .unwrap_or_else(|| t.text.gamma_multiply(0.6));
    t.border = get(&["panel.border", "editorGroup.border", "sideBar.border", "contrastBorder"])
        .unwrap_or_else(|| shift(bg, if dark { 22 } else { -16 }));
    t.term_bg = get(&["terminal.background"]).unwrap_or(bg);
    t.term_fg = get(&["terminal.foreground"]).unwrap_or(t.text);
    if let Some(c) = get(&["editorError.foreground", "errorForeground"]) {
        t.err = c;
    }
    if let Some(c) = get(&["editorWarning.foreground"]) {
        t.warn = c;
    }

    const ANSI_KEYS: [&str; 16] = [
        "terminal.ansiBlack",
        "terminal.ansiRed",
        "terminal.ansiGreen",
        "terminal.ansiYellow",
        "terminal.ansiBlue",
        "terminal.ansiMagenta",
        "terminal.ansiCyan",
        "terminal.ansiWhite",
        "terminal.ansiBrightBlack",
        "terminal.ansiBrightRed",
        "terminal.ansiBrightGreen",
        "terminal.ansiBrightYellow",
        "terminal.ansiBrightBlue",
        "terminal.ansiBrightMagenta",
        "terminal.ansiBrightCyan",
        "terminal.ansiBrightWhite",
    ];
    for (i, k) in ANSI_KEYS.iter().enumerate() {
        if let Some(c) = get(&[k]) {
            t.ansi[i] = c;
        }
    }

    t.syntect_theme = if dark {
        "base16-ocean.dark".into()
    } else {
        "InspiredGitHub".into()
    };
    Ok(t)
}

fn luminance(c: Color32) -> f32 {
    (0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32) / 255.0
}

fn shift(c: Color32, d: i16) -> Color32 {
    let f = |v: u8| (v as i16 + d).clamp(0, 255) as u8;
    Color32::from_rgb(f(c.r()), f(c.g()), f(c.b()))
}

fn parse_color(s: &str) -> Option<Color32> {
    let hex = s.trim().strip_prefix('#')?;
    let h = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    match hex.len() {
        3 => {
            let d = |i: usize| {
                u8::from_str_radix(hex.get(i..i + 1)?, 16)
                    .ok()
                    .map(|v| v * 17)
            };
            Some(Color32::from_rgb(d(0)?, d(1)?, d(2)?))
        }
        6 => Some(Color32::from_rgb(h(0)?, h(2)?, h(4)?)),
        8 => Some(Color32::from_rgba_unmultiplied(h(0)?, h(2)?, h(4)?, h(6)?)),
        _ => None,
    }
}

/// JSONC (コメント・末尾カンマ入りJSON) を素のJSONへ変換する。
/// マルチバイト安全のため全処理をバイト単位で行う。
fn strip_jsonc(s: &str) -> String {
    let b = s.as_bytes();
    let mut pass1: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut in_str = false;
    let mut escape = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            pass1.push(c);
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
        } else if c == b'"' {
            in_str = true;
            pass1.push(c);
            i += 1;
        } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
        } else {
            pass1.push(c);
            i += 1;
        }
    }

    // 末尾カンマ除去
    let mut out: Vec<u8> = Vec::with_capacity(pass1.len());
    let mut in_str = false;
    let mut escape = false;
    for (idx, &ch) in pass1.iter().enumerate() {
        if in_str {
            out.push(ch);
            if escape {
                escape = false;
            } else if ch == b'\\' {
                escape = true;
            } else if ch == b'"' {
                in_str = false;
            }
            continue;
        }
        if ch == b'"' {
            in_str = true;
            out.push(ch);
            continue;
        }
        if ch == b',' {
            let mut j = idx + 1;
            while j < pass1.len() && pass1[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < pass1.len() && (pass1[j] == b'}' || pass1[j] == b']') {
                continue;
            }
        }
        out.push(ch);
    }
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// std::env::temp_dir() 配下に一意なディレクトリを自作する（HOME 非依存）。
    fn unique_temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "zaivern-themejson-test-{}-{}-{}-{}",
            tag,
            std::process::id(),
            nanos,
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("create unique temp dir");
        dir
    }

    fn write_theme(dir: &Path, file: &str, body: &str) -> PathBuf {
        let p = dir.join(file);
        std::fs::write(&p, body).expect("write theme json");
        p
    }

    /// strip_jsonc の結果が素の JSON として解釈できることを確認する。
    fn strip_and_parse(src: &str) -> serde_json::Value {
        let clean = strip_jsonc(src);
        serde_json::from_str(&clean)
            .unwrap_or_else(|e| panic!("strip_jsonc の出力が JSON として不正: {e} / {clean:?}"))
    }

    // ---- strip_jsonc: 文字列リテラルを壊さないこと ----

    #[test]
    fn strip_jsonc_keeps_double_slash_inside_string() {
        let src = r#"{"url": "http://example.com"}"#;
        assert_eq!(strip_jsonc(src), src);
        assert_eq!(strip_and_parse(src)["url"], "http://example.com");
    }

    #[test]
    fn strip_jsonc_keeps_block_comment_markers_inside_string() {
        let src = r#"{"a": "/* not a comment */", "b": "x /* y"}"#;
        assert_eq!(strip_jsonc(src), src);
        let v = strip_and_parse(src);
        assert_eq!(v["a"], "/* not a comment */");
        assert_eq!(v["b"], "x /* y");
    }

    #[test]
    fn strip_jsonc_keeps_comment_after_escaped_quote_in_string() {
        // 文字列内の \" で in_str が誤って閉じないこと
        let src = r#"{"a": "say \" // still inside"}"#;
        assert_eq!(strip_jsonc(src), src);
        assert_eq!(strip_and_parse(src)["a"], r#"say " // still inside"#);
    }

    #[test]
    fn strip_jsonc_keeps_trailing_comma_lookalike_inside_string() {
        let src = r#"{"a": "1,]", "b": "2,}"}"#;
        assert_eq!(strip_jsonc(src), src);
        let v = strip_and_parse(src);
        assert_eq!(v["a"], "1,]");
        assert_eq!(v["b"], "2,}");
    }

    #[test]
    fn strip_jsonc_keeps_backslash_pair_before_closing_quote() {
        // "C:\\" の直後の // が文字列外のコメントとして扱われること
        let src = "{\"a\": \"C:\\\\\" // tail\n}";
        let v = strip_and_parse(src);
        assert_eq!(v["a"], "C:\\");
    }

    // ---- strip_jsonc: コメント・末尾カンマの除去 ----

    #[test]
    fn strip_jsonc_removes_line_comment() {
        let src = "{\n  // これはコメント\n  \"a\": 1\n}";
        let v = strip_and_parse(src);
        assert_eq!(v["a"], 1);
        assert!(!strip_jsonc(src).contains("コメント"));
    }

    #[test]
    fn strip_jsonc_removes_block_comment() {
        let src = r#"{"a": /* mid */ 1, /* multi
line */ "b": 2}"#;
        let v = strip_and_parse(src);
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], 2);
    }

    #[test]
    fn strip_jsonc_removes_trailing_comma_in_object_and_array() {
        let v = strip_and_parse(r#"{"a": [1, 2, ], "b": 3,}"#);
        assert_eq!(v["a"][1], 2);
        assert_eq!(v["b"], 3);
    }

    #[test]
    fn strip_jsonc_removes_trailing_comma_followed_by_comment() {
        let src = "{\n  \"a\": 1, // 末尾カンマ + コメント\n}";
        assert_eq!(strip_and_parse(src)["a"], 1);
    }

    #[test]
    fn strip_jsonc_keeps_multibyte_text_intact() {
        let src = r#"{"名前": "日本語テーマ"}"#;
        assert_eq!(strip_jsonc(src), src);
        assert_eq!(strip_and_parse(src)["名前"], "日本語テーマ");
    }

    #[test]
    fn strip_jsonc_empty_input_stays_empty() {
        assert_eq!(strip_jsonc(""), "");
    }

    #[test]
    fn strip_jsonc_unterminated_block_comment_consumes_rest() {
        assert_eq!(strip_jsonc("{\"a\": 1 /* oops"), "{\"a\": 1 ");
    }

    // ---- parse_color ----

    #[test]
    fn parse_color_six_digit_hex() {
        assert_eq!(
            parse_color("#ff8800"),
            Some(Color32::from_rgb(0xff, 0x88, 0x00))
        );
    }

    #[test]
    fn parse_color_three_digit_hex_expands_each_nibble() {
        assert_eq!(
            parse_color("#abc"),
            Some(Color32::from_rgb(0xaa, 0xbb, 0xcc))
        );
        assert_eq!(
            parse_color("#fff"),
            Some(Color32::from_rgb(0xff, 0xff, 0xff))
        );
        assert_eq!(parse_color("#000"), Some(Color32::from_rgb(0, 0, 0)));
    }

    #[test]
    fn parse_color_eight_digit_hex_carries_alpha() {
        assert_eq!(
            parse_color("#11223344"),
            Some(Color32::from_rgba_unmultiplied(0x11, 0x22, 0x33, 0x44))
        );
    }

    #[test]
    fn parse_color_trims_surrounding_whitespace() {
        assert_eq!(
            parse_color("  #ff0000\t"),
            Some(Color32::from_rgb(0xff, 0, 0))
        );
    }

    #[test]
    fn parse_color_is_case_insensitive() {
        assert_eq!(parse_color("#AbCdEf"), parse_color("#abcdef"));
    }

    #[test]
    fn parse_color_rejects_missing_hash() {
        assert_eq!(parse_color("ff8800"), None);
    }

    #[test]
    fn parse_color_rejects_bad_length() {
        assert_eq!(parse_color("#"), None);
        assert_eq!(parse_color("#f"), None);
        assert_eq!(parse_color("#ff"), None);
        assert_eq!(parse_color("#ff88"), None);
        assert_eq!(parse_color("#ff8800a"), None);
        assert_eq!(parse_color("#ff8800aabb"), None);
    }

    #[test]
    fn parse_color_rejects_non_hex_digits() {
        assert_eq!(parse_color("#gggggg"), None);
        assert_eq!(parse_color("#zzz"), None);
        assert_eq!(parse_color("#12345g"), None);
    }

    #[test]
    fn parse_color_rejects_empty_and_multibyte() {
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("   "), None);
        assert_eq!(parse_color("#あいう"), None);
    }

    // ---- luminance / shift ----

    #[test]
    fn luminance_spans_black_to_white() {
        assert!(luminance(Color32::BLACK).abs() < 1e-6);
        assert!((luminance(Color32::WHITE) - 1.0).abs() < 1e-6);
        assert!(luminance(Color32::from_rgb(0x1e, 0x1e, 0x1e)) < 0.5);
        assert!(luminance(Color32::from_rgb(0xf5, 0xf5, 0xf5)) > 0.5);
    }

    #[test]
    fn shift_clamps_at_both_ends() {
        assert_eq!(shift(Color32::from_rgb(10, 10, 10), -50), Color32::from_rgb(0, 0, 0));
        assert_eq!(
            shift(Color32::from_rgb(250, 250, 250), 50),
            Color32::from_rgb(255, 255, 255)
        );
        assert_eq!(
            shift(Color32::from_rgb(100, 110, 120), 10),
            Color32::from_rgb(110, 120, 130)
        );
    }

    // ---- scan_flat ----

    #[test]
    fn scan_flat_collects_json_and_strips_color_theme_suffix() {
        let dir = unique_temp_dir("scan");
        write_theme(&dir, "solarized-color-theme.json", "{}");
        write_theme(&dir, "plain.json", "{}");
        write_theme(&dir, "notes.txt", "not a theme");
        let mut out = Vec::new();
        scan_flat(&dir, &mut out);
        let mut labels: Vec<String> = out.iter().map(|(l, _)| l.clone()).collect();
        labels.sort();
        assert_eq!(labels, vec!["plain".to_string(), "solarized".to_string()]);
    }

    #[test]
    fn scan_flat_on_missing_dir_yields_nothing() {
        let mut out = Vec::new();
        scan_flat(Path::new("/no/such/zaivern-theme-dir"), &mut out);
        assert!(out.is_empty());
    }

    // ---- load ----

    #[test]
    fn load_missing_file_returns_error() {
        // Theme は Debug を実装していないため unwrap_err() は使えない
        let err = match load(Path::new("/no/such/zaivern-theme.json")) {
            Err(e) => e,
            Ok(_) => panic!("存在しないパスなのに Ok が返った"),
        };
        assert!(err.contains("テーマを読めません"), "unexpected: {err}");
    }

    #[test]
    fn load_broken_json_returns_parse_error() {
        let dir = unique_temp_dir("broken");
        let p = write_theme(&dir, "broken.json", "{ this is not json ");
        let err = match load(&p) {
            Err(e) => e,
            Ok(_) => panic!("壊れた JSON なのに Ok が返った"),
        };
        assert!(err.contains("テーマJSONの解析に失敗"), "unexpected: {err}");
    }

    #[test]
    fn load_empty_object_falls_back_to_defaults() {
        let dir = unique_temp_dir("empty");
        let p = write_theme(&dir, "empty.json", "{}");
        let t = load(&p).expect("empty object should still load");
        assert_eq!(t.bg, Color32::from_rgb(0x1e, 0x1e, 0x1e));
        assert!(t.dark, "既定背景は暗いので dark 判定になる");
        assert_eq!(t.label, "empty");
        assert_eq!(t.name, p.to_string_lossy().to_string());
        assert_eq!(t.syntect_theme, "base16-ocean.dark");
    }

    #[test]
    fn load_uses_name_field_as_label() {
        let dir = unique_temp_dir("label");
        let p = write_theme(&dir, "whatever.json", r#"{"name": "My Theme"}"#);
        assert_eq!(load(&p).expect("load").label, "My Theme");
    }

    #[test]
    fn load_falls_back_to_file_stem_without_color_theme_suffix() {
        let dir = unique_temp_dir("stem");
        let p = write_theme(&dir, "dracula-color-theme.json", "{}");
        assert_eq!(load(&p).expect("load").label, "dracula");
    }

    #[test]
    fn load_maps_colors_section() {
        let dir = unique_temp_dir("colors");
        let src = r##"{
            // 行コメント入り JSONC
            "name": "Mapped",
            "type": "dark",
            "colors": {
                "editor.background": "#101010",
                "sideBar.background": "#202020",
                "focusBorder": "#ff0000",
                "editor.foreground": "#eeeeee",
                "terminal.background": "#030303",
                "editorError.foreground": "#ff00ff",
                "terminal.ansiRed": "#c00000",
            },
        }"##;
        let p = write_theme(&dir, "mapped.json", src);
        let t = load(&p).expect("load");
        assert_eq!(t.bg, Color32::from_rgb(0x10, 0x10, 0x10));
        assert_eq!(t.panel, Color32::from_rgb(0x20, 0x20, 0x20));
        assert_eq!(t.accent, Color32::from_rgb(0xff, 0, 0));
        assert_eq!(t.text, Color32::from_rgb(0xee, 0xee, 0xee));
        assert_eq!(t.term_bg, Color32::from_rgb(0x03, 0x03, 0x03));
        assert_eq!(t.err, Color32::from_rgb(0xff, 0, 0xff));
        assert_eq!(t.ansi[1], Color32::from_rgb(0xc0, 0, 0));
    }

    #[test]
    fn load_uses_key_fallback_order() {
        let dir = unique_temp_dir("fallback");
        // sideBar.background は無く activityBar.background だけある
        let src = r##"{"colors": {
            "editor.background": "#101010",
            "activityBar.background": "#303030"
        }}"##;
        let p = write_theme(&dir, "fb.json", src);
        assert_eq!(load(&p).expect("load").panel, Color32::from_rgb(0x30, 0x30, 0x30));
    }

    #[test]
    fn load_ignores_unknown_keys_and_invalid_color_values() {
        let dir = unique_temp_dir("invalid");
        let src = r##"{"colors": {
            "editor.background": "rgb(1,2,3)",
            "totally.unknown.key": "#123456",
            "terminal.background": 42
        }}"##;
        let p = write_theme(&dir, "inv.json", src);
        let t = load(&p).expect("不正な色値でも load 自体は成功する");
        // 不正値は無視され既定の背景にフォールバックする
        assert_eq!(t.bg, Color32::from_rgb(0x1e, 0x1e, 0x1e));
        assert_eq!(t.term_bg, t.bg);
    }

    #[test]
    fn load_light_type_wins_over_dark_background() {
        let dir = unique_temp_dir("light-type");
        let src = r##"{"type": "light", "colors": {"editor.background": "#000000"}}"##;
        let p = write_theme(&dir, "lt.json", src);
        let t = load(&p).expect("load");
        assert!(!t.dark);
        assert_eq!(t.syntect_theme, "InspiredGitHub");
        assert_eq!(t.bg, Color32::from_rgb(0, 0, 0));
    }

    #[test]
    fn load_hc_light_type_is_treated_as_light() {
        let dir = unique_temp_dir("hc-light");
        let p = write_theme(&dir, "hc.json", r#"{"type": "hc-light"}"#);
        assert!(!load(&p).expect("load").dark);
    }

    #[test]
    fn load_infers_light_from_background_luminance_when_type_missing() {
        let dir = unique_temp_dir("infer");
        let src = r##"{"colors": {"editor.background": "#fafafa"}}"##;
        let p = write_theme(&dir, "infer.json", src);
        let t = load(&p).expect("load");
        assert!(!t.dark);
        assert_eq!(t.syntect_theme, "InspiredGitHub");
    }

    #[test]
    fn load_accepts_url_like_string_in_json() {
        let dir = unique_temp_dir("url");
        // 文字列内の // がコメント除去で壊されないこと（壊れると JSON 解析に失敗する）
        let src = r##"{"name": "Has URL", "homepage": "http://example.com/x", "colors": {"editor.background": "#123456"}}"##;
        let p = write_theme(&dir, "url.json", src);
        let t = load(&p).expect("URL を含むテーマも読める");
        assert_eq!(t.label, "Has URL");
        assert_eq!(t.bg, Color32::from_rgb(0x12, 0x34, 0x56));
    }
}
