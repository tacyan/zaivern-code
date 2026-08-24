//! バッファ内検索 (エディタの検索/置換バーの中身)。
//!
//! **正規表現エンジンをここに持たない。** 全ファイル検索
//! ([`crate::file_search`]) の [`Matcher`] をそのまま使う。理由は 2 つ:
//!
//! * 正規表現・大小区別・単語単位の意味論を 1 箇所に閉じ込める
//!   (「全ファイル検索では引っかかるのにバッファ内では引っかからない」を構造的に無くす)。
//! * `regex` クレートは有限オートマトン方式で**入力長に対して線形時間**なので、
//!   検索窓に `(a+)+$` のような病的パターンを打たれても固まらない
//!   (自前のバックトラッキング実装を持たない理由でもある)。
//!
//! 位置は**すべてバイト**で持つ。ただしバイト位置の出どころは
//! [`Matcher::find_all`] = `char_indices` 由来なので、常に文字境界に乗る
//! (CJK 混じりの本文をここで slice しても panic しない)。
//!
//! 走査は**行単位**に切る。VS Code と同じく `^` / `$` を「行頭 / 行末」として
//! 扱い、`.` が改行をまたがないようにするため。

use eframe::egui::text::{LayoutJob, LayoutSection};
use eframe::egui::Color32;

use crate::file_search::{Matcher, SearchError, SearchOptions};
use crate::theme::Theme;

/// 1 バッファで覚えるヒットの上限。
///
/// `x*` のような空幅にも一致するパターンは本文の文字数だけヒットを作れるので、
/// 上限が無いとメモリと描画 (セクション分割) が爆発する。
/// 超えた分は数えず、UI 側は「以上」を付けて表示する。
pub const MAX_HITS: usize = 5_000;

// ─────────────────────────── 検索条件とヒット ───────────────────────────

/// 検索バーのトグル 3 つ。VS Code と同じ並び (正規表現 / 大小区別 / 単語単位)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FindOptions {
    /// 大文字小文字を区別する (Aa)。
    pub case_sensitive: bool,
    /// 単語単位 (ab|)。前後が単語文字でないマッチだけを拾う。
    pub whole_word: bool,
    /// 正規表現 (.*)。
    pub regex: bool,
}

/// 本文中の 1 ヒット。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufHit {
    /// 本文先頭からのバイト位置 (文字境界)。
    pub start: usize,
    pub end: usize,
    /// 0 始まりの行番号 (ミニマップの印に使う)。
    pub line: usize,
}

impl BufHit {
    /// バイト範囲。
    pub fn range(&self) -> (usize, usize) {
        (self.start, self.end)
    }
}

/// 条件からマッチャを組み立てる。正規表現が不正なら [`SearchError`] を返す
/// (**panic しない**。UI はこれをそのまま表示する)。
pub fn compile(query: &str, opts: FindOptions) -> Result<Matcher, SearchError> {
    Matcher::compile(&SearchOptions {
        query: query.to_string(),
        case_sensitive: opts.case_sensitive,
        whole_word: opts.whole_word,
        regex: opts.regex,
        ..SearchOptions::default()
    })
}

/// 本文全体のヒットを行単位で拾う。戻り値の `bool` は
/// [`MAX_HITS`] で打ち切ったか。
pub fn find_all(text: &str, m: &Matcher) -> (Vec<BufHit>, bool) {
    let mut out: Vec<BufHit> = Vec::new();
    let mut base = 0usize;
    for (line_no, line) in text.split('\n').enumerate() {
        // split('\n') は CR を残すので、CRLF の CR は行の中身から外す
        // (行末の `$` が CR の手前に来るようにする)。
        let content = line.strip_suffix('\r').unwrap_or(line);
        for (s, e) in m.find_all(content) {
            if out.len() >= MAX_HITS {
                return (out, true);
            }
            out.push(BufHit {
                start: base + s,
                end: base + e,
                line: line_no,
            });
        }
        // 行の長さ + 改行 1 バイト。最終行では次の周回が無いので余分は無害。
        base += line.len() + 1;
    }
    (out, false)
}

/// 次 / 前のヒットを選ぶ。戻り値は `(hits の添字, 折り返したか)`。
///
/// `from` は探索の起点バイト。
/// * `forward` — `start >= from` の**最初**。無ければ先頭へ折り返す。
/// * 逆方向 — `start < from` の**最後**。無ければ末尾へ折り返す。
///
/// ヒットが 0 件なら `None` (折り返しようが無い)。
pub fn step(hits: &[BufHit], from: usize, forward: bool) -> Option<(usize, bool)> {
    if hits.is_empty() {
        return None;
    }
    if forward {
        match hits.iter().position(|h| h.start >= from) {
            Some(i) => Some((i, false)),
            None => Some((0, true)),
        }
    } else {
        match hits.iter().rposition(|h| h.start < from) {
            Some(i) => Some((i, false)),
            None => Some((hits.len() - 1, true)),
        }
    }
}

/// バイト位置を文字位置へ直す (egui の `TextEdit` は char 単位で選択するため)。
/// 文字境界でないバイトを渡されても panic せず、直前の境界に丸める。
pub fn byte_to_char(text: &str, byte: usize) -> usize {
    let b = byte.min(text.len());
    text.char_indices().take_while(|(i, _)| *i < b).count()
}

// ─────────────────────────── 置換 ───────────────────────────

/// 置換文字列を 1 ヒット分だけ展開する。
///
/// 正規表現モードでは `$1` / `${name}` のグループ参照が効く (VS Code 準拠)。
/// リテラルモードでは `$` も普通の文字として扱う (打った通りに入る)。
pub fn expand(m: &Matcher, line: &str, hit: (usize, usize), replacement: &str) -> String {
    let Some(re) = m.regex() else {
        return replacement.to_string();
    };
    let Some(c) = re.captures_at(line, hit.0) else {
        return replacement.to_string();
    };
    // `captures_at` は「hit.0 以降の最初のマッチ」を返す。単語単位で 1 個
    // 飛ばした場合は別のマッチを掴みうるので、**範囲が一致したときだけ**展開する
    // (取り違えた captures で置換文字列を作らない)。
    if c.get(0).map(|m0| (m0.start(), m0.end())) != Some(hit) {
        return replacement.to_string();
    }
    let mut out = String::new();
    c.expand(replacement, &mut out);
    out
}

/// 本文全体を置換する。改行 (LF / CRLF / 末尾改行なし) はそのまま保つ。
/// 戻り値は `(新しい本文, 置換件数)`。
pub fn replace_all(text: &str, m: &Matcher, replacement: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut count = 0usize;
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let (content, cr) = match line.strip_suffix('\r') {
            Some(c) => (c, "\r"),
            None => (line, ""),
        };
        let ranges = m.find_all(content);
        if ranges.is_empty() {
            out.push_str(line);
            continue;
        }
        let mut at = 0usize;
        for (s, e) in &ranges {
            out.push_str(&content[at..*s]);
            out.push_str(&expand(m, content, (*s, *e), replacement));
            at = *e;
        }
        out.push_str(&content[at..]);
        out.push_str(cr);
        count += ranges.len();
    }
    (out, count)
}

// ─────────────────────────── 本文のハイライト ───────────────────────────

/// 他のヒットの背景 (VS Code の `findMatchHighlightBackground` 相当)。
/// **テーマから作る** — 固定色を書かない。
pub fn hit_bg(t: &Theme) -> Color32 {
    t.accent.gamma_multiply(0.28)
}

/// いま選ばれているヒットの背景 (VS Code の `findMatchBackground` 相当)。
/// 他のヒットより強くして「今どこにいるか」を一目で分かるようにする。
pub fn current_hit_bg(t: &Theme) -> Color32 {
    t.warn.gamma_multiply(0.60)
}

/// 0 件のときに検索欄を囲む色。
pub fn no_match_color(t: &Theme) -> Color32 {
    t.err
}

/// 本文の [`LayoutJob`] に検索ヒットの背景色を差し込む。
///
/// `hits` は **start 昇順・重なり無し** ([`find_all`] の出力) を前提にする。
/// 空幅マッチ (`start == end`) は塗る面積が無いので飛ばす。
///
/// 空白可視化 (`whitespace_layout_job`) は `sec.format` を丸ごと引き継ぐので、
/// **この関数を先に通してから**空白可視化を掛ければ背景は保たれる
/// (空白可視化はバイト長を変えるため、順番を逆にすると範囲がズレる)。
pub fn apply_hits(
    mut job: LayoutJob,
    hits: &[BufHit],
    current: Option<(usize, usize)>,
    other: Color32,
    cur: Color32,
) -> LayoutJob {
    if hits.is_empty() {
        return job;
    }
    let mut out: Vec<LayoutSection> = Vec::with_capacity(job.sections.len() + hits.len() * 2);
    // hits を走査する位置。セクションは byte_range 昇順なので巻き戻らない。
    let mut hi = 0usize;
    for sec in std::mem::take(&mut job.sections) {
        let (ss, se) = (sec.byte_range.start, sec.byte_range.end);
        while hi < hits.len() && hits[hi].end <= ss {
            hi += 1;
        }
        if se <= ss {
            // 空セクションはそのまま残す (落とすと leading_space が消える)
            out.push(sec);
            continue;
        }
        let mut leading = sec.leading_space;
        let mut at = ss;
        let mut k = hi;
        while k < hits.len() && hits[k].start < se {
            let h = hits[k];
            k += 1;
            if h.end <= h.start {
                continue; // 空幅マッチ
            }
            let s = h.start.max(at);
            let e = h.end.min(se);
            // **ヒットは 1 フレーム古い本文のものでありうる。**
            // 打鍵と同じフレームで本文が変わる経路 (`TextEdit` 自身の編集 /
            // 複数キャレット / スニペット展開 / 自動ペア) があり、そのフレームの
            // ヒットは 1 つ前の本文の位置を指す。ズレた位置で切ると CJK の
            // 途中に当たり、**epaint が panic する** (実際に 7 回落ちた:
            // `end byte index N is not a char boundary; it is inside '（'`)。
            // 境界に乗らないヒットは**塗らずに飛ばす** — 1 フレーム塗り遅れる
            // だけで、次のフレームには正しい位置で塗られる。
            // `is_char_boundary` は範囲外にも false を返すので長さの検査も兼ねる。
            if s >= e || !job.text.is_char_boundary(s) || !job.text.is_char_boundary(e) {
                continue;
            }
            if s > at {
                push_section(&mut out, &sec, at..s, None, &mut leading);
            }
            let bg = if current == Some((h.start, h.end)) {
                cur
            } else {
                other
            };
            push_section(&mut out, &sec, s..e, Some(bg), &mut leading);
            at = e;
        }
        if at < se {
            push_section(&mut out, &sec, at..se, None, &mut leading);
        }
    }
    job.sections = out;
    job
}

/// [`apply_hits`] の 1 断片。`leading_space` は最初の断片だけが引き継ぐ。
fn push_section(
    out: &mut Vec<LayoutSection>,
    src: &LayoutSection,
    range: std::ops::Range<usize>,
    bg: Option<Color32>,
    leading: &mut f32,
) {
    if range.end <= range.start {
        return;
    }
    let mut format = src.format.clone();
    if let Some(c) = bg {
        format.background = c;
    }
    out.push(LayoutSection {
        leading_space: std::mem::take(leading),
        byte_range: range,
        format,
    });
}

// ─────────────────────────── 検索バーのレイアウト ───────────────────────────

/// 検索バー 1 行目に並ぶ要素の幅 (pt)。
///
/// egui の既定パディング任せにせず、描画側は `add_sized` でこの幅を使う
/// (モデルと画面をズレさせないため)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BarMetrics {
    /// 置換行の開閉キャレット。
    pub caret: f32,
    /// 虫眼鏡のラベル。
    pub glyph: f32,
    /// トグル 1 個 (.* / Aa / ab|)。
    pub toggle: f32,
    /// 前へ/次へ をアイコンだけで出すときの幅。
    pub nav_icon: f32,
    /// 前へ/次へ をラベル付きで出すときの幅。
    pub nav_label: f32,
    /// 「3 / 27」の表示。
    pub count: f32,
    /// 閉じるボタン。
    pub close: f32,
    pub query_min: f32,
    /// これ以上狭くできない検索欄の幅。
    pub query_floor: f32,
    pub query_max: f32,
    /// 通常時の要素間隔。
    pub spacing: f32,
    /// 最も詰めたときの要素間隔。
    pub spacing_tight: f32,
}

impl Default for BarMetrics {
    fn default() -> Self {
        Self {
            caret: 22.0,
            glyph: 18.0,
            toggle: 26.0,
            nav_icon: 26.0,
            nav_label: 54.0,
            count: 56.0,
            close: 24.0,
            query_min: 72.0,
            query_floor: 40.0,
            query_max: 260.0,
            spacing: 8.0,
            spacing_tight: 3.0,
        }
    }
}

/// 幅に応じた見せ方の段。狭くなるほど落とす。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Density {
    /// 全部出す (前へ/次へ はラベル付き)。
    Full,
    /// 虫眼鏡を落とし、前へ/次へ をアイコンのみへ縮退。
    Compact,
    /// キャレットとヒット数も落とす。トグルとナビはアイコンのみ。
    Minimal,
}

/// 検索バー 1 行目の配置。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BarLayout {
    pub density: Density,
    pub show_caret: bool,
    pub show_glyph: bool,
    pub show_count: bool,
    /// 前へ/次へ にラベルを付けるか (false = アイコンのみ)。
    pub nav_labels: bool,
    pub spacing: f32,
    pub query_width: f32,
}

impl BarLayout {
    /// 並べる要素の個数 (間隔の本数 = これ - 1)。
    pub fn item_count(&self) -> usize {
        // 検索欄 + トグル 3 + 前へ + 次へ + 閉じる
        let mut n = 1 + 3 + 2 + 1;
        n += usize::from(self.show_caret);
        n += usize::from(self.show_glyph);
        n += usize::from(self.show_count);
        n
    }

    /// 検索欄以外の幅の合計 (間隔込み)。
    pub fn fixed_width(&self, m: &BarMetrics) -> f32 {
        let nav = if self.nav_labels {
            m.nav_label
        } else {
            m.nav_icon
        };
        let mut w = m.toggle * 3.0 + nav * 2.0 + m.close;
        if self.show_caret {
            w += m.caret;
        }
        if self.show_glyph {
            w += m.glyph;
        }
        if self.show_count {
            w += m.count;
        }
        w + self.spacing * (self.item_count().saturating_sub(1)) as f32
    }

    /// 実際に並べたときの合計幅。
    pub fn total_width(&self, m: &BarMetrics) -> f32 {
        self.fixed_width(m) + self.query_width
    }
}

/// `density` の骨組み (検索欄の幅はまだ入っていない)。
fn template(density: Density, m: &BarMetrics) -> BarLayout {
    match density {
        Density::Full => BarLayout {
            density,
            show_caret: true,
            show_glyph: true,
            show_count: true,
            nav_labels: true,
            spacing: m.spacing,
            query_width: 0.0,
        },
        Density::Compact => BarLayout {
            density,
            show_caret: true,
            show_glyph: false,
            show_count: true,
            nav_labels: false,
            spacing: m.spacing,
            query_width: 0.0,
        },
        Density::Minimal => BarLayout {
            density,
            show_caret: false,
            show_glyph: false,
            show_count: false,
            nav_labels: false,
            spacing: m.spacing_tight,
            query_width: 0.0,
        },
    }
}

/// これより狭いと、どう詰めても収まらない幅。
pub fn min_width(m: &BarMetrics) -> f32 {
    template(Density::Minimal, m).fixed_width(m) + m.query_floor
}

/// 可用幅から配置を決める**純粋関数**。
///
/// 「トグルをアイコンのみへ縮退するか」「ヒット数を出すか」は
/// すべてここだけで決める (描画側に判断を散らさない)。
pub fn bar_layout(available: f32, m: &BarMetrics) -> BarLayout {
    for density in [Density::Full, Density::Compact, Density::Minimal] {
        let mut l = template(density, m);
        let room = available - l.fixed_width(m);
        if room >= m.query_min {
            l.query_width = room.min(m.query_max);
            return l;
        }
        if density == Density::Minimal {
            l.query_width = room.clamp(m.query_floor, m.query_max);
            return l;
        }
    }
    unreachable!("Density は 3 段すべて試している")
}

/// ヒット行を見せるための新しいスクロール位置。**もう見えているなら `None`**。
///
/// VS Code の "reveal" と同じ約束: 画面に入っている一致のために本文を動かさない。
/// インクリメンタル検索は**打鍵のたびに**走るので、毎回中央へ寄せると 1 文字ごとに
/// 本文が飛び跳ねて読めなくなる (「1 文字打つと検索へ行ってしまう」の半分はこれ)。
///
/// 見えていないときは画面の上から 4 割の位置へ寄せる (前後の文脈が見える)。
/// 端に貼り付くと次の行が見えないので、**上下 1 行ぶんの余白**を要求する。
pub fn reveal_scroll(line: usize, row_h: f32, scroll_y: f32, view_h: f32) -> Option<f32> {
    let row_h = row_h.max(1.0);
    let y = line as f32 * row_h;
    let margin = row_h;
    // 余白 2 行ぶんも取れない高さでは「見えている」と言えない (必ず寄せる)
    let roomy = view_h >= margin * 3.0;
    if roomy && y >= scroll_y + margin && y + row_h <= scroll_y + view_h - margin {
        return None;
    }
    Some((y - view_h * 0.4).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(query: &str, opts: FindOptions) -> Matcher {
        compile(query, opts).expect("コンパイルできる想定")
    }

    fn lit() -> FindOptions {
        FindOptions::default()
    }

    fn rx() -> FindOptions {
        FindOptions {
            regex: true,
            ..FindOptions::default()
        }
    }

    // ── 検索 ──

    #[test]
    fn 不正な正規表現でpanicせずエラーを返す() {
        for bad in ["(", "[a-", "a{2,1}", "*", "(?P<", "\\", "(?=x)", "\\1"] {
            let r = compile(bad, rx());
            assert!(r.is_err(), "{bad:?} は弾かれるべき");
            // 表示できること (UI がそのまま出す)
            assert!(!r.unwrap_err().to_string().is_empty());
        }
        // リテラルモードでは同じ文字列がただの検索語になる (エラーにならない)
        assert!(compile("(", lit()).is_ok());
    }

    #[test]
    fn ヒット0件() {
        let (hits, trunc) = find_all("hello world", &m("zzz", lit()));
        assert!(hits.is_empty());
        assert!(!trunc);
        assert_eq!(step(&hits, 0, true), None);
        assert_eq!(step(&hits, 0, false), None);
    }

    #[test]
    fn ヒット1件は前後どちらへ回っても同じ場所に留まる() {
        let text = "alpha beta gamma";
        let (hits, _) = find_all(text, &m("beta", lit()));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].range(), (6, 10));
        assert_eq!(hits[0].line, 0);
        assert_eq!(step(&hits, 0, true), Some((0, false)));
        // 現在ヒットの次を探すと折り返して同じヒットに戻る
        assert_eq!(step(&hits, hits[0].start + 1, true), Some((0, true)));
        assert_eq!(step(&hits, hits[0].start, false), Some((0, true)));
    }

    #[test]
    fn 末尾から先頭へ折り返す() {
        let text = "a\nb a\nc a";
        let (hits, _) = find_all(text, &m("a", lit()));
        assert_eq!(hits.len(), 3);
        assert_eq!(
            hits.iter().map(|h| h.line).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        // 最後のヒットから「次へ」→ 先頭へ折り返す
        let last = hits[2].start;
        assert_eq!(step(&hits, last + 1, true), Some((0, true)));
        // 先頭のヒットから「前へ」→ 末尾へ折り返す
        assert_eq!(step(&hits, hits[0].start, false), Some((2, true)));
        // 折り返さない普通の移動
        assert_eq!(step(&hits, hits[0].start + 1, true), Some((1, false)));
        assert_eq!(step(&hits, hits[2].start, false), Some((1, false)));
    }

    #[test]
    fn 大小区別トグル() {
        let text = "Foo foo FOO";
        let ci = find_all(text, &m("foo", lit())).0;
        assert_eq!(ci.len(), 3);
        let cs = find_all(
            text,
            &m(
                "foo",
                FindOptions {
                    case_sensitive: true,
                    ..lit()
                },
            ),
        )
        .0;
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].range(), (4, 7));
    }

    #[test]
    fn 単語単位トグル() {
        let text = "cat category cat";
        let all = find_all(text, &m("cat", lit())).0;
        assert_eq!(all.len(), 3);
        let word = find_all(
            text,
            &m(
                "cat",
                FindOptions {
                    whole_word: true,
                    ..lit()
                },
            ),
        )
        .0;
        assert_eq!(word.len(), 2);
        assert_eq!(word[0].range(), (0, 3));
        assert_eq!(word[1].range(), (13, 16));
    }

    #[test]
    fn 正規表現のグループ検索() {
        let text = "a@b\nlong@host";
        let (hits, _) = find_all(text, &m(r"(\w+)@(\w+)", rx()));
        assert_eq!(hits.len(), 2);
        assert_eq!(&text[hits[0].start..hits[0].end], "a@b");
        assert_eq!(&text[hits[1].start..hits[1].end], "long@host");
        assert_eq!(hits[1].line, 1);
    }

    #[test]
    fn 行頭アンカーは各行の先頭に当たる() {
        let text = "x1\nx2\nyx3";
        let (hits, _) = find_all(text, &m("^x", rx()));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].line, 0);
        assert_eq!(hits[1].line, 1);
    }

    #[test]
    fn crlfでも行末アンカーが当たる() {
        let text = "ab\r\ncd\r\n";
        let (hits, _) = find_all(text, &m("b$", rx()));
        assert_eq!(hits.len(), 1);
        assert_eq!(&text[hits[0].start..hits[0].end], "b");
    }

    // ── CJK / バイト境界 ──

    #[test]
    fn cjkを含む本文でバイト境界を壊さない() {
        let text = "日本語のテキスト\nあいう世界えお\n世界";
        let (hits, _) = find_all(text, &m("世界", lit()));
        assert_eq!(hits.len(), 2);
        for h in &hits {
            // バイト位置で slice しても panic しない = 文字境界に乗っている
            assert!(text.is_char_boundary(h.start));
            assert!(text.is_char_boundary(h.end));
            assert_eq!(&text[h.start..h.end], "世界");
        }
        assert_eq!(hits[0].line, 1);
        assert_eq!(hits[1].line, 2);
        // char 位置への変換も文字数で合う
        assert_eq!(
            byte_to_char(text, hits[1].start),
            "日本語のテキスト\nあいう世界えお\n".chars().count()
        );
    }

    #[test]
    fn cjkの正規表現でもバイト境界を壊さない() {
        let text = "🎌旗と日本語\n絵文字🎌だけ";
        let (hits, _) = find_all(text, &m("🎌.", rx()));
        assert_eq!(hits.len(), 2);
        for h in &hits {
            assert!(text.is_char_boundary(h.start));
            assert!(text.is_char_boundary(h.end));
            let _ = &text[h.start..h.end]; // panic しない
        }
    }

    #[test]
    fn 空幅マッチは打ち切り上限で止まる() {
        let text = "x".repeat(MAX_HITS * 2);
        let (hits, trunc) = find_all(&text, &m("y*", rx()));
        assert!(trunc);
        assert_eq!(hits.len(), MAX_HITS);
    }

    // ── 置換 ──

    #[test]
    fn 後方参照の置換() {
        let mm = m(r"(\w+)@(\w+)", rx());
        let (out, n) = replace_all("a@b and long@host", &mm, "$2:$1");
        assert_eq!(n, 2);
        assert_eq!(out, "b:a and host:long");
    }

    #[test]
    fn 名前付きグループの置換() {
        let mm = m(r"(?P<user>\w+)@(?P<host>\w+)", rx());
        let (out, n) = replace_all("a@b", &mm, "${host}/${user}");
        assert_eq!(n, 1);
        assert_eq!(out, "b/a");
    }

    #[test]
    fn リテラルモードではドル記号をそのまま入れる() {
        let mm = m("foo", lit());
        let (out, n) = replace_all("foo bar", &mm, "$1");
        assert_eq!(n, 1);
        assert_eq!(out, "$1 bar");
    }

    #[test]
    fn 置換は改行を保つ() {
        let mm = m("a", lit());
        let (out, n) = replace_all("a\r\na\na", &mm, "b");
        assert_eq!(n, 3);
        assert_eq!(out, "b\r\nb\nb");
        // 末尾改行あり
        let (out2, n2) = replace_all("a\n", &mm, "b");
        assert_eq!(n2, 1);
        assert_eq!(out2, "b\n");
    }

    #[test]
    fn cjkの置換でバイト境界を壊さない() {
        let mm = m("世界", lit());
        let (out, n) = replace_all("こんにちは世界。世界!", &mm, "World");
        assert_eq!(n, 2);
        assert_eq!(out, "こんにちはWorld。World!");
    }

    #[test]
    fn 単語単位の置換は部分一致を巻き込まない() {
        let mm = m(
            "cat",
            FindOptions {
                whole_word: true,
                ..lit()
            },
        );
        let (out, n) = replace_all("cat category cat", &mm, "dog");
        assert_eq!(n, 2);
        assert_eq!(out, "dog category dog");
    }

    #[test]
    fn 置換後に検索語を含んでも無限ループしない() {
        let mm = m("a", lit());
        let (out, n) = replace_all("aaa", &mm, "aa");
        assert_eq!(n, 3);
        assert_eq!(out, "aaaaaa");
    }

    // ── ハイライト ──

    fn job_of(text: &str) -> LayoutJob {
        let mut j = LayoutJob::default();
        j.append(text, 0.0, eframe::egui::TextFormat::default());
        j
    }

    fn covered(job: &LayoutJob) -> String {
        let mut s = String::new();
        for sec in &job.sections {
            s.push_str(&job.text[sec.byte_range.clone()]);
        }
        s
    }

    /// **1 フレーム古いヒットで切っても、文字境界しか切らない。**
    ///
    /// 打鍵と同じフレームで本文が変わる経路 (`TextEdit` 自身の編集 / 複数
    /// キャレット / スニペット展開 / 自動ペア) があり、そのフレームのヒットは
    /// 1 つ前の本文の位置を指す。ズレた位置で CJK の途中を切ると
    /// **epaint が落ちる** — 実際に利用者の `panic.log` に 7 回残っていた
    /// (`end byte index N is not a char boundary; it is inside '（'`)。
    #[test]
    fn 古いヒットでも文字境界しか切らない() {
        let before = "あいうえお かきくけこ あいうえお";
        let (hits, _) = find_all(before, &m("あいうえお", lit()));
        assert_eq!(hits.len(), 2, "前提: 2 件見つかっている");
        // 本文の先頭へ ASCII が 1 文字入った = 以降のヒットが 1 バイトずれる。
        // ずれた終端はすべて CJK の途中に来る (境界に乗らない)。
        let after = format!("x{before}");
        assert!(
            hits.iter()
                .any(|h| !after.is_char_boundary(h.end) || !after.is_char_boundary(h.start)),
            "前提: ずらしたヒットは文字境界に乗っていない"
        );
        let job = apply_hits(job_of(&after), &hits, None, Color32::RED, Color32::GREEN);
        for sec in &job.sections {
            let (s0, e0) = (sec.byte_range.start, sec.byte_range.end);
            assert!(
                after.is_char_boundary(s0) && after.is_char_boundary(e0),
                "文字境界でない範囲を切った: {s0}..{e0}"
            );
        }
        assert_eq!(covered(&job), after, "本文は 1 バイトも欠けない");
        // 実際に組んでも落ちないこと (epaint の要求そのものを踏む)
        let ctx = eframe::egui::Context::default();
        let _ = ctx.run(Default::default(), |ctx| {
            ctx.fonts(|f| {
                let _ = f.layout_job(job.clone());
            });
        });
    }

    /// 境界に乗るヒットは今までどおり塗る (安全側へ倒しすぎない)。
    #[test]
    fn 境界に乗るヒットは従来どおり塗る() {
        let text = "あいうえお かきくけこ あいうえお";
        let (hits, _) = find_all(text, &m("あいうえお", lit()));
        let job = apply_hits(job_of(text), &hits, None, Color32::RED, Color32::GREEN);
        let painted = job
            .sections
            .iter()
            .filter(|s| s.format.background == Color32::RED)
            .count();
        assert_eq!(painted, 2, "CJK でも 2 件とも塗る");
    }

    #[test]
    fn ハイライトは本文を欠けさせない() {
        let text = "abc def abc";
        let (hits, _) = find_all(text, &m("abc", lit()));
        let job = apply_hits(
            job_of(text),
            &hits,
            Some(hits[1].range()),
            Color32::RED,
            Color32::GREEN,
        );
        assert_eq!(covered(&job), text);
        let bgs: Vec<Color32> = job.sections.iter().map(|s| s.format.background).collect();
        assert!(bgs.contains(&Color32::RED));
        assert!(bgs.contains(&Color32::GREEN));
        // 現在ヒットは 1 断片だけ
        assert_eq!(bgs.iter().filter(|c| **c == Color32::GREEN).count(), 1);
    }

    #[test]
    fn cjk本文のハイライトでも本文が欠けない() {
        let text = "あ世界い\n世界";
        let (hits, _) = find_all(text, &m("世界", lit()));
        let job = apply_hits(job_of(text), &hits, None, Color32::RED, Color32::GREEN);
        assert_eq!(covered(&job), text);
    }

    #[test]
    fn ヒットが無ければジョブは素通し() {
        let text = "abc";
        let job = apply_hits(job_of(text), &[], None, Color32::RED, Color32::GREEN);
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.background, Color32::TRANSPARENT);
    }

    #[test]
    fn 空幅マッチは塗らない() {
        let text = "abc";
        let (hits, _) = find_all(text, &m("x*", rx()));
        assert!(!hits.is_empty());
        let job = apply_hits(job_of(text), &hits, None, Color32::RED, Color32::GREEN);
        assert_eq!(covered(&job), text);
        assert!(job
            .sections
            .iter()
            .all(|s| s.format.background == Color32::TRANSPARENT));
    }

    // ── レイアウト (テーブルテスト) ──

    #[test]
    fn 検索バーは可用幅に必ず収まる() {
        let m0 = BarMetrics::default();
        // (可用幅, 期待する段, トグルをアイコンのみへ縮退するか)
        let table: &[(f32, Density, bool)] = &[
            (900.0, Density::Full, false),
            (640.0, Density::Full, false),
            (450.0, Density::Full, false),
            (400.0, Density::Compact, true),
            (380.0, Density::Compact, true),
            (250.0, Density::Minimal, true),
            (min_width(&m0), Density::Minimal, true),
        ];
        for (avail, want_density, want_icons_only) in table {
            let l = bar_layout(*avail, &m0);
            assert_eq!(l.density, *want_density, "幅 {avail} の段");
            assert_eq!(!l.nav_labels, *want_icons_only, "幅 {avail} の縮退");
            assert!(
                l.total_width(&m0) <= *avail + 0.01,
                "幅 {avail} で {} にはみ出した",
                l.total_width(&m0)
            );
            assert!(
                l.query_width >= m0.query_floor,
                "幅 {avail} の検索欄が潰れた"
            );
        }
    }

    #[test]
    fn 幅を狭めても段は戻らない() {
        let m0 = BarMetrics::default();
        let rank = |d: Density| match d {
            Density::Full => 2,
            Density::Compact => 1,
            Density::Minimal => 0,
        };
        let mut prev = 3;
        let mut w = 1200.0f32;
        while w >= min_width(&m0) {
            let r = rank(bar_layout(w, &m0).density);
            assert!(r <= prev, "幅 {w} で段が戻った");
            prev = r;
            w -= 5.0;
        }
    }

    #[test]
    fn 検索欄は広げすぎない() {
        let m0 = BarMetrics::default();
        assert_eq!(bar_layout(4000.0, &m0).query_width, m0.query_max);
    }

    #[test]
    fn 段が下がるほど要素は減る() {
        let m0 = BarMetrics::default();
        let full = bar_layout(900.0, &m0);
        let compact = bar_layout(400.0, &m0);
        let minimal = bar_layout(250.0, &m0);
        assert!(full.item_count() >= compact.item_count());
        assert!(compact.item_count() >= minimal.item_count());
        assert!(full.show_glyph && !compact.show_glyph);
        assert!(compact.show_count && !minimal.show_count);
        assert!(compact.show_caret && !minimal.show_caret);
    }

    /// 見えている一致のために画面を動かさない (VS Code の reveal)。
    /// 打鍵ごとに走る検索で毎回寄せると、本文が 1 文字ごとに飛び跳ねる。
    #[test]
    fn 見えている行では画面を動かさない() {
        let (row_h, view_h) = (18.0_f32, 600.0_f32);
        // 画面 = 行 10..43 (スクロール 180px, 高さ 600px)
        let scroll = 10.0 * row_h;
        // 表: (行, 期待)
        for (line, visible) in [
            (0usize, false), // ずっと上
            (9, false),      // 上の余白 1 行に掛かる
            (11, true),      // 余裕で見えている
            (25, true),      // 真ん中
            (41, true),      // 下の余白の内側
            (42, false),     // 下の余白 1 行に掛かる
            (500, false),    // ずっと下
        ] {
            let got = reveal_scroll(line, row_h, scroll, view_h);
            assert_eq!(
                got.is_none(),
                visible,
                "行 {line} の判定が違う (got={got:?})"
            );
        }
    }

    /// 見えていないときは上から 4 割の位置へ寄せ、先頭より上には行かない。
    #[test]
    fn 見えていない行は文脈が見える位置へ寄せる() {
        let (row_h, view_h) = (18.0_f32, 600.0_f32);
        let y = reveal_scroll(100, row_h, 0.0, view_h).expect("見えていないので寄せる");
        assert_eq!(y, 100.0 * row_h - view_h * 0.4);
        // 上の方の行では負にしない (0 で止める)
        assert_eq!(reveal_scroll(1, row_h, 5000.0, view_h), Some(0.0));
    }

    /// 高さが 0 / 行高が 0 でも panic せず、必ず寄せる側に倒す。
    #[test]
    fn 潰れた画面でも判断を返す() {
        assert!(reveal_scroll(5, 0.0, 0.0, 0.0).is_some());
        assert!(
            reveal_scroll(0, 18.0, 0.0, 20.0).is_some(),
            "1 行ぶんの高さでは見えているとは言わない"
        );
    }
}
