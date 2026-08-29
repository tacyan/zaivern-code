//! **AI 開発チーム制御面 (Team Control Plane)。**
//!
//! SPEC.md を渡すだけで、Zaivern が AI 開発チームを編成し、タスク分解 →
//! 担当割当 → 並列実装 → 検証 → レビュー → 修正 → 統合まで面倒を見る層。
//!
//! ## 層の分け方
//!
//! ```text
//!   SPEC / Goal
//!       ↓ planner.rs      (SPEC → 計画。LLM 差し替え可能な境界)
//!   TeamPlan / Task Graph  plan_schema.rs / graph.rs / model.rs
//!       ↓ runtime.rs      (Reconciliation Loop — egui を知らない)
//!   Deterministic Scheduler scheduler.rs
//!       ↓
//!   既存 Coordinator       crate::coordinator (安全制御はここが持つ)
//!       ↓
//!   既存 Session / Terminal / Lease / Czero
//! ```
//!
//! ## 何を作り直していないか (重要)
//!
//! 次は**既存のものをそのまま使う**。並行実装を作らない:
//!
//! * エージェント起動・terminal tile — `crate::agents` / `crate::terminal`
//! * `SessionState` の導出 — `crate::app::coordinator_state`
//! * 割り当ての可否・ファイル重なりの fail-closed 判定・前任者停止の確認 —
//!   `crate::coordinator::{admit, try_assign}`
//! * パターンの重なり判定と正規化 — `crate::lease::{overlaps, normalize_path}`
//! * 承認ゲート / quota / cost 上限 — 既存の approval / quota 経路
//! * ワークスペースキーと置き場 — `crate::history::workspace_key`
//!
//! ## 守っている約束
//!
//! * **描画中に副作用を起こさない。** Runtime は [`runtime::TeamEffect`] を
//!   返すだけで、実行は app 側の安全な場所が行う。
//! * **「完了と言った」で完了にしない。** 構造化報告 → 検証 → レビュー承認を
//!   通ったものだけが `Completed` になる ([`state_machine`] が抜け道を塞ぐ)。
//! * **同じ入力に同じ割り当て。** [`scheduler`] は純関数。
//! * **人へ上げる操作は自動実行しない。** push / merge / deploy /
//!   権限昇格 / 破壊的操作は [`graph::check_command`] が止める。
//!
//! 使い方と設計の全体像は `docs/team.md`。

pub mod cli;
pub mod graph;
pub mod inspector;
pub mod launch;
pub mod model;
pub mod organization_board;
pub mod panel;
pub mod persistence;
pub mod plan_schema;
pub mod planner;
pub mod prompt;
pub mod result_parser;
pub mod reviewer;
pub mod roles;
pub mod runtime;
pub mod scheduler;
pub mod state_machine;
pub mod validation_command;
pub mod view_model;

#[cfg(test)]
mod testkit;

#[cfg(test)]
mod e2e_tests;

#[cfg(test)]
mod runtime_tests;

#[cfg(test)]
mod wiring_tests;
