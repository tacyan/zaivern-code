//! **射影** — 正準ビュー ([`AgentView`]) から、各画面が要る形へ落とす純関数群。
//!
//! 射影は**変換だけ**で、判断を 1 つも持たない。判断は
//! [`crate::fleet::engine`] が済ませてあるので、ここに `if attention` のような
//! 条件が生えたら、それは判断が漏れ出した合図である。
//!
//! ## なぜ射影を 1 か所に集めるか
//!
//! Cockpit (`app/cockpit.rs`) とサイドバー (`app/sidebar_ui.rs`) は
//! `if running { if attention { warn } else { ok } } else { err }` を
//! **それぞれ独立に**書いていた。見張りを 1 バイトも見ていないので、
//! 停滞中のエージェントが緑の ● で表示されていた。
//! 同じ式が 2 か所にある時点で、片方だけ直る経路が必ずできる。

use crate::kanban::{Column, LANES};
use crate::theme::Theme;

use super::model::{AgentKind, AgentView, Snapshot};

/// **駆動方式で絞ったビュー列** — 集計系の入口はすべてこれを通す。
///
/// `None` は Fleet 全体 (`Total Agents` はこちら)。
/// `Some(AgentKind::Pty)` は端末セッションだけ。
///
/// **絞り込みを引数にしたのは、既定を決め打つと必ず片方が嘘になるから。**
/// スマホの一覧は PTY セッションだけを返す (操作 API がセッション index を
/// 宛先に使うので ACP を混ぜられない) のに、見出しの件数を Fleet 全体で
/// 数えると「見出しは 4 なのに行は 2 本」になる。数える対象と並べる対象は
/// 必ず同じでなければならない。
fn views(snap: &Snapshot, kind: Option<AgentKind>) -> impl Iterator<Item = &AgentView> {
    snap.agents
        .iter()
        .filter(move |a| kind.is_none_or(|k| a.kind == k))
}

/// 一覧の丸印が示す「注意の度合い」。**色の決定はここ 1 か所**。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dot {
    /// 動いている / 手が空いている (通常)
    Ok,
    /// 人の手が要る (承認待ち・入力待ち)
    Attention,
    /// 停滞・ループ・エラー多発・レート制限
    Trouble,
    /// 終了している
    Dead,
}

impl Dot {
    /// 正準ビューから決める。**レーンだけを見る** (生フラグを見ない)。
    ///
    /// レーンは既に確信度の床とヒステリシスを通っているので、
    /// ここで `attention` を直接読むと**床を迂回する**ことになる。
    pub fn of(v: &AgentView) -> Dot {
        if !v.running {
            return Dot::Dead;
        }
        match v.lane {
            Column::Approval => Dot::Attention,
            Column::Trouble => Dot::Trouble,
            Column::Done => Dot::Dead,
            _ => Dot::Ok,
        }
    }

    pub fn color(self, theme: &Theme) -> eframe::egui::Color32 {
        match self {
            Dot::Ok => theme.ok,
            Dot::Attention => theme.warn,
            Dot::Trouble => theme.err,
            Dot::Dead => theme.err,
        }
    }
}

/// レーン別の人数 ([`Column::index`] 順)。スマホの見出しと KPI が読む。
///
/// `kind` の意味は [`views`] を参照。**並べる対象と同じ絞り込みを渡すこと。**
pub fn lane_counts(snap: &Snapshot, kind: Option<AgentKind>) -> [usize; LANES] {
    let mut counts = [0usize; LANES];
    for a in views(snap, kind) {
        counts[a.lane.index()] += 1;
    }
    counts
}

/// 「待ち」= 人の手が要るレーンに居る体数。
///
/// スマホは**バッジ (`/api/state`) と一覧 (`/api/agents`) で同じ数**を出す。
/// 数え方が 2 つあると「バッジ 3 なのに一覧は 5 件」になるので、
/// 判定 ([`crate::remote::is_waiting_lane`]) も絞り込みもここ 1 か所に置く。
pub fn waiting_count(snap: &Snapshot, kind: Option<AgentKind>) -> usize {
    views(snap, kind)
        .filter(|a| crate::remote::is_waiting_lane(a.lane))
        .count()
}

/// 「停止中 (= 生きているのに前へ進んでいない)」と見なす ID。
///
/// 判定は [`crate::supervisor::SessionState::is_stuck`] が唯一の元だったが、
/// レーンで見れば同じことが**床を通った後**で言える。
pub fn stuck_ids(snap: &Snapshot, kind: Option<AgentKind>) -> std::collections::HashSet<u64> {
    views(snap, kind)
        .filter(|a| a.running && matches!(a.lane, Column::Ready | Column::Trouble))
        .map(|a| a.id)
        .collect()
}
