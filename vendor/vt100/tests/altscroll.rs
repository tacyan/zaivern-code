//! 代替画面 (alternate screen) の履歴。
//!
//! zaivern patch の番人。上流の vt100 は `alternate_grid` を
//! `Grid::new(size, 0)` で作る = 代替画面に履歴を持たない。
//! **バージョンを上げるときは、このテストが赤くなったらパッチを移植し直すこと。**
//!
//! なぜ持たせたか: Claude Code のようなストリーム出力するエージェントは
//! 代替画面のまま画面を下へ流す。履歴が 0 だと流れた行はその場で消え、
//! 「Shell では上へドラッグして過去の出力まで選択できるのに、
//!  エージェントでは画面に見えているぶんしか選択できない」差になる。

/// 代替画面へ入り、画面より多くの行を流して、履歴が積まれることを確かめる。
#[test]
fn 代替画面でも履歴が積まれる() {
    let mut p = vt100::Parser::new(4, 20, 100);
    p.process(b"\x1b[?1049h"); // 代替画面へ
    assert!(p.screen().alternate_screen(), "代替画面に入っていない");
    for i in 0..20 {
        p.process(format!("line{i}\r\n").as_bytes());
    }
    p.set_scrollback(10);
    assert!(
        p.screen().scrollback() > 0,
        "代替画面に履歴が 1 行も積まれていない (パッチが外れている)"
    );
}

/// 対照: 通常画面。ここが落ちるなら原因は代替画面ではない。
#[test]
fn 通常画面では履歴が積まれる_対照() {
    let mut p = vt100::Parser::new(4, 20, 100);
    for i in 0..20 {
        p.process(format!("line{i}\r\n").as_bytes());
    }
    p.set_scrollback(10);
    assert!(p.screen().scrollback() > 0, "通常画面ですら積まれていない");
}

/// **全画面 TUI (vim / less) の出力は 1 行も積まない。**
///
/// 積む枝は `Grid::scroll_up` の `!scroll_region_active()` 側だけ。
/// スクロール領域を設定して流すアプリは、画面を描き直しているだけなので
/// 履歴に入れてはいけない (入れると同じ画面の残骸が延々と溜まる)。
#[test]
fn スクロール領域を使う全画面アプリは履歴を汚さない() {
    let mut p = vt100::Parser::new(6, 20, 100);
    p.process(b"\x1b[?1049h");
    // DECSTBM: 1〜5 行だけをスクロール領域にする (6 行目 = ステータス行を残す、
    // vim / less が実際にやる形)。**画面全体を指定すると領域なしと同じ**なので
    // (`scroll_region_active` は上端 0 かつ下端 = 最終行を「なし」と見る)、
    // ここを 1;6 にするとこのテストは意味を失う。
    p.process(b"\x1b[1;5r");
    for i in 0..30 {
        p.process(format!("frame{i}\r\n").as_bytes());
    }
    p.set_scrollback(10);
    assert_eq!(
        p.screen().scrollback(),
        0,
        "スクロール領域を使うアプリの描き直しが履歴に入っている"
    );
}

/// 代替画面へ**入り直したら**前のアプリの履歴は残さない。
/// 残すと、別のエージェント (や vim) の出力が新しいアプリの過去として見える。
#[test]
fn 代替画面へ入り直すと前の履歴は捨てられる() {
    let mut p = vt100::Parser::new(4, 20, 100);
    p.process(b"\x1b[?1049h");
    for i in 0..20 {
        p.process(format!("old{i}\r\n").as_bytes());
    }
    p.set_scrollback(10);
    assert!(p.screen().scrollback() > 0, "前提: 履歴が積まれていること");

    p.process(b"\x1b[?1049l"); // 抜ける
    p.process(b"\x1b[?1049h"); // 入り直す
    p.set_scrollback(10);
    assert_eq!(
        p.screen().scrollback(),
        0,
        "入り直したのに前のアプリの履歴が残っている"
    );
}
