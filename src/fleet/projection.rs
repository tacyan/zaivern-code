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

use super::model::{AgentView, Snapshot};

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
pub fn lane_counts(snap: &Snapshot) -> [usize; LANES] {
    let mut counts = [0usize; LANES];
    for a in &snap.agents {
        counts[a.lane.index()] += 1;
    }
    counts
}

/// 「停止中 (= 生きているのに前へ進んでいない)」と見なす ID。
///
/// 判定は [`crate::supervisor::SessionState::is_stuck`] が唯一の元だったが、
/// レーンで見れば同じことが**床を通った後**で言える。
pub fn stuck_ids(snap: &Snapshot) -> Vec<u64> {
    snap.agents
        .iter()
        .filter(|a| a.running && matches!(a.lane, Column::Ready | Column::Trouble))
        .map(|a| a.id)
        .collect()
}
