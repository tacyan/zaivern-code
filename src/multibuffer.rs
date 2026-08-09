//! マルチバッファ — **複数ファイルの抜粋 (excerpt) を 1 本の面に並べる**。
//!
//! Zed の multibuffer 相当。検索ヒット・診断・作業ツリーの変更のように
//! 「ワークスペース中に散らばった注目点」を、ファイルを開いて回らずに
//! **前後の文脈込みで一望**するための面である。
//!
//! ここには **画面に触らない純粋関数とデータだけ** を置く
//! (レイアウト判断を純粋関数へ切り出してテーブルテストで固定する、という
//!  既存方針に合わせる)。描画は `app.rs` の `multibuffer_ui` が行う。
//!
//! ## 構造
//!
//! ```text
//! Multibuffer
//!  └ Excerpt (1 ファイルの連続した行範囲)
//!     ├ lines : その範囲の本文
//!     ├ focus : 注目行 (検索ヒット行 / 診断行 / 変更行) の絶対行番号
//!     ├ notes : 行に付く注記 (診断メッセージなど)
//!     └ marks : 行の中で強調するバイト範囲 (検索の一致箇所)
//! ```
//!
//! ## 行番号の単位
//!
//! **このモジュールの `line` は全部 1-based の絶対行番号** (エディタのガターに
//! 出る番号と同じ)。`Excerpt::lines` の添字だけが 0-based で、
//! `first_line + idx` が絶対行番号になる。LSP の 0-based とは違うので、
//! 診断から種を作るときは呼び出し側で +1 すること。
//!
//! ## なぜ「行だけ」で持つのか
//!
//! 抜粋を本文のバイト範囲で持つと、元ファイルが変わった瞬間に全部ずれる。
//! 行番号で持てば、ずれても「その行を開く」までは必ず正しく、開いた先の
//! エディタが本物の現在地を見せる。マルチバッファは**索引であって実体ではない**。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// データモデル
// ---------------------------------------------------------------------------

/// 抜粋の出所。タイトルと既定の文脈行数がここで決まる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// ワークスペース全体の検索ヒット。
    Search,
    /// 診断 (問題パネル) の全件。
    Problems,
    /// 作業ツリーの変更 (git diff) の全件。
    Changes,
}

impl Source {
    /// タブに出す見出し (翻訳前の原文)。
    pub fn title(self) -> &'static str {
        match self {
            Source::Search => "検索結果",
            Source::Problems => "問題",
            Source::Changes => "変更",
        }
    }

    /// 見出しの前に置く絵文字。
    pub fn icon(self) -> &'static str {
        match self {
            Source::Search => "🔎",
            Source::Problems => "⚠",
            Source::Changes => "±",
        }
    }

    /// 抜粋に付ける前後の文脈行数の既定値。
    ///
    /// 検索は「その行が読めれば十分」なので狭く、診断と変更は
    /// 周りを見ないと判断できないので広く取る。
    pub fn default_context(self) -> usize {
        match self {
            Source::Search => 2,
            Source::Problems => 3,
            Source::Changes => 3,
        }
    }
}

/// 行に付く注記 (診断メッセージなど)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// 1-based の絶対行番号。
    pub line: usize,
    pub text: String,
    /// 1=error 2=warning 3=information 4=hint。0 = 色付けなし。
    pub severity: u8,
}

/// 行の中で強調するバイト範囲 (検索の一致箇所)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    /// 1-based の絶対行番号。
    pub line: usize,
    /// その行の**バイト**開始位置 (`start < end`)。
    pub start: usize,
    pub end: usize,
}

/// 1 つの抜粋 = 1 ファイルの連続した行範囲。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Excerpt {
    pub path: PathBuf,
    /// 表示名 (ワークスペース相対。外なら絶対パス)。
    pub label: String,
    /// 抜粋の先頭行 (1-based)。
    pub first_line: usize,
    /// 抜粋の本文。`lines[i]` の絶対行番号は `first_line + i`。
    pub lines: Vec<String>,
    /// 注目行 (1-based, 昇順・重複なし)。
    pub focus: Vec<usize>,
    pub notes: Vec<Note>,
    pub marks: Vec<Mark>,
    /// 畳んでいるか (見出しだけを出す)。
    pub collapsed: bool,
}

impl Excerpt {
    /// 抜粋の末尾行 (1-based)。空なら `first_line`。
    pub fn last_line(&self) -> usize {
        self.first_line + self.lines.len().saturating_sub(1)
    }

    /// 絶対行番号 `line` の本文。範囲外なら `None`。
    pub fn line_text(&self, line: usize) -> Option<&str> {
        line.checked_sub(self.first_line)
            .and_then(|i| self.lines.get(i))
            .map(|s| s.as_str())
    }

    /// この抜粋で最も重い severity (小さいほど重い)。注記が無ければ `None`。
    pub fn worst_severity(&self) -> Option<u8> {
        self.notes
            .iter()
            .filter(|n| n.severity > 0)
            .map(|n| n.severity)
            .min()
    }
}

/// マルチバッファ本体。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Multibuffer {
    pub source: Source,
    /// 見出しに出す元の条件 (検索語など)。空でもよい。
    pub subtitle: String,
    pub excerpts: Vec<Excerpt>,
    /// 上限で切り落とした**種の件数** (0 なら全部入っている)。
    pub dropped: usize,
    /// 読めなかったファイル数 (削除済み・権限なし・巨大)。
    pub unreadable: usize,
}

impl Multibuffer {
    /// 注目行の総数 (「12 件」の 12)。
    pub fn focus_count(&self) -> usize {
        self.excerpts.iter().map(|e| e.focus.len()).sum()
    }

    /// 抜粋が 1 つも無いか。
    pub fn is_empty(&self) -> bool {
        self.excerpts.is_empty()
    }

    /// 全部畳む / 全部開く。
    pub fn set_all_collapsed(&mut self, collapsed: bool) {
        for e in &mut self.excerpts {
            e.collapsed = collapsed;
        }
    }

    /// 1 つでも開いているか (「全部畳む」ボタンの向きを決めるのに使う)。
    pub fn any_expanded(&self) -> bool {
        self.excerpts.iter().any(|e| !e.collapsed)
    }
}

// ---------------------------------------------------------------------------
// 行の平坦化 (描画用)
// ---------------------------------------------------------------------------

/// 描画する 1 行。`ScrollArea::show_rows` に渡すため **1 本の列にならす**
/// (可変長リストの中で `CollapsingHeader` を使うと ID の取り回しが要る)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// 抜粋の見出し (ファイル名 + 行範囲 + 折りたたみ)。
    Header { ex: usize },
    /// 本文 1 行。`idx` は `Excerpt::lines` の添字。
    Line { ex: usize, idx: usize },
    /// 注記 1 件。`note` は `Excerpt::notes` の添字。
    Note { ex: usize, note: usize },
    /// 抜粋の区切り (最後の抜粋の後には出さない)。
    Separator { ex: usize },
}

impl Row {
    /// この行が属する抜粋の添字。
    pub fn excerpt(self) -> usize {
        match self {
            Row::Header { ex } | Row::Line { ex, .. } | Row::Note { ex, .. } => ex,
            Row::Separator { ex } => ex,
        }
    }
}

/// マルチバッファ全体を描画行へ平坦化する。
///
/// 畳んだ抜粋は見出しだけになる。注記は**その行の直後**に置くので、
/// 「どの行の話か」を目で追わなくて済む。
pub fn rows(mb: &Multibuffer) -> Vec<Row> {
    let mut out = Vec::new();
    let last = mb.excerpts.len().saturating_sub(1);
    for (ex, e) in mb.excerpts.iter().enumerate() {
        out.push(Row::Header { ex });
        if !e.collapsed {
            // 行 → その行に付く注記の添字 (複数可)
            let mut by_line: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
            for (i, n) in e.notes.iter().enumerate() {
                by_line.entry(n.line).or_default().push(i);
            }
            for idx in 0..e.lines.len() {
                out.push(Row::Line { ex, idx });
                if let Some(ns) = by_line.get(&(e.first_line + idx)) {
                    for &note in ns {
                        out.push(Row::Note { ex, note });
                    }
                }
            }
        }
        if ex != last {
            out.push(Row::Separator { ex });
        }
    }
    out
}

/// 次 / 前の**注目行**へ動く。`from` は現在の行添字 (範囲外でもよい)。
///
/// 端では折り返さない (端に着いたことが分かるように `None` を返す)。
/// 畳んだ抜粋の中の注目行は飛ばす — 見えていない場所へ跳ぶと迷子になるため。
pub fn step_focus(rows: &[Row], mb: &Multibuffer, from: usize, forward: bool) -> Option<usize> {
    let is_focus = |r: &Row| match *r {
        Row::Line { ex, idx } => mb
            .excerpts
            .get(ex)
            .is_some_and(|e| e.focus.binary_search(&(e.first_line + idx)).is_ok()),
        _ => false,
    };
    if forward {
        rows.iter()
            .enumerate()
            .skip(from.saturating_add(1))
            .find(|(_, r)| is_focus(r))
            .map(|(i, _)| i)
    } else {
        rows.iter()
            .enumerate()
            .take(from.min(rows.len()))
            .rev()
            .find(|(_, r)| is_focus(r))
            .map(|(i, _)| i)
    }
}

/// 最初の注目行 (開いた直後のカーソル位置)。無ければ 0。
///
/// 行 0 は必ず見出し ([`rows`] が最初に積む) なので、`from = 0` からの
/// 前方走査で必ず先頭の注目行に当たる — 行 0 自身を取りこぼすことはない。
pub fn first_focus(rows: &[Row], mb: &Multibuffer) -> usize {
    step_focus(rows, mb, 0, true).unwrap_or(0)
}

/// 行添字 → 開くべき (パス, 1-based 行番号)。見出しは抜粋の先頭行を返す。
pub fn target_of(rows: &[Row], mb: &Multibuffer, row: usize) -> Option<(PathBuf, usize)> {
    let r = *rows.get(row)?;
    let e = mb.excerpts.get(r.excerpt())?;
    let line = match r {
        Row::Line { idx, .. } => e.first_line + idx,
        Row::Note { note, .. } => e.notes.get(note).map(|n| n.line).unwrap_or(e.first_line),
        Row::Header { .. } | Row::Separator { .. } => {
            // 見出しからは「最初の注目行」へ跳ぶ (先頭の文脈行ではない)
            e.focus.first().copied().unwrap_or(e.first_line)
        }
    };
    Some((e.path.clone(), line))
}

// ---------------------------------------------------------------------------
// 組み立て
// ---------------------------------------------------------------------------

/// マルチバッファの種 1 件 = 「このファイルのこの行に注目している」。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seed {
    pub path: PathBuf,
    /// 1-based の絶対行番号。0 を渡したら 1 に丸める。
    pub line: usize,
    /// 行に付ける注記 (診断メッセージなど)。空なら注記を作らない。
    pub note: String,
    /// 1=error 2=warning 3=information 4=hint。0 = 注記に色を付けない。
    pub severity: u8,
    /// 行の中で強調するバイト範囲。`None` なら行全体。
    pub mark: Option<(usize, usize)>,
}

impl Seed {
    /// 注記なしの種 (検索ヒット / 変更行)。
    pub fn plain(path: PathBuf, line: usize) -> Self {
        Self {
            path,
            line,
            note: String::new(),
            severity: 0,
            mark: None,
        }
    }
}

/// 組み立ての上限。**どれも「無制限」を持たない** — 種は数万件になりうるので、
/// 打ち切りは既定であって例外ではない (打ち切った件数は `dropped` に出す)。
#[derive(Clone, Copy, Debug)]
pub struct BuildOpts {
    /// 注目行の前後に付ける文脈行数。
    pub context: usize,
    /// 抜粋の最大数。
    pub max_excerpts: usize,
    /// 1 抜粋あたりの最大行数 (超えたら**後ろを切る** = 先頭側を残す)。
    pub max_lines: usize,
}

impl BuildOpts {
    /// 出所ごとの既定。
    pub fn for_source(source: Source) -> Self {
        Self {
            context: source.default_context(),
            max_excerpts: 300,
            max_lines: 200,
        }
    }
}

/// ワークスペース相対の表示名。`root` の外にあるものは絶対パスのまま返す。
pub fn label_for(path: &Path, root: Option<&Path>) -> String {
    root.and_then(|r| path.strip_prefix(r).ok())
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// 注目行の並び → 文脈込みの行範囲 (1-based, 両端含む)。
///
/// 隣り合う範囲は**間が 1 行以下なら繋ぐ** — 1 行だけ空けて 2 つの見出しを
/// 出すより、繋いだほうが読める。`total` は元ファイルの行数 (上限クランプ用)。
pub fn ranges(lines: &[usize], context: usize, total: usize) -> Vec<(usize, usize)> {
    let mut sorted: Vec<usize> = lines.iter().map(|&l| l.max(1).min(total.max(1))).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for l in sorted {
        let a = l.saturating_sub(context).max(1);
        let b = (l + context).min(total.max(1));
        match out.last_mut() {
            // 「間が 1 行以下」= 次の開始が前の終端 + 2 まで
            Some(prev) if a <= prev.1.saturating_add(2) => prev.1 = prev.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// 種の集合からマルチバッファを組み立てる。
///
/// `load` は「パス → 行の配列」。**IO を注入する**ので、この関数自体は
/// ファイルシステムに触らず、テストは実ファイルなしで全部書ける。
/// `load` が `None` を返したファイルは丸ごと落として `unreadable` に数える。
///
/// 種の順序は**入力順を保つ** (検索結果はファイル名順で来る、診断は深刻度順で
/// 来る、といった呼び出し側の並びを勝手に崩さない)。
pub fn build(
    source: Source,
    subtitle: &str,
    seeds: &[Seed],
    root: Option<&Path>,
    opts: BuildOpts,
    mut load: impl FnMut(&Path) -> Option<Vec<String>>,
) -> Multibuffer {
    // ファイルごとにまとめる (**最初に出てきた順**を保つ)
    let mut order: Vec<PathBuf> = Vec::new();
    let mut by_file: BTreeMap<PathBuf, Vec<&Seed>> = BTreeMap::new();
    for s in seeds {
        if !by_file.contains_key(&s.path) {
            order.push(s.path.clone());
        }
        by_file.entry(s.path.clone()).or_default().push(s);
    }

    let mut mb = Multibuffer {
        source,
        subtitle: subtitle.to_string(),
        excerpts: Vec::new(),
        dropped: 0,
        unreadable: 0,
    };

    for path in order {
        let group = by_file.get(&path).map(|v| v.as_slice()).unwrap_or(&[]);
        let Some(text) = load(&path) else {
            mb.unreadable += 1;
            continue;
        };
        let total = text.len();
        if total == 0 {
            mb.unreadable += 1;
            continue;
        }
        let label = label_for(&path, root);
        let focus_lines: Vec<usize> = group.iter().map(|s| s.line.max(1).min(total)).collect();
        for (a, b) in ranges(&focus_lines, opts.context, total) {
            if mb.excerpts.len() >= opts.max_excerpts {
                // この抜粋に載るはずだった種を数える
                mb.dropped += group
                    .iter()
                    .filter(|s| {
                        let l = s.line.max(1).min(total);
                        (a..=b).contains(&l)
                    })
                    .count();
                continue;
            }
            let b = b.min(a + opts.max_lines.saturating_sub(1));
            let lines: Vec<String> = text[a - 1..b].to_vec();
            let mut focus: Vec<usize> = Vec::new();
            let mut notes: Vec<Note> = Vec::new();
            let mut marks: Vec<Mark> = Vec::new();
            for s in group {
                let l = s.line.max(1).min(total);
                if !(a..=b).contains(&l) {
                    continue;
                }
                focus.push(l);
                if !s.note.is_empty() {
                    notes.push(Note {
                        line: l,
                        text: s.note.clone(),
                        severity: s.severity,
                    });
                }
                if let Some((ms, me)) = s.mark {
                    // 行の実長でクランプする (元行が変わっていても壊れない)
                    let len = text.get(l - 1).map(|t| t.len()).unwrap_or(0);
                    let (ms, me) = (ms.min(len), me.min(len));
                    if ms < me {
                        marks.push(Mark {
                            line: l,
                            start: ms,
                            end: me,
                        });
                    }
                }
            }
            focus.sort_unstable();
            focus.dedup();
            mb.excerpts.push(Excerpt {
                path: path.clone(),
                label: label.clone(),
                first_line: a,
                lines,
                focus,
                notes,
                marks,
                collapsed: false,
            });
        }
    }
    mb
}

/// テキストを行の配列へ (改行は落とす。CRLF の `\r` も落とす)。
///
/// `load` に渡す実装の共通部分。末尾改行で空行が 1 本増えないようにする。
pub fn split_lines(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    if v.len() > 1 && v.last().is_some_and(|l| l.is_empty()) {
        v.pop();
    }
    v
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str) -> PathBuf {
        // OS 依存のセパレータを避けるため、必ず PathBuf の join で組む
        PathBuf::from("proj").join(name)
    }

    fn text(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("line {i}")).collect()
    }

    #[test]
    fn 範囲は文脈を付けて重なりを繋ぐ() {
        // 単独
        assert_eq!(ranges(&[10], 2, 100), vec![(8, 12)]);
        // 重なる 2 つは 1 本になる
        assert_eq!(ranges(&[10, 12], 2, 100), vec![(8, 14)]);
        // 間が 1 行 (13..15 の 14 が空く) でも繋ぐ
        assert_eq!(ranges(&[10, 16], 2, 100), vec![(8, 18)]);
        // 間が 2 行以上なら分ける
        assert_eq!(ranges(&[10, 20], 2, 100), vec![(8, 12), (18, 22)]);
        // 先頭と末尾でクランプ
        assert_eq!(ranges(&[1], 3, 5), vec![(1, 4)]);
        assert_eq!(ranges(&[5], 3, 5), vec![(2, 5)]);
        // 順不同・重複も同じ結果
        assert_eq!(ranges(&[12, 10, 10], 2, 100), vec![(8, 14)]);
        // 空
        assert!(ranges(&[], 2, 100).is_empty());
    }

    #[test]
    fn 行数ゼロや行番号ゼロでも落ちない() {
        assert_eq!(ranges(&[0], 2, 0), vec![(1, 1)]);
        assert_eq!(ranges(&[999], 1, 3), vec![(2, 3)]);
    }

    #[test]
    fn 組み立てはファイルの初出順を保つ() {
        let seeds = vec![
            Seed::plain(p("b.rs"), 3),
            Seed::plain(p("a.rs"), 5),
            Seed::plain(p("b.rs"), 40),
        ];
        let mb = build(
            Source::Search,
            "foo",
            &seeds,
            Some(Path::new("proj")),
            BuildOpts::for_source(Source::Search),
            |_| Some(text(100)),
        );
        // b.rs が 2 抜粋 (3 行目付近と 40 行目付近)、その後に a.rs
        assert_eq!(mb.excerpts.len(), 3);
        assert_eq!(mb.excerpts[0].label, "b.rs");
        assert_eq!(mb.excerpts[0].first_line, 1);
        assert_eq!(mb.excerpts[1].label, "b.rs");
        assert_eq!(mb.excerpts[1].first_line, 38);
        assert_eq!(mb.excerpts[2].label, "a.rs");
        assert_eq!(mb.focus_count(), 3);
    }

    #[test]
    fn 読めないファイルは落として数える() {
        let seeds = vec![Seed::plain(p("gone.rs"), 1), Seed::plain(p("ok.rs"), 1)];
        let mb = build(
            Source::Problems,
            "",
            &seeds,
            None,
            BuildOpts::for_source(Source::Problems),
            |path| {
                if path.ends_with("gone.rs") {
                    None
                } else {
                    Some(text(10))
                }
            },
        );
        assert_eq!(mb.excerpts.len(), 1);
        assert_eq!(mb.unreadable, 1);
    }

    #[test]
    fn 抜粋の上限を超えたぶんは打ち切って数える() {
        let seeds: Vec<Seed> = (0..10)
            .map(|i| Seed::plain(p(&format!("f{i}.rs")), 1))
            .collect();
        let opts = BuildOpts {
            context: 0,
            max_excerpts: 3,
            max_lines: 200,
        };
        let mb = build(Source::Search, "", &seeds, None, opts, |_| Some(text(10)));
        assert_eq!(mb.excerpts.len(), 3);
        assert_eq!(mb.dropped, 7);
    }

    #[test]
    fn 一抜粋の行数上限で後ろを切る() {
        let seeds = vec![Seed::plain(p("a.rs"), 50)];
        let opts = BuildOpts {
            context: 20,
            max_excerpts: 10,
            max_lines: 5,
        };
        let mb = build(Source::Search, "", &seeds, None, opts, |_| Some(text(100)));
        let e = &mb.excerpts[0];
        assert_eq!(e.first_line, 30);
        assert_eq!(e.lines.len(), 5);
        assert_eq!(e.last_line(), 34);
    }

    #[test]
    fn 強調範囲は行の実長でクランプされる() {
        let seeds = vec![Seed {
            path: p("a.rs"),
            line: 2,
            note: String::new(),
            severity: 0,
            // "line 2" は 6 バイト。3..999 は 3..6 へ落ちる
            mark: Some((3, 999)),
        }];
        let mb = build(
            Source::Search,
            "",
            &seeds,
            None,
            BuildOpts::for_source(Source::Search),
            |_| Some(text(10)),
        );
        assert_eq!(
            mb.excerpts[0].marks,
            vec![Mark {
                line: 2,
                start: 3,
                end: 6
            }]
        );
    }

    #[test]
    fn 潰れた強調範囲は捨てる() {
        let seeds = vec![Seed {
            path: p("a.rs"),
            line: 2,
            note: String::new(),
            severity: 0,
            mark: Some((6, 6)),
        }];
        let mb = build(
            Source::Search,
            "",
            &seeds,
            None,
            BuildOpts::for_source(Source::Search),
            |_| Some(text(10)),
        );
        assert!(mb.excerpts[0].marks.is_empty());
    }

    fn sample() -> Multibuffer {
        let seeds = vec![
            Seed {
                path: p("a.rs"),
                line: 3,
                note: "unused".into(),
                severity: 2,
                mark: None,
            },
            Seed::plain(p("b.rs"), 2),
        ];
        build(
            Source::Problems,
            "",
            &seeds,
            Some(Path::new("proj")),
            BuildOpts {
                context: 1,
                max_excerpts: 10,
                max_lines: 100,
            },
            |_| Some(text(10)),
        )
    }

    #[test]
    fn 平坦化は見出しと本文と注記を一本の列にする() {
        let mb = sample();
        let rs = rows(&mb);
        // a.rs: 見出し + 2,3,4 行 + 3 行目の注記 + 区切り = 6
        // b.rs: 見出し + 1,2,3 行 = 4
        assert_eq!(rs.len(), 10);
        assert_eq!(rs[0], Row::Header { ex: 0 });
        assert_eq!(rs[1], Row::Line { ex: 0, idx: 0 });
        assert_eq!(rs[2], Row::Line { ex: 0, idx: 1 });
        assert_eq!(rs[3], Row::Note { ex: 0, note: 0 });
        assert_eq!(rs[4], Row::Line { ex: 0, idx: 2 });
        assert_eq!(rs[5], Row::Separator { ex: 0 });
        assert_eq!(rs[6], Row::Header { ex: 1 });
        // 最後の抜粋の後に区切りは出さない
        assert!(!matches!(rs.last(), Some(Row::Separator { .. })));
    }

    #[test]
    fn 畳んだ抜粋は見出しだけになる() {
        let mut mb = sample();
        mb.excerpts[0].collapsed = true;
        let rs = rows(&mb);
        assert_eq!(rs[0], Row::Header { ex: 0 });
        assert_eq!(rs[1], Row::Separator { ex: 0 });
        assert_eq!(rs[2], Row::Header { ex: 1 });
    }

    #[test]
    fn 注目行の前後移動は端で止まる() {
        let mb = sample();
        let rs = rows(&mb);
        let first = first_focus(&rs, &mb);
        // a.rs の 3 行目 = rs[2]
        assert_eq!(first, 2);
        let next = step_focus(&rs, &mb, first, true).expect("次がある");
        // b.rs の 2 行目 = 見出し(6) + 1 行目(7) + 2 行目(8)
        assert_eq!(next, 8);
        assert_eq!(step_focus(&rs, &mb, next, true), None);
        assert_eq!(step_focus(&rs, &mb, next, false), Some(first));
        assert_eq!(step_focus(&rs, &mb, first, false), None);
    }

    #[test]
    fn 畳んだ抜粋の注目行は飛ばす() {
        let mut mb = sample();
        mb.excerpts[0].collapsed = true;
        let rs = rows(&mb);
        // a.rs は見出しだけなので、最初の注目行は b.rs 側
        let f = first_focus(&rs, &mb);
        assert_eq!(rs[f], Row::Line { ex: 1, idx: 1 });
    }

    #[test]
    fn 行から開く先が引ける() {
        let mb = sample();
        let rs = rows(&mb);
        // 見出しからは「最初の注目行」へ
        assert_eq!(target_of(&rs, &mb, 0), Some((p("a.rs"), 3)));
        // 本文行からはその行へ
        assert_eq!(target_of(&rs, &mb, 1), Some((p("a.rs"), 2)));
        // 注記からは注記の行へ
        assert_eq!(target_of(&rs, &mb, 3), Some((p("a.rs"), 3)));
        // 範囲外
        assert_eq!(target_of(&rs, &mb, 999), None);
    }

    #[test]
    fn 全部畳むと全部開くが往復する() {
        let mut mb = sample();
        assert!(mb.any_expanded());
        mb.set_all_collapsed(true);
        assert!(!mb.any_expanded());
        mb.set_all_collapsed(false);
        assert!(mb.any_expanded());
    }

    #[test]
    fn 行分割は末尾改行で空行を増やさない() {
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines("a\r\nb\r\n"), vec!["a", "b"]);
        assert_eq!(split_lines("a\nb"), vec!["a", "b"]);
        assert_eq!(split_lines(""), vec![""]);
        // 本当に空行で終わるファイル (改行 2 つ) は空行を保つ
        assert_eq!(split_lines("a\n\n"), vec!["a", ""]);
    }

    #[test]
    fn 表示名はワークスペース相対になる() {
        // セパレータは OS で違うので、比較相手も必ず join で組む
        let root = PathBuf::from("proj");
        let inside = root.join("src").join("a.rs");
        assert_eq!(
            label_for(&inside, Some(&root)),
            PathBuf::from("src")
                .join("a.rs")
                .to_string_lossy()
                .into_owned()
        );
        // root の外は絶対パスのまま
        let out = PathBuf::from("other").join("a.rs");
        assert_eq!(
            label_for(&out, Some(&root)),
            out.to_string_lossy().into_owned()
        );
        assert_eq!(label_for(&out, None), out.to_string_lossy().into_owned());
    }

    #[test]
    fn 最も重い深刻度が取れる() {
        let mb = sample();
        assert_eq!(mb.excerpts[0].worst_severity(), Some(2));
        assert_eq!(mb.excerpts[1].worst_severity(), None);
    }

    #[test]
    fn 抜粋の本文が絶対行番号で引ける() {
        let mb = sample();
        let e = &mb.excerpts[0];
        assert_eq!(e.line_text(2), Some("line 2"));
        assert_eq!(e.line_text(3), Some("line 3"));
        assert_eq!(e.line_text(1), None);
        assert_eq!(e.line_text(99), None);
    }

    #[test]
    fn 出所ごとに文脈行数と見出しが決まる() {
        for s in [Source::Search, Source::Problems, Source::Changes] {
            assert!(!s.title().is_empty());
            assert!(!s.icon().is_empty());
            assert!(s.default_context() >= 1);
            assert_eq!(BuildOpts::for_source(s).context, s.default_context());
        }
    }
}
