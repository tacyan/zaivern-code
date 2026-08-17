//! 変更一覧 — **どのファイルの、どの行が変わったか**を 1 画面で一望する。
//!
//! デッキ / 看板と同じ「中央パネル全面のビュー」。トップバーの 🗒 で入る。
//!
//! ## なぜ専用のビューなのか
//!
//! これまで未コミットの変更へ辿り着く道は 3 つあった —
//! サイドバーの git パネル (幅が狭くて行番号まで出せない)、レビューパネル
//! (1 ファイルずつ)、まとめて開く (`open_changes_multibuffer`: 本文に混ぜて
//! 開くので「全体で何件か」が見えない)。**どれも「一望」ができない**。
//! ここは一望だけをやる — 探す・数える・飛ぶ。編集はエディタの仕事。
//!
//! ## 見せ方の約束 (CLAUDE.md の UI 原則)
//!
//! * **どの幅でも見切れない** — 行は `ui.available_width()` に必ず収める。
//!   狭いときは行番号の要約を落とし、パスは中身を省略してホバーで全文
//! * **空白は作らない** — 変更 0 件なら見出しごと出さず、中央に 1 枚のカード
//! * **画面が突然変わらない** — 展開は明示的な操作 (行のクリック) を待つ
//! * **git は UI スレッドで待たない** — 中身は
//!   `app::remote_api::changes_snapshot` の控え (裏スレッドが取り直す) を読む。
//!   ここは受け取った値を描くだけで、1 度も git を起こさない
//!
//! ## 「どこの行が」を一望させる
//!
//! ファイル名と ±件数だけでは「どこを触ったか」が分からない。ハンクの
//! 新しい側の行番号を連続域へ畳んで [`line_summary`] が **`行 120–124, 301`**
//! のように出す。畳まないと 1 ファイルで 40 個の数字が並び、かえって読めない。

use crate::i18n::{tr, trf};
use crate::theme::Theme;
use eframe::egui;
use egui::RichText;

/// 1 ファイルぶんの見せ方 (借り物だけで作る — 控えを複製しない)。
pub struct FileRow<'a> {
    pub rel: &'a str,
    /// `"M"|"A"|"D"|"R"|"?"`
    pub status: &'a str,
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
    /// 上限で切ったか (巨大ファイル)。
    pub truncated: bool,
    pub hunks: &'a [crate::diff::Hunk],
}

/// 画面が覚えておく状態 (どれを開いているか・絞り込み)。
#[derive(Default)]
pub struct ChangesState {
    /// パスの絞り込み (あいまい検索)。
    pub query: String,
    /// 展開しているファイル (相対パス)。
    open: std::collections::HashSet<String>,
    /// 直近で開いた行 (再入したときに位置を戻すため)。
    pub last_open: Option<(String, usize)>,
}

impl ChangesState {
    pub fn is_open(&self, rel: &str) -> bool {
        self.open.contains(rel)
    }
    pub fn toggle(&mut self, rel: &str) {
        if !self.open.remove(rel) {
            self.open.insert(rel.to_string());
        }
    }
    pub fn collapse_all(&mut self) {
        self.open.clear();
    }
    pub fn expand_all(&mut self, rows: &[FileRow<'_>]) {
        for r in rows {
            self.open.insert(r.rel.to_string());
        }
    }
    /// 開いている件数 (「全部畳む」を出すかの判断に使う)。
    pub fn open_count(&self) -> usize {
        self.open.len()
    }
}

/// 画面から出てくる要求。描画中に `self` を触らないための伝票。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// そのファイルの、その行を開く (1 始まり。0 = 行を指定しない)。
    Open { rel: String, line: usize },
    /// 変更を全部まとめて 1 枚のバッファへ開く (既存の multibuffer)。
    OpenAll,
    /// 控えを取り直す。
    Refresh,
}

/// 状態記号 → アイコン。**記号を画面へそのまま出さない**
/// (`?` だけ見せられても意味が伝わらない)。
pub fn status_icon(status: &str) -> &'static str {
    match status {
        "A" => "✚",
        "D" => "✖",
        "R" => "➜",
        "?" => "◇",
        _ => "●",
    }
}

/// 状態記号 → 画面に出す語 (ホバーで出す)。
pub fn status_label(status: &str) -> String {
    match status {
        "A" => tr("追加"),
        "D" => tr("削除"),
        "R" => tr("改名"),
        "?" => tr("追跡外"),
        _ => tr("変更"),
    }
}

/// 増減の表示。バイナリは行を数えられないので数字を出さない
/// (0 と出すと「変わっていない」に見える)。
pub fn stat_text(added: usize, removed: usize, binary: bool) -> String {
    if binary {
        return tr("バイナリ");
    }
    match (added, removed) {
        (0, 0) => String::new(),
        (a, 0) => format!("+{a}"),
        (0, r) => format!("−{r}"),
        (a, r) => format!("+{a} −{r}"),
    }
}

/// パスを (フォルダ, ファイル名) へ分ける。区切りは `/`
/// (git の出す相対パスは Windows でも `/` で来る)。
pub fn split_path(rel: &str) -> (&str, &str) {
    match rel.rfind('/') {
        Some(i) => (&rel[..=i], &rel[i + 1..]),
        None => ("", rel),
    }
}

/// ハンクから「新しい側で中身が変わった行」の連続域を作る。
///
/// 返すのは 1 始まりの `(開始, 終了)` の並び (両端を含む)。
/// **削除だけのハンク**は新しい側に行が無いので、消えた場所
/// (`new_start`) を 1 行の域として置く — 「触った場所」は消した場所でもある。
pub fn changed_ranges(hunks: &[crate::diff::Hunk]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut push = |n: usize| match out.last_mut() {
        Some(last) if n == last.1 + 1 => last.1 = n,
        Some(last) if n >= last.0 && n <= last.1 => {}
        _ => out.push((n, n)),
    };
    for h in hunks {
        let mut any = false;
        for l in &h.lines {
            match l.kind {
                crate::diff::LineKind::Added => {
                    if let Some(n) = l.new_no {
                        any = true;
                        push(n);
                    }
                }
                crate::diff::LineKind::Removed => {
                    // 消えた行は新しい側に番号を持たない。**消えた場所**を指す
                    any = true;
                    push(h.new_start.max(1));
                }
                _ => {}
            }
        }
        if !any && !h.lines.is_empty() {
            push(h.new_start.max(1));
        }
    }
    out.sort_unstable();
    out.dedup();
    // 並べ直したことで隣り合った域を畳む (削除の点が飛び込むため)
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(out.len());
    for (s, e) in out {
        match merged.last_mut() {
            Some(last) if s <= last.1 + 1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// 行の要約。`max` 個まで出して、残りは `+N` と畳む。
///
/// 畳まないと 1 ファイルで数十個の数字が並び、**一望のための行が
/// いちばん読みにくい行**になる。
pub fn line_summary(ranges: &[(usize, usize)], max: usize) -> String {
    if ranges.is_empty() || max == 0 {
        return String::new();
    }
    let shown = ranges.len().min(max);
    let mut s = String::new();
    for (i, (a, b)) in ranges[..shown].iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        if a == b {
            s.push_str(&a.to_string());
        } else {
            s.push_str(&format!("{a}–{b}"));
        }
    }
    let rest = ranges.len() - shown;
    if rest > 0 {
        s.push_str(&format!(" +{rest}"));
    }
    trf("行 {list}", &[("list", s)])
}

/// 可用幅から「行の要約をいくつ出すか」を決める (0 = 出さない)。
///
/// 狭い窓では**まずここを削る** — パスと増減は削れないため。
/// 表で固定してあるので、幅を変えても勝手に見切れない。
pub fn summary_slots(avail: f32) -> usize {
    if avail < 420.0 {
        0
    } else if avail < 620.0 {
        2
    } else if avail < 900.0 {
        4
    } else {
        6
    }
}

/// 絞り込み後に描く行の添字 (既存のあいまい検索を使う。新しい実装を書かない)。
pub fn visible_rows(rows: &[FileRow<'_>], query: &str) -> Vec<usize> {
    let q = query.trim();
    if q.is_empty() {
        return (0..rows.len()).collect();
    }
    let pq = crate::fuzzy::PreparedQuery::new(q);
    (0..rows.len())
        .filter(|&i| pq.score(rows[i].rel).is_some())
        .collect()
}

/// 1 フレームぶんの入力 (引数を数えないで済むよう 1 つに束ねる)。
pub struct View<'a> {
    pub rows: &'a [FileRow<'a>],
    pub added: usize,
    pub removed: usize,
    /// 変更が多くて上限で切ったか。
    pub truncated: bool,
    /// 控えの取得に失敗した理由 (git リポジトリでない等)。
    pub err: Option<&'a str>,
    /// まだ 1 度も取れていない。**空とは区別する** — 取れていないのに
    /// 「変更はありません」と出すのは嘘になる。
    pub pending: bool,
}

/// 変更一覧を描く。返り値は「やってほしいこと」だけ。
pub fn ui(st: &mut ChangesState, ui: &mut egui::Ui, theme: &Theme, v: &View<'_>) -> Vec<Action> {
    let rows = v.rows;
    let mut acts: Vec<Action> = Vec::new();
    let full_w = ui.available_width();
    header(st, ui, theme, v, &mut acts);
    ui.separator();

    if let Some(e) = v.err {
        mid_card(ui, theme, "⚠", e);
        return acts;
    }
    if rows.is_empty() {
        // 「まだ読んでいない」と「変更が無い」を混ぜない
        let (icon, msg) = if v.pending {
            ("⏳", tr("git を読んでいます…"))
        } else {
            ("✓", tr("未コミットの変更はありません"))
        };
        mid_card(ui, theme, icon, &msg);
        return acts;
    }
    let shown = visible_rows(rows, &st.query);
    if shown.is_empty() {
        mid_card(
            ui,
            theme,
            "🔎",
            &trf(
                "「{q}」に一致する変更ファイルはありません",
                &[("q", st.query.clone())],
            ),
        );
        return acts;
    }

    let slots = summary_slots(full_w);
    egui::ScrollArea::vertical()
        .id_salt("zv-changes-list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for &i in &shown {
                file_row(st, ui, theme, &rows[i], slots, &mut acts);
            }
        });
    acts
}

/// 見出し (件数・増減・絞り込み・操作)。狭いときはボタンをアイコンだけへ縮める。
fn header(
    st: &mut ChangesState,
    ui: &mut egui::Ui,
    theme: &Theme,
    v: &View<'_>,
    acts: &mut Vec<Action>,
) {
    let (rows, added, removed, truncated) = (v.rows, v.added, v.removed, v.truncated);
    let compact = ui.available_width() < 560.0;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(tr("🗒 変更一覧")).strong());
        ui.label(
            RichText::new(trf("{n} ファイル", &[("n", rows.len().to_string())]))
                .color(theme.text_dim),
        );
        if added > 0 {
            ui.label(RichText::new(format!("+{added}")).color(theme.ok));
        }
        if removed > 0 {
            ui.label(RichText::new(format!("−{removed}")).color(theme.err));
        }
        if truncated {
            ui.label(RichText::new(tr("(一部のみ)")).color(theme.warn))
                .on_hover_text(tr("変更が多いので上限で切りました"));
        }
        ui.separator();
        let w = (ui.available_width() - if compact { 120.0 } else { 260.0 }).clamp(80.0, 320.0);
        ui.add(
            egui::TextEdit::singleline(&mut st.query)
                .id_salt("zv-changes-filter")
                .hint_text(tr("🔎 パスで絞り込み"))
                .desired_width(w),
        );
        if !st.query.is_empty() && ui.small_button("✖").clicked() {
            st.query.clear();
        }
        // 全部畳む / 全部開く は**同じ場所で切り替える** (押せないボタンを並べない)
        if st.open_count() > 0 {
            if ui
                .button(if compact {
                    "⊟".to_string()
                } else {
                    tr("⊟ 全部畳む")
                })
                .on_hover_text(tr("開いているファイルを全部畳む"))
                .clicked()
            {
                st.collapse_all();
            }
        } else if ui
            .button(if compact {
                "⊞".to_string()
            } else {
                tr("⊞ 全部開く")
            })
            .on_hover_text(tr("すべてのファイルの差分を開く"))
            .clicked()
        {
            st.expand_all(rows);
        }
        if ui
            .button(if compact {
                "↻".to_string()
            } else {
                tr("↻ 取り直し")
            })
            .on_hover_text(tr("git から読み直す"))
            .clicked()
        {
            acts.push(Action::Refresh);
        }
        if !rows.is_empty()
            && ui
                .button(if compact {
                    "⧉".to_string()
                } else {
                    tr("⧉ まとめて開く")
                })
                .on_hover_text(tr("変更箇所を 1 枚のバッファへ集めて開く"))
                .clicked()
        {
            acts.push(Action::OpenAll);
        }
    });
}

/// 1 ファイルの行 (畳んだ状態) と、開いていればその下に差分。
fn file_row(
    st: &mut ChangesState,
    ui: &mut egui::Ui,
    theme: &Theme,
    row: &FileRow<'_>,
    slots: usize,
    acts: &mut Vec<Action>,
) {
    let open = st.is_open(row.rel);
    let ranges = changed_ranges(row.hunks);
    let (dir, name) = split_path(row.rel);
    let avail = ui.available_width();
    let resp = ui
        .horizontal(|ui| {
            ui.set_width(avail);
            ui.label(RichText::new(if open { "▾" } else { "▸" }).color(theme.text_dim));
            ui.label(
                RichText::new(status_icon(row.status)).color(match row.status {
                    "A" => theme.ok,
                    "D" => theme.err,
                    "?" => theme.text_dim,
                    _ => theme.accent,
                }),
            )
            .on_hover_text(status_label(row.status));
            if !dir.is_empty() {
                ui.label(RichText::new(dir).color(theme.text_dim));
            }
            ui.label(RichText::new(name).color(theme.text));
            let stat = stat_text(row.added, row.removed, row.binary);
            if !stat.is_empty() {
                ui.label(
                    RichText::new(stat)
                        .small()
                        .color(if row.removed > row.added {
                            theme.err
                        } else {
                            theme.ok
                        }),
                );
            }
            if slots > 0 && !ranges.is_empty() {
                let s = line_summary(&ranges, slots);
                ui.label(RichText::new(s).small().color(theme.text_dim))
                    .on_hover_text(line_summary(&ranges, ranges.len()));
            }
        })
        .response
        .interact(egui::Sense::click());
    if resp.clicked() {
        st.toggle(row.rel);
    }
    resp.on_hover_text(tr("クリックで差分を開閉 / 行をクリックでその場所へ飛ぶ"));

    if !open {
        return;
    }
    if row.binary {
        ui.label(
            RichText::new(tr("バイナリファイル — 差分は出せません"))
                .small()
                .color(theme.text_dim),
        );
        return;
    }
    if row.hunks.is_empty() {
        ui.label(
            RichText::new(tr("差分がありません (追跡外か、上限で切られています)"))
                .small()
                .color(theme.text_dim),
        );
    }
    for h in row.hunks {
        hunk_ui(ui, theme, row.rel, h, acts);
    }
    if row.truncated {
        ui.label(
            RichText::new(tr("… 上限で切りました"))
                .small()
                .color(theme.warn),
        );
    }
    ui.add_space(4.0);
}

/// 1 ハンク。行番号つきで、クリックするとその行を開く。
fn hunk_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    rel: &str,
    h: &crate::diff::Hunk,
    acts: &mut Vec<Action>,
) {
    let mono = egui::TextStyle::Monospace.resolve(ui.style());
    ui.label(
        RichText::new(&h.header)
            .font(mono.clone())
            .small()
            .color(theme.accent_soft),
    );
    for l in &h.lines {
        let (mark, col) = match l.kind {
            crate::diff::LineKind::Added => ('+', theme.ok),
            crate::diff::LineKind::Removed => ('-', theme.err),
            _ => (' ', theme.text_dim),
        };
        let no = l.new_no.or(l.old_no).unwrap_or(0);
        let text = format!("{no:>6} {mark} {}", l.text);
        let r = ui
            .add(
                egui::Label::new(RichText::new(text).font(mono.clone()).small().color(col))
                    .sense(egui::Sense::click()),
            )
            .on_hover_text(trf(
                "{rel}:{no} を開く",
                &[("rel", rel.to_string()), ("no", no.to_string())],
            ));
        if r.clicked() && no > 0 {
            acts.push(Action::Open {
                rel: rel.to_string(),
                line: no,
            });
        }
    }
}

/// 空状態・エラーは**利用可能領域の中央に 1 枚のカード**で (UI 原則)。
fn mid_card(ui: &mut egui::Ui, theme: &Theme, icon: &str, msg: &str) {
    let h = ui.available_height().max(60.0);
    ui.allocate_ui(egui::vec2(ui.available_width(), h), |ui| {
        ui.centered_and_justified(|ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(h * 0.5 - 34.0);
                ui.label(RichText::new(icon).size(28.0));
                ui.label(RichText::new(msg).color(theme.text_dim));
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffLine, Hunk, LineKind};

    fn line(kind: LineKind, old: Option<usize>, new: Option<usize>) -> DiffLine {
        DiffLine {
            kind,
            old_no: old,
            new_no: new,
            text: String::new(),
            no_newline: false,
            crlf: false,
        }
    }

    #[test]
    fn パスはフォルダと名前に分かれる() {
        assert_eq!(split_path("src/app/mod.rs"), ("src/app/", "mod.rs"));
        assert_eq!(split_path("README.md"), ("", "README.md"));
        assert_eq!(split_path("a/"), ("a/", ""));
    }

    #[test]
    fn 増減の表示はゼロとバイナリを区別する() {
        assert_eq!(stat_text(0, 0, false), "");
        assert_eq!(stat_text(3, 0, false), "+3");
        assert_eq!(stat_text(0, 4, false), "−4");
        assert_eq!(stat_text(3, 4, false), "+3 −4");
        // バイナリは行を数えられない。0 と出すと「変わっていない」に見える
        assert_ne!(stat_text(0, 0, true), "");
        assert_ne!(stat_text(9, 9, true), "+9 −9");
    }

    /// 変わった行は**連続域へ畳む**。畳まないと数字が数十個並ぶ。
    #[test]
    fn 変わった行は連続域へ畳まれる() {
        let h = Hunk {
            header: "@@ -10,3 +10,5 @@".into(),
            old_start: 10,
            new_start: 10,
            lines: vec![
                line(LineKind::Context, Some(10), Some(10)),
                line(LineKind::Added, None, Some(11)),
                line(LineKind::Added, None, Some(12)),
                line(LineKind::Context, Some(11), Some(13)),
                line(LineKind::Added, None, Some(14)),
            ],
        };
        assert_eq!(changed_ranges(&[h]), vec![(11, 12), (14, 14)]);
    }

    /// 削除だけのハンクは新しい側に行が無い。**消えた場所**を 1 行として置く
    /// (置かないと「触った場所」が一望から丸ごと消える)。
    #[test]
    fn 削除だけのハンクも場所を持つ() {
        let h = Hunk {
            header: "@@ -40,2 +40,0 @@".into(),
            old_start: 40,
            new_start: 40,
            lines: vec![
                line(LineKind::Removed, Some(40), None),
                line(LineKind::Removed, Some(41), None),
            ],
        };
        assert_eq!(changed_ranges(&[h]), vec![(40, 40)]);
    }

    #[test]
    fn 行の要約は上限で畳む() {
        let r = vec![(3, 3), (10, 14), (30, 30), (77, 80)];
        // 上限まで出して、残りは +N
        assert!(line_summary(&r, 2).contains("3, 10–14 +2"));
        assert!(line_summary(&r, 9).contains("3, 10–14, 30, 77–80"));
        assert!(!line_summary(&r, 9).contains('+'));
        assert_eq!(line_summary(&[], 4), "");
        assert_eq!(line_summary(&r, 0), "");
    }

    /// 狭い窓では要約を削る。**行が見切れるより、要約が無いほうがまし**。
    #[test]
    fn 狭い窓では行の要約から削る() {
        for (w, want) in [
            (320.0_f32, 0usize),
            (419.0, 0),
            (420.0, 2),
            (619.0, 2),
            (620.0, 4),
            (899.0, 4),
            (900.0, 6),
            (2000.0, 6),
        ] {
            assert_eq!(summary_slots(w), want, "avail={w}");
        }
    }

    #[test]
    fn 絞り込みは既存のあいまい検索を使う() {
        let hunks: Vec<Hunk> = Vec::new();
        let mk = |rel: &'static str| FileRow {
            rel,
            status: "M",
            added: 1,
            removed: 0,
            binary: false,
            truncated: false,
            hunks: &hunks,
        };
        let rows = vec![mk("src/app/mod.rs"), mk("docs/i18n.md")];
        assert_eq!(visible_rows(&rows, ""), vec![0, 1]);
        assert_eq!(visible_rows(&rows, "appmod"), vec![0]);
        assert_eq!(visible_rows(&rows, "i18"), vec![1]);
        assert!(visible_rows(&rows, "zzzz").is_empty());
        assert!(crate::fuzzy::score("appmod", "src/app/mod.rs").is_some());
    }

    /// 状態記号はそのまま出さない (`?` だけ見せられても意味が伝わらない)。
    #[test]
    fn 状態は記号ではなくアイコンと語で出す() {
        for s in ["M", "A", "D", "R", "?"] {
            assert!(!status_icon(s).is_empty());
            assert!(!status_label(s).is_empty());
            assert_ne!(status_icon(s), s);
        }
        // 追加と削除は見分けが付くこと
        assert_ne!(status_icon("A"), status_icon("D"));
    }
}
