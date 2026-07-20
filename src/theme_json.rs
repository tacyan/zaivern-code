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
