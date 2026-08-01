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

use crate::terminal::{Gutter, SplitDir, SplitLayout};

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
    /// タブ 1 枚の幅。
    pub tab_w: f32,
    /// 横スクロールが要るか (アイコンのみでも収まらないとき)。
    pub scroll: bool,
}

/// **可用幅・タブ数・最長ラベル幅 → 1 枚の幅と縮退の度合い** (純関数)。
///
/// 不変条件:
/// * `tab_w >= 0` かつ有限
/// * `scroll == false` なら `tab_w * count <= avail_w` (= 見切れない)
/// * `count == 0` なら `tab_w == 0`
pub fn tab_strip(avail_w: f32, count: usize, longest_label_w: f32) -> TabStrip {
    if count == 0 || !avail_w.is_finite() || avail_w <= 0.0 {
        return TabStrip {
            mode: TabLabelMode::IconOnly,
            tab_w: 0.0,
            scroll: false,
        };
    }
    let n = count as f32;
    let label = if longest_label_w.is_finite() {
        longest_label_w.max(0.0)
    } else {
        0.0
    };
    let ideal = label + TAB_CHROME_W;
    if ideal * n <= avail_w {
        return TabStrip {
            mode: TabLabelMode::Full,
            tab_w: ideal,
            scroll: false,
        };
    }
    let share = avail_w / n;
    if share >= TAB_MIN_TEXT_W {
        return TabStrip {
            mode: TabLabelMode::Truncated,
            tab_w: share,
            scroll: false,
        };
    }
    // アイコンだけにしても入らない幅 → 横スクロールへ逃がす。
    if TAB_ICON_W * n <= avail_w {
        TabStrip {
            mode: TabLabelMode::IconOnly,
            tab_w: TAB_ICON_W,
            scroll: false,
        }
    } else {
        TabStrip {
            mode: TabLabelMode::IconOnly,
            tab_w: TAB_ICON_W,
            scroll: true,
        }
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
}

impl EditorPane {
    fn new(id: PaneId) -> Self {
        Self {
            id,
            tabs: Vec::new(),
            active: 0,
            scroll: 0.0,
            cursor: (1, 1),
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
        true
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
        }
        // 分割元のスクロール位置とカーソルも引き継ぐ (画面が飛ばない)。
        if let Some(src) = self.pane(src_id) {
            pane.scroll = src.scroll;
            pane.cursor = src.cursor;
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
        let active = self.active_buf();
        for id in self.order() {
            if let Some(p) = self.pane(id) {
                for b in &p.tabs {
                    if !tabs.contains(b) {
                        tabs.push(*b);
                    }
                }
            }
        }
        let (scroll, cursor) = self
            .pane(keep)
            .map(|p| (p.scroll, p.cursor))
            .unwrap_or((0.0, (1, 1)));
        self.panes.clear();
        let mut pane = EditorPane::new(keep);
        pane.tabs = tabs;
        pane.scroll = scroll;
        pane.cursor = cursor;
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
        let orphans: Vec<BufId> = self
            .pane(id)
            .map(|p| {
                p.tabs
                    .iter()
                    .copied()
                    .filter(|b| self.open_count(*b) <= 1)
                    .collect()
            })
            .unwrap_or_default();
        self.layout.close_leaf(id);
        self.panes.retain(|p| p.id != id);
        let f = self.focus_id();
        if let Some(p) = self.pane_mut(f) {
            for b in orphans {
                if !p.tabs.contains(&b) {
                    p.tabs.push(b);
                }
            }
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
            }
        }

        // 3. 外からのアクティブ変更を取り込む
        if let Some(b) = editor_active.filter(|b| buffer_ids.contains(b)) {
            let f = self.focused();
            if !f.activate(b) {
                f.tabs.push(b);
                f.active = f.tabs.len() - 1;
            }
        }
        self.active_buf()
    }
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

    /// タブ列の中にタブを詰める (テスト用の参照実装)。
    /// `scroll` のときは可用幅を超えるぶんも返す — 実際は `ScrollArea` の
    /// 中に置かれるので見切れない。
    fn tab_rects(strip: Rect, layout: TabStrip, count: usize) -> Vec<Rect> {
        let mut out = Vec::with_capacity(count);
        if count == 0 || layout.tab_w <= 0.0 || strip.height() <= 0.0 {
            return out;
        }
        for i in 0..count {
            let x0 = strip.min.x + layout.tab_w * i as f32;
            out.push(Rect::from_min_max(
                pos2(x0, strip.min.y),
                pos2(x0 + layout.tab_w, strip.max.y),
            ));
        }
        out
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
            let got = tab_strip(*w, *n, *longest);
            assert_eq!(got.mode, *want, "avail={w} n={n} longest={longest}");
        }
    }

    #[test]
    fn タブ列はスクロールしない限り可用幅に収まる() {
        for (w, _) in SIZES {
            for n in [0usize, 1, 2, 5, 20, 200] {
                for longest in [0.0f32, 12.0, 90.0, 400.0] {
                    let s = tab_strip(*w, n, longest);
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
                let s = tab_strip(l.tabs.width(), n, 90.0);
                let rects = tab_rects(l.tabs, s, n);
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
}
