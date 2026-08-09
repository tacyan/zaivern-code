//! ACP (Agent Client Protocol) クライアントの登録。
//!
//! 実体と `FEATURE` の定義は [`crate::acp`] にあるので、ここは**再エクスポート
//! するだけ**。実装ブランチは `feature.rs` の `REGISTRY` へ直接 1 行足して
//! いたが、統合側で build.rs 生成へ移行済みだったため、この形へ移した。
//!
//! 定義を写経して 2 か所に持つとズレるので、必ず `pub use` にすること。
//! (`draw: Some(acp_tick)` のような後から足された要素を取りこぼす)

pub use crate::acp::FEATURE;
