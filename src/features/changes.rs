//! 「変更一覧」(未コミットの変更をまとめて見る面) への**入口だけ**。
//! 実体は [`crate::multibuffer`] の `Source::Changes` と
//! `ZaivernApp::open_changes_multibuffer`。
//!
//! ## なぜ要るのか
//!
//! 面そのものは前からある (Zed の project diff 相当) のに、到達経路が
//! **コマンドパレットの 1 行だけ**で、しかもその見出しが
//! 「マルチバッファ: 未コミットの変更をまとめて直す」だった。
//! *マルチバッファ*は実装の名前であって利用者の言葉ではないので、
//! 「変更」「差分」「diff」を思い浮かべた人はまず辿り着けない。
//! **あるのに見つからない**は、無いのとほとんど同じである。
//!
//! ## 何を足して、何を足さなかったか
//!
//! * 足したのは **項目 1 つと打鍵 1 つだけ**。CLAUDE.md の
//!   「同じ操作への到達経路が 3 つあるなら 2 つ削る」に従い、
//!   「まとめて開く」以外の入口 (ステージ済みだけ / 直前のコミットとの差 …) は
//!   足していない。どれも `git` の呼び分けが要るので `app` 側のグルーが増え、
//!   増やしただけパレットが長くなって**また見つからなくなる**。
//! * **重複を作らない設計**: この項目は既存のパレット行
//!   `ZaivernApp::open_changes_multibuffer` へ直に行く。
//!
//! **以前あった `Cmd::OpenChangesMultibuffer` は消した。** 同じ面へ行く経路が
//! パレットの行とここで 2 つになり、`Cmd` の variant が誰からも作られなくなった
//! (`never used` 警告がそれを教えてくれた)。到達経路は「パレット 1 行 + 既定打鍵」
//! の 1 系統だけにしてある。
//!   同じ面への行が 2 本並ぶのは UI 原則に反するので、
//!   **既存の行は統合担当が消す**約束にしてある (パレットは並列ブランチが
//!   取り合う共有ファイルなので、ここからは触らない)。消えれば経路は
//!   「パレット 1 行 + 打鍵 1 つ」で、増えるのは打鍵だけになる。
//!
//! `main.rs` の `mod` 一覧にも `feature.rs` のレジストリにも触らない
//! (`build.rs` が `src/features/*.rs` を集める)。

use crate::feature::{Bind, Entry, Feature};

pub const FEATURE: Feature = Feature {
    module: "changes",
    entries: &[Entry {
        // 「変更」「差分」「diff」のどれで探しても当たる見出しにする。
        // パレットは前方一致ではなく曖昧一致なので、利用者が使う言葉を
        // 全部 1 行に入れておくのがいちばん効く。
        icon: "±",
        label: "変更一覧 — 未コミットの差分 (diff) をまとめて見て直す",
        id: "changes.open",
    }],
    dispatch: |app, _ctx, id| match id {
        "changes.open" => {
            app.open_changes_multibuffer();
            true
        }
        _ => false,
    },
    // 面は中央ビュー (エディタのタブ) に出るので、オーバーレイは持たない。
    binds: &[Bind {
        id: "changes.open",
        // ⇧⌥⌘D: D は Diff。⌘D / ⇧⌘D は既に埋まっており、⌥⌘D は macOS の
        // Dock が握っている (`MACOS_RESERVED`)。⇧⌥⌘ 系は空きが多い。
        // ⌘ と C/V/X の組みは egui-winit 0.29 が押下ごと飲み込むので使わない。
        default: "cmd+alt+shift+d",
    }],
    ..Feature::DEFAULT
};

#[cfg(test)]
mod tests {
    use super::*;

    /// 既定打鍵が予約表とも既存割り当てともぶつからない。
    ///
    /// 食い合うと実行時には**片方が黙って死ぬ**だけなので、統合前に落とす。
    /// (レジストリ全体の検査は `keybinds::tests::機能の既定打鍵は…` にもあるが、
    ///  こちらは「このモジュールの打鍵」だけを名指しで守る。)
    #[test]
    fn 既定打鍵は予約表とも既存割り当てともぶつからない() {
        let spec = FEATURE.binds[0].default;
        let b = crate::keybinds::parse_binding(spec)
            .unwrap_or_else(|| panic!("{spec:?} が parse_binding で読めない"));
        let first = b.first();

        // 1) macOS の実測予約表。**どの OS で走らせても表と突き合わせる**
        for (m, k, why) in crate::keybinds::MACOS_RESERVED {
            assert!(
                !crate::keybinds::same_stroke(egui::KeyboardShortcut::new(*m, *k), first),
                "{spec:?} は macOS が握っている: {why}"
            );
        }
        // 2) 既存の全アクション (chord の 1 打鍵目とぶつかっても死ぬ)
        for a in crate::keybinds::ALL_ACTIONS {
            let d = crate::keybinds::default_binding(a);
            assert!(
                !crate::keybinds::same_stroke(d.first(), first),
                "{spec:?} は {a:?} の既定打鍵とぶつかる"
            );
        }
        // 3) egui-winit 0.29 が押下ごと Cut/Copy/Paste へすり替える組み合わせ
        let swallowed = [egui::Key::X, egui::Key::C, egui::Key::V];
        assert!(
            !(first.modifiers.command && swallowed.contains(&first.logical_key)),
            "{spec:?} は egui-winit がイベントごと飲み込むので絶対に発火しない"
        );
    }

    #[test]
    fn 登録の識別子はモジュール接頭辞を持ち打鍵はそれを指す() {
        assert_eq!(FEATURE.module, "changes");
        for e in FEATURE.entries {
            assert!(e.id.starts_with("changes."), "{:?}", e.id);
        }
        for b in FEATURE.binds {
            assert!(
                FEATURE.entries.iter().any(|e| e.id == b.id),
                "打鍵 {:?} が指す ID が entries に無い (押しても何も起きない打鍵になる)",
                b.id
            );
        }
    }

    /// **入口は 1 つだけ。** 増やすときは「既存の経路を減らせないか」を
    /// 先に考える (CLAUDE.md)。ここが増えたらこのテストを直す前に理由を書くこと。
    #[test]
    fn 入口は一つだけに絞ってある() {
        assert_eq!(
            FEATURE.entries.len(),
            1,
            "経路を増やす前に、既存のパレット行を減らせないかを考えること"
        );
    }
}
