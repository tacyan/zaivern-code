//! unified diff のパースと、追加/削除行を色分けするインライン diff ビュー。
//!
//! `git diff` / `gh pr diff` が吐く unified 形式をそのまま受け取り、
//! ファイル単位 → ハンク単位 → 行単位に分解して描画する。
//!
//! パース部 (`parse_unified`) は純関数で、GUI に依存しない。
//! ハンクヘッダの解釈は `git::parse_range` / `git::parse_hunk_marks` と同じ流儀
//! (カウント省略 = 1、行番号は diff 上 1-based) に揃えてある。


use eframe::egui::{self, Color32, FontId, RichText};

use crate::theme::Theme;

// ---------------------------------------------------------------------------
// データモデル
// ---------------------------------------------------------------------------

/// diff 行の種別。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// 変更なしの文脈行 (先頭 ' ')
    Context,
    /// 追加行 (先頭 '+')
    Added,
    /// 削除行 (先頭 '-')
    Removed,
}

/// diff の 1 行。`old_no` / `new_no` は 1-based の行番号 (存在しない側は None)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    /// 先頭のマーカー (' ' / '+' / '-') を除いた本文。
    pub text: String,
}

/// `@@ -a,b +c,d @@ ...` で区切られる 1 ハンク。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// `@@` 行そのもの (末尾の文脈テキストを含む)。
    pub header: String,
    pub old_start: usize,
    pub new_start: usize,
    pub lines: Vec<DiffLine>,
}

/// 1 ファイル分の diff。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub hunks: Vec<Hunk>,
    pub is_binary: bool,
    pub is_rename: bool,
    pub additions: usize,
    pub deletions: usize,
}

const DEV_NULL: &str = "/dev/null";

impl FileDiff {
    fn new() -> Self {
        FileDiff {
            old_path: String::new(),
            new_path: String::new(),
            hunks: Vec::new(),
            is_binary: false,
            is_rename: false,
            additions: 0,
            deletions: 0,
        }
    }

    /// 表示用のパス。リネームなら `old → new`、それ以外は存在する方。
    pub fn display_path(&self) -> String {
        if self.is_rename && self.old_path != self.new_path {
            format!("{} → {}", self.old_path, self.new_path)
        } else if !self.new_path.is_empty() && self.new_path != DEV_NULL {
            self.new_path.clone()
        } else {
            self.old_path.clone()
        }
    }
}

// ---------------------------------------------------------------------------
// パース
// ---------------------------------------------------------------------------

/// `-a,b` / `+c,d` / `+c` (カウント省略 = 1) を (start, count) にパースする。
/// git.rs の `parse_range` と同じ規約。
fn parse_range(token: &str) -> Option<(usize, usize)> {
    let body = token
        .strip_prefix('+')
        .or_else(|| token.strip_prefix('-'))?;
    let mut parts = body.splitn(2, ',');
    let start: usize = parts.next()?.trim().parse().ok()?;
    let count: usize = match parts.next() {
        Some(cnt) => cnt.trim().parse().ok()?,
        None => 1,
    };
    Some((start, count))
}

/// `@@ -a,b +c,d @@ trailing` から ((old_start, old_count), (new_start, new_count)) を取り出す。
fn parse_hunk_header(line: &str) -> Option<((usize, usize), (usize, usize))> {
    if !line.starts_with("@@") {
        return None;
    }
    let mut tokens = line.split_whitespace();
    let _at = tokens.next()?; // "@@"
    let (old_tok, new_tok) = match (tokens.next(), tokens.next()) {
        (Some(o), Some(n)) if o.starts_with('-') && n.starts_with('+') => (o, n),
        _ => return None,
    };
    Some((parse_range(old_tok)?, parse_range(new_tok)?))
}

/// `--- a/foo` / `+++ b/foo` / `--- /dev/null` からパスを取り出す。
/// 末尾のタイムスタンプ (タブ区切り) は落とす。
fn strip_side_prefix(rest: &str) -> String {
    let rest = rest.split('\t').next().unwrap_or(rest).trim_end();
    let rest = rest.trim_matches('"');
    if rest == DEV_NULL {
        return DEV_NULL.to_string();
    }
    rest.strip_prefix("a/")
        .or_else(|| rest.strip_prefix("b/"))
        .unwrap_or(rest)
        .to_string()
}

/// `diff --git a/x b/y` の残り部分から (old, new) を取り出す。
/// スペースを含むパスに備え、まず ` b/` を境界として探す。
fn split_git_header(rest: &str) -> Option<(String, String)> {
    if let Some(pos) = rest.rfind(" b/") {
        let (a, b) = rest.split_at(pos);
        return Some((strip_side_prefix(a), strip_side_prefix(&b[1..])));
    }
    // フォールバック: 空白 2 分割。
    let (a, b) = rest.split_once(' ')?;
    Some((strip_side_prefix(a), strip_side_prefix(b)))
}

/// unified diff 全体を FileDiff の並びへ分解する。
///
/// 対応: 複数ファイル / `diff --git` ヘッダ / `--- +++` / `@@` (カウント省略含む) /
/// new file・deleted file mode / バイナリ / リネーム (ハンク無しも可) /
/// `\ No newline at end of file` (本文行として数えない)。
pub fn parse_unified(input: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut cur: Option<FileDiff> = None;
    // ハンク進行中の状態。
    let mut rem_old = 0usize;
    let mut rem_new = 0usize;
    let mut old_no = 0usize;
    let mut new_no = 0usize;
    let mut in_hunk = false;

    for line in input.lines() {
        // --- ハンク本体 (宣言された行数を消化しきるまでを最優先で処理) ---
        if in_hunk && (rem_old > 0 || rem_new > 0) {
            if line.starts_with('\\') {
                // "\ No newline at end of file" — 本文行ではない。
                continue;
            }
            let parsed = match line.as_bytes().first() {
                Some(b'+') => Some((LineKind::Added, &line[1..])),
                Some(b'-') => Some((LineKind::Removed, &line[1..])),
                Some(b' ') => Some((LineKind::Context, &line[1..])),
                // 空行は「空の文脈行」として出力されることがある。
                None => Some((LineKind::Context, "")),
                _ => None,
            };
            let Some((kind, body)) = parsed else {
                // ハンク内で想定外の行 → ハンク終了とみなして読み直す。
                in_hunk = false;
                rem_old = 0;
                rem_new = 0;
                process_file_level(line, &mut cur, &mut files);
                continue;
            };

            let file = cur.as_mut().expect("in_hunk implies a current file");
            let (o, n) = match kind {
                LineKind::Context => {
                    let (o, n) = (old_no, new_no);
                    old_no += 1;
                    new_no += 1;
                    rem_old = rem_old.saturating_sub(1);
                    rem_new = rem_new.saturating_sub(1);
                    (Some(o), Some(n))
                }
                LineKind::Added => {
                    let n = new_no;
                    new_no += 1;
                    rem_new = rem_new.saturating_sub(1);
                    file.additions += 1;
                    (None, Some(n))
                }
                LineKind::Removed => {
                    let o = old_no;
                    old_no += 1;
                    rem_old = rem_old.saturating_sub(1);
                    file.deletions += 1;
                    (Some(o), None)
                }
            };
            file.hunks
                .last_mut()
                .expect("in_hunk implies at least one hunk")
                .lines
                .push(DiffLine {
                    kind,
                    old_no: o,
                    new_no: n,
                    text: body.to_string(),
                });
            if rem_old == 0 && rem_new == 0 {
                in_hunk = false;
            }
            continue;
        }
        in_hunk = false;

        // --- ハンクヘッダ ---
        if let Some(((os, oc), (ns, nc))) = parse_hunk_header(line) {
            let file = cur.get_or_insert_with(FileDiff::new);
            file.hunks.push(Hunk {
                header: line.to_string(),
                old_start: os,
                new_start: ns,
                lines: Vec::new(),
            });
            rem_old = oc;
            rem_new = nc;
            old_no = os;
            new_no = ns;
            in_hunk = rem_old > 0 || rem_new > 0;
            continue;
        }

        // 宣言行数を消化しきった直後の "\ No newline" はここに落ちてくる。
        if line.starts_with('\\') {
            continue;
        }

        process_file_level(line, &mut cur, &mut files);
    }

    if let Some(f) = cur.take() {
        files.push(f);
    }
    files
}

/// ハンク外のメタ行を処理する。
fn process_file_level(line: &str, cur: &mut Option<FileDiff>, files: &mut Vec<FileDiff>) {
    if let Some(rest) = line.strip_prefix("diff --git ") {
        if let Some(f) = cur.take() {
            files.push(f);
        }
        let mut f = FileDiff::new();
        if let Some((a, b)) = split_git_header(rest) {
            f.old_path = a;
            f.new_path = b;
        }
        *cur = Some(f);
        return;
    }

    if let Some(rest) = line.strip_prefix("--- ") {
        // `diff --git` を伴わない素の unified diff では、ここが次ファイルの開始。
        if cur.as_ref().map(|f| !f.hunks.is_empty()).unwrap_or(false) {
            if let Some(f) = cur.take() {
                files.push(f);
            }
        }
        let f = cur.get_or_insert_with(FileDiff::new);
        f.old_path = strip_side_prefix(rest);
        return;
    }
    if let Some(rest) = line.strip_prefix("+++ ") {
        let f = cur.get_or_insert_with(FileDiff::new);
        f.new_path = strip_side_prefix(rest);
        return;
    }

    if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
        let f = cur.get_or_insert_with(FileDiff::new);
        f.is_binary = true;
        // "Binary files a/x and b/y differ" からパスを補う。
        if f.new_path.is_empty() {
            if let Some(body) = line
                .strip_prefix("Binary files ")
                .and_then(|b| b.strip_suffix(" differ"))
            {
                if let Some((a, b)) = body.split_once(" and ") {
                    f.old_path = strip_side_prefix(a);
                    f.new_path = strip_side_prefix(b);
                }
            }
        }
        return;
    }

    if let Some(rest) = line.strip_prefix("rename from ") {
        let f = cur.get_or_insert_with(FileDiff::new);
        f.is_rename = true;
        f.old_path = strip_side_prefix(rest);
        return;
    }
    if let Some(rest) = line.strip_prefix("rename to ") {
        let f = cur.get_or_insert_with(FileDiff::new);
        f.is_rename = true;
        f.new_path = strip_side_prefix(rest);
    }

    // new file mode / deleted file mode / index / similarity index / mode 変更などは
    // 追加情報を持たないので読み飛ばす。
}

// ---------------------------------------------------------------------------
// 描画
// ---------------------------------------------------------------------------

/// `a` と `b` を混ぜる。t=0 で a、t=1 で b。
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

/// テーマ由来の diff 配色。ハードコードせず bg と ok/err/accent を混ぜて作る。
struct DiffPalette {
    add_bg: Color32,
    del_bg: Color32,
    gutter_bg: Color32,
    hunk_bg: Color32,
    add_fg: Color32,
    del_fg: Color32,
}

impl DiffPalette {
    fn from_theme(t: &Theme) -> Self {
        // ライトテーマは地の明度が高く、同じ比率では色が沈むので濃いめに混ぜる。
        let tint = if t.dark { 0.18 } else { 0.26 };
        DiffPalette {
            add_bg: mix(t.bg, t.ok, tint),
            del_bg: mix(t.bg, t.err, tint),
            gutter_bg: mix(t.bg, t.panel, 0.7),
            hunk_bg: mix(t.bg, t.accent_soft, 0.9),
            // 記号 (+/-) は本文より強調するが、テーマ色を保つ。
            add_fg: mix(t.text, t.ok, if t.dark { 0.65 } else { 0.55 }),
            del_fg: mix(t.text, t.err, if t.dark { 0.65 } else { 0.55 }),
        }
    }
}

const GUTTER_COL_W: f32 = 34.0;
const SIGN_W: f32 = 12.0;

/// diff をインライン表示する。スクロールは呼び出し側の責務。
pub fn diff_ui(ui: &mut egui::Ui, theme: &Theme, files: &[FileDiff]) {
    let pal = DiffPalette::from_theme(theme);
    let size = 12.5;

    if files.is_empty() {
        ui.label(
            RichText::new("差分はありません")
                .color(theme.text_dim)
                .size(size),
        );
        return;
    }

    for (fi, file) in files.iter().enumerate() {
        let header = file_header_job(file, theme, size);
        egui::CollapsingHeader::new(header)
            .id_salt(("zv-diff-file", fi))
            .default_open(true)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                if file.is_binary {
                    ui.label(
                        RichText::new("バイナリファイル (差分表示なし)")
                            .color(theme.text_dim)
                            .size(size),
                    );
                    return;
                }
                if file.hunks.is_empty() {
                    let msg = if file.is_rename {
                        "リネームのみ (内容の変更なし)"
                    } else {
                        "変更行なし"
                    };
                    ui.label(RichText::new(msg).color(theme.text_dim).size(size));
                    return;
                }
                for hunk in &file.hunks {
                    hunk_header_ui(ui, theme, &pal, hunk, size);
                    for line in &hunk.lines {
                        diff_line_ui(ui, theme, &pal, line, size);
                    }
                }
            });
        ui.add_space(6.0);
    }
}

/// ファイル見出し: パス + `+追加 -削除`。
fn file_header_job(file: &FileDiff, theme: &Theme, size: f32) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let mut job = LayoutJob::default();
    let fmt = |color: Color32| TextFormat {
        font_id: FontId::monospace(size),
        color,
        ..Default::default()
    };
    job.append(&file.display_path(), 0.0, fmt(theme.text));
    if file.is_rename {
        job.append("  [renamed]", 0.0, fmt(theme.text_dim));
    }
    if file.is_binary {
        job.append("  [binary]", 0.0, fmt(theme.text_dim));
    } else {
        job.append(&format!("  +{}", file.additions), 0.0, fmt(theme.ok));
        job.append(&format!(" -{}", file.deletions), 0.0, fmt(theme.err));
    }
    job
}

/// ハンク見出し (`@@ ... @@`) — アクセント色の帯で本文と区別する。
fn hunk_header_ui(ui: &mut egui::Ui, theme: &Theme, pal: &DiffPalette, hunk: &Hunk, size: f32) {
    let w = ui.available_width();
    egui::Frame::none()
        .fill(pal.hunk_bg)
        .inner_margin(egui::Margin::symmetric(4.0, 2.0))
        .show(ui, |ui| {
            ui.set_min_width(w);
            ui.add(
                egui::Label::new(
                    RichText::new(&hunk.header)
                        .monospace()
                        .size(size)
                        .color(theme.accent),
                )
                .wrap_mode(egui::TextWrapMode::Extend),
            );
        });
}

/// 1 行分: [旧行番号][新行番号] +/- 本文。
fn diff_line_ui(ui: &mut egui::Ui, theme: &Theme, pal: &DiffPalette, line: &DiffLine, size: f32) {
    let (bg, sign_fg, sign) = match line.kind {
        LineKind::Added => (pal.add_bg, pal.add_fg, "+"),
        LineKind::Removed => (pal.del_bg, pal.del_fg, "-"),
        LineKind::Context => (theme.bg, theme.text_dim, " "),
    };
    let w = ui.available_width();
    egui::Frame::none().fill(bg).show(ui, |ui| {
        ui.set_min_width(w);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            gutter_cell(ui, theme, pal, line.old_no, size);
            gutter_cell(ui, theme, pal, line.new_no, size);
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(
                    RichText::new(sign).monospace().size(size).color(sign_fg),
                )
                .wrap_mode(egui::TextWrapMode::Extend),
            );
            ui.add_space(SIGN_W - 6.0);
            ui.add(
                egui::Label::new(
                    RichText::new(&line.text)
                        .monospace()
                        .size(size)
                        .color(theme.text),
                )
                .wrap_mode(egui::TextWrapMode::Extend),
            );
        });
    });
}

/// 行番号 1 列 (右寄せ)。番号が無い側は空欄。
fn gutter_cell(ui: &mut egui::Ui, theme: &Theme, pal: &DiffPalette, no: Option<usize>, size: f32) {
    let text = no.map(|n| n.to_string()).unwrap_or_default();
    let h = ui.spacing().interact_size.y;
    egui::Frame::none()
        .fill(pal.gutter_bg)
        .inner_margin(egui::Margin::symmetric(3.0, 0.0))
        .show(ui, |ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(GUTTER_COL_W, h),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(text)
                                .monospace()
                                .size(size * 0.9)
                                .color(theme.text_dim),
                        )
                        .wrap_mode(egui::TextWrapMode::Extend),
                    );
                },
            );
        });
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(h: &Hunk) -> Vec<LineKind> {
        h.lines.iter().map(|l| l.kind).collect()
    }

    fn nums(h: &Hunk) -> Vec<(Option<usize>, Option<usize>)> {
        h.lines.iter().map(|l| (l.old_no, l.new_no)).collect()
    }

    // ---- parse_range / parse_hunk_header ----

    #[test]
    fn range_with_count() {
        assert_eq!(parse_range("-10,3"), Some((10, 3)));
        assert_eq!(parse_range("+7,0"), Some((7, 0)));
    }

    #[test]
    fn range_without_count_defaults_to_one() {
        assert_eq!(parse_range("-5"), Some((5, 1)));
        assert_eq!(parse_range("+5"), Some((5, 1)));
    }

    #[test]
    fn range_rejects_garbage() {
        assert_eq!(parse_range("5,3"), None);
        assert_eq!(parse_range("-x,3"), None);
    }

    #[test]
    fn hunk_header_with_trailing_context() {
        let got = parse_hunk_header("@@ -1,4 +1,6 @@ fn main() {");
        assert_eq!(got, Some(((1, 4), (1, 6))));
    }

    #[test]
    fn hunk_header_omitted_counts() {
        assert_eq!(parse_hunk_header("@@ -3 +3 @@"), Some(((3, 1), (3, 1))));
    }

    #[test]
    fn hunk_header_rejects_non_hunk() {
        assert_eq!(parse_hunk_header("+++ b/foo.rs"), None);
        assert_eq!(parse_hunk_header("@@ broken @@"), None);
    }

    // ---- parse_unified ----

    #[test]
    fn empty_input_yields_no_files() {
        assert!(parse_unified("").is_empty());
        assert!(parse_unified("\n\n").is_empty());
    }

    #[test]
    fn simple_single_file() {
        let input = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 fn a() {}
-fn b() {}
+fn b2() {}
+fn c() {}
 fn d() {}
";
        let files = parse_unified(input);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.old_path, "src/lib.rs");
        assert_eq!(f.new_path, "src/lib.rs");
        assert_eq!(f.additions, 2);
        assert_eq!(f.deletions, 1);
        assert!(!f.is_binary && !f.is_rename);
        assert_eq!(f.hunks.len(), 1);
        let h = &f.hunks[0];
        assert_eq!(h.header, "@@ -1,3 +1,4 @@");
        assert_eq!((h.old_start, h.new_start), (1, 1));
        assert_eq!(
            kinds(h),
            vec![
                LineKind::Context,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Added,
                LineKind::Context
            ]
        );
        assert_eq!(
            nums(h),
            vec![
                (Some(1), Some(1)),
                (Some(2), None),
                (None, Some(2)),
                (None, Some(3)),
                (Some(3), Some(4)),
            ]
        );
        assert_eq!(h.lines[2].text, "fn b2() {}");
    }

    #[test]
    fn hunk_header_trailing_text_is_kept_not_parsed_as_line() {
        let input = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -10,2 +10,2 @@ impl Foo {
-    let x = 1;
+    let x = 2;
";
        let files = parse_unified(input);
        let h = &files[0].hunks[0];
        assert_eq!(h.header, "@@ -10,2 +10,2 @@ impl Foo {");
        assert_eq!((h.old_start, h.new_start), (10, 10));
        assert_eq!(h.lines.len(), 2);
        assert_eq!(h.lines[0].old_no, Some(10));
        assert_eq!(h.lines[1].new_no, Some(10));
    }

    #[test]
    fn omitted_counts_in_stream() {
        let input = "\
diff --git a/x b/x
--- a/x
+++ b/x
@@ -4 +4 @@
-old
+new
";
        let files = parse_unified(input);
        let h = &files[0].hunks[0];
        assert_eq!((h.old_start, h.new_start), (4, 4));
        assert_eq!(nums(h), vec![(Some(4), None), (None, Some(4))]);
        assert_eq!((files[0].additions, files[0].deletions), (1, 1));
    }

    #[test]
    fn multiple_hunks_track_line_numbers_independently() {
        let input = "\
diff --git a/m.rs b/m.rs
--- a/m.rs
+++ b/m.rs
@@ -1,2 +1,3 @@
 one
+two
 three
@@ -20,2 +21,2 @@
-alpha
+beta
 gamma
";
        let files = parse_unified(input);
        let f = &files[0];
        assert_eq!(f.hunks.len(), 2);
        assert_eq!(
            nums(&f.hunks[0]),
            vec![(Some(1), Some(1)), (None, Some(2)), (Some(2), Some(3))]
        );
        assert_eq!(
            nums(&f.hunks[1]),
            vec![(Some(20), None), (None, Some(21)), (Some(21), Some(22))]
        );
        assert_eq!((f.additions, f.deletions), (2, 1));
    }

    #[test]
    fn new_file_mode() {
        let input = "\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..3b18e51
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
";
        let files = parse_unified(input);
        let f = &files[0];
        assert_eq!(f.old_path, "/dev/null");
        assert_eq!(f.new_path, "new.txt");
        assert_eq!(f.display_path(), "new.txt");
        assert_eq!((f.additions, f.deletions), (2, 0));
        let h = &f.hunks[0];
        assert_eq!((h.old_start, h.new_start), (0, 1));
        assert_eq!(nums(h), vec![(None, Some(1)), (None, Some(2))]);
    }

    #[test]
    fn deleted_file_mode() {
        let input = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 3b18e51..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-hello
-world
";
        let files = parse_unified(input);
        let f = &files[0];
        assert_eq!(f.old_path, "gone.txt");
        assert_eq!(f.new_path, "/dev/null");
        assert_eq!(f.display_path(), "gone.txt");
        assert_eq!((f.additions, f.deletions), (0, 2));
        assert_eq!(nums(&f.hunks[0]), vec![(Some(1), None), (Some(2), None)]);
    }

    #[test]
    fn binary_file_is_flagged() {
        let input = "\
diff --git a/img.png b/img.png
index 1111111..2222222 100644
Binary files a/img.png and b/img.png differ
";
        let files = parse_unified(input);
        assert_eq!(files.len(), 1);
        assert!(files[0].is_binary);
        assert_eq!(files[0].new_path, "img.png");
        assert!(files[0].hunks.is_empty());
    }

    #[test]
    fn rename_without_hunks() {
        let input = "\
diff --git a/old/name.rs b/new/name.rs
similarity index 100%
rename from old/name.rs
rename to new/name.rs
";
        let files = parse_unified(input);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert!(f.is_rename);
        assert_eq!(f.old_path, "old/name.rs");
        assert_eq!(f.new_path, "new/name.rs");
        assert!(f.hunks.is_empty());
        assert_eq!(f.display_path(), "old/name.rs → new/name.rs");
    }

    #[test]
    fn rename_with_hunks() {
        let input = "\
diff --git a/a.rs b/b.rs
similarity index 88%
rename from a.rs
rename to b.rs
--- a/a.rs
+++ b/b.rs
@@ -1,2 +1,2 @@
 keep
-drop
+add
";
        let files = parse_unified(input);
        let f = &files[0];
        assert!(f.is_rename);
        assert_eq!((f.old_path.as_str(), f.new_path.as_str()), ("a.rs", "b.rs"));
        assert_eq!((f.additions, f.deletions), (1, 1));
        assert_eq!(f.hunks.len(), 1);
    }

    #[test]
    fn no_newline_marker_is_not_a_content_line() {
        let input = "\
diff --git a/n.txt b/n.txt
--- a/n.txt
+++ b/n.txt
@@ -1,2 +1,2 @@
 keep
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";
        let files = parse_unified(input);
        let f = &files[0];
        let h = &f.hunks[0];
        assert_eq!(h.lines.len(), 3);
        assert_eq!(
            kinds(h),
            vec![LineKind::Context, LineKind::Removed, LineKind::Added]
        );
        assert_eq!((f.additions, f.deletions), (1, 1));
        assert!(h.lines.iter().all(|l| !l.text.contains("No newline")));
    }

    #[test]
    fn trailing_no_newline_after_hunk_is_ignored() {
        let input = "\
diff --git a/t.txt b/t.txt
--- a/t.txt
+++ b/t.txt
@@ -1 +1 @@
-a
+b
\\ No newline at end of file
diff --git a/u.txt b/u.txt
--- a/u.txt
+++ b/u.txt
@@ -1 +1 @@
-c
+d
";
        let files = parse_unified(input);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].hunks[0].lines.len(), 2);
        assert_eq!(files[1].hunks[0].lines.len(), 2);
        assert_eq!(files[1].new_path, "u.txt");
    }

    #[test]
    fn empty_context_line_counts_as_context() {
        // git は空の文脈行を完全な空行として出すことがある。
        let input = "\
diff --git a/e.rs b/e.rs
--- a/e.rs
+++ b/e.rs
@@ -1,3 +1,3 @@
 fn a() {}

-old
+new
";
        let files = parse_unified(input);
        let h = &files[0].hunks[0];
        assert_eq!(h.lines.len(), 4);
        assert_eq!(h.lines[1].kind, LineKind::Context);
        assert_eq!(h.lines[1].text, "");
        assert_eq!((h.lines[1].old_no, h.lines[1].new_no), (Some(2), Some(2)));
    }

    #[test]
    fn plain_unified_without_git_header() {
        let input = "\
--- a/one.txt
+++ b/one.txt
@@ -1 +1 @@
-a
+b
--- a/two.txt
+++ b/two.txt
@@ -1 +1 @@
-c
+d
";
        let files = parse_unified(input);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path, "one.txt");
        assert_eq!(files[1].new_path, "two.txt");
    }

    #[test]
    fn side_paths_strip_timestamps() {
        assert_eq!(strip_side_prefix("a/src/x.rs\t2024-01-01 12:00"), "src/x.rs");
        assert_eq!(strip_side_prefix("/dev/null"), "/dev/null");
        assert_eq!(strip_side_prefix("b/plain.txt"), "plain.txt");
    }

    #[test]
    fn git_header_with_spaces_in_path() {
        let got = split_git_header("a/my dir/x.rs b/my dir/x.rs");
        assert_eq!(
            got,
            Some(("my dir/x.rs".to_string(), "my dir/x.rs".to_string()))
        );
    }

    #[test]
    fn removal_line_starting_with_dashes_inside_hunk() {
        // 本文が "--" で始まる削除行をファイルヘッダと誤認しないこと。
        let input = "\
diff --git a/s.sql b/s.sql
--- a/s.sql
+++ b/s.sql
@@ -1,2 +1,2 @@
---- old comment
+--- new comment
";
        let files = parse_unified(input);
        assert_eq!(files.len(), 1);
        let h = &files[0].hunks[0];
        assert_eq!(h.lines.len(), 2);
        assert_eq!(h.lines[0].kind, LineKind::Removed);
        assert_eq!(h.lines[0].text, "--- old comment");
        assert_eq!(h.lines[1].kind, LineKind::Added);
        assert_eq!(h.lines[1].text, "--- new comment");
    }

    #[test]
    fn realistic_multi_file_stream() {
        let input = "\
diff --git a/Cargo.toml b/Cargo.toml
index aaaaaaa..bbbbbbb 100644
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -8,6 +8,7 @@ edition = \"2021\"
 [dependencies]
 eframe = \"0.29\"
 egui = \"0.29\"
+serde = { version = \"1\", features = [\"derive\"] }
 anyhow = \"1\"
 dirs = \"5\"
 rfd = \"0.14\"
diff --git a/assets/logo.png b/assets/logo.png
index ccccccc..ddddddd 100644
Binary files a/assets/logo.png and b/assets/logo.png differ
diff --git a/src/old_name.rs b/src/new_name.rs
similarity index 97%
rename from src/old_name.rs
rename to src/new_name.rs
index eeeeeee..fffffff 100644
--- a/src/old_name.rs
+++ b/src/new_name.rs
@@ -1,5 +1,5 @@
-//! 旧モジュール
+//! 新モジュール

 pub fn run() {
     println!(\"hi\");
 }
diff --git a/src/dropped.rs b/src/dropped.rs
deleted file mode 100644
index 1234567..0000000
--- a/src/dropped.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-fn gone() {
-    // bye
-}
diff --git a/src/added.rs b/src/added.rs
new file mode 100644
index 0000000..7654321
--- /dev/null
+++ b/src/added.rs
@@ -0,0 +1,2 @@
+pub fn fresh() -> u32 { 42 }
+
";
        let files = parse_unified(input);
        assert_eq!(files.len(), 5);

        let toml = &files[0];
        assert_eq!(toml.new_path, "Cargo.toml");
        assert_eq!((toml.additions, toml.deletions), (1, 0));
        assert_eq!(toml.hunks[0].lines.len(), 7);
        assert_eq!(toml.hunks[0].header, "@@ -8,6 +8,7 @@ edition = \"2021\"");
        // 追加行は新側 11 行目 (8,9,10 が文脈)。
        let added = toml.hunks[0]
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Added)
            .unwrap();
        assert_eq!((added.old_no, added.new_no), (None, Some(11)));

        let png = &files[1];
        assert!(png.is_binary);
        assert_eq!(png.new_path, "assets/logo.png");

        let ren = &files[2];
        assert!(ren.is_rename);
        assert_eq!(ren.old_path, "src/old_name.rs");
        assert_eq!(ren.new_path, "src/new_name.rs");
        assert_eq!((ren.additions, ren.deletions), (1, 1));
        assert_eq!(ren.hunks[0].lines.len(), 6);

        let del = &files[3];
        assert_eq!(del.new_path, "/dev/null");
        assert_eq!((del.additions, del.deletions), (0, 3));

        let new = &files[4];
        assert_eq!(new.old_path, "/dev/null");
        assert_eq!((new.additions, new.deletions), (2, 0));
        assert_eq!(new.hunks[0].lines[1].text, "");
    }

    #[test]
    fn totals_across_stream() {
        let input = "\
diff --git a/a b/a
--- a/a
+++ b/a
@@ -1,2 +1,2 @@
-x
+y
diff --git a/b b/b
--- a/b
+++ b/b
@@ -1,1 +1,3 @@
 keep
+p
+q
";
        let files = parse_unified(input);
        let adds: usize = files.iter().map(|f| f.additions).sum();
        let dels: usize = files.iter().map(|f| f.deletions).sum();
        assert_eq!((adds, dels), (3, 1));
    }

    // ---- 描画ヘルパ ----

    #[test]
    fn mix_endpoints_and_midpoint() {
        let a = Color32::from_rgb(0, 0, 0);
        let b = Color32::from_rgb(100, 200, 50);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        assert_eq!(mix(a, b, 0.5), Color32::from_rgb(50, 100, 25));
        // 範囲外の t はクランプされる。
        assert_eq!(mix(a, b, 2.0), b);
        assert_eq!(mix(a, b, -1.0), a);
    }

    #[test]
    fn palette_readable_in_every_theme() {
        for t in crate::theme::all() {
            let p = DiffPalette::from_theme(&t);
            assert_ne!(p.add_bg, t.bg, "theme {} add_bg", t.name);
            assert_ne!(p.del_bg, t.bg, "theme {} del_bg", t.name);
            assert_ne!(p.add_bg, p.del_bg, "theme {} add/del", t.name);
            // 本文色 (theme.text) が着色背景に埋もれないこと。
            for bg in [p.add_bg, p.del_bg, p.hunk_bg, p.gutter_bg] {
                let d = |x: u8, y: u8| (x as i32 - y as i32).abs();
                let delta = d(t.text.r(), bg.r()) + d(t.text.g(), bg.g()) + d(t.text.b(), bg.b());
                assert!(delta > 120, "theme {} contrast too low: {}", t.name, delta);
            }
        }
    }
}
