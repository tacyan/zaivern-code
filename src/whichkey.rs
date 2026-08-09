//! which-key ポップアップ — chord (2 打鍵) の 1 打鍵目を握っている間、
//! **そこから続く打鍵の一覧**を画面の右下に出す。
//!
//! ## なぜ「1 行のヒント」ではなく一覧なのか
//!
//! これまでは ステータスバーに「⌘K が押されました。待機中…」と 1 行出すだけで、
//! **次に何を押せるのかは画面のどこにも無かった**。忘れた人はキーバインド表
//! (⌘K ⌘S) を開くしかないが、その打鍵自体を忘れているから困っている。
//!
//! ## 2 つの待ち時間 (この実装の核)
//!
//! ```text
//! Idle --prefix--> Pending{started, shown:false}
//! Pending && !shown && 経過 >= FIRST_DELAY -> shown = true
//! Pending &&  shown && 次の打鍵で降りる    -> 同じフレームで描き直す (2 段目は 0ms)
//! 確定 / Esc / prefix が消えた             -> Idle
//! ```
//!
//! - [`DEFAULT_FIRST_DELAY`] = **200ms** (which-key.nvim の既定)。
//!   Emacs は 1 秒、Zed も 1 秒で、**Zed のこの実装への不満の第 1 位がまさに
//!   「遅い」**。1 段目の待ちがあるのは、chord を 300ms で打ち切る手慣れた人に
//!   ポップアップを**一度も見せない**ため。
//! - [`SECOND_DELAY`] = **0ms**。一度出た時点で「迷っている」と自己申告した
//!   のと同じなので、2 打鍵目以降を待たせる理由が無い。
//!
//! 1 段目の待ちは `config.toml` の `whichkey_delay_ms` で変えられる (0 = 即座)。
//!
//! ## アイドルのコストはゼロ
//!
//! 待機中だけ [`crate::perf::repaint_after`] を「出す時刻ちょうど」に予約する。
//! 毎フレームのポーリングもアニメーションも持たない (設計原則 3)。
//!
//! ## 実データの行 (which-key.nvim の content plugin 相当)
//!
//! which-key.nvim の最も効いている発想は、**同じウィジェットに静的な
//! キーバインドではなく実データを描く**こと (`'` はマーク一覧、`"` はレジスタの
//! 中身を出す)。ここでは [`LiveRow`] がそれで、`]` / `[` (差分ファイル間の移動)
//! を握っている間は **いま変更のあるファイル**が行として並ぶ。
//! `]f` は「次のファイルへ」を目隠しで撃つ打鍵だが、握っているだけで
//! 行き先の一覧が見えて、番号で直接飛べる。

use crate::i18n::{tr, trf};
use crate::keybinds::{self, BindAction, Binding, Keybinds};
use egui::{Key, KeyboardShortcut, Modifiers, Pos2, Rect, Vec2};
use std::time::Duration;

// ─────────────────────────────────────────────────────────────────────────
// 待ち時間
// ─────────────────────────────────────────────────────────────────────────

/// 1 打鍵目を押してからポップアップを出すまで (既定)。
///
/// which-key.nvim の既定と同じ 200ms。Emacs (1 秒) / Zed (1 秒) は遅すぎて、
/// 「出たころには自分で思い出している」になる。**この待ちがあるおかげで、
/// chord を淀みなく打つ人はポップアップを 1 度も見ない。**
pub const DEFAULT_FIRST_DELAY_MS: u64 = 200;

/// `whichkey_delay_ms` に許す上限 (ms)。これ以上は「実質出ない」と同じなので
/// 設定画面のスライダを無駄に長くしない。
pub const MAX_FIRST_DELAY_MS: u64 = 5_000;

/// 2 打鍵目以降の待ち。**0 固定**。
///
/// 一度出た = 利用者が「分からない」と自己申告した状態なので、
/// そこから先はもう待たせない (which-key.nvim / Helix と同じ)。
pub const SECOND_DELAY: Duration = Duration::ZERO;

/// 設定値 (ms) から 1 段目の待ちを作る。範囲外は丸める。
pub fn first_delay(cfg_ms: u64) -> Duration {
    Duration::from_millis(cfg_ms.min(MAX_FIRST_DELAY_MS))
}

// ─────────────────────────────────────────────────────────────────────────
// ポップアップ自身が拾う打鍵
// ─────────────────────────────────────────────────────────────────────────

/// 待機を捨てる。実際の消費は [`keybinds::ChordState::begin_frame`] が行い、
/// ここは表示を追従させるだけ。
pub const CANCEL_KEY: Key = Key::Escape;
/// 1 打鍵ぶん戻る (Zed に無くて最も要望されている操作)。
pub const POP_KEY: Key = Key::Backspace;
/// 検索できる全ショートカット一覧へ抜ける。
pub const ALL_KEY: Key = Key::Questionmark;

/// 実データ行に振る番号キー (1〜9)。`0` は使わない (10 個目以降は出さない)。
const DIGIT_KEYS: [Key; 9] = [
    Key::Num1,
    Key::Num2,
    Key::Num3,
    Key::Num4,
    Key::Num5,
    Key::Num6,
    Key::Num7,
    Key::Num8,
    Key::Num9,
];

/// 実データ行の上限。これを超えたぶんは出さない
/// (画面を埋め尽くすより「番号で選べる範囲」に収める)。
pub const MAX_LIVE_ROWS: usize = DIGIT_KEYS.len();

/// 番号キーの打鍵表記 (`1`〜`9`)。**ベタ書きせず生成する。**
pub fn digit_label(idx: usize) -> Option<String> {
    DIGIT_KEYS
        .get(idx)
        .map(|k| keybinds::format_shortcut(KeyboardShortcut::new(Modifiers::NONE, *k)))
}

// ─────────────────────────────────────────────────────────────────────────
// 状態機械 (純粋)
// ─────────────────────────────────────────────────────────────────────────

/// いまポップアップをどうすべきか。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// prefix を握っていない
    Idle,
    /// 握っているが、まだ 1 段目の待ちが明けていない (**何も描かない**)
    Hidden,
    /// 出す
    Shown,
}

/// which-key の表示状態。フレームを跨ぐので `App` が持つ。
///
/// `path` は打鍵の並びで持つ。いまの [`Binding`] は 2 打鍵までしか表せないので
/// 実際には長さ 1 だが、**深い chord が入った日に状態側を直さなくて済む**ように
/// 最初から並びで持つ ([`Self::pop`] のテストもその形で書いてある)。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WhichKey {
    path: Vec<KeyboardShortcut>,
    /// 出す時刻 (`InputState::time` と同じ「起動からの秒」)。
    ///
    /// 開始時刻ではなく**期限**で持つのは [`keybinds::ChordState`] と同じ理由 —
    /// `now - started >= delay` は起動から数時間経った大きな `now` で
    /// 丸め誤差が出て、境界のちょうどで 1 フレーム遅れる
    /// (実際にテストが `10.2 - 10.0 < 0.2` で落ちた)。
    deadline: f64,
    /// 1 段目の待ちが明けたか。**一度 true になったら path が伸びても true のまま**
    /// (= 2 段目の待ちは 0)。
    shown: bool,
}

impl WhichKey {
    /// 握っている打鍵の並び。
    pub fn path(&self) -> &[KeyboardShortcut] {
        &self.path
    }

    pub fn is_active(&self) -> bool {
        !self.path.is_empty()
    }

    /// 1 打鍵ぶん降りる。**期限は先頭を握ったときのまま**動かさない
    /// (2 段目以降は待たせない = [`SECOND_DELAY`] が 0)。
    pub fn push(&mut self, sc: KeyboardShortcut, now: f64, first_delay: Duration) {
        if self.path.is_empty() {
            self.deadline = now + first_delay.as_secs_f64();
            self.shown = false;
        } else if self.shown {
            // 既に出ている = 利用者が「分からない」と自己申告した状態。
            // ここから先は 1 ミリ秒も待たせない ([`SECOND_DELAY`] = 0)。
            self.deadline = now + SECOND_DELAY.as_secs_f64();
        }
        // まだ出ていない途中で降りたときは **期限を動かさない** —
        // 淀みなく打ち切る人には最後まで見せない。
        self.path.push(sc);
    }

    /// 1 打鍵ぶん戻る (`Backspace`)。戻る先が無ければ false。
    ///
    /// 空になったら [`Phase::Idle`] へ落ちる = chord の待機も捨てる、が呼び出し側の責務。
    pub fn pop(&mut self) -> bool {
        if self.path.pop().is_none() {
            return false;
        }
        if self.path.is_empty() {
            self.shown = false;
        }
        true
    }

    pub fn clear(&mut self) {
        self.path.clear();
        self.shown = false;
    }

    /// [`keybinds::ChordState`] の待機を表示状態へ写す。
    ///
    /// * `pending` が None → Idle へ落とす (時間切れ・Esc・確定のいずれも同じ)
    /// * `pending` が今の先頭と違う → 握り直し (時計もリセット)
    /// * 同じ → そのまま (深い path は [`Self::push`] が育てたものなので壊さない)
    pub fn sync(&mut self, pending: Option<KeyboardShortcut>, now: f64, first_delay: Duration) {
        match pending {
            None => self.clear(),
            Some(sc) => {
                let same = self
                    .path
                    .first()
                    .is_some_and(|f| keybinds::same_stroke(*f, sc));
                if !same {
                    self.clear();
                    self.push(sc, now, first_delay);
                }
            }
        }
    }

    /// いまの段階。1 段目の待ちが明けたらここで `shown` が立つ。
    pub fn phase(&mut self, now: f64) -> Phase {
        if self.path.is_empty() {
            return Phase::Idle;
        }
        if self.shown {
            return Phase::Shown;
        }
        if now >= self.deadline {
            self.shown = true;
            return Phase::Shown;
        }
        Phase::Hidden
    }

    /// 出るまでの残り時間。**再描画の予約はこの値ちょうどで 1 回だけ**行う
    /// (毎フレームのポーリングを持たないため)。出るまでもなければ None。
    pub fn until_shown(&self, now: f64) -> Option<Duration> {
        if self.path.is_empty() || self.shown {
            return None;
        }
        Some(Duration::from_secs_f64((self.deadline - now).max(0.0)))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 行の組み立て
// ─────────────────────────────────────────────────────────────────────────

/// 打鍵の並び 1 本ぶんの定義。
///
/// **説明は [`keybinds::action_label`] から来る** = アクション定義に貼り付いて
/// いるので、別表を持たせて片方だけ古くなる、が起こらない (仕様 7)。
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub path: Vec<KeyboardShortcut>,
    pub label: String,
    /// 途中段の表示名。`groups[i]` が `path[..=i]` の塊の名前。
    ///
    /// **後付けの書き換え表ではなく定義側が持つ** — Zed の which-key は
    /// 人が読める塊の名前を 1 つも持たず `+{n} keybinds` としか出せず、
    /// それが公開されている残課題の筆頭になっている。
    pub groups: Vec<String>,
}

/// 行の出どころ。
#[derive(Clone, Debug, PartialEq)]
pub enum RowKind {
    /// キーバインド表の 1 アクション (押せば発火する。ここからは実行しない)
    Action(BindAction),
    /// さらに下の段がある塊 (`+名前`)
    Group,
    /// 実行時の状態から来た行 ([`LiveRow`] の添字)
    Live(usize),
}

/// ポップアップの 1 行。
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// 次に押す打鍵 (同じ説明の行を畳んだ結果、複数になることがある)。
    pub keys: Vec<String>,
    pub desc: String,
    pub kind: RowKind,
}

impl Row {
    /// 打鍵の列に出す文字列 (`j, ↓`)。
    pub fn key_text(&self) -> String {
        self.keys.join(", ")
    }
}

/// 実行時の状態から作る行 (which-key.nvim の content plugin 相当)。
#[derive(Clone, Debug, PartialEq)]
pub struct LiveRow {
    /// 画面に出す説明 (ファイル名など)。
    pub desc: String,
    /// ホバーで出す全文 (省略された行の全体)。空なら `desc` を使う。
    pub detail: String,
}

/// [`Keybinds`] から chord の定義を起こす。
///
/// いまの [`Binding`] は 2 打鍵までなので `groups` は常に空になる
/// (中間段が存在しない)。深い chord が入れば [`rows_for`] 側は既に対応済み。
pub fn entries_from_keybinds(keys: &Keybinds) -> Vec<Entry> {
    let mut out = Vec::new();
    for a in keybinds::ALL_ACTIONS {
        if let Binding::Chord(first, second) = keys.binding(a) {
            out.push(Entry {
                path: vec![first, second],
                label: tr(keybinds::action_label(a)),
                groups: Vec::new(),
            });
        }
    }
    out
}

/// `prefix` から続く行を作る。
///
/// 1. `prefix` で始まり、かつ 1 段以上長い定義だけを拾う
/// 2. 次の 1 打鍵が同じで説明も同じものは 1 行へ畳む (仕様 3 — Helix と同じ)
/// 3. 塊 (`+名前`) は最後へ回す (仕様 6)
/// 4. 実データの行を最後に足す
pub fn rows_for(entries: &[Entry], prefix: &[KeyboardShortcut], live: &[LiveRow]) -> Vec<Row> {
    let depth = prefix.len();
    // 次の 1 打鍵 → (説明, 塊かどうか, 塊の中身の説明)
    let mut leaves: Vec<(String, String, BindActionSlot)> = Vec::new();
    let mut groups: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
    for e in entries {
        if e.path.len() <= depth {
            continue;
        }
        let starts = prefix
            .iter()
            .zip(e.path.iter())
            .all(|(a, b)| keybinds::same_stroke(*a, *b));
        if !starts {
            continue;
        }
        let next = keybinds::format_shortcut(e.path[depth]);
        if e.path.len() == depth + 1 {
            leaves.push((next, e.label.clone(), BindActionSlot(e.action())));
            continue;
        }
        // 中間段 = 塊。名前は定義側 (`groups`) が持っていればそれを、
        // 無ければ中身の説明から導く (後付けの書き換え表は作らない)。
        let named = e.groups.get(depth).cloned().unwrap_or_default();
        match groups.iter_mut().find(|(k, _, _)| *k == next) {
            Some((_, names, members)) => {
                if !named.is_empty() {
                    names.push(named);
                }
                members.push(e.label.clone());
            }
            None => {
                let names = if named.is_empty() {
                    Vec::new()
                } else {
                    vec![named]
                };
                groups.push((next, names, vec![e.label.clone()]));
            }
        }
    }

    let mut rows = coalesce(leaves);
    for (key, names, members) in groups {
        rows.push(Row {
            keys: vec![key],
            desc: group_name(&names, &members),
            kind: RowKind::Group,
        });
    }
    // 実データの行 (番号キー)。**畳まない** — 同名のファイルでも別の行。
    for (i, l) in live.iter().take(MAX_LIVE_ROWS).enumerate() {
        if let Some(k) = digit_label(i) {
            rows.push(Row {
                keys: vec![k],
                desc: l.desc.clone(),
                kind: RowKind::Live(i),
            });
        }
    }
    rows
}

/// 説明が同じ行を 1 本へ畳む。打鍵は行の中で整列する (仕様 3)。
fn coalesce(leaves: Vec<(String, String, BindActionSlot)>) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for (key, desc, slot) in leaves {
        match rows.iter_mut().find(|r| r.desc == desc) {
            Some(r) => {
                if !r.keys.contains(&key) {
                    r.keys.push(key);
                    r.keys.sort();
                }
            }
            None => rows.push(Row {
                keys: vec![key],
                desc,
                kind: match slot.0 {
                    Some(a) => RowKind::Action(a),
                    None => RowKind::Group,
                },
            }),
        }
    }
    rows.sort_by(|a, b| a.keys.first().cmp(&b.keys.first()));
    rows
}

/// 塊の表示名。定義側の名前 → 中身の説明の共通部分 → 件数、の順に落ちる。
///
/// **`+{n} 個` まで落ちるのは名前を作れなかったときだけ。** Zed はここが
/// 常に `+{n} keybinds` で、何の塊なのか画面から一切分からない。
fn group_name(names: &[String], members: &[String]) -> String {
    if let Some(n) = names.iter().find(|n| !n.is_empty()) {
        return format!("+{n}");
    }
    if let Some(common) = common_label_head(members) {
        return format!("+{common}");
    }
    trf("+{n} 個", &[("n", members.len().to_string())])
}

/// 中身の説明に共通する見出し (`折りたたみ: 1 段` `折りたたみ: 2 段` → `折りたたみ`)。
/// 区切りは `: ` と `/`。共通部分が無ければ None。
fn common_label_head(members: &[String]) -> Option<String> {
    let head = |s: &str| -> Option<String> {
        let cut = s.find(": ").or_else(|| s.find('/'))?;
        let h = s[..cut].trim();
        (!h.is_empty()).then(|| h.to_string())
    };
    let first = head(members.first()?)?;
    members
        .iter()
        .skip(1)
        .all(|m| head(m).as_deref() == Some(first.as_str()))
        .then_some(first)
}

/// [`Entry`] から `BindAction` を引くための入れ物。
///
/// `Entry` は打鍵の並びだけの汎用形にしておきたい (テストで合成できる) 一方、
/// 実物の行はどのアクションか分かっていた方がよいので、
/// [`entries_from_keybinds`] が作った並びだけ復元できるようにしてある。
struct BindActionSlot(Option<BindAction>);

impl Entry {
    /// この定義に対応する `BindAction` (打鍵が一致するものを 1 つ)。
    /// 合成した Entry (テスト用) では None。
    fn action(&self) -> Option<BindAction> {
        let last = *self.path.last()?;
        let first = *self.path.first()?;
        keybinds::ALL_ACTIONS.into_iter().find(|a| {
            matches!(keybinds::default_binding(*a), Binding::Chord(p, s)
                if keybinds::same_stroke(p, first)
                    && keybinds::same_stroke(s, last)
                    && tr(keybinds::action_label(*a)) == self.label)
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 詰め込み (which-key.nvim の列割り) — 純関数
// ─────────────────────────────────────────────────────────────────────────

/// 1 列の最小幅。これを割るくらいなら列を減らす。
pub const MIN_COL_W: f32 = 168.0;
/// 列と列のあいだ。
pub const COL_GAP: f32 = 14.0;
/// 打鍵の列と説明のあいだ。
pub const KEY_DESC_GAP: f32 = 8.0;
/// カードの内側の余白。
pub const CARD_PAD: f32 = 10.0;
/// カードと画面の縁のあいだ。
pub const SCREEN_MARGIN: f32 = 12.0;
/// カードが使ってよい幅の割合 (本文を覆い尽くさない)。
const MAX_W_RATIO: f32 = 0.66;
/// カードが使ってよい高さの割合。
const MAX_H_RATIO: f32 = 0.52;

/// 列割りの結果。`cols * col_w + (cols-1) * COL_GAP <= container_w` を必ず満たす。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Packing {
    pub cols: usize,
    pub col_w: f32,
    /// 1 列に積む行数。
    pub rows_per_col: usize,
}

/// which-key.nvim の列割りをそのまま移したもの。
///
/// ```text
/// col_w        = clamp(最長行幅, MIN_COL_W, container_w)
/// cols         = max(floor(container_w / (col_w + gap)), 1)
/// col_w        = floor((container_w - gap*(cols-1)) / cols)   // 余りを配り直す
/// rows_per_col = max(ceil(n / cols), 2)
/// ```
///
/// **Emacs の「高さ最小化探索」は移植しない。** あれは打鍵のたびに組み直して
/// ポップアップの高さが変わるので、「画面が突然変わらない」に真っ向から反する
/// (Emacs の which-key で最も嫌われている挙動でもある)。
///
/// `max_lines` は 1 列に積める上限 (高さの都合)。超えるときは**列を増やして**
/// 収める。それでも入らなければ `rows_per_col > max_lines` のまま返す
/// (呼び出し側がスクロールさせる)。
pub fn pack(container_w: f32, max_row_w: f32, item_count: usize, max_lines: usize) -> Packing {
    let container = container_w.max(1.0);
    let n = item_count;
    if n == 0 {
        return Packing {
            cols: 1,
            col_w: container,
            rows_per_col: 0,
        };
    }
    // 1) 行の実幅を下限と container で挟む
    let want = max_row_w.max(MIN_COL_W).min(container);
    // 2) 何列入るか
    let mut cols = ((container / (want + COL_GAP)).floor() as usize).max(1);
    // 3) 1 列あたりの行数。2 未満にはしない (1 行だけの列が横に並ぶのを避ける)
    //    ただし件数そのものは超えない (**空の行を確保しない**)。
    let mut per = n.div_ceil(cols).max(2).min(n);
    // 4) 高さが足りなければ列を増やして収める (1 回だけ。探索はしない)
    if per > max_lines.max(1) {
        let need = n.div_ceil(max_lines.max(1));
        let fit = ((container / (MIN_COL_W + COL_GAP)).floor() as usize).max(1);
        cols = cols.max(need.min(fit));
        per = n.div_ceil(cols).max(1).min(n);
    }
    // 5) 空の列を作らない (per を丸めた結果、後ろの列が空になることがある)
    cols = n.div_ceil(per.max(1)).max(1);
    let gaps = COL_GAP * (cols.saturating_sub(1)) as f32;
    let col_w = ((container - gaps) / cols as f32).floor().max(1.0);
    Packing {
        cols,
        col_w,
        rows_per_col: per,
    }
}

/// ポップアップの配置。**`popup` は必ず `area` の中に収まる。**
#[derive(Clone, PartialEq, Debug)]
pub struct Layout {
    /// カード全体 (枠と余白を含む)。
    pub popup: Rect,
    /// 行を敷き詰める領域 (カードの内側・見出しの下)。
    /// スクロールするときは `viewport` より高い。
    pub content: Rect,
    /// 実際に見えている高さぶんの領域。
    pub viewport: Rect,
    /// 行 1 つぶんの矩形 (`content` の中。互いに重ならない)。
    pub cells: Vec<Rect>,
    pub packing: Packing,
    /// 高さが足りず、スクロールが要るか。
    pub scroll: bool,
}

/// 画面 (中央ビュー) の矩形と行の必要幅から配置を決める。
///
/// * 右下を錨にして**錨から遠い方向 (左と上) へ育つ** — 本文を押しのけない。
///   Emacs のように画面幅いっぱいの下部ウィンドウを開くとレイアウトが動くので採らない。
/// * `bottom_inset` はステータスバー等、下端から空けておく高さ。
/// * `row_ws[i]` は行 i の必要幅 (打鍵列 + すきま + 説明)。
pub fn layout(
    area: Rect,
    bottom_inset: f32,
    row_ws: &[f32],
    row_h: f32,
    header_h: f32,
    ppp: f32,
) -> Layout {
    let snap = |v: f32| crate::theme::snap_len(v, ppp);
    let row_h = row_h.max(1.0);
    let n = row_ws.len();

    // 使ってよい外寸 (画面より大きくならない)
    let max_w = (area.width() - SCREEN_MARGIN * 2.0)
        .min(area.width() * MAX_W_RATIO)
        .max(1.0)
        .min(area.width());
    let max_h = (area.height() - bottom_inset - SCREEN_MARGIN)
        .min(area.height() * MAX_H_RATIO)
        .max(row_h + header_h + CARD_PAD * 2.0)
        .min(area.height());

    let container_w = (max_w - CARD_PAD * 2.0).max(1.0);
    let grid_h_budget = (max_h - CARD_PAD * 2.0 - header_h).max(row_h);
    let max_lines = ((grid_h_budget / row_h).floor() as usize).max(1);

    let max_row_w = row_ws.iter().cloned().fold(0.0_f32, f32::max);
    let p = pack(container_w, max_row_w, n, max_lines);

    let grid_w = snap(p.col_w * p.cols as f32 + COL_GAP * p.cols.saturating_sub(1) as f32);
    let grid_h = snap(p.rows_per_col as f32 * row_h);
    let scroll = p.rows_per_col > max_lines;
    let view_h = if scroll { snap(grid_h_budget) } else { grid_h };

    let card_w = snap((grid_w + CARD_PAD * 2.0).min(area.width()));
    let card_h = snap((view_h + header_h + CARD_PAD * 2.0).min(area.height()));

    // 右下の錨。カードは左と上へ育つ。
    let right = area.right() - SCREEN_MARGIN;
    let bottom = area.bottom() - bottom_inset;
    let mut min = Pos2::new(right - card_w, bottom - card_h);
    // どんなに狭くても画面からはみ出させない
    min.x = min.x.max(area.left());
    min.y = min.y.max(area.top());
    let popup = Rect::from_min_size(min, Vec2::new(card_w, card_h));

    let grid_origin = Pos2::new(
        snap(popup.left() + CARD_PAD),
        popup.top() + CARD_PAD + header_h,
    );
    let content = Rect::from_min_size(grid_origin, Vec2::new(grid_w, grid_h));
    let viewport = Rect::from_min_size(grid_origin, Vec2::new(grid_w, view_h));

    let mut cells = Vec::with_capacity(n);
    for i in 0..n {
        let col = i / p.rows_per_col.max(1);
        let line = i % p.rows_per_col.max(1);
        let x = snap(grid_origin.x + col as f32 * (p.col_w + COL_GAP));
        let y = grid_origin.y + line as f32 * row_h;
        cells.push(Rect::from_min_size(
            Pos2::new(x, y),
            Vec2::new(p.col_w, row_h),
        ));
    }

    Layout {
        popup,
        content,
        viewport,
        cells,
        packing: p,
        scroll,
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 描画
// ─────────────────────────────────────────────────────────────────────────

/// ポップアップが返す操作。`App` 側で 1 か所だけ捌く。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    None,
    /// `Backspace` — 1 打鍵戻す (空になったら待機ごと捨てる)
    Pop,
    /// `?` — 検索できる全ショートカット一覧へ抜ける
    OpenAll,
    /// 実データの行が選ばれた (番号キー or クリック)
    Pick(usize),
}

/// 描画に要る材料。**`App` からの呼び出しを 1 つに保つため 1 つの構造体で渡す。**
pub struct Params<'a> {
    /// [`keybinds::ChordState::pending`] の値。
    pub pending: Option<KeyboardShortcut>,
    pub keys: &'a Keybinds,
    pub theme: &'a crate::theme::Theme,
    /// 実データの行 (無ければ空)。
    pub live: &'a [LiveRow],
    /// 1 段目の待ち。
    pub first_delay: Duration,
    /// ポップアップを置いてよい領域 (中央ビュー)。
    pub area: Rect,
    /// 下端から空ける高さ (ステータスバー等)。
    pub bottom_inset: f32,
}

/// which-key ポップアップを描く。**待機していないフレームでは 1 ピクセルも出さない。**
pub fn popup_ui(ctx: &egui::Context, st: &mut WhichKey, p: Params<'_>) -> Outcome {
    let now = ctx.input(|i| i.time);
    st.sync(p.pending, now, p.first_delay);
    match st.phase(now) {
        Phase::Idle => return Outcome::None,
        Phase::Hidden => {
            // 出す時刻ちょうどに 1 回だけ起こす (常時ポーリングしない)
            if let Some(left) = st.until_shown(now) {
                crate::perf::repaint_after(ctx, left, "whichkey");
            }
            return Outcome::None;
        }
        Phase::Shown => {}
    }

    let entries = entries_from_keybinds(p.keys);
    let rows = rows_for(&entries, st.path(), p.live);
    // 続きが 1 つも無い prefix でも「何を握っているか」は出す
    // (何も出さないと「固まった」に見える。ここは 1 行の見出しだけで済む)。
    draw(ctx, st, &p, &rows)
}

/// ポップアップが拾う打鍵 (`Backspace` / `?` / 実データの番号) を取る。
///
/// **描画 ([`popup_ui`]) より前、パネルを描くより前に呼ぶこと。**
/// ポップアップは中央ビューの上に最後に描くが、そのころには本文の
/// `TextEdit` が Backspace を食べ終わっている (フォーカスがあれば必ず取る)。
/// 打鍵の取り合いはフレームの頭で決着させる。
///
/// `live_len` は実データ行の数 (0 なら番号キーは拾わない = 素の数字入力を
/// 横取りしない)。呼び出し側は chord を握っている間だけ呼ぶこと。
///
/// **消費は必ず [`keybinds::consume_shortcut_compat`] を通す** — 素の
/// `consume_shortcut` は egui-winit にすり替えられた打鍵を取りこぼす。
pub fn take_keys(ctx: &egui::Context, live_len: usize) -> Option<Outcome> {
    ctx.input_mut(|i| {
        if keybinds::consume_shortcut_compat(i, KeyboardShortcut::new(Modifiers::NONE, POP_KEY)) {
            return Some(Outcome::Pop);
        }
        // `?` は配列によって shift 付き / 無しの両方で届く
        for m in [Modifiers::SHIFT, Modifiers::NONE] {
            if keybinds::consume_shortcut_compat(i, KeyboardShortcut::new(m, ALL_KEY)) {
                return Some(Outcome::OpenAll);
            }
        }
        for (idx, k) in DIGIT_KEYS
            .iter()
            .enumerate()
            .take(live_len.min(MAX_LIVE_ROWS))
        {
            if keybinds::consume_shortcut_compat(i, KeyboardShortcut::new(Modifiers::NONE, *k)) {
                return Some(Outcome::Pick(idx));
            }
        }
        None
    })
}

fn draw(ctx: &egui::Context, st: &WhichKey, p: &Params<'_>, rows: &[Row]) -> Outcome {
    let theme = p.theme;
    let ppp = ctx.pixels_per_point();
    let body = egui::FontId::proportional(12.0);
    let head = egui::FontId::proportional(11.0);

    let measure = |text: &str, font: &egui::FontId| -> f32 {
        ctx.fonts(|f| {
            f.layout_no_wrap(text.to_string(), font.clone(), theme.text)
                .size()
                .x
        })
    };

    // 打鍵の列幅は表全体で 1 回だけ決める (行ごとに変えると列がぶれる)
    let key_w = rows
        .iter()
        .map(|r| measure(&r.key_text(), &body))
        .fold(0.0_f32, f32::max)
        .min(120.0);
    let desc_ws: Vec<f32> = rows.iter().map(|r| measure(&r.desc, &body)).collect();
    let row_ws: Vec<f32> = desc_ws.iter().map(|w| key_w + KEY_DESC_GAP + w).collect();
    let row_h = crate::theme::snap_len(body.size + 8.0, ppp);

    let title = header_text(st, rows.len());
    let hint = hint_text();
    let header_h = crate::theme::snap_len(head.size + 6.0, ppp) * 2.0;
    let lay = layout(p.area, p.bottom_inset, &row_ws, row_h, header_h, ppp);

    let mut out = Outcome::None;
    egui::Area::new(egui::Id::new("zv-whichkey"))
        .order(egui::Order::Foreground)
        .fixed_pos(lay.popup.min)
        .constrain_to(p.area)
        .show(ctx, |ui| {
            egui::Frame::none()
                .fill(theme.panel)
                .stroke(egui::Stroke::new(
                    1.0_f32,
                    theme.accent.gamma_multiply(0.55),
                ))
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin::same(CARD_PAD))
                .show(ui, |ui| {
                    ui.set_width(lay.popup.width() - CARD_PAD * 2.0);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(title)
                                .font(head.clone())
                                .color(theme.accent)
                                .strong(),
                        )
                        .selectable(false)
                        .truncate(),
                    );
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(hint).font(head).color(theme.text_dim),
                        )
                        .selectable(false)
                        .truncate(),
                    );
                    if rows.is_empty() {
                        return;
                    }
                    egui::ScrollArea::vertical()
                        // ScrollArea は `make_persistent_id` を通るので
                        // **必ず salt を付ける** (CLAUDE.md の既知の罠)。
                        .id_salt("zv-whichkey-rows")
                        .max_height(lay.viewport.height())
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            out = grid(ui, theme, &lay, rows, &desc_ws, key_w, &body, ppp);
                        });
                });
        });
    out
}

/// 行を敷く。列は右寄せの打鍵 + 伸縮する説明。
#[allow(clippy::too_many_arguments)]
fn grid(
    ui: &mut egui::Ui,
    theme: &crate::theme::Theme,
    lay: &Layout,
    rows: &[Row],
    desc_ws: &[f32],
    key_w: f32,
    font: &egui::FontId,
    ppp: f32,
) -> Outcome {
    let (grid_rect, _) = ui.allocate_exact_size(lay.content.size(), egui::Sense::hover());
    let shift = grid_rect.min - lay.content.min;
    let mut out = Outcome::None;
    for (i, r) in rows.iter().enumerate() {
        let Some(cell) = lay.cells.get(i) else { break };
        let cell = cell.translate(shift);
        if !ui.is_rect_visible(cell) {
            continue;
        }
        let live = matches!(r.kind, RowKind::Live(_));
        let group = matches!(r.kind, RowKind::Group);
        // **実データの行だけ押せる。** キーバインドの行は「押す打鍵の案内」なので
        // クリックの対象にしない (同じ操作への到達経路を増やさない)。
        // ホバーはどの行でも取る — 省略された説明の全文を出すため。
        let sense = if live {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        };
        let resp = ui.interact(cell, ui.id().with(("wk", i)), sense);
        if live {
            if resp.clicked() {
                if let RowKind::Live(idx) = r.kind {
                    out = Outcome::Pick(idx);
                }
            }
            if resp.hovered() {
                ui.painter().rect_filled(
                    cell,
                    egui::Rounding::same(4.0),
                    theme.accent_soft.gamma_multiply(0.5),
                );
            }
        }

        let key_col = crate::theme::snap_len(key_w, ppp);
        let key_txt = r.key_text();
        let kg = ui.fonts(|f| f.layout_no_wrap(key_txt, font.clone(), theme.accent));
        let kx = crate::theme::snap_len(cell.left() + key_col - kg.size().x, ppp);
        ui.painter().galley(
            egui::pos2(kx, cell.top() + (cell.height() - kg.size().y) * 0.5),
            kg,
            theme.accent,
        );

        let dx = crate::theme::snap_len(cell.left() + key_col + KEY_DESC_GAP, ppp);
        let dw = (cell.right() - dx).max(1.0);
        let color = if group { theme.warn } else { theme.text };
        let dg = ui.fonts(|f| f.layout_job(ellipsis_job(&r.desc, font.clone(), color, dw)));
        ui.painter().galley(
            egui::pos2(dx, cell.top() + (cell.height() - dg.size().y) * 0.5),
            dg,
            color,
        );
        // 省略した行だけホバーで全文を出す (切れていない行に吹き出しを出さない)。
        // 判定は**組む前に測った実幅**で行う — 組んだ後の galley は必ず
        // `max_width` 以下に収まるので、そこからは切れたか分からない。
        if desc_ws.get(i).is_some_and(|w| *w > dw) {
            resp.on_hover_text(&r.desc);
        }
    }
    out
}

/// 1 行に収め、あふれたら `…` で切るレイアウト指定。
fn ellipsis_job(
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    max_w: f32,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping {
        max_width: max_w,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    job
}

/// 見出し (握っている打鍵と件数)。**打鍵表記は必ず生成する。**
fn header_text(st: &WhichKey, n: usize) -> String {
    let held = st
        .path()
        .iter()
        .map(|sc| keybinds::format_shortcut(*sc))
        .collect::<Vec<_>>()
        .join(" ");
    trf(
        "{keys} — 続けて押せる打鍵 ({n})",
        &[("keys", held), ("n", n.to_string())],
    )
}

/// 操作の案内。**打鍵表記は [`keybinds::format_shortcut`] から作る**
/// (Esc / ⌫ / ? はキーバインド表に無い固定キーなので、ここが唯一の出所)。
fn hint_text() -> String {
    let f = |k: Key| keybinds::format_shortcut(KeyboardShortcut::new(Modifiers::NONE, k));
    trf(
        "{esc}: 取消 ・ {pop}: 1 つ戻る ・ {all}: すべて表示",
        &[
            ("esc", f(CANCEL_KEY)),
            ("pop", f(POP_KEY)),
            ("all", f(ALL_KEY)),
        ],
    )
}

// ════════════════════════════════════════════════════════════════════════
// テスト
// ════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn sc(m: Modifiers, k: Key) -> KeyboardShortcut {
        KeyboardShortcut::new(m, k)
    }

    fn kb() -> Keybinds {
        Keybinds::from_overrides(&HashMap::new())
    }

    fn entry(path: &[KeyboardShortcut], label: &str, groups: &[&str]) -> Entry {
        Entry {
            path: path.to_vec(),
            label: label.to_string(),
            groups: groups.iter().map(|s| s.to_string()).collect(),
        }
    }

    // ── 状態機械 ────────────────────────────────────────────────────

    #[test]
    fn 一段目の待ちが明けるまでは出さない() {
        let d = first_delay(DEFAULT_FIRST_DELAY_MS);
        let mut st = WhichKey::default();
        assert_eq!(st.phase(10.0), Phase::Idle);
        st.sync(Some(sc(Modifiers::COMMAND, Key::K)), 10.0, d);
        assert_eq!(st.phase(10.0), Phase::Hidden);
        // 199ms ではまだ出ない (chord を淀みなく打つ人には見せない)
        assert_eq!(st.phase(10.199), Phase::Hidden);
        assert_eq!(st.phase(10.2), Phase::Shown);
    }

    #[test]
    fn 二段目の待ちはゼロ() {
        let d = first_delay(DEFAULT_FIRST_DELAY_MS);
        let mut st = WhichKey::default();
        st.sync(Some(sc(Modifiers::COMMAND, Key::K)), 0.0, d);
        assert_eq!(st.phase(0.3), Phase::Shown);
        // 出たあとに 1 段降りても、そのフレームで出たまま
        st.push(sc(Modifiers::NONE, Key::A), 0.31, d);
        assert_eq!(st.phase(0.31), Phase::Shown);
        assert_eq!(st.path().len(), 2);
        assert_eq!(SECOND_DELAY, Duration::ZERO);
    }

    #[test]
    fn 素早く打ち切ればポップアップは一度も出ない() {
        let d = first_delay(DEFAULT_FIRST_DELAY_MS);
        let mut st = WhichKey::default();
        st.sync(Some(sc(Modifiers::COMMAND, Key::K)), 5.0, d);
        // 120ms で 2 打鍵目が決まり、chord が解けた
        assert_eq!(st.phase(5.12), Phase::Hidden);
        st.sync(None, 5.12, d);
        assert_eq!(st.phase(5.12), Phase::Idle);
        assert!(!st.is_active());
    }

    #[test]
    fn escで待機が消えたら追従して閉じる() {
        let d = first_delay(0);
        let mut st = WhichKey::default();
        st.sync(Some(sc(Modifiers::COMMAND, Key::K)), 1.0, d);
        assert_eq!(st.phase(1.0), Phase::Shown);
        // ChordState::begin_frame が Esc で待機を捨てた → pending が None
        st.sync(None, 1.05, d);
        assert_eq!(st.phase(1.05), Phase::Idle);
    }

    #[test]
    fn backspaceは一打鍵ずつ戻る() {
        let d = first_delay(DEFAULT_FIRST_DELAY_MS);
        let mut st = WhichKey::default();
        st.sync(Some(sc(Modifiers::COMMAND, Key::K)), 0.0, d);
        st.push(sc(Modifiers::NONE, Key::A), 0.1, d);
        assert_eq!(st.path().len(), 2);
        assert!(st.pop());
        assert_eq!(st.path().len(), 1);
        assert!(st.pop());
        assert!(!st.is_active());
        // 空から更に戻ろうとしても false (呼び出し側が握りを捨てる合図)
        assert!(!st.pop());
    }

    #[test]
    fn 別のprefixを握り直したら時計もやり直す() {
        let d = first_delay(DEFAULT_FIRST_DELAY_MS);
        let mut st = WhichKey::default();
        st.sync(Some(sc(Modifiers::COMMAND, Key::K)), 0.0, d);
        assert_eq!(st.phase(0.3), Phase::Shown);
        st.sync(Some(sc(Modifiers::NONE, Key::CloseBracket)), 1.0, d);
        assert_eq!(st.phase(1.0), Phase::Hidden, "握り直しは待ちからやり直す");
        assert_eq!(st.path(), &[sc(Modifiers::NONE, Key::CloseBracket)]);
    }

    #[test]
    fn 再描画の予約は出す時刻ちょうど() {
        let d = first_delay(DEFAULT_FIRST_DELAY_MS);
        let mut st = WhichKey::default();
        st.sync(Some(sc(Modifiers::COMMAND, Key::K)), 0.0, d);
        let left = st.until_shown(0.05).expect("まだ出ていない");
        assert!((left.as_secs_f64() - 0.15).abs() < 1e-6, "{left:?}");
        // 出たあとは予約しない (アイドルのコストはゼロ)
        assert_eq!(st.phase(0.3), Phase::Shown);
        assert!(st.until_shown(0.3).is_none());
    }

    #[test]
    fn 設定の待ち時間は範囲へ丸める() {
        assert_eq!(first_delay(0), Duration::ZERO);
        assert_eq!(
            first_delay(DEFAULT_FIRST_DELAY_MS),
            Duration::from_millis(200)
        );
        assert_eq!(
            first_delay(u64::MAX),
            Duration::from_millis(MAX_FIRST_DELAY_MS)
        );
    }

    // ── prefix からの列挙 ────────────────────────────────────────────

    #[test]
    fn 実際のキーバインド表からchordを起こせる() {
        let keys = kb();
        let entries = entries_from_keybinds(&keys);
        assert!(
            entries.iter().all(|e| e.path.len() == 2),
            "いまの Binding は 2 打鍵まで"
        );
        // ⌘K の続きは ⌘S (キーバインド設定) 1 本
        let cmd_k = keys.binding(BindAction::KeybindEditor).first();
        let rows = rows_for(&entries, &[cmd_k], &[]);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].kind, RowKind::Action(BindAction::KeybindEditor));
        assert_eq!(
            rows[0].key_text(),
            keybinds::format_shortcut(
                keys.binding(BindAction::KeybindEditor)
                    .second()
                    .expect("chord")
            )
        );
    }

    #[test]
    fn 続きが無いprefixは空になる() {
        let entries = entries_from_keybinds(&kb());
        // 単打しか無いアクションの打鍵を prefix にしても 1 行も出ない
        let rows = rows_for(&entries, &[sc(Modifiers::COMMAND, Key::Q)], &[]);
        assert!(rows.is_empty(), "{rows:?}");
    }

    #[test]
    fn 続きが多いprefixは全部並ぶ() {
        let p = sc(Modifiers::COMMAND, Key::K);
        let entries: Vec<Entry> = ["A", "B", "C", "D"]
            .iter()
            .zip([Key::A, Key::B, Key::C, Key::D])
            .map(|(l, k)| entry(&[p, sc(Modifiers::NONE, k)], l, &[]))
            .collect();
        let rows = rows_for(&entries, &[p], &[]);
        assert_eq!(rows.len(), 4);
        let keys: Vec<String> = rows.iter().map(|r| r.key_text()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "打鍵で整列している");
    }

    #[test]
    fn 説明が同じ行は打鍵をまとめる() {
        let p = sc(Modifiers::COMMAND, Key::K);
        let entries = vec![
            entry(&[p, sc(Modifiers::NONE, Key::ArrowDown)], "下へ移動", &[]),
            entry(&[p, sc(Modifiers::NONE, Key::J)], "下へ移動", &[]),
            entry(&[p, sc(Modifiers::NONE, Key::K)], "上へ移動", &[]),
        ];
        let rows = rows_for(&entries, &[p], &[]);
        assert_eq!(rows.len(), 2, "{rows:?}");
        let down = rows.iter().find(|r| r.desc == "下へ移動").expect("ある");
        assert_eq!(down.keys.len(), 2);
        let mut want = vec![
            keybinds::format_shortcut(sc(Modifiers::NONE, Key::J)),
            keybinds::format_shortcut(sc(Modifiers::NONE, Key::ArrowDown)),
        ];
        want.sort();
        assert_eq!(down.keys, want, "行の中で打鍵が整列している");
        assert_eq!(down.key_text(), want.join(", "));
    }

    #[test]
    fn 塊は名前付きで最後に並ぶ() {
        let p = sc(Modifiers::COMMAND, Key::K);
        let g = sc(Modifiers::NONE, Key::F);
        let entries = vec![
            entry(&[p, sc(Modifiers::NONE, Key::Z)], "葉っぱ", &[]),
            // `groups[i]` は `path[i]` が開く塊の名前。添字 0 は prefix 自身
            // (まだ何も握っていないときの見え方)、添字 1 が `g` の開く塊。
            entry(
                &[p, g, sc(Modifiers::NONE, Key::Num1)],
                "1 段",
                &["", "折りたたみ"],
            ),
            entry(
                &[p, g, sc(Modifiers::NONE, Key::Num2)],
                "2 段",
                &["", "折りたたみ"],
            ),
        ];
        let rows = rows_for(&entries, &[p], &[]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].desc, "葉っぱ");
        assert_eq!(rows[1].kind, RowKind::Group, "塊は最後");
        assert_eq!(rows[1].desc, "+折りたたみ", "名前は定義側が持つ");
        // 1 段降りれば中身が出る
        let inner = rows_for(&entries, &[p, g], &[]);
        assert_eq!(inner.len(), 2);
        assert!(inner
            .iter()
            .all(|r| matches!(r.kind, RowKind::Action(_) | RowKind::Group)));
    }

    #[test]
    fn 塊の名前は定義が無ければ中身から導く() {
        let p = sc(Modifiers::COMMAND, Key::K);
        let g = sc(Modifiers::NONE, Key::F);
        let named = vec![
            entry(
                &[p, g, sc(Modifiers::NONE, Key::Num1)],
                "折りたたみ: 1 段",
                &[],
            ),
            entry(
                &[p, g, sc(Modifiers::NONE, Key::Num2)],
                "折りたたみ: 2 段",
                &[],
            ),
        ];
        assert_eq!(rows_for(&named, &[p], &[])[0].desc, "+折りたたみ");
        // 共通部分が無ければ件数まで落ちる (最後の手段)
        let mixed = vec![
            entry(&[p, g, sc(Modifiers::NONE, Key::Num1)], "あれ", &[]),
            entry(&[p, g, sc(Modifiers::NONE, Key::Num2)], "これ", &[]),
        ];
        assert_eq!(rows_for(&mixed, &[p], &[])[0].desc, "+2 個");
    }

    #[test]
    fn 実データの行は番号キーで並ぶ() {
        let keys = kb();
        let entries = entries_from_keybinds(&keys);
        let p = keys.binding(BindAction::DiffNextFile).first();
        let live: Vec<LiveRow> = (0..12)
            .map(|i| LiveRow {
                desc: format!("src/f{i}.rs"),
                detail: String::new(),
            })
            .collect();
        let rows = rows_for(&entries, &[p], &live);
        let live_rows: Vec<&Row> = rows
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Live(_)))
            .collect();
        assert_eq!(live_rows.len(), MAX_LIVE_ROWS, "9 行で打ち止め");
        assert_eq!(live_rows[0].key_text(), digit_label(0).unwrap());
        assert_eq!(live_rows[8].key_text(), digit_label(8).unwrap());
        assert!(digit_label(9).is_none());
        // キーバインド由来の行 (`f`) も残っている
        assert!(rows
            .iter()
            .any(|r| r.kind == RowKind::Action(BindAction::DiffNextFile)));
    }

    // ── 列割りと配置 ────────────────────────────────────────────────

    #[test]
    fn 列割りは可用幅を超えない() {
        for (w, n) in [(600.0_f32, 3_usize), (600.0, 30), (200.0, 8), (1200.0, 40)] {
            let p = pack(w, 240.0, n, 12);
            let total = p.col_w * p.cols as f32 + COL_GAP * (p.cols - 1) as f32;
            assert!(total <= w + 0.5, "w={w} n={n} total={total} {p:?}");
            assert!(p.cols >= 1 && p.rows_per_col >= 1);
            assert!(p.cols * p.rows_per_col >= n, "全部入る {p:?}");
            // 空の列を作らない
            assert!(
                (p.cols - 1) * p.rows_per_col < n,
                "最後の列が空 {p:?} n={n}"
            );
        }
    }

    #[test]
    fn 件数が一件でも空行を確保しない() {
        let p = pack(600.0, 200.0, 1, 12);
        assert_eq!(p.rows_per_col, 1, "{p:?}");
        assert_eq!(p.cols, 1);
        assert_eq!(pack(600.0, 200.0, 0, 12).rows_per_col, 0);
    }

    #[test]
    fn 高さが足りなければ列を増やして収める() {
        // 20 件・1 列に 4 行しか入らない → 5 列
        let p = pack(1200.0, 180.0, 20, 4);
        assert!(p.cols >= 5, "{p:?}");
        assert!(p.rows_per_col <= 4, "{p:?}");
    }

    /// 極端なサイズでも **全ての矩形が領域内に収まり、互いに重ならない**。
    /// CLAUDE.md「レイアウト判断は純粋関数に切り出してテーブルテストで固定する」。
    #[test]
    fn どの画面サイズでも矩形が領域内で重ならない() {
        let cases: [(f32, f32, usize); 6] = [
            (900.0, 700.0, 1),
            (900.0, 700.0, 12),
            (900.0, 700.0, 60),
            (1200.0, 300.0, 3),
            (1200.0, 300.0, 24),
            (420.0, 240.0, 9),
        ];
        for (w, h, n) in cases {
            let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h));
            let row_ws: Vec<f32> = (0..n).map(|i| 120.0 + (i % 7) as f32 * 30.0).collect();
            let lay = layout(area, 34.0, &row_ws, 20.0, 30.0, 2.0);
            assert_eq!(lay.cells.len(), n, "{w}x{h} n={n}");
            assert!(
                area.contains_rect(lay.popup),
                "{w}x{h} n={n}: カードが画面からはみ出す {:?}",
                lay.popup
            );
            assert!(
                lay.popup.right() <= area.right() - SCREEN_MARGIN + 0.5,
                "{w}x{h}: 右下の錨からずれている"
            );
            for (i, c) in lay.cells.iter().enumerate() {
                assert!(
                    lay.content.contains_rect(c.shrink(0.01)),
                    "{w}x{h} n={n}: 行 {i} が領域外 {c:?} ⊄ {:?}",
                    lay.content
                );
                for (j, d) in lay.cells.iter().enumerate().skip(i + 1) {
                    let hit = c.intersect(*d);
                    assert!(
                        hit.width() <= 0.01 || hit.height() <= 0.01,
                        "{w}x{h} n={n}: 行 {i} と {j} が重なる"
                    );
                }
            }
            // 中身が縦に収まらないときだけスクロールする
            assert_eq!(
                lay.scroll,
                lay.content.height() > lay.viewport.height() + 0.01,
                "{w}x{h} n={n}"
            );
        }
    }

    #[test]
    fn 空でも見出しぶんの高さで画面に収まる() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 300.0));
        let lay = layout(area, 34.0, &[], 20.0, 30.0, 1.0);
        assert!(lay.cells.is_empty());
        assert!(area.contains_rect(lay.popup));
    }

    // ── ポップアップが拾う打鍵 ──────────────────────────────────────

    /// 1 打鍵ぶんの `RawInput` を作って [`take_keys`] を 1 回回す。
    fn press(key: Key, mods: Modifiers, live_len: usize) -> (Option<Outcome>, usize) {
        let ctx = egui::Context::default();
        let mut got = None;
        let mut left = 0;
        let _ = ctx.run(
            egui::RawInput {
                modifiers: mods,
                events: vec![egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: mods,
                }],
                ..Default::default()
            },
            |ctx| {
                got = take_keys(ctx, live_len);
                // 消費できていれば、後続 (本文の TextEdit) には届かない
                left = ctx.input(|i| i.events.len());
            },
        );
        (got, left)
    }

    #[test]
    fn backspaceを拾って後続へ渡さない() {
        let (out, left) = press(POP_KEY, Modifiers::NONE, 0);
        assert_eq!(out, Some(Outcome::Pop));
        assert_eq!(left, 0, "消費したのにイベントが残っている");
    }

    #[test]
    fn 疑問符は配列によらず拾える() {
        for m in [Modifiers::NONE, Modifiers::SHIFT] {
            assert_eq!(press(ALL_KEY, m, 0).0, Some(Outcome::OpenAll), "{m:?}");
        }
    }

    #[test]
    fn 実データが無いときは数字を横取りしない() {
        // 実データ 0 件 → 数字はそのまま本文へ通す
        let (out, left) = press(Key::Num1, Modifiers::NONE, 0);
        assert_eq!(out, None);
        assert_eq!(left, 1, "拾わないと決めたイベントを消してはいけない");
        // 3 件あるなら 1〜3 だけ拾う
        assert_eq!(
            press(Key::Num1, Modifiers::NONE, 3).0,
            Some(Outcome::Pick(0))
        );
        assert_eq!(
            press(Key::Num3, Modifiers::NONE, 3).0,
            Some(Outcome::Pick(2))
        );
        assert_eq!(press(Key::Num4, Modifiers::NONE, 3).0, None);
    }

    #[test]
    fn 関係ない打鍵は素通りする() {
        let (out, left) = press(Key::A, Modifiers::NONE, 9);
        assert_eq!(out, None);
        assert_eq!(left, 1);
    }

    // ── 打鍵表記 ────────────────────────────────────────────────────

    #[test]
    fn 案内の打鍵表記は生成してある() {
        let hint = hint_text();
        for k in [CANCEL_KEY, POP_KEY, ALL_KEY] {
            let want = keybinds::format_shortcut(KeyboardShortcut::new(Modifiers::NONE, k));
            assert!(hint.contains(&want), "{hint} に {want} が無い");
        }
        // `Key::name()` の生の変種名 ("Questionmark") が漏れていない
        assert!(!hint.contains("Questionmark"), "{hint}");
    }

    #[test]
    fn 見出しは握っている打鍵をそのまま出す() {
        let keys = kb();
        let d = first_delay(0);
        let mut st = WhichKey::default();
        let p = keys.binding(BindAction::KeybindEditor).first();
        st.sync(Some(p), 0.0, d);
        let h = header_text(&st, 1);
        assert!(h.contains(&keybinds::format_shortcut(p)), "{h}");
    }

    /// 打鍵表記のベタ書き禁止 (CLAUDE.md)。
    /// `keybinds::tests::画面のショートカット表記をベタ書きしていない` の
    /// 対象ファイルにこのモジュールも入れてあるが、追加され忘れても
    /// ここで落ちるようにしておく。
    #[test]
    fn ソースに打鍵記号をベタ書きしていない() {
        let src = include_str!("whichkey.rs").replace("\r\n", "\n");
        for line in src.lines() {
            if line.contains("assert") || !line.contains('"') {
                continue;
            }
            let (a, b) = match (line.find('"'), line.rfind('"')) {
                (Some(a), Some(b)) if b > a => (a, b),
                _ => continue,
            };
            let lit = &line[a..=b];
            for g in ['⌘', '⌥', '⌃', '⇧'] {
                assert!(
                    !lit.contains(g),
                    "打鍵表記がベタ書きされている: {}",
                    line.trim()
                );
            }
        }
    }
}
