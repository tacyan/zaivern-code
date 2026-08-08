//! エディタの分割ビュー (split editor) — VS Code の editor group 相当。
//!
//! # 設計
//!
//! * **分割木は端末と同じものを使う。** 木そのものは
//!   [`crate::terminal::SplitLayout`] をそのまま流用する。あの木はリーフを
//!   `u64` としか思っておらず (`SessionId` は単なる型別名)、Session も PTY も
//!   egui も知らない純粋なデータ構造なので、リーフを「ペイン ID」と読み替える
//!   だけでエディタ側でも同じ意味になる。**terminal.rs は 1 行も変えていない**
//!   ので、端末側の挙動とテストには一切触れない。
//!   → 分割/結合・幾何・仕切りのドラッグ・均等化は全部あちら側の実装が担う。
//!   ここに同じ木を書き直さない。
//!   (端末側にあるズームはエディタでは公開していない — キーもコマンドも
//!   割り当てていない API を持たないため。要るようになったら
//!   `SplitLayout::zoom_focused` を 1 行で生やせる。)
//!
//! * **バッファは共有。** ペインが持つのは [`crate::editor::Buffer::id`] の
//!   並びだけで、本文の実体は `Editor::buffers` にしか無い。だから同じ
//!   ファイルを 2 つのペインで開いても実体は 1 つで、片方の編集は必ず
//!   もう片方にも出る (VS Code と同じ)。
//!
//! * **ビュー状態はペインごと。** スクロール位置とカーソルは
//!   [`EditorPane`] が持つ。本文と混ぜない。
//!
//! * **レイアウト判断は純関数。** [`pane_layout`] と [`tab_strip`] が
//!   「可用幅・件数・最長ラベル幅 → 矩形／列幅／縮退」を決め、描画側は
//!   その結果を置くだけ。極端なサイズでの不変条件はテーブルテストで固定する。

use crate::terminal::{Gutter, SplitDir, SplitLayout, SplitLayoutRec};

/// ペインの識別子。分割木のリーフに入る値。
pub type PaneId = u64;
/// バッファの識別子 (= [`crate::editor::Buffer::id`])。
pub type BufId = u64;

/// ペインの仕切り幅。端末と同じ値を使う (見た目を揃えるため)。
pub const GUTTER: f32 = crate::terminal::GUTTER;

/// タブ列の高さの既定値。実際の高さはフォント設定で変わるので、
/// 描画側は測った値を [`pane_layout`] に渡す (この定数はテストと下限用)。
pub const TAB_STRIP_H: f32 = 33.0;

/// タブ 1 枚の左右余白 (Frame の inner_margin 10 × 2) + 「×」ぶん。
pub const TAB_CHROME_W: f32 = 20.0 + 18.0;
/// これ未満の幅しか割けないなら題名を出さない (アイコンのみへ縮退)。
pub const TAB_MIN_TEXT_W: f32 = 46.0;
/// アイコンのみのタブの幅。
pub const TAB_ICON_W: f32 = 30.0;
/// ピン留めタブの幅。**アイコン + 短縮名だけ**で、「×」は置かない —
/// 誤って閉じないことがピン留めの目的なので、閉じるボタンを出す意味が無い。
pub const TAB_PIN_W: f32 = TAB_ICON_W + 24.0;

// ════════════════════════════════════════════════════════════════════
// 純関数のレイアウト
// ════════════════════════════════════════════════════════════════════

/// ペイン 1 枚の内訳。`tabs` と `body` は必ず `pane` の内側で、重ならない。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneLayout {
    /// タブ列。**タブが 0 枚なら高さ 0** — 空のセクションで高さを取らない。
    pub tabs: egui::Rect,
    /// 本文 (エディタ本体)。
    pub body: egui::Rect,
}

/// ペイン矩形をタブ列と本文に割る。
///
/// `strip_h` は測ったタブ列の高さ (フォント設定で変わる)。
///
/// 不変条件:
/// * `tabs` ∪ `body` = `pane`、`tabs` ∩ `body` = ∅
/// * タブ 0 枚 → `tabs.height() == 0`（空白を作らない）
/// * 極端に低いペインでもタブ列が本文を食い潰さない (高さの半分で頭打ち)
pub fn pane_layout(pane: egui::Rect, tab_count: usize, strip_h: f32) -> PaneLayout {
    let want = if strip_h.is_finite() && strip_h > 0.0 {
        strip_h
    } else {
        TAB_STRIP_H
    };
    let h = if tab_count == 0 {
        0.0
    } else {
        want.min((pane.height() * 0.5).max(0.0))
    };
    let split_y = pane.min.y + h;
    PaneLayout {
        tabs: egui::Rect::from_min_max(pane.min, egui::pos2(pane.max.x, split_y)),
        body: egui::Rect::from_min_max(egui::pos2(pane.min.x, split_y), pane.max),
    }
}

/// タブ題名の出し方。狭くなるほど削る。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TabLabelMode {
    /// 題名をそのまま出す。
    Full,
    /// 題名を省略して詰める。
    Truncated,
    /// アイコンだけ (題名はホバーで見せる)。
    IconOnly,
}

/// タブ列の割り付け結果。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TabStrip {
    pub mode: TabLabelMode,
    /// 通常タブ 1 枚の幅。
    pub tab_w: f32,
    /// ピン留めタブ 1 枚の幅 (常に `tab_w` 以下か、アイコン幅と同じ)。
    pub pin_w: f32,
    /// 横スクロールが要るか (アイコンのみでも収まらないとき)。
    pub scroll: bool,
}

/// **可用幅・タブ数・ピン留め枚数・最長ラベル幅 → 各タブの幅** (純関数)。
///
/// ピン留めタブは左端に固定幅 ([`TAB_PIN_W`]) で並び、残りの幅を通常タブが
/// 分け合う。縮退の順は「そのまま → 省略 → アイコンのみ → 横スクロール」。
///
/// 不変条件:
/// * `tab_w` / `pin_w` は有限で 0 以上
/// * `scroll == false` なら [`tab_total_w`] `<= avail_w` (= 見切れない)
/// * `count == 0` なら幅はどちらも 0
pub fn tab_strip_pinned(
    avail_w: f32,
    count: usize,
    pinned: usize,
    longest_label_w: f32,
) -> TabStrip {
    if count == 0 || !avail_w.is_finite() || avail_w <= 0.0 {
        return TabStrip {
            mode: TabLabelMode::IconOnly,
            tab_w: 0.0,
            pin_w: 0.0,
            scroll: false,
        };
    }
    let pinned = pinned.min(count);
    let rest = count - pinned;
    let label = if longest_label_w.is_finite() {
        longest_label_w.max(0.0)
    } else {
        0.0
    };
    let ideal = label + TAB_CHROME_W;
    let pin_take = TAB_PIN_W * pinned as f32;
    if pin_take + ideal * rest as f32 <= avail_w {
        return TabStrip {
            mode: TabLabelMode::Full,
            // 全部ピン留めなら「通常タブの幅」は使われないが、
            // 不変条件 (`tab_w * count <= avail_w`) を保つ値にしておく。
            tab_w: if rest == 0 { TAB_PIN_W } else { ideal },
            pin_w: TAB_PIN_W,
            scroll: false,
        };
    }
    let share = if rest == 0 {
        0.0
    } else {
        (avail_w - pin_take) / rest as f32
    };
    if rest > 0 && share >= TAB_MIN_TEXT_W {
        return TabStrip {
            mode: TabLabelMode::Truncated,
            tab_w: share,
            pin_w: TAB_PIN_W,
            scroll: false,
        };
    }
    // アイコンだけにしても入らない幅 → 横スクロールへ逃がす。
    TabStrip {
        mode: TabLabelMode::IconOnly,
        tab_w: TAB_ICON_W,
        pin_w: TAB_ICON_W,
        scroll: TAB_ICON_W * count as f32 > avail_w,
    }
}

/// タブ列が実際に使う横幅 (純関数)。`scroll == false` ならこれが可用幅以下。
pub fn tab_total_w(layout: TabStrip, count: usize, pinned: usize) -> f32 {
    let pinned = pinned.min(count);
    layout.pin_w * pinned as f32 + layout.tab_w * (count - pinned) as f32
}

/// **タブ列の矩形 → 1 枚ずつの矩形** (純関数)。左から
/// ピン留め ([`TabStrip::pin_w`]) → 通常 ([`TabStrip::tab_w`]) の順に詰める。
///
/// 不変条件: 返る矩形は互いに重ならず、`scroll == false` なら全部 `strip` の内側。
pub fn tab_rects(
    strip: egui::Rect,
    layout: TabStrip,
    count: usize,
    pinned: usize,
) -> Vec<egui::Rect> {
    let mut out = Vec::with_capacity(count);
    if count == 0 || strip.height() <= 0.0 || (layout.tab_w <= 0.0 && layout.pin_w <= 0.0) {
        return out;
    }
    let pinned = pinned.min(count);
    let mut x = strip.min.x;
    for i in 0..count {
        let w = if i < pinned {
            layout.pin_w
        } else {
            layout.tab_w
        }
        .max(0.0);
        out.push(egui::Rect::from_min_max(
            egui::pos2(x, strip.min.y),
            egui::pos2(x + w, strip.max.y),
        ));
        x += w;
    }
    out
}

/// ドラッグの落とし先を**ピン境界でクランプ**する (純関数)。
///
/// ピン留めタブは左の区画から出られず、通常タブは左の区画へ入れない
/// (= ピン留めが常に左端に固まっているという不変条件を、ドラッグでも壊さない)。
pub fn clamp_reorder(count: usize, pinned: usize, from: usize, to: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let pinned = pinned.min(count);
    let last = count - 1;
    if from < pinned {
        to.min(pinned.saturating_sub(1))
    } else {
        to.max(pinned).min(last)
    }
}

// ════════════════════════════════════════════════════════════════════
// MRU タブ切替 (⌃Tab)
// ════════════════════════════════════════════════════════════════════

/// ⌃Tab を**押している間**だけ生きる切替の状態。
///
/// VS Code / Zed と同じ約束: 押すたびに候補を 1 つ進め、修飾キーを
/// **離した瞬間に確定**する。押している間はどのタブもアクティブにしない
/// (画面が突然変わらない — 動くのはオーバーレイの選択枠だけ)。
#[derive(Clone, Debug, PartialEq)]
pub struct TabSwitcher {
    /// 切替の対象ペイン。別のペインへフォーカスが移ったら畳む。
    pub pane: PaneId,
    /// MRU 順の候補 (先頭 = 開き始めた時点のアクティブ)。
    pub order: Vec<BufId>,
    /// いま選んでいる位置 (`order` の index)。
    pub sel: usize,
}

impl TabSwitcher {
    /// `order` の 2 番目 (= 直前に使ったタブ) を選んだ状態で始める。
    /// **候補が 2 枚未満なら `None`** — 1 枚しか無いのに枠だけ出さない。
    pub fn start(pane: PaneId, order: Vec<BufId>, dir: i64) -> Option<Self> {
        if order.len() < 2 {
            return None;
        }
        let mut s = Self {
            pane,
            order,
            sel: 0,
        };
        s.step(dir);
        Some(s)
    }

    /// 候補を 1 つ進める / 戻す (端は巡回する)。
    pub fn step(&mut self, dir: i64) {
        let n = self.order.len();
        if n == 0 {
            return;
        }
        let d = if dir >= 0 { 1 } else { -1 };
        self.sel = (self.sel as i64 + d).rem_euclid(n as i64) as usize;
    }

    /// いま選んでいるバッファ。
    pub fn pick(&self) -> Option<BufId> {
        self.order.get(self.sel).copied()
    }

    /// 候補から消えたタブ (閉じられた) を落とす。空になったら `false` =
    /// 呼び出し側は切替を畳む。
    pub fn retain_alive(&mut self, alive: &[BufId]) -> bool {
        let cur = self.pick();
        self.order.retain(|b| alive.contains(b));
        if self.order.len() < 2 {
            return false;
        }
        self.sel = cur
            .and_then(|c| self.order.iter().position(|b| *b == c))
            .unwrap_or(0);
        true
    }
}

// ════════════════════════════════════════════════════════════════════
// モデル
// ════════════════════════════════════════════════════════════════════

/// ペイン 1 枚。タブの並びとアクティブタブ、そして**ビュー状態**を持つ。
/// 本文は持たない (バッファは `Editor::buffers` が唯一の実体)。
#[derive(Clone, Debug, PartialEq)]
pub struct EditorPane {
    pub id: PaneId,
    /// このペインに並ぶタブ (バッファ ID の列)。
    pub tabs: Vec<BufId>,
    /// `tabs` の中のアクティブ位置。`tabs` が空なら意味を持たない。
    pub active: usize,
    /// 縦スクロール位置 (ペインごと)。
    pub scroll: f32,
    /// カーソル (行, 桁) 1 始まり (ペインごと)。
    pub cursor: (usize, usize),
    /// ピン留めされたタブ。[`Self::normalize`] が `tabs` の**先頭側へ寄せる**ので、
    /// 「先頭から連続する N 枚がピン留め」という不変条件が常に成り立つ。
    pub pinned: Vec<BufId>,
    /// 最近使った順 (先頭が最新)。Ctrl+Tab の巡回順の真実源。
    /// `tabs` に居ないバッファは持たない。
    pub mru: Vec<BufId>,
    /// プレビュータブ (使い捨て・斜体)。ペインに高々 1 枚。
    /// 次のプレビューで置き換わり、確定 (編集 / ピン留め / ドラッグ / 再クリック)
    /// で `None` へ落ちる。
    pub preview: Option<BufId>,
}

impl EditorPane {
    fn new(id: PaneId) -> Self {
        Self {
            id,
            tabs: Vec::new(),
            active: 0,
            scroll: 0.0,
            cursor: (1, 1),
            pinned: Vec::new(),
            mru: Vec::new(),
            preview: None,
        }
    }

    /// アクティブなバッファ ID。
    pub fn active_buf(&self) -> Option<BufId> {
        self.tabs.get(self.active).copied()
    }

    fn activate(&mut self, buf: BufId) -> bool {
        match self.tabs.iter().position(|b| *b == buf) {
            Some(i) => {
                self.active = i;
                self.touch(buf);
                true
            }
            None => false,
        }
    }

    /// タブを 1 枚落とす。アクティブは「直後 → 直前」の順で寄せる
    /// (先頭に飛ばすと視線が吹っ飛ぶため — 端末の `heal` と同じ考え方)。
    fn remove(&mut self, buf: BufId) -> bool {
        let Some(i) = self.tabs.iter().position(|b| *b == buf) else {
            return false;
        };
        self.tabs.remove(i);
        if self.active > i || (self.active == i && self.active >= self.tabs.len()) {
            self.active = self.active.saturating_sub(1);
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        // ピン留め・MRU・プレビューからも必ず落とす
        // (消えたタブを指したままにすると Ctrl+Tab が幽霊を選ぶ)。
        self.pinned.retain(|b| *b != buf);
        self.mru.retain(|b| *b != buf);
        if self.preview == Some(buf) {
            self.preview = None;
        }
        true
    }

    // ── ピン留め / MRU / プレビュー ──────────────────────────────

    /// ピン留めされているか。
    pub fn is_pinned(&self, buf: BufId) -> bool {
        self.pinned.contains(&buf)
    }

    /// 先頭から連続するピン留めタブの枚数 (= レイアウトの「左の区画」の幅)。
    pub fn pinned_count(&self) -> usize {
        self.tabs
            .iter()
            .take_while(|b| self.pinned.contains(b))
            .count()
    }

    /// ピン留めを付け外しする。付けた時点で**確定タブへ昇格**する
    /// (使い捨てのままピン留めできると意味が矛盾するため)。
    pub fn set_pinned(&mut self, buf: BufId, on: bool) -> bool {
        if !self.tabs.contains(&buf) || self.pinned.contains(&buf) == on {
            return false;
        }
        if on {
            self.pinned.push(buf);
            if self.preview == Some(buf) {
                self.preview = None;
            }
        } else {
            self.pinned.retain(|b| *b != buf);
        }
        self.normalize();
        true
    }

    /// プレビュー枠を張り替える。ピン留め済み / 居ないタブは受け付けない。
    pub fn set_preview(&mut self, buf: Option<BufId>) {
        self.preview = buf.filter(|b| self.tabs.contains(b) && !self.pinned.contains(b));
    }

    /// プレビュータブを確定タブへ昇格させる (戻り値は昇格したか)。
    pub fn promote(&mut self, buf: BufId) -> bool {
        if self.preview == Some(buf) {
            self.preview = None;
            return true;
        }
        false
    }

    /// 「今このタブを使った」を MRU へ記録する。
    pub fn touch(&mut self, buf: BufId) {
        if !self.tabs.contains(&buf) {
            return;
        }
        self.mru.retain(|b| *b != buf);
        self.mru.insert(0, buf);
    }

    /// MRU 順のタブ列 (先頭 = いまアクティブ、その次 = 直前に使ったもの)。
    ///
    /// 記録に無いタブは並びの後ろへ回すので、**必ず `tabs` と同じ集合**を返す。
    /// Ctrl+Tab はこの並びを 1 つずつ進むだけ (= 2 回で直前のファイルへ戻る)。
    pub fn mru_order(&self) -> Vec<BufId> {
        let mut out: Vec<BufId> = self
            .mru
            .iter()
            .copied()
            .filter(|b| self.tabs.contains(b))
            .collect();
        for b in &self.tabs {
            if !out.contains(b) {
                out.push(*b);
            }
        }
        if let Some(a) = self.active_buf() {
            if let Some(i) = out.iter().position(|b| *b == a) {
                let a = out.remove(i);
                out.insert(0, a);
            }
        }
        out
    }

    /// タブの並びを整える: ピン留めを先頭へ寄せ、消えたタブを
    /// ピン留め / MRU / プレビューから落とす。アクティブは同じタブを指し続ける。
    pub fn normalize(&mut self) {
        let tabs = std::mem::take(&mut self.tabs);
        self.pinned.retain(|b| tabs.contains(b));
        self.mru.retain(|b| tabs.contains(b));
        if self.preview.is_some_and(|p| !tabs.contains(&p)) {
            self.preview = None;
        }
        if self.preview.is_some_and(|p| self.pinned.contains(&p)) {
            self.preview = None;
        }
        let cur = tabs.get(self.active).copied();
        let mut out: Vec<BufId> = Vec::with_capacity(tabs.len());
        out.extend(tabs.iter().filter(|b| self.pinned.contains(b)));
        out.extend(tabs.iter().filter(|b| !self.pinned.contains(b)));
        self.active = cur
            .and_then(|c| out.iter().position(|x| *x == c))
            .unwrap_or(0);
        self.tabs = out;
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
    }
}

/// エディタのペイン一式。分割木 + ペインの実体。
///
/// フィールドは非公開 — 「木に居ないペイン」「ペインの無い木」といった
/// 壊れた状態を作らせないため、操作は全てメソッド経由にする。
#[derive(Clone, Debug)]
pub struct EditorPanes {
    layout: SplitLayout,
    panes: Vec<EditorPane>,
    next_id: PaneId,
}

impl Default for EditorPanes {
    fn default() -> Self {
        Self::new()
    }
}

impl EditorPanes {
    /// ペイン 1 枚 (タブ無し) から始める。
    pub fn new() -> Self {
        Self {
            layout: SplitLayout::single(1),
            panes: vec![EditorPane::new(1)],
            next_id: 2,
        }
    }

    /// ペイン枚数。
    pub fn len(&self) -> usize {
        self.panes.len()
    }

    /// 分割されているか。`false` の間は**従来どおりの単一描画経路**を通す。
    pub fn is_split(&self) -> bool {
        self.len() > 1
    }

    /// フォーカス中のペイン ID。木が壊れていても必ず 1 つ返す。
    pub fn focus_id(&self) -> PaneId {
        self.layout
            .focus()
            .filter(|f| self.panes.iter().any(|p| p.id == *f))
            .unwrap_or_else(|| self.panes[0].id)
    }

    /// 左上→右下の順のペイン ID。
    pub fn order(&self) -> Vec<PaneId> {
        let leaves = self.layout.leaves();
        if leaves.len() == self.panes.len() {
            leaves
        } else {
            self.panes.iter().map(|p| p.id).collect()
        }
    }

    pub fn pane(&self, id: PaneId) -> Option<&EditorPane> {
        self.panes.iter().find(|p| p.id == id)
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut EditorPane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    fn focused(&mut self) -> &mut EditorPane {
        let f = self.focus_id();
        self.panes
            .iter_mut()
            .find(|p| p.id == f)
            .expect("フォーカス中ペイン")
    }

    /// フォーカス中ペインのアクティブバッファ。
    pub fn active_buf(&self) -> Option<BufId> {
        self.pane(self.focus_id()).and_then(|p| p.active_buf())
    }

    /// このバッファを開いているペインの数。
    pub fn open_count(&self, buf: BufId) -> usize {
        self.panes.iter().filter(|p| p.tabs.contains(&buf)).count()
    }

    // ── ピン留め / MRU / プレビュー ──────────────────────────────

    /// ピン留めを付け外しする (そのペインの中だけ)。
    pub fn set_pinned(&mut self, pane: PaneId, buf: BufId, on: bool) -> bool {
        self.pane_mut(pane)
            .map(|p| p.set_pinned(buf, on))
            .unwrap_or(false)
    }

    /// ピン留めの反転。戻り値は**反転後の状態**。
    pub fn toggle_pinned(&mut self, pane: PaneId, buf: BufId) -> bool {
        let on = !self.is_pinned(pane, buf);
        self.set_pinned(pane, buf, on);
        self.is_pinned(pane, buf)
    }

    pub fn is_pinned(&self, pane: PaneId, buf: BufId) -> bool {
        self.pane(pane).is_some_and(|p| p.is_pinned(buf))
    }

    /// どこかのペインでピン留めされているバッファ (永続化用)。
    pub fn pinned_bufs(&self) -> Vec<BufId> {
        let mut out: Vec<BufId> = Vec::new();
        for p in &self.panes {
            for b in &p.tabs {
                if p.is_pinned(*b) && !out.contains(b) {
                    out.push(*b);
                }
            }
        }
        out
    }

    /// プレビュー枠を張り替える。
    pub fn set_preview(&mut self, pane: PaneId, buf: Option<BufId>) {
        if let Some(p) = self.pane_mut(pane) {
            p.set_preview(buf);
        }
    }

    /// そのペインのプレビュータブ。
    pub fn preview_of(&self, pane: PaneId) -> Option<BufId> {
        self.pane(pane).and_then(|p| p.preview)
    }

    /// プレビュータブを確定タブへ昇格させる (全ペイン)。
    /// 同じファイルを別ペインでプレビューしていても、まとめて確定になる。
    pub fn promote(&mut self, buf: BufId) -> bool {
        let mut hit = false;
        for p in &mut self.panes {
            hit |= p.promote(buf);
        }
        hit
    }

    /// 「今このタブを使った」を記録する。
    pub fn touch(&mut self, pane: PaneId, buf: BufId) {
        if let Some(p) = self.pane_mut(pane) {
            p.touch(buf);
        }
    }

    /// そのペインの MRU 順 (先頭 = アクティブ)。
    pub fn mru_order(&self, pane: PaneId) -> Vec<BufId> {
        self.pane(pane).map(|p| p.mru_order()).unwrap_or_default()
    }

    // ── 幾何 ────────────────────────────────────────────────────

    /// 描画するペインの矩形。ズーム中はフォーカス中の 1 枚だけ。
    /// 返る矩形はすべて `area` の内側で、互いに重ならない (端末側の不変条件)。
    pub fn rects(&self, area: egui::Rect, gutter: f32) -> Vec<(PaneId, egui::Rect)> {
        self.layout.rects(area, gutter)
    }

    /// ドラッグできる仕切り一覧。
    pub fn gutters(&self, area: egui::Rect, gutter: f32) -> Vec<Gutter> {
        self.layout.gutters(area, gutter)
    }

    /// ガターのドラッグを比率へ反映する。
    pub fn drag_gutter(&mut self, path: &[bool], delta_px: f32, span_px: f32, gutter: f32) -> bool {
        self.layout.drag_gutter(path, delta_px, span_px, gutter)
    }

    /// その仕切りだけを 50:50 に戻す (ダブルクリック)。
    pub fn equalize_at(&mut self, path: &[bool]) -> bool {
        self.layout.equalize_at(path)
    }

    // ── 操作 ────────────────────────────────────────────────────

    /// フォーカス中ペインを分割する。新しいペインは**今開いているファイルを
    /// そのまま引き継ぐ** (VS Code の「エディターの分割」と同じ) ので、
    /// 分割直後に同じバッファが 2 ペインに並ぶ。戻り値は新しいペイン ID。
    pub fn split(&mut self, dir: SplitDir) -> PaneId {
        let id = self.next_id;
        self.next_id += 1;
        let inherit = self.active_buf();
        let src_id = self.focus_id();
        if !self.layout.split_focused(dir, id) {
            self.next_id -= 1;
            return self.focus_id();
        }
        let mut pane = EditorPane::new(id);
        if let Some(b) = inherit {
            pane.tabs.push(b);
            pane.mru.push(b);
        }
        // 分割元のスクロール位置とカーソルも引き継ぐ (画面が飛ばない)。
        // ピン留めも引き継ぐ — 「大事なタブ」の意味は分割で変わらない。
        if let Some(src) = self.pane(src_id) {
            pane.scroll = src.scroll;
            pane.cursor = src.cursor;
            if let Some(b) = inherit.filter(|b| src.is_pinned(*b)) {
                pane.pinned.push(b);
            }
        }
        self.panes.push(pane);
        id
    }

    /// 分割を解除して 1 枚に戻す。他のペインのタブは畳み先へ吸収する
    /// (どのペインにも居ないバッファを作らない)。
    pub fn unsplit(&mut self) -> bool {
        if !self.is_split() {
            return false;
        }
        let keep = self.focus_id();
        let mut tabs: Vec<BufId> = Vec::new();
        let mut pinned: Vec<BufId> = Vec::new();
        let mut mru: Vec<BufId> = Vec::new();
        let active = self.active_buf();
        // 畳み先を先に見る — 残る側の MRU 順とプレビュー枠をそのまま活かす。
        let mut ids = vec![keep];
        ids.extend(self.order().into_iter().filter(|id| *id != keep));
        for id in ids {
            if let Some(p) = self.pane(id) {
                for b in &p.tabs {
                    if !tabs.contains(b) {
                        tabs.push(*b);
                    }
                    if p.is_pinned(*b) && !pinned.contains(b) {
                        pinned.push(*b);
                    }
                }
                for b in p.mru_order() {
                    if !mru.contains(&b) {
                        mru.push(b);
                    }
                }
            }
        }
        let (scroll, cursor, preview) = self
            .pane(keep)
            .map(|p| (p.scroll, p.cursor, p.preview))
            .unwrap_or((0.0, (1, 1), None));
        self.panes.clear();
        let mut pane = EditorPane::new(keep);
        pane.tabs = tabs;
        pane.pinned = pinned;
        pane.mru = mru;
        pane.preview = preview;
        pane.scroll = scroll;
        pane.cursor = cursor;
        pane.normalize();
        if let Some(b) = active {
            pane.activate(b);
        }
        self.panes.push(pane);
        self.layout = SplitLayout::single(keep);
        true
    }

    /// フォーカス中ペインを閉じる。最後の 1 枚は閉じない (空にしない)。
    pub fn close_pane(&mut self, id: PaneId) -> bool {
        if self.panes.len() < 2 || !self.panes.iter().any(|p| p.id == id) {
            return false;
        }
        // 畳む前に、他のどのペインにも居ないタブを残る側へ移す。
        // ピン留めは**タブと一緒に**引っ越す (畳んだ拍子に外れない)。
        let orphans: Vec<(BufId, bool)> = self
            .pane(id)
            .map(|p| {
                p.tabs
                    .iter()
                    .copied()
                    .filter(|b| self.open_count(*b) <= 1)
                    .map(|b| (b, p.is_pinned(b)))
                    .collect()
            })
            .unwrap_or_default();
        self.layout.close_leaf(id);
        self.panes.retain(|p| p.id != id);
        let f = self.focus_id();
        if let Some(p) = self.pane_mut(f) {
            for (b, pin) in orphans {
                if !p.tabs.contains(&b) {
                    p.tabs.push(b);
                }
                if pin && !p.pinned.contains(&b) {
                    p.pinned.push(b);
                }
            }
            p.normalize();
        }
        true
    }

    /// 次のペインへフォーカスを送る (巡回)。
    pub fn focus_next(&mut self) -> bool {
        let order = self.order();
        if order.len() < 2 {
            return false;
        }
        let cur = self.focus_id();
        let at = order.iter().position(|x| *x == cur).unwrap_or(0);
        let next = order[(at + 1) % order.len()];
        self.layout.set_focus(next)
    }

    /// n 番目 (1 始まり) のペインへフォーカス。VS Code の ⌘1 / ⌘2 相当。
    pub fn focus_index(&mut self, n: usize) -> bool {
        let order = self.order();
        match n.checked_sub(1).and_then(|i| order.get(i)) {
            Some(id) => self.layout.set_focus(*id),
            None => false,
        }
    }

    /// クリックなどで直接フォーカスを移す。
    pub fn set_focus(&mut self, id: PaneId) -> bool {
        self.layout.set_focus(id)
    }

    /// フォーカス中ペインの**アクティブタブを次のペインへ移す**。
    /// ペインが 1 枚しか無いときは「右へ分割して移す」。
    /// 戻り値は移せたか。
    pub fn move_active_tab_to_next(&mut self) -> bool {
        let Some(buf) = self.active_buf() else {
            return false;
        };
        let src = self.focus_id();
        let pinned = self.is_pinned(src, buf);
        if !self.is_split() {
            // 右へ分割 → 新ペインは分割元のタブを引き継いでいるので、
            // 元のペインから外せば「移動」になる。
            self.split(SplitDir::Horizontal);
        } else {
            self.focus_next();
            let to = self.focus_id();
            if src == to {
                return false;
            }
            if let Some(p) = self.pane_mut(to) {
                if !p.tabs.contains(&buf) {
                    p.tabs.push(buf);
                }
                if pinned && !p.pinned.contains(&buf) {
                    p.pinned.push(buf);
                }
                p.normalize();
                p.activate(buf);
            }
        }
        // 送り元から外す。1 枚も残らなければペインごと畳む。
        let mut emptied = false;
        if let Some(p) = self.pane_mut(src) {
            p.remove(buf);
            emptied = p.tabs.is_empty();
        }
        if emptied {
            self.close_pane(src);
        }
        true
    }

    /// タブ 1 枚をペインから外す。**バッファ自体は消さない** —
    /// 呼び出し側が「他のペインにも居るか」で消すかを決める。
    /// 最後の 1 枚を外したペインは (2 枚以上あれば) 畳む。
    pub fn close_tab(&mut self, pane: PaneId, buf: BufId) -> bool {
        let Some(p) = self.pane_mut(pane) else {
            return false;
        };
        if !p.remove(buf) {
            return false;
        }
        if p.tabs.is_empty() && self.panes.len() > 1 {
            self.close_pane(pane);
        }
        true
    }

    // ── 同期 ────────────────────────────────────────────────────

    /// `Editor` の実体 (バッファ列とアクティブ) とペインを突き合わせる。
    ///
    /// 1. 消えたバッファをタブから落とす。空になったペインは畳む
    ///    (**空ペインを放置しない**)。
    /// 2. 単一ペインのときはタブ列 = バッファ列そのもの
    ///    (= 分割していない間は今までと 1 枚も違わない)。
    ///    分割中はどのペインにも居ないバッファをフォーカス中ペインへ足す。
    /// 3. 外から `editor.active` が動いていたら、フォーカス中ペインで開く
    ///    (VS Code と同じ「アクティブなグループに開く」)。
    ///
    /// 戻り値はフォーカス中ペインのアクティブバッファ ID
    /// (呼び出し側が `editor.active` を合わせ直すために使う)。
    pub fn sync(&mut self, buffer_ids: &[BufId], editor_active: Option<BufId>) -> Option<BufId> {
        // 1. 死んだバッファを落とす
        for p in &mut self.panes {
            let before = p.tabs.len();
            let keep: Vec<BufId> = p
                .tabs
                .iter()
                .copied()
                .filter(|b| buffer_ids.contains(b))
                .collect();
            if keep.len() != before {
                let active_buf = p.active_buf();
                let fallback = p.active.min(keep.len().saturating_sub(1));
                let next = active_buf
                    .and_then(|b| keep.iter().position(|x| *x == b))
                    .unwrap_or(fallback);
                p.tabs = keep;
                p.active = next;
                // 消えたバッファをピン留め / MRU / プレビューからも落とす。
                p.normalize();
            }
        }
        // 空ペインを畳む (最後の 1 枚は残す)
        loop {
            let empty = self
                .panes
                .iter()
                .find(|p| p.tabs.is_empty())
                .map(|p| p.id)
                .filter(|_| self.panes.len() > 1);
            match empty {
                Some(id) => {
                    self.close_pane(id);
                }
                None => break,
            }
        }

        // 2. 取りこぼしを埋める
        if !self.is_split() {
            let p = &mut self.panes[0];
            let active_buf = p.active_buf();
            let next = active_buf
                .and_then(|b| buffer_ids.iter().position(|x| *x == b))
                .unwrap_or(0);
            p.tabs = buffer_ids.to_vec();
            p.active = next;
            // ピン留めを左端へ寄せ直す。呼び出し側 (`app::sync_panes`) は
            // この並びを `editor.buffers` へ写し戻すので、**画面の並びと
            // バッファ列は必ず一致する** (ドラッグの添字がずれない)。
            p.normalize();
        } else {
            let missing: Vec<BufId> = buffer_ids
                .iter()
                .copied()
                .filter(|b| self.open_count(*b) == 0)
                .collect();
            if !missing.is_empty() {
                let f = self.focused();
                for b in missing {
                    f.tabs.push(b);
                }
                f.normalize();
            }
        }

        // 3. 外からのアクティブ変更を取り込む
        if let Some(b) = editor_active.filter(|b| buffer_ids.contains(b)) {
            let f = self.focused();
            if !f.activate(b) {
                f.tabs.push(b);
                f.active = f.tabs.len() - 1;
                f.normalize();
                f.activate(b);
            }
        }
        // アクティブは必ず MRU の先頭 (Ctrl+Tab の起点)。
        let f = self.focus_id();
        if let Some(b) = self.pane(f).and_then(|p| p.active_buf()) {
            self.touch(f, b);
        }
        self.active_buf()
    }

    // ── 永続化 ──────────────────────────────────────────────────

    /// 保存用の形へ落とす。`path_of` はバッファ ID → **絶対パス文字列**
    /// (無題タブなど引けないものは `None`)。
    ///
    /// **バッファ ID は保存しない** — 再起動で必ず変わるため、リーフは
    /// 端末側が生ログのパスで指しているのと同じ流儀で、ファイルの絶対パスで指す。
    /// パスを引けるタブが 1 枚も無いペインは落とし、残りが 1 枚以下なら
    /// 空 (= 保存する分割は無い) を返す。
    pub fn to_rec(&self, path_of: &mut dyn FnMut(BufId) -> Option<String>) -> EditorPanesRec {
        if !self.is_split() {
            return EditorPanesRec::default();
        }
        let mut recs: Vec<PaneRec> = Vec::new();
        let mut keys: Vec<(PaneId, String)> = Vec::new();
        for p in &self.panes {
            let mut paths: Vec<String> = Vec::new();
            let mut active = 0usize;
            for (i, b) in p.tabs.iter().enumerate() {
                let Some(s) = path_of(*b) else { continue };
                if i == p.active {
                    active = paths.len();
                }
                paths.push(s);
            }
            if paths.is_empty() {
                continue;
            }
            let key = format!("p{}", recs.len());
            keys.push((p.id, key.clone()));
            recs.push(PaneRec { key, active, paths });
        }
        if recs.len() < 2 {
            return EditorPanesRec::default();
        }
        let split = self.layout.to_rec(&mut |id| {
            keys.iter()
                .find(|(i, _)| *i == id)
                .map(|(_, k)| k.to_string())
        });
        // 木から落とされたペインの記録は残さない (穴あきの記録を書かない)。
        let alive: Vec<&str> = split
            .nodes
            .iter()
            .filter_map(|t| t.strip_prefix("L:"))
            .collect();
        recs.retain(|r| alive.contains(&r.key.as_str()));
        if recs.len() < 2 {
            return EditorPanesRec::default();
        }
        EditorPanesRec { split, panes: recs }
    }
}

// ════════════════════════════════════════════════════════════════════
// 永続化 (保存形式)
// ════════════════════════════════════════════════════════════════════

/// 保存形式のバージョン。未知の値を読んだら**分割なしで開き直す**
/// (前方互換のために panic も部分解釈もしない)。
const REC_VERSION: &str = "1";

/// 最上位の区切り (ASCII Group Separator)。端末側が使っている
/// FS(`\u{1f}`) / RS(`\u{1e}`) の 1 つ外側の階層として使う。
const REC_GS: char = '\u{1d}';
/// ペイン記録のフィールド区切り (端末と同じ ASCII Unit Separator)。
const REC_FS: char = '\u{1f}';
/// ペイン記録のパス列の区切り (端末と同じ ASCII Record Separator)。
const REC_RS: char = '\u{1e}';

/// ペイン 1 枚の保存記録。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaneRec {
    /// 分割木のリーフが指す安定キー (`"p0"`, `"p1"`, …)。
    pub key: String,
    /// アクティブタブの位置 (`paths` の index)。範囲外なら復元時に丸める。
    pub active: usize,
    /// このペインに並んでいたファイルの**絶対パス**。
    pub paths: Vec<String>,
}

impl PaneRec {
    fn to_field(&self) -> String {
        format!(
            "{}{REC_FS}{}{REC_FS}{}",
            self.key,
            self.active,
            self.paths.join(&REC_RS.to_string())
        )
    }

    /// 壊れていれば `None` (その 1 枚だけ落として残りは活かす)。
    fn from_field(s: &str) -> Option<Self> {
        let mut it = s.splitn(3, REC_FS);
        let (key, active, paths) = (it.next()?, it.next()?, it.next()?);
        if key.is_empty() || paths.is_empty() {
            return None;
        }
        Some(Self {
            key: key.to_string(),
            // 数字でなければ先頭タブ扱い (壊れた記録でも開けるようにする)。
            active: active.parse().unwrap_or(0),
            paths: paths
                .split(REC_RS)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect(),
        })
    }
}

/// エディタ分割レイアウトの保存記録。
///
/// 分割木そのものは端末と**同じ** [`SplitLayoutRec`] を使う
/// (木の形と比率の書式・healing・不正比率のクランプを二重に書かない)。
/// ここが足すのは「どのリーフがどのファイルを並べていたか」だけ。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditorPanesRec {
    /// 分割木。リーフは [`PaneRec::key`] を指す。
    pub split: SplitLayoutRec,
    /// ペインの中身。
    pub panes: Vec<PaneRec>,
}

impl EditorPanesRec {
    pub fn is_empty(&self) -> bool {
        self.split.is_empty() || self.panes.is_empty()
    }

    /// 1 行の文字列へ潰す。保存側 (`session.rs`) に**プレーンな `String` を
    /// 1 本足すだけ**で済ませるための形 — 端末分割と同じ流儀。
    pub fn to_line(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::from(REC_VERSION);
        out.push(REC_GS);
        out.push_str(&self.split.to_line());
        for p in &self.panes {
            out.push(REC_GS);
            out.push_str(&p.to_field());
        }
        out
    }

    /// [`Self::to_line`] の逆。**壊れた記録・未知のバージョンでも panic しない** —
    /// 読めなければ空 (= 分割なし) を返す。
    pub fn from_line(line: &str) -> Self {
        let mut it = line.split(REC_GS);
        let (Some(ver), Some(split)) = (it.next(), it.next()) else {
            return Self::default();
        };
        if ver != REC_VERSION {
            return Self::default();
        }
        let panes: Vec<PaneRec> = it.filter_map(PaneRec::from_field).collect();
        Self {
            split: SplitLayoutRec::from_line(split),
            panes,
        }
    }

    /// 実行時の形へ戻す純関数。`buf_of` は絶対パス → バッファ ID
    /// (開けない = 存在しなくなったファイルは `None`)。
    ///
    /// * 引けないパスは**黙って飛ばす**。
    /// * タブが 1 枚も残らないペインは畳む (空ペインを残さない)。
    /// * 残るペインが 1 枚以下なら `None` — 呼び出し側は 1 ペインのまま開く。
    /// * 比率が負 / NaN / inf でも [`SplitLayoutRec::to_layout`] が丸めるので panic しない。
    pub fn to_panes(&self, buf_of: &mut dyn FnMut(&str) -> Option<BufId>) -> Option<EditorPanes> {
        if self.is_empty() {
            return None;
        }
        let mut live: Vec<(String, EditorPane)> = Vec::new();
        let mut next_id: PaneId = 1;
        for r in &self.panes {
            if live.iter().any(|(k, _)| *k == r.key) {
                continue; // 同じキーが 2 回出てくる記録は先勝ち
            }
            let mut tabs: Vec<BufId> = Vec::new();
            let mut active = 0usize;
            for (i, p) in r.paths.iter().enumerate() {
                let Some(b) = buf_of(p) else { continue };
                if tabs.contains(&b) {
                    continue; // 同じペインに同じバッファは 1 枚だけ
                }
                if i == r.active {
                    active = tabs.len();
                }
                tabs.push(b);
            }
            if tabs.is_empty() {
                continue;
            }
            let mut pane = EditorPane::new(next_id);
            next_id += 1;
            pane.active = active.min(tabs.len() - 1);
            // 再起動直後の Ctrl+Tab は「保存されていた並び」を辿る
            // (MRU は実行時の記録なので保存しない — 嘘の履歴を作らない)。
            pane.mru = tabs.clone();
            pane.tabs = tabs;
            live.push((r.key.clone(), pane));
        }
        if live.len() < 2 {
            return None;
        }
        let layout = self
            .split
            .to_layout(&mut |k| live.iter().find(|(key, _)| key == k).map(|(_, p)| p.id));
        let leaves = layout.leaves();
        if leaves.len() < 2 {
            return None;
        }
        let panes: Vec<EditorPane> = live
            .into_iter()
            .map(|(_, p)| p)
            .filter(|p| leaves.contains(&p.id))
            .collect();
        if panes.len() < 2 {
            return None;
        }
        Some(EditorPanes {
            layout,
            panes,
            next_id,
        })
    }
}

// ════════════════════════════════════════════════════════════════════
// 描画 (⌃Tab のオーバーレイ)
// ════════════════════════════════════════════════════════════════════

/// ⌃Tab を押している間だけ出す候補一覧。
///
/// * **画面中央の 1 枚のカード**だけ — レイアウトは 1px も動かさない
///   (「画面が突然変わらない」の原則)。
/// * 幅は画面幅から導くので、どの幅でも見切れない。長い題名は省略して
///   ホバーで全文を出す。
/// * `items` は (アイコン + 題名, 補足) の並び。`sel` が選択位置。
pub fn tab_switcher_overlay(
    ctx: &egui::Context,
    theme: &crate::theme::Theme,
    title: &str,
    items: &[(String, String)],
    sel: usize,
) {
    if items.is_empty() {
        return;
    }
    let screen = ctx.screen_rect();
    let card_w = (screen.width() * 0.6)
        .min((screen.width() - 32.0).max(0.0))
        .clamp(0.0, 560.0);
    if card_w <= 0.0 {
        return;
    }
    egui::Area::new(egui::Id::new("zv-tab-switcher"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::none()
                .fill(theme.panel)
                .stroke(egui::Stroke::new(1.0_f32, theme.border))
                .rounding(8.0)
                .inner_margin(egui::Margin::symmetric(12.0, 10.0))
                .show(ui, |ui| {
                    ui.set_width(card_w);
                    ui.add(
                        egui::Label::new(egui::RichText::new(title).color(theme.text_dim).small())
                            .selectable(false),
                    );
                    ui.add_space(4.0);
                    for (i, (name, hint)) in items.iter().enumerate() {
                        let on = i == sel;
                        let fill = if on {
                            theme.accent_soft
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        egui::Frame::none()
                            .fill(fill)
                            .rounding(5.0)
                            .inner_margin(egui::Margin::symmetric(8.0, 4.0))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    let c = if on { theme.text } else { theme.text_dim };
                                    ui.add(
                                        egui::Label::new(egui::RichText::new(name).color(c))
                                            .selectable(false)
                                            .truncate(),
                                    )
                                    .on_hover_text(name);
                                    if !hint.is_empty() {
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(hint)
                                                    .color(theme.text_dim)
                                                    .small(),
                                            )
                                            .selectable(false)
                                            .truncate(),
                                        );
                                    }
                                });
                            });
                    }
                });
        });
}

// ════════════════════════════════════════════════════════════════════
// テスト
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, Rect};

    fn area(w: f32, h: f32) -> Rect {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(w, h))
    }

    /// 2 つの矩形が (面積を持って) 重なるか。
    fn overlaps(a: Rect, b: Rect) -> bool {
        let x = a.min.x.max(b.min.x) < a.max.x.min(b.max.x) - 0.001;
        let y = a.min.y.max(b.min.y) < a.max.y.min(b.max.y) - 0.001;
        x && y
    }

    fn inside(inner: Rect, outer: Rect) -> bool {
        inner.min.x >= outer.min.x - 0.01
            && inner.min.y >= outer.min.y - 0.01
            && inner.max.x <= outer.max.x + 0.01
            && inner.max.y <= outer.max.y + 0.01
    }

    /// 検証に使う極端なサイズ一覧。
    /// 320 = 携帯並みの狭さ、1200×300 = 横長、900×700 = 通常。
    const SIZES: &[(f32, f32)] = &[
        (320.0, 240.0),
        (320.0, 700.0),
        (400.0, 700.0),
        (900.0, 700.0),
        (1200.0, 300.0),
        (1920.0, 1080.0),
        (60.0, 40.0),
        (1.0, 1.0),
        (0.0, 0.0),
    ];

    // ── 純関数: ペイン内訳 ────────────────────────────────────

    #[test]
    fn ペイン内訳はタブ0枚で高さを取らない() {
        for (w, h) in SIZES {
            let p = area(*w, *h);
            let l = pane_layout(p, 0, TAB_STRIP_H);
            assert_eq!(l.tabs.height(), 0.0, "{w}x{h}: 空タブ列が高さを取った");
            assert_eq!(l.body, p, "{w}x{h}: 本文がペイン全体でない");
        }
    }

    #[test]
    fn ペイン内訳はどのサイズでも収まり重ならない() {
        for (w, h) in SIZES {
            for n in [0usize, 1, 3, 12, 60] {
                for strip in [0.0f32, 12.0, TAB_STRIP_H, 400.0, f32::NAN] {
                    let p = area(*w, *h);
                    let l = pane_layout(p, n, strip);
                    assert!(
                        inside(l.tabs, p),
                        "{w}x{h} n={n} strip={strip}: タブ列がはみ出した"
                    );
                    assert!(
                        inside(l.body, p),
                        "{w}x{h} n={n} strip={strip}: 本文がはみ出した"
                    );
                    assert!(
                        !overlaps(l.tabs, l.body),
                        "{w}x{h} n={n} strip={strip}: 重なった"
                    );
                }
                let p = area(*w, *h);
                let l = pane_layout(p, n, TAB_STRIP_H);
                assert!(inside(l.tabs, p), "{w}x{h} n={n}: タブ列がはみ出した");
                assert!(inside(l.body, p), "{w}x{h} n={n}: 本文がはみ出した");
                assert!(
                    !overlaps(l.tabs, l.body),
                    "{w}x{h} n={n}: タブ列と本文が重なった"
                );
                // 隙間なく埋める
                assert!(
                    (l.tabs.height() + l.body.height() - p.height()).abs() < 0.01,
                    "{w}x{h} n={n}: 隙間か食い込みがある"
                );
                // 本文が消えない (タブ列は高さの半分まで)
                if n > 0 && p.height() > 0.0 {
                    assert!(
                        l.body.height() >= p.height() * 0.5 - 0.01,
                        "{w}x{h}: 本文が潰れた"
                    );
                }
            }
        }
    }

    // ── 純関数: タブ列 ────────────────────────────────────────

    #[test]
    fn タブ列は狭くなるほど縮退する() {
        // (可用幅, 件数, 最長ラベル幅) -> 期待モード
        let cases: &[(f32, usize, f32, TabLabelMode)] = &[
            (900.0, 1, 80.0, TabLabelMode::Full),
            (900.0, 3, 80.0, TabLabelMode::Full),
            (900.0, 12, 80.0, TabLabelMode::Truncated),
            (320.0, 3, 200.0, TabLabelMode::Truncated),
            (320.0, 8, 80.0, TabLabelMode::IconOnly),
            (120.0, 8, 80.0, TabLabelMode::IconOnly),
            (0.0, 5, 80.0, TabLabelMode::IconOnly),
            (900.0, 0, 80.0, TabLabelMode::IconOnly),
        ];
        for (w, n, longest, want) in cases {
            let got = tab_strip_pinned(*w, *n, 0, *longest);
            assert_eq!(got.mode, *want, "avail={w} n={n} longest={longest}");
        }
    }

    #[test]
    fn タブ列はスクロールしない限り可用幅に収まる() {
        for (w, _) in SIZES {
            for n in [0usize, 1, 2, 5, 20, 200] {
                for longest in [0.0f32, 12.0, 90.0, 400.0] {
                    let s = tab_strip_pinned(*w, n, 0, longest);
                    assert!(s.tab_w.is_finite() && s.tab_w >= 0.0, "幅が壊れた: {s:?}");
                    if n == 0 {
                        assert_eq!(s.tab_w, 0.0);
                        continue;
                    }
                    if !s.scroll {
                        assert!(
                            s.tab_w * n as f32 <= *w + 0.01,
                            "avail={w} n={n} longest={longest}: 見切れる ({s:?})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn タブ矩形はタブ列に収まり重ならない() {
        for (w, h) in SIZES {
            for n in [0usize, 1, 4, 30] {
                let pane = area(*w, *h);
                let l = pane_layout(pane, n, TAB_STRIP_H);
                let s = tab_strip_pinned(l.tabs.width(), n, 0, 90.0);
                let rects = tab_rects(l.tabs, s, n, 0);
                for (i, r) in rects.iter().enumerate() {
                    // スクロールするときは可用幅を超えてよい (ScrollArea の中)
                    if !s.scroll {
                        assert!(
                            inside(*r, l.tabs),
                            "{w}x{h} n={n} #{i}: タブがタブ列からはみ出した"
                        );
                    }
                    assert!(inside(
                        Rect::from_min_max(pos2(r.min.x, r.min.y), pos2(r.min.x, r.max.y)),
                        Rect::from_min_max(
                            pos2(l.tabs.min.x, l.tabs.min.y),
                            pos2(f32::INFINITY, l.tabs.max.y)
                        )
                    ));
                    for (j, o) in rects.iter().enumerate() {
                        if i != j {
                            assert!(
                                !overlaps(*r, *o),
                                "{w}x{h} n={n}: タブ {i} と {j} が重なった"
                            );
                        }
                    }
                }
            }
        }
    }

    // ── 純関数: ピン留めを含むタブ列 ──────────────────────────

    /// **可用幅・タブ数・ピン留め枚数・最長ラベル幅 → 各タブの矩形**を
    /// 極端なサイズで総当たりし、「全部が可用領域に収まり、互いに重ならない」
    /// を固定する。見切れを許すのは `scroll == true` (= ScrollArea の中) だけ。
    #[test]
    fn ピン留め込みのタブ矩形はどの幅でも収まり重ならない() {
        for (w, h) in SIZES {
            for n in [0usize, 1, 2, 5, 12, 40] {
                for pinned in [0usize, 1, 3, 40] {
                    for longest in [0.0f32, 40.0, 120.0, 400.0] {
                        let pinned = pinned.min(n);
                        let pane = area(*w, *h);
                        let l = pane_layout(pane, n, TAB_STRIP_H);
                        let s = tab_strip_pinned(l.tabs.width(), n, pinned, longest);
                        let rects = tab_rects(l.tabs, s, n, pinned);
                        let ctx = format!("{w}x{h} n={n} pin={pinned} longest={longest}");
                        assert!(s.tab_w.is_finite() && s.tab_w >= 0.0, "{ctx}: 幅が壊れた");
                        assert!(
                            s.pin_w.is_finite() && s.pin_w >= 0.0,
                            "{ctx}: ピン幅が壊れた"
                        );
                        if n == 0 {
                            assert!(rects.is_empty(), "{ctx}: タブ 0 枚で矩形が出た");
                            continue;
                        }
                        if l.tabs.height() <= 0.0 {
                            continue;
                        }
                        assert_eq!(rects.len(), n, "{ctx}: 矩形の数が合わない");
                        if !s.scroll {
                            assert!(
                                tab_total_w(s, n, pinned) <= l.tabs.width() + 0.01,
                                "{ctx}: 合計幅が可用幅を超えた ({s:?})"
                            );
                        }
                        for (i, r) in rects.iter().enumerate() {
                            if !s.scroll {
                                assert!(inside(*r, l.tabs), "{ctx} #{i}: タブがはみ出した");
                            }
                            assert!(r.width() >= 0.0, "{ctx} #{i}: 幅が負");
                            for (j, o) in rects.iter().enumerate() {
                                if i != j {
                                    assert!(!overlaps(*r, *o), "{ctx}: タブ {i} と {j} が重なった");
                                }
                            }
                        }
                        // ピン留めは必ず左端から、幅は**固定**
                        // (題名の長さで動かない = 左端の区画がぶれない)。
                        for (i, r) in rects.iter().take(pinned).enumerate() {
                            assert!(
                                (r.width() - s.pin_w).abs() < 0.01,
                                "{ctx} #{i}: ピン留めの幅が固定でない"
                            );
                            assert!(
                                r.min.x <= rects[pinned.min(n - 1)].min.x + 0.01,
                                "{ctx} #{i}: ピン留めが通常タブより右にある"
                            );
                        }
                    }
                }
            }
        }
    }

    /// ピン留めだけのタブ列 / 幅が足りないケースの縮退。
    #[test]
    fn 全部ピン留めでも幅が足りなければ縮退してから横スクロールへ逃げる() {
        // (可用幅, 件数=ピン留め件数) -> 期待
        let wide = tab_strip_pinned(900.0, 4, 4, 300.0);
        assert_eq!(wide.mode, TabLabelMode::Full);
        assert_eq!(wide.pin_w, TAB_PIN_W);
        assert!(!wide.scroll, "広ければスクロールしない");

        // ピン留め 8 枚 = 432px。300px には入らないのでアイコンのみへ
        let narrow = tab_strip_pinned(300.0, 8, 8, 300.0);
        assert_eq!(narrow.mode, TabLabelMode::IconOnly);
        assert!(!narrow.scroll, "アイコンなら 240px で 300px に入る");
        assert!(tab_total_w(narrow, 8, 8) <= 300.0);

        // アイコンでも入らない幅 → 横スクロール
        let tiny = tab_strip_pinned(100.0, 8, 8, 300.0);
        assert!(tiny.scroll, "アイコンでも入らないならスクロールへ逃がす");
    }

    /// ピン留めが左を占めると、通常タブの取り分だけが縮む。
    #[test]
    fn ピン留めは固定幅で残りを通常タブが分け合う() {
        let s = tab_strip_pinned(600.0, 6, 2, 400.0);
        assert_eq!(s.pin_w, TAB_PIN_W);
        assert_eq!(s.mode, TabLabelMode::Truncated);
        let want = (600.0 - TAB_PIN_W * 2.0) / 4.0;
        assert!((s.tab_w - want).abs() < 0.01, "{s:?}");
        let rects = tab_rects(area(600.0, 30.0), s, 6, 2);
        assert!((rects[0].width() - TAB_PIN_W).abs() < 0.01);
        assert!((rects[1].width() - TAB_PIN_W).abs() < 0.01);
        assert!((rects[2].width() - want).abs() < 0.01);
        // 左端から隙間なく並ぶ
        assert!((rects[0].min.x - 0.0).abs() < 0.01);
        for i in 1..rects.len() {
            assert!(
                (rects[i].min.x - rects[i - 1].max.x).abs() < 0.01,
                "隙間 {i}"
            );
        }
    }

    /// ドラッグの落とし先はピン境界を越えない。
    #[test]
    fn ドラッグの落とし先はピン境界でクランプされる() {
        // (件数, ピン留め, from, to) -> 期待
        let cases: &[(usize, usize, usize, usize, usize)] = &[
            // ピン留めタブは左の区画から出られない
            (5, 2, 0, 4, 1),
            (5, 2, 1, 3, 1),
            (5, 2, 0, 1, 1),
            // 通常タブは左の区画へ入れない
            (5, 2, 4, 0, 2),
            (5, 2, 3, 1, 2),
            (5, 2, 3, 4, 4),
            // ピン留めが無ければ素通し
            (5, 0, 0, 4, 4),
            (5, 0, 4, 0, 0),
            // 全部ピン留め
            (3, 3, 2, 0, 0),
            // 端
            (0, 0, 0, 0, 0),
            (1, 1, 0, 9, 0),
        ];
        for (count, pinned, from, to, want) in cases {
            assert_eq!(
                clamp_reorder(*count, *pinned, *from, *to),
                *want,
                "count={count} pinned={pinned} from={from} to={to}"
            );
        }
    }

    // ── ピン留め / MRU / プレビュー ────────────────────────────

    #[test]
    fn ピン留めはタブ列の先頭へ寄りアクティブを見失わない() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11, 12, 13], Some(13));
        let pane = p.focus_id();
        assert!(p.set_pinned(pane, 12, true));
        assert_eq!(p.pane(pane).unwrap().tabs, vec![12, 10, 11, 13]);
        assert_eq!(p.pane(pane).unwrap().pinned_count(), 1);
        // 掴んでいたタブ (アクティブ) は同じものを指し続ける
        assert_eq!(p.active_buf(), Some(13));
        assert!(p.set_pinned(pane, 11, true));
        assert_eq!(p.pane(pane).unwrap().tabs, vec![12, 11, 10, 13]);
        assert_eq!(p.pane(pane).unwrap().pinned_count(), 2);
        assert_eq!(p.pinned_bufs(), vec![12, 11]);
        // 解除で通常タブ側へ戻る
        assert!(p.set_pinned(pane, 12, false));
        assert_eq!(p.pane(pane).unwrap().pinned_count(), 1);
        assert_eq!(p.pane(pane).unwrap().tabs[0], 11);
        // 同じ状態への設定は何も起きない
        assert!(!p.set_pinned(pane, 11, true));
        // 居ないタブは無視
        assert!(!p.set_pinned(pane, 999, true));
    }

    #[test]
    fn ピン留めタブは同期でも並び替えでも左端に残る() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11, 12], Some(10));
        let pane = p.focus_id();
        p.set_pinned(pane, 12, true);
        // 新しいバッファが増えても、ピン留めは先頭のまま
        p.sync(&[10, 11, 12, 13, 14], Some(14));
        assert_eq!(p.pane(pane).unwrap().tabs, vec![12, 10, 11, 13, 14]);
        assert_eq!(p.pane(pane).unwrap().pinned_count(), 1);
    }

    #[test]
    fn ctrl_tab_を2回押すと直前のファイルへ戻る() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11, 12], Some(10));
        let pane = p.focus_id();
        // 10 → 11 → 12 の順に使った
        p.pane_mut(pane).unwrap().activate(11);
        p.pane_mut(pane).unwrap().activate(12);
        let order = p.mru_order(pane);
        assert_eq!(order, vec![12, 11, 10], "MRU の先頭はアクティブ");

        // 1 回目: 直前のファイル (11) が選ばれる
        let mut sw = TabSwitcher::start(pane, order.clone(), 1).expect("2 枚以上ある");
        assert_eq!(sw.pick(), Some(11));
        // 離して確定 → 11 がアクティブ
        p.pane_mut(pane).unwrap().activate(sw.pick().unwrap());
        assert_eq!(p.active_buf(), Some(11));

        // もう一度 ⌃Tab 2 回で 12 → 元の 12 へ戻れる (2 つのファイルを行き来できる)
        let order = p.mru_order(pane);
        assert_eq!(order, vec![11, 12, 10]);
        sw = TabSwitcher::start(pane, order, 1).expect("2 枚以上ある");
        assert_eq!(sw.pick(), Some(12));
        p.pane_mut(pane).unwrap().activate(12);
        assert_eq!(p.active_buf(), Some(12));
    }

    #[test]
    fn ctrl_shift_tab_は逆順で回り候補が1枚なら開かない() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11, 12], Some(10));
        let pane = p.focus_id();
        p.pane_mut(pane).unwrap().activate(11);
        p.pane_mut(pane).unwrap().activate(12);
        let order = p.mru_order(pane); // [12, 11, 10]
        let sw = TabSwitcher::start(pane, order.clone(), -1).expect("2 枚以上");
        assert_eq!(sw.pick(), Some(10), "逆順は末尾から");

        // 巡回する
        let mut sw = TabSwitcher::start(pane, order, 1).expect("2 枚以上");
        for want in [11, 10, 12, 11] {
            assert_eq!(sw.pick(), Some(want));
            sw.step(1);
        }

        // 候補 1 枚 / 0 枚では開かない (枠だけ出さない)
        assert!(TabSwitcher::start(pane, vec![10], 1).is_none());
        assert!(TabSwitcher::start(pane, Vec::new(), 1).is_none());
    }

    #[test]
    fn プレビュータブを閉じた直後もmruは整合する() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11, 12], Some(10));
        let pane = p.focus_id();
        p.pane_mut(pane).unwrap().activate(11);
        p.pane_mut(pane).unwrap().activate(12);
        p.set_preview(pane, Some(12));
        assert_eq!(p.preview_of(pane), Some(12));

        // プレビュータブが閉じられた (= バッファが消えた)
        p.close_tab(pane, 12);
        p.sync(&[10, 11], Some(11));
        assert_eq!(p.preview_of(pane), None, "消えたタブがプレビューに残った");
        let order = p.mru_order(pane);
        assert_eq!(order, vec![11, 10], "消えたタブが MRU に残った");
        assert!(
            !order.contains(&12),
            "閉じたタブを ⌃Tab が選べてしまう ({order:?})"
        );
        // 切替の候補からも落ちる
        let mut sw = TabSwitcher::start(pane, vec![12, 11, 10], 1).expect("2 枚以上");
        assert!(sw.retain_alive(&[10, 11]));
        assert_eq!(sw.order, vec![11, 10]);
        // 1 枚以下になったら畳む
        assert!(!sw.retain_alive(&[10]));
    }

    #[test]
    fn ペインを閉じるとそのmruも消えピン留めは残る() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11], Some(10));
        let first = p.focus_id();
        let second = p.split(SplitDir::Horizontal);
        p.set_focus(second);
        p.sync(&[10, 11, 12], Some(12));
        p.set_pinned(second, 12, true);
        assert!(p.is_pinned(second, 12));

        // 2 枚目を閉じる → 孤児のタブは残る側へ、ピン留めも一緒に引っ越す
        assert!(p.close_pane(second));
        assert!(p.pane(second).is_none(), "閉じたペインが残っている");
        let keep = p.focus_id();
        assert_eq!(keep, first);
        assert!(p.pane(keep).unwrap().tabs.contains(&12));
        assert!(p.is_pinned(keep, 12), "畳んだ拍子にピン留めが外れた");
        assert_eq!(p.pane(keep).unwrap().tabs[0], 12, "ピン留めが左端に無い");
        // 消えたペインの MRU を引きずらない
        for b in p.mru_order(keep) {
            assert!(p.pane(keep).unwrap().tabs.contains(&b));
        }
    }

    #[test]
    fn ピン留めとプレビューは分割をまたいでも壊れない() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11], Some(11));
        let first = p.focus_id();
        p.set_pinned(first, 11, true);
        // 分割 → 新ペインもピン留めを引き継ぐ
        let second = p.split(SplitDir::Vertical);
        assert!(p.is_pinned(second, 11), "分割でピン留てが落ちた");
        // タブを次のペインへ移してもピン留めは付いてくる
        p.set_focus(first);
        p.sync(&[10, 11], Some(11));
        assert!(p.move_active_tab_to_next());
        let holder = p.order().into_iter().find(|id| {
            p.pane(*id)
                .map(|q| q.tabs.contains(&11) && q.is_pinned(11))
                .unwrap_or(false)
        });
        assert!(holder.is_some(), "移動でピン留めが外れた");
        // 分割解除でもピン留めは残り、左端へ寄る
        p.unsplit();
        let one = p.focus_id();
        assert!(p.is_pinned(one, 11));
        assert_eq!(p.pane(one).unwrap().tabs[0], 11);
    }

    #[test]
    fn ピン留めするとプレビューは確定タブになる() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11], Some(11));
        let pane = p.focus_id();
        p.set_preview(pane, Some(11));
        assert_eq!(p.preview_of(pane), Some(11));
        p.set_pinned(pane, 11, true);
        assert_eq!(p.preview_of(pane), None, "ピン留めしたのに使い捨てのまま");
        // ピン留め済みのタブはプレビュー枠に入れない
        p.set_preview(pane, Some(11));
        assert_eq!(p.preview_of(pane), None);
        // 昇格はどのペインからでも効く
        p.set_preview(pane, Some(10));
        assert!(p.promote(10));
        assert_eq!(p.preview_of(pane), None);
        assert!(!p.promote(10), "2 回目は何も起きない");
    }

    #[test]
    fn タブ0枚とピン留めのみの端でも壊れない() {
        // タブ 0 枚
        let s = tab_strip_pinned(900.0, 0, 0, 100.0);
        assert_eq!(s.tab_w, 0.0);
        assert_eq!(s.pin_w, 0.0);
        assert!(tab_rects(area(900.0, 30.0), s, 0, 0).is_empty());
        assert_eq!(tab_total_w(s, 0, 0), 0.0);
        // ピン留め枚数がタブ数を超えても飽和する
        let s = tab_strip_pinned(900.0, 2, 9, 100.0);
        assert!(tab_total_w(s, 2, 9) <= 900.0);
        assert_eq!(tab_rects(area(900.0, 30.0), s, 2, 9).len(), 2);
        // 空のペインでもピン留め API は嘘をつかない
        let mut p = EditorPanes::new();
        let pane = p.focus_id();
        assert!(!p.set_pinned(pane, 10, true));
        assert!(!p.is_pinned(pane, 10));
        assert!(p.pinned_bufs().is_empty());
        assert!(p.mru_order(pane).is_empty());
        assert_eq!(p.preview_of(pane), None);
    }

    // ── ペインの矩形 ──────────────────────────────────────────

    /// 分割の形を作る (右→下→右 の入れ子)。
    fn nested() -> EditorPanes {
        let mut p = EditorPanes::new();
        p.pane_mut(1).unwrap().tabs = vec![10];
        p.split(SplitDir::Horizontal);
        p.split(SplitDir::Vertical);
        p.split(SplitDir::Horizontal);
        p
    }

    #[test]
    fn ペイン矩形はどのサイズでも領域に収まり重ならない() {
        for (w, h) in SIZES {
            for panes in [EditorPanes::new(), nested()] {
                let a = area(*w, *h);
                let rects = panes.rects(a, GUTTER);
                assert!(!rects.is_empty(), "{w}x{h}: ペインが 0 枚になった");
                for (i, (_, r)) in rects.iter().enumerate() {
                    assert!(inside(*r, a), "{w}x{h}: ペイン {i} がはみ出した {r:?}");
                    for (j, (_, o)) in rects.iter().enumerate() {
                        if i != j {
                            assert!(!overlaps(*r, *o), "{w}x{h}: ペイン {i} と {j} が重なった");
                        }
                    }
                }
                // ペインごとの内訳まで含めて領域内に収まる
                for (_, r) in &rects {
                    let l = pane_layout(*r, 3, TAB_STRIP_H);
                    assert!(
                        inside(l.tabs, a) && inside(l.body, a),
                        "{w}x{h}: 内訳がはみ出した"
                    );
                }
            }
        }
    }

    #[test]
    fn 仕切りは領域内にありペインと重ならない() {
        for (w, h) in [(900.0f32, 700.0f32), (1200.0, 300.0), (320.0, 240.0)] {
            let panes = nested();
            let a = area(w, h);
            let rects = panes.rects(a, GUTTER);
            for g in panes.gutters(a, GUTTER) {
                assert!(inside(g.rect, a), "{w}x{h}: 仕切りがはみ出した");
                for (_, r) in &rects {
                    assert!(!overlaps(g.rect, *r), "{w}x{h}: 仕切りがペインと重なった");
                }
            }
        }
    }

    // ── モデルの振る舞い ──────────────────────────────────────

    #[test]
    fn 分割すると同じバッファが2ペインに並ぶ() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11], Some(11));
        assert!(!p.is_split());
        let new_id = p.split(SplitDir::Horizontal);
        assert!(p.is_split());
        assert_eq!(p.focus_id(), new_id, "分割後は新しいペインへフォーカス");
        assert_eq!(p.active_buf(), Some(11), "分割先は同じファイルを開く");
        assert_eq!(p.open_count(11), 2, "同じバッファが 2 ペインに居る");
        // 元のペインは元のタブ列を保ったまま
        let old = p.order()[0];
        assert_eq!(p.pane(old).unwrap().tabs, vec![10, 11]);
    }

    #[test]
    fn 上下分割と入れ子ができる() {
        let mut p = EditorPanes::new();
        p.sync(&[10], Some(10));
        p.split(SplitDir::Vertical);
        p.split(SplitDir::Horizontal);
        assert_eq!(p.len(), 3);
        let a = area(1200.0, 300.0);
        assert_eq!(p.rects(a, GUTTER).len(), 3);
        assert_eq!(p.gutters(a, GUTTER).len(), 2, "入れ子ぶんの仕切りが要る");
    }

    #[test]
    fn 最後のタブを閉じるとペインが畳まれる() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11], Some(10));
        p.split(SplitDir::Horizontal); // 新ペインは 10 を開く
        assert_eq!(p.len(), 2);
        let f = p.focus_id();
        assert_eq!(p.pane(f).unwrap().tabs, vec![10]);
        p.close_tab(f, 10);
        assert_eq!(p.len(), 1, "空になったペインが残った");
        assert!(!p.is_split());
    }

    #[test]
    fn 空ペインは同期でも畳まれる() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11], Some(10));
        p.split(SplitDir::Horizontal);
        assert_eq!(p.len(), 2);
        // 10 番のバッファが消えた → 新ペインは空になる
        let left = p.sync(&[11], Some(11));
        assert_eq!(p.len(), 1, "空ペインが放置された");
        assert_eq!(left, Some(11));
    }

    #[test]
    fn 単一ペインのタブ列は常にバッファ列と一致する() {
        let mut p = EditorPanes::new();
        p.sync(&[1, 2, 3], Some(2));
        assert_eq!(p.pane(p.focus_id()).unwrap().tabs, vec![1, 2, 3]);
        assert_eq!(p.active_buf(), Some(2));
        // 途中のバッファが閉じられても並びは維持される
        p.sync(&[1, 3], Some(3));
        assert_eq!(p.pane(p.focus_id()).unwrap().tabs, vec![1, 3]);
        assert_eq!(p.active_buf(), Some(3));
    }

    #[test]
    fn 分割中もどのペインにも居ないバッファは作らない() {
        let mut p = EditorPanes::new();
        p.sync(&[10], Some(10));
        p.split(SplitDir::Horizontal);
        // 新しく開かれたバッファ 20 はどこにも居ない → フォーカス中へ入る
        p.sync(&[10, 20], Some(20));
        assert!(p.open_count(20) >= 1, "開いたのに見えないタブができた");
        assert_eq!(p.active_buf(), Some(20));
    }

    #[test]
    fn フォーカス移動は巡回し番号でも指せる() {
        let mut p = EditorPanes::new();
        p.sync(&[10], Some(10));
        p.split(SplitDir::Horizontal);
        p.split(SplitDir::Vertical);
        let order = p.order();
        assert_eq!(order.len(), 3);
        p.focus_index(1);
        assert_eq!(p.focus_id(), order[0]);
        p.focus_next();
        assert_eq!(p.focus_id(), order[1]);
        p.focus_next();
        assert_eq!(p.focus_id(), order[2]);
        p.focus_next();
        assert_eq!(p.focus_id(), order[0], "巡回していない");
        assert!(p.focus_index(3));
        assert_eq!(p.focus_id(), order[2]);
        assert!(!p.focus_index(9), "居ないペインを指せてしまった");
        assert!(!p.focus_index(0), "0 は無効");
    }

    #[test]
    fn タブを次のペインへ移すと分割元から消える() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11], Some(11));
        // 1 枚のときは「右へ分割して移す」
        assert!(p.move_active_tab_to_next());
        assert_eq!(p.len(), 2);
        assert_eq!(p.active_buf(), Some(11));
        assert_eq!(p.open_count(11), 1, "移動なのに 2 箇所に残った");
        let src = p.order()[0];
        assert_eq!(
            p.pane(src).unwrap().tabs,
            vec![10],
            "送り元から消えていない"
        );
    }

    #[test]
    fn 移動で空になった送り元は畳まれる() {
        let mut p = EditorPanes::new();
        p.sync(&[10], Some(10));
        assert!(p.move_active_tab_to_next());
        // 送り元は 10 しか持っていなかった → 畳まれて 1 枚に戻る
        assert_eq!(p.len(), 1, "空の送り元が残った");
        assert_eq!(p.active_buf(), Some(10));
    }

    #[test]
    fn 分割解除で全タブが一枚に吸収される() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11], Some(10));
        p.split(SplitDir::Horizontal);
        p.sync(&[10, 11, 12], Some(12));
        assert!(p.unsplit());
        assert!(!p.is_split());
        let tabs = p.pane(p.focus_id()).unwrap().tabs.clone();
        for b in [10, 11, 12] {
            assert!(tabs.contains(&b), "分割解除で {b} が消えた");
        }
        assert_eq!(p.active_buf(), Some(12));
        assert!(!p.unsplit(), "1 枚のときは何も起きない");
    }

    #[test]
    fn ペインを閉じても孤児のタブは残る() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11], Some(10));
        p.split(SplitDir::Horizontal);
        // 新ペインだけで 12 を開いている状態を作る
        let f = p.focus_id();
        p.pane_mut(f).unwrap().tabs = vec![12];
        p.pane_mut(f).unwrap().active = 0;
        p.close_pane(f);
        assert_eq!(p.len(), 1);
        let tabs = &p.pane(p.focus_id()).unwrap().tabs;
        assert!(tabs.contains(&12), "唯一そのペインが持っていたタブが消えた");
    }

    #[test]
    fn ビュー状態はペインごとに分かれる() {
        let mut p = EditorPanes::new();
        p.sync(&[10], Some(10));
        let a = p.focus_id();
        p.pane_mut(a).unwrap().scroll = 120.0;
        p.pane_mut(a).unwrap().cursor = (42, 7);
        let b = p.split(SplitDir::Horizontal);
        // 分割直後は引き継ぐ (画面が飛ばない)
        assert_eq!(p.pane(b).unwrap().scroll, 120.0);
        // その後は独立して動く
        p.pane_mut(b).unwrap().scroll = 0.0;
        p.pane_mut(b).unwrap().cursor = (1, 1);
        assert_eq!(p.pane(a).unwrap().scroll, 120.0, "片方の操作が他方へ漏れた");
        assert_eq!(p.pane(a).unwrap().cursor, (42, 7));
    }

    #[test]
    fn 仕切りのドラッグと均等化が効く() {
        let mut p = EditorPanes::new();
        p.sync(&[10], Some(10));
        p.split(SplitDir::Horizontal);
        let a = area(900.0, 700.0);
        let g = p.gutters(a, GUTTER);
        assert_eq!(g.len(), 1);
        let before = p.rects(a, GUTTER)[0].1.width();
        assert!(p.drag_gutter(&g[0].path, 120.0, g[0].span.width(), GUTTER));
        let after = p.rects(a, GUTTER)[0].1.width();
        assert!(after > before, "ドラッグで広がらなかった");
        assert!(p.equalize_at(&g[0].path));
        let back = p.rects(a, GUTTER)[0].1.width();
        assert!((back - before).abs() < 2.0, "均等化で戻らなかった");
    }

    /// **完了条件**: 同じファイルを 2 ペインで開き、片方で編集したとき
    /// もう片方にも必ず反映されること (= バッファは単一の実体)。
    #[test]
    fn 同じファイルを2ペインで開くと片方の編集がもう片方に出る() {
        let dir = crate::test_util::unique_temp_dir("zv", "split-share");
        std::fs::create_dir_all(&dir).expect("一時ディレクトリ");
        let file = dir.join("共有.txt");
        std::fs::write(&file, "はじめの本文").expect("書き込み");

        let hl = crate::highlight::Highlighter::new();
        let mut ed = crate::editor::Editor::new();
        ed.open(&file, &hl).expect("開ける");

        let mut panes = EditorPanes::new();
        let ids: Vec<BufId> = ed.buffers.iter().map(|b| b.id).collect();
        panes.sync(&ids, ed.active.map(|i| ed.buffers[i].id));

        let left = panes.focus_id();
        let right = panes.split(SplitDir::Horizontal);
        assert_ne!(left, right, "分割できていない");

        // 2 ペインが同じバッファ ID を指している = 実体は 1 つ
        let lb = panes.pane(left).unwrap().active_buf().expect("左のタブ");
        let rb = panes.pane(right).unwrap().active_buf().expect("右のタブ");
        assert_eq!(lb, rb, "分割先が別のバッファを開いてしまった");
        assert_eq!(ed.buffers.len(), 1, "バッファが複製された");

        // 左のペイン経由で編集する
        let i = ed
            .buffers
            .iter()
            .position(|b| b.id == lb)
            .expect("左の実体");
        ed.buffers[i].text.push_str("＋追記");

        // 右のペインから読むと同じ本文が見える
        let j = ed
            .buffers
            .iter()
            .position(|b| b.id == rb)
            .expect("右の実体");
        assert_eq!(i, j, "実体が 2 つに分かれている");
        assert_eq!(
            ed.buffers[j].text, "はじめの本文＋追記",
            "編集が伝わっていない"
        );

        // 片方のペインを畳んでもバッファは生き残る
        panes.close_tab(right, rb);
        assert_eq!(panes.len(), 1);
        assert_eq!(ed.buffers.len(), 1, "ペインを畳んだらバッファまで消えた");
        assert_eq!(ed.buffers[0].text, "はじめの本文＋追記");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 空のエディタでもペインは1枚残る() {
        let mut p = EditorPanes::new();
        let left = p.sync(&[], None);
        assert_eq!(left, None);
        assert_eq!(p.len(), 1);
        assert!(!p.is_split());
        // ペインが 0 枚にならないので focus_id は必ず引ける
        assert!(p.pane(p.focus_id()).is_some());
    }

    // ── 永続化 ────────────────────────────────────────────────

    /// テスト用のパス解決: `paths` に載っているものだけ「存在する」。
    /// index+1 をバッファ ID にする (0 を避ける)。
    fn resolver<'a>(paths: &'a [&'a str]) -> impl FnMut(&str) -> Option<BufId> + 'a {
        move |p: &str| paths.iter().position(|x| *x == p).map(|i| i as BufId + 1)
    }

    /// 保存 → 文字列 → 復元 を通す。
    fn roundtrip(panes: &EditorPanes, of: &[(BufId, &str)], alive: &[&str]) -> Option<EditorPanes> {
        let line = panes
            .to_rec(&mut |b| {
                of.iter()
                    .find(|(id, _)| *id == b)
                    .map(|(_, p)| (*p).to_string())
            })
            .to_line();
        EditorPanesRec::from_line(&line).to_panes(&mut resolver(alive))
    }

    #[test]
    fn 分割していなければ保存する分割は無い() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11], Some(10));
        let rec = p.to_rec(&mut |b| Some(format!("/w/{b}.rs")));
        assert!(rec.is_empty(), "1 ペインなら記録しない");
        assert_eq!(rec.to_line(), "");
    }

    #[test]
    fn 分割と比率とタブがそのまま戻る() {
        let mut p = EditorPanes::new();
        p.sync(&[10, 11], Some(10));
        p.split(SplitDir::Horizontal);
        // 仕切りを動かして 50:50 から外す
        p.drag_gutter(&[], 100.0, 1000.0, GUTTER);
        let ratio_before = p.rects(area(1000.0, 600.0), GUTTER)[0].1.width();

        let of = [(10u64, "/w/a.rs"), (11u64, "/w/b.rs")];
        let back = roundtrip(&p, &of, &["/w/a.rs", "/w/b.rs"]).expect("復元できる");
        assert_eq!(back.len(), 2);
        let ratio_after = back.rects(area(1000.0, 600.0), GUTTER)[0].1.width();
        assert!(
            (ratio_before - ratio_after).abs() < 0.01,
            "比率が戻っていない: {ratio_before} → {ratio_after}"
        );
        // 左ペインは 2 枚、分割で生えた右ペインは引き継いだ 1 枚
        let tabs: Vec<usize> = back
            .order()
            .iter()
            .map(|id| back.pane(*id).unwrap().tabs.len())
            .collect();
        assert_eq!(tabs, vec![2, 1]);
    }

    #[test]
    fn 入れ子の分割も戻る() {
        let mut p = EditorPanes::new();
        p.sync(&[10], Some(10));
        p.split(SplitDir::Vertical);
        p.split(SplitDir::Horizontal);
        assert_eq!(p.len(), 3);
        let of = [(10u64, "/w/a.rs")];
        let back = roundtrip(&p, &of, &["/w/a.rs"]).expect("復元できる");
        assert_eq!(back.len(), 3, "入れ子の 3 ペインが戻る");
        // どの矩形も領域内で重ならない (端末側の不変条件を引き継いでいる)
        let rects = back.rects(area(1200.0, 300.0), GUTTER);
        assert_eq!(rects.len(), 3);
        for (i, (_, r)) in rects.iter().enumerate() {
            assert!(inside(*r, area(1200.0, 300.0)), "ペイン {i} が領域外");
            for (j, (_, o)) in rects.iter().enumerate() {
                assert!(i == j || !overlaps(*r, *o), "ペイン {i} と {j} が重なった");
            }
        }
    }

    #[test]
    fn 復元のテーブル() {
        // (名前, 保存元のタブ配置, 復元時に存在するパス, 期待するペイン枚数)
        // 枚数 0 = 分割を復元しない (1 ペインのまま開く)
        let cases: [(&str, [&[&str]; 2], &[&str], usize); 6] = [
            (
                "両方そろっている",
                [&["/w/a.rs"], &["/w/b.rs"]],
                &["/w/a.rs", "/w/b.rs"],
                2,
            ),
            (
                "片方だけ存在 → 1 枚しか残らないので畳む",
                [&["/w/a.rs"], &["/w/b.rs"]],
                &["/w/a.rs"],
                0,
            ),
            (
                "存在しないパスだけ → 畳む",
                [&["/w/a.rs"], &["/w/b.rs"]],
                &[],
                0,
            ),
            (
                "片ペインの一部だけ消えても残りで復元する",
                [&["/w/a.rs", "/w/gone.rs"], &["/w/b.rs"]],
                &["/w/a.rs", "/w/b.rs"],
                2,
            ),
            (
                "無関係なパスしか無い → 畳む",
                [&["/w/a.rs"], &["/w/b.rs"]],
                &["/w/z.rs"],
                0,
            ),
            (
                "同じファイルを 2 ペインで開いていた",
                [&["/w/a.rs"], &["/w/a.rs"]],
                &["/w/a.rs"],
                2,
            ),
        ];
        for (name, tabs, alive, want) in cases {
            // 保存元を組む: バッファ ID は 100 番台の連番で振る
            let mut of: Vec<(BufId, &str)> = Vec::new();
            let mut id = 100u64;
            let mut p = EditorPanes::new();
            p.split(SplitDir::Horizontal);
            let order = p.order();
            for (pi, list) in tabs.iter().enumerate() {
                let pane = p.pane_mut(order[pi]).unwrap();
                pane.tabs.clear();
                for path in list.iter() {
                    let bid = match of.iter().find(|(_, x)| x == path) {
                        Some((b, _)) => *b,
                        None => {
                            id += 1;
                            of.push((id, path));
                            id
                        }
                    };
                    pane.tabs.push(bid);
                }
                pane.active = 0;
            }
            let got = roundtrip(&p, &of, alive);
            match want {
                0 => assert!(got.is_none(), "{name}: 分割を復元しないはず"),
                n => assert_eq!(got.map(|g| g.len()), Some(n), "{name}"),
            }
        }
    }

    #[test]
    fn 壊れた記録でも例外を出さず1ペインで開く() {
        let fs = '\u{1f}';
        let rs = '\u{1e}';
        let gs = '\u{1d}';
        let bad: Vec<String> = vec![
            String::new(),
            "ごみ".into(),
            // 未知のバージョン
            format!("9{gs}0{fs}p0{fs}L:p0{rs}L:p1{gs}p0{fs}0{fs}/w/a.rs"),
            // 木だけあって中身が無い
            format!("1{gs}0{fs}{fs}L:p0"),
            // ノードが足りない (H の子が 1 つしかない)
            format!("1{gs}0{fs}p0{fs}H:0.5{rs}L:p0{gs}p0{fs}0{fs}/w/a.rs"),
            // 比率が NaN / inf / 負
            format!("1{gs}0{fs}p0{fs}H:NaN{rs}L:p0{rs}L:p1{gs}p0{fs}0{fs}/w/a.rs{gs}p1{fs}0{fs}/w/b.rs"),
            format!("1{gs}0{fs}p0{fs}H:inf{rs}L:p0{rs}L:p1{gs}p0{fs}0{fs}/w/a.rs{gs}p1{fs}0{fs}/w/b.rs"),
            format!("1{gs}0{fs}p0{fs}V:-9{rs}L:p0{rs}L:p1{gs}p0{fs}0{fs}/w/a.rs{gs}p1{fs}0{fs}/w/b.rs"),
            // active が範囲外 / 数字ですらない
            format!("1{gs}0{fs}p0{fs}H:0.5{rs}L:p0{rs}L:p1{gs}p0{fs}999{fs}/w/a.rs{gs}p1{fs}x{fs}/w/b.rs"),
            // 同じキーが 2 回
            format!("1{gs}0{fs}p0{fs}H:0.5{rs}L:p0{rs}L:p1{gs}p0{fs}0{fs}/w/a.rs{gs}p0{fs}0{fs}/w/b.rs"),
        ];
        for line in bad {
            let rec = EditorPanesRec::from_line(&line);
            let got = rec.to_panes(&mut resolver(&["/w/a.rs", "/w/b.rs"]));
            // panic しないこと自体が主張。復元できた場合も必ず 2 枚以上。
            if let Some(g) = &got {
                assert!(g.len() >= 2, "1 枚以下の分割を作った: {line:?}");
                // 比率が壊れていても矩形は領域内に収まる
                for (_, r) in g.rects(area(900.0, 700.0), GUTTER) {
                    assert!(inside(r, area(900.0, 700.0)), "領域外: {line:?}");
                }
            }
        }
    }

    #[test]
    fn アクティブタブの位置も戻る() {
        let mut p = EditorPanes::new();
        p.split(SplitDir::Horizontal);
        let order = p.order();
        p.pane_mut(order[0]).unwrap().tabs = vec![1, 2, 3];
        p.pane_mut(order[0]).unwrap().active = 2;
        p.pane_mut(order[1]).unwrap().tabs = vec![2];
        p.pane_mut(order[1]).unwrap().active = 0;
        let of = [(1u64, "/w/a.rs"), (2u64, "/w/b.rs"), (3u64, "/w/c.rs")];
        let back = roundtrip(&p, &of, &["/w/a.rs", "/w/b.rs", "/w/c.rs"]).expect("復元");
        let o = back.order();
        let left = back.pane(o[0]).unwrap();
        assert_eq!(left.tabs.len(), 3);
        assert_eq!(left.active, 2, "アクティブは 3 枚目のまま");
    }

    #[test]
    fn 消えたファイルがアクティブでもはみ出さない() {
        let mut p = EditorPanes::new();
        p.split(SplitDir::Horizontal);
        let order = p.order();
        p.pane_mut(order[0]).unwrap().tabs = vec![1, 2];
        p.pane_mut(order[0]).unwrap().active = 1; // 消える方がアクティブ
        p.pane_mut(order[1]).unwrap().tabs = vec![3];
        let of = [(1u64, "/w/a.rs"), (2u64, "/w/gone.rs"), (3u64, "/w/c.rs")];
        let back = roundtrip(&p, &of, &["/w/a.rs", "/w/c.rs"]).expect("復元");
        let left = back.pane(back.order()[0]).unwrap();
        assert_eq!(left.tabs.len(), 1);
        assert!(left.active < left.tabs.len(), "active が範囲外");
    }
}
