use super::*;

#[test]
fn 未読が0件なら飛び先は無い() {
    assert_eq!(next_unread(&[], 0), None);
    assert_eq!(next_unread(&[false, false, false], 1), None);
}

#[test]
fn 未読が1件ならそれが今の相手でも返す() {
    assert_eq!(next_unread(&[true], 0), Some(0));
    assert_eq!(next_unread(&[false, true, false], 0), Some(1));
    // 今いる相手だけが未読 → 一巡して自分へ戻る
    // (「押しても何も起きないボタン」を作らない)
    assert_eq!(next_unread(&[false, true, false], 1), Some(1));
}

#[test]
fn 全部未読なら次の要素へ順に進む() {
    let all = [true, true, true];
    assert_eq!(next_unread(&all, 0), Some(1));
    assert_eq!(next_unread(&all, 1), Some(2));
}

#[test]
fn 端で折り返す() {
    let all = [true, true, true];
    assert_eq!(next_unread(&all, 2), Some(0));
    assert_eq!(next_unread(&[true, false, false], 2), Some(0));
    // 現在位置が範囲外 (セッションが減った直後) でも壊れない
    assert_eq!(next_unread(&[true, false], 99), Some(0));
}

#[test]
fn 未読に戻すと巡回の順序が変わる() {
    let mut flags = vec![false, true, false];
    assert_eq!(next_unread(&flags, 0), Some(1));
    // いまの相手 (0) を未読へ戻す = 後回し宣言
    flags[0] = true;
    assert_eq!(next_unread(&flags, 0), Some(1));
    assert_eq!(
        next_unread(&flags, 1),
        Some(0),
        "未読に戻した相手が巡回に入っていない"
    );
}

#[test]
fn 巡回はセッションの並び順で固定される() {
    // 通知の新しさで並べ替えない (cmux が「⌘1-9 の割当が動き続ける」と
    // 批判された轍を踏まない)。同じ入力なら毎回同じ順に回る。
    let flags = [true, false, true, false, true];
    let mut seen = Vec::new();
    let mut cur = 0usize;
    for _ in 0..5 {
        cur = next_unread(&flags, cur).expect("未読があるのに飛べない");
        seen.push(cur);
    }
    assert_eq!(seen, vec![2, 4, 0, 2, 4]);
}
