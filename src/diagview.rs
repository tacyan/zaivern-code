//! 診断のインライン表示 (本文の波線・行末メッセージ) と対応括弧の強調。
//!
//! ここには **画面に触らない純粋関数だけ** を置く。座標も色も文字列もここで
//! 決めて、`app.rs` は結果を塗るだけにする (レイアウト判断を純粋関数へ
//! 切り出してテーブルテストで固定する、という既存方針に合わせる)。
//!
//! 位置の単位に注意:
//! * LSP の `Diagnostic` は **UTF-16 code unit** の列を持つ。
//! * egui の galley / `CCursor` は **char** 添字。
//! * `&str` のスライスは **byte** 添字。
//!
//! 3 つを混ぜると日本語・絵文字の行で必ず壊れるので、変換は
//! [`crate::lsp`] の既存関数 (`lsp_pos_to_char_index` / `range_to_byte_span`)
//! だけを通す。このモジュールは新しい変換規則を作らない。

use crate::editor::combine_hash;
use crate::editor_ops;
use crate::lsp::{self, Diagnostic};
use crate::theme::{snap_len, Theme};
use eframe::egui::{Color32, Pos2};
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// 診断 → 本文のスパン
// ---------------------------------------------------------------------------

/// 本文に波線を引くための 1 件分のスパン (**char** 添字, `start < end`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagSpan {
    pub start: usize,
    pub end: usize,
    /// 1=error 2=warning 3=information 4=hint
    pub severity: u8,
    /// 元の診断配列の添字 (メッセージの引き当て用)
    pub index: usize,
}

/// 診断 1 件を **char** スパンへ写す。
///
/// * 行末を超える列は行末へクランプされる (`lsp_pos_to_char_index` の規則)。
/// * 空範囲 (`start == end`。「ここに `;` が要る」系) は幅 0 では波線が
///   引けないので **1 文字ぶんに広げる** — 次の文字、それが改行/末尾なら
///   直前の文字へ。どちらも取れなければ空のまま返す (呼び出し側が落とす)。
pub fn char_span(text: &str, d: &Diagnostic) -> (usize, usize) {
    let n = text.chars().count();
    let a = lsp::lsp_pos_to_char_index(text, d.line, d.col).min(n);
    let b = lsp::lsp_pos_to_char_index(text, d.end_line, d.end_col).min(n);
    let (mut a, mut b) = if a <= b { (a, b) } else { (b, a) };
    if a == b {
        // 1 回の走査で「a の文字」と「a-1 の文字」を取る (nth の二度引きを避ける)
        let (mut cur, mut prev) = (None, None);
        for (i, ch) in text.chars().enumerate() {
            if i + 1 == a {
                prev = Some(ch);
            }
            if i == a {
                cur = Some(ch);
                break;
            }
        }
        match cur {
            Some(c) if c != '\n' => b = a + 1,
            _ => {
                if a > 0 && prev.is_some_and(|c| c != '\n') {
                    a -= 1;
                }
            }
        }
    }
    (a, b)
}

/// 診断の一覧 → 本文へ塗る char スパン。
///
/// 幅 0 に潰れたもの (空行の空範囲など) は落とす — 描けないものは持たない。
/// 並びは **深刻度の低い順** = 後から塗る error が上に来る。
pub fn spans(text: &str, diags: &[Diagnostic]) -> Vec<DiagSpan> {
    let mut v: Vec<DiagSpan> = diags
        .iter()
        .enumerate()
        .filter_map(|(index, d)| {
            let (start, end) = char_span(text, d);
            (start < end).then_some(DiagSpan {
                start,
                end,
                severity: d.severity,
                index,
            })
        })
        .collect();
    v.sort_by(|x, y| y.severity.cmp(&x.severity).then(x.start.cmp(&y.start)));
    v
}

/// char 添字 `at` を覆う診断のうち **最も深刻なもの** の元添字。
pub fn diag_at(spans: &[DiagSpan], at: usize) -> Option<usize> {
    spans
        .iter()
        .filter(|s| s.start <= at && at < s.end)
        .min_by_key(|s| s.severity)
        .map(|s| s.index)
}

// ---------------------------------------------------------------------------
// アクティブバッファの診断キャッシュ
// ---------------------------------------------------------------------------

/// アクティブバッファの診断キャッシュ。
///
/// 「行 → 最悪 severity」(ガター / ミニマップ用) と **範囲付きの診断そのもの**
/// (本文の波線・ホバー・行末メッセージ用) を 1 か所で持つ。
///
/// 再構築するのは鍵が変わったフレームだけ:
/// * 診断の内容が変わった → 行マップと件数を作り直す
/// * 診断 **または本文** が変わった → char スパンを作り直す
///   (打鍵で本文がずれたら、サーバーの応答を待たずに波線もずれるべき)
///
/// それ以外のフレームでは `Vec` にも `HashMap` にも触らない (設計原則 3)。
pub struct DiagCache {
    /// 診断の内容ハッシュ (`u64::MAX` = 未構築の番兵)
    key: u64,
    /// スパンを組んだときの (診断, 本文) の複合ハッシュ
    span_key: u64,
    /// 行 → その行で最も深刻な severity
    pub by_line: HashMap<usize, u8>,
    pub errors: usize,
    pub warnings: usize,
    /// 診断そのもの。`Arc` なのでフレームごとの複製は起きない。
    pub items: Arc<Vec<Diagnostic>>,
    /// 本文へ塗る char スパン (深刻度の低い順)
    pub spans: Vec<DiagSpan>,
}

impl Default for DiagCache {
    fn default() -> Self {
        DiagCache {
            key: u64::MAX,
            span_key: u64::MAX,
            by_line: HashMap::new(),
            errors: 0,
            warnings: 0,
            items: Arc::new(Vec::new()),
            spans: Vec::new(),
        }
    }
}

/// 診断列の内容ハッシュ。**範囲もメッセージも混ぜる** — 行だけを混ぜると
/// 「同じ行の中で列がずれた」変更を取りこぼして波線が古いまま残る。
fn diags_hash(diags: &[Diagnostic]) -> u64 {
    let mut h = diags.len() as u64;
    for d in diags {
        h = combine_hash(h, d.line as u64);
        h = combine_hash(h, ((d.col as u64) << 32) | d.end_col as u64);
        h = combine_hash(h, ((d.end_line as u64) << 8) | d.severity as u64);
        h = combine_hash(h, d.message.len() as u64);
        h = combine_hash(h, crate::editor::hash_str(&d.message));
    }
    h
}

impl DiagCache {
    /// 診断 / 本文が変わったときだけ組み直す。戻り値は「作り直したか」。
    pub fn refresh(&mut self, diags: Arc<Vec<Diagnostic>>, text: &str, text_hash: u64) -> bool {
        let key = diags_hash(&diags);
        let span_key = combine_hash(key, text_hash);
        if key == self.key && span_key == self.span_key {
            return false;
        }
        if key != self.key {
            let mut by_line: HashMap<usize, u8> = HashMap::new();
            let (mut errors, mut warnings) = (0usize, 0usize);
            for d in diags.iter() {
                match d.severity {
                    1 => errors += 1,
                    2 => warnings += 1,
                    _ => {}
                }
                let e = by_line.entry(d.line).or_insert(4);
                if d.severity < *e {
                    *e = d.severity;
                }
            }
            self.by_line = by_line;
            self.errors = errors;
            self.warnings = warnings;
            self.key = key;
        }
        self.spans = spans(text, &diags);
        self.span_key = span_key;
        self.items = diags;
        true
    }

    /// 元の診断配列の添字 → 診断。
    pub fn get(&self, index: usize) -> Option<&Diagnostic> {
        self.items.get(index)
    }
}

/// severity → 色。**テーマから取る** (色はどこにもベタ書きしない)。
pub fn severity_color(theme: &Theme, severity: u8) -> Color32 {
    match severity {
        1 => theme.err,
        2 => theme.warn,
        3 => theme.accent,
        _ => theme.text_dim,
    }
}

/// ホバー/問題パネルに出す 1 行ラベル。サーバー名 (`source`) があれば前に付ける。
pub fn labeled_message(d: &Diagnostic) -> String {
    if d.source.is_empty() {
        d.message.clone()
    } else {
        format!("{}: {}", d.source, d.message)
    }
}

/// カーソル行の行末に出すメッセージ (Error Lens 相当)。
///
/// 同じ行に複数あるときは **最も深刻な 1 件**。`max_chars` は残り幅から
/// 決まる表示可能文字数で、0 なら出さない (幅が無いのに書かない)。
pub fn inline_message(diags: &[Diagnostic], line: usize, max_chars: usize) -> Option<(String, u8)> {
    if max_chars == 0 {
        return None;
    }
    let d = diags
        .iter()
        .filter(|d| d.line.min(d.end_line) <= line && line <= d.line.max(d.end_line))
        .min_by_key(|d| (d.severity, d.col))?;
    let s = lsp::one_line_label(&d.message, max_chars);
    (!s.is_empty()).then_some((s, d.severity))
}

/// カーソル位置 `from` から見て次 (`forward`) / 前の診断の添字。
///
/// 端では巻き戻る (VS Code の F8 / ⇧F8 と同じ)。診断が無ければ `None`。
pub fn step_diag(diags: &[Diagnostic], from: lsp::Position, forward: bool) -> Option<usize> {
    if diags.is_empty() {
        return None;
    }
    let key = |i: usize| lsp::Position::new(diags[i].line, diags[i].col);
    let mut order: Vec<usize> = (0..diags.len()).collect();
    order.sort_by_key(|&i| (key(i), diags[i].severity, i));
    if forward {
        Some(
            order
                .iter()
                .copied()
                .find(|&i| key(i) > from)
                .unwrap_or(order[0]),
        )
    } else {
        Some(
            order
                .iter()
                .copied()
                .rev()
                .find(|&i| key(i) < from)
                .unwrap_or_else(|| *order.last().expect("空でないことは確認済み")),
        )
    }
}

// ---------------------------------------------------------------------------
// 波線 (純粋な幾何)
// ---------------------------------------------------------------------------

/// 波線の振幅 (pt)。行の下端から下へ `AMP` だけ振れる。
pub const SQUIGGLE_AMP: f32 = 1.5;
/// 波長 (pt)。半波長ごとに 1 頂点を置く。
pub const SQUIGGLE_WAVE: f32 = 4.0;
/// 1 本の波線が持てる頂点の上限 (極端に長い行で頂点を作りすぎない)。
pub const SQUIGGLE_MAX_POINTS: usize = 512;
/// 1 件の診断が波線を引ける視覚行の上限。巨大な範囲を返すサーバーで
/// 1 フレームを潰さないための天井 (画面に入る行数より十分多い)。
pub const SQUIGGLE_MAX_ROWS: usize = 64;

/// `(x0, x1, y, 振幅, 波長)` → 波線の頂点列。
///
/// 不変条件 (テーブルテストで固定):
/// * すべての頂点の x は `[min(x0,x1), max(x0,x1)]` に収まる (可用領域を出ない)
/// * すべての頂点の y は `[y-amp, y+amp]` に収まる
/// * 先頭は左端、末尾は右端 (途中で切れて見えない、が起きない)
/// * 座標は `ppp` で整数ピクセルにスナップする (端末セルと同じ流儀。
///   スナップしないと 100% 表示で波の間隔が 2/1/2/1 px と揺れる)
/// * 振幅か波長が使えない値なら **下線へ縮退** する (何も出さないより読める)
pub fn squiggle_points(x0: f32, x1: f32, y: f32, amp: f32, wave: f32, ppp: f32) -> Vec<Pos2> {
    if !x0.is_finite() || !x1.is_finite() || !y.is_finite() {
        return Vec::new();
    }
    let (lo, hi) = (x0.min(x1), x0.max(x1));
    let (lo, hi, y) = (snap_len(lo, ppp), snap_len(hi, ppp), snap_len(y, ppp));
    if hi <= lo {
        return Vec::new();
    }
    let amp = if amp.is_finite() { amp.max(0.0) } else { 0.0 };
    let step = if wave.is_finite() && wave > 0.0 {
        wave * 0.5
    } else {
        0.0
    };
    if amp <= 0.0 || step <= 0.0 {
        return vec![Pos2::new(lo, y), Pos2::new(hi, y)];
    }
    let n = ((((hi - lo) / step).ceil() as usize).saturating_add(1)).min(SQUIGGLE_MAX_POINTS);
    let mut pts: Vec<Pos2> = Vec::with_capacity(n);
    for i in 0..n {
        // snap 後も lo..hi を出ない: hi は既にスナップ済みなので round は超えない
        let x = snap_len((lo + i as f32 * step).min(hi), ppp).clamp(lo, hi);
        let dy = if i % 2 == 0 { amp } else { -amp };
        pts.push(Pos2::new(x, y + dy));
        if x >= hi {
            break;
        }
    }
    // 右端で必ず閉じる (頂点上限で打ち切られた場合も含む)
    if pts.last().is_some_and(|p| p.x < hi) {
        let dy = if pts.len().is_multiple_of(2) {
            amp
        } else {
            -amp
        };
        pts.push(Pos2::new(hi, y + dy));
    }
    pts
}

// ---------------------------------------------------------------------------
// インレイヒント (型・引数名) の行内位置
// ---------------------------------------------------------------------------

/// 1 行に出すヒントの上限。tsserver は 1 行の関数呼び出しに引数名ヒントを
/// 十数個返してくるので、行末の 1 行が伸びきる前にここで切る。
pub const INLAY_MAX_PER_LINE: usize = 8;

/// ヒント 1 件のラベルの最大文字数。長いジェネリック型
/// (`Vec<HashMap<String, Vec<u8>>>`) をそのまま出すと行末が読めなくなる。
/// 切り詰めは `lsp::one_line_label` に任せるので **char 単位** = 日本語でも
/// 文字の途中で切れない。
pub const INLAY_LABEL_MAX: usize = 32;

/// 行末へ複数並べるときの区切り。
const INLAY_SEP: &str = " ";

/// 本文の **char** 添字へ写した「行内の挿入位置つきヒント」。
///
/// `at` は「本文のこの char の**直前**にヒントが入るべき」位置。実際に本文へ
/// 差し込むことはしない ([`inlay_line_text`] の説明を参照)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayView {
    /// 挿入位置 (本文の char 添字)
    pub at: usize,
    /// 0-based の原文行
    pub line: usize,
    /// 画面に出す文字列 (padding 反映済み・畳み済み)
    pub text: String,
    /// [`lsp::INLAY_KIND_TYPE`] / [`lsp::INLAY_KIND_PARAMETER`] / 0
    pub kind: u8,
}

/// LSP のヒント列 → 本文の char 添字つきビュー。
///
/// * 位置は必ず [`lsp::lsp_pos_to_char_index`] を通す (新しい変換規則は作らない)。
///   行末を超える桁は行末へ、行が足りなければ本文末尾へクランプされる。
/// * 本文の行数を超える行のヒントは**落とす** — クランプすると全部が最終行の
///   末尾に積み上がり、まったく関係ない行の行末が埋まる。
/// * `paddingLeft` / `paddingRight` は前後の半角空白として畳み込む。
/// * ラベルが空 (または空白のみ) の件は落とす。
/// * 位置順に安定整列し、1 行あたり [`INLAY_MAX_PER_LINE`] 件で切る。
pub fn inlay_views(text: &str, hints: &[lsp::InlayHint]) -> Vec<InlayView> {
    if hints.is_empty() {
        return Vec::new();
    }
    let lines = text.split('\n').count(); // 空文字でも 1 行として数える
    let n = text.chars().count();
    let mut out: Vec<InlayView> = Vec::new();
    for h in hints {
        if h.line >= lines {
            continue;
        }
        let label = lsp::one_line_label(&h.label, INLAY_LABEL_MAX);
        if label.trim().is_empty() {
            continue;
        }
        let mut s = String::with_capacity(label.len() + 2);
        if h.padding_left {
            s.push(' ');
        }
        s.push_str(&label);
        if h.padding_right {
            s.push(' ');
        }
        out.push(InlayView {
            at: lsp::lsp_pos_to_char_index(text, h.line, h.character).min(n),
            line: h.line,
            text: s,
            kind: h.kind,
        });
    }
    // 位置順。同じ位置に複数来たらサーバーの返した順のまま (安定ソート)
    out.sort_by_key(|v| (v.line, v.at));
    // 行あたりの件数で切る (1 行が伸びきらない)
    let mut kept: Vec<InlayView> = Vec::with_capacity(out.len());
    let (mut cur_line, mut cnt) = (usize::MAX, 0usize);
    for v in out {
        if v.line != cur_line {
            cur_line = v.line;
            cnt = 0;
        }
        if cnt < INLAY_MAX_PER_LINE {
            cnt += 1;
            kept.push(v);
        }
    }
    kept
}

/// `line` のヒントを **行末へ出す 1 行**にまとめる。`max_chars` に収まらなければ
/// 末尾を畳み、幅が無ければ `None` (幅が無いのに書かない)。
///
/// ## なぜ行末なのか (本文へ混ぜない理由)
///
/// egui の `TextEdit` は **本文そのもの**から galley を組み、キャレットも選択も
/// マウス当たり判定も galley の char 添字で表される。ヒントを本文へ混ぜたり
/// レイアウタで足したりすると、galley の添字が原文とずれて
/// **カーソル位置・選択・クリック位置が全部壊れる** (折りたたみ表示で
/// 「char 添字がずれるので波線を出さない」としているのと同じ理由)。
///
/// かといって挿入位置へそのまま重ね描きすると、右隣のコードを覆ってしまう。
/// そこで **行末のマージンへまとめて出し、挿入位置には細い縦の目印だけを打つ**。
/// 本文の galley には一切触らないので添字は原文のまま、どのヒントが行の
/// どこに属すかは目印の x で読める。
pub fn inlay_line_text(views: &[InlayView], line: usize, max_chars: usize) -> Option<String> {
    if max_chars == 0 {
        return None;
    }
    let mut joined = String::new();
    for v in views.iter().filter(|v| v.line == line) {
        if !joined.is_empty() {
            joined.push_str(INLAY_SEP);
        }
        joined.push_str(v.text.trim_end());
    }
    let s = lsp::one_line_label(&joined, max_chars);
    (!s.is_empty()).then_some(s)
}

/// `line` のヒントの (挿入位置 = 本文の char 添字, 種別)。
/// 描画側が縦の目印を打つために使う。件数は [`inlay_views`] で既に切ってある。
pub fn inlay_marks(views: &[InlayView], line: usize) -> Vec<(usize, u8)> {
    views
        .iter()
        .filter(|v| v.line == line)
        .map(|v| (v.at, v.kind))
        .collect()
}

/// インレイヒントの色。**テーマから取る** (色はどこにもベタ書きしない)。
/// 型と引数名で少しだけ色を変える (どちらも本文より淡い)。
pub fn inlay_color(theme: &Theme, kind: u8) -> Color32 {
    match kind {
        lsp::INLAY_KIND_PARAMETER => theme.accent,
        _ => theme.text_dim,
    }
}

/// ヒント列の内容ハッシュ。**位置もラベルも混ぜる** — 件数だけを見ると
/// 「同じ数だけ返ってきたが中身が変わった」応答を取りこぼす。
fn inlay_hash(hints: &[lsp::InlayHint]) -> u64 {
    let mut h = hints.len() as u64;
    for x in hints {
        h = combine_hash(h, ((x.line as u64) << 20) | x.character as u64);
        h = combine_hash(h, crate::editor::hash_str(&x.label));
        h = combine_hash(
            h,
            (x.kind as u64) | ((x.padding_left as u64) << 8) | ((x.padding_right as u64) << 9),
        );
    }
    h
}

/// アクティブバッファのインレイヒントのキャッシュ ([`DiagCache`] と同じ流儀)。
///
/// 組み直すのは **ヒントか本文が変わったフレームだけ**。それ以外では
/// `Vec` を 1 個も確保しない (設計原則 3: アイドル時のコストはゼロ)。
/// `views` を `Arc` で持つのは、描画側が毎フレーム clone しても
/// 参照カウントの増減だけで済ませるため。
pub struct InlayCache {
    /// (ヒント, 本文) の複合ハッシュ (`u64::MAX` = 未構築の番兵)
    key: u64,
    pub views: Arc<Vec<InlayView>>,
}

impl Default for InlayCache {
    fn default() -> Self {
        InlayCache {
            key: u64::MAX,
            views: Arc::new(Vec::new()),
        }
    }
}

impl InlayCache {
    /// ヒント / 本文が変わったときだけ組み直す。戻り値は「作り直したか」。
    pub fn refresh(&mut self, hints: &[lsp::InlayHint], text: &str, text_hash: u64) -> bool {
        let key = combine_hash(inlay_hash(hints), text_hash);
        if key == self.key {
            return false;
        }
        self.views = Arc::new(inlay_views(text, hints));
        self.key = key;
        true
    }

    /// 全部捨てる (機能 OFF / タブに LSP が無い / 応答待ち)。
    /// 既に空なら**何も確保しない** (毎フレーム通っても無料)。
    pub fn clear(&mut self) {
        if self.key != u64::MAX {
            self.views = Arc::new(Vec::new());
            self.key = u64::MAX;
        }
    }
}

// ---------------------------------------------------------------------------
// 対応括弧
// ---------------------------------------------------------------------------

/// 強調する括弧の位置 (char 添字)。`other` が `None` なら対応が無い括弧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BracketHl {
    /// カーソルに隣接している方の括弧
    pub at: usize,
    /// 対応する相手 (`editor_ops::matching_bracket` の結果)
    pub other: Option<usize>,
}

/// 強調対象になる括弧文字。
///
/// **対応探索そのものは持たない** — 相手を見つけるのは
/// [`editor_ops::matching_bracket`] ただ 1 つで、ここは「カーソルの隣が
/// 括弧かどうか」だけを見る。`<>` は比較演算子と区別できないので対象外
/// (`matching_bracket` と同じ)。両者がずれていないことは
/// `対応括弧の文字表はマッチャと一致する` が見張る。
pub const BRACKET_CHARS: [char; 6] = ['(', ')', '[', ']', '{', '}'];

/// カーソルに隣接する括弧と、その相手。
///
/// 隣接の判定は `matching_bracket` と**同じ順** (カーソル直後 → 直前) なので、
/// 「⇧⌘\ で飛ぶ先」と「光る場所」が食い違わない。
/// 文字列リテラル / コメントの中は考慮しない (素朴なネスト数え = 既存の挙動)。
pub fn bracket_hl(text: &str, cursor_char: usize) -> Option<BracketHl> {
    let is_br = |c: char| BRACKET_CHARS.contains(&c);
    let cursor = cursor_char;
    let (mut cur, mut prev) = (None, None);
    for (i, ch) in text.chars().enumerate() {
        if i + 1 == cursor {
            prev = Some(ch);
        }
        if i == cursor {
            cur = Some(ch);
            break;
        }
    }
    let at = if cur.is_some_and(is_br) {
        cursor
    } else if cursor > 0 && prev.is_some_and(is_br) {
        cursor - 1
    } else {
        return None;
    };
    Some(BracketHl {
        at,
        other: editor_ops::matching_bracket(text, cursor),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// インレイヒントを 1 件作る (位置は LSP 座標 = 0-based 行 / UTF-16 桁)。
    fn ih(line: usize, character: usize, label: &str, kind: u8) -> lsp::InlayHint {
        lsp::InlayHint {
            line,
            character,
            label: label.into(),
            kind,
            padding_left: false,
            padding_right: false,
        }
    }

    /// **LSP 座標 → 本文の char 添字**。テーブルで固定する。
    ///
    /// 変換は `lsp::lsp_pos_to_char_index` 1 本しか通さないので、日本語 /
    /// 絵文字の行でも「UTF-16 桁」と「char 添字」がずれない。
    #[test]
    fn inlay_views_は行内位置をchar添字へ写す() {
        // 行 1 は日本語 (1 文字 = UTF-16 1)、行 2 は絵文字 (1 文字 = UTF-16 2)
        let text = "let x = 1;\nlet 名前 = 2;\nlet 🙂 = 3;";
        // (行, UTF-16 桁, 期待する char 添字, 説明)
        let table: &[(usize, usize, usize, &str)] = &[
            (0, 0, 0, "行頭"),
            (0, 5, 5, "ASCII 行の途中"),
            (0, 99, 10, "行末を超える桁は行末へクランプ"),
            (1, 4, 15, "日本語の手前"),
            (1, 6, 17, "日本語 2 文字ぶん = UTF-16 2"),
            (2, 4, 27, "絵文字の手前"),
            (2, 6, 28, "サロゲートペアの後ろ = char は 1 つだけ進む"),
        ];
        for (line, ch, want, why) in table {
            let v = inlay_views(text, &[ih(*line, *ch, ": T", lsp::INLAY_KIND_TYPE)]);
            assert_eq!(v.len(), 1, "{why}");
            assert_eq!(v[0].at, *want, "{why}");
            assert_eq!(v[0].line, *line, "{why}");
        }
    }

    /// 落とすもの・畳むもののテーブル。
    #[test]
    fn inlay_views_は空ラベルと行外を落としpaddingを畳む() {
        let text = "a\nb\n"; // 3 行 (最終行は空)
        let long = "X".repeat(INLAY_LABEL_MAX + 20);
        let mut pad = ih(0, 1, "name:", lsp::INLAY_KIND_PARAMETER);
        pad.padding_left = true;
        pad.padding_right = true;
        let hints = vec![
            ih(0, 0, "", lsp::INLAY_KIND_TYPE),       // 空 → 落とす
            ih(0, 0, "   ", lsp::INLAY_KIND_TYPE),    // 空白のみ → 落とす
            ih(0, 0, "a\n\tb", lsp::INLAY_KIND_TYPE), // 改行/タブは 1 個の空白へ
            ih(9, 0, ": far", lsp::INLAY_KIND_TYPE),  // 本文の行数外 → 落とす
            ih(0, 0, &long, lsp::INLAY_KIND_TYPE),    // 長すぎ → 畳む
            pad,
        ];
        let v = inlay_views(text, &hints);
        assert_eq!(v.len(), 3, "空 2 件と行外 1 件が落ちる: {v:?}");
        assert_eq!(v[0].text, "a b");
        assert_eq!(v[1].text.chars().count(), INLAY_LABEL_MAX);
        assert_eq!(v[2].text, " name: ", "padding は前後の半角空白になる");
        assert_eq!(v[2].kind, lsp::INLAY_KIND_PARAMETER);
    }

    /// 1 行あたりの件数は必ず上限で切る (行末の 1 行が伸びきらない)。
    /// 並びは位置順で、行をまたいでも取り違えない。
    #[test]
    fn inlay_views_は位置順に整列して行ごとに上限で切る() {
        let text = "aaaaaaaaaaaaaaaaaaaa\nbbbbbbbbbbbbbbbbbbbb\n";
        let mut hints = Vec::new();
        // 行 0 は上限より多く、わざと逆順に積む
        for i in (0..INLAY_MAX_PER_LINE + 4).rev() {
            hints.push(ih(0, i, &format!("t{i}"), lsp::INLAY_KIND_TYPE));
        }
        hints.push(ih(1, 3, "z", lsp::INLAY_KIND_PARAMETER));
        let v = inlay_views(text, &hints);
        assert_eq!(v.len(), INLAY_MAX_PER_LINE + 1, "行 0 が上限で切れる");
        // 行 0 は位置順 (0,1,2,...) に整列している
        for (i, view) in v.iter().take(INLAY_MAX_PER_LINE).enumerate() {
            assert_eq!(view.line, 0);
            assert_eq!(view.at, i);
            assert_eq!(view.text, format!("t{i}"));
        }
        // 行 1 の 1 件は上限に巻き込まれない
        assert_eq!(v[INLAY_MAX_PER_LINE].line, 1);
        assert_eq!(inlay_marks(&v, 1), vec![(24, lsp::INLAY_KIND_PARAMETER)]);
        assert_eq!(inlay_marks(&v, 2), Vec::<(usize, u8)>::new());
        assert!(inlay_views(text, &[]).is_empty());
    }

    /// 行末に出す 1 行のテーブル: 幅が無ければ出さない・足りなければ畳む。
    #[test]
    fn inlay_line_textは可用幅に必ず収まる() {
        let text = "let a = f(1, 2);\nlet b = 3;\n";
        let views = inlay_views(
            text,
            &[
                ih(0, 10, "x:", lsp::INLAY_KIND_PARAMETER),
                ih(0, 13, "y:", lsp::INLAY_KIND_PARAMETER),
                ih(1, 5, ": i32", lsp::INLAY_KIND_TYPE),
            ],
        );
        // (行, 可用文字数, 期待, 説明)
        let table: &[(usize, usize, Option<&str>, &str)] = &[
            (0, 0, None, "幅が無いのに書かない"),
            (
                0,
                32,
                Some("x: y:"),
                "同じ行のヒントは区切って 1 行にまとめる",
            ),
            (0, 4, Some("x: …"), "収まらなければ末尾を畳む"),
            (1, 32, Some(": i32"), "1 件なら区切りは入らない"),
            (2, 32, None, "ヒントの無い行には何も出さない"),
        ];
        for (line, max, want, why) in table {
            let got = inlay_line_text(&views, *line, *max);
            assert_eq!(got.as_deref(), *want, "{why}");
            // どの幅でも見切れない = 必ず可用文字数以内
            if let Some(s) = got {
                assert!(s.chars().count() <= *max, "{why}: {s:?} が幅を超えた");
            }
        }
    }

    /// キャッシュは (ヒント, 本文) が変わったフレームだけ組み直す
    /// (アイドル時のコストはゼロ = 設計原則 3)。
    #[test]
    fn inlay_cacheは変わらないフレームで組み直さない() {
        let text = "let a = 1;\n";
        let hints = vec![ih(0, 5, ": i32", lsp::INLAY_KIND_TYPE)];
        let mut c = InlayCache::default();
        assert!(c.refresh(&hints, text, 1), "初回は必ず組む");
        assert_eq!(c.views.len(), 1);
        assert!(!c.refresh(&hints, text, 1), "同じなら組み直さない");
        assert!(c.refresh(&hints, text, 2), "本文が変われば組み直す");
        // 件数が同じでも中身が変われば組み直す (len だけ見ていると取りこぼす)
        let moved = vec![ih(0, 9, ": i32", lsp::INLAY_KIND_TYPE)];
        assert!(c.refresh(&moved, text, 2), "位置が変われば組み直す");
        let renamed = vec![ih(0, 9, ": u8", lsp::INLAY_KIND_TYPE)];
        assert!(c.refresh(&renamed, text, 2), "ラベルが変われば組み直す");
        c.clear();
        assert!(c.views.is_empty());
        assert!(c.refresh(&renamed, text, 2), "clear の後は組み直す");
    }

    /// 診断の範囲を **byte** スパンへ写す (`&text[a..b]` が成立する)。
    ///
    /// 本番の描画は char 添字 ([`char_span`]) で足りるので、byte 変換は
    /// `lsp::range_to_byte_span` をそのまま使う。ここで叩いているのは
    /// **診断の範囲が byte 境界を割らない**ことを固定するため。
    fn byte_span(text: &str, d: &Diagnostic) -> (usize, usize) {
        lsp::range_to_byte_span(
            text,
            &lsp::Range::new(
                lsp::Position::new(d.line, d.col),
                lsp::Position::new(d.end_line, d.end_col),
            ),
        )
    }

    fn diag(line: usize, col: usize, end_line: usize, end_col: usize, severity: u8) -> Diagnostic {
        Diagnostic {
            line,
            col,
            end_line,
            end_col,
            severity,
            message: "m".into(),
            source: String::new(),
        }
    }

    // ── 範囲 → スパン ────────────────────────────────────────────────

    #[test]
    fn 診断の範囲は行内に収まる() {
        let t = "let a = 1;\nlet b = 2;\n";
        let (s, e) = char_span(t, &diag(0, 4, 0, 5, 1));
        assert_eq!(&t.chars().skip(s).take(e - s).collect::<String>(), "a");
        // 2 行目
        let (s, e) = char_span(t, &diag(1, 4, 1, 5, 1));
        assert_eq!(&t.chars().skip(s).take(e - s).collect::<String>(), "b");
    }

    #[test]
    fn 複数行に跨る範囲は全体を覆う() {
        let t = "fn f() {\n    let x = 1;\n}\n";
        let (s, e) = char_span(t, &diag(0, 7, 2, 1, 1));
        let got: String = t.chars().skip(s).take(e - s).collect();
        assert!(got.starts_with('{'), "開始が {{ でない: {got:?}");
        assert!(got.ends_with('}'), "終了が }} でない: {got:?}");
        assert!(got.contains('\n'), "改行を跨いでいない: {got:?}");
    }

    #[test]
    fn 範囲が行末を超えたら行末へクランプされる() {
        let t = "abc\ndef\n";
        // 1 行目は 3 文字しかないのに col 999 を要求してくるサーバーがある
        let (s, e) = char_span(t, &diag(0, 1, 0, 999, 2));
        assert_eq!((s, e), (1, 3), "改行の手前で止まっていない");
        // 行そのものが存在しない
        let (s, e) = char_span(t, &diag(99, 0, 99, 5, 2));
        assert_eq!(s, e, "存在しない行は空スパン");
    }

    #[test]
    fn 空範囲は一文字ぶんに広がる() {
        let t = "let a = 1\nlet b = 2\n";
        // 行末の空範囲 (「; が要る」) → 直前の文字へ広げる
        let (s, e) = char_span(t, &diag(0, 9, 0, 9, 1));
        assert_eq!(e - s, 1, "行末の空範囲が広がっていない");
        assert_eq!(t.chars().nth(s), Some('1'));
        // 行中の空範囲 → 次の文字へ広げる
        let (s, e) = char_span(t, &diag(0, 4, 0, 4, 1));
        assert_eq!(e - s, 1);
        assert_eq!(t.chars().nth(s), Some('a'));
    }

    #[test]
    fn 空行の空範囲は幅ゼロのまま落とされる() {
        let t = "\n\n";
        let (s, e) = char_span(t, &diag(0, 0, 0, 0, 1));
        assert_eq!(s, e, "空行では広げられない");
        assert!(
            spans(t, &[diag(0, 0, 0, 0, 1)]).is_empty(),
            "描けないスパンを持ち続けている"
        );
    }

    #[test]
    fn start_と_end_が逆でも壊れない() {
        let t = "abcdef\n";
        let (s, e) = char_span(t, &diag(0, 4, 0, 1, 1));
        assert_eq!((s, e), (1, 4));
    }

    // ── UTF-8 / UTF-16 の境界 ────────────────────────────────────────

    #[test]
    fn 日本語の行でバイト境界スライスが_panic_しない() {
        let t = "let こんにちは = 1;\nlet 世界 = 2;\n";
        for d in [
            diag(0, 0, 0, 100, 1),
            diag(0, 4, 0, 9, 1),
            diag(0, 5, 1, 3, 2),
            diag(1, 4, 1, 6, 3),
            diag(0, 0, 5, 0, 1),
        ] {
            let (a, b) = byte_span(t, &d);
            // panic しないこと自体が主張 (境界でなければここで落ちる)
            let _ = &t[a..b];
            assert!(t.is_char_boundary(a) && t.is_char_boundary(b));
            let (s, e) = char_span(t, &d);
            let _: String = t.chars().skip(s).take(e.saturating_sub(s)).collect();
        }
    }

    #[test]
    fn utf16_の列がそのままバイト位置になっていない() {
        // "世界" は UTF-16 で 2 code unit / UTF-8 で 6 byte
        let t = "let 世界 = 1;\n";
        // col 4..6 = 「世界」
        let (a, b) = byte_span(t, &diag(0, 4, 0, 6, 1));
        assert_eq!(&t[a..b], "世界", "UTF-16 列を byte として扱っている");
        let (s, e) = char_span(t, &diag(0, 4, 0, 6, 1));
        assert_eq!(t.chars().skip(s).take(e - s).collect::<String>(), "世界");
    }

    #[test]
    fn 絵文字のサロゲートペアで列がずれない() {
        // 😀 は UTF-16 で 2 code unit / char としては 1 個 / UTF-8 で 4 byte
        let t = "a😀b\n";
        // col 1..3 = 😀 (UTF-16 では 1 から 2 code unit ぶん)
        let (a, b) = byte_span(t, &diag(0, 1, 0, 3, 1));
        assert_eq!(&t[a..b], "😀");
        let (s, e) = char_span(t, &diag(0, 1, 0, 3, 1));
        assert_eq!(t.chars().skip(s).take(e - s).collect::<String>(), "😀");
        // サロゲートペアの**途中** (col 2) を指されても char 境界へ丸める
        let (a, b) = byte_span(t, &diag(0, 2, 0, 4, 1));
        let _ = &t[a..b]; // panic しないこと
        assert!(t.is_char_boundary(a) && t.is_char_boundary(b));
    }

    #[test]
    fn 診断の並びは深刻度の低い順_エラーが最後に塗られる() {
        let t = "aaa bbb ccc\n";
        let v = spans(
            t,
            &[
                diag(0, 0, 0, 3, 1),
                diag(0, 4, 0, 7, 4),
                diag(0, 8, 0, 11, 2),
            ],
        );
        let sev: Vec<u8> = v.iter().map(|s| s.severity).collect();
        assert_eq!(sev, vec![4, 2, 1], "error が最後 (= 一番上) に来ていない");
        assert_eq!(diag_at(&v, 1), Some(0), "error のスパンを引けない");
        assert_eq!(diag_at(&v, 5), Some(1));
        assert_eq!(diag_at(&v, 20), None, "範囲外を拾っている");
    }

    #[test]
    fn 重なった診断はより深刻な方を返す() {
        let t = "abcdef\n";
        let v = spans(t, &[diag(0, 0, 0, 6, 3), diag(0, 2, 0, 4, 1)]);
        assert_eq!(diag_at(&v, 3), Some(1), "error より info を優先している");
        assert_eq!(diag_at(&v, 0), Some(0));
    }

    // ── 行末メッセージ ───────────────────────────────────────────────

    #[test]
    fn 行末メッセージは最も深刻な一件を幅ぶんだけ返す() {
        let mut a = diag(3, 0, 3, 4, 2);
        a.message = "warn here".into();
        let mut b = diag(3, 6, 3, 9, 1);
        b.message = "error here".into();
        let (s, sev) = inline_message(&[a.clone(), b], 3, 40).expect("出るはず");
        assert_eq!(sev, 1);
        assert_eq!(s, "error here");
        // 幅が足りなければ切り詰める / 0 なら出さない
        let (s, _) = inline_message(&[a.clone()], 3, 4).expect("出るはず");
        assert!(s.chars().count() <= 4, "幅を超えた: {s:?}");
        assert!(inline_message(&[a.clone()], 3, 0).is_none());
        assert!(inline_message(&[a], 9, 40).is_none(), "別の行に出している");
    }

    #[test]
    fn 行末メッセージは複数行診断の途中の行にも出る() {
        let mut d = diag(2, 0, 5, 1, 1);
        d.message = "spans lines".into();
        for l in 2..=5 {
            assert!(inline_message(&[d.clone()], l, 40).is_some(), "line {l}");
        }
        assert!(inline_message(&[d], 6, 40).is_none());
    }

    #[test]
    fn ソース名は_labeled_message_だけに付く() {
        let mut d = diag(0, 0, 0, 1, 1);
        d.message = "boom".into();
        d.source = "rustc".into();
        assert_eq!(labeled_message(&d), "rustc: boom");
        assert_eq!(inline_message(&[d.clone()], 0, 40).unwrap().0, "boom");
        let mut e = d;
        e.source = String::new();
        assert_eq!(labeled_message(&e), "boom");
    }

    // ── 次/前の診断 ─────────────────────────────────────────────────

    #[test]
    fn 次と前の診断は端で巻き戻る() {
        let ds = [
            diag(1, 2, 1, 5, 1),
            diag(4, 0, 4, 3, 2),
            diag(9, 1, 9, 2, 3),
        ];
        let p = lsp::Position::new;
        assert_eq!(step_diag(&ds, p(0, 0), true), Some(0));
        assert_eq!(step_diag(&ds, p(1, 2), true), Some(1), "同じ位置で止まった");
        assert_eq!(
            step_diag(&ds, p(9, 1), true),
            Some(0),
            "末尾から巻き戻らない"
        );
        assert_eq!(step_diag(&ds, p(9, 9), false), Some(2));
        assert_eq!(step_diag(&ds, p(4, 0), false), Some(0));
        assert_eq!(
            step_diag(&ds, p(0, 0), false),
            Some(2),
            "先頭から巻き戻らない"
        );
        assert_eq!(step_diag(&[], p(0, 0), true), None);
    }

    // ── 波線の頂点 (テーブルテスト) ─────────────────────────────────

    #[test]
    fn 波線の頂点は領域内に収まる() {
        // (x0, x1, y, amp, wave, ppp)
        const CASES: &[(f32, f32, f32, f32, f32, f32)] = &[
            (0.0, 10.0, 20.0, 1.5, 4.0, 1.0),
            (0.0, 10.0, 20.0, 1.5, 4.0, 2.0),
            (12.25, 13.0, 7.5, 1.5, 4.0, 2.0),   // 1 文字ぶん
            (0.0, 1200.0, 40.0, 1.5, 4.0, 1.25), // 広い行
            (100.0, 100.5, 3.0, 1.5, 4.0, 1.0),  // 半 px
            (30.0, 10.0, 5.0, 1.5, 4.0, 1.0),    // 左右が逆
            (0.0, 4000.0, 9.0, 2.0, 0.5, 1.0),   // 頂点上限に当たる
        ];
        for &(x0, x1, y, amp, wave, ppp) in CASES {
            let pts = squiggle_points(x0, x1, y, amp, wave, ppp);
            let (lo, hi) = (snap_len(x0.min(x1), ppp), snap_len(x0.max(x1), ppp));
            let sy = snap_len(y, ppp);
            assert!(pts.len() >= 2, "{x0}..{x1}: 頂点が足りない");
            assert!(
                pts.len() <= SQUIGGLE_MAX_POINTS + 1,
                "{x0}..{x1}: 頂点が多すぎる ({})",
                pts.len()
            );
            for p in &pts {
                assert!(
                    p.x >= lo - f32::EPSILON && p.x <= hi + f32::EPSILON,
                    "{x0}..{x1}: x={} が領域外",
                    p.x
                );
                assert!(
                    (p.y - sy).abs() <= amp + f32::EPSILON,
                    "{x0}..{x1}: y={} が振幅を超えた",
                    p.y
                );
            }
            assert_eq!(pts.first().map(|p| p.x), Some(lo), "左端から始まっていない");
            assert_eq!(pts.last().map(|p| p.x), Some(hi), "右端で閉じていない");
        }
    }

    #[test]
    fn 波線は整数ピクセルにスナップする() {
        for ppp in [1.0_f32, 1.25, 1.5, 2.0, 3.0] {
            let pts = squiggle_points(0.3, 37.7, 11.4, 1.5, 4.0, ppp);
            for p in &pts {
                let px = p.x * ppp;
                assert!(
                    (px - px.round()).abs() < 1e-3,
                    "ppp={ppp}: x={} が px 境界に乗っていない",
                    p.x
                );
            }
        }
    }

    #[test]
    fn 幅ゼロや壊れた値では頂点を作らない() {
        assert!(squiggle_points(5.0, 5.0, 1.0, 1.5, 4.0, 1.0).is_empty());
        assert!(squiggle_points(f32::NAN, 5.0, 1.0, 1.5, 4.0, 1.0).is_empty());
        assert!(squiggle_points(0.0, f32::INFINITY, 1.0, 1.5, 4.0, 1.0).is_empty());
        assert!(squiggle_points(0.0, 5.0, f32::NAN, 1.5, 4.0, 1.0).is_empty());
    }

    #[test]
    fn 振幅や波長が使えないときは下線へ縮退する() {
        for (amp, wave) in [(0.0_f32, 4.0_f32), (1.5, 0.0), (f32::NAN, 4.0)] {
            let pts = squiggle_points(0.0, 10.0, 3.0, amp, wave, 1.0);
            assert_eq!(pts.len(), 2, "amp={amp} wave={wave}: 下線になっていない");
            assert_eq!(pts[0].y, pts[1].y);
        }
    }

    // ── 対応括弧 ────────────────────────────────────────────────────

    #[test]
    fn 対応括弧はカーソルの直後を優先し直前も見る() {
        let t = "fn f(a) {}";
        // 直後が '('
        let h = bracket_hl(t, 4).expect("括弧の上");
        assert_eq!(h.at, 4);
        assert_eq!(h.other, Some(6), "')' を指していない");
        // 直前が ')'
        let h = bracket_hl(t, 7).expect("括弧の直後");
        assert_eq!(h.at, 6);
        assert_eq!(h.other, Some(4));
        // どちらも括弧でない
        assert!(bracket_hl(t, 2).is_none());
    }

    #[test]
    fn 対応が無い括弧は相手を持たない() {
        let t = "fn f(a {\n";
        let h = bracket_hl(t, 4).expect("'(' の上");
        assert_eq!(h.at, 4);
        assert_eq!(h.other, None, "閉じが無いのに相手を返した");
        let t2 = ")";
        let h = bracket_hl(t2, 0).expect("')' の上");
        assert_eq!((h.at, h.other), (0, None));
        assert!(bracket_hl("", 0).is_none(), "空文字列で括弧を見つけた");
        assert!(bracket_hl("ab", 99).is_none(), "範囲外で拾っている");
    }

    #[test]
    fn 対応括弧は日本語の行でもずれない() {
        let t = "let 世界 = fn(引数);";
        let open = t.chars().position(|c| c == '(').expect("'(' がある");
        let close = t.chars().position(|c| c == ')').expect("')' がある");
        let h = bracket_hl(t, open).expect("'(' の上");
        assert_eq!((h.at, h.other), (open, Some(close)));
        let h = bracket_hl(t, close + 1).expect("')' の直後");
        assert_eq!((h.at, h.other), (close, Some(open)));
    }

    /// 文字列リテラル / コメント内の括弧は**素通し**する (既存 `matching_bracket`
    /// の明記された挙動)。ここで固定しておかないと「直したつもりで直っていない」
    /// 変更が黙って入る。
    #[test]
    fn 文字列やコメント内の括弧も数える既存挙動を固定する() {
        // 文字列の中の '(' が相手として選ばれる
        let t = r#"f(")");"#;
        // 0:f 1:( 2:" 3:) 4:" 5:) 6:;
        let h = bracket_hl(t, 1).expect("'(' の上");
        assert_eq!(
            h.other,
            Some(3),
            "文字列内の ')' を飛ばしている (挙動が変わった)"
        );
        // 行コメントの中も同じ
        let t = "f( // )\n);";
        let h = bracket_hl(t, 1).expect("'(' の上");
        assert_eq!(h.other, Some(6), "コメント内の ')' を飛ばしている");
    }

    /// [`BRACKET_CHARS`] が [`editor_ops::matching_bracket`] の対象と一致する。
    /// (こちらだけに文字を足すと「光るのに飛べない」が起きる)
    #[test]
    fn 対応括弧の文字表はマッチャと一致する() {
        for c in BRACKET_CHARS {
            let t = format!("{c}{c}");
            assert!(
                bracket_hl(&t, 0).is_some(),
                "{c} を隣接判定が拾えない (表がずれている)"
            );
        }
        // 表に無い文字ではマッチャも何も返さない
        for c in ['<', '>', '"', '\'', '`'] {
            let t = format!("a{c}b{c}c");
            assert!(
                bracket_hl(&t, 1).is_none(),
                "{c} を括弧として拾っている (matching_bracket は対象外)"
            );
            assert_eq!(editor_ops::matching_bracket(&t, 1), None);
        }
    }

    // ── キャッシュ ──────────────────────────────────────────────────

    #[test]
    fn 診断キャッシュは鍵が同じなら組み直さない() {
        let text = "let a = 1;\nlet b = 2;\n";
        let h = crate::editor::hash_str(text);
        let ds = Arc::new(vec![diag(0, 4, 0, 5, 1), diag(1, 4, 1, 5, 2)]);
        let mut c = DiagCache::default();
        assert!(c.refresh(ds.clone(), text, h), "初回は必ず組む");
        assert_eq!((c.errors, c.warnings), (1, 1));
        assert_eq!(c.by_line.get(&0), Some(&1));
        assert_eq!(c.spans.len(), 2);
        assert!(!c.refresh(ds.clone(), text, h), "同じ鍵で組み直している");
        // 本文だけ変わってもスパンは組み直す (打鍵で位置がずれるため)
        let text2 = "let aa = 1;\nlet b = 2;\n";
        assert!(c.refresh(ds, text2, crate::editor::hash_str(text2)));
    }

    #[test]
    fn 列だけ変わった診断も取りこぼさない() {
        let text = "let a = 1;\n";
        let h = crate::editor::hash_str(text);
        let mut c = DiagCache::default();
        assert!(c.refresh(Arc::new(vec![diag(0, 4, 0, 5, 1)]), text, h));
        let first = c.spans[0];
        // 行も severity も同じで **列だけ** 違う
        assert!(
            c.refresh(Arc::new(vec![diag(0, 8, 0, 9, 1)]), text, h),
            "列の変化を鍵が拾えていない"
        );
        assert_ne!(c.spans[0], first, "古いスパンが残っている");
    }

    #[test]
    fn 診断が空なら件数もスパンも空になる() {
        let text = "x\n";
        let h = crate::editor::hash_str(text);
        let mut c = DiagCache::default();
        c.refresh(Arc::new(vec![diag(0, 0, 0, 1, 1)]), text, h);
        assert_eq!(c.errors, 1);
        c.refresh(Arc::new(Vec::new()), text, h);
        assert_eq!((c.errors, c.warnings), (0, 0));
        assert!(c.spans.is_empty() && c.by_line.is_empty());
        assert!(c.get(0).is_none());
    }

    #[test]
    fn severity_色はテーマから取る() {
        let th = crate::theme::by_name("zaivern-dark");
        assert_eq!(severity_color(&th, 1), th.err);
        assert_eq!(severity_color(&th, 2), th.warn);
        assert_eq!(severity_color(&th, 3), th.accent);
        assert_eq!(severity_color(&th, 4), th.text_dim);
        assert_eq!(severity_color(&th, 9), th.text_dim, "未知の severity");
    }
}
