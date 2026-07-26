//! エージェントデッキ — 縦 1 本のリストでエージェントを管理する、キーボード優先の画面。
//!
//! cmux の「左に縦一列のセッション、右にその端末」という操作感を、
//! Zaivern の材料 (稼働中セッション / ローカルに残っている過去の会話 /
//! 起動プリセット) の**全部**を 1 本のリストに並べる形で作り直したもの。
//!
//! ## 既存の 2 画面との違い
//! - **Cockpit** はライブタイルの**グリッド**。全員を同時に眺める画面で、
//!   1 体を深く触るには向かない。デッキは常に「1 本を選んで、その端末を全高で見る」。
//! - **フリート看板 (kanban.rs)** は状態レーンの**ボード**。「誰がどの状態か」を
//!   俯瞰する画面で、過去の会話も起動導線も持たない。デッキは状態で並べ替えず、
//!   稼働中 → ローカルのセッション → 新規 の 3 セクションを縦に固定して並べ、
//!   「再開する / 起動する / 名前を変える / 複製する / 止める」までを鍵盤だけで回す。
//!
//! ## 構成
//! ```text
//! ┌ ヘッダー: 稼働中 N / 承認待ち M ・上限の助言 ・レイアウト切替 ────────┐
//! │ フィルタチップ (すべて/作業中/待機/要対応) + 絞り込み欄               │
//! ├────────────┬────────────────────────────────────────────────────────┤
//! │ ▾ 稼働中    │  選択中セッションのライブ端末 (terminal::draw)          │
//! │ ▾ ローカル  │  ─ 積み上げモードでは複数セッションを上下に並べる ─      │
//! │ ▾ 新規      │  下端: そのエージェント宛ての複数行コンポーザ           │
//! └────────────┴────────────────────────────────────────────────────────┘
//! ```
//!
//! ## 負荷 (アイドルで 1 枚も描かない)
//! - PTY 画面の読み直しは [`DeckState::sample_due`] が真のフレームだけ
//!   (kanban と同じ適応周期: 動いていれば ~6.7Hz / 静かなら 1Hz)。
//! - 再描画要求は [`deck_repaint_ms`] が決める。**誰も出力していなければ
//!   `None` = 1 枚も予約しない** — app.rs の `schedule_idle_repaint` に判断を返す。
//!   ここが `Some` に固定されるとアイドル時の CPU が跳ねる (回帰テストあり)。
//!
//! 作法は kanban.rs / orchestration.rs と同じ: 判断と描画はこのモジュール、
//! 副作用 (起動・再開・停止・PTY への書き込み) は [`DeckAction`] で app.rs へ返す。
//! Session を直接借りない (app.rs が [`LiveRow`] へ写して渡す)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, RichText, Stroke};

use crate::i18n::{tr, trf};
use crate::kanban::{self, Activity, Source};
use crate::session_picker::PastSession;
use crate::supervisor;
use crate::theme::Theme;

/// デッキ画面の記号 (パレット・メニュー・ヘッダーで共通に使う)。
/// フォントに glyph がある字だけを使う (app.rs の `ui_symbols_have_glyphs` 参照)。
pub const DECK_ICON: &str = "📇";

// ---------------------------------------------------------------------------
// リストの構成要素
// ---------------------------------------------------------------------------

/// 縦リストの区画。表示順はこの並び (上から順に固定)。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Section {
    /// いま走っている (または枠だけ残っている) セッション
    Live,
    /// ディスクに残っている過去の会話 (再開できる)
    Past,
    /// 起動プリセット (1 打鍵で新しいエージェントを起こす)
    New,
}

/// 表示順。
pub const SECTIONS: [Section; 3] = [Section::Live, Section::Past, Section::New];

impl Section {
    /// 折りたたみ配列の添字。
    pub fn ix(self) -> usize {
        match self {
            Section::Live => 0,
            Section::Past => 1,
            Section::New => 2,
        }
    }

    /// 見出し (tr のキーになる日本語原文)。
    pub fn title(self) -> &'static str {
        match self {
            Section::Live => "稼働中",
            Section::Past => "ローカルのセッション",
            Section::New => "新規",
        }
    }

    /// 見出しの記号。
    pub fn icon(self) -> &'static str {
        match self {
            Section::Live => "●",
            Section::Past => "💬",
            Section::New => "➕",
        }
    }

    /// 見出しのホバー説明 (tr のキー)。
    pub fn hint(self) -> &'static str {
        match self {
            Section::Live => "いま動いているセッション — Enter でその端末へ入ります",
            Section::Past => "この PC に残っている過去の会話 — Enter で再開します",
            Section::New => "起動プリセット — Enter で新しいエージェントを起こします",
        }
    }
}

/// 行の同一性。**index ではなく中身**で持つので、リストが増減しても選択が飛ばない。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum RowKey {
    /// 稼働中セッション (セッション ID)
    Live(u64),
    /// 過去の会話 (実行ファイル名, 会話 ID)
    Past(String, String),
    /// 起動プリセット (プリセット index)
    Launcher(usize),
}

impl RowKey {
    /// 永続メモリへ書ける文字列表現 (egui の memory は型付きなので 1 本の String にする)。
    pub fn to_persist(&self) -> String {
        match self {
            RowKey::Live(id) => format!("L\u{1}{id}"),
            RowKey::Past(bin, id) => format!("P\u{1}{bin}\u{1}{id}"),
            RowKey::Launcher(i) => format!("N\u{1}{i}"),
        }
    }

    /// [`RowKey::to_persist`] の逆。壊れた文字列は `None` (選択なしに戻すだけ)。
    pub fn from_persist(s: &str) -> Option<RowKey> {
        let mut it = s.split('\u{1}');
        match it.next()? {
            "L" => it.next()?.parse().ok().map(RowKey::Live),
            "P" => {
                let bin = it.next()?.to_string();
                let id = it.next()?.to_string();
                Some(RowKey::Past(bin, id))
            }
            "N" => it.next()?.parse().ok().map(RowKey::Launcher),
            _ => None,
        }
    }

    /// 稼働中セッションの ID (それ以外は `None`)。
    pub fn live_id(&self) -> Option<u64> {
        match self {
            RowKey::Live(id) => Some(*id),
            _ => None,
        }
    }
}

/// 稼働中セッション 1 本のスナップショット (app.rs が毎フレーム写す)。
#[derive(Clone, Debug, Default)]
pub struct LiveRow {
    /// `AgentManager.sessions` の index (**このフレーム内でのみ**有効)
    pub idx: usize,
    pub id: u64,
    pub icon: String,
    pub title: String,
    pub cwd: PathBuf,
    /// 起動に使ったコマンド。過去の会話との重複判定 (`--resume <id>`) に使う。
    pub command: String,
    pub running: bool,
    pub attention: bool,
    pub unread: bool,
    pub rate_limited: bool,
    /// このセッション宛ての承認待ち件数 (`agents.approvals`)
    pub approvals: usize,
    /// 見張り (supervisor.rs) の判定
    pub sup: Option<supervisor::SessionState>,
    /// アクティブ (紫枠) のセッションか
    pub active: bool,
    /// 連続稼働時間の表示 (`Session::uptime`)
    pub uptime: String,
    /// 画面末尾の意味のある行。**サンプリングしたフレームだけ**中身が入る。
    pub tail_lines: Vec<String>,
}

/// 過去の会話 1 本。
#[derive(Clone, Debug)]
pub struct PastRow {
    pub session: PastSession,
    /// 一覧に出す相対時刻 (`session_picker::relative_age` の結果)
    pub age: String,
    /// エージェントの印 (アイコン or 実行ファイル名の頭文字)
    pub mark: String,
}

/// 起動プリセット 1 本。
#[derive(Clone, Debug)]
pub struct LauncherRow {
    /// `cfg.agents` の index
    pub idx: usize,
    pub icon: String,
    pub name: String,
}

/// 画面に実際に並ぶ 1 行。詳細は `idx` で元のスライスを引く。
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub key: RowKey,
    pub section: Section,
    /// Live: `live[idx]` / Past: `past[idx]` / New: `launchers[idx]`
    pub idx: usize,
}

// ---------------------------------------------------------------------------
// フィルタ
// ---------------------------------------------------------------------------

/// ヘッダーのフィルタチップ。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Chip {
    /// 全部 (3 セクションとも出す)
    #[default]
    All,
    /// 出力が動いているセッションだけ
    Working,
    /// 生きているが動いていないセッションだけ
    Waiting,
    /// 承認待ち・停滞・レート制限・未読の注意マークが付くものだけ
    Attention,
}

/// チップの表示順。
pub const CHIPS: [Chip; 4] = [Chip::All, Chip::Working, Chip::Waiting, Chip::Attention];

impl Chip {
    /// チップのラベル (tr のキー)。
    pub fn label(self) -> &'static str {
        match self {
            Chip::All => "すべて",
            Chip::Working => "作業中",
            Chip::Waiting => "待機",
            Chip::Attention => "要対応",
        }
    }

    /// 稼働中セクションの 1 行がこのチップに残るか (純関数)。
    pub fn keeps(self, a: Activity, row: &LiveRow) -> bool {
        match self {
            Chip::All => true,
            Chip::Working => a.is_busy(),
            Chip::Waiting => matches!(a, Activity::Idle | Activity::Starting),
            Chip::Attention => {
                row.attention
                    || row.approvals > 0
                    || matches!(
                        a,
                        Activity::Approval | Activity::Stalled | Activity::RateLimited
                    )
            }
        }
    }

    /// 「すべて」以外は**稼働中セクションだけ**に絞る。
    /// 過去の会話も起動プリセットも「作業中/待機/要対応」を持たないので、
    /// 混ぜて出すとチップの意味が壊れる。
    pub fn live_only(self) -> bool {
        self != Chip::All
    }
}

/// リスト構築に効く表示条件。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    pub chip: Chip,
    /// 打ち込み絞り込み (空なら無効)。大文字小文字は区別しない。
    pub query: String,
    /// セクションごとの折りたたみ ([`Section::ix`] の順)
    pub collapsed: [bool; 3],
}

/// 絞り込み語がどれかの欄に含まれるか (純関数・大文字小文字を無視)。
pub fn matches_query(query: &str, fields: &[&str]) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    fields.iter().any(|f| f.to_lowercase().contains(&q))
}

/// 過去の会話が「いま稼働中のセッションとして開かれている」か (純関数)。
///
/// 再開起動のコマンドには会話 ID がそのまま入る (`--resume <uuid>` 等) ので、
/// それを唯一の根拠にする。cwd の一致だけで消すと、同じフォルダの別の会話まで
/// 一覧から消えてしまう。
pub fn is_open_live(past_id: &str, live: &[LiveRow]) -> bool {
    if past_id.trim().is_empty() {
        return false;
    }
    live.iter()
        .any(|l| l.running && l.command.contains(past_id))
}

/// 3 セクションぶんの行を 1 本の縦リストへ組み立てる **純関数**。
///
/// - 稼働中: 渡された順 (app.rs = セッションの並び順) をそのまま保つ
/// - ローカルのセッション: 渡された順 (新しい順)。稼働中と重複するものは落とす
/// - 新規: プリセットの順
/// - 折りたたみ中のセクションは 0 行 (見出しだけ UI 側が描く)
pub fn build_rows(
    live: &[LiveRow],
    acts: &[Activity],
    past: &[PastRow],
    launchers: &[LauncherRow],
    f: &Filter,
) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::new();

    if !f.collapsed[Section::Live.ix()] {
        for (i, l) in live.iter().enumerate() {
            let a = acts.get(i).copied().unwrap_or(Activity::Starting);
            if !f.chip.keeps(a, l) {
                continue;
            }
            let cwd = l.cwd.to_string_lossy();
            if !matches_query(&f.query, &[&l.title, &cwd, &l.icon]) {
                continue;
            }
            out.push(Row {
                key: RowKey::Live(l.id),
                section: Section::Live,
                idx: i,
            });
        }
    }

    if !f.chip.live_only() && !f.collapsed[Section::Past.ix()] {
        for (i, p) in past.iter().enumerate() {
            if is_open_live(&p.session.id, live) {
                continue;
            }
            let cwd = p.session.cwd.to_string_lossy();
            if !matches_query(
                &f.query,
                &[&p.session.summary, &cwd, &p.session.agent_bin],
            ) {
                continue;
            }
            out.push(Row {
                key: RowKey::Past(p.session.agent_bin.clone(), p.session.id.clone()),
                section: Section::Past,
                idx: i,
            });
        }
    }

    if !f.chip.live_only() && !f.collapsed[Section::New.ix()] {
        for (i, n) in launchers.iter().enumerate() {
            if !matches_query(&f.query, &[&n.name, &n.icon]) {
                continue;
            }
            out.push(Row {
                key: RowKey::Launcher(n.idx),
                section: Section::New,
                idx: i,
            });
        }
    }

    out
}

/// 見出しに出す件数 (折りたたみを無視した、フィルタ適用後の件数)。
pub fn section_counts(
    live: &[LiveRow],
    acts: &[Activity],
    past: &[PastRow],
    launchers: &[LauncherRow],
    f: &Filter,
) -> [usize; 3] {
    let open = Filter {
        collapsed: [false; 3],
        ..f.clone()
    };
    let rows = build_rows(live, acts, past, launchers, &open);
    let mut n = [0usize; 3];
    for r in &rows {
        n[r.section.ix()] += 1;
    }
    n
}

// ---------------------------------------------------------------------------
// 選択とキーボード移動
// ---------------------------------------------------------------------------

/// 選択をいまのリストへ解決する **純関数**。
///
/// - キーがそのまま残っていれば、その位置を返す (並べ替え・挿入で飛ばない)
/// - 消えていれば、最後に居た位置へ寄せる (末尾を超えたら末尾)
/// - リストが空なら `None`
pub fn resolve_selection(
    sel: Option<&RowKey>,
    last_pos: usize,
    rows: &[Row],
) -> Option<(RowKey, usize)> {
    if rows.is_empty() {
        return None;
    }
    if let Some(k) = sel {
        if let Some(pos) = rows.iter().position(|r| &r.key == k) {
            return Some((k.clone(), pos));
        }
    }
    let pos = last_pos.min(rows.len() - 1);
    Some((rows[pos].key.clone(), pos))
}

/// 上下移動 **純関数**。セクションの境目は素通りする (リストが 1 本だから)。
/// 端では止まる (巻き戻さない — 長いリストで迷子になるため)。
pub fn move_selection(rows: &[Row], cur: Option<&RowKey>, delta: i32) -> Option<RowKey> {
    if rows.is_empty() || delta == 0 {
        return None;
    }
    let at = cur
        .and_then(|k| rows.iter().position(|r| &r.key == k))
        .map(|p| p as i32);
    let next = match at {
        Some(p) => (p + delta).clamp(0, rows.len() as i32 - 1),
        // 未選択なら、下キーで先頭 / 上キーで末尾から入る
        None if delta > 0 => 0,
        None => rows.len() as i32 - 1,
    };
    rows.get(next as usize).map(|r| r.key.clone())
}

// ---------------------------------------------------------------------------
// レイアウト
// ---------------------------------------------------------------------------

/// 端末ペインの見せ方。Cockpit (グリッド) と看板 (ボード) に無い「使い分け」。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DeckLayout {
    /// 選択中の 1 本だけを全高で出す
    #[default]
    Single,
    /// 左レール + 右に 1 本 (既定の 2 分割はレールの有無で決まるので、
    /// ここでは「レールを常に出す」意味になる)
    Split,
    /// 選択の前後を上下に積み上げる (cmux の複数ペイン)
    Stacked,
}

/// 積み上げモードで同時に出せるセッション数の下限・上限。
pub const MIN_STACK: usize = 2;
pub const MAX_STACK: usize = 6;

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
            1 => DeckLayout::Split,
            2 => DeckLayout::Stacked,
            _ => DeckLayout::Single,
        }
    }

    /// 次のレイアウトへ (1画面 → 2分割 → 積み上げ → 1画面)。
    pub fn next(self) -> Self {
        match self {
            DeckLayout::Single => DeckLayout::Split,
            DeckLayout::Split => DeckLayout::Stacked,
            DeckLayout::Stacked => DeckLayout::Single,
        }
    }

    /// ラベル (tr のキー)。
    pub fn label(self) -> &'static str {
        match self {
            DeckLayout::Single => "1画面 (選択のみ)",
            DeckLayout::Split => "2分割",
            DeckLayout::Stacked => "積み上げ",
        }
    }

    /// この配置で一覧 (レール) を出すか。1 画面モードは端末に全部渡す。
    /// `enabled` は呼び出し側の都合 (窓が狭すぎる等) で畳みたいときの上書き。
    pub fn shows_rail(self, enabled: bool) -> bool {
        match self {
            DeckLayout::Single => false,
            DeckLayout::Split | DeckLayout::Stacked => enabled,
        }
    }
}

/// 積み上げ数を範囲へ収める **純関数**。
pub fn clamp_stack(n: usize) -> usize {
    n.clamp(MIN_STACK, MAX_STACK)
}

/// 積み上げモードで実際に描くセッション ID を決める **純関数**。
///
/// 選択中の行から下へ順に稼働中セッションを拾い、足りなければ先頭から補う。
/// 稼働中セッションが 1 本も無ければ空 (端末を描かない)。
pub fn stacked_ids(rows: &[Row], live: &[LiveRow], sel: Option<&RowKey>, want: usize) -> Vec<u64> {
    let ids: Vec<u64> = rows
        .iter()
        .filter(|r| r.section == Section::Live)
        .filter_map(|r| live.get(r.idx).map(|l| l.id))
        .collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let start = sel
        .and_then(|k| k.live_id())
        .and_then(|id| ids.iter().position(|x| *x == id))
        .unwrap_or(0);
    let n = want.min(ids.len());
    (0..n).map(|i| ids[(start + i) % ids.len()]).collect()
}

/// 積み上げペインの高さ比を、ドラッグ量に合わせて付け替える **純関数**。
///
/// `i` 番目と `i+1` 番目の間のバーを `delta`(全高に対する割合) だけ動かす。
/// どちらのペインも [`MIN_WEIGHT`] より薄くならない。
pub const MIN_WEIGHT: f32 = 0.08;

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
    /// Enter — 稼働中なら端末へ / 過去なら再開 / 新規なら起動
    Enter,
    /// n — 新しいエージェント (選択中プリセット、無ければ先頭)
    New,
    /// r — 名前変更
    Rename,
    /// d — 複製 (同じプリセット + 同じ作業ディレクトリ)
    Duplicate,
    /// x — 停止 (2 打鍵で確定)
    Stop,
    /// s — 再起動
    Restart,
    /// ⌥↑ — 並べ替え (上へ)
    MoveUp,
    /// ⌥↓ — 並べ替え (下へ)
    MoveDown,
}

/// 打鍵 → [`DeckKey`] の対応表 **純関数**。
/// 修飾キー付きは並べ替えだけ (文字キーに修飾が乗っていたら無視する)。
pub fn key_intent(key: egui::Key, alt: bool, cmd: bool) -> Option<DeckKey> {
    if cmd {
        return None;
    }
    if alt {
        return match key {
            egui::Key::ArrowUp => Some(DeckKey::MoveUp),
            egui::Key::ArrowDown => Some(DeckKey::MoveDown),
            _ => None,
        };
    }
    Some(match key {
        egui::Key::ArrowUp | egui::Key::K => DeckKey::Up,
        egui::Key::ArrowDown | egui::Key::J => DeckKey::Down,
        egui::Key::Enter => DeckKey::Enter,
        egui::Key::N => DeckKey::New,
        egui::Key::R => DeckKey::Rename,
        egui::Key::D => DeckKey::Duplicate,
        egui::Key::X => DeckKey::Stop,
        egui::Key::S => DeckKey::Restart,
        _ => return None,
    })
}

/// app.rs へ返す副作用の要求。実行は app.rs (`deck_ui`) 側。
#[derive(Clone, Debug, PartialEq)]
pub enum DeckAction {
    /// アクティブ (紫枠) をこのセッション index へ
    Select(usize),
    /// プリセット index のエージェントを起動
    Launch(usize),
    /// この過去の会話を再開する
    Resume(Box<PastSession>),
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
    /// 選択中セッションへ送信
    Send { id: u64, text: String },
    /// 全エージェントへ送信 (コンポーザの宛先が「全員」のとき)
    Broadcast(String),
    /// デッキを閉じる
    Close,
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
/// `row` は選択中の行。`stop_armed` は「x の 1 打目が入っているセッション」。
pub fn dispatch(
    k: DeckKey,
    rows: &[Row],
    row: Option<&Row>,
    live: &[LiveRow],
    past: &[PastRow],
    launchers: &[LauncherRow],
    stop_armed: Option<u64>,
) -> Vec<Intent> {
    let mut out = Vec::new();
    // 選択が動く操作は、いつでも停止の確認を取り下げる (誤爆防止)
    let live_of = |r: &Row| -> Option<&LiveRow> {
        if r.section == Section::Live {
            live.get(r.idx)
        } else {
            None
        }
    };
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
            let Some(r) = row else { return out };
            match r.section {
                Section::Live => {
                    if let Some(l) = live.get(r.idx) {
                        out.push(Intent::Act(DeckAction::Select(l.idx)));
                    }
                    out.push(Intent::FocusTerminal);
                }
                Section::Past => {
                    if let Some(p) = past.get(r.idx) {
                        out.push(Intent::Act(DeckAction::Resume(Box::new(
                            p.session.clone(),
                        ))));
                    }
                }
                Section::New => {
                    if let Some(n) = launchers.get(r.idx) {
                        out.push(Intent::Act(DeckAction::Launch(n.idx)));
                    }
                }
            }
        }
        DeckKey::New => {
            // 選択が起動プリセットならそれを、そうでなければ先頭のプリセットを起こす
            let pick = row
                .filter(|r| r.section == Section::New)
                .and_then(|r| launchers.get(r.idx))
                .or_else(|| launchers.first());
            if let Some(n) = pick {
                out.push(Intent::Act(DeckAction::Launch(n.idx)));
            }
        }
        DeckKey::Rename => {
            if let Some(l) = row.and_then(live_of) {
                out.push(Intent::BeginRename(l.id));
            }
        }
        DeckKey::Duplicate => {
            if let Some(l) = row.and_then(live_of) {
                out.push(Intent::Act(DeckAction::Duplicate(l.idx)));
            }
        }
        DeckKey::Stop => {
            if let Some(l) = row.and_then(live_of) {
                if stop_armed == Some(l.id) {
                    out.push(Intent::DisarmStop);
                    out.push(Intent::Act(DeckAction::Stop(l.idx)));
                } else {
                    out.push(Intent::ArmStop(l.id));
                }
            }
        }
        DeckKey::Restart => {
            if let Some(l) = row.and_then(live_of) {
                out.push(Intent::Act(DeckAction::Restart(l.idx)));
            }
        }
        DeckKey::MoveUp | DeckKey::MoveDown => {
            let Some(r) = row else { return out };
            let Some(l) = live_of(r) else { return out };
            let up = k == DeckKey::MoveUp;
            // 画面の並び (稼働中セクション内) で隣の行を探し、その実 index と入れ替える
            let live_rows: Vec<&Row> =
                rows.iter().filter(|x| x.section == Section::Live).collect();
            let Some(at) = live_rows.iter().position(|x| x.key == r.key) else {
                return out;
            };
            let to = if up {
                at.checked_sub(1)
            } else {
                (at + 1 < live_rows.len()).then_some(at + 1)
            };
            let Some(to) = to.and_then(|t| live_rows.get(t)) else {
                return out;
            };
            let Some(target) = live.get(to.idx) else {
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

/// 出力が動いているときの PTY 画面サンプリング間隔 (≈6.7Hz)。
const FAST_SAMPLE_MS: u64 = 150;
/// 静かなときのサンプリング間隔 (1Hz)。
const SLOW_SAMPLE_MS: u64 = 1_000;
/// 過去セッションの走査中だけ回す刻み (結果が届いたらすぐ止まる)。
const SCAN_POLL_MS: u64 = 250;
/// 出力の勢いスパークラインの窓とバケツ数。
const PULSE_WINDOW_MS: u64 = 30_000;
const PULSE_BUCKETS: usize = 20;

/// 次に再描画を予約するまでの ms。**`None` なら 1 枚も予約しない**。
///
/// これがデッキの負荷の全て。誰も出力しておらず、走っているエージェントも
/// 無く、走査も飛んでいなければ `None` を返し、判断を app.rs の
/// `schedule_idle_repaint` (= 完全アイドルなら 0 枚) に返す。
pub fn deck_repaint_ms(busy: bool, any_running: bool, scanning: bool) -> Option<u64> {
    if busy {
        return Some(FAST_SAMPLE_MS);
    }
    if any_running {
        return Some(SLOW_SAMPLE_MS);
    }
    if scanning {
        return Some(SCAN_POLL_MS);
    }
    None
}

/// 1 セッションぶんの追跡状態 (アクティビティ・経過・出力の勢い)。
#[derive(Clone, Debug)]
pub struct Track {
    activity: Activity,
    since_ms: u64,
    source: Source,
    detail: String,
    tail: Vec<String>,
    /// 出力の勢い `(時刻, 新規文字数)`
    pulse: Vec<(u64, u64)>,
}

impl Track {
    fn new(a: Activity, source: Source, now_ms: u64) -> Self {
        Self {
            activity: a,
            since_ms: now_ms,
            source,
            detail: String::new(),
            tail: Vec::new(),
            pulse: Vec::new(),
        }
    }

    /// 現在のアクティビティが続いている時間 (ms)。
    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.since_ms)
    }

    /// 直近 30 秒の出力の勢い (古い → 新しい)。
    pub fn pulse_series(&self, now_ms: u64) -> Vec<f32> {
        kanban::bucket_series(&self.pulse, now_ms, PULSE_WINDOW_MS, PULSE_BUCKETS)
    }

    /// 直近 3 秒に新しい出力があったか。
    fn recently_noisy(&self, now_ms: u64) -> bool {
        self.pulse
            .iter()
            .any(|(t, v)| *v > 0 && now_ms.saturating_sub(*t) <= 3_000)
    }

    pub fn activity(&self) -> Activity {
        self.activity
    }

    pub fn source(&self) -> Source {
        self.source
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

// ---------------------------------------------------------------------------
// 画面の状態
// ---------------------------------------------------------------------------

/// デッキ画面の UI 状態 (app.rs が保持する)。
///
/// 永続化は egui の persisted memory に置く。config.rs は他所有なので触らない
/// — **将来 config.toml へ移すべき** (レイアウト・積み上げ数・折りたたみ・選択)。
#[derive(Default)]
pub struct DeckState {
    /// 選択中の行 (中身で持つので並べ替えで飛ばない)
    selected: Option<RowKey>,
    /// 最後に居た位置 (行が消えたときの寄せ先)
    sel_pos: usize,
    /// フィルタ (チップ・絞り込み語・折りたたみ)
    pub filter: Filter,
    /// レイアウト (None = 永続メモリから未読込)
    layout: Option<DeckLayout>,
    /// 積み上げ数 (None = 未読込)
    stack: Option<usize>,
    /// 積み上げペインの高さ比
    stack_weights: Vec<f32>,
    /// 左レールの取り分 (0.18..0.5)
    rail: Option<f32>,
    dirty: bool,
    /// セッション id → 追跡状態
    tracks: HashMap<u64, Track>,
    /// 最後に PTY 画面をサンプルした時刻
    last_sample_ms: Option<u64>,
    /// 直近のサンプルで「動いている」と判定したか
    busy: bool,
    /// 稼働中のセッションが 1 つでもあるか
    any_running: bool,
    /// 名前変更中のセッション
    rename_for: Option<u64>,
    rename_buf: String,
    rename_focus: bool,
    /// 停止の 1 打目が入っているセッション
    stop_armed: Option<u64>,
    /// 絞り込み欄へフォーカスを移す予約
    filter_focus: bool,
    /// 次のフレームで端末へフォーカスを移す
    focus_term_req: bool,
    /// 前フレームに描いた端末の egui Id (Esc を一覧へ返すために覚える)
    term_ids: Vec<egui::Id>,
}

impl DeckState {
    /// **PTY 画面を読み直してよいフレームか。**
    /// これが false のあいだ app.rs は `screen_tail_lines` を呼ばない。
    pub fn sample_due(&mut self, now_ms: u64) -> bool {
        let interval = if self.busy {
            FAST_SAMPLE_MS
        } else {
            SLOW_SAMPLE_MS
        };
        match self.last_sample_ms {
            Some(last) if now_ms.saturating_sub(last) < interval => false,
            _ => {
                self.last_sample_ms = Some(now_ms);
                true
            }
        }
    }

    /// 追跡状態を 1 ステップ進め、行ごとのアクティビティを返す。
    /// `fresh` が true のフレームだけ `live[..].tail_lines` に新しい画面が入っている。
    pub fn update_tracks(&mut self, live: &[LiveRow], now_ms: u64, fresh: bool) -> Vec<Activity> {
        let mut acts = Vec::with_capacity(live.len());
        let mut busy = false;
        let mut any_running = false;
        for l in live {
            if l.running {
                any_running = true;
            }
            // 画面が来ていないフレームは前回サンプルした画面で判定する
            // (生死・承認・レート制限といった構造化信号は毎フレーム最新)
            let read = match (fresh, self.tracks.get(&l.id)) {
                (true, _) => kanban::classify(
                    l.running,
                    l.attention,
                    l.rate_limited,
                    l.sup,
                    &l.tail_lines,
                ),
                (false, Some(t)) => {
                    kanban::classify(l.running, l.attention, l.rate_limited, l.sup, &t.tail)
                }
                (false, None) => {
                    kanban::classify(l.running, l.attention, l.rate_limited, l.sup, &[])
                }
            };
            let track = self
                .tracks
                .entry(l.id)
                .or_insert_with(|| Track::new(read.activity, read.source, now_ms));
            if fresh {
                let delta = kanban::tail_delta(&track.tail, &l.tail_lines);
                track.pulse.push((now_ms, delta));
                let from = now_ms.saturating_sub(PULSE_WINDOW_MS);
                track.pulse.retain(|(t, _)| *t >= from);
                track.tail = l.tail_lines.clone();
            }
            if track.activity != read.activity {
                track.activity = read.activity;
                track.since_ms = now_ms;
            }
            track.source = read.source;
            track.detail = read.detail.clone();
            if read.activity.is_busy() || track.recently_noisy(now_ms) {
                busy = true;
            }
            acts.push(read.activity);
        }
        // 消えたセッションの追跡は捨てる
        self.tracks.retain(|id, _| live.iter().any(|l| l.id == *id));
        self.busy = busy;
        self.any_running = any_running;
        acts
    }

    /// いまの選択 (テスト・app.rs 用)。
    pub fn selected(&self) -> Option<&RowKey> {
        self.selected.as_ref()
    }

    /// 選択を差し替える (クリック・パレットから)。
    pub fn select(&mut self, key: RowKey) {
        self.selected = Some(key);
        self.dirty = true;
    }

    /// 選択をいまのリストへ解決して覚え直す。
    pub fn sync_selection(&mut self, rows: &[Row], live: &[LiveRow]) -> Option<RowKey> {
        // 初回はアクティブ (紫枠) のセッションを選んでおく
        if self.selected.is_none() {
            self.selected = live
                .iter()
                .find(|l| l.active)
                .map(|l| RowKey::Live(l.id))
                .or_else(|| rows.first().map(|r| r.key.clone()));
        }
        match resolve_selection(self.selected.as_ref(), self.sel_pos, rows) {
            Some((k, pos)) => {
                self.selected = Some(k.clone());
                self.sel_pos = pos;
                Some(k)
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

    /// 追跡状態の参照 (描画・テスト用)。
    pub fn track(&self, id: u64) -> Option<&Track> {
        self.tracks.get(&id)
    }

    /// 停止の確認が出ているセッション。
    pub fn stop_armed(&self) -> Option<u64> {
        self.stop_armed
    }

    /// 内部の意図を 1 件処理する (キーボードもボタンも同じ口を通す)。
    fn apply_intent(&mut self, it: Intent, rows: &[Row], out: &mut Vec<DeckAction>) {
        match it {
            Intent::Move(d) => {
                if let Some(k) = move_selection(rows, self.selected.as_ref(), d) {
                    self.selected = Some(k);
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

/// レールを出せる最小幅。これより細い窓ではレールを上、端末を下に積む。
const RAIL_MIN_WIDTH: f32 = 720.0;

/// アクティビティの色 (すべて theme.rs 由来 — リテラルを書かない)。
fn activity_color(th: &Theme, a: Activity) -> Color32 {
    match a {
        Activity::Starting | Activity::Idle => th.text_dim,
        Activity::Thinking => th.ansi[13],
        Activity::Editing => th.accent,
        Activity::Running => th.ok,
        Activity::Verifying => th.ansi[14],
        Activity::Approval => th.warn,
        Activity::RateLimited | Activity::Stalled => th.err,
        Activity::Exited => th.text_dim,
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

/// デッキ画面を描き、押された操作を返す。
///
/// `now_ms` は supervisor の経過時計 (アプリ起動からの ms)。
/// `fresh_tail` は「このフレームの `live[..].tail_lines` が新しいか」。
/// `scanning` は過去セッションの走査が飛んでいるか (再描画のリズムに効く)。
#[allow(clippy::too_many_arguments)]
pub fn ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    live: &[LiveRow],
    past: &[PastRow],
    launchers: &[LauncherRow],
    quota: Option<(String, u8)>,
    now_ms: u64,
    fresh_tail: bool,
    scanning: bool,
    composer: &mut crate::agent_input::AgentInputBuffer,
    draw: LiveDraw<'_>,
) -> Vec<DeckAction> {
    let mut acts: Vec<DeckAction> = Vec::new();
    let ctx = ui.ctx().clone();

    // ── 永続状態の読み書き ─────────────────────────────────
    if st.layout.is_none() {
        let v = ctx.data_mut(|d| *d.get_persisted_mut_or(mem_id("layout"), 0_u8));
        st.layout = Some(DeckLayout::from_u8(v));
    }
    if st.stack.is_none() {
        let v = ctx.data_mut(|d| *d.get_persisted_mut_or(mem_id("stack"), MIN_STACK));
        st.stack = Some(clamp_stack(v));
    }
    if st.rail.is_none() {
        let v = ctx.data_mut(|d| *d.get_persisted_mut_or(mem_id("rail"), 0.28_f32));
        st.rail = Some(v.clamp(0.18, 0.5));
    }
    if st.selected.is_none() {
        let s = ctx.data_mut(|d| d.get_persisted_mut_or(mem_id("sel"), String::new()).clone());
        st.selected = RowKey::from_persist(&s);
    }
    {
        let mask = ctx.data_mut(|d| *d.get_persisted_mut_or(mem_id("collapse"), 0_u8));
        if !st.dirty {
            for s in SECTIONS {
                st.filter.collapsed[s.ix()] = mask & (1 << s.ix()) != 0;
            }
        }
    }

    // ── 判定 (PTY は sample_due のフレームだけ読まれている) ──
    let activities = st.update_tracks(live, now_ms, fresh_tail);
    // 無条件の再描画はしない。動きがあるときだけ回す (完全に静かなら 1 枚も出さない)。
    if let Some(ms) = deck_repaint_ms(st.busy, st.any_running, scanning) {
        ctx.request_repaint_after(std::time::Duration::from_millis(ms));
    }

    let rows = build_rows(live, &activities, past, launchers, &st.filter);
    let counts = section_counts(live, &activities, past, launchers, &st.filter);
    let selected = st.sync_selection(&rows, live);

    egui::Frame::none()
        .inner_margin(egui::Margin::same(10.0))
        .show(ui, |ui| {
            // ボトムパネルと同じ理由で、先に割り当てられた全高を消費しておく。
            ui.set_min_height(ui.available_height());
            let wide = ui.available_width();

            header_ui(st, ui, theme, live, &activities, quota, &mut acts);
            ui.add_space(6.0);
            chips_ui(st, ui, theme);
            ui.add_space(6.0);

            keyboard_ui(st, ui, &rows, live, past, launchers, &mut acts);

            let horizontal = wide >= RAIL_MIN_WIDTH;
            // 「1画面 (選択のみ)」は一覧を畳んで端末に全部渡す — cmux で
            // 1 セッションに没入するときの見え方。移動は ↑↓/jk がそのまま効く。
            let show_rail = st.layout().shows_rail(true);
            let rail_frac = st.rail.unwrap_or(0.28);
            let main_h = (ui.available_height()).max(160.0);
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), main_h),
                if horizontal {
                    egui::Layout::left_to_right(egui::Align::Min)
                } else {
                    egui::Layout::top_down(egui::Align::Min)
                },
                |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                    if !show_rail {
                        pane_ui(
                            st, ui, theme, &rows, live, &activities, selected.as_ref(), main_h,
                            now_ms, composer, draw, &mut acts,
                        );
                    } else if horizontal {
                        let rail_w = (wide * rail_frac).clamp(210.0, wide * 0.5);
                        ui.allocate_ui_with_layout(
                            egui::vec2(rail_w, main_h),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                rail_ui(
                                    st, ui, theme, &rows, &counts, live, &activities, past,
                                    launchers, now_ms, &mut acts,
                                );
                            },
                        );
                        splitter_ui(st, ui, theme, true, wide);
                        pane_ui(
                            st, ui, theme, &rows, live, &activities, selected.as_ref(), main_h,
                            now_ms, composer, draw, &mut acts,
                        );
                    } else {
                        // 細い窓: 一覧を上、端末を下に積む
                        let rail_h = (main_h * 0.42).clamp(120.0, main_h - 140.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width(), rail_h),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                rail_ui(
                                    st, ui, theme, &rows, &counts, live, &activities, past,
                                    launchers, now_ms, &mut acts,
                                );
                            },
                        );
                        let rest = (main_h - rail_h - 8.0).max(120.0);
                        pane_ui(
                            st, ui, theme, &rows, live, &activities, selected.as_ref(), rest,
                            now_ms, composer, draw, &mut acts,
                        );
                    }
                },
            );
        });

    // ── 永続状態の書き戻し ─────────────────────────────────
    if st.dirty {
        let mask: u8 = SECTIONS.iter().fold(0, |m, s| {
            m | (u8::from(st.filter.collapsed[s.ix()]) << s.ix())
        });
        let layout = st.layout.unwrap_or_default().to_u8();
        let stack = st.stack();
        let rail = st.rail.unwrap_or(0.28);
        let sel = st.selected.as_ref().map(RowKey::to_persist).unwrap_or_default();
        ctx.data_mut(|d| {
            d.insert_persisted(mem_id("layout"), layout);
            d.insert_persisted(mem_id("stack"), stack);
            d.insert_persisted(mem_id("rail"), rail);
            d.insert_persisted(mem_id("collapse"), mask);
            d.insert_persisted(mem_id("sel"), sel);
        });
        st.dirty = false;
    }

    acts
}

/// ヘッダー (件数・上限の助言・レイアウト切替・閉じる)。
#[allow(clippy::too_many_arguments)]
fn header_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    live: &[LiveRow],
    acts_of: &[Activity],
    quota: Option<(String, u8)>,
    acts: &mut Vec<DeckAction>,
) {
    let running = live.iter().filter(|l| l.running).count();
    let pending: usize = live.iter().map(|l| l.approvals).sum();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{DECK_ICON} {}", tr("エージェントデッキ")))
                .size(14.0)
                .strong()
                .color(theme.text),
        );
        chip(
            ui,
            theme.ok,
            &trf("稼働中 {n}", &[("n", running.to_string())]),
        );
        chip(
            ui,
            if pending > 0 { theme.warn } else { theme.text_dim },
            &trf("承認待ち {n}", &[("n", pending.to_string())]),
        );
        let working = acts_of.iter().filter(|a| a.is_busy()).count();
        chip(
            ui,
            theme.accent,
            &trf("作業中 {n}", &[("n", working.to_string())]),
        );
        if let Some((msg, sev)) = quota {
            if !msg.is_empty() {
                ui.label(
                    RichText::new(format!("⚠ {msg}"))
                        .size(11.0)
                        .color(if sev >= 2 { theme.err } else { theme.warn }),
                )
                .on_hover_text(tr("使用量の見立て (coordinator::QuotaWatch)"));
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button("✕")
                .on_hover_text(tr("デッキを閉じる (Esc)"))
                .clicked()
            {
                acts.push(DeckAction::Close);
            }
            // 積み上げ数 (積み上げモードのときだけ)
            if st.layout() == DeckLayout::Stacked {
                if ui
                    .small_button("＋")
                    .on_hover_text(tr("同時に映すセッションを増やす"))
                    .clicked()
                {
                    st.set_stack(st.stack() + 1);
                }
                ui.label(
                    RichText::new(st.stack().to_string())
                        .size(11.5)
                        .color(theme.text),
                );
                if ui
                    .small_button("–")
                    .on_hover_text(tr("同時に映すセッションを減らす"))
                    .clicked()
                {
                    st.set_stack(st.stack().saturating_sub(1));
                }
            }
            for l in [DeckLayout::Stacked, DeckLayout::Split, DeckLayout::Single] {
                let on = st.layout() == l;
                if ui
                    .selectable_label(on, RichText::new(tr(l.label())).size(11.0))
                    .clicked()
                {
                    st.set_layout(l);
                }
            }
            if ui
                .small_button("⇄")
                .on_hover_text(tr("表示を切り替える (1画面 → 2分割 → 積み上げ)"))
                .clicked()
            {
                st.set_layout(st.layout().next());
            }
        });
    });
}

/// フィルタチップの行 + 打ち込み絞り込み欄。
fn chips_ui(st: &mut DeckState, ui: &mut egui::Ui, theme: &Theme) {
    ui.horizontal(|ui| {
        for c in CHIPS {
            let on = st.filter.chip == c;
            if ui
                .selectable_label(on, RichText::new(tr(c.label())).size(11.0))
                .clicked()
            {
                st.filter.chip = c;
                st.dirty = true;
            }
        }
        ui.add_space(8.0);
        ui.label(RichText::new("🔎").size(11.0).color(theme.text_dim));
        let id = mem_id("filter-text");
        let resp = ui.add(
            egui::TextEdit::singleline(&mut st.filter.query)
                .id(id)
                .desired_width(180.0)
                .hint_text(tr("絞り込み (/ でここへ)")),
        );
        if st.filter_focus {
            resp.request_focus();
            st.filter_focus = false;
        }
        if !st.filter.query.is_empty()
            && ui
                .small_button("✕")
                .on_hover_text(tr("絞り込みを消す"))
                .clicked()
        {
            st.filter.query.clear();
        }
    });
}

/// 一覧と端末の間のドラッグバー。
fn splitter_ui(st: &mut DeckState, ui: &mut egui::Ui, theme: &Theme, horizontal: bool, span: f32) {
    let size = if horizontal {
        egui::vec2(4.0, ui.available_height())
    } else {
        egui::vec2(ui.available_width(), 4.0)
    };
    let resp = ui.allocate_response(size, egui::Sense::drag());
    let hot = resp.hovered() || resp.dragged();
    ui.painter()
        .rect_filled(resp.rect, 2.0, if hot { theme.accent } else { theme.border });
    resp.clone().on_hover_cursor(if horizontal {
        egui::CursorIcon::ResizeHorizontal
    } else {
        egui::CursorIcon::ResizeVertical
    });
    if resp.dragged() && span > 1.0 {
        let d = resp.drag_delta();
        let delta = if horizontal { d.x } else { d.y } / span;
        st.rail = Some((st.rail.unwrap_or(0.28) + delta).clamp(0.18, 0.5));
        st.dirty = true;
    }
}

/// 左レール: 3 セクションの縦 1 本リスト。
#[allow(clippy::too_many_arguments)]
fn rail_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    rows: &[Row],
    counts: &[usize; 3],
    live: &[LiveRow],
    activities: &[Activity],
    past: &[PastRow],
    launchers: &[LauncherRow],
    now_ms: u64,
    acts: &mut Vec<DeckAction>,
) {
    let home = home_dir();
    egui::Frame::none()
        .fill(theme.panel)
        .stroke(Stroke::new(1.0_f32, theme.border))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::same(6.0))
        .show(ui, |ui| {
            ui.set_min_height(ui.available_height());
            let w = ui.available_width();
            egui::ScrollArea::vertical()
                .id_salt("zv-deck-rail")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_width(w);
                    for sec in SECTIONS {
                        section_header_ui(st, ui, theme, sec, counts[sec.ix()]);
                        if st.filter.collapsed[sec.ix()] {
                            continue;
                        }
                        let mut any = false;
                        for r in rows.iter().filter(|r| r.section == sec) {
                            any = true;
                            row_ui(
                                st, ui, theme, r, live, activities, past, launchers, now_ms,
                                home.as_deref(), acts,
                            );
                        }
                        if !any {
                            ui.label(
                                RichText::new(tr("該当なし"))
                                    .size(10.5)
                                    .color(theme.text_dim),
                            );
                        }
                        ui.add_space(4.0);
                    }
                });
        });
}

/// セクション見出し (折りたたみ)。
fn section_header_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    sec: Section,
    count: usize,
) {
    let open = !st.filter.collapsed[sec.ix()];
    let arrow = if open { "▾" } else { "▸" };
    let label = format!("{arrow} {} {} ({count})", sec.icon(), tr(sec.title()));
    let resp = ui.add(
        egui::Label::new(
            RichText::new(label)
                .size(11.5)
                .strong()
                .color(theme.text_dim),
        )
        .sense(egui::Sense::click()),
    );
    if resp.on_hover_text(tr(sec.hint())).clicked() {
        st.filter.collapsed[sec.ix()] = open;
        st.dirty = true;
    }
}

/// 一覧の 1 行。
#[allow(clippy::too_many_arguments)]
fn row_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    r: &Row,
    live: &[LiveRow],
    activities: &[Activity],
    past: &[PastRow],
    launchers: &[LauncherRow],
    now_ms: u64,
    home: Option<&Path>,
    acts: &mut Vec<DeckAction>,
) {
    let on = st.selected() == Some(&r.key);
    let bg = if on { theme.accent_soft } else { theme.panel_alt };
    let stroke = if on { theme.accent } else { theme.border };
    let mut clicked = false;
    let mut activated = false;
    egui::Frame::none()
        .fill(bg)
        .stroke(Stroke::new(1.0_f32, stroke))
        .rounding(egui::Rounding::same(6.0))
        .inner_margin(egui::Margin::symmetric(6.0, 4.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            match r.section {
                Section::Live => {
                    let Some(l) = live.get(r.idx) else { return };
                    let a = activities.get(r.idx).copied().unwrap_or(Activity::Starting);
                    let col = activity_color(theme, a);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("●").size(9.0).color(col));
                        // 名前変更中はその場で入力欄に化ける
                        if st.rename_for == Some(l.id) {
                            let id = mem_id("rename");
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut st.rename_buf)
                                    .id(id)
                                    .desired_width(ui.available_width() - 8.0),
                            );
                            if st.rename_focus {
                                resp.request_focus();
                                st.rename_focus = false;
                            }
                            let done = resp.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            if done {
                                let t = st.rename_buf.trim().to_string();
                                if !t.is_empty() {
                                    acts.push(DeckAction::Rename { id: l.id, title: t });
                                }
                                st.rename_for = None;
                            } else if resp.lost_focus() {
                                st.rename_for = None;
                            }
                            return;
                        }
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("{} {}", l.icon, l.title))
                                    .size(12.0)
                                    .strong()
                                    .color(theme.text),
                            )
                            .truncate()
                            .sense(egui::Sense::click()),
                        )
                        .clicked()
                        .then(|| clicked = true);
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if l.approvals > 0 {
                                    ui.label(
                                        RichText::new(format!("🛡{}", l.approvals))
                                            .size(10.0)
                                            .color(theme.warn),
                                    )
                                    .on_hover_text(tr("承認待ちがあります"));
                                }
                                if l.attention {
                                    ui.label(RichText::new("⏳").size(10.0).color(theme.warn))
                                        .on_hover_text(tr("入力・承認を待っています"));
                                }
                                if l.unread {
                                    ui.label(RichText::new("◆").size(9.0).color(theme.accent))
                                        .on_hover_text(tr(
                                            "最後に見てから新しい出力があります",
                                        ));
                                }
                            },
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(short_path(&l.cwd, home))
                                .size(10.0)
                                .color(theme.text_dim),
                        );
                    });
                    ui.horizontal(|ui| {
                        let (label, elapsed) = match st.track(l.id) {
                            Some(t) => (
                                tr(t.activity().label()),
                                kanban::fmt_elapsed(t.elapsed_ms(now_ms)),
                            ),
                            None => (tr(a.label()), String::new()),
                        };
                        ui.label(RichText::new(label).size(10.0).color(col));
                        if !elapsed.is_empty() {
                            ui.label(
                                RichText::new(elapsed).size(9.5).color(theme.text_dim),
                            );
                        }
                        if let Some(t) = st.track(l.id) {
                            if !t.detail().is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(t.detail())
                                            .size(9.5)
                                            .color(theme.text_dim),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(trf(
                                    "{src}: {detail}",
                                    &[
                                        ("src", tr(t.source().label())),
                                        ("detail", t.detail().to_string()),
                                    ],
                                ));
                            }
                            if t.source().is_guess() {
                                ui.label(
                                    RichText::new(tr("推定")).size(9.0).color(theme.text_dim),
                                );
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    pulse_ui(ui, col, &t.pulse_series(now_ms));
                                },
                            );
                        }
                    });
                    if st.stop_armed() == Some(l.id) {
                        ui.label(
                            RichText::new(tr("もう一度 x で停止します"))
                                .size(10.0)
                                .color(theme.err),
                        );
                    }
                }
                Section::Past => {
                    let Some(p) = past.get(r.idx) else { return };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&p.mark).size(11.0).color(theme.text_dim));
                        ui.add(
                            egui::Label::new(
                                RichText::new(if p.session.summary.is_empty() {
                                    tr("（要約なし）")
                                } else {
                                    p.session.summary.clone()
                                })
                                .size(11.5)
                                .color(theme.text),
                            )
                            .truncate()
                            .sense(egui::Sense::click()),
                        )
                        .clicked()
                        .then(|| clicked = true);
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&p.age).size(10.0).color(theme.text_dim));
                        ui.label(
                            RichText::new(short_path(&p.session.cwd, home))
                                .size(10.0)
                                .color(theme.text_dim),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui
                                    .small_button(tr("再開"))
                                    .on_hover_text(tr("この会話を続きから開きます"))
                                    .clicked()
                                {
                                    acts.push(DeckAction::Resume(Box::new(p.session.clone())));
                                }
                            },
                        );
                    });
                }
                Section::New => {
                    let Some(n) = launchers.get(r.idx) else { return };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&n.icon).size(12.0));
                        ui.add(
                            egui::Label::new(
                                RichText::new(&n.name).size(11.5).color(theme.text),
                            )
                            .truncate()
                            .sense(egui::Sense::click()),
                        )
                        .clicked()
                        .then(|| clicked = true);
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui
                                    .small_button(tr("起動"))
                                    .on_hover_text(tr("このプリセットで新しく起こします"))
                                    .clicked()
                                {
                                    activated = true;
                                }
                            },
                        );
                    });
                }
            }
        });
    ui.add_space(3.0);

    if clicked {
        st.select(r.key.clone());
        st.stop_armed = None;
        if let Some(l) = (r.section == Section::Live)
            .then(|| live.get(r.idx))
            .flatten()
        {
            acts.push(DeckAction::Select(l.idx));
        }
    }
    if activated {
        if let Some(n) = launchers.get(r.idx) {
            acts.push(DeckAction::Launch(n.idx));
        }
    }
}

/// 出力の勢い (小さなバー)。
fn pulse_ui(ui: &mut egui::Ui, color: Color32, values: &[f32]) {
    let h = 10.0;
    let w = 44.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
    if values.is_empty() {
        return;
    }
    let n = values.len() as f32;
    let bw = (rect.width() / n).max(1.0);
    for (i, v) in values.iter().enumerate() {
        let vh = (v * h).clamp(0.0, h);
        if vh <= 0.5 {
            continue;
        }
        let x = rect.left() + i as f32 * bw;
        let r = egui::Rect::from_min_size(
            egui::pos2(x, rect.bottom() - vh),
            egui::vec2((bw - 1.0).max(1.0), vh),
        );
        ui.painter().rect_filled(r, 0.0, color.gamma_multiply(0.85));
    }
}

/// 右 (または下) の端末ペイン。レイアウトに応じて 1 本 / 積み上げを描く。
#[allow(clippy::too_many_arguments)]
fn pane_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    rows: &[Row],
    live: &[LiveRow],
    activities: &[Activity],
    selected: Option<&RowKey>,
    height: f32,
    now_ms: u64,
    composer: &mut crate::agent_input::AgentInputBuffer,
    draw: LiveDraw<'_>,
    acts: &mut Vec<DeckAction>,
) {
    let w = ui.available_width();
    ui.allocate_ui_with_layout(
        egui::vec2(w, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            // 下端のコンポーザぶんを先に確保する (端末は残りを全部使う)
            let composer_h = 96.0_f32.min(height * 0.4);
            let term_h = (height - composer_h - 8.0).max(80.0);

            let ids: Vec<u64> = match st.layout() {
                DeckLayout::Stacked => stacked_ids(rows, live, selected, st.stack()),
                _ => selected.and_then(RowKey::live_id).into_iter().collect(),
            };

            st.term_ids.clear();
            if ids.is_empty() {
                empty_pane_ui(ui, theme, term_h, live.is_empty());
            } else {
                fit_weights(&mut st.stack_weights, ids.len());
                let total: f32 = st.stack_weights.iter().sum();
                let bars = (ids.len().saturating_sub(1)) as f32 * 6.0;
                let usable = (term_h - bars).max(60.0);
                let mut want_focus = st.focus_term_req;
                for (i, id) in ids.iter().enumerate() {
                    let frac = st.stack_weights.get(i).copied().unwrap_or(1.0) / total.max(0.001);
                    let h = (usable * frac).max(60.0);
                    let focused_pane = Some(*id) == selected.and_then(RowKey::live_id);
                    term_pane_ui(
                        st,
                        ui,
                        theme,
                        live,
                        activities,
                        *id,
                        h,
                        now_ms,
                        focused_pane && want_focus,
                        draw,
                        acts,
                    );
                    if focused_pane {
                        want_focus = false;
                    }
                    if i + 1 < ids.len() {
                        stack_bar_ui(st, ui, theme, i, usable);
                    }
                }
                st.focus_term_req = false;
            }

            ui.add_space(4.0);
            // ── 選択中エージェント宛てのコンポーザ (ブロードキャストではない) ──
            let target: Option<(u64, String)> = selected
                .and_then(RowKey::live_id)
                .and_then(|id| live.iter().find(|l| l.id == id))
                .map(|l| (l.id, format!("{} {}", l.icon, l.title)));
            // 宛先チップは入力欄の下に横一列で出す (全ライブセッション)。
            let targets: Vec<(u64, String)> = live
                .iter()
                .map(|l| (l.id, format!("{} {}", l.icon, l.title)))
                .collect();
            match crate::panels::agent_composer_ui(
                ui,
                theme,
                composer,
                target.as_ref().map(|(id, t)| (*id, t.as_str())),
                &targets,
            ) {
                crate::panels::ComposerAction::SendTo(id, text) => {
                    acts.push(DeckAction::Send { id, text })
                }
                crate::panels::ComposerAction::Send(text) => {
                    acts.push(DeckAction::Broadcast(text))
                }
                crate::panels::ComposerAction::Cancel => {
                    ui.memory_mut(|m| m.stop_text_input());
                }
                crate::panels::ComposerAction::None => {}
            }
        },
    );
}

/// 端末 1 枚 (ヘッダー付き)。
#[allow(clippy::too_many_arguments)]
fn term_pane_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    theme: &Theme,
    live: &[LiveRow],
    activities: &[Activity],
    id: u64,
    height: f32,
    now_ms: u64,
    want_focus: bool,
    draw: LiveDraw<'_>,
    acts: &mut Vec<DeckAction>,
) {
    let Some((i, l)) = live.iter().enumerate().find(|(_, l)| l.id == id) else {
        return;
    };
    let a = activities.get(i).copied().unwrap_or(Activity::Starting);
    let col = activity_color(theme, a);
    let on = st.selected.as_ref() == Some(&RowKey::Live(id));
    let w = ui.available_width();
    ui.allocate_ui_with_layout(
        egui::vec2(w, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::Frame::none()
                .fill(theme.panel)
                .stroke(Stroke::new(1.0_f32, if on { theme.accent } else { theme.border }))
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin::same(6.0))
                .show(ui, |ui| {
                    ui.set_width(w - 12.0);
                    ui.set_min_height(height - 12.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("●").size(9.0).color(col));
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("{} {}", l.icon, l.title))
                                    .size(12.0)
                                    .strong()
                                    .color(theme.text),
                            )
                            .truncate(),
                        );
                        ui.label(RichText::new(tr(a.label())).size(10.5).color(col));
                        if !l.uptime.is_empty() {
                            ui.label(
                                RichText::new(&l.uptime).size(10.0).color(theme.text_dim),
                            );
                        }
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui
                                    .small_button("✕")
                                    .on_hover_text(tr("このセッションを閉じる (x を 2 回)"))
                                    .clicked()
                                {
                                    acts.push(DeckAction::Stop(l.idx));
                                }
                                if ui
                                    .small_button("⟳")
                                    .on_hover_text(tr("再起動 (s)"))
                                    .clicked()
                                {
                                    acts.push(DeckAction::Restart(l.idx));
                                }
                                if ui
                                    .small_button("⇄")
                                    .on_hover_text(tr("複製 (d) — 同じプリセットと作業ディレクトリ"))
                                    .clicked()
                                {
                                    acts.push(DeckAction::Duplicate(l.idx));
                                }
                                ui.label(
                                    RichText::new(tr(
                                        "↑↓/jk: 選択 / Enter: 端末へ / Esc: 一覧へ戻る",
                                    ))
                                    .size(9.0)
                                    .color(theme.text_dim),
                                );
                            },
                        );
                    });
                    ui.add_space(3.0);
                    let inner = ui.available_height().max(50.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), inner),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| match draw(ui, id) {
                            Some(r) => {
                                st.term_ids.push(r.id);
                                if want_focus {
                                    r.request_focus();
                                }
                                // 端末を触ったらアクティブ選択も追従させる
                                if r.clicked() || r.drag_started() || r.gained_focus() {
                                    st.selected = Some(RowKey::Live(id));
                                    st.dirty = true;
                                    acts.push(DeckAction::Select(l.idx));
                                }
                            }
                            None => {
                                ui.label(
                                    RichText::new(tr("この端末はいま表示できません"))
                                        .size(11.0)
                                        .color(theme.text_dim),
                                );
                            }
                        },
                    );
                    let _ = now_ms;
                });
        },
    );
}

/// 積み上げペインの間のドラッグバー。
fn stack_bar_ui(st: &mut DeckState, ui: &mut egui::Ui, theme: &Theme, i: usize, span: f32) {
    let resp = ui.allocate_response(egui::vec2(ui.available_width(), 4.0), egui::Sense::drag());
    let hot = resp.hovered() || resp.dragged();
    ui.painter()
        .rect_filled(resp.rect, 2.0, if hot { theme.accent } else { theme.border });
    resp.clone().on_hover_cursor(egui::CursorIcon::ResizeVertical);
    if resp.dragged() && span > 1.0 {
        adjust_weights(&mut st.stack_weights, i, resp.drag_delta().y / span);
        st.dirty = true;
    }
}

/// 端末が 1 枚も無いときの案内。
fn empty_pane_ui(ui: &mut egui::Ui, theme: &Theme, height: f32, no_sessions: bool) {
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), height),
        egui::Layout::top_down(egui::Align::Center),
        |ui| {
            ui.add_space(height * 0.3);
            ui.label(
                RichText::new(if no_sessions {
                    tr("まだセッションがありません — 「新規」から n で起こせます")
                } else {
                    tr("一覧から選ぶと、ここにその端末が出ます")
                })
                .size(12.0)
                .color(theme.text_dim),
            );
        },
    );
}

/// 角丸チップ。
fn chip(ui: &mut egui::Ui, color: Color32, text: &str) {
    egui::Frame::none()
        .fill(color.gamma_multiply(0.18))
        .rounding(egui::Rounding::same(9.0))
        .inner_margin(egui::Margin::symmetric(8.0, 2.0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.0).strong().color(color));
        });
}

/// キーボード操作。端末・入力欄にフォーカスがある間は選択移動を奪わない。
#[allow(clippy::too_many_arguments)]
fn keyboard_ui(
    st: &mut DeckState,
    ui: &mut egui::Ui,
    rows: &[Row],
    live: &[LiveRow],
    past: &[PastRow],
    launchers: &[LauncherRow],
    acts: &mut Vec<DeckAction>,
) {
    let focus = ui.ctx().memory(|m| m.focused());
    let term_focused = focus.is_some_and(|f| st.term_ids.contains(&f));
    if term_focused {
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

    let filter_id = mem_id("filter-text");
    let typing_filter = focus == Some(filter_id);
    // 絞り込み欄以外の入力欄 (名前変更・コンポーザ) を打っている間は何もしない
    if focus.is_some() && !typing_filter {
        return;
    }

    // Tab で端末へ (一覧側にフォーカスがあるときだけ奪う)
    if !typing_filter && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)) {
        st.focus_term_req = true;
    }
    // / で絞り込み欄へ
    if !typing_filter && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Slash)) {
        st.filter_focus = true;
        return;
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
        .as_ref()
        .and_then(|k| rows.iter().find(|r| &r.key == k))
        .cloned();
    for (key, m) in pressed {
        // 絞り込み欄を打っている間は、矢印と Enter だけ受ける (文字は欄へ)
        if typing_filter
            && !matches!(
                key,
                egui::Key::ArrowUp | egui::Key::ArrowDown | egui::Key::Enter
            )
        {
            continue;
        }
        let Some(k) = key_intent(key, m.alt, m.command || m.mac_cmd || m.ctrl) else {
            continue;
        };
        let intents = dispatch(k, rows, row.as_ref(), live, past, launchers, st.stop_armed);
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
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn live(idx: usize, id: u64, title: &str) -> LiveRow {
        LiveRow {
            idx,
            id,
            icon: "👾".into(),
            title: title.into(),
            cwd: PathBuf::from("/tmp/work"),
            command: "claude".into(),
            running: true,
            uptime: "0:10".into(),
            ..Default::default()
        }
    }

    fn past(bin: &str, id: &str, summary: &str) -> PastRow {
        PastRow {
            session: PastSession {
                id: id.into(),
                agent_bin: bin.into(),
                started: UNIX_EPOCH + Duration::from_secs(1),
                modified: UNIX_EPOCH + Duration::from_secs(2),
                summary: summary.into(),
                cwd: PathBuf::from("/tmp/work"),
            },
            age: "1時間前".into(),
            mark: "C".into(),
        }
    }

    fn launcher(idx: usize, name: &str) -> LauncherRow {
        LauncherRow {
            idx,
            icon: "👾".into(),
            name: name.into(),
        }
    }

    fn acts_of(n: usize, a: Activity) -> Vec<Activity> {
        vec![a; n]
    }

    // ── リストの組み立て ────────────────────────────────────

    #[test]
    fn sections_are_built_in_fixed_order() {
        let l = vec![live(0, 1, "a"), live(1, 2, "b")];
        let p = vec![past("claude", "uuid-1", "直したい")];
        let n = vec![launcher(0, "Claude"), launcher(1, "Codex")];
        let rows = build_rows(&l, &acts_of(2, Activity::Idle), &p, &n, &Filter::default());
        let secs: Vec<Section> = rows.iter().map(|r| r.section).collect();
        assert_eq!(
            secs,
            vec![
                Section::Live,
                Section::Live,
                Section::Past,
                Section::New,
                Section::New
            ]
        );
        assert_eq!(rows[0].key, RowKey::Live(1));
        assert_eq!(
            rows[2].key,
            RowKey::Past("claude".into(), "uuid-1".into())
        );
        assert_eq!(rows[4].key, RowKey::Launcher(1));
    }

    #[test]
    fn collapsed_sections_contribute_no_rows_but_counts_stay() {
        let l = vec![live(0, 1, "a")];
        let p = vec![past("claude", "u1", "x")];
        let n = vec![launcher(0, "Claude")];
        let mut f = Filter::default();
        f.collapsed[Section::Past.ix()] = true;
        let rows = build_rows(&l, &acts_of(1, Activity::Idle), &p, &n, &f);
        assert!(rows.iter().all(|r| r.section != Section::Past));
        let c = section_counts(&l, &acts_of(1, Activity::Idle), &p, &n, &f);
        assert_eq!(c, [1, 1, 1], "件数は折りたたんでも数える");
    }

    /// 稼働中として開かれている過去の会話は一覧から落ちる (二重表示しない)。
    #[test]
    fn past_session_open_as_live_is_deduped() {
        let mut l = live(0, 1, "claude");
        l.command = "claude --resume 4f2a-uuid".into();
        let p = vec![past("claude", "4f2a-uuid", "続き"), past("claude", "other", "別")];
        let rows = build_rows(
            &[l],
            &acts_of(1, Activity::Thinking),
            &p,
            &[],
            &Filter::default(),
        );
        let past_keys: Vec<&RowKey> = rows
            .iter()
            .filter(|r| r.section == Section::Past)
            .map(|r| &r.key)
            .collect();
        assert_eq!(past_keys.len(), 1);
        assert_eq!(*past_keys[0], RowKey::Past("claude".into(), "other".into()));
    }

    #[test]
    fn dedup_needs_a_running_session_and_a_real_id() {
        let mut l = live(0, 1, "claude");
        l.command = "claude --resume 4f2a".into();
        l.running = false;
        assert!(!is_open_live("4f2a", &[l.clone()]), "終了済みは隠さない");
        l.running = true;
        assert!(is_open_live("4f2a", &[l.clone()]));
        assert!(!is_open_live("", &[l]), "空 ID で全部消さない");
    }

    #[test]
    fn query_filters_across_sections() {
        let l = vec![live(0, 1, "claude 本体"), live(1, 2, "codex")];
        let p = vec![past("claude", "u1", "codex の話")];
        let n = vec![launcher(0, "Codex CLI"), launcher(1, "Claude Code")];
        let f = Filter {
            query: "codex".into(),
            ..Default::default()
        };
        let rows = build_rows(&l, &acts_of(2, Activity::Idle), &p, &n, &f);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].key, RowKey::Live(2));
        assert_eq!(rows[1].section, Section::Past);
        assert_eq!(rows[2].key, RowKey::Launcher(0));
    }

    #[test]
    fn query_is_case_insensitive_and_trims() {
        assert!(matches_query("  CLAUDE ", &["claude code"]));
        assert!(matches_query("", &["なんでも"]));
        assert!(!matches_query("zzz", &["claude"]));
    }

    // ── フィルタチップ ──────────────────────────────────────

    #[test]
    fn chips_select_live_states() {
        let mut busy = live(0, 1, "busy");
        let idle = live(1, 2, "idle");
        let mut waiting = live(2, 3, "waiting");
        waiting.attention = true;
        busy.command = "x".into();
        let l = vec![busy, idle, waiting];
        let a = vec![Activity::Running, Activity::Idle, Activity::Approval];
        let n = vec![launcher(0, "Claude")];

        let pick = |c: Chip| -> Vec<RowKey> {
            build_rows(
                &l,
                &a,
                &[],
                &n,
                &Filter {
                    chip: c,
                    ..Default::default()
                },
            )
            .into_iter()
            .map(|r| r.key)
            .collect()
        };
        assert_eq!(
            pick(Chip::All),
            vec![
                RowKey::Live(1),
                RowKey::Live(2),
                RowKey::Live(3),
                RowKey::Launcher(0)
            ]
        );
        assert_eq!(pick(Chip::Working), vec![RowKey::Live(1)]);
        assert_eq!(pick(Chip::Waiting), vec![RowKey::Live(2)]);
        assert_eq!(pick(Chip::Attention), vec![RowKey::Live(3)]);
    }

    #[test]
    fn non_all_chips_hide_past_and_launcher_sections() {
        let l = vec![live(0, 1, "a")];
        let p = vec![past("claude", "u1", "x")];
        let n = vec![launcher(0, "Claude")];
        for c in [Chip::Working, Chip::Waiting, Chip::Attention] {
            let rows = build_rows(
                &l,
                &acts_of(1, Activity::Running),
                &p,
                &n,
                &Filter {
                    chip: c,
                    ..Default::default()
                },
            );
            assert!(
                rows.iter().all(|r| r.section == Section::Live),
                "{c:?} で稼働中以外が残っている"
            );
        }
    }

    // ── 選択の安定性 ────────────────────────────────────────

    fn rows_of(ids: &[u64]) -> Vec<Row> {
        ids.iter()
            .enumerate()
            .map(|(i, id)| Row {
                key: RowKey::Live(*id),
                section: Section::Live,
                idx: i,
            })
            .collect()
    }

    #[test]
    fn selection_survives_insert_and_reorder() {
        let rows = rows_of(&[1, 2, 3]);
        let sel = RowKey::Live(2);
        let (k, pos) = resolve_selection(Some(&sel), 1, &rows).unwrap();
        assert_eq!((k.clone(), pos), (RowKey::Live(2), 1));
        // 先頭に 1 本挿さっても同じセッションを指す
        let rows2 = rows_of(&[9, 1, 2, 3]);
        let (k2, pos2) = resolve_selection(Some(&k), pos, &rows2).unwrap();
        assert_eq!((k2.clone(), pos2), (RowKey::Live(2), 2));
        // 並べ替えても同じ
        let rows3 = rows_of(&[3, 2, 1, 9]);
        let (k3, pos3) = resolve_selection(Some(&k2), pos2, &rows3).unwrap();
        assert_eq!((k3, pos3), (RowKey::Live(2), 1));
    }

    #[test]
    fn selection_falls_back_to_the_same_slot_when_removed() {
        let rows = rows_of(&[1, 2, 3]);
        let gone = RowKey::Live(2);
        // 2 が消えた: 同じ位置 (index 1) に居る 3 へ寄る
        let rows2 = rows_of(&[1, 3]);
        let (k, pos) = resolve_selection(Some(&gone), 1, &rows2).unwrap();
        assert_eq!((k, pos), (RowKey::Live(3), 1));
        // 末尾が消えた: 末尾へ丸める
        let rows3 = rows_of(&[1]);
        let (k, pos) = resolve_selection(Some(&RowKey::Live(3)), 2, &rows3).unwrap();
        assert_eq!((k, pos), (RowKey::Live(1), 0));
        // 空なら選択なし
        assert_eq!(resolve_selection(Some(&RowKey::Live(1)), 0, &[]), None);
        let _ = rows;
    }

    // ── キーボード移動 ──────────────────────────────────────

    #[test]
    fn arrows_cross_section_boundaries() {
        let l = vec![live(0, 1, "a")];
        let p = vec![past("claude", "u1", "x")];
        let n = vec![launcher(0, "Claude")];
        let rows = build_rows(&l, &acts_of(1, Activity::Idle), &p, &n, &Filter::default());
        assert_eq!(rows.len(), 3);
        let mut cur = Some(rows[0].key.clone());
        cur = move_selection(&rows, cur.as_ref(), 1);
        assert_eq!(cur, Some(rows[1].key.clone()), "稼働中 → ローカル");
        cur = move_selection(&rows, cur.as_ref(), 1);
        assert_eq!(cur, Some(rows[2].key.clone()), "ローカル → 新規");
        // 端では止まる
        assert_eq!(
            move_selection(&rows, cur.as_ref(), 1),
            Some(rows[2].key.clone())
        );
        assert_eq!(
            move_selection(&rows, Some(&rows[0].key), -1),
            Some(rows[0].key.clone())
        );
    }

    #[test]
    fn navigation_respects_the_active_filter() {
        let l = vec![live(0, 1, "claude"), live(1, 2, "codex")];
        let n = vec![launcher(0, "Codex")];
        let f = Filter {
            query: "codex".into(),
            ..Default::default()
        };
        let rows = build_rows(&l, &acts_of(2, Activity::Idle), &[], &n, &f);
        assert_eq!(rows.len(), 2);
        // 絞り込みで消えた行 (Live(1)) からの移動は、残っている先頭へ入る
        let next = move_selection(&rows, Some(&RowKey::Live(1)), 1);
        assert_eq!(next, Some(RowKey::Live(2)));
        let next = move_selection(&rows, next.as_ref(), 1);
        assert_eq!(next, Some(RowKey::Launcher(0)));
    }

    #[test]
    fn navigation_on_empty_list_is_noop() {
        assert_eq!(move_selection(&[], None, 1), None);
        assert_eq!(move_selection(&rows_of(&[1]), None, 0), None);
    }

    // ── レイアウトの状態機械 ───────────────────────────────

    #[test]
    fn layout_cycles_and_round_trips_through_u8() {
        let mut l = DeckLayout::default();
        assert_eq!(l, DeckLayout::Single);
        l = l.next();
        assert_eq!(l, DeckLayout::Split);
        l = l.next();
        assert_eq!(l, DeckLayout::Stacked);
        l = l.next();
        assert_eq!(l, DeckLayout::Single);
        for l in [DeckLayout::Single, DeckLayout::Split, DeckLayout::Stacked] {
            assert_eq!(DeckLayout::from_u8(l.to_u8()), l);
        }
        assert_eq!(DeckLayout::from_u8(99), DeckLayout::Single, "壊れた値は既定へ");
    }

    #[test]
    fn stack_count_is_bounded() {
        assert_eq!(clamp_stack(0), MIN_STACK);
        assert_eq!(clamp_stack(1), MIN_STACK);
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
        let l = vec![live(0, 1, "a"), live(1, 2, "b"), live(2, 3, "c")];
        let rows = build_rows(&l, &acts_of(3, Activity::Idle), &[], &[], &Filter::default());
        let sel = RowKey::Live(2);
        assert_eq!(stacked_ids(&rows, &l, Some(&sel), 2), vec![2, 3]);
        assert_eq!(stacked_ids(&rows, &l, Some(&sel), 3), vec![2, 3, 1]);
        // 欲しい数がセッション数を超えても重複させない
        assert_eq!(stacked_ids(&rows, &l, Some(&sel), 9), vec![2, 3, 1]);
        // 稼働中が無ければ空
        assert!(stacked_ids(&[], &[], Some(&sel), 3).is_empty());
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

    // ── ライフサイクルの発行 (偽のディスパッチャで記録するだけ) ──

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
            past: &[PastRow],
            launchers: &[LauncherRow],
        ) {
            let row = st
                .selected
                .as_ref()
                .and_then(|key| rows.iter().find(|r| &r.key == key))
                .cloned();
            let intents = dispatch(k, rows, row.as_ref(), live, past, launchers, st.stop_armed);
            for it in intents {
                st.apply_intent(it, rows, &mut self.seen);
            }
        }
    }

    #[test]
    fn lifecycle_keys_emit_intents_without_launching() {
        let l = vec![live(0, 7, "claude"), live(1, 8, "codex")];
        let p = vec![past("claude", "u1", "続き")];
        let n = vec![launcher(0, "Claude"), launcher(1, "Codex")];
        let rows = build_rows(&l, &acts_of(2, Activity::Idle), &p, &n, &Filter::default());
        let mut st = DeckState::default();
        st.select(RowKey::Live(7));
        let mut d = FakeDispatcher::default();

        // n = 新規 (選択が稼働中なら先頭プリセット)
        d.feed(&mut st, DeckKey::New, &rows, &l, &p, &n);
        assert_eq!(d.seen.last(), Some(&DeckAction::Launch(0)));

        // d = 複製
        d.feed(&mut st, DeckKey::Duplicate, &rows, &l, &p, &n);
        assert_eq!(d.seen.last(), Some(&DeckAction::Duplicate(0)));

        // s = 再起動
        d.feed(&mut st, DeckKey::Restart, &rows, &l, &p, &n);
        assert_eq!(d.seen.last(), Some(&DeckAction::Restart(0)));

        // r = 名前変更 (副作用は出さず、入力欄が開くだけ)
        let before = d.seen.len();
        d.feed(&mut st, DeckKey::Rename, &rows, &l, &p, &n);
        assert_eq!(d.seen.len(), before, "r 単体では何も実行しない");
        assert_eq!(st.rename_for, Some(7));
        st.rename_for = None;

        // x = 停止 (1 打目は確認、2 打目で実行)
        let before = d.seen.len();
        d.feed(&mut st, DeckKey::Stop, &rows, &l, &p, &n);
        assert_eq!(d.seen.len(), before, "1 打目では止めない");
        assert_eq!(st.stop_armed(), Some(7));
        d.feed(&mut st, DeckKey::Stop, &rows, &l, &p, &n);
        assert_eq!(d.seen.last(), Some(&DeckAction::Stop(0)));
        assert_eq!(st.stop_armed(), None, "実行したら確認は下ろす");

        // 選択が動いたら確認は取り下げる
        d.feed(&mut st, DeckKey::Stop, &rows, &l, &p, &n);
        assert_eq!(st.stop_armed(), Some(7));
        d.feed(&mut st, DeckKey::Down, &rows, &l, &p, &n);
        assert_eq!(st.stop_armed(), None);
        assert_eq!(st.selected(), Some(&RowKey::Live(8)));
    }

    #[test]
    fn enter_resumes_a_past_session_and_launches_a_preset() {
        let l = vec![live(0, 7, "claude")];
        let p = vec![past("claude", "u1", "続き")];
        let n = vec![launcher(3, "Codex")];
        let rows = build_rows(&l, &acts_of(1, Activity::Idle), &p, &n, &Filter::default());
        let mut st = DeckState::default();
        let mut d = FakeDispatcher::default();

        st.select(rows[1].key.clone());
        d.feed(&mut st, DeckKey::Enter, &rows, &l, &p, &n);
        match d.seen.last() {
            Some(DeckAction::Resume(s)) => assert_eq!(s.id, "u1"),
            other => panic!("再開が出ていない: {other:?}"),
        }

        st.select(rows[2].key.clone());
        d.feed(&mut st, DeckKey::Enter, &rows, &l, &p, &n);
        assert_eq!(
            d.seen.last(),
            Some(&DeckAction::Launch(3)),
            "プリセットの実 index を渡す"
        );

        // 稼働中で Enter = アクティブ切替 + 端末へフォーカス
        st.select(rows[0].key.clone());
        d.feed(&mut st, DeckKey::Enter, &rows, &l, &p, &n);
        assert_eq!(d.seen.last(), Some(&DeckAction::Select(0)));
        assert!(st.focus_term_req);
    }

    #[test]
    fn alt_arrows_reorder_within_the_live_section_only() {
        let l = vec![live(0, 1, "a"), live(1, 2, "b"), live(2, 3, "c")];
        let n = vec![launcher(0, "Claude")];
        let rows = build_rows(&l, &acts_of(3, Activity::Idle), &[], &n, &Filter::default());
        let mut st = DeckState::default();
        let mut d = FakeDispatcher::default();

        st.select(RowKey::Live(2));
        d.feed(&mut st, DeckKey::MoveUp, &rows, &l, &[], &n);
        assert_eq!(d.seen.last(), Some(&DeckAction::Reorder { from: 1, to: 0 }));
        d.feed(&mut st, DeckKey::MoveDown, &rows, &l, &[], &n);
        assert_eq!(d.seen.last(), Some(&DeckAction::Reorder { from: 1, to: 2 }));

        // 端では何も出さない
        let before = d.seen.len();
        st.select(RowKey::Live(1));
        d.feed(&mut st, DeckKey::MoveUp, &rows, &l, &[], &n);
        assert_eq!(d.seen.len(), before);

        // 起動プリセットの行では並べ替えない
        st.select(RowKey::Launcher(0));
        d.feed(&mut st, DeckKey::MoveDown, &rows, &l, &[], &n);
        assert_eq!(d.seen.len(), before);
    }

    #[test]
    fn key_intents_ignore_command_chords() {
        assert_eq!(key_intent(egui::Key::J, false, false), Some(DeckKey::Down));
        assert_eq!(key_intent(egui::Key::K, false, false), Some(DeckKey::Up));
        assert_eq!(key_intent(egui::Key::X, false, false), Some(DeckKey::Stop));
        assert_eq!(
            key_intent(egui::Key::ArrowUp, true, false),
            Some(DeckKey::MoveUp)
        );
        // ⌘ が乗っていたらデッキは手を出さない (アプリのショートカットが先)
        assert_eq!(key_intent(egui::Key::N, false, true), None);
        assert_eq!(key_intent(egui::Key::ArrowUp, true, true), None);
        // ⌥ + 文字は並べ替え以外に割り当てない
        assert_eq!(key_intent(egui::Key::D, true, false), None);
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
        assert_eq!(deck_repaint_ms(true, true, false), Some(FAST_SAMPLE_MS));
        assert_eq!(deck_repaint_ms(false, true, false), Some(SLOW_SAMPLE_MS));
        // 走査中だけは短く回して、結果が届いたら止まる
        assert_eq!(deck_repaint_ms(false, false, true), Some(SCAN_POLL_MS));
        // 出力があるときは走っている扱いより優先
        assert_eq!(deck_repaint_ms(true, false, true), Some(FAST_SAMPLE_MS));
    }

    #[test]
    fn sampling_is_throttled_and_speeds_up_when_busy() {
        let mut st = DeckState::default();
        assert!(st.sample_due(0), "初回は必ず読む");
        assert!(!st.sample_due(10));
        assert!(!st.sample_due(999));
        assert!(st.sample_due(1_000), "静かなときは 1Hz");
        st.busy = true;
        assert!(!st.sample_due(1_100));
        assert!(st.sample_due(1_150), "動いているときは ~6.7Hz");
    }

    #[test]
    fn tracks_are_dropped_with_their_sessions() {
        let mut st = DeckState::default();
        let l = vec![live(0, 1, "a"), live(1, 2, "b")];
        let a = st.update_tracks(&l, 0, true);
        assert_eq!(a.len(), 2);
        assert!(st.track(1).is_some() && st.track(2).is_some());
        st.update_tracks(&l[..1], 100, true);
        assert!(st.track(2).is_none(), "消えたセッションの追跡は捨てる");
        assert!(st.any_running);
    }

    #[test]
    fn tracks_reuse_the_last_screen_when_no_fresh_sample() {
        let mut st = DeckState::default();
        let mut l = live(0, 1, "a");
        l.sup = Some(supervisor::SessionState::Working);
        l.tail_lines = vec!["$ cargo test".into()];
        let a1 = st.update_tracks(std::slice::from_ref(&l), 0, true);
        // 次のフレームは tail が空 (サンプリングしていない) でも判定が落ちない
        let mut l2 = l.clone();
        l2.tail_lines.clear();
        let a2 = st.update_tracks(std::slice::from_ref(&l2), 50, false);
        assert_eq!(a1, a2, "サンプルしていないフレームで状態が揺れない");
    }

    // ── 永続化のキー ────────────────────────────────────────

    #[test]
    fn row_keys_round_trip_through_persistence() {
        for k in [
            RowKey::Live(42),
            RowKey::Past("claude".into(), "uuid-1".into()),
            RowKey::Launcher(3),
        ] {
            assert_eq!(RowKey::from_persist(&k.to_persist()), Some(k));
        }
        assert_eq!(RowKey::from_persist(""), None);
        assert_eq!(RowKey::from_persist("Z\u{1}1"), None);
        assert_eq!(RowKey::from_persist("L\u{1}notanumber"), None);
    }

    // ── 表示ヘルパ ──────────────────────────────────────────

    #[test]
    fn short_path_uses_home_and_keeps_the_tail() {
        let home = PathBuf::from("/Users/me");
        assert_eq!(short_path(Path::new("/Users/me"), Some(&home)), "~");
        assert_eq!(
            short_path(Path::new("/Users/me/dev"), Some(&home)),
            "~/dev"
        );
        assert_eq!(
            short_path(Path::new("/Users/me/dev/a/b/c"), Some(&home)),
            "…/b/c"
        );
        assert_eq!(short_path(Path::new("/tmp"), None), "/tmp");
    }

    #[test]
    fn section_indices_are_distinct_and_ordered() {
        let ixs: Vec<usize> = SECTIONS.iter().map(|s| s.ix()).collect();
        assert_eq!(ixs, vec![0, 1, 2]);
    }

    #[test]
    fn deck_state_selection_defaults_to_the_active_session() {
        let mut l = vec![live(0, 1, "a"), live(1, 2, "b")];
        l[1].active = true;
        let rows = build_rows(&l, &acts_of(2, Activity::Idle), &[], &[], &Filter::default());
        let mut st = DeckState::default();
        assert_eq!(st.sync_selection(&rows, &l), Some(RowKey::Live(2)));
    }

    #[test]
    fn relative_age_helper_is_reused_from_session_picker() {
        // 過去セクションの相対時刻は session_picker の実装をそのまま使う
        let now = SystemTime::now();
        let then = now - Duration::from_secs(3600);
        assert!(!crate::session_picker::relative_age(now, then).is_empty());
    }
}
