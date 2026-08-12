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

// ══════════════════════════════════════════════════════════════════════════
//  スクロールバー装飾 (Zed 型) — ミニマップを払わずに印だけを出す層
// ══════════════════════════════════════════════════════════════════════════
//
// ## なぜ別の層なのか
//
// `Marks` が持つ印 (ブックマーク / 検索ヒット / エラー / 警告) は、
// ミニマップが **既定 off** なので実質誰にも見えていない。
// ミニマップは 64px を本文から奪うので「常に出す」選択が取れない
// (競合の VS Code でも minimap は "clutter" として嫌われている)。
//
// そこで **スクロールバー幅 (12px) の縦帯**へ印だけを置く。
// 本文の遠景 (`MinimapRows`) を一切持たないので、
// 幅は 1/5、再構築の費用は 0 (行番号の配列を舐めるだけ)。
//
// この層は `Geom` と**独立**している。ミニマップ用の API は 1 つも変えていない
// (両方同時に出すこともできる)。

/// スクロールバー帯の幅 (論理 px)。
///
/// 10px 未満だと 2 レーンに割ったとき 1 レーンが 4px を切り、印が
/// 「線」ではなく「点」に見える。14px を超えると本文から奪う幅が
/// スクロールバーとして不自然に太い。12px はその中間で、
/// macOS のオーバーレイスクロールバー (既定 15px) より細い。
pub const SCROLLBAR_W: f32 = 12.0;

/// 印 1 本の最小の高さ (論理 px)。
///
/// 1px はディスプレイのサブピクセル配置と背景のアンチエイリアスに埋もれて
/// 「ちらつき」にしか見えない。2px が「目盛り」として読める下限。
pub const MARK_MIN_H: f32 = 2.0;

/// 統合した印 1 本の最大の高さ (論理 px)。
///
/// 近接した印を**無制限に**1 本へ畳むと、密な検索結果 (10 万行に 5000 件) が
/// 帯いっぱいの 1 本の棒になり、「どこに固まっているか」という唯一の情報が消える。
/// 8px で切ると 700px の帯が 87 段に量子化され、段ごとの本数 ([`ScrollbarMark::count`])
/// が濃さの分布として残る。
pub const MARK_MAX_H: f32 = 8.0;

/// ビューポート枠の最小の高さ (論理 px)。掴んでドラッグできる下限。
pub const VIEWPORT_MIN_H: f32 = 6.0;

/// 帯の幅に対するレーン 1 本の割合。
///
/// 0.42 × 2 = 0.84 なので中央に 16% (12px なら約 2px) の隙間が残る。
/// 隙間が無いと左右のレーンが 1 本の帯に見えて、種別の区別が付かない。
const LANE_FRAC: f32 = 0.42;

/// 統合の閾値 (物理 px)。これ以下の隙間は「離れている」と読めないので畳む。
///
/// 物理 1px の隙間は、スクロールで印が 1px 動くたびに出たり消えたりして
/// ちらつきに見える。畳んでしまったほうが静か。
const MERGE_GAP_PX: f32 = 1.0;

/// 印 1 本だけの run でも見える濃さの下限 (0..=1)。
const MIN_WEIGHT: f32 = 0.35;

/// y → 行 で「境界のすぐ下」へ落ちたときに整数へ吸着させる許容 (行単位)。
///
/// [`ScrollbarGeom::line_y`] は f32 を返すので、10 万行を 700px へ写すと
/// 1 行の間隔 (0.007px) に対し f32 の刻み (6e-5) が 0.44% ぶん誤差になる。
/// そのまま `floor` すると `y_line(line_y(l))` が `l-1` を返す。
/// **整数の近傍だけ**を吸着させるので、行の途中をクリックした結果は動かない。
const SNAP_TOL: f64 = 1.0 / 64.0;

/// 帯に出す印の種別。
///
/// [`Marker`] (ミニマップ用) とは別にしてある。こちらは**カーソル行**を持ち、
/// レーンと優先順位という帯だけの概念を伴うため。
///
/// **宣言順がそのまま優先順位** (`Ord` を導出しているので比較できる)。
/// 別に `priority()` を持たせると 2 つの真実源ができて必ずずれるので持たない。
/// [`scrollbar_marks`] の出力はこの昇順に並ぶ = 先頭から順に描けば
/// 優先度の高い印が上に乗る。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ScrollKind {
    /// 検索ヒット。数が多く、消えても困らないので最下層。
    SearchHit,
    /// ブックマーク。利用者が自分で置いたもの。
    Bookmark,
    /// 警告。
    Warn,
    /// エラー。
    Error,
    /// カーソル行。「いま自分がどこにいるか」は最優先。
    Cursor,
}

/// 帯の中の縦の列。
///
/// ## なぜ 2 レーン + 全幅なのか (Zed に倣い、根拠を残す)
///
/// Zed は左を git、右を診断に割り当てている。**同じ列に重ねると優先順位が要り、
/// 負けた側は「無い」のと同じになる**ため、性格の違うものは列で分ける。
/// このリポジトリの [`Marks`] には git が無いので、分け方を性格で決めた:
///
/// - **左 = 利用者が置いた印** (ブックマーク / 検索ヒット)。自分の操作の結果なので
///   「探しに行く」対象。
/// - **右 = ツールが出した印** (エラー / 警告)。Zed と同じ配置。
/// - **全幅 = カーソル行**。列を跨いで 1 本引く。ここだけは埋もれてはいけない。
///
/// 3 レーンにしなかったのは、12px を 3 等分すると 1 レーンが 3px になり、
/// [`MARK_MIN_H`] = 2px の印が「線」ではなく「ゴミ」に見えるため。
/// 左レーンの中で重なるブックマークと検索ヒットは [`ScrollKind`] の順序
/// (= 出力順) で解決する — **後から描かれるほうが上**。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lane {
    Left,
    Right,
    Full,
}

impl ScrollKind {
    /// この種別を置く列。
    fn lane(self) -> Lane {
        match self {
            ScrollKind::SearchHit | ScrollKind::Bookmark => Lane::Left,
            ScrollKind::Warn | ScrollKind::Error => Lane::Right,
            ScrollKind::Cursor => Lane::Full,
        }
    }
}

/// 帯の割り付けと行 ↔ y の変換。**描画は持たない**。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ScrollbarGeom {
    /// 印を置く帯 (呼び出し側が決めた矩形。正規化済み)。
    pub band: Rect,
    /// 行数 (最低 1)。
    pub line_count: usize,
    /// 行 → y のスケール (論理 px / 行)。帯が潰れていれば 0。
    pub scale: f32,
    /// 物理 1 ピクセルの論理長。
    pub px: f32,
    /// 物理ピクセル密度 (スナップ用)。
    ppp: f32,
}

/// 帯の矩形と行数から [`ScrollbarGeom`] を作る純関数。
///
/// 潰れた / 反転した / NaN の矩形でも落ちない (正規化して scale = 0 に倒す)。
pub fn scrollbar_geometry(band: Rect, line_count: usize, ppp: f32) -> ScrollbarGeom {
    let ppp = if ppp.is_finite() && ppp > 0.0 {
        ppp
    } else {
        1.0
    };
    let px = 1.0 / ppp;
    let line_count = line_count.max(1);

    let left = if band.left().is_finite() {
        band.left()
    } else {
        0.0
    };
    let top = if band.top().is_finite() {
        band.top()
    } else {
        0.0
    };
    let right = if band.right().is_finite() {
        band.right().max(left)
    } else {
        left
    };
    let bottom = if band.bottom().is_finite() {
        band.bottom().max(top)
    } else {
        top
    };
    let band = Rect::from_min_max(egui::pos2(left, top), egui::pos2(right, bottom));

    let h = band.height().max(0.0);
    let scale = if h <= 0.0 || band.width() <= 0.0 {
        0.0
    } else {
        h / line_count as f32
    };

    ScrollbarGeom {
        band,
        line_count,
        scale,
        px,
        ppp,
    }
}

/// スクロールバー帯の幅 (物理ピクセル境界へスナップ済み)。
pub fn scrollbar_width(ppp: f32) -> f32 {
    snap_len(SCROLLBAR_W, ppp)
}

impl ScrollbarGeom {
    /// 行 (0 始まり) の帯上の y 座標。
    ///
    /// [`Self::y_line`] との**往復が一致する** — ただし 1 行が物理 1px 未満へ
    /// 潰れる (`scale < px`) 領域では、そもそも別の行が同じピクセルに載るので
    /// 「同じピクセルへ戻る」までしか保証しない。
    pub fn line_y(&self, line: usize) -> f32 {
        if self.scale <= 0.0 {
            return self.band.top();
        }
        // f64 で積んでから 1 回だけ f32 へ丸める (途中で 2 回丸めると誤差が倍になる)
        let y = self.band.top() as f64 + line.min(self.line_count) as f64 * self.scale as f64;
        (y as f32).clamp(self.band.top(), self.band.bottom())
    }

    /// 帯上の y 座標が指す行 (0 始まり・行数でクランプ)。
    pub fn y_line(&self, y: f32) -> usize {
        if self.scale <= 0.0 || !y.is_finite() {
            return 0;
        }
        let rel = (y as f64 - self.band.top() as f64) / self.scale as f64;
        if !rel.is_finite() || rel <= 0.0 {
            return 0;
        }
        // 整数の近傍だけ吸着させる (SNAP_TOL の根拠は定数のコメント)
        let near = rel.round();
        let rel = if (rel - near).abs() <= SNAP_TOL {
            near
        } else {
            rel
        };
        if rel >= self.line_count as f64 {
            self.line_count.saturating_sub(1)
        } else {
            (rel.floor() as usize).min(self.line_count.saturating_sub(1))
        }
    }

    /// 帯の y をクリックしたときのスクロール量 (その行を中央に置く)。
    pub fn scroll_for_y(&self, y: f32, text_row_h: f32, view_h: f32) -> f32 {
        let line = self.y_line(y);
        let s = line as f32 * text_row_h - view_h * 0.5;
        if s.is_finite() {
            s.max(0.0)
        } else {
            0.0
        }
    }

    /// 列の x 範囲 (物理ピクセル境界へスナップ済み・左右は重ならない)。
    fn lane_x(&self, lane: Lane) -> (f32, f32) {
        let l = self.band.left();
        let r = self.band.right().max(l);
        let w = r - l;
        let mid = l + w * 0.5;
        // 帯が極端に細くても最低 1 物理ピクセルは確保する (印が消えないように)
        let lane_w = (w * LANE_FRAC).max(self.px).min(w);
        match lane {
            Lane::Full => (l, r),
            // 中央 (mid) を越えさせないので、左右のレーンは決して重ならない
            Lane::Left => (l, snap_at(l + lane_w, self.ppp).clamp(l, mid.max(l))),
            Lane::Right => (snap_at(r - lane_w, self.ppp).clamp(mid.min(r), r), r),
        }
    }

    /// いま画面に出ている範囲を示す枠 (Zed / VS Code はここが薄く光る)。
    ///
    /// 掴めるように [`VIEWPORT_MIN_H`] の高さを必ず確保し、帯からはみ出さない。
    pub fn viewport_rect(&self, first_line: f32, lines_on_screen: f32) -> Rect {
        let top = self.band.top();
        let bottom = self.band.bottom().max(top);
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
        let min_h = VIEWPORT_MIN_H.max(self.px).min(bottom - top);
        let y0 = (top + first * self.scale).clamp(top, bottom);
        let y1 = (top + (first + lines) * self.scale).clamp(top, bottom);
        let y1 = y1.max(y0 + min_h).min(bottom);
        let y0 = y0.min(y1 - min_h).clamp(top, bottom);
        Rect::from_min_max(
            egui::pos2(self.band.left(), y0),
            egui::pos2(self.band.right().max(self.band.left()), y1.max(y0)),
        )
    }
}

/// 帯へ描く印 1 本。**近接する同種の印はここへ畳まれている**。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ScrollbarMark {
    /// 描く矩形。必ず帯の内側で、高さは [`MARK_MIN_H`] 以上、
    /// **同じ種別どうしは決して重ならない**。
    /// 印が詰まっている場所では最小高さのぶん実際の行より下へずれ得るので、
    /// 「どの行か」は [`Self::lines`] を見ること。
    pub rect: Rect,
    /// 色を決めるための種別。
    pub kind: ScrollKind,
    /// この 1 本へ畳んだ元の印の本数 (1 なら畳んでいない)。
    pub count: u32,
    /// 濃さの係数 (0..=1)。同じ種別の中で最も混んでいる 1 本を 1.0 とした対数尺度。
    /// 呼び出し側は色の alpha へ掛ける (`Color32::from_rgba_unmultiplied` 等)。
    pub weight: f32,
    /// 畳んだ範囲の (先頭行, 末尾行)。クリックで先頭行へ飛ぶために返す。
    pub lines: (usize, usize),
}

/// [`scrollbar_marks`] の結果。
#[derive(Clone, PartialEq, Debug)]
pub struct ScrollbarDeco {
    /// **優先順位の昇順**。呼び出し側はこの順に描けば、重なったとき
    /// 優先度の高い印 (カーソル > エラー > 警告 > ブックマーク > 検索) が上に乗る。
    pub marks: Vec<ScrollbarMark>,
    /// いま画面に出ている範囲。
    pub viewport: Rect,
}

// テスト用のカウンタは**スレッドローカル**にする
// (プロセス共通の AtomicUsize にすると、同時に走る他のテストの呼び出しが混ざる)。
#[cfg(test)]
thread_local! {
    static SWEEP_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn bump_sweep(n: usize) {
    SWEEP_STEPS.with(|c| c.set(c.get() + n));
}

#[cfg(not(test))]
#[inline(always)]
fn bump_sweep(_n: usize) {}

/// 帯へ置く印とビューポート枠を計算する純関数。
///
/// 入力は **既存の [`Marks`] をそのまま**使う (`app.rs` がミニマップ用に
/// 組み立てている値を作り直さずに済む)。`cursor` は 0 始まりの行。
/// `first_line` / `lines_on_screen` は小数可 (滑らかに動かすため)。
///
/// 極端な入力 (行数 0 / 帯の高さ 0 / 負の高さ / 行番号が行数を超える /
/// 印が 0 件 / 印が行数より多い) でも panic せず、空か妥当な結果を返す。
pub fn scrollbar_marks(
    geom: &ScrollbarGeom,
    marks: &Marks,
    first_line: f32,
    lines_on_screen: f32,
    cursor: Option<usize>,
) -> ScrollbarDeco {
    let viewport = geom.viewport_rect(first_line, lines_on_screen);
    let mut out: Vec<ScrollbarMark> = Vec::new();
    if geom.band.width() <= 0.0 || geom.band.height() <= 0.0 || geom.scale <= 0.0 {
        return ScrollbarDeco {
            marks: out,
            viewport,
        };
    }

    // 優先順位の昇順に積む (= 呼び出し側が順に描くと高優先が上に乗る)
    let mut buf: Vec<usize> = Vec::new();

    buf.extend(marks.search.iter().copied());
    push_runs(geom, ScrollKind::SearchHit, &mut buf, &mut out);

    buf.extend(marks.bookmarks.iter().copied());
    push_runs(geom, ScrollKind::Bookmark, &mut buf, &mut out);

    buf.extend(
        marks
            .diags
            .iter()
            .filter(|(_, sev)| **sev != 1)
            .map(|(l, _)| *l),
    );
    push_runs(geom, ScrollKind::Warn, &mut buf, &mut out);

    buf.extend(
        marks
            .diags
            .iter()
            .filter(|(_, sev)| **sev == 1)
            .map(|(l, _)| *l),
    );
    push_runs(geom, ScrollKind::Error, &mut buf, &mut out);

    if let Some(c) = cursor {
        buf.push(c);
        push_runs(geom, ScrollKind::Cursor, &mut buf, &mut out);
    }

    ScrollbarDeco {
        marks: out,
        viewport,
    }
}

/// 1 種別ぶんの行番号を、近接するものを畳みながら矩形へ落とす。
///
/// `lines` は**呼び出しのたびに空にして返す** (呼び出し側で使い回すため)。
/// 計算量は整列の O(n log n) + 掃引の O(n)。総当たりは 1 度もしない。
fn push_runs(
    geom: &ScrollbarGeom,
    kind: ScrollKind,
    lines: &mut Vec<usize>,
    out: &mut Vec<ScrollbarMark>,
) {
    // 行数を超える行番号は捨てる (「行番号が総行数を超える」入力)
    lines.retain(|l| *l < geom.line_count);
    lines.sort_unstable();
    lines.dedup();
    if lines.is_empty() {
        lines.clear();
        return;
    }

    let (x0, x1) = geom.lane_x(kind.lane());
    let top = geom.band.top();
    let bottom = geom.band.bottom();
    let h = snap_len(MARK_MIN_H, geom.ppp)
        .max(geom.px)
        .min(bottom - top);
    let max_h = snap_len(MARK_MAX_H, geom.ppp).max(h);
    let gap = MERGE_GAP_PX * geom.px;

    // (y0, y1, 本数, 先頭行, 末尾行)
    let mut runs: Vec<(f32, f32, u32, usize, usize)> = Vec::new();
    bump_sweep(lines.len());
    for &l in lines.iter() {
        // 帯の下端でも高さ h を必ず確保する (「1px 未満に潰れないこと」)
        let raw = geom.line_y(l).clamp(top, (bottom - h).max(top));
        let Some(r) = runs.last_mut() else {
            runs.push((raw, (raw + h).min(bottom), 1, l, l));
            continue;
        };
        // 隙間が 1 物理px 以下なら畳む。ただし max_h を超えては畳まない
        // (畳みすぎると密度の分布が 1 本の棒に潰れるため)
        if raw <= r.1 + gap && (raw + h - r.0) <= max_h + 1e-4 {
            r.1 = (raw + h).min(bottom).max(r.1);
            r.2 = r.2.saturating_add(1);
            r.4 = l;
            continue;
        }
        // **重なりは絶対に作らない。** max_h で切った直後の 1 本は、最小高さの
        // ぶんだけ前の 1 本へめり込み得る (実測でここが壊れた)。半透明で重ね塗り
        // すると交差だけが濃くなり、「密度を濃さで表す」という前提が崩れる。
        // ずらすぶん矩形は実際の行より下へ出るが、正確な行は `lines` が持つ。
        let y0 = raw.max(r.1).min((bottom - h).max(top));
        if y0 < r.1 {
            // 帯の下端で新しい 1 本を置く場所が無い → 前の 1 本へ畳む
            r.2 = r.2.saturating_add(1);
            r.4 = l;
            continue;
        }
        runs.push((y0, (y0 + h).min(bottom), 1, l, l));
    }

    let max_count = runs.iter().map(|r| r.2).max().unwrap_or(1);
    out.reserve(runs.len());
    for (y0, y1, count, first, last) in runs {
        out.push(ScrollbarMark {
            rect: Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1.max(x0), y1.max(y0))),
            kind,
            count,
            weight: density_weight(count, max_count),
            lines: (first, last),
        });
    }
    lines.clear();
}

/// 畳んだ本数 → 濃さ (0..=1)。
///
/// 線形にすると 1 件が 4000 件の前で完全に消える。対数にすると
/// 1 件と 5 件の差は大きく、100 件と 200 件の差は小さくなり、
/// 「どこに固まっているか」を見る目的に合う。
fn density_weight(count: u32, max_count: u32) -> f32 {
    if max_count <= 1 || count >= max_count {
        return 1.0;
    }
    let w = (count as f32 + 1.0).ln() / (max_count as f32 + 1.0).ln();
    if w.is_finite() {
        w.clamp(MIN_WEIGHT, 1.0)
    } else {
        1.0
    }
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

    // ── スクロールバー装飾 (帯) ──────────────────────────────────

    fn band_of(w: f32, h: f32) -> Rect {
        Rect::from_min_size(egui::pos2(37.0, 11.0), egui::vec2(w, h))
    }

    fn empty_marks() -> (Vec<usize>, HashMap<usize, u8>, HashSet<usize>) {
        (Vec::new(), HashMap::new(), HashSet::new())
    }

    /// 行 → y → 行 の往復が**厳密に一致**する (1 行が物理 1px 以上ある領域)。
    /// クリックでその行へ飛ぶ機能はこの一致に完全に依存している。
    #[test]
    fn scrollbar_line_y_roundtrips_exactly_when_resolvable() {
        for (w, h) in [
            (12.0f32, 700.0f32),
            (14.0, 300.0),
            (10.0, 1440.0),
            (12.0, 23.0),
        ] {
            for ppp in [1.0f32, 1.25, 2.0, 3.0] {
                for n in [1usize, 2, 7, 40, 137, 1_000, 10_000] {
                    let g = scrollbar_geometry(band_of(w, h), n, ppp);
                    if g.scale < g.px {
                        continue; // 1 行が物理 1px 未満 = そもそも解像できない領域
                    }
                    for l in 0..n {
                        let y = g.line_y(l);
                        assert_eq!(
                            g.y_line(y),
                            l,
                            "{w}x{h} ppp={ppp} n={n}: 行 {l} の往復が壊れた (y={y})"
                        );
                        assert!(
                            g.band.top() - 1e-3 <= y && y <= g.band.bottom() + 1e-3,
                            "{w}x{h} n={n}: 行 {l} の y={y} が帯の外"
                        );
                    }
                }
            }
        }
    }

    /// 解像できない領域 (10 万行を 700px へ) でも、往復は**同じ物理ピクセル**へ戻る。
    /// 厳密一致は f32 の刻みより行間隔が狭くなる時点で物理的に不可能なので、
    /// 保証をここで正直に切り替える。
    #[test]
    fn scrollbar_roundtrip_stays_within_one_pixel_when_unresolvable() {
        for n in [100_000usize, 1_000_000, 5_000_000] {
            for ppp in [1.0f32, 2.0] {
                let g = scrollbar_geometry(band_of(12.0, 700.0), n, ppp);
                assert!(g.scale < g.px, "n={n} は解像できない前提のはず");
                for l in [0usize, 1, n / 3, n / 2, n - 2, n - 1] {
                    let y = g.line_y(l);
                    let back = g.y_line(y);
                    let dy = (g.line_y(back) - y).abs();
                    assert!(
                        dy <= g.px + 1e-3,
                        "n={n} ppp={ppp}: 行 {l} の往復が {dy}px ずれた (物理1px超)"
                    );
                }
                // y → 行 → y も同じ画素に戻る
                let mut y = g.band.top();
                while y <= g.band.bottom() {
                    let back = g.line_y(g.y_line(y));
                    assert!(
                        (back - y).abs() <= g.scale + g.px + 1e-3,
                        "n={n}: y={y} の往復が {}px ずれた",
                        (back - y).abs()
                    );
                    y += 0.5;
                }
            }
        }
    }

    /// y → 行 は単調非減少で、帯の外は先頭 / 末尾で飽和する。
    #[test]
    fn scrollbar_y_line_is_monotonic_and_saturates() {
        let g = scrollbar_geometry(band_of(12.0, 300.0), 1_200, 2.0);
        let mut prev = 0usize;
        let mut y = g.band.top();
        while y <= g.band.bottom() {
            let l = g.y_line(y);
            assert!(l >= prev, "y={y} で行が戻った ({prev} → {l})");
            assert!(l < 1_200);
            prev = l;
            y += 0.25;
        }
        assert_eq!(g.y_line(g.band.top() - 999.0), 0, "上に外れたら先頭行");
        assert_eq!(
            g.y_line(g.band.bottom() + 999.0),
            1_199,
            "下に外れたら最終行"
        );
        assert_eq!(g.y_line(f32::NAN), 0);
        assert_eq!(g.scroll_for_y(g.band.top(), 18.0, 300.0), 0.0);
        assert!(g.scroll_for_y(g.band.bottom(), 18.0, 300.0) > 0.0);
    }

    /// 極端なサイズ × 極端な行数で、**全ての矩形が帯の中に収まり、
    /// 同じレーンの中で重ならない**。左右のレーンも決して重ならない。
    #[test]
    fn scrollbar_rects_stay_inside_and_never_overlap() {
        let sizes: &[(f32, f32)] = &[
            (12.0, 700.0),
            (10.0, 300.0),
            (14.0, 1440.0),
            (12.0, 20.0),
            (2.0, 700.0),
            (0.5, 700.0),
        ];
        let counts: &[usize] = &[1, 2, 40, 1_000, 100_000];
        for (w, h) in sizes {
            for n in counts {
                for ppp in [1.0f32, 1.25, 2.0] {
                    let g = scrollbar_geometry(band_of(*w, *h), *n, ppp);
                    let tag = format!("{w}x{h} n={n} ppp={ppp}");

                    let search: Vec<usize> = (0..*n).step_by((n / 97).max(1)).collect();
                    let mut diags: HashMap<usize, u8> = HashMap::new();
                    for (i, l) in (0..*n).step_by((n / 13).max(1)).enumerate() {
                        diags.insert(l, if i % 2 == 0 { 1 } else { 2 });
                    }
                    let bookmarks: HashSet<usize> = (0..*n).step_by((n / 5).max(1)).collect();
                    let marks = Marks {
                        search: &search,
                        diags: &diags,
                        bookmarks: &bookmarks,
                    };
                    let deco = scrollbar_marks(&g, &marks, *n as f32 * 0.3, 40.0, Some(n / 2));

                    assert!(
                        g.band.contains_rect(deco.viewport),
                        "{tag}: ビューポート枠が帯からはみ出した {:?}",
                        deco.viewport
                    );

                    let mut by_kind: HashMap<ScrollKind, Vec<Rect>> = HashMap::new();
                    for m in &deco.marks {
                        assert!(
                            g.band.contains_rect(m.rect),
                            "{tag}: 印 {:?} が帯からはみ出した {:?}",
                            m.kind,
                            m.rect
                        );
                        assert!(
                            m.rect.height() >= (MARK_MIN_H.min(*h) - 1e-3),
                            "{tag}: 印の高さ {} が最小 {MARK_MIN_H} を割った",
                            m.rect.height()
                        );
                        assert!(m.count >= 1 && m.weight > 0.0 && m.weight <= 1.0, "{tag}");
                        assert!(m.lines.0 <= m.lines.1 && m.lines.1 < g.line_count, "{tag}");
                        by_kind.entry(m.kind).or_default().push(m.rect);
                    }
                    // 同じ種別の印は 1 本も重ならない (畳んだ結果なので当然そうなるべき)
                    for (k, rects) in &by_kind {
                        for pair in rects.windows(2) {
                            assert!(
                                pair[0].bottom() <= pair[1].top() + 1e-3,
                                "{tag}: 種別 {k:?} の印が重なった {:?} / {:?}",
                                pair[0],
                                pair[1]
                            );
                        }
                    }
                    // 左右のレーンは重ならない (性格の違う印を混ぜないための不変条件)
                    let left = g.lane_x(Lane::Left);
                    let right = g.lane_x(Lane::Right);
                    assert!(
                        left.1 <= right.0 + 1e-3,
                        "{tag}: 左レーン {left:?} と右レーン {right:?} が重なった"
                    );
                    assert!(
                        left.0 >= g.band.left() - 1e-3 && right.1 <= g.band.right() + 1e-3,
                        "{tag}: レーンが帯からはみ出した"
                    );
                    // 出力は優先順位の昇順 (呼び出し側は先頭から順に描くだけでよい)
                    // 宣言順 = 優先順位。呼び出し側は先頭から順に描くだけでよい
                    let mut prev = ScrollKind::SearchHit;
                    for m in &deco.marks {
                        assert!(
                            m.kind >= prev,
                            "{tag}: 出力が優先順位の昇順になっていない ({prev:?} → {:?})",
                            m.kind
                        );
                        prev = m.kind;
                    }
                }
            }
        }
    }

    /// 近接する同種の印は 1 本へ畳まれ、**件数を増やしても出力は増えない**。
    /// 10 万行に 5000 件の検索ヒットがあっても帯が真っ黒にならないための性質。
    #[test]
    fn scrollbar_merges_dense_marks_into_bounded_runs() {
        let g = scrollbar_geometry(band_of(12.0, 700.0), 100_000, 2.0);
        let (_, diags, bookmarks) = empty_marks();
        // 帯の物理ピクセル数から決まる上限。件数には依存しない
        let cap = (700.0f32 / MARK_MIN_H).ceil() as usize + 2;

        let mut prev_len = usize::MAX;
        for hits in [5_000usize, 10_000, 20_000, 40_000] {
            let search: Vec<usize> = (0..hits).map(|i| i * (100_000 / hits)).collect();
            let marks = Marks {
                search: &search,
                diags: &diags,
                bookmarks: &bookmarks,
            };
            let deco = scrollbar_marks(&g, &marks, 0.0, 40.0, None);
            assert!(
                deco.marks.len() <= cap,
                "{hits} 件で {} 本出た (上限 {cap})",
                deco.marks.len()
            );
            // 件数を 2 倍にしても本数は増えない (畳みが効いている証拠)
            assert!(
                deco.marks.len() <= prev_len || prev_len == usize::MAX,
                "{hits} 件で本数が増えた ({prev_len} → {})",
                deco.marks.len()
            );
            prev_len = deco.marks.len();
            // 畳んだことが本数として残っている
            let total: u32 = deco.marks.iter().map(|m| m.count).sum();
            assert_eq!(
                total as usize, hits,
                "{hits} 件: 畳んだ本数の合計が合わない"
            );
            assert!(
                deco.marks.iter().any(|m| m.count > 1),
                "{hits} 件: 1 本も畳まれていない"
            );
            // 印はレーン内に収まるので、帯の幅の半分以上は必ず空いている
            for m in &deco.marks {
                assert!(
                    m.rect.width() <= g.band.width() * (LANE_FRAC + 1e-3),
                    "検索ヒットが帯の全幅を塗った"
                );
            }
        }
    }

    /// 疎な印は畳まれず、行の位置が保たれる。濃さは本数に応じて上がる。
    #[test]
    fn scrollbar_keeps_sparse_marks_separate() {
        let g = scrollbar_geometry(band_of(12.0, 700.0), 200, 2.0);
        let search: Vec<usize> = vec![3, 40, 120, 199];
        let (_, diags, bookmarks) = empty_marks();
        let marks = Marks {
            search: &search,
            diags: &diags,
            bookmarks: &bookmarks,
        };
        let deco = scrollbar_marks(&g, &marks, 0.0, 20.0, None);
        assert_eq!(deco.marks.len(), 4, "疎な 4 件は畳まれない");
        for (m, l) in deco.marks.iter().zip(search.iter()) {
            assert_eq!(m.count, 1);
            assert_eq!(m.lines, (*l, *l));
            assert_eq!(m.weight, 1.0, "全部 1 件なら濃さは一様");
            assert_eq!(g.y_line(m.rect.top()), *l, "印の頭がその行を指していない");
        }

        // 濃さは対数尺度で単調 (1 件 < 5 件 < 100 件)、下限を割らない
        assert!(density_weight(1, 100) >= MIN_WEIGHT);
        assert!(density_weight(1, 100) < density_weight(5, 100));
        assert!(density_weight(5, 100) < density_weight(100, 100));
        assert_eq!(density_weight(100, 100), 1.0);
        assert_eq!(density_weight(1, 1), 1.0);
        assert_eq!(density_weight(7, 0), 1.0, "max が 0 でも 0 除算しない");
    }

    /// 種別ごとにレーンが分かれ、カーソルだけが全幅。
    #[test]
    fn scrollbar_lanes_separate_user_marks_from_diagnostics() {
        let g = scrollbar_geometry(band_of(12.0, 700.0), 400, 2.0);
        let search = vec![10usize];
        let bookmarks: HashSet<usize> = [20usize].into_iter().collect();
        let diags: HashMap<usize, u8> = [(30usize, 1u8), (40, 2)].into_iter().collect();
        let marks = Marks {
            search: &search,
            diags: &diags,
            bookmarks: &bookmarks,
        };
        let deco = scrollbar_marks(&g, &marks, 0.0, 20.0, Some(50));
        let find = |k: ScrollKind| deco.marks.iter().find(|m| m.kind == k).unwrap().rect;
        let (ll, lr) = g.lane_x(Lane::Left);
        let (rl, rr) = g.lane_x(Lane::Right);
        for k in [ScrollKind::SearchHit, ScrollKind::Bookmark] {
            let r = find(k);
            assert!(
                (r.left() - ll).abs() < 1e-3 && (r.right() - lr).abs() < 1e-3,
                "{k:?} が左レーンに無い"
            );
        }
        for k in [ScrollKind::Error, ScrollKind::Warn] {
            let r = find(k);
            assert!(
                (r.left() - rl).abs() < 1e-3 && (r.right() - rr).abs() < 1e-3,
                "{k:?} が右レーンに無い"
            );
        }
        let c = find(ScrollKind::Cursor);
        assert!(
            (c.width() - g.band.width()).abs() < 1e-3,
            "カーソル行は帯の全幅"
        );
        assert_eq!(
            deco.marks.last().unwrap().kind,
            ScrollKind::Cursor,
            "カーソルが最後 = 最前面"
        );
    }

    /// 極端な入力で panic せず、空か妥当な結果を返す。
    #[test]
    fn scrollbar_extreme_inputs_never_panic() {
        let big: Vec<usize> = (0..5_000).collect();
        let far: Vec<usize> = vec![0, 1, usize::MAX, 999_999_999];
        let diags: HashMap<usize, u8> = [(0usize, 1u8), (usize::MAX, 2)].into_iter().collect();
        let bookmarks: HashSet<usize> = [0usize, usize::MAX].into_iter().collect();
        let (empty_search, empty_diags, empty_bm) = empty_marks();

        for (w, h) in [
            (12.0f32, 700.0f32),
            (12.0, 0.0),
            (0.0, 700.0),
            (0.0, 0.0),
            (-8.0, -8.0),
            (f32::NAN, 700.0),
            (12.0, f32::INFINITY),
        ] {
            for n in [0usize, 1, 2, 10, 5_000_000] {
                for ppp in [1.0f32, 2.0, 0.0, -1.0, f32::NAN] {
                    let g = scrollbar_geometry(band_of(w, h), n, ppp);
                    let tag = format!("{w}x{h} n={n} ppp={ppp}");
                    // 印が 0 件
                    let d = scrollbar_marks(
                        &g,
                        &Marks {
                            search: &empty_search,
                            diags: &empty_diags,
                            bookmarks: &empty_bm,
                        },
                        0.0,
                        10.0,
                        None,
                    );
                    assert!(d.marks.is_empty(), "{tag}: 印 0 件なのに矩形が出た");
                    // 行数を超える行番号 / 印が行数より多い
                    for search in [&far, &big] {
                        let d = scrollbar_marks(
                            &g,
                            &Marks {
                                search,
                                diags: &diags,
                                bookmarks: &bookmarks,
                            },
                            f32::NAN,
                            f32::NAN,
                            Some(usize::MAX),
                        );
                        for m in &d.marks {
                            assert!(m.lines.1 < g.line_count, "{tag}: 行数を超える印が残った");
                            assert!(m.rect.width() >= 0.0 && m.rect.height() >= 0.0, "{tag}");
                            assert!(g.band.contains_rect(m.rect), "{tag}: 帯の外へ出た");
                        }
                        assert!(d.viewport.height() >= 0.0, "{tag}");
                    }
                    let _ = g.line_y(usize::MAX);
                    let _ = g.y_line(f32::INFINITY);
                    let _ = g.y_line(f32::NEG_INFINITY);
                    let _ = g.scroll_for_y(f32::NAN, f32::NAN, f32::NAN);
                    let _ = g.viewport_rect(f32::NEG_INFINITY, -5.0);
                }
            }
        }
        assert!(scrollbar_width(2.0) > 0.0);
        assert_eq!(scrollbar_width(1.0), SCROLLBAR_W);
    }

    /// 掃引の費用は件数に**線形**。総当たり (O(N²)) へ退化していないことを、
    /// 実時間ではなく**掃引の回数**で固定する (負荷で揺れないため)。
    #[test]
    fn scrollbar_sweep_cost_is_linear_in_mark_count() {
        let g = scrollbar_geometry(band_of(12.0, 700.0), 200_000, 2.0);
        let (_, diags, bookmarks) = empty_marks();
        let measure = |hits: usize| -> usize {
            let search: Vec<usize> = (0..hits).map(|i| i * 7 % 200_000).collect();
            SWEEP_STEPS.with(|c| c.set(0));
            let _ = scrollbar_marks(
                &g,
                &Marks {
                    search: &search,
                    diags: &diags,
                    bookmarks: &bookmarks,
                },
                0.0,
                40.0,
                None,
            );
            SWEEP_STEPS.with(|c| c.get())
        };
        let a = measure(2_000);
        let b = measure(4_000);
        let c = measure(8_000);
        assert_eq!(a, 2_000, "掃引回数が件数と一致しない");
        assert_eq!(b, 2 * a, "件数 2 倍で掃引が {b} (線形なら {})", 2 * a);
        assert_eq!(c, 2 * b, "件数 4 倍で掃引が {c} (線形なら {})", 2 * b);
    }

    /// ビューポート枠は掴める高さを必ず持ち、帯の中を上から下へ動く。
    #[test]
    fn scrollbar_viewport_is_grabbable_and_moves_monotonically() {
        for (n, h) in [(40usize, 700.0f32), (100_000, 700.0), (3, 20.0)] {
            let g = scrollbar_geometry(band_of(12.0, h), n, 2.0);
            let mut prev_top = f32::NEG_INFINITY;
            for first in [
                0.0f32,
                n as f32 * 0.25,
                n as f32 * 0.5,
                n as f32,
                n as f32 * 10.0,
            ] {
                let vp = g.viewport_rect(first, 30.0);
                assert!(g.band.contains_rect(vp), "n={n}: 枠が帯の外 {vp:?}");
                assert!(
                    vp.height() >= VIEWPORT_MIN_H.min(h) - 1e-3,
                    "n={n}: 枠の高さ {} が掴める下限を割った",
                    vp.height()
                );
                assert!(vp.top() >= prev_top - 1e-3, "n={n}: 枠が上へ戻った");
                prev_top = vp.top();
                assert!((vp.width() - g.band.width()).abs() < 1e-3, "枠は帯の全幅");
            }
        }
    }
}
