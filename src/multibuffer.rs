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
    ///
    /// **ここが編集対象**。書き戻しはこの `lines` と [`Excerpt::orig_lines`] の
    /// 差分だけを元ファイルへ当てる (抜粋の外は 1 バイトも触らない)。
    pub lines: Vec<String>,
    /// 抜粋を作った時点の `lines` (編集前の原本)。差分の基準。
    pub orig_lines: Vec<String>,
    /// 抜粋を作った時点の**元ファイル全体**の内容キー ([`content_hash`])。
    ///
    /// 書き戻すとき、いまのファイルのキーと一致しなければ**その抜粋を拒否**する
    /// (マルチバッファを開いてから別のエージェントが同じファイルを
    ///  書き換えているのが、このアプリでは標準的な状況なので)。
    pub origin_hash: u64,
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

    /// 書き戻していない編集を抱えているか。
    pub fn edited(&self) -> bool {
        self.lines != self.orig_lines
    }

    /// 編集で変わった行数 (見出しのバッジ用)。
    ///
    /// 差分アルゴリズムではない — 「位置がずれた行」を数えるだけの粗い目安。
    /// 行数が変わったぶんは丸ごと数える。
    pub fn changed_lines(&self) -> usize {
        let common = self.lines.len().min(self.orig_lines.len());
        let diff = (0..common)
            .filter(|&i| self.lines[i] != self.orig_lines[i])
            .count();
        diff + self.lines.len().abs_diff(self.orig_lines.len())
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
    /// 行内編集の状態 `(抜粋の添字, 行の添字, 編集中の文字列)`。
    ///
    /// 編集途中の文字列を**ここに持つ**ので、描画側は `&mut String` を
    /// 抜粋から借りずに済む (借用が `&mut self` とぶつからない)。
    pub editing: Option<(usize, usize, String)>,
    /// 一括置換の入力欄 (強調範囲をまとめて置き換える)。
    pub replace_with: String,
    /// 書き戻しの取り消し履歴。**1 回の書き戻し = 1 段**。
    pub writebacks: Vec<WriteBack>,
}

impl Default for Multibuffer {
    fn default() -> Self {
        Self {
            source: Source::Search,
            subtitle: String::new(),
            excerpts: Vec::new(),
            dropped: 0,
            unreadable: 0,
            editing: None,
            replace_with: String::new(),
            writebacks: Vec::new(),
        }
    }
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

    /// 書き戻していない編集を抱えた行数 (0 なら書き戻すものが無い)。
    pub fn pending_lines(&self) -> usize {
        self.excerpts.iter().map(|e| e.changed_lines()).sum()
    }

    /// 編集を抱えたファイル数。
    pub fn pending_files(&self) -> usize {
        self.excerpts
            .iter()
            .filter(|e| e.edited())
            .map(|e| e.path.as_path())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    }

    /// 編集を全部捨てて開いた直後へ戻す (書き戻しはしない)。
    pub fn revert_edits(&mut self) {
        self.editing = None;
        for e in &mut self.excerpts {
            e.lines = e.orig_lines.clone();
        }
    }

    /// 編集前の姿 ([`diff_excerpts`] へ渡す `before`)。
    pub fn baseline(&self) -> Multibuffer {
        Multibuffer {
            source: self.source,
            subtitle: self.subtitle.clone(),
            excerpts: self
                .excerpts
                .iter()
                .map(|e| Excerpt {
                    lines: e.orig_lines.clone(),
                    ..e.clone()
                })
                .collect(),
            dropped: self.dropped,
            unreadable: self.unreadable,
            editing: None,
            replace_with: String::new(),
            writebacks: Vec::new(),
        }
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
        ..Multibuffer::default()
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
        // 書き戻しの「開いたときの元ファイル」の指紋。1 ファイル 1 回だけ。
        let origin_hash = content_hash(&text);
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
                orig_lines: lines.clone(),
                origin_hash,
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
// 書き戻し — 編集した抜粋を元のファイルへ戻す
// ---------------------------------------------------------------------------
//
// **ここが「まとめて読む」と「まとめて直す」を分ける部分**。
//
// 3 つの罠があり、どれも黙って壊れるので全部テストで固定してある:
//
//  1. **行番号のずれ。** 同じファイルに抜粋が複数あるとき、前の抜粋から
//     当てると 2 つ目以降の行番号が全部狂う。必ず**後ろの抜粋から**当てる
//     ([`diff_excerpts`] は `first_line` の降順で返し、[`apply_to_text`] は
//      降順でないものを [`ApplyError::Overlap`] で弾く)。
//  2. **抜粋の外を触る事故。** 見えていない場所が壊れるのが最悪なので、
//     置換は行の列へ分解した上で対象の範囲だけを差し替える。
//     改行様式 (LF / CRLF / 混在) と末尾改行の有無は元の行から引き継ぐ。
//  3. **開いてから書き戻すまでに元ファイルが変わる。** 並列エージェントを
//     走らせていれば普通に起こる。[`content_hash`] が一致しない抜粋は
//     **そのファイルだけ拒否**して理由を残す (全部を諦めない)。

/// ファイル本文の同一性キー。**行の内容だけ**を見る。
///
/// 改行様式と末尾改行はここでは無視する — 書き戻しは
/// [`apply_to_text`] がディスクの本文からそのまま引き継ぐので、
/// そこが変わっただけで書き戻しを止める理由が無い。
pub fn content_hash(lines: &[String]) -> u64 {
    let mut acc = crate::editor::hash_str("zv-multibuffer");
    for l in lines {
        acc = crate::editor::combine_hash(acc, crate::editor::hash_str(l));
    }
    crate::editor::combine_hash(acc, lines.len() as u64)
}

/// 元ファイル 1 本への行置換 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineEdit {
    /// 置き換える範囲の先頭行 (1-based, 含む)。
    pub first_line: usize,
    /// 置き換える**元の**行数。0 なら純粋な挿入。
    pub old_len: usize,
    /// 新しい行 (改行文字は含まない)。
    pub lines: Vec<String>,
    /// 出所の抜粋の添字 (拒否理由の表示に使う)。
    pub excerpt: usize,
}

/// 1 ファイルへの書き戻し。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEdit {
    pub path: PathBuf,
    pub label: String,
    /// 抜粋を作った時点の元ファイルの [`content_hash`]。
    pub origin_hash: u64,
    /// **`first_line` の降順**。前から当てると 2 つ目以降がずれる。
    pub edits: Vec<LineEdit>,
}

/// [`apply_to_text`] が当てられなかった理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// 置換範囲が今の本文の外 (元ファイルが縮んでいる)。
    OutOfRange {
        first_line: usize,
        old_len: usize,
        total: usize,
    },
    /// 置換が重なっている / `first_line` の降順に並んでいない。
    Overlap { first_line: usize },
}

impl ApplyError {
    /// 画面に出す理由 (翻訳前の原文を `trf` に通したもの)。
    pub fn reason(&self) -> String {
        match *self {
            ApplyError::OutOfRange {
                first_line, total, ..
            } => crate::i18n::trf(
                "{line} 行目は今のファイル ({total} 行) にありません",
                &[
                    ("line", first_line.to_string()),
                    ("total", total.to_string()),
                ],
            ),
            ApplyError::Overlap { first_line } => crate::i18n::trf(
                "{line} 行目の置換が重なっています",
                &[("line", first_line.to_string())],
            ),
        }
    }
}

/// 抜粋の編集結果を、元ファイルごとの「行範囲 → 新しい行」の集合へ畳む。
///
/// `before` / `after` は**同じ組み立て結果**であること (添字で対応させる)。
/// パスか `first_line` が食い違う抜粋は、対応が取れないので黙って落とす。
///
/// 返り値のファイルの並びは `after` の抜粋の初出順、各ファイルの
/// `edits` は **`first_line` の降順**。
pub fn diff_excerpts(before: &Multibuffer, after: &Multibuffer) -> Vec<FileEdit> {
    let mut order: Vec<PathBuf> = Vec::new();
    let mut by_file: BTreeMap<PathBuf, FileEdit> = BTreeMap::new();
    for (i, a) in after.excerpts.iter().enumerate() {
        let Some(b) = before.excerpts.get(i) else {
            continue;
        };
        if b.path != a.path || b.first_line != a.first_line || b.lines == a.lines {
            continue;
        }
        if !by_file.contains_key(&a.path) {
            order.push(a.path.clone());
            by_file.insert(
                a.path.clone(),
                FileEdit {
                    path: a.path.clone(),
                    label: a.label.clone(),
                    origin_hash: a.origin_hash,
                    edits: Vec::new(),
                },
            );
        }
        if let Some(fe) = by_file.get_mut(&a.path) {
            fe.edits.push(LineEdit {
                first_line: b.first_line,
                old_len: b.lines.len(),
                lines: a.lines.clone(),
                excerpt: i,
            });
        }
    }
    let mut out = Vec::new();
    for p in order {
        if let Some(mut fe) = by_file.remove(&p) {
            // **降順**。ここを昇順にすると 2 つ目以降の行番号が全部狂う。
            fe.edits.sort_by(|x, y| y.first_line.cmp(&x.first_line));
            out.push(fe);
        }
    }
    out
}

/// 本文を `(行, その行の改行)` の列へ分解する。
///
/// 改行は `"\r\n"` / `"\n"` / `""` (最終行に改行が無い) のいずれか。
/// **行数は [`split_lines`] と必ず一致する** (`行分割と行末分解は同じ行数`)。
fn split_rows(text: &str) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = Vec::new();
    let mut rest = text;
    loop {
        match rest.find('\n') {
            Some(p) => {
                let (line, after) = rest.split_at(p);
                let (line, term) = match line.strip_suffix('\r') {
                    Some(l) => (l, "\r\n"),
                    None => (line, "\n"),
                };
                out.push((line.to_string(), term));
                rest = &after[1..];
            }
            None => {
                if !rest.is_empty() || out.is_empty() {
                    out.push((rest.to_string(), ""));
                }
                break;
            }
        }
    }
    out
}

/// `FileEdit` の行置換を実ファイルの本文へ当てる (**行番号のずれを後ろから**)。
///
/// `edits` は `first_line` の降順であること。改行様式は元の行のものを
/// そのまま引き継ぐので、CRLF のファイルは CRLF のまま、混在は混在のまま、
/// 末尾改行の有無も変わらない。**置換範囲の外は 1 バイトも変わらない。**
pub fn apply_to_text(text: &str, edits: &[LineEdit]) -> Result<String, ApplyError> {
    let mut rows = split_rows(text);
    let total = rows.len();
    // ファイルとしての既定の改行 (最初に見つかった実物)。無ければ LF。
    let fallback = rows
        .iter()
        .map(|r| r.1)
        .find(|t| !t.is_empty())
        .unwrap_or("\n");
    // 直前に当てた編集の開始行。降順・非重複をここで強制する。
    let mut limit = usize::MAX;
    for e in edits {
        if e.first_line == 0 {
            return Err(ApplyError::OutOfRange {
                first_line: e.first_line,
                old_len: e.old_len,
                total,
            });
        }
        let start = e.first_line - 1;
        let end = match start.checked_add(e.old_len) {
            Some(v) if v <= rows.len() => v,
            _ => {
                return Err(ApplyError::OutOfRange {
                    first_line: e.first_line,
                    old_len: e.old_len,
                    total,
                })
            }
        };
        if e.first_line.saturating_add(e.old_len) > limit {
            return Err(ApplyError::Overlap {
                first_line: e.first_line,
            });
        }
        limit = e.first_line;

        let old_terms: Vec<&'static str> = rows[start..end].iter().map(|r| r.1).collect();
        // 足りないぶんに使う改行: 置き換えた行のもの → ファイルの既定。
        let fill = old_terms
            .iter()
            .copied()
            .find(|t| !t.is_empty())
            .unwrap_or(fallback);
        let new_rows: Vec<(String, &'static str)> = if e.old_len == 0 {
            // 純粋な挿入。既存の行の改行は 1 つも変えない。
            e.lines.iter().map(|l| (l.clone(), fill)).collect()
        } else {
            let last_term = old_terms.last().copied().unwrap_or(fill);
            let n = e.lines.len();
            e.lines
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    // 最後の行だけは**元の最終行の改行**を継ぐ
                    // (末尾改行の有無をここで保つ)。
                    let t = if i + 1 == n {
                        last_term
                    } else {
                        match old_terms.get(i) {
                            Some(t) if !t.is_empty() => *t,
                            _ => fill,
                        }
                    };
                    (l.clone(), t)
                })
                .collect()
        };
        rows.splice(start..end, new_rows);
    }
    let mut out = String::with_capacity(text.len());
    for (l, t) in &rows {
        out.push_str(l);
        out.push_str(t);
    }
    Ok(out)
}

/// 書き戻しを断る理由。**通せるものは通し、通せないものを名指しで残す**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reject {
    /// 今は読めない (消された / 権限が無い)。
    Unreadable,
    /// マルチバッファを開いてから元ファイルが変わっている。
    Changed,
    /// 他のインスタンスが保有している (リースの拒否理由をそのまま出す)。
    Leased(String),
    /// 行範囲が今の本文に当たらない。
    Apply(ApplyError),
}

impl Reject {
    /// 画面に出す理由。
    pub fn reason(&self) -> String {
        match self {
            Reject::Unreadable => crate::i18n::tr("読めません (消された / 権限が無い)"),
            Reject::Changed => crate::i18n::tr("開いたあとに変わっています — 開き直してください"),
            Reject::Leased(m) => m.clone(),
            Reject::Apply(e) => e.reason(),
        }
    }
}

/// 書き戻し 1 ファイルぶんの結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteItem {
    pub path: PathBuf,
    pub label: String,
    /// 書き戻す**前**の全文 (取り消しで戻すもの)。拒否したときは空。
    pub before: String,
    /// `Ok(書き戻した後の全文)` / `Err(拒否理由)`。
    pub outcome: Result<String, Reject>,
}

/// 書き戻しの計画 (まだ 1 バイトも書いていない)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WritePlan {
    pub items: Vec<WriteItem>,
}

/// 編集した抜粋 → 「どのファイルへ何を書くか」。**I/O は注入する**ので、
/// この関数自体はファイルにもリース台帳にも触らない。
///
/// - `read` はそのファイルの**いまの本文** (エディタで開いていれば未保存の本文)。
///   `None` なら [`Reject::Unreadable`]。
/// - `lease` は保有者による拒否理由。通るなら `None`。
///
/// 判定の順番は 読める → 変わっていない → 自分が書ける → 行が当たる。
/// **1 ファイルでも落ちたら全部やめる、はしない** — 通せるものは通す。
pub fn plan_writeback(
    before: &Multibuffer,
    after: &Multibuffer,
    mut read: impl FnMut(&Path) -> Option<String>,
    mut lease: impl FnMut(&Path) -> Option<String>,
) -> WritePlan {
    let mut plan = WritePlan::default();
    for fe in diff_excerpts(before, after) {
        let Some(cur) = read(&fe.path) else {
            plan.items.push(WriteItem {
                path: fe.path,
                label: fe.label,
                before: String::new(),
                outcome: Err(Reject::Unreadable),
            });
            continue;
        };
        let outcome = if content_hash(&split_lines(&cur)) != fe.origin_hash {
            Err(Reject::Changed)
        } else if let Some(msg) = lease(&fe.path) {
            Err(Reject::Leased(msg))
        } else {
            apply_to_text(&cur, &fe.edits).map_err(Reject::Apply)
        };
        plan.items.push(WriteItem {
            path: fe.path,
            label: fe.label,
            before: if outcome.is_ok() { cur } else { String::new() },
            outcome,
        });
    }
    plan
}

/// 書き戻し 1 回ぶん = **取り消しの 1 段**。
///
/// ファイルごとにバラバラに戻ると使えないので、1 回の書き戻しで触った
/// ファイルと抜粋をまとめて 1 段にする。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteBack {
    /// `(パス, 書き戻す前の全文, 書き戻した後の全文)`。
    pub files: Vec<(PathBuf, String, String)>,
    /// 書き戻す前の抜粋 `(添字, first_line, orig_lines, origin_hash)`。
    pub excerpts: Vec<(usize, usize, Vec<String>, u64)>,
}

impl WriteBack {
    /// 取り消し情報として抱えている本文の合計バイト数。
    pub fn bytes(&self) -> usize {
        self.files
            .iter()
            .map(|(_, a, b)| a.len() + b.len())
            .sum::<usize>()
    }
}

/// 書き戻しが通ったファイルについて、抜粋を「書き戻した後」の姿へ揃える。
///
/// - `orig_lines` を `lines` に合わせる (もう差分ではない)
/// - `origin_hash` を新しい本文のものへ更新する (続けて 2 回目が撃てる)
/// - **行数が変わったぶん、同じファイルの後ろの抜粋の行番号をずらす**
///
/// 戻り値は取り消し用の抜粋スナップショット。
pub fn settle_file(
    mb: &mut Multibuffer,
    path: &Path,
    new_lines: &[String],
) -> Vec<(usize, usize, Vec<String>, u64)> {
    let new_hash = content_hash(new_lines);
    let mut snap = Vec::new();
    let mut delta: isize = 0;
    for (i, e) in mb.excerpts.iter_mut().enumerate() {
        if e.path != path {
            continue;
        }
        snap.push((i, e.first_line, e.orig_lines.clone(), e.origin_hash));
        if delta != 0 {
            let shift = |l: usize| -> usize { l.saturating_add_signed(delta).max(1) };
            e.first_line = shift(e.first_line);
            for f in &mut e.focus {
                *f = shift(*f);
            }
            for n in &mut e.notes {
                n.line = shift(n.line);
            }
            for m in &mut e.marks {
                m.line = shift(m.line);
            }
        }
        delta += e.lines.len() as isize - e.orig_lines.len() as isize;
        e.orig_lines = e.lines.clone();
        e.origin_hash = new_hash;
    }
    snap
}

/// 取り消しで抜粋を書き戻し前の姿へ戻す ([`settle_file`] の逆)。
pub fn restore_excerpts(mb: &mut Multibuffer, snap: &[(usize, usize, Vec<String>, u64)]) {
    for (i, first_line, orig, hash) in snap {
        let Some(e) = mb.excerpts.get_mut(*i) else {
            continue;
        };
        let delta = *first_line as isize - e.first_line as isize;
        if delta != 0 {
            let shift = |l: usize| -> usize { l.saturating_add_signed(delta).max(1) };
            e.first_line = *first_line;
            for f in &mut e.focus {
                *f = shift(*f);
            }
            for n in &mut e.notes {
                n.line = shift(n.line);
            }
            for m in &mut e.marks {
                m.line = shift(m.line);
            }
        }
        e.orig_lines = orig.clone();
        e.origin_hash = *hash;
    }
}

/// 強調範囲 (`marks`) を一括で置き換える。返り値は置き換えた件数。
///
/// 「検索結果を 1 面に集めて、その場で全部書き換える」の**一括側**。
/// 1 行に複数の一致があるので、行を左から組み直しながら `marks` の位置も
/// 作り直す (置換後の位置を指すので、続けてもう一度置換しても壊れない)。
pub fn replace_marks(mb: &mut Multibuffer, to: &str) -> usize {
    let mut n = 0usize;
    for e in &mut mb.excerpts {
        if e.marks.is_empty() {
            continue;
        }
        // 行ごとに、昇順の (start, end) を集める
        let mut by_line: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (i, m) in e.marks.iter().enumerate() {
            by_line.entry(m.line).or_default().push(i);
        }
        for (line, mut idxs) in by_line {
            let Some(row) = line.checked_sub(e.first_line) else {
                continue;
            };
            let Some(src) = e.lines.get(row).cloned() else {
                continue;
            };
            idxs.sort_by_key(|&i| e.marks[i].start);
            let mut out = String::with_capacity(src.len());
            let mut cur = 0usize;
            let mut placed: Vec<(usize, usize, usize)> = Vec::new();
            for i in idxs {
                let (s, t) = (e.marks[i].start, e.marks[i].end);
                if s < cur || t > src.len() || !src.is_char_boundary(s) || !src.is_char_boundary(t)
                {
                    continue;
                }
                out.push_str(&src[cur..s]);
                let ns = out.len();
                out.push_str(to);
                placed.push((i, ns, out.len()));
                cur = t;
                n += 1;
            }
            if placed.is_empty() {
                continue;
            }
            out.push_str(&src[cur..]);
            e.lines[row] = out;
            for (i, s, t) in placed {
                e.marks[i].start = s;
                e.marks[i].end = t;
            }
        }
    }
    if n > 0 {
        mb.editing = None;
    }
    n
}

// ---------------------------------------------------------------------------
// 行内編集と見出し行の割り付け (どちらも描画に触らない)
// ---------------------------------------------------------------------------

/// 行内編集を抜粋へ確定する。1 行でも変わったら `true`。
///
/// 改行を含む文字列は**複数行へ広げる** (貼り付けで行が増える経路)。
/// 行数が変わると同じファイルの後ろの抜粋の行番号がずれるが、書き戻しは
/// [`diff_excerpts`] が降順に畳んでから当てるので狂わない。
pub fn commit_line_edit(mb: &mut Multibuffer, st: Option<(usize, usize, String)>) -> bool {
    let Some((ex, idx, text)) = st else {
        return false;
    };
    let Some(e) = mb.excerpts.get_mut(ex) else {
        return false;
    };
    if idx >= e.lines.len() {
        return false;
    }
    let new = split_lines(&text);
    if e.lines[idx..idx + 1] == new[..] {
        return false;
    }
    e.lines.splice(idx..idx + 1, new);
    true
}

/// 見出し行の割り付け。**可用幅から「何を出すか」だけを決める純粋関数**。
///
/// どの幅でも見切れないよう、入り切らない順に
/// 「一括置換の入力欄 → ボタンの文字ラベル」を落とす。
#[derive(Clone, Debug, PartialEq)]
pub struct Head {
    /// 一括置換の入力欄の幅。`0.0` なら出さない。
    pub replace_w: f32,
    /// ボタンに文字ラベルを付けるか (偽ならアイコンだけ)。
    pub labels: bool,
    /// 左から並べた部品の幅。**合計は可用幅以下**。
    ///
    /// ただしアイコンだけへ縮退してもボタンが入り切らない極端に狭い幅では、
    /// これ以上縮められないのでアイコン幅をそのまま返す (描画側は
    /// `horizontal_wrapped` なので折り返して見切れない)。
    /// そのときは `labels == false` かつ `replace_w == 0.0` になる。
    pub widths: Vec<f32>,
}

/// 見出し行に常に出るボタン (全部畳む / 前へ / 次へ) の数。
const HEAD_FIXED_BUTTONS: usize = 3;

/// 見出し行の割り付けを決める。
///
/// - `avail` 利用可能幅 (px) / `glyph` 等幅 1 文字の幅 (px)
/// - `has_marks` 一括置換の対象 (検索の一致) があるか
/// - `pending` 書き戻していない編集があるか (「書き戻す」「捨てる」を出す)
/// - `undoable` 取り消せる書き戻しがあるか (「↩」を出す)
pub fn head_layout(avail: f32, glyph: f32, has_marks: bool, pending: bool, undoable: bool) -> Head {
    let glyph = glyph.max(1.0);
    let avail = avail.max(glyph);
    let icon_w = glyph * 3.0;
    // 文字ラベル付きのボタン幅 (原文の文字数 + 余白 2 文字ぶん)。
    let label_w = |chars: usize| glyph * (chars as f32 + 2.0);
    let mut n_icons = HEAD_FIXED_BUTTONS;
    if undoable {
        n_icons += 1;
    }
    let extra_labels: &[usize] = if pending { &[4, 6] } else { &[] };
    let icons_total = icon_w * n_icons as f32;
    let labels_total: f32 = extra_labels.iter().map(|&c| label_w(c)).sum();
    let icons_only_total: f32 = icon_w * extra_labels.len() as f32;

    let info_min = glyph * 8.0;
    let replace_pref = if has_marks { glyph * 20.0 } else { 0.0 };
    let replace_min = if has_marks { glyph * 10.0 } else { 0.0 };

    // 1) 文字ラベル付きで入るか
    let mut labels = info_min + replace_pref + icons_total + labels_total <= avail;
    let mut buttons = icons_total
        + if labels {
            labels_total
        } else {
            icons_only_total
        };
    if !labels {
        // 2) アイコンだけにして入るか
        buttons = icons_total + icons_only_total;
    }
    let mut replace_w = if !has_marks {
        0.0
    } else if info_min + replace_pref + buttons <= avail {
        replace_pref
    } else {
        let left = avail - info_min - buttons;
        if left >= replace_min {
            left
        } else {
            0.0
        }
    };
    // 3) それでも入らないなら置換欄を落とす → ラベルも落とす
    if info_min + replace_w + buttons > avail {
        replace_w = 0.0;
        labels = false;
        buttons = icons_total + icons_only_total;
    }
    let info_w = (avail - replace_w - buttons).max(0.0);

    let mut widths = vec![info_w];
    if replace_w > 0.0 {
        widths.push(replace_w);
    }
    for &c in extra_labels {
        widths.push(if labels { label_w(c) } else { icon_w });
    }
    for _ in 0..n_icons {
        widths.push(icon_w);
    }
    Head {
        replace_w,
        labels,
        widths,
    }
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

    // ── 書き戻し ───────────────────────────────────────────────────
    //
    // ここが「まとめて読む」と「まとめて直す」を分ける部分なので、
    // 罠を 1 つずつ名前の付いたテストで固定する。

    /// `n` 行のファイル本文 (`L1` … `Ln`)。末尾改行あり。
    fn file_text(n: usize) -> String {
        (1..=n).map(|i| format!("L{i}\n")).collect()
    }

    /// 1 ファイル・複数抜粋のマルチバッファを組み立てる。
    fn mb_of(path: &str, lines: &[usize], total: usize, context: usize) -> Multibuffer {
        let seeds: Vec<Seed> = lines.iter().map(|&l| Seed::plain(p(path), l)).collect();
        build(
            Source::Search,
            "",
            &seeds,
            None,
            BuildOpts {
                context,
                max_excerpts: 50,
                max_lines: 100,
            },
            |_| Some(split_lines(&file_text(total))),
        )
    }

    #[test]
    fn 行分割と行末分解は同じ行数() {
        for t in [
            "",
            "\n",
            "a",
            "a\n",
            "a\nb",
            "a\nb\n",
            "a\r\nb\r\n",
            "a\r\nb\nc",
            "\n\n\n",
        ] {
            let rows = split_rows(t);
            assert_eq!(
                rows.len(),
                split_lines(t).len(),
                "{t:?} の行数が split_lines と食い違う"
            );
            let back: String = rows.iter().map(|(l, m)| format!("{l}{m}")).collect();
            assert_eq!(back, t, "{t:?} は分解して戻すと同じ本文になる");
        }
    }

    #[test]
    fn 一ファイル三抜粋で全部行数が変わっても行番号がずれない() {
        // 40 行のファイルに 5 / 20 / 35 行目の 3 抜粋 (文脈 1 行)。
        let text = file_text(40);
        let before = mb_of("a.rs", &[5, 20, 35], 40, 1);
        assert_eq!(before.excerpts.len(), 3, "抜粋は 3 つ");
        let mut after = before.clone();
        // 1 つ目は 1 行減らし、2 つ目は行を増やし、3 つ目は書き換える。
        after.excerpts[0].lines = vec!["X4".into(), "X5".into()];
        after.excerpts[1].lines = vec![
            "Y19".into(),
            "Y20a".into(),
            "Y20b".into(),
            "Y20c".into(),
            "Y21".into(),
        ];
        after.excerpts[2].lines = vec!["Z34".into()];

        let fes = diff_excerpts(&before, &after);
        assert_eq!(fes.len(), 1, "ファイルは 1 本にまとまる");
        let starts: Vec<usize> = fes[0].edits.iter().map(|e| e.first_line).collect();
        assert_eq!(starts, vec![34, 19, 4], "**降順**で返る (後ろから当てる)");

        let out = apply_to_text(&text, &fes[0].edits).expect("当たる");
        let got = split_lines(&out);
        // 期待: 1-3 そのまま / 4-5 が X / 6-18 そのまま / 19-21 が Y / 22-33 そのまま
        //       / 34-36 が Z (3 行 → 1 行) / 37-40 そのまま
        let mut want: Vec<String> = (1..=3).map(|i| format!("L{i}")).collect();
        want.extend(["X4".to_string(), "X5".to_string()]);
        want.extend((7..=18).map(|i| format!("L{i}")));
        want.extend(
            ["Y19", "Y20a", "Y20b", "Y20c", "Y21"]
                .iter()
                .map(|s| s.to_string()),
        );
        want.extend((22..=33).map(|i| format!("L{i}")));
        want.push("Z34".to_string());
        want.extend((37..=40).map(|i| format!("L{i}")));
        assert_eq!(got, want);
    }

    #[test]
    fn 抜粋の外は一バイトも変えない() {
        // 20 行のうち 10 行目だけを直す。前後は**バイト列として**同一。
        let text = file_text(20);
        let before = mb_of("a.rs", &[10], 20, 0);
        let mut after = before.clone();
        after.excerpts[0].lines = vec!["まったく違う行".into()];
        let fes = diff_excerpts(&before, &after);
        let out = apply_to_text(&text, &fes[0].edits).expect("当たる");
        let head: String = (1..=9).map(|i| format!("L{i}\n")).collect();
        let tail: String = (11..=20).map(|i| format!("L{i}\n")).collect();
        assert_eq!(&out[..head.len()], head, "前は 1 バイトも変わらない");
        assert!(out.ends_with(&tail), "後ろは 1 バイトも変わらない");
        assert_eq!(out, format!("{head}まったく違う行\n{tail}"));
    }

    #[test]
    fn 改行様式と末尾改行を保つ() {
        // (元の本文, 直す行 (1-based), 新しい行, 期待)
        let cases: &[(&str, usize, &[&str], &str)] = &[
            // CRLF のファイルは CRLF のまま
            ("a\r\nb\r\nc\r\n", 2, &["B"], "a\r\nB\r\nc\r\n"),
            // 末尾改行が無いファイルは無いまま
            ("a\nb\nc", 3, &["C"], "a\nb\nC"),
            // 混在は**各行のもの**を保つ
            ("a\r\nb\nc\r\n", 2, &["B"], "a\r\nB\nc\r\n"),
            // 最終行 (改行なし) を 3 行へ広げても、末尾は改行なしのまま
            ("a\nb", 2, &["x", "y", "z"], "a\nx\ny\nz"),
            // CRLF のファイルで行を増やすと、増えた行も CRLF
            ("a\r\nb\r\n", 1, &["p", "q"], "p\r\nq\r\nb\r\n"),
            // 3 行を 1 行へ畳んでも末尾改行の有無は元の最終行に従う
            ("a\nb\nc", 1, &["only"], "only"),
        ];
        for (text, line, new, want) in cases {
            let e = LineEdit {
                first_line: *line,
                old_len: split_lines(text).len() + 1 - *line,
                lines: new.iter().map(|s| s.to_string()).collect(),
                excerpt: 0,
            };
            // old_len は「その行から最後まで」にすると畳みの検証になるので、
            // 単一行の置換だけ old_len=1 に戻す。
            let e = if new.len() == 1 && *line != 1 {
                LineEdit { old_len: 1, ..e }
            } else if *text == "a\r\nb\r\n" {
                LineEdit { old_len: 1, ..e }
            } else {
                e
            };
            assert_eq!(
                apply_to_text(text, &[e]).expect("当たる"),
                *want,
                "{text:?} の {line} 行目"
            );
        }
    }

    #[test]
    fn 降順でない置換と範囲外は弾く() {
        let text = file_text(10);
        // 昇順に並べると 2 件目で重なり判定に落ちる
        let asc = vec![
            LineEdit {
                first_line: 2,
                old_len: 2,
                lines: vec!["x".into()],
                excerpt: 0,
            },
            LineEdit {
                first_line: 6,
                old_len: 1,
                lines: vec!["y".into()],
                excerpt: 1,
            },
        ];
        assert_eq!(
            apply_to_text(&text, &asc),
            Err(ApplyError::Overlap { first_line: 6 })
        );
        // 重なり (降順でも 4..6 と 5..6 は重なる)
        let ov = vec![
            LineEdit {
                first_line: 5,
                old_len: 2,
                lines: vec!["x".into()],
                excerpt: 0,
            },
            LineEdit {
                first_line: 4,
                old_len: 2,
                lines: vec!["y".into()],
                excerpt: 1,
            },
        ];
        assert_eq!(
            apply_to_text(&text, &ov),
            Err(ApplyError::Overlap { first_line: 4 })
        );
        // 元ファイルが縮んでいる
        let far = vec![LineEdit {
            first_line: 30,
            old_len: 1,
            lines: vec!["x".into()],
            excerpt: 0,
        }];
        assert_eq!(
            apply_to_text(&text, &far),
            Err(ApplyError::OutOfRange {
                first_line: 30,
                old_len: 1,
                total: 10
            })
        );
        assert!(!ApplyError::OutOfRange {
            first_line: 30,
            old_len: 1,
            total: 10
        }
        .reason()
        .is_empty());
    }

    #[test]
    fn 直していない抜粋は書き戻しに出てこない() {
        let before = mb_of("a.rs", &[5, 20], 40, 1);
        let after = before.clone();
        assert!(diff_excerpts(&before, &after).is_empty());
        assert_eq!(after.pending_lines(), 0);
        assert_eq!(after.pending_files(), 0);
    }

    #[test]
    fn 開いたあとに変わったファイルはその抜粋だけ拒否する() {
        let before = mb_of("a.rs", &[5], 40, 1);
        let mut after = before.clone();
        after.excerpts[0].lines[0] = "直した".into();
        // 「いまの本文」が組み立て時と違う (別のエージェントが書いた)
        let plan = plan_writeback(&before, &after, |_| Some(file_text(41)), |_| None);
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].outcome, Err(Reject::Changed));
        assert!(!Reject::Changed.reason().is_empty());
        // 一致していれば通る
        let ok = plan_writeback(&before, &after, |_| Some(file_text(40)), |_| None);
        assert!(ok.items[0].outcome.is_ok());
    }

    #[test]
    fn 他人が保有しているファイルへは書き戻さない() {
        let before = mb_of("a.rs", &[5], 40, 1);
        let mut after = before.clone();
        after.excerpts[0].lines[0] = "直した".into();
        let plan = plan_writeback(
            &before,
            &after,
            |_| Some(file_text(40)),
            |_| Some("他のインスタンスが保有中".into()),
        );
        assert_eq!(
            plan.items[0].outcome,
            Err(Reject::Leased("他のインスタンスが保有中".into()))
        );
        assert_eq!(
            plan.items[0].outcome.clone().unwrap_err().reason(),
            "他のインスタンスが保有中"
        );
    }

    #[test]
    fn 読めないファイルは拒否して残りは通す() {
        let seeds = vec![Seed::plain(p("a.rs"), 5), Seed::plain(p("b.rs"), 5)];
        let before = build(
            Source::Search,
            "",
            &seeds,
            None,
            BuildOpts {
                context: 1,
                max_excerpts: 50,
                max_lines: 100,
            },
            |_| Some(split_lines(&file_text(20))),
        );
        let mut after = before.clone();
        for e in &mut after.excerpts {
            e.lines[0] = "直した".into();
        }
        let plan = plan_writeback(
            &before,
            &after,
            |path| {
                if path.ends_with("a.rs") {
                    None
                } else {
                    Some(file_text(20))
                }
            },
            |_| None,
        );
        assert_eq!(plan.items.len(), 2);
        assert_eq!(plan.items[0].outcome, Err(Reject::Unreadable));
        assert!(plan.items[1].outcome.is_ok(), "通せるものは通す");
    }

    #[test]
    fn 書き戻すと後ろの抜粋の行番号がずれて取り消しで戻る() {
        let mut mb = mb_of("a.rs", &[5, 20, 35], 40, 1);
        // 1 つ目の抜粋 (4-6) を 3 行 → 5 行にする (+2)
        mb.excerpts[0].lines = vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()];
        let (f1, f2) = (mb.excerpts[1].first_line, mb.excerpts[2].first_line);
        let new_lines = split_lines(&file_text(42));
        let snap = settle_file(&mut mb, &p("a.rs"), &new_lines);
        assert_eq!(mb.excerpts[1].first_line, f1 + 2, "後ろの抜粋が 2 行ずれる");
        assert_eq!(mb.excerpts[2].first_line, f2 + 2);
        assert_eq!(mb.excerpts[1].focus, vec![22], "注目行も一緒にずれる");
        assert!(!mb.excerpts[0].edited(), "書き戻したので差分は無くなる");
        assert_eq!(mb.excerpts[0].origin_hash, content_hash(&new_lines));

        restore_excerpts(&mut mb, &snap);
        assert_eq!(mb.excerpts[1].first_line, f1, "取り消しで戻る");
        assert_eq!(mb.excerpts[2].first_line, f2);
        assert_eq!(mb.excerpts[1].focus, vec![20]);
        assert!(mb.excerpts[0].edited(), "編集は保留へ戻る");
    }

    #[test]
    fn 一行に複数ある一致をまとめて置き換える() {
        let mut mb = build(
            Source::Search,
            "foo",
            &[Seed {
                path: p("a.rs"),
                line: 1,
                note: String::new(),
                severity: 0,
                mark: Some((0, 3)),
            }],
            None,
            BuildOpts {
                context: 0,
                max_excerpts: 5,
                max_lines: 10,
            },
            |_| Some(vec!["foo bar foo".to_string()]),
        );
        // 同じ行の 2 つ目の一致を手で足す (build は種 1 件 = mark 1 件)
        mb.excerpts[0].marks.push(Mark {
            line: 1,
            start: 8,
            end: 11,
        });
        assert_eq!(replace_marks(&mut mb, "qux"), 2);
        assert_eq!(mb.excerpts[0].lines[0], "qux bar qux");
        assert_eq!(
            mb.excerpts[0].marks[0],
            Mark {
                line: 1,
                start: 0,
                end: 3
            }
        );
        assert_eq!(
            mb.excerpts[0].marks[1],
            Mark {
                line: 1,
                start: 8,
                end: 11
            }
        );
        // もう一度置換しても壊れない (marks が置換後を指している)
        assert_eq!(replace_marks(&mut mb, "z"), 2);
        assert_eq!(mb.excerpts[0].lines[0], "z bar z");
    }

    #[test]
    fn 行内編集の確定は改行で行を増やす() {
        let mut mb = mb_of("a.rs", &[5], 40, 1);
        assert!(!commit_line_edit(&mut mb, None));
        let same = mb.excerpts[0].lines[0].clone();
        assert!(!commit_line_edit(&mut mb, Some((0, 0, same))));
        assert!(commit_line_edit(&mut mb, Some((0, 1, "x\ny\nz".into()))));
        assert_eq!(
            mb.excerpts[0].lines,
            vec!["L4", "x", "y", "z", "L6"],
            "1 行が 3 行へ広がる"
        );
        assert_eq!(mb.excerpts[0].changed_lines(), 4);
        // 範囲外は何もしない
        assert!(!commit_line_edit(&mut mb, Some((9, 0, "x".into()))));
        assert!(!commit_line_edit(&mut mb, Some((0, 99, "x".into()))));
        mb.revert_edits();
        assert!(!mb.excerpts[0].edited(), "捨てると開いた直後へ戻る");
    }

    #[test]
    fn 見出し行はどの幅でも収まって重ならない() {
        let glyph = 8.0_f32;
        // (可用幅, 一致あり, 編集あり, 取り消しあり)
        for &(avail, marks, pending, undo) in &[
            (900.0_f32, true, true, true),
            (1200.0, true, true, true),
            (300.0, true, true, true),
            (300.0, false, false, false),
            (120.0, true, true, true),
            (40.0, true, true, true),
        ] {
            let h = head_layout(avail, glyph, marks, pending, undo);
            let total: f32 = h.widths.iter().sum();
            // 入り切らない幅では**完全に縮退している**ことまでを見る
            // (縮退しきってなお超えるぶんは `horizontal_wrapped` が折り返す)。
            assert!(
                total <= avail + 0.01 || (!h.labels && h.replace_w == 0.0),
                "{avail}px: 合計 {total} が可用幅を超えたのに縮退していない"
            );
            assert!(h.widths.iter().all(|w| *w >= 0.0));
            // 左から並べて重ならないこと
            let mut x = 0.0_f32;
            let mut rects: Vec<(f32, f32)> = Vec::new();
            for w in &h.widths {
                rects.push((x, x + w));
                x += w;
            }
            for pair in rects.windows(2) {
                assert!(pair[0].1 <= pair[1].0 + 0.01, "{avail}px: 矩形が重なった");
            }
            if total <= avail + 0.01 {
                assert!(
                    rects.last().map(|r| r.1).unwrap_or(0.0) <= avail + 0.01,
                    "{avail}px: 右端がはみ出した"
                );
            }
            if !marks {
                assert_eq!(h.replace_w, 0.0, "一致が無い面に置換欄は出さない");
            }
        }
        // 広ければ文字ラベル、狭ければアイコンだけへ縮退する
        assert!(head_layout(1200.0, 8.0, true, true, true).labels);
        assert!(!head_layout(180.0, 8.0, true, true, true).labels);
        assert_eq!(head_layout(180.0, 8.0, true, true, true).replace_w, 0.0);
    }

    /// **統合テスト。** 実ファイルを一時ディレクトリに作り、マルチバッファ経由で
    /// 複数ファイルを同時に書き換えて、ディスクの内容が期待どおりになることを見る。
    /// (アプリ側の配線 = `multibuffer_write_one` と同じ手順をここで踏む。)
    #[test]
    fn 実ファイルを複数まとめて書き戻す() {
        let dir = crate::test_util::unique_temp_dir("zv-mbedit", "writeback");
        std::fs::create_dir_all(&dir).expect("一時ディレクトリ");
        let a = dir.join("a.rs");
        let b = dir.join("b.rs");
        let c = dir.join("c.rs");
        // a は LF・末尾改行あり / b は CRLF / c は末尾改行なし
        std::fs::write(&a, file_text(40)).unwrap();
        std::fs::write(&b, "x1\r\nx2\r\nx3\r\nx4\r\nx5\r\n").unwrap();
        std::fs::write(&c, "y1\ny2\ny3").unwrap();

        let seeds = vec![
            Seed::plain(a.clone(), 5),
            Seed::plain(a.clone(), 20),
            Seed::plain(a.clone(), 35),
            Seed::plain(b.clone(), 3),
            Seed::plain(c.clone(), 2),
        ];
        let read = |path: &Path| std::fs::read_to_string(path).ok();
        let mut mb = build(
            Source::Search,
            "",
            &seeds,
            Some(&dir),
            BuildOpts {
                context: 1,
                max_excerpts: 50,
                max_lines: 100,
            },
            |path| read(path).as_deref().map(split_lines),
        );
        assert_eq!(mb.excerpts.len(), 5, "a に 3 抜粋 + b + c");
        let before = mb.baseline();

        // a の 3 抜粋を全部いじる (行数も変える) / b と c も直す
        mb.excerpts[0].lines = vec!["A4".into(), "A5".into()]; // 3 行 → 2 行
        mb.excerpts[1].lines = vec!["A19".into(), "A20".into(), "A20b".into(), "A21".into()]; // 3 行 → 4 行
        mb.excerpts[2].lines = vec!["A34".into(), "A35".into(), "A36".into()];
        mb.excerpts[3].lines[1] = "X3".into();
        mb.excerpts[4].lines = vec!["y1".into(), "Y2".into(), "y3".into()];
        assert_eq!(mb.pending_files(), 3);

        let plan = plan_writeback(&before, &mb, |path| read(path), |_| None);
        assert_eq!(plan.items.len(), 3, "3 ファイルぶんの書き戻し");
        for item in &plan.items {
            let new_text = item.outcome.as_ref().expect("全部通る");
            std::fs::write(&item.path, new_text).unwrap();
            settle_file(&mut mb, &item.path, &split_lines(new_text));
        }

        // ── ディスクの内容 ──
        let got_a = std::fs::read_to_string(&a).unwrap();
        let mut want: Vec<String> = (1..=3).map(|i| format!("L{i}")).collect();
        want.extend(["A4".to_string(), "A5".to_string()]);
        want.extend((7..=18).map(|i| format!("L{i}")));
        want.extend(["A19", "A20", "A20b", "A21"].iter().map(|s| s.to_string()));
        want.extend((22..=33).map(|i| format!("L{i}")));
        want.extend(["A34", "A35", "A36"].iter().map(|s| s.to_string()));
        want.extend((37..=40).map(|i| format!("L{i}")));
        assert_eq!(
            got_a,
            want.iter().map(|l| format!("{l}\n")).collect::<String>(),
            "a.rs は 3 抜粋ぶんが後ろから当たって行番号がずれない"
        );
        assert_eq!(
            std::fs::read_to_string(&b).unwrap(),
            "x1\r\nx2\r\nX3\r\nx4\r\nx5\r\n",
            "b.rs は CRLF のまま"
        );
        assert_eq!(
            std::fs::read_to_string(&c).unwrap(),
            "y1\nY2\ny3",
            "c.rs は末尾改行なしのまま"
        );
        // 書き戻したので差分は残っていない = 続けてもう一度撃てる
        assert_eq!(mb.pending_lines(), 0);
        assert!(diff_excerpts(&mb.baseline(), &mb).is_empty());

        // ── 外から変わったファイルは次の書き戻しで拒否される ──
        let before2 = mb.baseline();
        mb.excerpts[0].lines[0] = "A4-again".into();
        mb.excerpts[3].lines[1] = "X3-again".into();
        std::fs::write(&a, "別のエージェントが全部書き換えた\n").unwrap();
        let plan2 = plan_writeback(&before2, &mb, |path| read(path), |_| None);
        let by: std::collections::BTreeMap<_, _> = plan2
            .items
            .iter()
            .map(|i| (i.path.clone(), i.outcome.clone()))
            .collect();
        assert_eq!(by[&a], Err(Reject::Changed), "a.rs だけ拒否");
        assert!(by[&b].is_ok(), "b.rs は通る (通せるものは通す)");

        // ── 他人のリースなら拒否 ──
        let plan3 = plan_writeback(
            &before2,
            &mb,
            |path| read(path),
            |path| (path == b).then(|| "他のインスタンスが保有中".to_string()),
        );
        let by3: std::collections::BTreeMap<_, _> = plan3
            .items
            .iter()
            .map(|i| (i.path.clone(), i.outcome.clone()))
            .collect();
        assert_eq!(
            by3[&b],
            Err(Reject::Leased("他のインスタンスが保有中".into())),
            "リースを持たれていたら書かない"
        );
        // b.rs は 1 バイトも変わっていない
        assert_eq!(
            std::fs::read_to_string(&b).unwrap(),
            "x1\r\nx2\r\nX3\r\nx4\r\nx5\r\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
