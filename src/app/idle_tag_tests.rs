//! アイドル予約の**出所タグ**が、予約の判断とずれていないことの番人。
//!
//! `idle_repaint_ms` (何 ms 後に回すか) と `idle_repaint_tag` (なぜ回すか) は
//! 同じ優先順位を 2 か所に書いている。片方だけ直すと、**数字は正しいのに
//! 出所だけ嘘**になり、`perf::dump` を読んで犯人を追う人が別の場所を掘る。
//! 実際にこの版で「見張りを降ろしたのに数字が 1 つも動かない」の犯人
//! (interval フックの期限) を、1 本しか無かったタグのせいで見つけ損ねた。

use super::*;

fn signals_of(bits: u8) -> IdleSignals {
    IdleSignals {
        had_input: bits & 1 != 0,
        animating: bits & 2 != 0,
        awaiting: bits & 4 != 0,
        agents_running: bits & 8 != 0,
        watching_files: bits & 16 != 0,
        timer_due_in_ms: (bits & 32 != 0).then_some(1_000),
        focused: bits & 64 != 0,
        visible: bits & 128 != 0,
    }
}

/// **予約する / しない と、タグの有無が必ず一致する。**
///
/// 全 256 通りで突き合わせる (組み合わせの数が小さいので全部見る)。
#[test]
fn 予約する理由とタグが必ず一致する() {
    for bits in 0..=u8::MAX {
        let s = signals_of(bits);
        let ms = idle_repaint_ms(s);
        let tag = idle_repaint_tag(s);
        assert_eq!(
            ms.is_some(),
            tag != "idle.none" && tag != "idle.animating",
            "{s:?}: ms={ms:?} tag={tag}"
        );
        assert!(tag.starts_with("idle."), "{s:?}: {tag}");
    }
}

/// タグは理由ごとに割れている (1 本にまとめない)。
#[test]
fn 理由ごとにタグが割れている() {
    let base = IdleSignals {
        focused: true,
        visible: true,
        ..Default::default()
    };
    let mut seen = std::collections::BTreeSet::new();
    for s in [
        IdleSignals {
            awaiting: true,
            ..base
        },
        IdleSignals {
            agents_running: true,
            ..base
        },
        IdleSignals {
            watching_files: true,
            ..base
        },
        IdleSignals {
            timer_due_in_ms: Some(1_000),
            ..base
        },
    ] {
        assert!(seen.insert(idle_repaint_tag(s)), "{s:?} のタグが他と同じ");
    }
    assert_eq!(seen.len(), 4);
}
