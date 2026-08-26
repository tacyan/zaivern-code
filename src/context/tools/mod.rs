//! 道具 — 1 つの入力を 1 つの「渡せる文脈」へ畳む処理。
//!
//! ## この層が持たないもの
//!
//! * **通信**: MCP / JSON-RPC / stdio は 1 バイトも無い。道具は
//!   `&ToolContext` と型付きの引数を受け、[`Rendered`] を返すだけ。
//! * **エージェントの知識**: どの CLI 向けかを一切見ない
//!   ([`super::tests::コアはエージェント名を知らない`] が走査で見張る)。
//! * **上限の出どころ**: 環境変数も設定ファイルも読まない。上限は
//!   [`super::ContextLimits`] として**渡される**。
//!
//! 元にした `token-slim-mcp` では、この 3 つが `tools.rs` の中で
//! `env::var("TOKEN_SLIM_…")` と `json!({"content": …})` として混ざっていた。
//! 分けた理由は将来 core crate として切り出せるようにするためで、
//! **切り出せる形かどうかは「この層が何を知らないか」で決まる**。

pub mod directory;
pub mod grep;
pub mod json;
pub mod read;
pub mod refs;
pub mod text;

use super::walk::Workspace;
use super::ContextLimits;

/// 道具が受け取る環境。
pub struct ToolContext<'a> {
    /// 触ってよい範囲。
    pub workspace: &'a Workspace,
    /// 上限。**呼び出し側が決めて渡す**。
    pub limits: &'a ContextLimits,
}

/// 道具の出力。ヘッダの組み立てと打ち切りは [`super::engine`] が行う。
#[derive(Debug)]
pub struct Rendered {
    /// ヘッダに載せる説明 (`src/a.rs strategy=outline lines=1200` 等)。
    pub detail: String,
    /// 本文。
    pub body: String,
    /// **最適化しなかった場合**のトークン数。削減率の分母になる。
    pub original_tokens: usize,
    /// 本文の末尾に足す案内 (次に何をすればよいか)。空でよい。
    pub hint: String,
}

impl Rendered {
    /// 説明と本文だけの結果 (元のトークン数は本文と同じ = 削減 0)。
    pub fn plain(detail: String, body: String) -> Self {
        let original_tokens = super::metrics::estimate_tokens(&body);
        Self {
            detail,
            body,
            original_tokens,
            hint: String::new(),
        }
    }
}
