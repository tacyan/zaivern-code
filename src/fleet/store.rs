//! **FleetStore** — Zaivern における Fleet 状態の Single Source of Truth。
//!
//! ## 約束
//!
//! 1. **書くのは [`FleetStore::update`] だけ。** 呼ぶのは 1 スレッド (UI スレッドの
//!    見張り刻み) で、ロックは持たない。
//! 2. **読むのは [`FleetStore::snapshot`] のクローン 1 回。**
//!    `Arc` を配るので、描画中にロックを持ち越さない
//!    (`git::Git::branch` が採っている「裏で作り、UI へは手元の値を返す」と同じ)。
//! 3. **判定は 1 か所。** 看板・デッキ・Cockpit・サイドバー・スマホは
//!    `classify` / `column_for` / `state_label` を**自分で呼ばない**。
//!
//! ## 費用
//!
//! * `update` はエージェント数 N に対して O(N)。中で PTY を読まない
//!   (画面は呼び出し側が [`Observation::tail_lines`] へ入れて渡す = **重複解析しない**)。
//! * [`FleetStore::sample_due`] が「画面を読み直してよいティックか」を決める。
//!   動いている間 ~6.7Hz、静かなら 1Hz。看板が持っていた間引きをそのまま移した。
//! * 読み側は `Arc::clone` 1 回。スナップショットの中身は複製しない。

use std::collections::HashMap;
use std::sync::Arc;

use crate::kanban::{FAST_SAMPLE_MS, SLOW_SAMPLE_MS};

use super::engine::{step_tracks, Track};

use super::model::{Observation, Snapshot};

/// Fleet 状態の唯一の持ち主。`ZaivernApp` が 1 つだけ持つ。
#[derive(Default)]
pub struct FleetStore {
    /// セッション ID → 時間依存の追跡状態。**ビューではなくここが持つ。**
    tracks: HashMap<u64, Track>,
    /// 最新のスナップショット。読み手はこれを `Arc` で受け取る。
    snap: Arc<Snapshot>,
    /// 最後に画面をサンプルした時刻 (間引き用)。
    last_sample_ms: Option<u64>,
}

impl FleetStore {
    /// **PTY 画面を読み直してよいティックか。**
    ///
    /// 呼び出し側はこれが false のあいだ `screen_tail_lines` を呼ばず、
    /// [`Observation::tail_lines`] を `None` にする
    /// (= 追跡側が前回サンプルを使い回すので、判定は落ちない)。
    ///
    /// 従来は `KanbanState::sample_due` にあり、**看板を開いているフレームしか
    /// 進まなかった**。ここへ移したことで、どの画面を開いていても同じ刻みで回る。
    pub fn sample_due(&mut self, now_ms: u64) -> bool {
        let interval = if self.snap.busy {
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

    /// **観測を 1 ティック分取り込み、スナップショットを丸ごと差し替える。**
    ///
    /// 部分更新はしない — 「A は新しい判定、B は前の判定」という混ざり方をすると、
    /// 一覧と集計が食い違う。
    pub fn update(&mut self, obs: &[Observation], now_ms: u64) {
        let (agents, stats) = step_tracks(&mut self.tracks, obs, now_ms);
        self.snap = Arc::new(Snapshot {
            agents,
            busy: stats.busy,
            any_running: stats.any_running,
            animating: stats.animating,
            arrived: stats.arrived,
            first_fill: stats.first_fill,
        });
    }

    /// 最新のスナップショット。**読み手はこれだけを見る。**
    pub fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.snap)
    }

    /// 借用で足りる読み手 (同じフレーム内で使い切る描画) 向け。
    pub fn snap(&self) -> &Snapshot {
        &self.snap
    }

    /// 追跡している体数 (掃除が効いているかの確認用)。
    #[cfg(test)]
    pub fn tracked(&self) -> usize {
        self.tracks.len()
    }
}
