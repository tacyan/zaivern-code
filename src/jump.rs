//! ジャンプモード — **2 打鍵で画面上の任意の単語へキャレットを飛ばす**。
//!
//! AceJump (IntelliJ) / flash.nvim / leap.nvim / vim-easymotion / Helix `gw`
//! と同じ操作。矢印キーの長押しとマウスに手を伸ばす税をゼロにする。
//!
//! ## 層の分け方 (ここが設計の全て)
//!
//! 1. **純粋層** — [`plan`] / [`assign_labels`] / [`press`]。
//!    `egui::Context` も `Ui` も**一切使わない**。入力は「見えている行 +
//!    キャレット」、出力は「(行, 桁, ラベル)」。ここをテーブルテストで固定する。
//! 2. **レイアウト層** — [`layout_labels`]。矩形の計算だけで、これも egui 非依存
//!    ([`Rect2`] は自前の 4 つの `f32`)。「全ての矩形が可用領域に収まり、
//!    互いに重ならない」を極端なサイズでテストする。
//! 3. **egui 層** — [`draw`] と [`exchange`]。状態は `enum` [`Phase`] 1 つ。
//!
//! ## ラベル割り当ての方式 (なぜこの形か)
//!
//! 先行実装は 3 通りある:
//!
//! * **Helix `gw` / Zed** — 検索フェーズ無し・**常にちょうど 2 文字**のラベル。
//!   「ラベルと次の文字の衝突」が定義上起きない代わりに、対象が 3 個でも
//!   **必ず 2 打鍵**要る。
//! * **AceJump / flash.nvim** — 先に検索してからラベル付け。1 文字ラベルが
//!   使えるが、衝突回避の規則が要る。
//! * **EasyMotion** — グルーピング木。
//!
//! ここでは **可変長 (1 文字 or 2 文字) の接頭辞フリーなラベル**を採る。
//! 理由は「近い候補ほど短いラベル」が本機能の価値そのものだから — 画面に
//! 単語が 20 個しか無いときに 2 打鍵を強いるのは、Helix 方式の弱点として
//! 実際に Zed へ要望が出ている点でもある。可変長にすると普通なら
//! 「`a` と `ab` を同時に出したら `a` を押した瞬間に確定して `ab` へ永久に
//! 到達できない」という**接頭辞衝突**を踏むが、[`assign_labels`] は
//!
//! * 1 文字ラベルに使う文字 = `alphabet[..singles]`
//! * 2 文字ラベルの 1 文字目 = `alphabet[singles..]`
//!
//! と**在庫を重ならない 2 つに割る**ので、接頭辞衝突は構造的に起こらない
//! (総当たりで `どのラベルも他のラベルの接頭辞になっていない` が番人)。
//!
//! ## 「近い候補ほど短い」の作り方
//!
//! 候補はキャレットから**前後交互**に集める (Helix の `jump_to_word` と同じ:
//! 添字 0,2,4… が前方、1,3,5… が後方)。そのうえで先頭から在庫を配るので、
//! キャレットの周りが自動的に 1 打鍵で届く。
//!
//! ## 描き方
//!
//! ラベルは対象語の**先頭 1〜2 セルを置き換える**オーバーレイとして描く。
//! 下地のテキストは 1 ピクセルも動かない (UI 原則「画面が突然変わらない」)。
//! 桁は [`crate::textenc::advance_col`] (wcwidth + タブ展開) で数えるので、
//! 全角・絵文字・タブが混ざってもラベルが横にずれない。
//!
//! ## アイドル時のコスト
//!
//! [`Phase::Idle`] のフレームは [`draw`] が**即 return** する。再描画も
//! 要求しない (設計原則 3)。

use std::sync::{Mutex, OnceLock};

use crate::i18n::{tr, trf};

// ─────────────────────────────────────────────────────────────────────────
// 1. 純粋層 — egui に一切触らない
// ─────────────────────────────────────────────────────────────────────────

/// ラベル在庫の既定。**ホームポジション優先**の並び (flash.nvim と同じ方針)。
///
/// Helix の既定は素の `a..z` だが、これは「最も遠い q や z が近い候補に
/// 当たる」ので利用者が全員上書きしている。既定でホームローから配る。
pub const DEFAULT_ALPHABET: &str = "asdfghjklqwertyuiopzxcvbnm";

/// 設定 (`jump.alphabet`) の値を、実際に使える在庫へ正規化する。
///
/// * 大文字は小文字へ畳む (打鍵は大文字小文字を区別しない)
/// * ASCII の英数字だけを通す (記号は OS/レイアウトで打てないことがある)
/// * 重複は先勝ちで落とす (同じ文字が 2 か所に居ると先勝ちで死ぬラベルができる)
/// * 2 文字未満になったら既定へ戻す (1 文字だと 2 文字ラベルが作れない)
pub fn alphabet_from(spec: &str) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    for c in spec.chars() {
        let c = c.to_ascii_lowercase();
        if !c.is_ascii_alphanumeric() || out.contains(&c) {
            continue;
        }
        out.push(c);
    }
    if out.len() < 2 {
        return DEFAULT_ALPHABET.chars().collect();
    }
    out
}

/// 画面に見えている 1 行。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    /// バッファ内の論理行番号 (0 始まり)。
    pub line: usize,
    /// 行のテキスト (改行は含まない)。
    pub text: String,
}

/// バッファ内の位置。`ch` は**文字インデックス**であって表示桁ではない。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub ch: usize,
}

/// ラベルの付いた候補 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// [`plan`] に渡した `rows` の中での添字 (画面 y の算出に使う)。
    pub row: usize,
    /// バッファ内の論理行番号。
    pub line: usize,
    /// 行頭からの文字インデックス。
    pub ch: usize,
    /// 行頭からの**表示桁** (wcwidth + タブ展開済み)。
    pub col: usize,
    /// 打つべきラベル。1 文字 or 2 文字。必ず小文字。
    pub label: String,
}

impl Target {
    /// ジャンプ先。
    pub fn pos(&self) -> Pos {
        Pos {
            line: self.line,
            ch: self.ch,
        }
    }
}

/// 1 回のジャンプぶんの割り当て。
///
/// **一度作ったら選択が終わるまで作り直さない** (AceJump の安定性不変条件:
/// 「一度割り当てた可視タグは、選択が終わるまで決して変化してはならない」)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub targets: Vec<Target>,
    /// 在庫 (`alphabet.len()^2`) を超えてラベルを付けられなかった件数。
    ///
    /// **無音で打ち切らない**ための数字で、[`draw`] がそのまま画面に出す。
    pub dropped: usize,
}

/// 在庫 `n` 文字で作れるラベルの最大数。1 文字ラベルと 2 文字ラベルを
/// **接頭辞が衝突しないように**割ったときの上限で、`n * n` になる
/// (`p` 文字を 2 文字ラベルの接頭辞に回すと `(n - p) + p * n` 個。
///  `p = n` で最大の `n * n`)。
pub fn label_capacity(n: usize) -> usize {
    n.saturating_mul(n)
}

/// `k` 個のラベルを作るのに、在庫の何文字を 2 文字ラベルの接頭辞へ回すか。
///
/// `(n - p) + p * n >= k` を満たす最小の `p`。
fn prefix_count(k: usize, n: usize) -> usize {
    if n < 2 || k <= n {
        return 0;
    }
    // (k - n) / (n - 1) の切り上げ。`n >= 2` なので除数は 1 以上。
    (k - n).div_ceil(n - 1).min(n)
}

/// `k` 個ぶんのラベルを**決定的に**作る。
///
/// * 先頭ほど短く打ちやすい (最初の `n - p` 個は 1 文字)
/// * どのラベルも他のラベルの接頭辞にならない
/// * `HashMap` を使わないので、同じ入力なら必ず同じ結果
///
/// `k` が [`label_capacity`] を超えた分は**作らない** (返る本数が減る)。
/// 打ち切った件数は [`plan`] が [`Plan::dropped`] に載せて画面へ出す。
pub fn assign_labels(k: usize, alphabet: &[char]) -> Vec<String> {
    let n = alphabet.len();
    if n < 2 {
        return Vec::new();
    }
    let k = k.min(label_capacity(n));
    let p = prefix_count(k, n);
    let singles = n - p;
    let mut out = Vec::with_capacity(k);
    for i in 0..k {
        if i < singles {
            out.push(alphabet[i].to_string());
        } else {
            let j = i - singles;
            let mut s = String::with_capacity(2);
            s.push(alphabet[singles + j / n]);
            s.push(alphabet[j % n]);
            out.push(s);
        }
    }
    out
}

/// 単語を構成する文字か。
///
/// CJK は [`char::is_alphanumeric`] が真になるので「日本語です」は 1 語に
/// なる — 桁の数え方 (wcwidth) と揃っていて、ラベルは語頭の全角 1 文字を
/// ちょうど覆う。
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// 可視行から**語頭**を文書順に拾う。
///
/// Helix と同じく「2 文字以上の、単語構成文字だけからなる語の先頭」だけを
/// 対象にする。1 文字の語を入れると `=<` のような演算子の連なりや `a` 1 つに
/// までラベルが付いて、在庫が一瞬で尽きる。
fn word_starts(rows: &[Row], tab_width: usize) -> Vec<Target> {
    let mut out = Vec::new();
    for (row, r) in rows.iter().enumerate() {
        let mut col = 0usize;
        let mut prev_word = false;
        // 「語頭かどうか」を決めた時点では語の長さが分からないので、
        // 語頭の候補を控えておいて、語が終わった時点で長さを見て採否を決める。
        let mut pending: Option<(usize, usize)> = None; // (ch, col)
        let mut run = 0usize;
        for (ch, c) in r.text.chars().enumerate() {
            let w = is_word(c);
            if w && !prev_word {
                pending = Some((ch, col));
                run = 1;
            } else if w {
                run += 1;
            } else if let Some((sch, scol)) = pending.take() {
                if run >= 2 {
                    out.push(Target {
                        row,
                        line: r.line,
                        ch: sch,
                        col: scol,
                        label: String::new(),
                    });
                }
            }
            col = crate::textenc::advance_col(col, c, tab_width);
            prev_word = w;
        }
        if let Some((sch, scol)) = pending {
            if run >= 2 {
                out.push(Target {
                    row,
                    line: r.line,
                    ch: sch,
                    col: scol,
                    label: String::new(),
                });
            }
        }
    }
    out
}

/// 文書順の候補を、キャレットから**前後交互**に並べ替える。
///
/// 添字 0,2,4… が前方 (キャレット以降)、1,3,5… が後方。これで
/// 「近い候補ほど在庫の先頭 = 短くて打ちやすいラベル」になる。
fn order_from_caret(all: Vec<Target>, caret: Pos) -> Vec<Target> {
    let split = all
        .iter()
        .position(|t| (t.line, t.ch) >= (caret.line, caret.ch))
        .unwrap_or(all.len());
    let (back, fwd) = all.split_at(split);
    let mut fwd = fwd.iter();
    let mut back = back.iter().rev();
    let mut out = Vec::with_capacity(all.len());
    loop {
        let a = fwd.next();
        let b = back.next();
        if a.is_none() && b.is_none() {
            break;
        }
        out.extend(a.cloned());
        out.extend(b.cloned());
    }
    out
}

/// 可視行 + キャレットから、ラベル付きの候補一覧を作る (**この機能の中核**)。
///
/// * `rows` — 画面に見えている行だけ (範囲は呼び出し側が決める)
/// * `caret` — 近さの基準
/// * `tab_width` — 桁の数え方 (`config.tab_width` を渡すこと)
/// * `alphabet` — [`alphabet_from`] を通した在庫
pub fn plan(rows: &[Row], caret: Pos, tab_width: usize, alphabet: &[char]) -> Plan {
    let ordered = order_from_caret(word_starts(rows, tab_width), caret);
    let total = ordered.len();
    let labels = assign_labels(total, alphabet);
    let dropped = total - labels.len();
    let targets = ordered
        .into_iter()
        .zip(labels)
        .map(|(mut t, l)| {
            t.label = l;
            t
        })
        .collect();
    Plan { targets, dropped }
}

/// 1 打鍵の結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// ラベルが確定した。ここへ飛ぶ。
    Jump(Pos),
    /// 1 打鍵目が確定して候補が絞れた。2 打鍵目を待つ。
    Narrow(char),
    /// どのラベルにも当たらなかった → 中断 (ラベルを消してキャレットは動かさない)。
    Cancel,
}

/// 打鍵を正規化する。**大文字で打っても同じラベルに当たる** (在庫は小文字のみ)。
fn norm(key: char) -> char {
    key.to_ascii_lowercase()
}

/// 状態機械の 1 ステップ。**純粋関数**なのでテーブルテストで固定できる。
///
/// `typed` は確定済みの 1 打鍵目 (まだなら `None`)。ラベルが接頭辞フリー
/// なので「1 文字ラベルの確定」と「2 文字ラベルの絞り込み」は排他になり、
/// 曖昧さは残らない。
pub fn press(plan: &Plan, typed: Option<char>, key: char) -> Press {
    let key = norm(key);
    match typed {
        None => {
            if let Some(t) = plan
                .targets
                .iter()
                .find(|t| t.label.chars().count() == 1 && t.label.starts_with(key))
            {
                return Press::Jump(t.pos());
            }
            if plan.targets.iter().any(|t| t.label.starts_with(key)) {
                return Press::Narrow(key);
            }
            Press::Cancel
        }
        Some(first) => {
            let first = norm(first);
            let mut want = String::with_capacity(2);
            want.push(first);
            want.push(key);
            match plan.targets.iter().find(|t| t.label == want) {
                Some(t) => Press::Jump(t.pos()),
                None => Press::Cancel,
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 2. レイアウト層 — これも egui 非依存 (自前の矩形)
// ─────────────────────────────────────────────────────────────────────────

/// 矩形。egui へ持ち込む前の**純粋な**表現。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect2 {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect2 {
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    /// 面積を持って重なっているか (辺が接するだけは重なりに数えない)。
    pub fn overlaps(&self, o: &Rect2) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
    /// `o` を完全に含むか。
    pub fn contains_rect(&self, o: &Rect2) -> bool {
        o.x >= self.x && o.y >= self.y && o.right() <= self.right() && o.bottom() <= self.bottom()
    }
}

/// 等幅グリッドの寸法。`origin` は `rows[0]` の左上。
#[derive(Clone, Copy, Debug)]
pub struct Geom {
    pub origin_x: f32,
    pub origin_y: f32,
    /// 1 桁の幅。
    pub cell_w: f32,
    /// 1 行の高さ。
    pub cell_h: f32,
    /// 物理ピクセル密度 ([`crate::theme::snap_len`] に渡す)。
    pub ppp: f32,
}

/// ラベルを置く矩形を決める。返り値は `(targets の添字, 矩形)`。
///
/// * 座標は [`crate::theme::snap_len`] で**整数ピクセルへ揃える**
///   (小数のままだと桁間隔が 8/8/7/8 px と揺れ、100% 表示で最も悪化する)
/// * `clip` からはみ出すものは**描かない** (横スクロールで画面外に出た語)
/// * 既に置いたラベルと重なるものは**描かない** (AceJump の `occupied` 方式)。
///   近い候補から先に置くので、生き残るのは打ちやすいラベルの側になる。
pub fn layout_labels(plan: &Plan, g: &Geom, clip: Rect2) -> Vec<(usize, Rect2)> {
    let mut placed: Vec<(usize, Rect2)> = Vec::with_capacity(plan.targets.len());
    for (i, t) in plan.targets.iter().enumerate() {
        let cells = t.label.chars().count() as f32;
        let r = Rect2 {
            x: crate::theme::snap_len(g.origin_x + t.col as f32 * g.cell_w, g.ppp),
            y: crate::theme::snap_len(g.origin_y + t.row as f32 * g.cell_h, g.ppp),
            w: crate::theme::snap_len(cells * g.cell_w, g.ppp),
            h: crate::theme::snap_len(g.cell_h, g.ppp),
        };
        if !clip.contains_rect(&r) {
            continue;
        }
        if placed.iter().any(|(_, p)| p.overlaps(&r)) {
            continue;
        }
        placed.push((i, r));
    }
    placed
}

// ─────────────────────────────────────────────────────────────────────────
// 3. egui 層 — 状態機械とオーバーレイ
// ─────────────────────────────────────────────────────────────────────────

/// エディタが毎フレーム渡す「いま見えているもの」。
///
/// ## 分割エディタ
///
/// 渡すのは**フォーカスされている 1 ペインぶんだけ**。他のペインへは飛ばない。
/// 「見えている全ペインへラベルを撒く」ほうが対象は増えるが、在庫を食い潰して
/// 手元のペインの 1 打鍵ラベルが 2 打鍵へ落ちるので、割に合わない
/// (AceJump も既定は現在のエディタのみ)。ペインを跨ぐ移動は既存の
/// ペイン切り替えで行い、そのあとジャンプする。
///
/// ## ソフトラップ・横スクロール
///
/// `rows` は**画面に出ている順の視覚行**を渡すこと ([`Target::row`] がそのまま
/// 画面 y の行番号になる)。ソフトラップしているなら折り返し後の断片を 1 行と
/// して渡す ([`Row::line`] には元の論理行番号を入れる — ジャンプ先はそちらで
/// 返る)。横スクロールは [`View::origin`] を左へずらして表現する。画面外へ出た
/// ラベルは [`View::clip`] で落ちる。
#[derive(Clone, Debug)]
pub struct View {
    /// 可視行だけ。
    pub rows: Vec<Row>,
    pub caret: Pos,
    /// 桁の数え方 (`config.tab_width`)。
    pub tab_width: usize,
    /// `rows[0]` の左上 (`TextEditOutput::galley_pos` 相当)。
    pub origin: egui::Pos2,
    /// 1 桁 × 1 行の大きさ (等幅グリッド)。
    pub cell: egui::Vec2,
    /// ラベルを描いてよい範囲。ここからはみ出すラベルは描かない。
    pub clip: egui::Rect,
}

/// ジャンプモードの状態。**独立した `bool` を複数持たない** (UI 原則:
/// 2 つの状態が同時に成立する事故を構造的に起こさない)。
#[derive(Default)]
enum Phase {
    /// 何も起きていない。このフレームは 1 ピクセルも触らない。
    #[default]
    Idle,
    /// 起動したが、まだ可視行を受け取っていない。
    Arming {
        prev_focus: Option<egui::Id>,
        since: u64,
    },
    /// ラベル表示中。
    Active {
        plan: Plan,
        /// 確定済みの 1 打鍵目。
        typed: Option<char>,
        prev_focus: Option<egui::Id>,
        since: u64,
        geom: Geom,
        clip: Rect2,
    },
}

/// モジュール側の状態。**`ZaivernApp` のフィールドを増やさない**ので、
/// `app.rs` を 1 バイトも触らずに機能が繋がる (`lease.rs` と同じ形)。
#[derive(Default)]
struct State {
    phase: Phase,
    /// エディタが直近に渡してきた可視行。
    view: Option<View>,
    /// `view` を受け取ったパス番号 (古い View を使わないための検査)。
    view_pass: u64,
    /// 確定したジャンプ先。[`exchange`] が 1 回だけ取り出す。
    pending: Option<Pos>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// ラベル在庫の上書きが入る場所 (`jump.alphabet` の実値)。
///
/// 設定は [`crate::feature::Setting`] として宣言してあり、値は `key` 文字列で
/// 届く。設定配線が値を持っていない間はここが空で、[`DEFAULT_ALPHABET`] が
/// 使われる。**`config.rs` へフィールドを足さない**のがこの経路の目的。
fn alphabet_id() -> egui::Id {
    egui::Id::new("zv-jump-alphabet")
}

/// いま使う在庫。
fn alphabet(ctx: &egui::Context) -> Vec<char> {
    let spec: Option<String> = ctx.data_mut(|d| d.get_persisted(alphabet_id()));
    alphabet_from(spec.as_deref().unwrap_or(DEFAULT_ALPHABET))
}

/// エディタが [`View`] を作るべきか。
///
/// **待機中のフレームでは `false`** を返すので、ジャンプを使っていない間は
/// 可視行の収集すら走らない (設計原則 3: アイドル時のコストはゼロ)。
pub fn wants_view() -> bool {
    state()
        .lock()
        .map(|st| !matches!(st.phase, Phase::Idle))
        .unwrap_or(false)
}

/// **エディタ側から呼ぶ唯一のグルー。** 1 フレームに 1 回。
///
/// 「いま見えているもの」を渡し、確定したジャンプ先を受け取る。
/// 返り値が `Some` のフレームだけキャレットを動かせばよい。
///
/// 中断 (Escape) のときは **`None` が返るだけ**でキャレットへは一度も触らない
/// ので、「直前のキャレットと選択を復元する」処理そのものが要らない
/// (保存して戻すより、そもそも動かさないほうが壊れない)。
///
/// # まだ呼ばれていない (統合担当への申し送り)
///
/// `ZaivernApp` のフィールドは公開されていないので、可視行・キャレット・
/// 等幅グリッドの寸法は**機能側からは取れない**。この 1 関数だけが
/// `app.rs` のエディタ描画から呼ばれるのを待っている:
///
/// ```ignore
/// // src/app.rs の TextEdit 描画直後 (`output.galley_pos` / `char_w` / `row_height`
/// // が手元にある場所) で 1 行:
/// if let Some(p) = crate::jump::exchange(ctx, view) { /* キャレットを (p.line, p.ch) へ */ }
/// ```
///
/// **繋いだら下の `allow(dead_code)` を必ず消すこと。** 消し忘れると、
/// 次に誰かが本当に使われなくなったときに検出できなくなる。ここに
/// 繋がっているので `allow(dead_code)` は外してある。
pub fn exchange(ctx: &egui::Context, view: View) -> Option<Pos> {
    let Ok(mut st) = state().lock() else {
        return None;
    };
    st.view = Some(view);
    st.view_pass = ctx.cumulative_pass_nr();
    st.pending.take()
}

/// ジャンプモードを開始する (パレット / キーバインドの入口)。
fn start(ctx: &egui::Context) {
    let Ok(mut st) = state().lock() else { return };
    st.pending = None;
    st.phase = Phase::Arming {
        prev_focus: ctx.memory(|m| m.focused()),
        since: ctx.cumulative_pass_nr(),
    };
}

/// 中断・確定のどちらでも通る後始末。フォーカスを元へ戻す。
fn finish(ctx: &egui::Context, st: &mut State) {
    let prev = match &st.phase {
        Phase::Idle => None,
        Phase::Arming { prev_focus, .. } => *prev_focus,
        Phase::Active { prev_focus, .. } => *prev_focus,
    };
    st.phase = Phase::Idle;
    if let Some(id) = prev {
        ctx.memory_mut(|m| m.request_focus(id));
    }
}

/// このフレームに届いた打鍵。
enum Stroke {
    /// Escape — 中断。
    Escape,
    /// ラベル候補の 1 文字。
    Char(char),
    /// 修飾キー付き — Helix と同じく中断する。
    Modified,
}

/// 打鍵を読む。**`Event::Text` を主に使う**ので、非 QWERTY 配列でも
/// 「画面に出ている文字」がそのまま当たる。
fn strokes(ctx: &egui::Context) -> Vec<Stroke> {
    ctx.input(|i| {
        let mut out = Vec::new();
        for e in &i.events {
            match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if *key == egui::Key::Escape {
                        out.push(Stroke::Escape);
                    } else if modifiers.command || modifiers.ctrl || modifiers.alt {
                        // ⇧ は除く — 在庫は小文字だけなので ⇧A も `a` に当てたい。
                        out.push(Stroke::Modified);
                    }
                }
                egui::Event::Text(t) => out.extend(t.chars().map(Stroke::Char)),
                _ => {}
            }
        }
        out
    })
}

/// 告知カードを閉じてよいか。
///
/// **クリックでも閉じる**のが要点 — 打鍵でしか消えないカードを画面の中央に
/// 置くと、マウスに手を伸ばした利用者が閉じ方を失う。`fresh` (起動した当の
/// フレーム) では常に `false` を返し、起動打鍵そのものを飲まない。
fn dismissed(ctx: &egui::Context, fresh: bool) -> bool {
    !fresh && (!strokes(ctx).is_empty() || ctx.input(|i| i.pointer.any_click()))
}

/// 毎フレームのオーバーレイ。
///
/// **[`Phase::Idle`] のフレームはここで即 return する** — 描画も再描画要求も
/// しない (設計原則 3: アイドル時のコストはゼロ)。
pub fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {
    // 状態はモジュール側に持つので `app` の中身へは触らない。
    let _ = app;
    let Ok(mut st) = state().lock() else { return };
    if matches!(st.phase, Phase::Idle) {
        return;
    }

    let pass = ctx.cumulative_pass_nr();
    // 起動した当のフレームの打鍵 (⌘⇧Y やパレットの Enter) を食べない。
    let fresh_start = match &st.phase {
        Phase::Arming { since, .. } | Phase::Active { since, .. } => *since == pass,
        Phase::Idle => false,
    };

    // Arming → Active: このフレームにエディタが View を渡していれば割り当てる。
    if let Phase::Arming { prev_focus, since } = st.phase {
        let alpha = alphabet(ctx);
        let ready = st
            .view
            .as_ref()
            .filter(|_| st.view_pass + 1 >= pass)
            .map(|v| {
                (
                    plan(&v.rows, v.caret, v.tab_width, &alpha),
                    Geom {
                        origin_x: v.origin.x,
                        origin_y: v.origin.y,
                        cell_w: v.cell.x,
                        cell_h: v.cell.y,
                        ppp: ctx.pixels_per_point(),
                    },
                    Rect2 {
                        x: v.clip.min.x,
                        y: v.clip.min.y,
                        w: v.clip.width(),
                        h: v.clip.height(),
                    },
                )
            });
        if let Some((p, geom, clip)) = ready {
            if p.targets.is_empty() {
                notice_card(
                    ctx,
                    &tr("ジャンプできる語が画面にありません"),
                    egui::Align2::CENTER_CENTER,
                );
                if dismissed(ctx, fresh_start) {
                    finish(ctx, &mut st);
                }
                return;
            }
            st.phase = Phase::Active {
                plan: p,
                typed: None,
                prev_focus,
                since,
                geom,
                clip,
            };
        } else {
            // エディタが出ていない (Cockpit 等)。空状態は中央に 1 枚だけ出す。
            notice_card(
                ctx,
                &tr("ジャンプできる編集画面がありません"),
                egui::Align2::CENTER_CENTER,
            );
            if dismissed(ctx, fresh_start) {
                finish(ctx, &mut st);
            }
            return;
        }
    }

    let Phase::Active {
        plan,
        typed,
        since: _,
        geom,
        clip,
        ..
    } = &mut st.phase
    else {
        return;
    };

    // ラベル入力中はエディタの `TextEdit` に文字を入れさせない。
    // (フォーカスを外すだけ。終わったら `finish` が元のウィジェットへ戻す)
    ctx.memory_mut(|m| m.stop_text_input());

    // 打鍵を処理する。ジャンプ先が決まったフレームはラベルを描かずに畳む。
    let mut jumped: Option<Pos> = None;
    let mut cancel = false;
    if !fresh_start {
        for s in strokes(ctx) {
            match s {
                Stroke::Escape | Stroke::Modified => {
                    cancel = true;
                    break;
                }
                Stroke::Char(c) => match press(plan, *typed, c) {
                    Press::Jump(p) => {
                        jumped = Some(p);
                        break;
                    }
                    Press::Narrow(c) => *typed = Some(c),
                    Press::Cancel => {
                        cancel = true;
                        break;
                    }
                },
            }
        }
    }
    if cancel {
        finish(ctx, &mut st);
        return;
    }
    if let Some(p) = jumped {
        st.pending = Some(p);
        finish(ctx, &mut st);
        // 次のフレームで `exchange` が拾えるように 1 回だけ描き直す。
        ctx.request_repaint();
        return;
    }

    let (plan, typed, geom, clip) = (&*plan, *typed, *geom, *clip);
    paint(ctx, plan, typed, &geom, clip);
    if plan.dropped > 0 {
        // 打ち切りは**空状態ではない**ので中央には置かない (ラベルを覆うため)。
        notice_card(
            ctx,
            &trf(
                "候補が多すぎるため {n} 件にはラベルを付けていません",
                &[("n", plan.dropped.to_string())],
            ),
            egui::Align2::CENTER_BOTTOM,
        );
    }
}

/// ラベルを描く。**下地のセルを置き換える**だけなので本文は 1 px も動かない。
fn paint(ctx: &egui::Context, plan: &Plan, typed: Option<char>, geom: &Geom, clip: Rect2) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("zv-jump-labels"),
    ));
    let visuals = ctx.style().visuals.clone();
    // 色はテーマのパレット (egui の visuals はテーマから設定されている) から
    // 起こす。**直書きしない**。
    let bg = visuals.selection.bg_fill;
    let fg = readable_on(bg, visuals.strong_text_color(), visuals.extreme_bg_color);
    let dim = visuals.weak_text_color();
    let font = egui::TextStyle::Monospace.resolve(&ctx.style());
    let ppp = ctx.pixels_per_point();
    let font = egui::FontId::new(crate::theme::snap_font_size(font.size, ppp), font.family);

    for (i, r) in layout_labels(plan, geom, clip) {
        let label = &plan.targets[i].label;
        if let Some(t) = typed {
            // 1 打鍵目に当たらないラベルは消す (候補が絞れたことを見せる)。
            if !label.starts_with(t) {
                continue;
            }
        }
        let rect = egui::Rect::from_min_size(egui::pos2(r.x, r.y), egui::vec2(r.w, r.h));
        painter.rect_filled(rect, egui::Rounding::same(2.0), bg);
        for (ci, c) in label.chars().enumerate() {
            let done = typed.is_some() && ci == 0;
            painter.text(
                egui::pos2(
                    crate::theme::snap_len(r.x + ci as f32 * geom.cell_w, ppp),
                    rect.center().y,
                ),
                egui::Align2::LEFT_CENTER,
                c,
                font.clone(),
                if done { dim } else { fg },
            );
        }
    }
}

/// 背景 `bg` の上で読める側の色を選ぶ (コントラスト比が高いほう)。
fn readable_on(bg: egui::Color32, a: egui::Color32, b: egui::Color32) -> egui::Color32 {
    if crate::theme::contrast_ratio(bg, a) >= crate::theme::contrast_ratio(bg, b) {
        a
    } else {
        b
    }
}

/// 空状態 / 打ち切りの告知。**1 枚だけ**出す (UI 原則)。
///
/// 空状態は可用領域の中央 (`CENTER_CENTER`)、打ち切りの注記は下端
/// (`CENTER_BOTTOM`) — 後者を中央に置くとラベルそのものを覆ってしまう。
fn notice_card(ctx: &egui::Context, text: &str, anchor: egui::Align2) {
    egui::Area::new(egui::Id::new("zv-jump-notice"))
        .order(egui::Order::Foreground)
        .anchor(anchor, egui::vec2(0.0, -24.0 * anchor.y().to_sign()))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(text);
            });
        });
}

/// コマンドパレット / キーバインドからの到達経路。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "jump",
    entries: &[crate::feature::Entry {
        icon: "🎯",
        label: "ジャンプ — 2 打鍵で画面上の語へ飛ぶ",
        id: "jump.start",
    }],
    dispatch: |_app, ctx, id| match id {
        "jump.start" => {
            start(ctx);
            true
        }
        _ => false,
    },
    draw: Some(draw),
    settings: &[crate::feature::Setting {
        key: "jump.alphabet",
        label: "ジャンプのラベル文字",
        help: "近い候補から順に使う。ホームポジション優先の並びにすると打ちやすい。\
               ASCII 英数字だけを通し、重複は落とす。2 文字未満なら既定へ戻す。",
        default: crate::feature::SettingValue::Text(DEFAULT_ALPHABET),
    }],
    // ⌘⇧Y: `MACOS_RESERVED` に無く、既存の `BindAction` とも重ならない唯一の
    // 空き。⌘J / ⌘⇧J / ⌥⌘J は既に埋まっており、⌘⇧X は egui-winit 0.29 が
    // 押下を Cut へすり替えるので構造的に発火しない。
    binds: &[crate::feature::Bind {
        id: "jump.start",
        default: "cmd+shift+y",
    }],
};

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(s: &str) -> Vec<char> {
        alphabet_from(s)
    }

    fn row(line: usize, text: &str) -> Row {
        Row {
            line,
            text: text.to_string(),
        }
    }

    fn rows(lines: &[&str]) -> Vec<Row> {
        lines.iter().enumerate().map(|(i, s)| row(i, s)).collect()
    }

    // ── ラベル割り当て ─────────────────────────────────────────────

    #[test]
    fn 候補が0件ならラベルは1つも出ない() {
        let p = plan(
            &rows(&["", "   ", "= = ="]),
            Pos::default(),
            4,
            &alpha("abc"),
        );
        assert!(p.targets.is_empty());
        assert_eq!(p.dropped, 0);
    }

    #[test]
    fn 候補が1件なら1打鍵で確定する() {
        let a = alpha("abc");
        let p = plan(&rows(&["hello"]), Pos::default(), 4, &a);
        assert_eq!(p.targets.len(), 1);
        assert_eq!(p.targets[0].label, "a");
        assert_eq!(press(&p, None, 'a'), Press::Jump(Pos { line: 0, ch: 0 }));
    }

    #[test]
    fn 在庫ちょうどなら全て1文字ラベル() {
        let a = alpha("abc");
        let l = assign_labels(3, &a);
        assert_eq!(l, vec!["a", "b", "c"]);
    }

    #[test]
    fn 在庫を1つ超えたら2文字へ落ちる() {
        let a = alpha("abc");
        // n=3, k=4 → 接頭辞 1 文字 (c) を 2 文字ラベルへ回す。
        // 1 文字ラベルは a / b の 2 つに減り、残りが "c" 始まりの 2 文字になる。
        assert_eq!(assign_labels(4, &a), vec!["a", "b", "ca", "cb"]);
        assert_eq!(assign_labels(5, &a), vec!["a", "b", "ca", "cb", "cc"]);
        // さらに増えると 1 文字ラベルが減っていく
        assert_eq!(
            assign_labels(7, &a),
            vec!["a", "ba", "bb", "bc", "ca", "cb", "cc"]
        );
        assert_eq!(assign_labels(9, &a).len(), 9);
    }

    #[test]
    fn 在庫の二乗を超えた分は打ち切って件数を報告する() {
        let a = alpha("abc"); // 上限 9
        assert_eq!(label_capacity(a.len()), 9);
        assert_eq!(assign_labels(20, &a).len(), 9);

        // 語が 10 個ある行 → 9 個にラベル、1 個は dropped
        let line = "aa bb cc dd ee ff gg hh ii jj";
        let p = plan(&rows(&[line]), Pos::default(), 4, &a);
        assert_eq!(p.targets.len(), 9);
        assert_eq!(p.dropped, 1, "無音で打ち切らず件数を持ち帰ること");
    }

    #[test]
    fn どのラベルも他のラベルの接頭辞になっていない() {
        for n in 2..=8usize {
            let a: Vec<char> = "abcdefgh".chars().take(n).collect();
            for k in 0..=label_capacity(n) {
                let labels = assign_labels(k, &a);
                assert_eq!(labels.len(), k, "n={n} k={k}");
                for (i, x) in labels.iter().enumerate() {
                    for (j, y) in labels.iter().enumerate() {
                        if i == j {
                            continue;
                        }
                        assert!(
                            !y.starts_with(x.as_str()),
                            "n={n} k={k}: {x:?} が {y:?} の接頭辞になっている \
                             (先に確定して {y:?} へ永久に到達できない)"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn 近い候補ほど短いラベルを得る() {
        let a = alpha("abc");
        let line = "aa bb cc dd ee ff gg";
        // キャレットを行頭に置くと、先頭の語ほど短いラベル
        let p = plan(&rows(&[line]), Pos { line: 0, ch: 0 }, 4, &a);
        let by_col: std::collections::BTreeMap<usize, String> =
            p.targets.iter().map(|t| (t.col, t.label.clone())).collect();
        let first = by_col.values().next().unwrap();
        assert_eq!(first.chars().count(), 1);
        // 1 文字ラベルを持つ候補は、2 文字ラベルの候補より必ずキャレットに近い
        let mut short_max = 0usize;
        let mut long_min = usize::MAX;
        for t in &p.targets {
            let d = t.col;
            if t.label.chars().count() == 1 {
                short_max = short_max.max(d);
            } else {
                long_min = long_min.min(d);
            }
        }
        assert!(
            short_max < long_min,
            "1 文字ラベル(最遠 {short_max}) が 2 文字ラベル(最近 {long_min}) より遠い"
        );
    }

    #[test]
    fn キャレットから前後交互に候補を集める() {
        let a = alpha("abcdefgh");
        let line = "aa bb cc dd ee";
        // 3 語目 (cc, col 6) にキャレット
        let p = plan(&rows(&[line]), Pos { line: 0, ch: 6 }, 4, &a);
        let seq: Vec<usize> = p.targets.iter().map(|t| t.col).collect();
        // 前方 6,9,12 と 後方 3,0 を交互に
        assert_eq!(seq, vec![6, 3, 9, 0, 12]);
    }

    #[test]
    fn 同じ入力なら必ず同じラベル() {
        let a = alpha(DEFAULT_ALPHABET);
        let src = rows(&[
            "let mut total = count + other;",
            "  for item in list { total += item; }",
            "日本語のテキストも混ざる",
        ]);
        let first = plan(&src, Pos { line: 1, ch: 4 }, 4, &a);
        for _ in 0..50 {
            assert_eq!(plan(&src, Pos { line: 1, ch: 4 }, 4, &a), first);
        }
    }

    // ── 状態遷移 ───────────────────────────────────────────────────

    #[test]
    fn 一打鍵目で候補が絞られ二打鍵目で確定する() {
        let a = alpha("ab"); // 上限 4
        let line = "aa bb cc dd";
        let p = plan(&rows(&[line]), Pos::default(), 4, &a);
        // n=2, k=4 → 全部 2 文字 ("aa","ab","ba","bb")
        assert_eq!(
            p.targets
                .iter()
                .map(|t| t.label.as_str())
                .collect::<Vec<_>>(),
            vec!["aa", "ab", "ba", "bb"]
        );
        assert_eq!(press(&p, None, 'a'), Press::Narrow('a'));
        assert_eq!(press(&p, Some('a'), 'b'), Press::Jump(p.targets[1].pos()));
    }

    #[test]
    fn 大文字で打っても同じラベルに当たる() {
        let a = alpha("ab");
        let p = plan(&rows(&["aa bb cc dd"]), Pos::default(), 4, &a);
        assert_eq!(press(&p, None, 'A'), Press::Narrow('a'));
        assert_eq!(press(&p, Some('A'), 'B'), press(&p, Some('a'), 'b'));
    }

    #[test]
    fn 在庫に無い文字は中断する() {
        let a = alpha("ab");
        let p = plan(&rows(&["aa bb cc dd"]), Pos::default(), 4, &a);
        assert_eq!(press(&p, None, 'z'), Press::Cancel);
        assert_eq!(press(&p, Some('a'), 'z'), Press::Cancel);
        // 中断は「ジャンプ先を返さない」ことで表す。キャレットへは触れない。
        assert!(!matches!(press(&p, None, 'z'), Press::Jump(_)));
    }

    #[test]
    fn 候補が空なら何を打っても中断する() {
        let p = Plan::default();
        assert_eq!(press(&p, None, 'a'), Press::Cancel);
        assert_eq!(press(&p, Some('a'), 'a'), Press::Cancel);
    }

    // ── 桁の計算 (CJK / タブ / 絵文字) ─────────────────────────────

    #[test]
    fn 全角文字を含む行でも桁がずれない() {
        let a = alpha(DEFAULT_ALPHABET);
        //   日本語 = 3 文字 / 6 桁、その後に半角スペース、次の語 "abc"
        let p = plan(&rows(&["日本語 abc"]), Pos::default(), 4, &a);
        let cols: Vec<(usize, usize)> = p
            .targets
            .iter()
            .map(|t| (t.ch, t.col))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(cols, vec![(0, 0), (4, 7)], "全角は 1 文字 2 桁で数える");
    }

    #[test]
    fn タブを含む行の桁はタブ幅で決まる() {
        let a = alpha(DEFAULT_ALPHABET);
        let src = rows(&["\tlet x"]);
        let p4 = plan(&src, Pos::default(), 4, &a);
        let p8 = plan(&src, Pos::default(), 8, &a);
        let col4: Vec<usize> = p4.targets.iter().map(|t| t.col).collect();
        let col8: Vec<usize> = p8.targets.iter().map(|t| t.col).collect();
        assert_eq!(col4.iter().min(), Some(&4));
        assert_eq!(col8.iter().min(), Some(&8));
    }

    #[test]
    fn 絵文字と結合文字を含む行でも桁がずれない() {
        let a = alpha(DEFAULT_ALPHABET);
        // 👨‍👩‍👧 は 人(2) + ZWJ(0) + 人(2) + ZWJ(0) + 人(2) = 6 桁
        let p = plan(&rows(&["👨‍👩‍👧 ok"]), Pos::default(), 4, &a);
        let t = p.targets.iter().find(|t| t.ch >= 5).expect("ok が候補");
        assert_eq!(t.col, 7, "絵文字の後ろの語が正しい桁に来ること");
    }

    #[test]
    fn 一文字の語と記号の連なりは候補にしない() {
        let a = alpha(DEFAULT_ALPHABET);
        let p = plan(&rows(&["a =< b ==> cc"]), Pos::default(), 4, &a);
        let cols: Vec<usize> = p.targets.iter().map(|t| t.col).collect();
        assert_eq!(cols, vec![11], "2 文字以上の語だけが候補");
    }

    // ── 在庫の正規化 ───────────────────────────────────────────────

    #[test]
    fn ラベル在庫は正規化される() {
        assert_eq!(alphabet_from("AbC"), vec!['a', 'b', 'c']);
        assert_eq!(alphabet_from("aabbc"), vec!['a', 'b', 'c']);
        assert_eq!(alphabet_from("a b!c"), vec!['a', 'b', 'c']);
        // 2 文字未満は既定へ戻す (1 文字だと 2 文字ラベルが作れない)
        assert_eq!(alphabet_from(""), alphabet_from(DEFAULT_ALPHABET));
        assert_eq!(alphabet_from("z"), alphabet_from(DEFAULT_ALPHABET));
        assert_eq!(alphabet_from("!!!"), alphabet_from(DEFAULT_ALPHABET));
        // 既定はホームローから始まる
        assert_eq!(&DEFAULT_ALPHABET[..4], "asdf");
    }

    #[test]
    fn 設定の既定値は定数と同じ() {
        let s = FEATURE
            .settings
            .iter()
            .find(|s| s.key == "jump.alphabet")
            .expect("jump.alphabet を宣言していること");
        assert_eq!(
            s.default,
            crate::feature::SettingValue::Text(DEFAULT_ALPHABET)
        );
    }

    // ── レイアウト ─────────────────────────────────────────────────

    fn geom() -> Geom {
        Geom {
            origin_x: 40.0,
            origin_y: 20.0,
            cell_w: 8.0,
            cell_h: 17.0,
            ppp: 2.0,
        }
    }

    #[test]
    fn ラベルは可用領域に収まり互いに重ならない() {
        let a = alpha(DEFAULT_ALPHABET);
        let line = "let alpha = beta + gamma * delta - epsilon / zeta;";
        let src: Vec<Row> = (0..40).map(|i| row(i, line)).collect();
        let p = plan(&src, Pos { line: 20, ch: 0 }, 4, &a);
        assert!(p.targets.len() > 30, "テストとして候補が少なすぎる");

        for (w, h) in [(900.0f32, 700.0f32), (1200.0, 300.0), (320.0, 200.0)] {
            let clip = Rect2 {
                x: 0.0,
                y: 0.0,
                w,
                h,
            };
            let placed = layout_labels(&p, &geom(), clip);
            for (_, r) in &placed {
                assert!(
                    clip.contains_rect(r),
                    "{w}x{h}: ラベルが可用領域からはみ出した: {r:?}"
                );
            }
            for i in 0..placed.len() {
                for j in (i + 1)..placed.len() {
                    assert!(
                        !placed[i].1.overlaps(&placed[j].1),
                        "{w}x{h}: ラベルが重なった: {:?} と {:?}",
                        placed[i].1,
                        placed[j].1
                    );
                }
            }
        }
    }

    #[test]
    fn ラベルの座標は整数ピクセルに揃う() {
        let a = alpha(DEFAULT_ALPHABET);
        let g = Geom {
            cell_w: 8.333_333,
            cell_h: 17.5,
            ppp: 1.25,
            ..geom()
        };
        let p = plan(
            &rows(&["alpha beta gamma delta epsilon zeta eta theta"]),
            Pos::default(),
            4,
            &a,
        );
        let clip = Rect2 {
            x: 0.0,
            y: 0.0,
            w: 1200.0,
            h: 300.0,
        };
        for (_, r) in layout_labels(&p, &g, clip) {
            for v in [r.x, r.y, r.w, r.h] {
                let px = v * g.ppp;
                assert!(
                    (px - px.round()).abs() < 1e-3,
                    "{v} が物理ピクセルに乗っていない (px={px})"
                );
            }
        }
    }

    #[test]
    fn ラベルは語の先頭セルを置き換えるので本文が動かない() {
        let a = alpha("ab");
        let p = plan(&rows(&["alpha beta gamma delta"]), Pos::default(), 4, &a);
        let g = geom();
        let clip = Rect2 {
            x: 0.0,
            y: 0.0,
            w: 900.0,
            h: 700.0,
        };
        for (i, r) in layout_labels(&p, &g, clip) {
            let t = &p.targets[i];
            // 左端は語の先頭桁ちょうど、幅はラベルの文字数ぶんちょうど
            assert_eq!(
                r.x,
                crate::theme::snap_len(g.origin_x + t.col as f32 * g.cell_w, g.ppp)
            );
            assert_eq!(
                r.w,
                crate::theme::snap_len(t.label.chars().count() as f32 * g.cell_w, g.ppp)
            );
        }
    }

    #[test]
    fn 画面外の候補にはラベルを描かない() {
        let a = alpha(DEFAULT_ALPHABET);
        let src: Vec<Row> = (0..60).map(|i| row(i, "alpha beta")).collect();
        let p = plan(&src, Pos::default(), 4, &a);
        let g = geom();
        // 5 行ぶんしか見えない領域
        let clip = Rect2 {
            x: 0.0,
            y: 0.0,
            w: 900.0,
            h: g.origin_y + g.cell_h * 5.0,
        };
        let placed = layout_labels(&p, &g, clip);
        assert!(!placed.is_empty());
        for (i, _) in &placed {
            assert!(p.targets[*i].row < 5, "画面外の行にラベルを描いている");
        }
    }

    // ── 登録内容 ───────────────────────────────────────────────────

    #[test]
    fn 既定打鍵は予約表とも既存割り当てともぶつからない() {
        let spec = FEATURE.binds[0].default;
        let b = crate::keybinds::parse_binding(spec)
            .unwrap_or_else(|| panic!("{spec:?} が parse_binding で読めない"));
        let first = b.first();

        // 1) macOS の実測予約表。**どの OS で走らせても表と突き合わせる**
        //    (`macos_reservation` は非 mac で常に None を返すため、表を直に見る)。
        for (m, k, why) in crate::keybinds::MACOS_RESERVED {
            assert!(
                !crate::keybinds::same_stroke(egui::KeyboardShortcut::new(*m, *k), first),
                "{spec:?} は macOS が握っている: {why}"
            );
        }

        // 2) 既存の全アクション。chord の 1 打鍵目とぶつかっても死ぬので
        //    `first()` 同士で見る。非 mac の ⌃→⌘ 畳み込みは `same_stroke` が吸収する。
        for a in crate::keybinds::ALL_ACTIONS {
            let d = crate::keybinds::default_binding(a);
            assert!(
                !crate::keybinds::same_stroke(d.first(), first),
                "{spec:?} は {a:?} の既定打鍵とぶつかる"
            );
        }

        // 3) egui-winit 0.29 が押下ごと Cut/Copy/Paste へすり替える組み合わせ
        //    (shift / alt の有無を見ていないので ⌘⇧X も死ぬ)。
        let swallowed = [egui::Key::X, egui::Key::C, egui::Key::V];
        assert!(
            !(first.modifiers.command && swallowed.contains(&first.logical_key)),
            "{spec:?} は egui-winit がイベントごと飲み込むので絶対に発火しない"
        );
    }

    #[test]
    fn 登録の識別子はモジュール接頭辞を持ち打鍵はそれを指す() {
        assert_eq!(FEATURE.module, "jump");
        for e in FEATURE.entries {
            assert!(e.id.starts_with("jump."), "{:?}", e.id);
        }
        for b in FEATURE.binds {
            assert!(
                FEATURE.entries.iter().any(|e| e.id == b.id),
                "打鍵 {:?} が指す ID が entries に無い",
                b.id
            );
        }
        for s in FEATURE.settings {
            assert!(s.key.starts_with("jump."), "{:?}", s.key);
        }
    }
}
