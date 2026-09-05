//! 🗣 エージェント同士の伝言 — **使い方を教える**入口。
//!
//! 配達そのものは [`crate::agent_talk`] と `app::orchestrate` が持つ。
//! ここは「エージェントにこの仕組みの存在を教える」1 手だけを足す。
//!
//! ## なぜ「教える」が要るのか
//!
//! Team Run では指示文に作法を載せているので、エージェントは自分から
//! 伝言を出せる。**通常タブにはその指示文が無い**ので、こちらがマーカーを
//! 待っていても永久に来ない (機能があっても到達経路が無いのと同じ)。
//!
//! そこで、いま選んでいるエージェントへ**作法と相手の一覧**を 1 回送る。
//! 送るのは人が押したときだけ — 勝手に注入すると、利用者が書いた文脈の
//! 上に頼んでいない指示が積まれる。

use crate::feature::{Entry, Feature};

/// いま選んでいるエージェントへ、伝言の作法を教える。
pub const ID_TEACH: &str = "agent_talk.teach";

pub const FEATURE: Feature = Feature {
    module: "agent_talk",
    entries: &[Entry {
        icon: "🗣",
        label: "エージェント同士の伝言の使い方を送る",
        id: ID_TEACH,
    }],
    dispatch: |app, _ctx, id| {
        if id != ID_TEACH {
            return false;
        }
        app.teach_agent_talk();
        true
    },
    ..Feature::DEFAULT
};
