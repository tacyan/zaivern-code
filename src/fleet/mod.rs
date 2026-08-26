//! # Fleet — エージェント状態の Single Source of Truth
//!
//! ## 何を解いたか
//!
//! 同じエージェントの状態を、**同じ瞬間に 6 通り**に判定していた:
//!
//! | 入口 | ラダー | 画面末尾 | flow 裏取り | ヒステリシス | 読み手 |
//! |---|---|---|---|---|---|
//! | `kanban::classify_stream` | ✅ | ✅ | ✅ | ✅ | 📋 看板 **描画中のみ** |
//! | `deck` の自前追跡 | ❌ | ✅ | ❌ | 一部 | 🃏 デッキ **描画中のみ** |
//! | `kanban::column_for` | ❌ | ❌ | ❌ | ❌ | スマホ / カード初期値 |
//! | `cockpit` / `sidebar` の生フラグ | ❌ | ❌ | ❌ | ❌ | ●○ |
//! | `acp::Phase` | (ACP) | ❌ | ❌ | ❌ | ACP パネルのみ (**Fleet 不可視**) |
//!
//! 原因は 2 つだけだった:
//!
//! * **R1**: 正準状態を「値」ではなく**公開された導出関数**で持っていたので、
//!   呼ぶ側が入力を選べた (`column_for` は ladder も tail も flow も渡さない)
//! * **R2**: 時間依存の状態 (ヒステリシス / `Flow` / `TROUBLE_HOLD_MS`) が
//!   **ビューのメモリ**にあり、そのビューを閉じると計時が止まった
//!
//! Phase 1 はこの 2 つを閉じる。**判定アルゴリズムは 1 つも新設していない** —
//! いちばん信頼できる既存の判定 (`classify_stream` + `Read::lane` の床 +
//! `trouble_confirmed` の裏取り + `LaneTracker` のヒステリシス) を
//! [`store::FleetStore`] の中へ集約しただけである。
//!
//! ## 読み取り経路は 1 本
//!
//! ```text
//! FleetStore → Snapshot → AgentView
//! ```
//!
//! 看板・デッキ・Cockpit・サイドバー・スマホ一覧・ACP はすべてこれを読む。
//!
//! ## 詳しい設計
//!
//! `docs/control-plane.md` §7〜§20 (Phase 1)。

pub mod engine;
pub mod model;
pub mod projection;
pub mod store;

#[cfg(test)]
mod tests;

#[allow(unused_imports)]
pub use model::{AgentKind, AgentView, Observation, Snapshot};
pub use store::FleetStore;
