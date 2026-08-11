//! git blame の表示段階を**パレットから直に選ぶ**ための登録。
//!
//! 表示メニューの 1 項目 (`Cmd::ToggleGitBlame`) は 3 段を順に回すだけなので、
//! それだけだと「循環はするが選べない」になる。ここで 3 段それぞれを
//! 独立した項目として出し、**どの段へでも 1 手で行ける**ようにする。
//!
//! 実体 (状態の書き換え・永続化・トースト) は `ZaivernApp::set_blame_mode`
//! ひとつだけで、3 経路すべてがそこへ集まる。

use crate::config::BlameMode;
use crate::feature::{Entry, Feature};

pub const FEATURE: Feature = Feature {
    module: "blame",
    entries: &[
        Entry {
            icon: "👤",
            label: "Git blame: 出さない",
            id: "blame.off",
        },
        Entry {
            icon: "👤",
            label: "Git blame: カーソル行だけ",
            id: "blame.current",
        },
        Entry {
            icon: "👤",
            label: "Git blame: 全行",
            id: "blame.all",
        },
    ],
    dispatch: |app, _ctx, id| match id {
        "blame.off" => {
            app.set_blame_mode(BlameMode::Off);
            true
        }
        "blame.current" => {
            app.set_blame_mode(BlameMode::Current);
            true
        }
        "blame.all" => {
            app.set_blame_mode(BlameMode::All);
            true
        }
        _ => false,
    },
    ..Feature::DEFAULT
};
