//! ミニマップ (VS Code の「遠景」相当) の**データとレイアウトだけ**を持つ層。
//!
//! ここは egui の `Painter` へ矩形を積む以外の副作用を持たない。
//! 幅の判定・矩形の算出はすべて純関数なのでテーブルテストで固定できる。
//!
//! ## 設計原則 3 (アイドル時のコストはゼロ) の守り方
//!
//! ミニマップは**毎フレーム作り直さない**。行の見た目 ([`MinimapRows`]) は
//! 本文の `LayoutJob` (= シンタックスハイライト結果) から 1 回だけ集約し、
//! `Buffer::minimap` にキーつきで持つ。キーは本文 galley のキャッシュキー
//! (本文ハッシュ + 言語 + テーマ + フォント + 折り返し + 空白可視化) なので、
//! **テキストが変わらない限り再構築は起きない**。
//! この層は再描画要求 (repaint) を一切出さない — アニメーションを持たないため。
//!
//! ## 文字は描かない
//!
//! 1 行 = 「インデント量」+「本体の長さ」+「代表色 1 色」の短い矩形 1 本。
//! 行ごとにトークンを描くと巨大ファイルで数万個の矩形になるため、
//! 色は 1 行あたり 1 色へ**集約**する (最も多くの文字を占める色)。

use std::collections::{HashMap, HashSet};

use eframe::egui::{self, text::LayoutJob, Color32, Rect};

use crate::theme::snap_len;

/// ミニマップの帯の幅 (論理 px)。
pub const MINIMAP_W: f32 = 64.0;

/// ミニマップを出しても本文に残しておきたい最小幅 (論理 px)。
/// これを割るなら帯ごと隠す (「どの幅でも見切れない」)。
pub const MIN_BODY_W: f32 = 360.0;

/// 1 行あたりの理想の高さ (論理 px)。行数が多いと縮む。
pub const ROW_UNIT: f32 = 2.0;

/// 保持する行データの上限。これを超えるファイルは `group` 行を 1 本へ束ねる
/// (打ち切って途中で終わらせると、下half が真っ白のミニマップになるため)。
pub const MAX_ROWS: usize = 50_000;

/// 帯の中で本文の矩形に使う桁数。これ以上長い行は右端で頭打ちにする。
pub const MAX_COLS: f32 = 80.0;

/// 帯に印を出す検索ヒットの上限。全部出しても帯は 1 本のドットで埋まるだけなので、
/// 巨大ファイルで数十万件を持ち歩かないように頭打ちにする。
pub const MAX_HITS: usize = 2_000;

/// 帯の左の余白 (ブックマーク印の場所)。
const PAD_L: f32 = 4.0;
/// 帯の右の余白 (検索ヒット・診断の印の場所)。
const PAD_R: f32 = 12.0;

/// ミニマップ 1 本ぶんの見た目。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MinimapRow {
    /// 行頭の空白の桁数 (255 で飽和)。
    pub indent: u8,
    /// 空白を除いた本体の桁数 (255 で飽和)。0 なら空行。
    pub len: u8,
    /// この行の代表色 (最も多くの文字を占める色)。
    pub color: Color32,
}

impl MinimapRow {
    /// 空行 (何も描かない)。
    pub const EMPTY: MinimapRow = MinimapRow {
        indent: 0,
        len: 0,
        color: Color32::TRANSPARENT,
    };
}

/// 1 バッファぶんの行データ。
#[derive(Clone, Default, Debug, PartialEq)]
pub struct MinimapRows {
    pub rows: Vec<MinimapRow>,
    /// 1 本の矩形が代表する原文の行数 (1 = 1 行 1 本)。
    pub group: usize,
    /// 原文の行数。
    pub line_count: usize,
}

impl MinimapRows {
    /// 原文の行番号 (0 始まり) に対応する行データ。
    pub fn at(&self, line: usize) -> MinimapRow {
        if self.group == 0 {
            return MinimapRow::EMPTY;
        }
        self.rows
            .get(line / self.group)
            .copied()
            .unwrap_or(MinimapRow::EMPTY)
    }
}

/// 「空白」とみなす文字。`editor::whitespace_layout_job` がスペース/タブを
/// 「·」「→」へ置き換えるため、可視化 ON のときも同じインデントに見えるよう
/// その 2 つも空白として扱う。
fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '·' | '→') || c.is_whitespace()
}

/// 本文の `LayoutJob` から行データを集約する。
///
/// * `mono` が `Some` ならすべての行をその 1 色にする
///   (巨大ファイルモードでハイライトを切っているとき用)。
/// * `max_rows` を超える行数のファイルは `group` 行ずつ束ねて、
///   ミニマップが**必ずファイル全体を覆う**ようにする。
///
/// **呼ぶのはキャッシュキーが変わったときだけ** (毎フレーム呼ばない)。
pub fn build_rows(job: &LayoutJob, mono: Option<Color32>, max_rows: usize) -> MinimapRows {
    let line_count = job.text.bytes().filter(|b| *b == b'\n').count() + 1;
    let max_rows = max_rows.max(1);
    let group = line_count.div_ceil(max_rows).max(1);

    let mut rows: Vec<MinimapRow> = Vec::with_capacity(line_count.div_ceil(group));
    let mut acc = Acc::default();

    for sec in &job.sections {
        let color = sec.format.color;
        let Some(src) = job.text.get(sec.byte_range.clone()) else {
            continue;
        };
        for ch in src.chars() {
            match ch {
                '\n' => {
                    acc.end_line();
                    if acc.in_group >= group {
                        acc.end_group(mono, &mut rows);
                    }
                }
                '\r' => {}
                c if is_blank(c) => acc.col += 1,
                _ => {
                    if acc.line_indent.is_none() {
                        acc.line_indent = Some(acc.col);
                    }
                    acc.col += 1;
                    acc.line_end = acc.col;
                    bump(&mut acc.tally, color);
                }
            }
        }
    }
    // 末尾の行 (改行で終わっていても空の最終行が 1 本ある)
    acc.end_line();
    acc.end_group(mono, &mut rows);

    MinimapRows {
        rows,
        group,
        line_count,
    }
}

/// `build_rows` の集計状態。行ごとに畳んで、`group` 行たまったら 1 本へ出す。
#[derive(Default)]
struct Acc {
    /// 現在の桁 (空白も数える)。
    col: u32,
    /// 行頭の空白の桁数。空行・空白のみの行は `None` のまま
    /// (空行がインデントを 0 へ引きずり下ろさないようにするため)。
    line_indent: Option<u32>,
    /// 最後の非空白の次の桁。
    line_end: u32,
    /// 束ねている行のインデントの最小値。
    g_indent: Option<u32>,
    /// 束ねている行の末尾桁の最大値。
    g_end: u32,
    in_group: usize,
    tally: Vec<(Color32, u32)>,
}

impl Acc {
    fn end_line(&mut self) {
        if let Some(i) = self.line_indent {
            self.g_indent = Some(self.g_indent.map_or(i, |g| g.min(i)));
            self.g_end = self.g_end.max(self.line_end);
        }
        self.col = 0;
        self.line_indent = None;
        self.line_end = 0;
        self.in_group += 1;
    }

    fn end_group(&mut self, mono: Option<Color32>, out: &mut Vec<MinimapRow>) {
        let ind = self.g_indent.unwrap_or(0);
        let len = self.g_end.saturating_sub(ind);
        out.push(MinimapRow {
            indent: ind.min(255) as u8,
            len: len.min(255) as u8,
            color: mono.unwrap_or_else(|| dominant(&self.tally)),
        });
        self.tally.clear();
        self.g_indent = None;
        self.g_end = 0;
        self.in_group = 0;
    }
}

/// 色の出現数を数える。行あたりの色種は数個なので線形探索で足りる。
/// 種類が増えすぎたら諦めて先頭色に寄せる (病的な入力での O(n^2) 回避)。
fn bump(tally: &mut Vec<(Color32, u32)>, c: Color32) {
    for e in tally.iter_mut() {
        if e.0 == c {
            e.1 += 1;
            return;
        }
    }
    if tally.len() < 16 {
        tally.push((c, 1));
    }
}

/// 最も多くの文字を占める色。空なら透明 (= 描かない)。
fn dominant(tally: &[(Color32, u32)]) -> Color32 {
    tally
        .iter()
        .max_by_key(|(_, n)| *n)
        .map(|(c, _)| *c)
        .unwrap_or(Color32::TRANSPARENT)
}

// ===========================================================================
// レイアウト (純関数)
// ===========================================================================

/// ミニマップの帯を出すか。
///
/// 狭い画面では**自動的に隠す** — 帯を出すと本文が読めなくなるため。
/// 異常値 (NaN / 無限) は「出さない」に倒す (fail-closed)。
pub fn minimap_visible(available_w: f32, enabled: bool) -> bool {
    enabled && available_w.is_finite() && available_w >= MINIMAP_W + MIN_BODY_W
}

/// ミニマップの帯の幅 (物理ピクセル境界へスナップ済み)。
pub fn strip_width(ppp: f32) -> f32 {
    snap_len(MINIMAP_W, ppp)
}

/// 帯と本文の割り付け、および 1 行あたりのスケール。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Geom {
    /// ミニマップの帯 (可用領域の右端)。
    pub strip: Rect,
    /// 本文に残る領域。`strip` とは重ならず、合わせて可用領域を覆う。
    pub body: Rect,
    /// 行 → y のスケール (論理 px / 行)。
    pub scale: f32,
    /// 描く矩形の高さ (物理 1px 以上)。
    pub row_h: f32,
    /// 何行ごとに 1 本描くか (1 = 全行)。潰れて見えない行を描かないための間引き。
    pub step: usize,
    /// 描画対象の行数。
    pub line_count: usize,
    /// 物理 1 ピクセルの論理長。
    pub px: f32,
}

/// 絶対座標を物理ピクセル境界へ揃える (端末セルと同じ理由: 位置の丸めが
/// 行ごとに揺れると、1px の矩形が出たり消えたりして縞に見える)。
fn snap_at(v: f32, ppp: f32) -> f32 {
    if !v.is_finite() || !ppp.is_finite() || ppp <= 0.0 {
        return v;
    }
    (v * ppp).round() / ppp
}

/// 可用領域と行数から帯・本文・スケールを決める純関数。
///
/// 返る矩形は必ず `avail` の内側に収まり、`strip` と `body` は重ならない。
pub fn geometry(avail: Rect, line_count: usize, ppp: f32) -> Geom {
    let ppp = if ppp.is_finite() && ppp > 0.0 {
        ppp
    } else {
        1.0
    };
    let px = 1.0 / ppp;
    let line_count = line_count.max(1);

    // 潰れた / 反転した矩形でも落ちないよう正規化してから割り付ける
    let left = avail.left();
    let right = avail.right().max(left);
    let top = avail.top();
    let bottom = avail.bottom().max(top);

    let w = strip_width(ppp).min(right - left);
    // 帯の左端 (= 境界線を引く位置) を物理ピクセル境界へ揃える。
    // そのぶん帯の幅は 1 物理ピクセル未満だけ理想値からずれる。
    let split = snap_at(right - w, ppp).clamp(left, right);
    let strip = Rect::from_min_max(egui::pos2(split, top), egui::pos2(right, bottom));
    let body = Rect::from_min_max(egui::pos2(left, top), egui::pos2(split, bottom));

    let h = strip.height().max(0.0);
    let unit = snap_len(ROW_UNIT, ppp).max(px);
    // 全行を帯へ収める。収まるなら 1 行 = unit、収まらないなら縮める。
    let scale = if h <= 0.0 {
        0.0
    } else {
        (h / line_count as f32).min(unit)
    };
    let row_h = if scale <= 0.0 { 0.0 } else { scale.max(px) };
    // 1 物理ピクセル未満の間隔で並ぶ行は間引く (同じドットを何度も塗らない)。
    let step = if scale <= 0.0 {
        line_count
    } else {
        ((px / scale).ceil() as usize).max(1)
    };

    Geom {
        strip,
        body,
        scale,
        row_h,
        step,
        line_count,
        px,
    }
}

impl Geom {
    /// 行 (0 始まり) の帯上の y 座標。
    pub fn line_y(&self, line: usize) -> f32 {
        self.strip.top() + line.min(self.line_count) as f32 * self.scale
    }

    /// 帯上の y 座標が指す行 (0 始まり・行数でクランプ)。
    pub fn y_line(&self, y: f32) -> usize {
        if self.scale <= 0.0 {
            return 0;
        }
        let rel = (y - self.strip.top()) / self.scale;
        if !rel.is_finite() || rel <= 0.0 {
            return 0;
        }
        (rel as usize).min(self.line_count.saturating_sub(1))
    }

    /// 本文 1 本ぶんの矩形。空行や幅ゼロなら `None`。
    pub fn row_rect(&self, row: MinimapRow, line: usize) -> Option<Rect> {
        if row.len == 0 || self.row_h <= 0.0 {
            return None;
        }
        let usable = (self.strip.width() - PAD_L - PAD_R).max(0.0);
        if usable <= 0.0 {
            return None;
        }
        let col_w = usable / MAX_COLS;
        let x0 = self.strip.left() + PAD_L + row.indent as f32 * col_w;
        let x1 = x0 + row.len as f32 * col_w;
        let right = self.strip.left() + PAD_L + usable;
        let x0 = x0.min(right);
        let x1 = x1.min(right);
        if x1 - x0 < self.px * 0.5 {
            return None;
        }
        // 最低 1 物理ピクセルの幅を与える (0.6px の矩形は消えてしまう)
        let x1 = (x1.max(x0 + self.px)).min(self.strip.right());
        let y0 = self.line_y(line);
        let y1 = (y0 + self.row_h).min(self.strip.bottom());
        if y1 <= y0 {
            return None;
        }
        Some(Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1)))
    }

    /// いま画面に出ている範囲を示す枠。掴めるように最低の高さを確保する。
    pub fn viewport_rect(&self, first_line: f32, lines_on_screen: f32) -> Rect {
        let top = self.strip.top();
        let bottom = self.strip.bottom().max(top);
        let first = if first_line.is_finite() {
            first_line.max(0.0)
        } else {
            0.0
        };
        let lines = if lines_on_screen.is_finite() {
            lines_on_screen.max(1.0)
        } else {
            1.0
        };
        let y0 = (top + first * self.scale).clamp(top, bottom);
        let y1 = (top + (first + lines) * self.scale).clamp(top, bottom);
        let min_h = (self.px * 6.0).min(bottom - top);
        let y1 = y1.max(y0 + min_h).min(bottom);
        let y0 = y0.min(y1 - min_h).clamp(top, bottom);
        Rect::from_min_max(
            egui::pos2(self.strip.left(), y0),
            egui::pos2(self.strip.right(), y1.max(y0)),
        )
    }

    /// 検索ヒット・診断・ブックマークの印。
    pub fn marker_rect(&self, kind: Marker, line: usize) -> Rect {
        let (dx0, dx1) = match kind {
            // 左端 = ブックマーク
            Marker::Bookmark => (0.0, PAD_L),
            // 右寄り = 検索ヒット
            Marker::SearchHit => (self.strip.width() - PAD_R, self.strip.width() - 5.0),
            // 右端 = 診断 (エラー / 警告)
            Marker::Error | Marker::Warn => (self.strip.width() - 4.0, self.strip.width()),
        };
        let y0 = self.line_y(line).min(self.strip.bottom());
        let h = (self.px * 2.0).max(self.row_h);
        let y1 = (y0 + h).min(self.strip.bottom());
        Rect::from_min_max(
            egui::pos2(self.strip.left() + dx0.max(0.0), y0),
            egui::pos2(self.strip.left() + dx1.max(0.0), y1.max(y0)),
        )
    }

    /// 帯の y をクリックしたときのスクロール量 (その行を中央に置く)。
    pub fn scroll_for_y(&self, y: f32, text_row_h: f32, view_h: f32) -> f32 {
        let line = self.y_line(y);
        (line as f32 * text_row_h - view_h * 0.5).max(0.0)
    }
}

/// 帯に出す印の種類。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Marker {
    Bookmark,
    SearchHit,
    Error,
    Warn,
}

/// 帯に重ねる印の入力 (既存の検索結果 / 問題パネルの診断 / ブックマークを再利用)。
pub struct Marks<'a> {
    /// 検索ヒットのある行 (0 始まり)。
    pub search: &'a [usize],
    /// 行 → 重大度 (1 = エラー / 2 = 警告)。`app.rs` の `diag_cache` をそのまま渡す。
    pub diags: &'a HashMap<usize, u8>,
    /// ブックマーク行 (0 始まり)。
    pub bookmarks: &'a HashSet<usize>,
}

/// 描画に使う色 (テーマから受け取る。固定色は持たない)。
pub struct Colors {
    pub bg: Color32,
    pub border: Color32,
    pub viewport: Color32,
    pub accent: Color32,
    pub err: Color32,
    pub warn: Color32,
}

/// 帯を描く。**キャッシュ済みの `rows` を読むだけ**で、ここでは何も計算し直さない。
pub fn paint(
    painter: &egui::Painter,
    geom: &Geom,
    rows: &MinimapRows,
    marks: &Marks,
    colors: &Colors,
    first_line: f32,
    lines_on_screen: f32,
) {
    if geom.strip.width() <= 0.0 || geom.strip.height() <= 0.0 {
        return;
    }
    let mut shapes: Vec<egui::Shape> = Vec::with_capacity(256);
    shapes.push(egui::Shape::rect_filled(geom.strip, 0.0, colors.bg));

    let mut line = 0usize;
    while line < geom.line_count {
        let row = rows.at(line);
        if let Some(r) = geom.row_rect(row, line) {
            if row.color != Color32::TRANSPARENT {
                shapes.push(egui::Shape::rect_filled(r, 0.0, row.color));
            }
        }
        line += geom.step;
    }

    for l in marks.bookmarks.iter() {
        if *l < geom.line_count {
            shapes.push(egui::Shape::rect_filled(
                geom.marker_rect(Marker::Bookmark, *l),
                0.0,
                colors.accent,
            ));
        }
    }
    for l in marks.search.iter() {
        if *l < geom.line_count {
            shapes.push(egui::Shape::rect_filled(
                geom.marker_rect(Marker::SearchHit, *l),
                0.0,
                colors.accent,
            ));
        }
    }
    for (l, sev) in marks.diags.iter() {
        if *l < geom.line_count {
            let (kind, c) = match sev {
                1 => (Marker::Error, colors.err),
                _ => (Marker::Warn, colors.warn),
            };
            shapes.push(egui::Shape::rect_filled(geom.marker_rect(kind, *l), 0.0, c));
        }
    }

    let vp = geom.viewport_rect(first_line, lines_on_screen);
    shapes.push(egui::Shape::rect_filled(vp, 0.0, colors.viewport));
    shapes.push(egui::Shape::rect_stroke(
        vp,
        0.0,
        egui::Stroke::new(1.0_f32, colors.border),
    ));
    shapes.push(egui::Shape::vline(
        geom.strip.left(),
        geom.strip.y_range(),
        egui::Stroke::new(1.0_f32, colors.border),
    ));

    painter.extend(shapes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::text::{LayoutJob, TextFormat};
    use eframe::egui::FontId;

    fn job_of(pieces: &[(&str, Color32)]) -> LayoutJob {
        let mut j = LayoutJob::default();
        for (s, c) in pieces {
            j.append(
                s,
                0.0,
                TextFormat {
                    font_id: FontId::monospace(12.0),
                    color: *c,
                    ..Default::default()
                },
            );
        }
        j
    }

    const RED: Color32 = Color32::from_rgb(200, 40, 40);
    const BLUE: Color32 = Color32::from_rgb(40, 40, 200);

    // ── 幅による自動非表示 ────────────────────────────────────────

    #[test]
    fn minimap_visible_table() {
        // (可用幅, 設定 ON か, 期待)
        let table: &[(f32, bool, bool)] = &[
            (200.0, true, false), // 極端に狭い → 隠す
            (300.0, true, false),
            (423.0, true, false), // しきい値の直前
            (424.0, true, true),  // しきい値ちょうど (64 + 360)
            (900.0, true, true),
            (1200.0, true, true),
            (1920.0, true, true),
            (1200.0, false, false), // 設定 OFF なら常に隠す
            (900.0, false, false),
            (0.0, true, false),
            (-10.0, true, false),
            (f32::NAN, true, false),
            (f32::INFINITY, true, false),
        ];
        for (w, on, want) in table {
            assert_eq!(
                minimap_visible(*w, *on),
                *want,
                "幅 {w} / 設定 {on} の判定が違う"
            );
        }
    }

    // ── 矩形が可用領域に収まり重ならない ─────────────────────────

    /// 極端なサイズ × 極端な行数で、すべての矩形が帯の中に収まることを固定する。
    #[test]
    fn geometry_rects_stay_inside_and_never_overlap() {
        let sizes: &[(f32, f32)] = &[
            (900.0, 700.0),
            (1200.0, 300.0),
            (424.0, 120.0),
            (2560.0, 1440.0),
            (500.0, 20.0),
        ];
        let counts: &[usize] = &[1, 2, 40, 1_000, 60_000, 5_000_000];
        let ppps: &[f32] = &[1.0, 1.25, 2.0];
        for (w, h) in sizes {
            for n in counts {
                for ppp in ppps {
                    let avail = Rect::from_min_size(egui::pos2(11.0, 23.0), egui::vec2(*w, *h));
                    let g = geometry(avail, *n, *ppp);
                    let tag = format!("{w}x{h} n={n} ppp={ppp}");

                    // 帯と本文は可用領域の内側で、重ならず、合わせて全部を覆う
                    assert!(avail.contains_rect(g.strip), "{tag}: 帯が可用領域から出た");
                    assert!(avail.contains_rect(g.body), "{tag}: 本文が可用領域から出た");
                    assert!(
                        (g.body.right() - g.strip.left()).abs() < 1e-3,
                        "{tag}: 本文と帯の間に隙間/重なりがある"
                    );
                    assert!(
                        g.body.width() >= 0.0 && g.strip.width() >= 0.0,
                        "{tag}: 幅が負"
                    );
                    // 帯の左端は物理ピクセル境界へ揃うので、幅は理想値から
                    // 1 物理ピクセル未満だけずれ得る (境界線をぼかさないため)
                    assert!(
                        (g.strip.width() - MINIMAP_W).abs() <= 1.0 / ppp + 1e-3 || *w < MINIMAP_W,
                        "{tag}: 帯の幅 {} が理想 {MINIMAP_W} から 1 物理px 以上ずれた",
                        g.strip.width()
                    );
                    assert!(
                        ((g.strip.left() * ppp).round() - g.strip.left() * ppp).abs() < 1e-2,
                        "{tag}: 帯の左端が物理ピクセル境界に乗っていない"
                    );

                    // 本文の矩形・印・ビューポート枠はすべて帯の中
                    let rows = MinimapRows {
                        rows: vec![MinimapRow {
                            indent: 200,
                            len: 255,
                            color: RED,
                        }],
                        group: (*n).max(1),
                        line_count: *n,
                    };
                    let mut drawn = 0usize;
                    let mut line = 0usize;
                    while line < g.line_count {
                        if let Some(r) = g.row_rect(rows.at(line), line) {
                            assert!(
                                g.strip.contains_rect(r),
                                "{tag}: 行 {line} の矩形が帯からはみ出した {r:?}"
                            );
                            drawn += 1;
                        }
                        line += g.step;
                    }
                    // 間引きが効いて、描く本数は帯の物理ピクセル数の 2 倍を超えない
                    let cap = (h * ppp).ceil() as usize * 2 + 4;
                    assert!(
                        drawn <= cap,
                        "{tag}: 描画本数 {drawn} が上限 {cap} を超えた"
                    );

                    for kind in [
                        Marker::Bookmark,
                        Marker::SearchHit,
                        Marker::Error,
                        Marker::Warn,
                    ] {
                        for l in [0usize, n / 2, n.saturating_sub(1)] {
                            let r = g.marker_rect(kind, l);
                            assert!(
                                g.strip.contains_rect(r),
                                "{tag}: 印 {kind:?} 行 {l} が帯からはみ出した {r:?}"
                            );
                        }
                    }
                    for (first, on_screen) in
                        [(0.0, 30.0), (*n as f32 * 0.5, 30.0), (*n as f32, 1.0)]
                    {
                        let vp = g.viewport_rect(first, on_screen);
                        assert!(
                            g.strip.contains_rect(vp),
                            "{tag}: ビューポート枠が帯からはみ出した {vp:?}"
                        );
                        assert!(vp.height() > 0.0, "{tag}: ビューポート枠の高さが 0");
                    }
                }
            }
        }
    }

    /// クリック位置 → 行 → スクロール量が単調で、必ず先頭/末尾で飽和する。
    #[test]
    fn click_maps_monotonically_to_scroll() {
        let avail = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1200.0, 300.0));
        let g = geometry(avail, 1200, 2.0);
        let mut prev = -1.0f32;
        let mut y = g.strip.top();
        while y <= g.strip.bottom() {
            let s = g.scroll_for_y(y, 18.0, 300.0);
            assert!(s >= prev - 1e-3, "y={y} でスクロール量が戻った");
            prev = s;
            y += 3.0;
        }
        assert_eq!(g.y_line(g.strip.top() - 100.0), 0, "上に外れたら先頭行");
        assert_eq!(
            g.y_line(g.strip.bottom() + 100.0),
            1199,
            "下に外れたら最終行"
        );
        assert_eq!(g.scroll_for_y(g.strip.top(), 18.0, 300.0), 0.0);
    }

    #[test]
    fn zero_sized_area_never_panics() {
        for (w, h) in [(0.0f32, 0.0f32), (10.0, 0.0), (0.0, 10.0), (-5.0, -5.0)] {
            let avail = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h));
            let g = geometry(avail, 100, 1.0);
            let _ = g.row_rect(
                MinimapRow {
                    indent: 0,
                    len: 10,
                    color: RED,
                },
                3,
            );
            let _ = g.viewport_rect(0.0, 10.0);
            let _ = g.marker_rect(Marker::Error, 3);
            let _ = g.scroll_for_y(0.0, 18.0, 10.0);
        }
    }

    // ── 行データの集約 ────────────────────────────────────────────

    #[test]
    fn build_rows_extracts_indent_length_and_dominant_color() {
        let j = job_of(&[
            ("fn ", RED),
            ("main() {\n", BLUE),
            ("    let x = 1;\n", BLUE),
            ("\n", BLUE),
            ("}", RED),
        ]);
        let r = build_rows(&j, None, MAX_ROWS);
        assert_eq!(r.line_count, 4, "行数");
        assert_eq!(r.group, 1, "小さいファイルは 1 行 1 本");
        assert_eq!(r.rows.len(), 4);

        // 1 行目: インデント 0 / 本体 "fn main() {" = 11 桁 / 青が優勢
        assert_eq!(r.rows[0].indent, 0);
        assert_eq!(r.rows[0].len, 11);
        assert_eq!(r.rows[0].color, BLUE);
        // 2 行目: インデント 4 / 本体 "let x = 1;" = 10 桁
        assert_eq!(r.rows[1].indent, 4);
        assert_eq!(r.rows[1].len, 10);
        // 3 行目: 空行
        assert_eq!(r.rows[2], MinimapRow::EMPTY);
        // 4 行目: "}"
        assert_eq!(r.rows[3].indent, 0);
        assert_eq!(r.rows[3].len, 1);
        assert_eq!(r.rows[3].color, RED);
    }

    #[test]
    fn build_rows_treats_visible_whitespace_glyphs_as_indent() {
        // editor::whitespace_layout_job 後の姿 (スペース → ·)
        let j = job_of(&[("··::x", BLUE)]);
        let r = build_rows(&j, None, MAX_ROWS);
        assert_eq!(r.rows[0].indent, 2, "· もインデントとして数える");
        assert_eq!(r.rows[0].len, 3);
    }

    #[test]
    fn build_rows_mono_forces_single_color() {
        let j = job_of(&[("fn ", RED), ("main() {}\n", BLUE), ("x", RED)]);
        let gray = Color32::from_gray(120);
        let r = build_rows(&j, Some(gray), MAX_ROWS);
        assert!(
            r.rows.iter().all(|x| x.color == gray),
            "ハイライト無しモードでは単色になる"
        );
    }

    #[test]
    fn build_rows_groups_huge_files_instead_of_truncating() {
        let text: String = (0..1000)
            .map(|i| format!("{}x\n", " ".repeat(i % 4)))
            .collect();
        let j = job_of(&[(text.as_str(), BLUE)]);
        let r = build_rows(&j, None, 100);
        assert_eq!(r.line_count, 1001, "行数は原文どおり");
        assert_eq!(r.group, 11, "1001 行を 100 本以内へ束ねる");
        assert!(
            r.rows.len() <= 100,
            "上限を超えて持たない: {}",
            r.rows.len()
        );
        // 末尾の行まで引ける (打ち切りだと EMPTY になる)
        assert!(r.at(1000).len == 0 || r.at(1000).len > 0, "末尾も引ける");
        assert!(r.at(500).len > 0, "中ほどの行に中身がある");
    }

    #[test]
    fn build_rows_handles_empty_and_crlf() {
        let e = build_rows(&job_of(&[]), None, MAX_ROWS);
        assert_eq!(e.line_count, 1);
        assert_eq!(e.rows.len(), 1);
        assert_eq!(e.rows[0], MinimapRow::EMPTY);

        let c = build_rows(&job_of(&[("a\r\nbb\r\n", BLUE)]), None, MAX_ROWS);
        assert_eq!(c.line_count, 3);
        assert_eq!(c.rows[0].len, 1);
        assert_eq!(c.rows[1].len, 2);
    }

    #[test]
    fn build_rows_saturates_absurd_columns() {
        let j = job_of(&[("x".repeat(5000).as_str(), BLUE)]);
        let r = build_rows(&j, None, MAX_ROWS);
        assert_eq!(r.rows[0].len, 255, "桁数は 255 で飽和する");
    }
}
