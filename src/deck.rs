//! エージェントデッキ — 左に細いレール、右にその端末だけ。cmux と同じ静かな見え方。
//!
//! ## レールに並ぶのは「いま動いているエージェント」だけ
//! デッキは **ランチャ + 端末** であって、ダッシュボードでも一覧画面でもない。
//! - 「誰がどの状態か」を持つのは**フリート看板 (kanban.rs)** の役目。
//! - 「過去の会話を再開する」を持つのは**サイドバーのセッションタブ
//!   (session_picker.rs)** の役目。
//!
//! どちらもデッキでは二重に持たない (実際に看板と被っていた)。だからレールの
//! 行は稼働中セッションと 1 対 1 で、状態は一切描かない: 区画見出し・
//! フィルタチップ・稼働/承認の件数・経過時間・出力スパークライン・承認バッジ —
//! すべて持たない。起動プリセットも行にしない (上端の ＋ メニューだけ)。
//!
//! ## 一覧の 1 行
//! [`RowView`] がそのまま「画面に出る 1 行」で、**タイトル 1 行 + 副題 1 行**しかない。
//!
//! ```text
//! ┌───────────────┬──────────────────────────────────────────────┐
//! │ ＋        ▤   │ zaivern-code · ~/dev/zaivern    ▤ ⊟ ＋ － ✕  │
//! │ zaivern-code  ├──────────────────────────────────────────────┤
//! │ main • ~/dev… │                                              │
//! │ kindle2pdf    │            端末 (端から端まで)                │
//! │ master • ~/d… │                                              │
//! └───────────────┴──────────────────────────────────────────────┘
//! ```
//!
//! 選択行は accent の**べた塗り**。文字色は [`on_accent`] が明度差で選ぶ。
//!
//! ## 絞り込みは「見えない」
//! 絞り込み欄は描かない。**そのまま文字を打てば絞り込み**、Backspace で 1 文字消し、
//! Esc で全消し ([`type_into_filter`])。絞り込みが効いている間だけ、
//! レール下端に細いピルでその語を出す (それ以外は何も出さない)。
//! そのため 1 打鍵のライフサイクル操作は **⌥ (Alt) 付き**に置いた
//! ([`key_intent`]): ⌥N 新規 / ⌥R 名前変更 / ⌥D 複製 / ⌥X 停止 (2 回) /
//! ⌥S 再起動 / ⌥↑⌥↓ 並べ替え。素の ↑↓ で選択、Enter で端末へ。
//!
//! ## 寸法
//! 固定ピクセルを書かない。レール幅は窓幅の割合 ([`rail_width`]) で、
//! 下限・上限は**本文 1 行の高さ**を単位に決める (DPI・フォント設定に追従する)。
//!
//! ## 負荷 (アイドルで 1 枚も描かない)
//! - PTY 画面の読み直しは [`crate::fleet::FleetStore::sample_due`] が決める。
//! - 再描画要求は [`deck_repaint_ms`] が決める。**誰も出力していなければ
//!   `None` = 1 枚も予約しない** — app.rs の `schedule_idle_repaint` に判断を返す。
//!   ここが `Some` に固定されるとアイドル時の CPU が跳ねる (回帰テストあり)。
//!
//! 作法は kanban.rs / orchestration.rs と同じ: 判断と描画はこのモジュール、
//! 副作用 (起動・停止・並べ替え) は [`DeckAction`] で app.rs へ返す。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::{self, Color32, RichText};

use crate::i18n::tr;
use crate::theme::Theme;

/// デッキ画面の記号 (パレット・メニュー・ヘッダーで共通に使う)。
/// フォントに glyph がある字だけを使う (app.rs の `ui_symbols_have_glyphs` 参照)。
pub const DECK_ICON: &str = "📇";

/// 副題の区切り (cmux と同じ中黒)。
pub const SUBTITLE_SEP: &str = " • ";

// ---------------------------------------------------------------------------
// リストの構成要素
// ---------------------------------------------------------------------------

/// 稼働中セッション 1 本のスナップショット (app.rs が毎フレーム写す)。
///
/// **状態の表示に使う値は持たない** (承認件数・未読・稼働時間は看板の担当)。
/// ここに残っているのは「並べる」「見分ける」「サンプリングの速さを決める」ための材料だけ。
#[derive(Clone, Debug, Default)]
pub struct LiveRow {
    /// `AgentManager.sessions` の index (**このフレーム内でのみ**有効)
    pub idx: usize,
    /// セッション ID。行の同一性はこれ (index ではない) なので並べ替えで飛ばない。
    pub id: u64,
    /// 一覧の 1 行目に出る表示名 (プリセット名 or セッション名)
    pub title: String,
    pub cwd: PathBuf,
    /// 作業ディレクトリの git ブランチ (分からなければ空)
    pub branch: String,
    /// 起動コマンド (名前も場所も無いときの最後の手がかり)
    pub command: String,
    /// アクティブ (紫枠) のセッションか (初回選択の既定)
    pub active: bool,
}

/// 起動プリセット 1 本 (レール上端の ＋ メニューに出る。**行にはしない**)。
#[derive(Clone, Debug)]
pub struct LauncherRow {
    /// `cfg.agents` の index
    pub idx: usize,
    pub icon: String,
    pub name: String,
}

/// 画面に実際に並ぶ 1 行 = 稼働中セッション 1 本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    /// セッション ID (選択の同一性)
    pub id: u64,
    /// `live[idx]` (**このフレーム内でのみ**有効)
    pub idx: usize,
}

/// **画面に出る 1 行そのもの**。タイトルと、薄い 1 行の副題だけ。
///
/// ここにフィールドを足すことが、そのまま「デッキがダッシュボードに戻る」こと
/// なので、増やさない (テストが分解パターンで固定している)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowView {
    pub id: u64,
    /// 1 行目 — エージェントの表示名
    pub title: String,
    /// 2 行目 — `<ブランチ> • <短縮 cwd>` / 短縮 cwd / 起動コマンド
    pub subtitle: String,
}

/// 絞り込み語がどれかの欄に含まれるか (純関数・大文字小文字を無視)。
pub fn matches_query(query: &str, fields: &[&str]) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    fields.iter().any(|f| f.to_lowercase().contains(&q))
}

/// レールに並べる行を組み立てる **純関数**。
///
/// 入力は**稼働中セッションだけ**。過去の会話も起動プリセットも受け取らない
/// (= 行として出しようがない)。並びは渡された順 = app.rs のセッション順。
pub fn build_rows(live: &[LiveRow], query: &str) -> Vec<Row> {
    live.iter()
        .enumerate()
        .filter(|(_, l)| {
            let cwd = l.cwd.to_string_lossy();
            matches_query(query, &[&l.title, &cwd, &l.branch])
        })
        .map(|(i, l)| Row { id: l.id, idx: i })
        .collect()
}

/// 副題 **純関数**。
/// repo が分かれば `<ブランチ> • <短縮 cwd>`、分からなければ短縮 cwd、
/// それも無ければ起動コマンド。改行は含めない (必ず 1 行)。
pub fn subtitle_of(branch: &str, cwd: &str, command: &str) -> String {
    let b = branch.trim();
    let c = cwd.trim();
    let s = match (b.is_empty(), c.is_empty()) {
        (false, false) => format!("{b}{SUBTITLE_SEP}{c}"),
        (true, false) => c.to_string(),
        (false, true) => b.to_string(),
        (true, true) => command.trim().to_string(),
    };
    one_line(&s)
}

/// 改行・タブを潰して 1 行にする (タイトルも副題も必ず 1 行という不変条件)。
fn one_line(s: &str) -> String {
    s.replace(['\n', '\r', '\t'], " ").trim().to_string()
}

/// 行 → 画面に出る 2 行 (**純関数**)。ここに状態は一切入らない。
pub fn row_views(rows: &[Row], live: &[LiveRow], home: Option<&Path>) -> Vec<RowView> {
    rows.iter()
        .filter_map(|r| {
            let l = live.get(r.idx)?;
            let cwd = short_path(&l.cwd, home);
            let title = if l.title.trim().is_empty() {
                one_line(&l.command)
            } else {
                one_line(&l.title)
            };
            Some(RowView {
                id: l.id,
                title,
                subtitle: subtitle_of(&l.branch, &cwd, &l.command),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 選択とキーボード移動
// ---------------------------------------------------------------------------

/// 選択をいまのリストへ解決する **純関数**。
///
/// - セッションがそのまま残っていれば、その位置を返す (並べ替え・挿入で飛ばない)
/// - 消えていれば、最後に居た位置へ寄せる (末尾を超えたら末尾)
/// - リストが空なら `None`
pub fn resolve_selection(sel: Option<u64>, last_pos: usize, rows: &[Row]) -> Option<(u64, usize)> {
    if rows.is_empty() {
        return None;
    }
    if let Some(id) = sel {
        if let Some(pos) = rows.iter().position(|r| r.id == id) {
            return Some((id, pos));
        }
    }
    let pos = last_pos.min(rows.len() - 1);
    Some((rows[pos].id, pos))
}

/// 上下移動 **純関数**。端では止まる (巻き戻さない — 長いリストで迷子になるため)。
pub fn move_selection(rows: &[Row], cur: Option<u64>, delta: i32) -> Option<u64> {
    if rows.is_empty() || delta == 0 {
        return None;
    }
    let at = cur
        .and_then(|id| rows.iter().position(|r| r.id == id))
        .map(|p| p as i32);
    let next = match at {
        Some(p) => (p + delta).clamp(0, rows.len() as i32 - 1),
        // 未選択なら、下キーで先頭 / 上キーで末尾から入る
        None if delta > 0 => 0,
        None => rows.len() as i32 - 1,
    };
    rows.get(next as usize).map(|r| r.id)
}

// ---------------------------------------------------------------------------
// レイアウト (寸法はすべて「窓幅」と「本文 1 行の高さ」から導く)
// ---------------------------------------------------------------------------

/// 端末ペインの見せ方。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DeckLayout {
    /// レールを畳んで、選択中の 1 本を全幅で出す
    Single,
    /// 左レール + 右に 1 本 (cmux の既定)
    #[default]
    Split,
    /// 左レール + 右で選択の前後を上下に積み上げる
    Stacked,
}

/// 積み上げモードで同時に出せるセッション数の下限・上限。
pub const MIN_STACK: usize = 2;
pub const MAX_STACK: usize = 6;

/// レール幅の既定 (窓幅に対する割合)。cmux の細い左レールに合わせる。
pub const RAIL_FRAC_DEFAULT: f32 = 0.14;
/// レール幅の上限 (窓幅に対する割合)。
pub const RAIL_FRAC_MAX: f32 = 0.42;
/// レール幅の下限 (本文 1 行の高さの倍数)。DPI・フォント設定に追従させるため
/// ピクセルではなく行の高さを単位にする。
const RAIL_MIN_UNITS: f32 = 9.0;
/// レールを横に置ける最小の窓幅 (本文 1 行の高さの倍数)。
/// これより細い窓では、上にレール・下に端末を積む。
const RAIL_SIDE_MIN_UNITS: f32 = 40.0;

impl DeckLayout {
    pub fn to_u8(self) -> u8 {
        match self {
            DeckLayout::Single => 0,
            DeckLayout::Split => 1,
            DeckLayout::Stacked => 2,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => DeckLayout::Single,
            2 => DeckLayout::Stacked,
            _ => DeckLayout::Split,
        }
    }

    /// この配置で一覧 (レール) を出すか。`Single` は端末に全部渡す。
    pub fn shows_rail(self, enabled: bool) -> bool {
        match self {
            DeckLayout::Single => false,
            DeckLayout::Split | DeckLayout::Stacked => enabled,
        }
    }

    /// レールの表示/非表示だけを切り替える (積み上げは保つ)。
    pub fn with_rail(self, on: bool) -> Self {
        match (self, on) {
            (DeckLayout::Stacked, true) => DeckLayout::Stacked,
            (_, true) => DeckLayout::Split,
            (_, false) => DeckLayout::Single,
        }
    }

    /// 積み上げの入/切だけを切り替える (レールは出したまま)。
    pub fn with_stacked(self, on: bool) -> Self {
        if on {
            DeckLayout::Stacked
        } else {
            DeckLayout::Split
        }
    }
}

/// 積み上げ数を範囲へ収める **純関数**。
pub fn clamp_stack(n: usize) -> usize {
    n.clamp(MIN_STACK, MAX_STACK)
}

/// レール幅を窓幅から決める **純関数**。
///
/// `unit` は本文 1 行の高さ (`ui.text_style_height(Body)`)。固定ピクセルを
/// 書かないための単位で、これにより DPI とフォント設定に自動で追従する。
/// 壊れた割合 (負・巨大) を渡されても、必ず `0 < w <= 窓幅 * RAIL_FRAC_MAX` に収まる。
pub fn rail_width(window_w: f32, frac: f32, unit: f32) -> f32 {
    let w = window_w.max(0.0);
    let hi = w * RAIL_FRAC_MAX;
    let lo = (unit.max(1.0) * RAIL_MIN_UNITS).min(hi);
    let want = w * frac.clamp(0.0, RAIL_FRAC_MAX);
    want.clamp(lo, hi.max(lo))
}

/// レールを横 (左) に置けるだけの幅があるか **純関数**。
pub fn rail_fits_beside(window_w: f32, unit: f32) -> bool {
    window_w >= unit.max(1.0) * RAIL_SIDE_MIN_UNITS
}

/// 積み上げモードで実際に描くセッション ID を決める **純関数**。
///
/// 選択中の行から下へ順に拾い、足りなければ先頭から補う。行が無ければ空。
pub fn stacked_ids(rows: &[Row], sel: Option<u64>, want: usize) -> Vec<u64> {
    if rows.is_empty() {
        return Vec::new();
    }
    let start = sel
        .and_then(|id| rows.iter().position(|r| r.id == id))
        .unwrap_or(0);
    let n = want.min(rows.len());
    (0..n).map(|i| rows[(start + i) % rows.len()].id).collect()
}

/// 積み上げペインの高さ比の下限 (全体に対する割合)。
pub const MIN_WEIGHT: f32 = 0.08;

/// 積み上げペインの高さ比を、ドラッグ量に合わせて付け替える **純関数**。
pub fn adjust_weights(w: &mut [f32], i: usize, delta: f32) {
    if i + 1 >= w.len() {
        return;
    }
    let total: f32 = w.iter().sum();
    if total <= 0.0 {
        return;
    }
    let d = delta * total;
    let a = w[i] + d;
    let b = w[i + 1] - d;
    let min = MIN_WEIGHT * total;
    if a < min || b < min {
        return;
    }
    w[i] = a;
    w[i + 1] = b;
}

/// ペイン数に合わせて重みの本数を整える (増えたぶんは平均値で足す)。
pub fn fit_weights(w: &mut Vec<f32>, n: usize) {
    if n == 0 {
        w.clear();
        return;
    }
    while w.len() > n {
        w.pop();
    }
    while w.len() < n {
        let avg = if w.is_empty() {
            1.0
        } else {
            w.iter().sum::<f32>() / w.len() as f32
        };
        w.push(avg);
    }
    if w.iter().all(|v| *v <= 0.0) {
        w.iter_mut().for_each(|v| *v = 1.0);
    }
}

// ---------------------------------------------------------------------------
// キー操作 → 意図 → 副作用
// ---------------------------------------------------------------------------

/// デッキが解釈する打鍵 (UI から切り離してテストするための中間表現)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeckKey {
    Up,
    Down,
    /// Enter — その端末へ入る
    Enter,
    /// ⌥N — 新しいエージェント (先頭プリセット)
    New,
    /// ⌥R — 名前変更
    Rename,
    /// ⌥D — 複製 (同じプリセット + 同じ作業ディレクトリ)
    Duplicate,
    /// ⌥X — 停止 (2 打鍵で確定)
    Stop,
    /// ⌥S — 再起動
    Restart,
    /// ⌥↑ — 並べ替え (上へ)
    MoveUp,
    /// ⌥↓ — 並べ替え (下へ)
    MoveDown,
}

/// 打鍵 → [`DeckKey`] の対応表 **純関数**。
///
/// **素の文字キーは何にも割り当てない** — そのまま「見えない絞り込み」へ流れる
/// から。ライフサイクル操作はすべて ⌥ (Alt) 付き。⌘ が乗っているときは
/// アプリ側のショートカットが先なので手を出さない。
pub fn key_intent(key: egui::Key, alt: bool, cmd: bool) -> Option<DeckKey> {
    if cmd {
        return None;
    }
    if alt {
        return match key {
            egui::Key::ArrowUp => Some(DeckKey::MoveUp),
            egui::Key::ArrowDown => Some(DeckKey::MoveDown),
            egui::Key::N => Some(DeckKey::New),
            egui::Key::R => Some(DeckKey::Rename),
            egui::Key::D => Some(DeckKey::Duplicate),
            egui::Key::X => Some(DeckKey::Stop),
            egui::Key::S => Some(DeckKey::Restart),
            _ => None,
        };
    }
    Some(match key {
        egui::Key::ArrowUp => DeckKey::Up,
        egui::Key::ArrowDown => DeckKey::Down,
        egui::Key::Enter => DeckKey::Enter,
        _ => return None,
    })
}

/// 「見えない絞り込み」へ打った文字を足す **純関数**。
/// 制御文字は捨てる。1 文字でも入ったら true。
pub fn type_into_filter(query: &mut String, text: &str) -> bool {
    let mut changed = false;
    for ch in text.chars().filter(|c| !c.is_control()) {
        query.push(ch);
        changed = true;
    }
    changed
}

/// app.rs へ返す副作用の要求。実行は app.rs (`deck_ui`) 側。
#[derive(Clone, Debug, PartialEq)]
pub enum DeckAction {
    /// アクティブ (紫枠) をこのセッション index へ
    Select(usize),
    /// プリセット index のエージェントを起動 (＋ メニュー / ⌥N)
    Launch(usize),
    /// セッションの表示名を変える
    Rename { id: u64, title: String },
    /// 同じプリセット + 同じ作業ディレクトリでもう 1 本起こす
    Duplicate(usize),
    /// セッションを閉じる (確認済み)
    Stop(usize),
    /// 再起動
    Restart(usize),
    /// 並べ替え (from → to)
    Reorder { from: usize, to: usize },
    /// デッキを閉じる
    Close,
}

/// **打鍵を誰が受け取るか** (純関数)。フォーカスの調停はここだけを通す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOwner {
    /// デッキ本体 — 見えない絞り込みとレールの移動が効く
    Deck,
    /// 端末 (PTY) — Esc / ⇧Tab だけデッキが横取りして一覧へ返す
    Terminal,
    /// テキスト入力 (レールの名前変更欄) — デッキは**一切**触らない
    TextInput,
}

impl KeyOwner {
    /// 素の文字が「見えない絞り込み」へ流れるか。
    pub fn deck_filters(self) -> bool {
        self == KeyOwner::Deck
    }

    /// ↑↓ / ⌥ 系のレール操作が効くか。
    pub fn deck_navigates(self) -> bool {
        self == KeyOwner::Deck
    }
}

/// フォーカス中の egui Id から所有者を決める。
///
/// 名前変更欄に文字を打っている間に「a」でレールが動いたり、絞り込みが
/// 効き始めたりしないための唯一の歯止め。端末の Id は前フレームに描いた分を
/// `term_ids` で覚えてある。**端末にフォーカスがあるときは打鍵をそのまま
/// PTY へ通す** (デッキは cmux と同じで「画面いっぱいがエージェント」)。
pub fn key_owner(focus: Option<egui::Id>, term_ids: &[egui::Id]) -> KeyOwner {
    match focus {
        None => KeyOwner::Deck,
        Some(f) if term_ids.contains(&f) => KeyOwner::Terminal,
        Some(_) => KeyOwner::TextInput,
    }
}

/// デッキ内部で処理する意図 + app.rs へ返す要求。
#[derive(Clone, Debug, PartialEq)]
pub enum Intent {
    /// 選択を動かす
    Move(i32),
    /// 端末へフォーカスを移す
    FocusTerminal,
    /// 名前変更の入力欄を開く
    BeginRename(u64),
    /// 停止の確認を出す (1 打目)
    ArmStop(u64),
    /// 確認を取り下げる
    DisarmStop,
    /// app.rs へ返す副作用
    Act(DeckAction),
}

/// 打鍵を意図へ落とす **純関数** (ここにプロセスを起こす処理は一切無い)。
///
/// `row` は選択中の行。`stop_armed` は「⌥X の 1 打目が入っているセッション」。
pub fn dispatch(
    k: DeckKey,
    rows: &[Row],
    row: Option<Row>,
    live: &[LiveRow],
    launchers: &[LauncherRow],
    stop_armed: Option<u64>,
) -> Vec<Intent> {
    let mut out = Vec::new();
    let cur = row.and_then(|r| live.get(r.idx));
    match k {
        DeckKey::Up => {
            out.push(Intent::DisarmStop);
            out.push(Intent::Move(-1));
        }
        DeckKey::Down => {
            out.push(Intent::DisarmStop);
            out.push(Intent::Move(1));
        }
        DeckKey::Enter => {
            if let Some(l) = cur {
                out.push(Intent::Act(DeckAction::Select(l.idx)));
                out.push(Intent::FocusTerminal);
            }
        }
        DeckKey::New => {
            if let Some(n) = launchers.first() {
                out.push(Intent::Act(DeckAction::Launch(n.idx)));
            }
        }
        DeckKey::Rename => {
            if let Some(l) = cur {
                out.push(Intent::BeginRename(l.id));
            }
        }
        DeckKey::Duplicate => {
            if let Some(l) = cur {
                out.push(Intent::Act(DeckAction::Duplicate(l.idx)));
            }
        }
        DeckKey::Stop => {
            if let Some(l) = cur {
                if stop_armed == Some(l.id) {
                    out.push(Intent::DisarmStop);
                    out.push(Intent::Act(DeckAction::Stop(l.idx)));
                } else {
                    out.push(Intent::ArmStop(l.id));
                }
            }
        }
        DeckKey::Restart => {
            if let Some(l) = cur {
                out.push(Intent::Act(DeckAction::Restart(l.idx)));
            }
        }
        DeckKey::MoveUp | DeckKey::MoveDown => {
            let (Some(r), Some(l)) = (row, cur) else {
                return out;
            };
            let up = k == DeckKey::MoveUp;
            // 画面の並びで隣の行を探し、その実 index と入れ替える
            let Some(at) = rows.iter().position(|x| x.id == r.id) else {
                return out;
            };
            let to = if up {
                at.checked_sub(1)
            } else {
                (at + 1 < rows.len()).then_some(at + 1)
            };
            let Some(target) = to.and_then(|t| rows.get(t)).and_then(|t| live.get(t.idx)) else {
                return out;
            };
            out.push(Intent::Act(DeckAction::Reorder {
                from: l.idx,
                to: target.idx,
            }));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// サンプリングと再描画のリズム
// ---------------------------------------------------------------------------

/// 裏の問い合わせ (ブランチ解決) が飛んでいる間だけ回す刻み。
const SCAN_POLL_MS: u64 = 250;

/// 次に再描画を予約するまでの ms。**`None` なら 1 枚も予約しない**。
///
/// これがデッキの負荷の全て。誰も出力しておらず、走っているエージェントも
/// 無く、裏の問い合わせも飛んでいなければ `None` を返し、判断を app.rs の
/// `schedule_idle_repaint` (= 完全アイドルなら 0 枚) に返す。
pub fn deck_repaint_ms(busy: bool, any_running: bool, scanning: bool) -> Option<u64> {
    if busy {
        return Some(crate::kanban::FAST_SAMPLE_MS);
    }
    if any_running {
        return Some(crate::kanban::SLOW_SAMPLE_MS);
    }
    if scanning {
        return Some(SCAN_POLL_MS);
    }
    None
}

/// 1 セッションぶんの追跡状態。**描画には一切使わない** —
/// 1 行ぶんの追跡状態は [`crate::fleet::engine::Track`] が持つ。
///
/// デッキが自前の `tracks` を持っていた頃は、**ラダー無しの判定** で
/// 回していた (`kanban::classify` は構造化プロトコルもフックも見ない) ので、
/// 同じ 1 体が看板とデッキで別のレーンに居ることが構造的に起こりえた。
/// いまは [`crate::fleet::FleetStore`] のスナップショットを読むだけ。

// ---------------------------------------------------------------------------
// 画面の状態
// ---------------------------------------------------------------------------

/// デッキ画面の UI 状態 (app.rs が保持する)。
///
/// 永続化は egui の persisted memory に置く。config.rs は他所有なので触らない。
#[derive(Default)]
pub struct DeckState {
    /// 選択中のセッション ID (中身で持つので並べ替えで飛ばない)
    selected: Option<u64>,
    /// 最後に居た位置 (行が消えたときの寄せ先)
    sel_pos: usize,
    /// 見えない絞り込み (打った文字がそのまま入る)
    query: String,
    /// レイアウト (None = 永続メモリから未読込)
    layout: Option<DeckLayout>,
    /// 積み上げ数 (None = 未読込)
    stack: Option<usize>,
    /// 積み上げペインの高さ比
    stack_weights: Vec<f32>,
    /// 左レールの取り分 (窓幅に対する割合)
    rail: Option<f32>,
    dirty: bool,
    /// 名前変更中のセッション
    rename_for: Option<u64>,
    rename_buf: String,
    rename_focus: bool,
    /// 停止の 1 打目が入っているセッション
    stop_armed: Option<u64>,
    /// 次のフレームで端末へフォーカスを移す
    focus_term_req: bool,
    /// 前フレームに描いた端末の egui Id (Esc を一覧へ返すために覚える)
    term_ids: Vec<egui::Id>,
}

impl DeckState {
    // PTY 画面の間引きも追跡も [`crate::fleet::FleetStore`] が持つ。
    // デッキは [`crate::fleet::Snapshot`] を読むだけ。

    /// いまの選択 (テスト用)。
    #[cfg(test)]
    fn selected(&self) -> Option<u64> {
        self.selected
    }

    /// 選択を差し替える (クリック・パレットから)。
    pub fn select(&mut self, id: u64) {
        self.selected = Some(id);
        self.dirty = true;
    }

    /// 停止の確認が出ているセッション (テスト用。UI は行の枠で示す)。
    #[cfg(test)]
    fn stop_armed(&self) -> Option<u64> {
        self.stop_armed
    }

    /// 選択をいまのリストへ解決して覚え直す。
    pub fn sync_selection(&mut self, rows: &[Row], live: &[LiveRow]) -> Option<u64> {
        // 初回はアクティブ (紫枠) のセッションを選んでおく
        if self.selected.is_none() {
            self.selected = live
                .iter()
                .find(|l| l.active)
                .map(|l| l.id)
                .or_else(|| rows.first().map(|r| r.id));
        }
        match resolve_selection(self.selected, self.sel_pos, rows) {
            Some((id, pos)) => {
                self.selected = Some(id);
                self.sel_pos = pos;
                Some(id)
            }
            None => {
                self.selected = None;
                None
            }
        }
    }

    /// レイアウト (未読込なら既定)。
    pub fn layout(&self) -> DeckLayout {
        self.layout.unwrap_or_default()
    }

    /// レイアウトを変える (状態機械の唯一の入口)。
    pub fn set_layout(&mut self, l: DeckLayout) {
        self.layout = Some(l);
        self.dirty = true;
    }

    /// 積み上げ数 (未読込なら下限)。
    pub fn stack(&self) -> usize {
        clamp_stack(self.stack.unwrap_or(MIN_STACK))
    }

    /// 積み上げ数を変える (範囲外は丸める)。
    pub fn set_stack(&mut self, n: usize) {
        self.stack = Some(clamp_stack(n));
        self.dirty = true;
    }

    /// レールの取り分 (窓幅に対する割合)。
    pub fn rail_frac(&self) -> f32 {
        self.rail.unwrap_or(RAIL_FRAC_DEFAULT)
    }

    /// 内部の意図を 1 件処理する (キーボードもクリックも同じ口を通す)。
    fn apply_intent(&mut self, it: Intent, rows: &[Row], out: &mut Vec<DeckAction>) {
        match it {
            Intent::Move(d) => {
                if let Some(id) = move_selection(rows, self.selected, d) {
                    self.selected = Some(id);
                    self.dirty = true;
                }
            }
            Intent::FocusTerminal => self.focus_term_req = true,
            Intent::BeginRename(id) => {
                self.rename_for = Some(id);
                self.rename_focus = true;
            }
            Intent::ArmStop(id) => self.stop_armed = Some(id),
            Intent::DisarmStop => self.stop_armed = None,
            Intent::Act(a) => out.push(a),
        }
    }
}

// ---------------------------------------------------------------------------
// 描画
// ---------------------------------------------------------------------------

/// 選択中セッションの端末を描くためのコールバック。
/// app.rs が `terminal::draw` を呼ぶだけの実装を渡す (端末を再実装しない)。
/// 引数はセッション ID。返り値の `Response` があればフォーカス移動に使う。
pub type LiveDraw<'a> = &'a mut dyn FnMut(&mut egui::Ui, u64) -> Option<egui::Response>;

/// 永続メモリのキー (config.rs は他所有なので egui の memory に持つ)。
fn mem_id(name: &str) -> egui::Id {
    egui::Id::new(("zv-deck", name))
}

/// 相対輝度 (sRGB の近似)。選択行の文字色を選ぶためだけに使う。
fn luma(c: Color32) -> f32 {
    (0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32) / 255.0
}

/// accent のべた塗りの上に置く文字色 **純関数**。
/// テーマの 2 色 (本文色 / 背景色) のうち、accent との明度差が大きい方を選ぶ。
/// リテラルの色を書かずに、どのテーマでも読める前景を得るための唯一の判断。
pub fn on_accent(theme: &Theme) -> Color32 {
    let a = luma(theme.accent);
    if (luma(theme.text) - a).abs() >= (luma(theme.bg) - a).abs() {
        theme.text
    } else {
        theme.bg
    }
}

/// 作業ディレクトリの短縮表示 (ホームは `~`、深いパスは末尾 2 段)。
pub fn short_path(p: &Path, home: Option<&Path>) -> String {
    let full = p.to_string_lossy().to_string();
    let rel = match home {
        Some(h) => match p.strip_prefix(h) {
            Ok(t) if t.as_os_str().is_empty() => "~".to_string(),
            Ok(t) => format!("~/{}", t.to_string_lossy()),
            Err(_) => full.clone(),
        },
        None => full.clone(),
    };
    // 段数が多いときは末尾 2 段 + 先頭の印だけ残す
    let segs: Vec<&str> = rel.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if segs.len() <= 3 {
        return rel;
    }
    format!("…/{}/{}", segs[segs.len() - 2], segs[segs.len() - 1])
}

/// 1 行に収めた galley (溢れたら `…`)。行の高さを固定するために使う。
fn line_galley(
    ui: &egui::Ui,
    text: &str,
    style: egui::TextStyle,
    color: Color32,
    max_w: f32,
) -> Arc<egui::Galley> {
    let font = style.resolve(ui.style());
    let mut job = egui::text::LayoutJob::single_section(
        text.to_string(),
        egui::TextFormat {
            font_id: font,
            color,
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: max_w.max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.fonts(|f| f.layout_job(job))
}

/// デッキ画面を描き、押された操作を返す。
///
/// `now_ms` は supervisor の経過時計 (アプリ起動からの ms)。
/// `snap` は [`crate::fleet::FleetStore`] のスナップショット。
/// **デッキはここからしか状態を読まない** (自分で `classify` を呼ばない)。
/// `scanning` は裏の問い合わせ (ブランチ解決) が飛んでいるか
/// (再描画のリズムに効く。終わったら止まる)。
#[allow(clippy::too_many_arguments)]
pub fn ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    live: &[LiveRow],
    launchers: &[LauncherRow],
    snap: &std::sync::Arc<crate::fleet::Snapshot>,
    scanning: bool,
    draw: LiveDraw<'_>,
) -> Vec<DeckAction> {
    let mut acts: Vec<DeckAction> = Vec::new();
    let ctx = ui.ctx().clone();

    // ── 永続状態の読み込み ─────────────────────────────────
    if st.layout.is_none() {
        let v = ctx
            .data_mut(|d| *d.get_persisted_mut_or(mem_id("layout"), DeckLayout::default().to_u8()));
        st.layout = Some(DeckLayout::from_u8(v));
    }
    if st.stack.is_none() {
        let v = ctx.data_mut(|d| *d.get_persisted_mut_or(mem_id("stack"), MIN_STACK));
        st.stack = Some(clamp_stack(v));
    }
    if st.rail.is_none() {
        let v = ctx.data_mut(|d| *d.get_persisted_mut_or(mem_id("rail"), RAIL_FRAC_DEFAULT));
        st.rail = Some(v.clamp(0.0, RAIL_FRAC_MAX));
    }
    if st.selected.is_none() {
        let v = ctx.data_mut(|d| *d.get_persisted_mut_or(mem_id("sel-id"), 0_u64));
        st.selected = (v != 0).then_some(v);
    }

    // ── 判定は **FleetStore が済ませてある**。ここは読むだけ ──
    // 無条件の再描画はしない。動きがあるときだけ回す (完全に静かなら 1 枚も出さない)。
    if let Some(ms) = deck_repaint_ms(snap.busy, snap.any_running, scanning) {
        crate::perf::repaint_after(&ctx, std::time::Duration::from_millis(ms), "deck_anim");
    }

    let rows = build_rows(live, &st.query);
    let selected = st.sync_selection(&rows, live);
    let views = row_views(&rows, live, home_dir().as_deref());

    let unit = ui.text_style_height(&egui::TextStyle::Body);
    let full_w = ui.available_width();
    let full_h = ui.available_height().max(unit * 4.0);
    ui.set_min_height(full_h);

    keyboard_ui(st, ui, &rows, live, launchers, &mut acts);

    let show_rail = st.layout().shows_rail(true);
    let beside = rail_fits_beside(full_w, unit);

    ui.allocate_ui_with_layout(
        egui::vec2(full_w, full_h),
        if beside {
            egui::Layout::left_to_right(egui::Align::Min)
        } else {
            egui::Layout::top_down(egui::Align::Min)
        },
        |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            if !show_rail {
                pane_ui(
                    st, ui, theme, &rows, live, selected, &views, full_w, full_h, launchers, draw,
                    &mut acts,
                );
            } else if beside {
                let rail_w = rail_width(full_w, st.rail_frac(), unit);
                ui.allocate_ui_with_layout(
                    egui::vec2(rail_w, full_h),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        rail_ui(
                            st, ui, theme, &rows, live, &views, launchers, unit, &mut acts,
                        );
                    },
                );
                let thin = splitter_ui(st, ui, theme, true, full_w, unit);
                let rest = (full_w - rail_w - thin).max(unit * 6.0);
                pane_ui(
                    st, ui, theme, &rows, live, selected, &views, rest, full_h, launchers, draw,
                    &mut acts,
                );
            } else {
                // 細い窓: 一覧を上、端末を下に積む
                let rail_h =
                    (full_h * 0.35).clamp(unit * 4.0, (full_h - unit * 8.0).max(unit * 4.0));
                ui.allocate_ui_with_layout(
                    egui::vec2(full_w, rail_h),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        rail_ui(
                            st, ui, theme, &rows, live, &views, launchers, unit, &mut acts,
                        );
                    },
                );
                let rest = (full_h - rail_h).max(unit * 6.0);
                pane_ui(
                    st, ui, theme, &rows, live, selected, &views, full_w, rest, launchers, draw,
                    &mut acts,
                );
            }
        },
    );

    // ── 永続状態の書き戻し ─────────────────────────────────
    if st.dirty {
        let layout = st.layout.unwrap_or_default().to_u8();
        let stack = st.stack();
        let rail = st.rail_frac();
        let sel = st.selected.unwrap_or(0);
        ctx.data_mut(|d| {
            d.insert_persisted(mem_id("layout"), layout);
            d.insert_persisted(mem_id("stack"), stack);
            d.insert_persisted(mem_id("rail"), rail);
            d.insert_persisted(mem_id("sel-id"), sel);
        });
        st.dirty = false;
    }

    acts
}

/// 小さなアイコンボタン (枠なし)。ヘッダー / レール上端で共通に使う。
fn icon_button(ui: &mut egui::Ui, theme: &Theme, glyph: &str, on: bool, hint: &str) -> bool {
    let color = if on { theme.accent } else { theme.text_dim };
    ui.add(
        egui::Button::new(RichText::new(glyph).small().color(color))
            .frame(false)
            .small(),
    )
    .on_hover_text(hint.to_string())
    .clicked()
}

/// ＋ メニュー (新しいエージェント)。押されたプリセットを返す。
/// **プリセットを行として並べないための唯一の入口**。
fn new_agent_menu(ui: &mut egui::Ui, theme: &Theme, launchers: &[LauncherRow]) -> Option<usize> {
    let mut picked = None;
    ui.menu_button(RichText::new("＋").small().color(theme.text_dim), |ui| {
        if launchers.is_empty() {
            ui.label(RichText::new(tr("起動プリセットがありません")).small());
        }
        for l in launchers {
            if ui.button(format!("{} {}", l.icon, l.name)).clicked() {
                picked = Some(l.idx);
                ui.close_menu();
            }
        }
    })
    .response
    .on_hover_text(tr("新しいエージェントを起こす (⌥N)"));
    picked
}

/// 左レール: ＋ とレール畳みだけの上端 + 稼働中エージェントの縦 1 本。
#[allow(clippy::too_many_arguments)]
fn rail_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    rows: &[Row],
    live: &[LiveRow],
    views: &[RowView],
    launchers: &[LauncherRow],
    unit: f32,
    acts: &mut Vec<DeckAction>,
) {
    let pad = unit * 0.35;
    egui::Frame::none()
        .fill(theme.panel)
        .inner_margin(egui::Margin::symmetric(pad, pad * 0.6))
        .show(ui, |ui| {
            ui.set_min_height(ui.available_height());
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(pad * 0.5, pad * 0.4);

            // ── 上端: ＋ と レール畳み だけ ──
            ui.horizontal(|ui| {
                if let Some(i) = new_agent_menu(ui, theme, launchers) {
                    acts.push(DeckAction::Launch(i));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icon_button(ui, theme, "▤", true, &tr("一覧を畳む (端末だけにする)"))
                    {
                        st.set_layout(st.layout().with_rail(false));
                    }
                });
            });

            // ── 一覧 (絞り込みピルを出すときだけ、その 1 行ぶんを残す) ──
            let pill_h = if st.query.is_empty() { 0.0 } else { unit * 1.4 };
            let list_h = (ui.available_height() - pill_h).max(unit * 2.0);
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), list_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    let w = ui.available_width();
                    egui::ScrollArea::vertical()
                        .id_salt("zv-deck-rail")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_width(w);
                            if views.is_empty() {
                                empty_rail_ui(ui, theme, launchers, live.is_empty(), unit, acts);
                                return;
                            }
                            for (v, r) in views.iter().zip(rows.iter()) {
                                row_ui(st, ui, theme, v, *r, live, unit, acts);
                            }
                        });
                },
            );

            // ── 下端: 絞り込みが効いているときだけ、細いピル ──
            if !st.query.is_empty() {
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    egui::Frame::none()
                        .fill(theme.panel_alt)
                        .rounding(egui::Rounding::same(unit * 0.5))
                        .inner_margin(egui::Margin::symmetric(pad, pad * 0.2))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(format!("🔎 {}", st.query))
                                    .small()
                                    .color(theme.text_dim),
                            )
                            .on_hover_text(tr("打った文字で絞り込み中 — Esc で消えます"));
                        });
                });
            }
        });
}

/// エージェントが 1 体も居ないときのレール (1 行 + ＋ だけ)。
fn empty_rail_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    launchers: &[LauncherRow],
    no_sessions: bool,
    unit: f32,
    acts: &mut Vec<DeckAction>,
) {
    ui.vertical_centered(|ui| {
        ui.add_space(unit);
        ui.label(
            RichText::new(if no_sessions {
                tr("まだエージェントがいません")
            } else {
                tr("該当なし")
            })
            .small()
            .color(theme.text_dim),
        );
        ui.add_space(unit * 0.4);
        if no_sessions {
            if let Some(i) = new_agent_menu(ui, theme, launchers) {
                acts.push(DeckAction::Launch(i));
            }
        }
    });
}

/// 一覧の 1 行 — **2 行の文字だけ**。選択行は accent のべた塗り。
#[allow(clippy::too_many_arguments)]
fn row_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    v: &RowView,
    r: Row,
    live: &[LiveRow],
    unit: f32,
    acts: &mut Vec<DeckAction>,
) {
    let on = st.selected == Some(v.id);
    let pad = unit * 0.3;
    let title_h = ui.text_style_height(&egui::TextStyle::Body);
    let sub_h = ui.text_style_height(&egui::TextStyle::Small);
    let h = title_h + sub_h + pad * 2.0;
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());

    let fill = if on {
        theme.accent
    } else if resp.hovered() {
        theme.panel_alt
    } else {
        Color32::TRANSPARENT
    };
    if fill != Color32::TRANSPARENT {
        ui.painter()
            .rect_filled(rect, egui::Rounding::same(pad), fill);
    }

    let fg = if on { on_accent(theme) } else { theme.text };
    let dim = if on {
        on_accent(theme).gamma_multiply(0.72)
    } else {
        theme.text_dim
    };
    let inner_w = (w - pad * 2.0).max(unit);

    // 名前変更中はタイトル行が入力欄に化ける (状態表示ではなく一時的な編集)
    if st.rename_for == Some(v.id) {
        let id = mem_id("rename");
        let edit = ui.put(
            egui::Rect::from_min_size(
                rect.min + egui::vec2(pad, pad * 0.5),
                egui::vec2(inner_w, title_h + pad),
            ),
            egui::TextEdit::singleline(&mut st.rename_buf).id(id),
        );
        if st.rename_focus {
            edit.request_focus();
            st.rename_focus = false;
        }
        let commit = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if commit {
            let t = st.rename_buf.trim().to_string();
            if !t.is_empty() {
                acts.push(DeckAction::Rename { id: v.id, title: t });
            }
            st.rename_for = None;
        } else if edit.lost_focus() {
            st.rename_for = None;
        }
    } else {
        let t = line_galley(ui, &v.title, egui::TextStyle::Body, fg, inner_w);
        ui.painter().galley(rect.min + egui::vec2(pad, pad), t, fg);
    }

    let s = line_galley(ui, &v.subtitle, egui::TextStyle::Small, dim, inner_w);
    ui.painter()
        .galley(rect.min + egui::vec2(pad, pad + title_h), s, dim);

    // 停止の 1 打目が入っている行は、枠だけで知らせる (文字は増やさない)
    if st.stop_armed == Some(v.id) {
        ui.painter().rect_stroke(
            rect,
            egui::Rounding::same(pad),
            egui::Stroke::new(1.0_f32, theme.err),
        );
    }

    let resp = resp.on_hover_text(format!("{}\n{}", v.title, v.subtitle));
    if resp.clicked() || resp.double_clicked() {
        st.select(v.id);
        st.stop_armed = None;
        if let Some(l) = live.get(r.idx) {
            acts.push(DeckAction::Select(l.idx));
        }
    }
}

/// レールと端末の間のドラッグバー。実際に使った太さを返す。
fn splitter_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    horizontal: bool,
    span: f32,
    unit: f32,
) -> f32 {
    let thin = (unit * 0.16).max(1.0);
    let size = if horizontal {
        egui::vec2(thin, ui.available_height())
    } else {
        egui::vec2(ui.available_width(), thin)
    };
    let resp = ui.allocate_response(size, egui::Sense::drag());
    let hot = resp.hovered() || resp.dragged();
    ui.painter().rect_filled(
        resp.rect,
        egui::Rounding::ZERO,
        if hot { theme.accent } else { theme.border },
    );
    resp.clone().on_hover_cursor(if horizontal {
        egui::CursorIcon::ResizeHorizontal
    } else {
        egui::CursorIcon::ResizeVertical
    });
    if resp.dragged() && span > 1.0 {
        let d = resp.drag_delta();
        let delta = if horizontal { d.x } else { d.y } / span;
        st.rail = Some((st.rail_frac() + delta).clamp(0.0, RAIL_FRAC_MAX));
        st.dirty = true;
    }
    thin
}

/// 右 (または下) 側 — 細いヘッダー 1 本 + 端末が残り全部。
#[allow(clippy::too_many_arguments)]
fn pane_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    rows: &[Row],
    live: &[LiveRow],
    selected: Option<u64>,
    views: &[RowView],
    width: f32,
    height: f32,
    launchers: &[LauncherRow],
    draw: LiveDraw<'_>,
    acts: &mut Vec<DeckAction>,
) {
    let unit = ui.text_style_height(&egui::TextStyle::Body);
    ui.allocate_ui_with_layout(
        egui::vec2(width, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            let head_h = unit * 1.7;
            header_strip_ui(
                st, ui, theme, selected, views, launchers, width, head_h, unit, acts,
            );

            // 細いヘッダーを引いた**残り全部**が端末。取り分ける帯は 1 つも無い
            // (cmux と同じで、打った字はそのまま端末へ行く = ペイン全体がエージェント)。
            let body_h = pane_body_h(height, head_h, unit);
            ui.allocate_ui_with_layout(
                egui::vec2(width, body_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                    term_stack_ui(
                        st, ui, theme, rows, live, selected, width, body_h, unit, draw, acts,
                    );
                },
            );
        },
    );
}

/// 端末に渡す高さ **純関数** — ヘッダー以外は 1 px も取り分けない。
///
/// 下端の入力欄を消したので、ここから引くのはヘッダーだけ。極端に低い窓でも
/// 3 行は残す (負の高さを egui へ渡さないため)。
pub fn pane_body_h(height: f32, head_h: f32, unit: f32) -> f32 {
    (height - head_h).max(unit * 3.0)
}

/// 端末の積み上げ本体 (ペインの残り全部を使う)。
#[allow(clippy::too_many_arguments)]
fn term_stack_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    rows: &[Row],
    live: &[LiveRow],
    selected: Option<u64>,
    width: f32,
    body_h: f32,
    unit: f32,
    draw: LiveDraw<'_>,
    acts: &mut Vec<DeckAction>,
) {
    let ids: Vec<u64> = match st.layout() {
        DeckLayout::Stacked => stacked_ids(rows, selected, st.stack()),
        _ => selected.into_iter().collect(),
    };

    st.term_ids.clear();
    if ids.is_empty() {
        empty_pane_ui(ui, theme, width, body_h);
        return;
    }
    fit_weights(&mut st.stack_weights, ids.len());
    let total: f32 = st.stack_weights.iter().sum();
    let bar = if ids.len() > 1 {
        (unit * 0.16).max(1.0)
    } else {
        0.0
    };
    let usable = (body_h - bar * (ids.len() - 1) as f32).max(unit * 3.0);
    let mut want_focus = st.focus_term_req;
    let multi = ids.len() > 1;
    for (i, id) in ids.iter().enumerate() {
        let frac = st.stack_weights.get(i).copied().unwrap_or(1.0) / total.max(0.001);
        let h = (usable * frac).max(unit * 3.0);
        let focused = Some(*id) == selected;
        term_pane_ui(
            st,
            ui,
            theme,
            live,
            *id,
            width,
            h,
            unit,
            multi,
            focused && want_focus,
            draw,
            acts,
        );
        if focused {
            want_focus = false;
        }
        if i + 1 < ids.len() {
            stack_bar_ui(st, ui, theme, i, usable, bar);
        }
    }
    st.focus_term_req = false;
}

/// 上端の細い帯 — 選んでいるものの名前と場所 + 小さなアイコンだけ。
#[allow(clippy::too_many_arguments)]
fn header_strip_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    selected: Option<u64>,
    views: &[RowView],
    launchers: &[LauncherRow],
    width: f32,
    height: f32,
    unit: f32,
    acts: &mut Vec<DeckAction>,
) {
    let cur = selected.and_then(|id| views.iter().find(|v| v.id == id));
    let pad = unit * 0.35;
    egui::Frame::none()
        .fill(theme.panel)
        .inner_margin(egui::Margin::symmetric(pad, pad * 0.25))
        .show(ui, |ui| {
            ui.set_width((width - pad * 2.0).max(unit));
            ui.set_min_height(height - pad * 0.5);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = pad * 0.6;
                let title = cur
                    .map(|v| v.title.clone())
                    .unwrap_or_else(|| format!("{DECK_ICON} {}", tr("エージェントデッキ")));
                let sub = cur.map(|v| v.subtitle.clone()).unwrap_or_default();
                let avail = ui.available_width();
                let t = line_galley(ui, &title, egui::TextStyle::Body, theme.text, avail * 0.45);
                let (r1, _) = ui.allocate_exact_size(t.size(), egui::Sense::hover());
                ui.painter().galley(r1.min, t, theme.text);
                if !sub.is_empty() {
                    let s = line_galley(
                        ui,
                        &sub,
                        egui::TextStyle::Small,
                        theme.text_dim,
                        avail * 0.35,
                    );
                    let (r2, _) = ui.allocate_exact_size(s.size(), egui::Sense::hover());
                    let y = r1.center().y - s.size().y * 0.5;
                    ui.painter()
                        .galley(egui::pos2(r2.min.x, y), s, theme.text_dim);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icon_button(ui, theme, "✕", false, &tr("デッキを閉じる (Esc)")) {
                        acts.push(DeckAction::Close);
                    }
                    if st.layout() == DeckLayout::Stacked {
                        if icon_button(ui, theme, "－", false, &tr("同時に映す数を減らす"))
                        {
                            st.set_stack(st.stack().saturating_sub(1));
                        }
                        if icon_button(ui, theme, "＋", false, &tr("同時に映す数を増やす"))
                        {
                            st.set_stack(st.stack() + 1);
                        }
                    }
                    let stacked = st.layout() == DeckLayout::Stacked;
                    if icon_button(ui, theme, "⊟", stacked, &tr("複数の端末を上下に積む"))
                    {
                        st.set_layout(st.layout().with_stacked(!stacked));
                    }
                    let rail = st.layout().shows_rail(true);
                    if icon_button(ui, theme, "▤", rail, &tr("一覧の出し入れ")) {
                        st.set_layout(st.layout().with_rail(!rail));
                    }
                    if !rail {
                        if let Some(i) = new_agent_menu(ui, theme, launchers) {
                            acts.push(DeckAction::Launch(i));
                        }
                    }
                });
            });
        });
}

/// 端末 1 枚 — 枠も余白も持たず、渡された矩形いっぱいに描く。
/// 積み上げているときだけ、上に薄い 1 行の名前を出す (どれか分からなくなるため)。
#[allow(clippy::too_many_arguments)]
fn term_pane_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    live: &[LiveRow],
    id: u64,
    width: f32,
    height: f32,
    unit: f32,
    show_name: bool,
    want_focus: bool,
    draw: LiveDraw<'_>,
    acts: &mut Vec<DeckAction>,
) {
    let Some(l) = live.iter().find(|l| l.id == id) else {
        return;
    };
    let idx = l.idx;
    let title = l.title.clone();
    let on = st.selected == Some(id);
    ui.allocate_ui_with_layout(
        egui::vec2(width, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            let mut body = height;
            if show_name {
                let hh = unit * 1.2;
                let (rect, _) = ui.allocate_exact_size(egui::vec2(width, hh), egui::Sense::hover());
                let col = if on { theme.text } else { theme.text_dim };
                let g = line_galley(ui, &title, egui::TextStyle::Small, col, width - unit * 0.6);
                ui.painter().galley(
                    egui::pos2(rect.min.x + unit * 0.3, rect.center().y - g.size().y * 0.5),
                    g,
                    col,
                );
                body -= hh;
            }
            ui.allocate_ui_with_layout(
                egui::vec2(width, body.max(unit * 2.0)),
                egui::Layout::top_down(egui::Align::Min),
                |ui| match draw(ui, id) {
                    Some(r) => {
                        st.term_ids.push(r.id);
                        if want_focus {
                            r.request_focus();
                        }
                        // 端末を触ったらアクティブ選択も追従させる
                        if r.clicked() || r.drag_started() || r.gained_focus() {
                            st.selected = Some(id);
                            st.dirty = true;
                            acts.push(DeckAction::Select(idx));
                        }
                    }
                    None => {
                        ui.label(
                            RichText::new(tr("この端末はいま表示できません"))
                                .small()
                                .color(theme.text_dim),
                        );
                    }
                },
            );
        },
    );
}

/// 積み上げペインの間のドラッグバー。
fn stack_bar_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    i: usize,
    span: f32,
    thin: f32,
) {
    let resp = ui.allocate_response(
        egui::vec2(ui.available_width(), thin.max(1.0)),
        egui::Sense::drag(),
    );
    let hot = resp.hovered() || resp.dragged();
    ui.painter().rect_filled(
        resp.rect,
        egui::Rounding::ZERO,
        if hot { theme.accent } else { theme.border },
    );
    resp.clone()
        .on_hover_cursor(egui::CursorIcon::ResizeVertical);
    if resp.dragged() && span > 1.0 {
        adjust_weights(&mut st.stack_weights, i, resp.drag_delta().y / span);
        st.dirty = true;
    }
}

/// 端末が 1 枚も無いときの右側 (1 行だけ。プリセット一覧は出さない)。
fn empty_pane_ui(ui: &mut egui::Ui, theme: &Theme, width: f32, height: f32) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, height),
        egui::Layout::top_down(egui::Align::Center),
        |ui| {
            ui.add_space(height * 0.35);
            ui.label(
                RichText::new(tr("一覧から選ぶと、ここにその端末が出ます"))
                    .small()
                    .color(theme.text_dim),
            );
        },
    );
}

/// キーボード操作。端末・名前変更欄にフォーカスがある間は打鍵を奪わない
/// (端末が持っているときは Esc / ⇧Tab 以外そのまま PTY へ流れる)。
///
/// 素の文字は**見えない絞り込み**へ流れる。ライフサイクルは ⌥ 付き。
fn keyboard_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    rows: &[Row],
    live: &[LiveRow],
    launchers: &[LauncherRow],
    acts: &mut Vec<DeckAction>,
) {
    let focus = ui.ctx().memory(|m| m.focused());
    let owner = key_owner(focus, &st.term_ids);
    if owner == KeyOwner::Terminal {
        // Esc / ⇧Tab で一覧へ戻る (端末が同じキーを PTY へ流さないよう先に取る)
        let back = ui.input_mut(|i| {
            i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
                || i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab)
        });
        if back {
            if let Some(f) = focus {
                ui.ctx().memory_mut(|m| m.surrender_focus(f));
            }
        }
        return;
    }
    // 名前変更欄を打っている間は**キーに一切触らない**。
    // ここを緩めると「名前を書いている最中に j でレールが動く」に戻る。
    if !owner.deck_navigates() {
        return;
    }

    // Tab で端末へ
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)) {
        st.focus_term_req = true;
    }
    // Esc: 絞り込みが効いていれば、まずそれを消す (デッキは閉じない)
    if !st.query.is_empty()
        && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        st.query.clear();
        return;
    }
    // Backspace で 1 文字消す
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Backspace)) {
        st.query.pop();
    }
    // 打った文字はそのまま絞り込みへ (修飾キーが乗っているときは触らない —
    // macOS の ⌥ は文字イベントも出すため)
    let typed: String = ui.input(|i| {
        if i.modifiers.alt || i.modifiers.command || i.modifiers.mac_cmd || i.modifiers.ctrl {
            return String::new();
        }
        i.events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    });
    if owner.deck_filters() {
        type_into_filter(&mut st.query, &typed);
    }

    let pressed: Vec<(egui::Key, egui::Modifiers)> = ui.input(|i| {
        i.events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => Some((*key, *modifiers)),
                _ => None,
            })
            .collect()
    });

    let row = st
        .selected
        .and_then(|id| rows.iter().find(|r| r.id == id))
        .copied();
    for (key, m) in pressed {
        let Some(k) = key_intent(key, m.alt, m.command || m.mac_cmd || m.ctrl) else {
            continue;
        };
        let intents = dispatch(k, rows, row, live, launchers, st.stop_armed);
        // 名前変更を始めるときは、いまの名前を初期値に入れる
        for it in intents {
            if let Intent::BeginRename(id) = &it {
                st.rename_buf = live
                    .iter()
                    .find(|l| l.id == *id)
                    .map(|l| l.title.clone())
                    .unwrap_or_default();
            }
            st.apply_intent(it, rows, acts);
        }
    }
}

/// ホームディレクトリ (短縮表示用。1 回だけ引く)。
fn home_dir() -> Option<PathBuf> {
    use std::sync::OnceLock;
    static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOME.get_or_init(dirs::home_dir).clone()
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn live(idx: usize, id: u64, title: &str) -> LiveRow {
        LiveRow {
            idx,
            id,
            title: title.into(),
            cwd: PathBuf::from("/tmp/work"),
            branch: String::new(),
            command: "claude".into(),
            ..Default::default()
        }
    }

    fn launcher(idx: usize, name: &str) -> LauncherRow {
        LauncherRow {
            idx,
            icon: "👾".into(),
            name: name.into(),
        }
    }

    // ── レールに並ぶのは稼働中エージェントだけ ─────────────

    /// 行はセッションと 1 対 1。見出しも過去セッションもプリセットも作らない。
    #[test]
    fn only_live_agents_get_rows() {
        let l = vec![live(0, 1, "a"), live(1, 2, "b"), live(2, 3, "c")];
        let rows = build_rows(&l, "");
        assert_eq!(rows.len(), l.len(), "行はセッションと 1 対 1");
        assert_eq!(
            rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "渡された順のまま"
        );
        let views = row_views(&rows, &l, None);
        assert_eq!(views.len(), rows.len(), "見出し行が挟まらない");
        assert_eq!(
            views.iter().map(|v| v.id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        // セッションが 0 なら行も 0 (プリセットで埋めない)
        assert!(build_rows(&[], "").is_empty());
        assert!(row_views(&[], &[], None).is_empty());
    }

    /// **画面に出るのは id + タイトル + 副題だけ。**
    /// ここに状態・経過・件数のフィールドが増えたら、分解パターンが落ちる。
    #[test]
    fn row_view_has_exactly_a_title_and_one_subtitle() {
        let mut a = live(0, 1, "zaivern-code");
        a.branch = "main".into();
        a.cwd = PathBuf::from("/Users/me/dev/zaivern-code");
        let home = PathBuf::from("/Users/me");
        let rows = build_rows(std::slice::from_ref(&a), "");
        let views = row_views(&rows, std::slice::from_ref(&a), Some(&home));

        // フィールドが増えたらここでコンパイルが落ちる (ダッシュボードへの出戻り防止)
        let RowView {
            id,
            title,
            subtitle,
        } = views[0].clone();
        assert_eq!(id, 1);
        assert_eq!(title, "zaivern-code");
        assert_eq!(subtitle, "main • ~/dev/zaivern-code");
        assert!(!title.contains('\n') && !subtitle.contains('\n'), "各 1 行");
    }

    #[test]
    fn subtitles_fall_back_from_branch_to_cwd_to_command() {
        assert_eq!(subtitle_of("main", "~/dev/x", "claude"), "main • ~/dev/x");
        assert_eq!(subtitle_of("", "~/dev/x", "claude"), "~/dev/x");
        assert_eq!(
            subtitle_of("", "", "claude --resume 1"),
            "claude --resume 1"
        );
        assert_eq!(subtitle_of("main", "", "claude"), "main");
        // 改行が混ざっても 1 行に潰す
        assert_eq!(subtitle_of("ma\nin", "x", "c"), "ma in • x");
    }

    #[test]
    fn live_rows_without_a_title_fall_back_to_the_command() {
        let mut l = live(0, 1, "");
        l.command = "codex --yolo".into();
        let rows = build_rows(std::slice::from_ref(&l), "");
        let views = row_views(&rows, std::slice::from_ref(&l), None);
        assert_eq!(views[0].title, "codex --yolo");
    }

    // ── 見えない絞り込み ───────────────────────────────────

    #[test]
    fn typing_filters_the_list_and_esc_clears_it() {
        let l = vec![live(0, 1, "claude 本体"), live(1, 2, "codex")];
        let mut q = String::new();

        assert!(type_into_filter(&mut q, "co"), "打った文字が入る");
        assert_eq!(q, "co");
        assert!(!type_into_filter(&mut q, "\n\t"), "制御文字は入れない");
        assert_eq!(q, "co");

        let rows = build_rows(&l, &q);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 2);

        // Backspace 相当と Esc 相当 (欄を持たないので単なる String 操作)
        q.pop();
        assert_eq!(build_rows(&l, &q).len(), 2, "1 文字消すと広がる");
        q.clear();
        assert_eq!(build_rows(&l, &q).len(), 2);
    }

    #[test]
    fn query_is_case_insensitive_and_trims() {
        assert!(matches_query("  CLAUDE ", &["claude code"]));
        assert!(matches_query("", &["なんでも"]));
        assert!(!matches_query("zzz", &["claude"]));
    }

    #[test]
    fn branch_is_searchable_even_though_it_is_only_a_subtitle() {
        let mut l = live(0, 1, "a");
        l.branch = "night/2026-07-26".into();
        let rows = build_rows(std::slice::from_ref(&l), "night/");
        assert_eq!(rows.len(), 1);
    }

    // ── 選択の安定性 ────────────────────────────────────────

    fn rows_of(ids: &[u64]) -> Vec<Row> {
        ids.iter()
            .enumerate()
            .map(|(i, id)| Row { id: *id, idx: i })
            .collect()
    }

    #[test]
    fn selection_survives_insert_and_reorder() {
        let rows = rows_of(&[1, 2, 3]);
        let (k, pos) = resolve_selection(Some(2), 1, &rows).unwrap();
        assert_eq!((k, pos), (2, 1));
        // 先頭に 1 本挿さっても同じセッションを指す
        let rows2 = rows_of(&[9, 1, 2, 3]);
        let (k2, pos2) = resolve_selection(Some(k), pos, &rows2).unwrap();
        assert_eq!((k2, pos2), (2, 2));
        // 並べ替えても同じ
        let rows3 = rows_of(&[3, 2, 1, 9]);
        assert_eq!(resolve_selection(Some(k2), pos2, &rows3).unwrap(), (2, 1));
    }

    #[test]
    fn selection_falls_back_to_the_same_slot_when_removed() {
        // 2 が消えた: 同じ位置 (index 1) に居る 3 へ寄る
        assert_eq!(
            resolve_selection(Some(2), 1, &rows_of(&[1, 3])),
            Some((3, 1))
        );
        // 末尾が消えた: 末尾へ丸める
        assert_eq!(resolve_selection(Some(3), 2, &rows_of(&[1])), Some((1, 0)));
        // 空なら選択なし
        assert_eq!(resolve_selection(Some(1), 0, &[]), None);
    }

    /// 一覧が入れ替わっても選択が飛ばない (止めた / 起こした直後)。
    #[test]
    fn selection_is_stable_when_the_list_mutates_under_it() {
        let l = vec![live(0, 1, "a"), live(1, 2, "b"), live(2, 3, "c")];
        let rows = build_rows(&l, "");
        let mut st = DeckState::default();
        st.select(2);
        assert_eq!(st.sync_selection(&rows, &l), Some(2));
        // 2 が消える → 同じ位置 (index 1) に来た 3 へ寄る
        let rest = vec![live(0, 1, "a"), live(1, 3, "c")];
        let rows2 = build_rows(&rest, "");
        assert_eq!(st.sync_selection(&rows2, &rest), Some(3));
        // 絞り込みで一時的に空になっても、解除で戻せる
        let none = build_rows(&rest, "zzz");
        assert_eq!(st.sync_selection(&none, &rest), None);
        assert_eq!(st.sync_selection(&rows2, &rest), Some(1), "先頭へ寄る");
    }

    // ── キーボード ──────────────────────────────────────────

    #[test]
    fn arrows_walk_the_list_and_stop_at_the_ends() {
        let rows = rows_of(&[1, 2, 3]);
        let mut cur = Some(1);
        cur = move_selection(&rows, cur, 1);
        assert_eq!(cur, Some(2));
        cur = move_selection(&rows, cur, 1);
        assert_eq!(cur, Some(3));
        assert_eq!(move_selection(&rows, cur, 1), Some(3), "端では止まる");
        assert_eq!(move_selection(&rows, Some(1), -1), Some(1));
        assert_eq!(move_selection(&[], None, 1), None);
        assert_eq!(move_selection(&rows, None, 0), None);
    }

    /// 素の文字キーは**何にも割り当てない** (絞り込みへ流すため)。
    #[test]
    fn bare_letters_go_to_the_filter_and_lifecycle_lives_on_alt() {
        for k in [egui::Key::D, egui::Key::X, egui::Key::N, egui::Key::J] {
            assert_eq!(key_intent(k, false, false), None, "{k:?} は絞り込みへ");
        }
        assert_eq!(
            key_intent(egui::Key::ArrowDown, false, false),
            Some(DeckKey::Down)
        );
        assert_eq!(
            key_intent(egui::Key::Enter, false, false),
            Some(DeckKey::Enter)
        );
        assert_eq!(
            key_intent(egui::Key::D, true, false),
            Some(DeckKey::Duplicate)
        );
        assert_eq!(key_intent(egui::Key::X, true, false), Some(DeckKey::Stop));
        assert_eq!(
            key_intent(egui::Key::S, true, false),
            Some(DeckKey::Restart)
        );
        assert_eq!(key_intent(egui::Key::R, true, false), Some(DeckKey::Rename));
        assert_eq!(key_intent(egui::Key::N, true, false), Some(DeckKey::New));
        assert_eq!(
            key_intent(egui::Key::ArrowUp, true, false),
            Some(DeckKey::MoveUp)
        );
        // ⌘ が乗っていたらデッキは手を出さない (アプリのショートカットが先)
        assert_eq!(key_intent(egui::Key::N, false, true), None);
        assert_eq!(key_intent(egui::Key::ArrowUp, true, true), None);
    }

    /// 実際の起動・停止を一切しない記録専用のディスパッチャ。
    #[derive(Default)]
    struct FakeDispatcher {
        seen: Vec<DeckAction>,
    }

    impl FakeDispatcher {
        fn feed(
            &mut self,
            st: &mut DeckState,
            k: DeckKey,
            rows: &[Row],
            live: &[LiveRow],
            launchers: &[LauncherRow],
        ) {
            let row = st
                .selected
                .and_then(|id| rows.iter().find(|r| r.id == id))
                .copied();
            let intents = dispatch(k, rows, row, live, launchers, st.stop_armed);
            for it in intents {
                st.apply_intent(it, rows, &mut self.seen);
            }
        }
    }

    #[test]
    fn lifecycle_keys_emit_intents_without_launching() {
        let l = vec![live(0, 7, "claude"), live(1, 8, "codex")];
        let n = vec![launcher(0, "Claude"), launcher(1, "Codex")];
        let rows = build_rows(&l, "");
        let mut st = DeckState::default();
        st.select(7);
        let mut d = FakeDispatcher::default();

        // ⌥N = 新規 (先頭プリセット。一覧にプリセット行は出さない)
        d.feed(&mut st, DeckKey::New, &rows, &l, &n);
        assert_eq!(d.seen.last(), Some(&DeckAction::Launch(0)));

        d.feed(&mut st, DeckKey::Duplicate, &rows, &l, &n);
        assert_eq!(d.seen.last(), Some(&DeckAction::Duplicate(0)));

        d.feed(&mut st, DeckKey::Restart, &rows, &l, &n);
        assert_eq!(d.seen.last(), Some(&DeckAction::Restart(0)));

        // Enter = アクティブ切替 + 端末へフォーカス
        d.feed(&mut st, DeckKey::Enter, &rows, &l, &n);
        assert_eq!(d.seen.last(), Some(&DeckAction::Select(0)));
        assert!(st.focus_term_req);

        // ⌥R = 名前変更 (副作用は出さず、入力欄が開くだけ)
        let before = d.seen.len();
        d.feed(&mut st, DeckKey::Rename, &rows, &l, &n);
        assert_eq!(d.seen.len(), before, "⌥R 単体では何も実行しない");
        assert_eq!(st.rename_for, Some(7));
        st.rename_for = None;

        // ⌥X = 停止 (1 打目は確認、2 打目で実行)
        let before = d.seen.len();
        d.feed(&mut st, DeckKey::Stop, &rows, &l, &n);
        assert_eq!(d.seen.len(), before, "1 打目では止めない");
        assert_eq!(st.stop_armed(), Some(7));
        d.feed(&mut st, DeckKey::Stop, &rows, &l, &n);
        assert_eq!(d.seen.last(), Some(&DeckAction::Stop(0)));
        assert_eq!(st.stop_armed(), None, "実行したら確認は下ろす");

        // 選択が動いたら確認は取り下げる
        d.feed(&mut st, DeckKey::Stop, &rows, &l, &n);
        assert_eq!(st.stop_armed(), Some(7));
        d.feed(&mut st, DeckKey::Down, &rows, &l, &n);
        assert_eq!(st.stop_armed(), None);
        assert_eq!(st.selected(), Some(8));
    }

    #[test]
    fn alt_arrows_reorder_by_screen_order() {
        let l = vec![live(0, 1, "a"), live(1, 2, "b"), live(2, 3, "c")];
        let rows = build_rows(&l, "");
        let mut st = DeckState::default();
        let mut d = FakeDispatcher::default();

        st.select(2);
        d.feed(&mut st, DeckKey::MoveUp, &rows, &l, &[]);
        assert_eq!(d.seen.last(), Some(&DeckAction::Reorder { from: 1, to: 0 }));
        d.feed(&mut st, DeckKey::MoveDown, &rows, &l, &[]);
        assert_eq!(d.seen.last(), Some(&DeckAction::Reorder { from: 1, to: 2 }));

        // 端では何も出さない
        let before = d.seen.len();
        st.select(1);
        d.feed(&mut st, DeckKey::MoveUp, &rows, &l, &[]);
        assert_eq!(d.seen.len(), before);
        st.select(3);
        d.feed(&mut st, DeckKey::MoveDown, &rows, &l, &[]);
        assert_eq!(d.seen.len(), before);
    }

    /// プリセットが 1 つも無ければ ⌥N は何も起こさない (落ちない)。
    #[test]
    fn new_agent_without_presets_is_a_noop() {
        let l = vec![live(0, 1, "a")];
        let rows = build_rows(&l, "");
        let mut st = DeckState::default();
        let mut d = FakeDispatcher::default();
        st.select(1);
        d.feed(&mut st, DeckKey::New, &rows, &l, &[]);
        assert!(d.seen.is_empty());
    }

    // ── レイアウトと寸法 ───────────────────────────────────

    #[test]
    fn layout_toggles_and_round_trips_through_u8() {
        assert_eq!(DeckLayout::default(), DeckLayout::Split);
        for l in [DeckLayout::Single, DeckLayout::Split, DeckLayout::Stacked] {
            assert_eq!(DeckLayout::from_u8(l.to_u8()), l);
        }
        assert_eq!(
            DeckLayout::from_u8(99),
            DeckLayout::Split,
            "壊れた値は既定へ"
        );
        // レールの出し入れは積み上げを壊さない
        assert_eq!(DeckLayout::Stacked.with_rail(false), DeckLayout::Single);
        assert_eq!(DeckLayout::Stacked.with_rail(true), DeckLayout::Stacked);
        assert_eq!(DeckLayout::Single.with_rail(true), DeckLayout::Split);
        assert_eq!(DeckLayout::Split.with_stacked(true), DeckLayout::Stacked);
        assert_eq!(DeckLayout::Stacked.with_stacked(false), DeckLayout::Split);
        assert!(!DeckLayout::Single.shows_rail(true));
        assert!(DeckLayout::Split.shows_rail(true));
        assert!(!DeckLayout::Split.shows_rail(false));
    }

    /// レール幅はどの窓幅・どの DPI でも「細いが読める」範囲に収まる。
    #[test]
    fn rail_width_is_clamped_across_window_sizes() {
        for unit in [12.0_f32, 18.0, 32.0] {
            for w in [320.0_f32, 800.0, 1440.0, 2560.0, 3840.0] {
                let r = rail_width(w, RAIL_FRAC_DEFAULT, unit);
                assert!(r > 0.0, "w={w} unit={unit}");
                assert!(
                    r <= w * RAIL_FRAC_MAX + 0.01,
                    "レールが窓の {RAIL_FRAC_MAX} を超えた (w={w})"
                );
                assert!(r <= w * 0.5, "レールが窓の半分を超えた (w={w})");
            }
            // 広い窓では割合どおり
            assert!((rail_width(3000.0, 0.2, unit) - 600.0).abs() < 0.01);
            // 狭い窓でも上限は必ず守る
            assert!(rail_width(200.0, RAIL_FRAC_DEFAULT, unit) <= 200.0 * RAIL_FRAC_MAX + 0.01);
            // 壊れた割合でも落ちない
            assert!(rail_width(1000.0, -5.0, unit) >= 0.0);
            assert!(rail_width(1000.0, 9.0, unit) <= 1000.0 * RAIL_FRAC_MAX + 0.01);
        }
        // 単位 (文字の高さ) が大きいほど下限も大きい = DPI に追従する
        assert!(rail_width(1000.0, 0.01, 32.0) > rail_width(1000.0, 0.01, 12.0));
        // 細い窓ではレールを横に置かない
        assert!(!rail_fits_beside(400.0, 18.0));
        assert!(rail_fits_beside(1440.0, 18.0));
    }

    #[test]
    fn stack_count_is_bounded() {
        assert_eq!(clamp_stack(0), MIN_STACK);
        assert_eq!(clamp_stack(3), 3);
        assert_eq!(clamp_stack(99), MAX_STACK);
        let mut st = DeckState::default();
        assert_eq!(st.stack(), MIN_STACK);
        st.set_stack(4);
        assert_eq!(st.stack(), 4);
        st.set_stack(usize::MAX);
        assert_eq!(st.stack(), MAX_STACK);
        st.set_stack(0);
        assert_eq!(st.stack(), MIN_STACK);
    }

    #[test]
    fn stacked_ids_start_at_the_selection_and_wrap() {
        let rows = rows_of(&[1, 2, 3]);
        assert_eq!(stacked_ids(&rows, Some(2), 2), vec![2, 3]);
        assert_eq!(stacked_ids(&rows, Some(2), 3), vec![2, 3, 1]);
        // 欲しい数がセッション数を超えても重複させない
        assert_eq!(stacked_ids(&rows, Some(2), 9), vec![2, 3, 1]);
        // 行が無ければ空
        assert!(stacked_ids(&[], Some(2), 3).is_empty());
        // 選択が消えていても先頭から出す
        assert_eq!(stacked_ids(&rows, Some(99), 2), vec![1, 2]);
    }

    #[test]
    fn stack_weights_are_fitted_and_bounded() {
        let mut w = vec![];
        fit_weights(&mut w, 3);
        assert_eq!(w.len(), 3);
        assert!(w.iter().all(|v| *v > 0.0));
        fit_weights(&mut w, 2);
        assert_eq!(w.len(), 2);
        let before = w.clone();
        // 小さすぎる分割は拒否する (ペインが潰れない)
        adjust_weights(&mut w, 0, -1.0);
        assert_eq!(w, before);
        adjust_weights(&mut w, 0, 0.1);
        assert!(w[0] > before[0] && w[1] < before[1]);
        // 最後のバーは存在しない
        let before = w.clone();
        adjust_weights(&mut w, 1, 0.1);
        assert_eq!(w, before);
    }

    /// 選択行 (accent のべた塗り) の文字は、どのテーマでも読める側が選ばれる。
    #[test]
    fn selected_row_text_contrasts_with_the_accent_fill() {
        for t in crate::theme::all() {
            let fg = on_accent(&t);
            assert!(fg == t.text || fg == t.bg);
            let d = (luma(fg) - luma(t.accent)).abs();
            let other = if fg == t.text { t.bg } else { t.text };
            assert!(
                d >= (luma(other) - luma(t.accent)).abs(),
                "{}: 明度差の小さい方を選んでいる",
                t.name
            );
            assert!(d > 0.1, "{}: 選択行の文字が読めない", t.name);
        }
    }

    // ── 再描画のリズム ──────────────────────────────────────

    /// 回帰テスト: 誰も出力していなければデッキは**1 枚も予約しない**。
    /// ここが Some に戻るとデッキを開いているだけで CPU を食う。
    #[test]
    fn idle_deck_asks_for_no_repaint() {
        assert_eq!(deck_repaint_ms(false, false, false), None);
    }

    #[test]
    fn repaint_cadence_follows_activity() {
        assert_eq!(
            deck_repaint_ms(true, true, false),
            Some(crate::kanban::FAST_SAMPLE_MS)
        );
        assert_eq!(
            deck_repaint_ms(false, true, false),
            Some(crate::kanban::SLOW_SAMPLE_MS)
        );
        // 裏の問い合わせ中だけは短く回して、届いたら止まる
        assert_eq!(deck_repaint_ms(false, false, true), Some(SCAN_POLL_MS));
        // 出力があるときは走っている扱いより優先
        assert_eq!(
            deck_repaint_ms(true, false, true),
            Some(crate::kanban::FAST_SAMPLE_MS)
        );
    }

    /// 何も出力しておらず、誰も走っていないフレームは 0 枚。
    ///
    /// **判定は `FleetStore` が持つ**ので、デッキはそのスナップショットを
    /// `deck_repaint_ms` へ渡すだけ。以前はデッキが自前の追跡を持っていて、
    /// しかもラダー無しの判定で回していた (看板と食い違う原因だった)。
    #[test]
    fn a_deck_with_nothing_producing_output_schedules_nothing() {
        let mut fleet = crate::fleet::FleetStore::default();
        fleet.update(
            &[crate::fleet::Observation {
                id: 1,
                kind: crate::fleet::model::AgentKindOpt::pty(),
                title: "a".into(),
                running: false,
                tail_lines: Some(Vec::new()),
                ..Default::default()
            }],
            10_000,
        );
        let snap = fleet.snapshot();
        assert!(!snap.any_running);
        assert!(!snap.busy);
        assert_eq!(deck_repaint_ms(snap.busy, snap.any_running, false), None);
    }

    /// 走っているセッションがあれば「起きている」刻みになる。
    #[test]
    fn a_running_session_keeps_the_deck_awake() {
        let mut fleet = crate::fleet::FleetStore::default();
        fleet.update(
            &[crate::fleet::Observation {
                id: 1,
                kind: crate::fleet::model::AgentKindOpt::pty(),
                title: "a".into(),
                running: true,
                sup: Some(crate::supervisor::SessionState::Idle),
                tail_lines: Some(Vec::new()),
                ..Default::default()
            }],
            0,
        );
        let snap = fleet.snapshot();
        assert!(snap.any_running);
        assert_eq!(
            deck_repaint_ms(snap.busy, snap.any_running, false),
            Some(crate::kanban::SLOW_SAMPLE_MS)
        );
    }

    // ── 表示ヘルパ ──────────────────────────────────────────

    #[test]
    fn short_path_uses_home_and_keeps_the_tail() {
        let home = PathBuf::from("/Users/me");
        assert_eq!(short_path(Path::new("/Users/me"), Some(&home)), "~");
        assert_eq!(short_path(Path::new("/Users/me/dev"), Some(&home)), "~/dev");
        assert_eq!(
            short_path(Path::new("/Users/me/dev/a/b/c"), Some(&home)),
            "…/b/c"
        );
        assert_eq!(short_path(Path::new("/tmp"), None), "/tmp");
    }

    #[test]
    fn deck_state_selection_defaults_to_the_active_session() {
        let mut l = vec![live(0, 1, "a"), live(1, 2, "b")];
        l[1].active = true;
        let rows = build_rows(&l, "");
        let mut st = DeckState::default();
        assert_eq!(st.sync_selection(&rows, &l), Some(2));
    }

    // ── フォーカスの調停 ─────────────────────────────────────────

    /// 名前変更欄に打っている間、デッキはキーを 1 つも見ない。
    /// (書いている最中に「j」でレールが動く / 文字が絞り込みへ流れる、を防ぐ)
    #[test]
    fn 名前変更欄に書いている間はデッキがキーを見ない() {
        let term = egui::Id::new("term-1");
        let rename = egui::Id::new("deck-rename-1");
        let terms = [term];

        // 名前変更欄にフォーカス → デッキは絞り込みも移動もしない
        let owner = key_owner(Some(rename), &terms);
        assert_eq!(owner, KeyOwner::TextInput);
        assert!(!owner.deck_filters(), "打鍵が絞り込みへ漏れている");
        assert!(!owner.deck_navigates(), "↑↓ がレールへ漏れている");

        // 実際に「a」を打っても絞り込みは空のまま / 選択も動かない
        let rows = rows_of(&[1, 2, 3]);
        let mut query = String::new();
        let mut sel = Some(2);
        if owner.deck_filters() {
            type_into_filter(&mut query, "a");
        }
        if owner.deck_navigates() {
            sel = move_selection(&rows, sel, 1);
        }
        assert_eq!(query, "", "名前変更の入力が絞り込みへ流れた");
        assert_eq!(sel, Some(2), "名前変更の入力でレールが動いた");

        // フォーカスが外れたら元どおり効く
        let owner = key_owner(None, &terms);
        assert_eq!(owner, KeyOwner::Deck);
        if owner.deck_filters() {
            type_into_filter(&mut query, "a");
        }
        if owner.deck_navigates() {
            sel = move_selection(&rows, sel, 1);
        }
        assert_eq!(query, "a");
        assert_eq!(sel, Some(3));

        // 端末にフォーカスがあるときは打鍵をデッキが一切見ない
        // (= そのまま PTY へ流れる。Esc/⇧Tab だけ横取りして一覧へ返す)
        let term_owner = key_owner(Some(term), &terms);
        assert_eq!(term_owner, KeyOwner::Terminal);
        assert!(!term_owner.deck_filters(), "端末への打鍵が絞り込みへ漏れた");
        assert!(!term_owner.deck_navigates(), "端末への打鍵がレールへ漏れた");
    }

    /// **端末はヘッダー以外の全高**を使う (取り分ける帯を作らない)。
    #[test]
    fn 端末はヘッダーを除く全高を使う() {
        // 窓が高くても低くても、引かれるのはヘッダーの分だけ
        let unit = 16.0;
        let head = unit * 1.7;
        for h in [700.0_f32, 300.0, 1200.0] {
            assert_eq!(
                pane_body_h(h, head, unit),
                h - head,
                "端末以外に帯を取り分けている (h={h})"
            );
        }
        // 極端に低い窓でも 3 行は残す (負の高さを egui へ渡さない)
        assert_eq!(pane_body_h(10.0, head, unit), unit * 3.0);
    }

    /// **看板・サイドバーとの被りを二度と作らないための番人。**
    /// デッキの描画にダッシュボード部品や「他の画面の仕事」が戻ってきたら落ちる。
    #[test]
    fn the_deck_renders_only_live_agents_and_nothing_status_like() {
        let src = &include_str!("deck.rs").replace("\r\n", "\n");
        // テストの中の語 (この関数自身) は除き、さらに**コメントも外して**
        // 実コードだけを見る (「サイドバーの担当」と書いた説明文で落ちないように)。
        let body: String = src
            .split("mod tests {")
            .next()
            .expect("本体がある")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for (banned, why) in [
            ("section_header_ui", "区画見出し"),
            ("chips_ui", "フィルタチップ"),
            ("pulse_ui", "出力スパークライン"),
            ("fmt_elapsed", "経過時間"),
            ("approvals", "承認バッジ"),
            ("activity_color", "状態の色分け"),
            // **入力欄そのものを禁じる**。デッキは cmux と同じで
            // 「打った字はそのまま端末へ」= ペイン全体がエージェント。
            // 下端に入力欄を置くと端末の高さを削り、二重の入力口ができる
            // (指示欄は Cockpit の担当)。
            ("agent_composer_ui", "下端の入力欄 (Cockpit の担当)"),
            ("agent_composer_inline_ui", "1 行の入力欄 (Cockpit の担当)"),
            ("ComposerScope", "コンポーザのスコープ"),
            ("AgentInputBuffer", "入力欄の下書き入れ"),
            ("composer_target_chips", "宛先チップ行 (Cockpit の担当)"),
            ("PastSession", "過去セッション行 (サイドバーの担当)"),
            ("session_picker", "過去セッションの走査"),
            ("再開", "再開導線 (サイドバーの担当)"),
        ] {
            assert!(
                !body.contains(banned),
                "デッキに {why} ({banned}) が戻っている — レールは稼働中エージェントだけ"
            );
        }
        // 逆向きの番人: 端末へ渡す高さはヘッダーを引くだけ (帯を取り分けない)。
        assert!(
            body.contains("pane_body_h(height, head_h, unit)"),
            "端末の高さが「全高 − ヘッダー」でなくなっている"
        );
    }
}
