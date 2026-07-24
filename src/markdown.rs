//! Markdown プレビュー描画 — 依存追加なしの軽量レンダラ。
//!
//! エディタで開いている .md をレンダリングして表示するためのモジュール。
//! CommonMark 完全準拠は狙わず、README / メモ用途で実用になる範囲を自前実装する:
//! 見出し・段落・箇条書き(ネスト/番号/タスク)・引用・水平線・テーブル・
//! フェンスコード(syntect ハイライト)・インライン装飾(強調/斜体/打消/コード/リンク)。
//!
//! 描画は egui の `horizontal_wrapped` + スパン単位の Label で行い、
//! リンクは `Hyperlink` としてクリックでブラウザが開く。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, FontId, RichText};

use crate::highlight::Highlighter;
use crate::theme::Theme;

/// このバッファを Markdown としてプレビュー可能か。
pub fn is_markdown(title: &str, lang: &str) -> bool {
    let t = title.to_lowercase();
    lang == "Markdown"
        || t.ends_with(".md")
        || t.ends_with(".markdown")
        || t.ends_with(".mdx")
}

// ─── インライン構文 ─────────────────────────────────────────────────

/// インライン装飾を適用した最小単位。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Span {
    pub text: String,
    pub code: bool,
    pub strong: bool,
    pub em: bool,
    pub strike: bool,
    /// Some(url) ならリンク (画像は "🖼 alt" テキストのリンクに落とす)
    pub link: Option<String>,
    /// `![alt](url)` 由来。ローカルファイルなら実画像として描画する
    pub image: bool,
}

fn flush(out: &mut Vec<Span>, cur: &mut String, strong: bool, em: bool, strike: bool) {
    if cur.is_empty() {
        return;
    }
    out.push(Span {
        text: std::mem::take(cur),
        strong,
        em,
        strike,
        ..Default::default()
    });
}

/// `[text](url)` を chars[i] の `[` から読む。成功したら (text, url, 次位置)。
fn read_link(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    debug_assert_eq!(chars.get(i), Some(&'['));
    let close = (i + 1..chars.len()).find(|&k| chars[k] == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = (close + 2..chars.len()).find(|&k| chars[k] == ')')?;
    let text: String = chars[i + 1..close].iter().collect();
    let url: String = chars[close + 2..end].iter().collect();
    Some((text, url, end + 1))
}

/// 1行分のインライン構文をスパン列へ分解する。
pub fn parse_inline(s: &str) -> Vec<Span> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut strong, mut em, mut strike) = (false, false, false);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\\' if next.is_some() => {
                // エスケープ: 次の1文字をそのまま出す
                cur.push(next.unwrap());
                i += 2;
            }
            '`' => {
                // インラインコード: 次のバッククォートまで
                if let Some(close) = (i + 1..chars.len()).find(|&k| chars[k] == '`') {
                    flush(&mut out, &mut cur, strong, em, strike);
                    out.push(Span {
                        text: chars[i + 1..close].iter().collect(),
                        code: true,
                        ..Default::default()
                    });
                    i = close + 1;
                } else {
                    cur.push('`');
                    i += 1;
                }
            }
            '*' | '_' if next == Some(c) => {
                // ** / __ = 強調
                flush(&mut out, &mut cur, strong, em, strike);
                strong = !strong;
                i += 2;
            }
            '*' => {
                flush(&mut out, &mut cur, strong, em, strike);
                em = !em;
                i += 1;
            }
            '_' => {
                // snake_case を斜体にしないよう、単語境界でのみ効かせる
                let prev = if i == 0 { None } else { chars.get(i - 1).copied() };
                let boundary = if em {
                    next.is_none_or(|n| !n.is_alphanumeric())
                } else {
                    prev.is_none_or(|p| !p.is_alphanumeric())
                };
                if boundary {
                    flush(&mut out, &mut cur, strong, em, strike);
                    em = !em;
                } else {
                    cur.push('_');
                }
                i += 1;
            }
            '~' if next == Some('~') => {
                flush(&mut out, &mut cur, strong, em, strike);
                strike = !strike;
                i += 2;
            }
            '!' if next == Some('[') => match read_link(&chars, i + 1) {
                Some((alt, url, ni)) => {
                    flush(&mut out, &mut cur, strong, em, strike);
                    out.push(Span {
                        text: format!("🖼 {}", if alt.is_empty() { &url } else { &alt }),
                        link: Some(url.clone()),
                        image: true,
                        ..Default::default()
                    });
                    i = ni;
                }
                None => {
                    cur.push('!');
                    i += 1;
                }
            },
            '[' => match read_link(&chars, i) {
                Some((text, url, ni)) => {
                    flush(&mut out, &mut cur, strong, em, strike);
                    out.push(Span {
                        text,
                        link: Some(url),
                        ..Default::default()
                    });
                    i = ni;
                }
                None => {
                    cur.push('[');
                    i += 1;
                }
            },
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }
    flush(&mut out, &mut cur, strong, em, strike);
    out
}

// ─── ブロック構文の判定ヘルパ ───────────────────────────────────────

/// 水平線 (`---` / `***` / `___`、3文字以上、空白許容)。
pub fn is_hr(t: &str) -> bool {
    let t = t.trim();
    if t.len() < 3 {
        return false;
    }
    for mark in ['-', '*', '_'] {
        if t.chars().all(|c| c == mark || c == ' ')
            && t.chars().filter(|&c| c == mark).count() >= 3
            && !t.contains(|c: char| c != mark && c != ' ')
        {
            return true;
        }
    }
    false
}

/// テーブルの区切り行 (`|---|:--:|` 形式) か。
pub fn is_table_sep(t: &str) -> bool {
    let t = t.trim();
    t.starts_with('|')
        && t.contains('-')
        && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// `| a | b |` をセル列へ分解する。`\|` はエスケープされたパイプとして本文に残す。
pub fn split_row(t: &str) -> Vec<String> {
    let mut cells: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = t.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '|' => {
                cells.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    // 行頭・行末のパイプが作る空要素は1つずつだけ取り除く (途中の空セルは保持)
    if cells.first().is_some_and(|c| c.is_empty()) {
        cells.remove(0);
    }
    if cells.len() > 1 && cells.last().is_some_and(|c| c.is_empty()) {
        cells.pop();
    }
    cells
}

/// テーブル列の揃え。区切り行の `:--` / `:-:` / `--:` に対応する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAlign {
    Left,
    Center,
    Right,
}

/// 区切り行 `|:--|:-:|--:|` から列ごとの揃えを得る。
pub fn table_aligns(sep: &str) -> Vec<TableAlign> {
    split_row(sep)
        .iter()
        .map(|c| match (c.starts_with(':'), c.ends_with(':')) {
            (true, true) => TableAlign::Center,
            (false, true) => TableAlign::Right,
            _ => TableAlign::Left,
        })
        .collect()
}

/// リスト行なら (本文開始オフセット, 行頭記号) を返す。
pub fn list_marker(t: &str) -> Option<(usize, String)> {
    if t.starts_with("- [ ] ") {
        return Some((6, "☐".into()));
    }
    if t.starts_with("- [x] ") || t.starts_with("- [X] ") {
        return Some((6, "☑".into()));
    }
    for m in ["- ", "* ", "+ "] {
        if t.starts_with(m) {
            return Some((2, "•".into()));
        }
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..=3).contains(&digits) && t[digits..].starts_with(". ") {
        return Some((digits + 2, format!("{}.", &t[..digits])));
    }
    None
}

/// CJK 文字か (段落の行連結で空白を挟まない判定に使う)。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x30FF | 0x3400..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF)
}

/// 段落バッファへ1行を連結する。日本語同士なら空白を挟まない。
pub fn append_para(para: &mut String, line: &str) {
    if para.is_empty() {
        para.push_str(line);
        return;
    }
    let last = para.chars().last().unwrap_or(' ');
    let first = line.chars().next().unwrap_or(' ');
    if !(is_cjk(last) && is_cjk(first)) {
        para.push(' ');
    }
    para.push_str(line);
}

// ─── 画像 ───────────────────────────────────────────────────────────

/// プレビュー内で参照されたローカル画像のテクスチャキャッシュ。
/// mtime をキーに含めるため、外部でファイルが差し替わると自動で再読込される。
#[derive(Default)]
pub struct ImageCache {
    map: HashMap<String, (Option<std::time::SystemTime>, Option<egui::TextureHandle>)>,
}

impl ImageCache {
    fn get(&mut self, ctx: &egui::Context, path: &Path) -> Option<egui::TextureHandle> {
        let key = path.to_string_lossy().to_string();
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if let Some((cached, tex)) = self.map.get(&key) {
            if *cached == mtime {
                return tex.clone();
            }
        }
        let tex = load_image_texture(ctx, path);
        self.map.insert(key, (mtime, tex.clone()));
        tex
    }
}

/// 画像ファイルをテクスチャへ読み込む (長辺 1600px へ縮小)。
fn load_image_texture(ctx: &egui::Context, path: &Path) -> Option<egui::TextureHandle> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > 20 * 1024 * 1024 {
        return None;
    }
    let img = image::load_from_memory(&bytes).ok()?;
    let mut rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    const MAX: u32 = 1600;
    if w > MAX || h > MAX {
        let scale = MAX as f32 / w.max(h) as f32;
        let nw = ((w as f32 * scale) as u32).max(1);
        let nh = ((h as f32 * scale) as u32).max(1);
        rgba = image::imageops::resize(&rgba, nw, nh, image::imageops::FilterType::Triangle);
    }
    let (w, h) = rgba.dimensions();
    let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
    Some(ctx.load_texture(
        format!("zv-md-img:{}", path.display()),
        color,
        egui::TextureOptions::LINEAR,
    ))
}

/// 画像 URL をローカルパスへ解決する。http(s) や存在しないパスは None。
fn resolve_image(dir: Option<&Path>, url: &str) -> Option<PathBuf> {
    if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("data:") {
        return None;
    }
    let clean = url.split(['?', '#']).next().unwrap_or(url);
    if clean.is_empty() {
        return None;
    }
    let p = Path::new(clean);
    let full = if p.is_absolute() {
        p.to_path_buf()
    } else {
        dir?.join(p)
    };
    full.is_file().then_some(full)
}

// ─── 描画 ───────────────────────────────────────────────────────────

/// スパン描画に必要な文脈 (画像の基準ディレクトリとテクスチャキャッシュ)。
pub struct RenderCtx<'a> {
    /// 相対パス画像の基準 (通常はバッファのあるディレクトリ)
    pub dir: Option<&'a Path>,
    pub images: &'a mut ImageCache,
}

/// インラインスパン列をその場に描く (呼び出し側が wrap コンテナを用意する)。
fn spans_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    text: &str,
    size: f32,
    strong_all: bool,
    color: Color32,
    rctx: &mut RenderCtx,
) {
    for sp in parse_inline(text) {
        if let Some(url) = &sp.link {
            // ローカル画像は実際に描画する。それ以外 (リモート/欠損) はリンク表示
            if sp.image {
                if let Some(path) = resolve_image(rctx.dir, url) {
                    if let Some(tex) = rctx.images.get(ui.ctx(), &path) {
                        let avail = ui.available_width().max(60.0);
                        ui.add(
                            egui::Image::new(&tex)
                                .max_width(avail.min(tex.size_vec2().x)),
                        )
                        .on_hover_text(url);
                        continue;
                    }
                }
            }
            ui.hyperlink_to(RichText::new(&sp.text).size(size), url)
                .on_hover_text(url);
            continue;
        }
        let mut rt = if sp.code {
            RichText::new(&sp.text)
                .font(FontId::monospace(size * 0.92))
                .color(theme.accent)
                .background_color(theme.panel_alt)
        } else {
            RichText::new(&sp.text).size(size).color(color)
        };
        if sp.strong || strong_all {
            rt = rt.strong();
        }
        if sp.em {
            rt = rt.italics();
        }
        if sp.strike {
            rt = rt.strikethrough();
        }
        ui.label(rt);
    }
}

/// 1行を折り返しつきで描く。
fn line_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    text: &str,
    size: f32,
    strong_all: bool,
    color: Color32,
    rctx: &mut RenderCtx,
) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        spans_ui(ui, theme, text, size, strong_all, color, rctx);
    });
}

/// セル内容を折り返しなしで描いたときの幅を見積もる (中央/右揃えの余白計算用)。
/// spans_ui は item_spacing.x = 0 で描くためスパン幅の総和と一致する。
fn spans_width(ui: &egui::Ui, text: &str, size: f32) -> f32 {
    ui.fonts(|f| {
        parse_inline(text)
            .iter()
            .map(|sp| {
                let font = if sp.code {
                    FontId::monospace(size * 0.92)
                } else {
                    FontId::proportional(size)
                };
                f.layout_no_wrap(sp.text.clone(), font, Color32::WHITE).size().x
            })
            .sum()
    })
}

/// テーブルの1セルを揃え付きで描く。
/// 中央/右揃えの列はセル幅いっぱいを確保して余白で寄せる
/// (egui::Grid のセルは常に左詰めのため、揃えはセル内で自前で行う)。
fn table_cell_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    text: &str,
    size: f32,
    strong_all: bool,
    color: Color32,
    align: TableAlign,
    rctx: &mut RenderCtx,
) {
    if align == TableAlign::Left {
        line_ui(ui, theme, text, size, strong_all, color, rctx);
        return;
    }
    let w = ui.available_width();
    let pad = (w - spans_width(ui, text, size)).max(0.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.set_min_width(w);
        ui.add_space(if align == TableAlign::Right { pad } else { pad * 0.5 });
        spans_ui(ui, theme, text, size, strong_all, color, rctx);
    });
}

/// フェンスコードブロック (syntect でハイライト、横スクロール)。
fn code_block_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    hl: &Highlighter,
    base: f32,
    idx: usize,
    lang_tok: &str,
    code: &str,
) {
    let lang = hl.lang_for_fence(lang_tok);
    let job = hl.layout_job(
        code.trim_end_matches('\n'),
        &lang,
        &theme.syntect_theme,
        FontId::monospace(base * 0.92),
        theme.term_fg,
    );
    egui::Frame::none()
        .fill(theme.term_bg)
        .rounding(egui::Rounding::same(6.0))
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            if !lang_tok.is_empty() {
                ui.label(RichText::new(lang_tok).size(10.5).color(theme.text_dim));
            }
            egui::ScrollArea::horizontal()
                .id_salt(("md-code", idx))
                .show(ui, |ui| {
                    ui.label(job);
                });
        });
}

/// Markdown 全文を ui へ描画する。
/// `rctx` は画像解決用の文脈 (基準ディレクトリ + テクスチャキャッシュ)。
pub fn render(
    ui: &mut egui::Ui,
    theme: &Theme,
    hl: &Highlighter,
    base: f32,
    text: &str,
    rctx: &mut RenderCtx,
) {
    ui.spacing_mut().item_spacing.y = 6.0;
    let lines: Vec<&str> = text.lines().collect();
    let mut para = String::new();
    let flush_para =
        |ui: &mut egui::Ui, para: &mut String, theme: &Theme, rctx: &mut RenderCtx| {
            if !para.trim_end().is_empty() {
                line_ui(ui, theme, para.trim_end(), base, false, theme.text, rctx);
            }
            para.clear();
        };

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();

        // フェンスコード
        if let Some(rest) = trimmed.strip_prefix("```") {
            flush_para(ui, &mut para, theme, rctx);
            let lang_tok = rest.trim().to_string();
            let start = i + 1;
            let mut end = start;
            while end < lines.len() && !lines[end].trim_start().starts_with("```") {
                end += 1;
            }
            let code: String = lines[start..end]
                .iter()
                .flat_map(|l| [*l, "\n"])
                .collect();
            code_block_ui(ui, theme, hl, base, i, &lang_tok, &code);
            i = (end + 1).min(lines.len());
            continue;
        }

        // 見出し
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            flush_para(ui, &mut para, theme, rctx);
            let scale = [1.85f32, 1.5, 1.28, 1.12, 1.02, 0.95][hashes - 1];
            ui.add_space(if hashes <= 2 { 8.0 } else { 4.0 });
            line_ui(ui, theme, trimmed[hashes + 1..].trim(), base * scale, true, theme.text, rctx);
            if hashes <= 2 {
                ui.separator();
            }
            i += 1;
            continue;
        }

        // 水平線
        if is_hr(trimmed) {
            flush_para(ui, &mut para, theme, rctx);
            ui.separator();
            i += 1;
            continue;
        }

        // 引用 (連続する > 行をまとめる)
        if trimmed.starts_with('>') {
            flush_para(ui, &mut para, theme, rctx);
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                let body = lines[i].trim_start()[1..].trim_start();
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.label(RichText::new("▍ ").color(theme.accent).size(base));
                    spans_ui(ui, theme, body, base, false, theme.text_dim, rctx);
                });
                i += 1;
            }
            continue;
        }

        // テーブル
        if trimmed.starts_with('|')
            && lines.get(i + 1).map(|l| is_table_sep(l)).unwrap_or(false)
        {
            flush_para(ui, &mut para, theme, rctx);
            let header = split_row(trimmed);
            let aligns = table_aligns(lines[i + 1]);
            let ncols = header.len().max(1);
            let table_id = i;
            let mut r = i + 2;
            let mut rows: Vec<Vec<String>> = Vec::new();
            while r < lines.len() && lines[r].trim_start().starts_with('|') {
                let lt = lines[r].trim_start();
                // 迷い込んだ区切り行 (`|---|` 等) はセルとして描画しない
                if !is_table_sep(lt) {
                    let mut row = split_row(lt);
                    // GFM 準拠: ヘッダより多いセルは切り捨て、足りない分は空セルで埋める
                    row.resize(ncols, String::new());
                    rows.push(row);
                }
                r += 1;
            }
            // 列幅の上限は全列均等割り (最低 80px)。egui::Grid は上限が有限のときだけ
            // セル内折り返しが有効になる。収まらない分は横スクロールで逃がす。
            let cap = ((ui.available_width() - 34.0 - 16.0 * (ncols - 1) as f32)
                / ncols as f32)
                .max(80.0);
            let col_align = |c: usize| aligns.get(c).copied().unwrap_or(TableAlign::Left);
            egui::ScrollArea::horizontal()
                .id_salt(("md-table-scroll", table_id))
                .show(ui, |ui| {
                    egui::Frame::none()
                        .stroke(egui::Stroke::new(1.0_f32, theme.border))
                        .rounding(egui::Rounding::same(4.0))
                        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                        .show(ui, |ui| {
                            egui::Grid::new(("md-table", table_id))
                                .num_columns(ncols)
                                .max_col_width(cap)
                                .striped(true)
                                .spacing([16.0, 5.0])
                                .show(ui, |ui| {
                                    for (c, cell) in header.iter().enumerate() {
                                        table_cell_ui(
                                            ui, theme, cell, base, true, theme.text,
                                            col_align(c), rctx,
                                        );
                                    }
                                    ui.end_row();
                                    for row in &rows {
                                        for (c, cell) in row.iter().enumerate() {
                                            table_cell_ui(
                                                ui, theme, cell, base, false, theme.text,
                                                col_align(c), rctx,
                                            );
                                        }
                                        ui.end_row();
                                    }
                                });
                        });
                });
            i = r;
            continue;
        }

        // リスト
        if let Some((off, bullet)) = list_marker(trimmed) {
            flush_para(ui, &mut para, theme, rctx);
            let done = bullet == "☑";
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add_space(6.0 + indent as f32 * base * 0.55);
                let bcol = if done { theme.ok } else { theme.accent };
                ui.label(RichText::new(format!("{bullet} ")).color(bcol).size(base));
                let tcol = if done { theme.text_dim } else { theme.text };
                spans_ui(ui, theme, &trimmed[off..], base, false, tcol, rctx);
            });
            i += 1;
            continue;
        }

        // 空行 = 段落の区切り
        if trimmed.is_empty() {
            flush_para(ui, &mut para, theme, rctx);
            i += 1;
            continue;
        }

        // 通常テキスト → 段落として連結。
        // 行末スペース2つ (Markdown のハード改行、<br> 由来も同じ) は段落を確定する
        append_para(&mut para, trimmed);
        if line.ends_with("  ") {
            flush_para(ui, &mut para, theme, rctx);
        }
        i += 1;
    }
    flush_para(ui, &mut para, theme, rctx);
}

// ─── テスト ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_markdown_files() {
        assert!(is_markdown("README.md", "Plain Text"));
        assert!(is_markdown("Notes.MD", "Plain Text"));
        assert!(is_markdown("x.markdown", "Plain Text"));
        assert!(is_markdown("untitled-1", "Markdown"));
        assert!(!is_markdown("main.rs", "Rust"));
    }

    #[test]
    fn inline_bold_and_code() {
        let sp = parse_inline("a **b** `c`");
        assert_eq!(sp.len(), 4);
        assert_eq!(sp[0].text, "a ");
        assert!(sp[1].strong && sp[1].text == "b");
        assert_eq!(sp[2].text, " ");
        assert!(sp[3].code && sp[3].text == "c");
    }

    #[test]
    fn inline_link_and_image() {
        let sp = parse_inline("see [doc](https://a.b) ![alt](img.png)");
        assert_eq!(sp[1].link.as_deref(), Some("https://a.b"));
        assert_eq!(sp[1].text, "doc");
        assert_eq!(sp[3].link.as_deref(), Some("img.png"));
        assert!(sp[3].text.contains("alt"));
    }

    #[test]
    fn inline_snake_case_is_not_emphasis() {
        let sp = parse_inline("use snake_case_name here");
        assert_eq!(sp.len(), 1);
        assert!(sp[0].text.contains("snake_case_name"));
        assert!(!sp[0].em);
    }

    #[test]
    fn inline_escape_keeps_literal() {
        let sp = parse_inline(r"\*not em\*");
        assert_eq!(sp.len(), 1);
        assert_eq!(sp[0].text, "*not em*");
    }

    #[test]
    fn block_helpers() {
        assert!(is_hr("---"));
        assert!(is_hr("* * *"));
        assert!(!is_hr("--"));
        assert!(!is_hr("a---"));
        assert!(is_table_sep("| --- | :--: |"));
        assert!(!is_table_sep("| a | b |"));
        assert_eq!(split_row("| a | b |"), vec!["a", "b"]);
    }

    #[test]
    fn table_row_split_edge_cases() {
        // エスケープされたパイプはセルを割らず本文の `|` になる
        assert_eq!(split_row(r"| a \| b | c |"), vec!["a | b", "c"]);
        // 空セルは保持される (先頭・途中・末尾)
        assert_eq!(split_row("|| b |"), vec!["", "b"]);
        assert_eq!(split_row("| a || c |"), vec!["a", "", "c"]);
        assert_eq!(split_row("| a | |"), vec!["a", ""]);
        // 閉じパイプなしでも同じ結果
        assert_eq!(split_row("| a | b"), vec!["a", "b"]);
        assert_eq!(split_row("a | b"), vec!["a", "b"]);
    }

    #[test]
    fn table_alignment_parse() {
        use TableAlign::*;
        assert_eq!(table_aligns("|:--|:-:|--:|---|"), vec![Left, Center, Right, Left]);
        assert_eq!(table_aligns("| :--: | --- |"), vec![Center, Left]);
        assert_eq!(list_marker("- x"), Some((2, "•".into())));
        assert_eq!(list_marker("3. x"), Some((3, "3.".into())));
        assert_eq!(list_marker("- [x] done"), Some((6, "☑".into())));
        assert_eq!(list_marker("普通の行"), None);
    }

    #[test]
    fn paragraph_join_is_cjk_aware() {
        let mut p = String::new();
        append_para(&mut p, "hello");
        append_para(&mut p, "world");
        assert_eq!(p, "hello world");
        let mut q = String::new();
        append_para(&mut q, "こんにちは");
        append_para(&mut q, "世界");
        assert_eq!(q, "こんにちは世界");
    }

    #[test]
    fn resolve_image_remote_and_data_urls_are_none() {
        let dir = crate::test_util::unique_temp_dir("zaivern-markdown-test", "remote");
        // dir があってもリモート/データ URL はローカル解決しない
        assert_eq!(resolve_image(Some(&dir), "http://a.b/img.png"), None);
        assert_eq!(resolve_image(Some(&dir), "https://a.b/img.png"), None);
        assert_eq!(resolve_image(Some(&dir), "data:image/png;base64,AAAA"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_image_empty_url_is_none() {
        let dir = crate::test_util::unique_temp_dir("zaivern-markdown-test", "empty");
        assert_eq!(resolve_image(Some(&dir), ""), None);
        // クエリ/フラグメントだけの URL も除去後に空になり None
        assert_eq!(resolve_image(Some(&dir), "?q=1"), None);
        assert_eq!(resolve_image(Some(&dir), "#frag"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_image_strips_query_and_fragment() {
        let dir = crate::test_util::unique_temp_dir("zaivern-markdown-test", "query");
        let img = dir.join("img.png");
        std::fs::write(&img, b"png").expect("write test image");
        assert_eq!(resolve_image(Some(&dir), "img.png?v=1"), Some(img.clone()));
        assert_eq!(resolve_image(Some(&dir), "img.png#sec"), Some(img.clone()));
        assert_eq!(resolve_image(Some(&dir), "img.png?v=1#sec"), Some(img));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_image_absolute_path_ignores_dir() {
        let dir = crate::test_util::unique_temp_dir("zaivern-markdown-test", "abs");
        let img = dir.join("abs.png");
        std::fs::write(&img, b"png").expect("write test image");
        let url = img.to_str().expect("utf-8 temp path");
        // 絶対パスは dir と無関係に解決される (dir が別でも None でも同じ)
        let other = crate::test_util::unique_temp_dir("zaivern-markdown-test", "abs-other");
        assert_eq!(resolve_image(Some(&other), url), Some(img.clone()));
        assert_eq!(resolve_image(None, url), Some(img));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn resolve_image_relative_needs_dir_and_existing_file() {
        let dir = crate::test_util::unique_temp_dir("zaivern-markdown-test", "rel");
        let img = dir.join("rel.png");
        std::fs::write(&img, b"png").expect("write test image");
        // dir がなければ相対パスは解決できない
        assert_eq!(resolve_image(None, "rel.png"), None);
        // dir 起点で実在すれば Some、しなければ None
        assert_eq!(resolve_image(Some(&dir), "rel.png"), Some(img));
        assert_eq!(resolve_image(Some(&dir), "missing.png"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_image_directory_is_not_a_file() {
        let dir = crate::test_util::unique_temp_dir("zaivern-markdown-test", "dirpath");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("create sub dir");
        // is_file 判定なのでディレクトリは None
        assert_eq!(resolve_image(Some(&dir), "sub"), None);
        let abs = sub.to_str().expect("utf-8 temp path");
        assert_eq!(resolve_image(None, abs), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
