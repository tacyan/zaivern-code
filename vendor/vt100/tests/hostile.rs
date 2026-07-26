//! 敵対的な端末バイト列に対して **debug ビルドでも panic しない** ことを保証する。
//!
//! 実運用で踏んだ事故はすべてこの形だった:
//! PTY 読取スレッドが `parser.process()` の中で panic
//!   → スレッドが死んで端末が更新されなくなる (= 固まって見える)
//!   → さらに Mutex が poison し、壊れた内部状態のまま描画側へ渡るので
//!      毎フレーム panic → そのタイルが隔離されて**真っ黒のまま**戻らない。
//!
//! したがって vt100 側の不変条件は「どんな入力でも絶対に panic しない」。
//! debug ビルドでは整数の桁溢れも panic するため、`cargo test` (debug) で
//! 回すこと自体がチェックになっている。

use std::io::Write as _;

/// 決定論的な擬似乱数 (xorshift64*)。テストの再現性のため rand は使わない。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

/// 端末エミュレータが実際に浴びる「意地悪な」断片。
/// CSI の引数は 0 / 巨大値 / 負記号 / 引数過多をわざと混ぜている。
const HOSTILE: &[&[u8]] = &[
    // カーソル移動 (0 引数・巨大引数・引数過多)
    b"\x1b[0;0H",
    b"\x1b[65535;65535H",
    b"\x1b[999999;999999H",
    b"\x1b[1;2;3;4;5;6;7;8;9H",
    b"\x1b[0G",
    b"\x1b[65535G",
    b"\x1b[0d",
    b"\x1b[65535d",
    b"\x1b[65535A",
    b"\x1b[65535B",
    b"\x1b[65535C",
    b"\x1b[65535D",
    // スクロール領域 (DECSTBM) — 逆転・0・範囲外
    b"\x1b[r",
    b"\x1b[0;0r",
    b"\x1b[1;1r",
    b"\x1b[30;2r",
    b"\x1b[65535;1r",
    b"\x1b[2;65535r",
    // 挿入・削除・消去
    b"\x1b[65535L",
    b"\x1b[65535M",
    b"\x1b[65535P",
    b"\x1b[65535@",
    b"\x1b[65535X",
    b"\x1b[65535S",
    b"\x1b[65535T",
    b"\x1b[3J",
    b"\x1b[2J",
    b"\x1b[1J",
    b"\x1b[0K",
    b"\x1b[1K",
    b"\x1b[2K",
    // 代替画面の出入り (TUI エージェントが頻繁に叩く)
    b"\x1b[?1049h",
    b"\x1b[?1049l",
    b"\x1b[?47h",
    b"\x1b[?47l",
    b"\x1b[?1047h",
    b"\x1b[?1048h",
    // カーソル保存・復元 (DECSC/DECRC と CSI s/u)
    b"\x1b7",
    b"\x1b8",
    b"\x1b[s",
    b"\x1b[u",
    // origin mode / 自動折返し / 各種モード
    b"\x1b[?6h",
    b"\x1b[?6l",
    b"\x1b[?7h",
    b"\x1b[?7l",
    b"\x1b[?25h",
    b"\x1b[?25l",
    b"\x1b[4h",
    b"\x1b[4l",
    b"\x1b[?2004h",
    b"\x1b[?2004l",
    // SGR (未知・巨大・truecolor・引数不足)
    b"\x1b[m",
    b"\x1b[0m",
    b"\x1b[1;4;7;9m",
    b"\x1b[38;2;255;128;0m",
    b"\x1b[38;2m",
    b"\x1b[38;5m",
    b"\x1b[38;5;255m",
    b"\x1b[48;2;1;2;3m",
    b"\x1b[99999m",
    b"\x1b[38;9;9;9;9;9;9m",
    // スクロール・改行・復帰・タブ
    b"\x1bD",
    b"\x1bM",
    b"\x1bE",
    b"\r\n",
    b"\n",
    b"\r",
    b"\t\t\t\t\t\t\t\t\t\t",
    b"\x08\x08\x08",
    b"\x0b\x0c",
    // OSC (タイトル・クリップボード・未終端)
    b"\x1b]0;title\x07",
    b"\x1b]2;\x1b\\",
    b"\x1b]52;c;aGVsbG8=\x07",
    b"\x1b]777;unknown;stuff\x07",
    b"\x1b]0;unterminated",
    // 全角・CJK・結合文字・絵文字 (幅計算の境界)
    "あいうえお漢字テスト".as_bytes(),
    "👨‍👩‍👧‍👦🇯🇵🏳️‍🌈".as_bytes(),
    "e\u{0301}a\u{0300}\u{0301}\u{0302}\u{0303}".as_bytes(),
    "ｱｲｳｴｵ".as_bytes(),
    "\u{200b}\u{200d}\u{feff}".as_bytes(),
    // 不正 UTF-8 / 生バイト
    b"\xff\xfe\xfd",
    b"\xe3\x81",
    b"\xf0\x9f\x98",
    b"\xc0\x80",
    // 未終端 / 壊れたエスケープ
    b"\x1b",
    b"\x1b[",
    b"\x1b[;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;m",
    b"\x1b[?",
    b"\x1bP",
    b"\x1bPtest\x1b\\",
    b"\x1bX\x1b\\",
    b"\x1b^\x1b\\",
    b"\x1b_\x1b\\",
    b"\x1b#8",
    b"\x1b(0",
    b"\x1b(B",
    b"\x1bc",
    // 長い本文 (スクロールバック生成)
    b"0123456789abcdefghijklmnopqrstuvwxyz",
];

/// 端末サイズの候補。1x1 のような極端値まで含める
/// (Cockpit がタイルを畳む途中で 1 行 1 桁まで縮むことがある)。
const SIZES: &[(u16, u16)] = &[
    (1, 1),
    (1, 80),
    (24, 1),
    (2, 2),
    (3, 20),
    (24, 80),
    (41, 41),
    (60, 200),
    (5, 5),
];

/// 画面全体を触るアクセサ。描画側 (app.rs) が毎フレーム呼ぶものと同じ経路。
fn touch_everything(parser: &vt100::Parser) {
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let _ = screen.contents();
    let _ = screen.contents_formatted();
    let _ = screen.title();
    let _ = screen.icon_name();
    let _ = screen.cursor_position();
    let _ = screen.hide_cursor();
    let _ = screen.alternate_screen();
    let _ = screen.application_cursor();
    let _ = screen.bracketed_paste();
    // 範囲内・範囲外どちらの座標にも耐えること。
    // 全升を舐めると大きい端末でテストが遅くなるだけなので、
    // 事故が出るのは常に「端」と「範囲外」なのでそこを厚く見る。
    let rr = [0, 1, rows / 2, rows.saturating_sub(2), rows.saturating_sub(1), rows, rows + 1];
    let cc = [0, 1, cols / 2, cols.saturating_sub(2), cols.saturating_sub(1), cols, cols + 1];
    for row in rr {
        for col in cc {
            let _ = screen.cell(row, col);
        }
    }
    let _ = screen.rows(0, cols).count();
    let _ = screen.rows_formatted(0, cols).count();
    let _ = screen.contents_between(0, 0, rows, cols);
}

/// 書き込みとリサイズとスクロールバック操作をランダムに織り交ぜる本体。
fn hammer(seed: u64, iters: usize) {
    let mut rng = Rng::new(seed);
    let scrollback = [0usize, 1, 10, 5000][rng.below(4) as usize];
    let (r, c) = SIZES[rng.below(SIZES.len() as u64) as usize];
    let mut parser = vt100::Parser::new(r, c, scrollback);

    for i in 0..iters {
        match rng.below(16) {
            // 大半は「敵対的な断片を書く」
            0..=9 => {
                let frag = HOSTILE[rng.below(HOSTILE.len() as u64) as usize];
                parser.process(frag);
            }
            // 書き込みの途中でリサイズが割り込む (ConPTY の実挙動)
            10 | 11 => {
                let (rows, cols) = SIZES[rng.below(SIZES.len() as u64) as usize];
                parser.set_size(rows, cols);
            }
            // スクロールバックの読み位置を動かす (ユーザーのホイール操作)
            12 => {
                parser.set_scrollback(rng.below(10_000) as usize);
            }
            // 大量出力でスクロールバックを溢れさせる
            13 => {
                let mut buf = Vec::new();
                for n in 0..64u32 {
                    let _ = write!(buf, "line {n} 漢字あ\r\n");
                }
                parser.process(&buf);
            }
            // 1 バイトずつ流し込む (vte のステートマシンを分断させる)
            14 => {
                let frag = HOSTILE[rng.below(HOSTILE.len() as u64) as usize];
                for b in frag {
                    parser.process(&[*b]);
                }
            }
            // 描画側と同じ全面アクセス
            _ => touch_everything(&parser),
        }
        // 数回に一度は必ず描画経路を通す (壊れた状態を持ち越さないため)
        if i % 32 == 0 {
            touch_everything(&parser);
        }
    }
    touch_everything(&parser);
}

/// ランダム化ファズ: どの種でも panic しない。
///
/// 種を小分けにして複数の `#[test]` へ散らす。cargo test はテスト単位で
/// 並列に走るので、1 本の巨大ループにするより実時間がずっと短くなる
/// (CI の待ち時間を伸ばさないため)。
macro_rules! fuzz_seeds {
    ($($name:ident => $lo:expr, $hi:expr;)*) => {
        $(
            #[test]
            fn $name() {
                for seed in $lo..=$hi {
                    hammer(seed, 3000);
                }
            }
        )*
    };
}

fuzz_seeds! {
    hostile_bytes_never_panic_seeds_01_04 => 1u64, 4u64;
    hostile_bytes_never_panic_seeds_25_28 => 25u64, 28u64;
    hostile_bytes_never_panic_seeds_29_32 => 29u64, 32u64;
    hostile_bytes_never_panic_seeds_05_08 => 5u64, 8u64;
    hostile_bytes_never_panic_seeds_09_12 => 9u64, 12u64;
    hostile_bytes_never_panic_seeds_13_16 => 13u64, 16u64;
    hostile_bytes_never_panic_seeds_17_20 => 17u64, 20u64;
    hostile_bytes_never_panic_seeds_21_24 => 21u64, 24u64;
}

/// 全ての敵対断片 × 指定サイズの総当たり (ランダムでは踏み損ねる組合せを潰す)。
fn every_fragment_at(sizes: &[(u16, u16)]) {
    for &(rows, cols) in sizes {
        for frag in HOSTILE {
            let mut p = vt100::Parser::new(rows, cols, 100);
            p.process(frag);
            touch_everything(&p);
            // 断片のあとにリサイズが来ても壊れない (全サイズへ総当たり)
            for &(r2, c2) in SIZES {
                let mut p2 = vt100::Parser::new(rows, cols, 100);
                p2.process(frag);
                p2.set_size(r2, c2);
                p2.process(frag);
                touch_everything(&p2);
            }
        }
    }
}

/// 総当たりもサイズごとに分けて並列に流す。
#[test]
fn every_hostile_fragment_on_tiny_sizes() {
    every_fragment_at(&[(1, 1), (1, 80), (24, 1), (2, 2)]);
}

#[test]
fn every_hostile_fragment_on_normal_sizes() {
    every_fragment_at(&[(3, 20), (24, 80), (5, 5)]);
}

#[test]
fn every_hostile_fragment_on_wide_sizes() {
    every_fragment_at(&[(41, 41), (60, 200)]);
}

/// スクロール領域を張ったまま画面を縮める — タイルを閉じた直後に起きる並び。
#[test]
fn shrink_while_scroll_region_active() {
    for &(rows, cols) in SIZES {
        for top in [1u16, 2, 5, 30] {
            for bottom in [1u16, 3, 10, 65535] {
                let mut p = vt100::Parser::new(60, 200, 1000);
                p.process(format!("\x1b[{top};{bottom}r").as_bytes());
                p.process(b"hello\r\nworld\r\n");
                p.set_size(rows, cols);
                p.process(b"\x1b[1L\x1b[1M\x1b[1S\x1b[1T\r\n");
                touch_everything(&p);
                p.set_size(60, 200);
                p.process(b"more\r\n");
                touch_everything(&p);
            }
        }
    }
}

/// 代替画面に入ったまま縮小 → 復帰。DECSC/DECRC が範囲外を復元する並び。
#[test]
fn alt_screen_resize_roundtrip() {
    for &(rows, cols) in SIZES {
        let mut p = vt100::Parser::new(40, 120, 2000);
        p.process(b"\x1b[40;120H");
        p.process(b"\x1b7"); // DECSC: 画面の一番端を保存
        p.process(b"\x1b[?1049h"); // 代替画面へ
        p.process("代替画面のテキスト\r\n".repeat(50).as_bytes());
        p.set_size(rows, cols);
        touch_everything(&p);
        p.process(b"\x1b[?1049l"); // 通常画面へ戻る
        p.process(b"\x1b8"); // DECRC: 範囲外かもしれない位置を復元
        p.process(b"after restore\r\n");
        touch_everything(&p);
    }
}

/// 全角文字が右端を跨ぐ位置で縮小する (wide continuation の整合性)。
#[test]
fn wide_glyph_at_edge_then_shrink() {
    for cols in 1u16..=12 {
        for pad in 0u16..12 {
            let mut p = vt100::Parser::new(5, 12, 100);
            p.process(&b" ".repeat(usize::from(pad)));
            p.process("漢".as_bytes());
            p.set_size(5, cols);
            touch_everything(&p);
            p.process("字".as_bytes());
            p.set_size(5, 12);
            touch_everything(&p);
        }
    }
}

/// 深いスクロールバックを持ったまま縮小し、履歴の奥まで覗く
/// (vendor パッチの元になった visible_rows の回帰テスト)。
#[test]
fn deep_scrollback_then_shrink_and_scroll() {
    let mut p = vt100::Parser::new(24, 80, 5000);
    let mut buf = Vec::new();
    for n in 0..6000u32 {
        let _ = write!(buf, "row {n} 日本語の行 with tail\r\n");
    }
    p.process(&buf);
    for &(rows, cols) in SIZES {
        p.set_size(rows, cols);
        for back in [0usize, 1, 100, 4999, 5000, 100_000] {
            p.set_scrollback(back);
            touch_everything(&p);
        }
    }
}

/// **実機の事故そのもの**の回帰テスト。極端なサイズではなく、Cockpit が
/// 普通に使う桁数 (20〜200) だけで再現する。
///
/// 並び:
///   1. 全角文字が 41〜42 桁目 (添字 40〜41) に置かれる
///   2. エージェントを削除 → タイルの再配置で端末が 41 桁へ縮む
///      → 行の升は**右から**捨てられ、全角の右半分だけが消える。
///        左半分は `is_wide()` を立てたまま最終列に残る。
///   3. 子プロセスがその位置へ描き直す
///
/// 修正前はここで
///   * `row.rs` の `clear_wide` が `cells[col + 1]` を触って添字範囲外
///     (**release ビルドでも panic**)
///   * `screen.rs` の `drawing_cell_mut(col + 1).unwrap()` が None
/// のどちらかで PTY 読取スレッドが即死していた。読取スレッドが死ぬと
/// その端末は二度と更新されず (= 固まる)、parser の Mutex も poison する。
#[test]
fn realistic_cockpit_shrink_splits_a_wide_glyph() {
    // 事故が出るのは「全角の右半分がちょうど切り落とされる幅」。
    // 幅を総当たりして、どの境界でも死なないことを見る。
    for start_col in 20u16..=79 {
        let mut p = vt100::Parser::new(30, 80, 5000);
        // 全角を start_col (1 始まり) へ置く
        p.process(format!("\x1b[1;{start_col}H").as_bytes());
        p.process("漢字".as_bytes());
        // タイルが縮む: 全角の右半分だけが落ちる幅へ
        p.set_size(30, start_col);
        touch_everything(&p);
        // 子プロセスが同じ場所へ描き直す (差分描画)
        p.process(format!("\x1b[1;{start_col}H").as_bytes());
        p.process(b"x");
        p.process(b"\x1b[1K\x1b[1P\x1b[1@");
        touch_everything(&p);
        // 元の幅へ戻す (タイルをもう一度開いた)
        p.set_size(30, 80);
        p.process("戻り\r\n".as_bytes());
        touch_everything(&p);
    }
}

/// 同じ並びを「行数が減る側」でも見る。エージェント削除でタイルが
/// 縦に伸び縮みするため、行の増減と全角の組合せも通る。
#[test]
fn realistic_cockpit_row_churn_with_wide_glyphs() {
    let mut p = vt100::Parser::new(30, 110, 5000);
    for step in 0..200u16 {
        let rows = 3 + step % 40;
        let cols = 20 + (step * 7) % 120;
        p.process("あ漢字ｱﾞe\u{0301}🇯🇵 mixed ".as_bytes());
        if step % 5 == 0 {
            p.process(b"\r\n");
        }
        if step % 7 == 0 {
            p.process(b"\x1b[?1049h");
        }
        if step % 11 == 0 {
            p.process(b"\x1b[?1049l");
        }
        p.set_size(rows, cols);
        touch_everything(&p);
    }
}

/// CI を赤くした条件そのもの: **1 桁幅の端末に全角文字**。
/// 元実装は `size.cols - width` (width == 2) で減算オーバーフローし、
/// debug ビルドでは panic、release では 65534 に化けて折返し判定が壊れていた。
/// `terminal::cjk_tests::wide_char_on_a_one_column_terminal_does_not_panic`
/// と対になる、パーサ単体の回帰テスト。
#[test]
fn wide_char_on_a_one_column_terminal_does_not_panic() {
    let mut p = vt100::Parser::new(5, 1, 100);
    p.process("漢字あいう".as_bytes());
    touch_everything(&p);
    p.set_size(1, 1);
    p.process("漢".as_bytes());
    touch_everything(&p);
    p.set_size(24, 80);
    p.process("漢".as_bytes());
    touch_everything(&p);
    p.set_size(3, 1);
    p.process("あ".as_bytes());
    touch_everything(&p);
    // 0 行 0 桁という理論上の下限 (タイルが畳まれ切った瞬間) も耐えること
    p.set_size(0, 0);
    p.process("漢字".as_bytes());
    touch_everything(&p);
    p.set_size(24, 80);
    p.process(b"ok\r\n");
    assert!(p.screen().contents().contains("ok"), "復帰後は普通に描けること");
}

/// **固まらないこと**の回帰テスト。
///
/// CSI の引数は 65535 まで取れる。元実装は `CSI 65535 @` (ICH) を素朴に
/// 65535 回の `Vec::insert` で回しており、**1 個のエスケープで約 2.8 秒**
/// かかっていた。これは PTY 読取スレッドが parser の Mutex を握ったまま起きるので、
/// 同じフレームで描こうとした UI スレッドがその間ずっと待たされる = アプリが固まる。
/// タイルを閉じて端末が縮み、TUI が全画面を描き直すときに実際に踏み得る並び。
///
/// 上限を超えた繰り返しは結果を変えないので、頭打ちにしても描画は同じ。
#[test]
fn huge_csi_counts_stay_fast() {
    use std::time::{Duration, Instant};

    // 実運用サイズ。深いスクロールバックも持たせる (履歴があるほど元実装は重い)。
    let mut p = vt100::Parser::new(60, 200, 5000);
    let mut buf = Vec::new();
    for n in 0..3000u32 {
        let _ = write!(buf, "row {n} 漢字テスト abcdefg\r\n");
    }
    p.process(&buf);

    // 1 個あたりの上限。実測は数十マイクロ秒なので桁で余裕を取る。
    // ここを超えるなら「繰り返し回数の頭打ち」が外れている。
    const BUDGET: Duration = Duration::from_millis(50);
    for seq in [
        &b"\x1b[65535@"[..], // ICH  挿入
        &b"\x1b[65535L"[..], // IL   行挿入
        &b"\x1b[65535M"[..], // DL   行削除
        &b"\x1b[65535P"[..], // DCH  削除
        &b"\x1b[65535X"[..], // ECH  消去
        &b"\x1b[65535S"[..], // SU   上スクロール
        &b"\x1b[65535T"[..], // SD   下スクロール
    ] {
        let t0 = Instant::now();
        for _ in 0..20 {
            p.process(seq);
        }
        let each = t0.elapsed() / 20;
        assert!(
            each < BUDGET,
            "{:?} が 1 回 {each:?} かかっている (上限 {BUDGET:?})。\
             繰り返し回数の頭打ちが外れると UI スレッドが待たされて固まる",
            String::from_utf8_lossy(seq)
        );
    }

    // スクロール領域を張った状態でも同じ。
    p.process(b"\x1b[5;40r");
    let t0 = Instant::now();
    for _ in 0..20 {
        p.process(b"\x1b[65535L\x1b[65535T\x1b[65535@");
    }
    assert!(
        t0.elapsed() / 20 < BUDGET * 3,
        "スクロール領域つきで遅い: {:?}",
        t0.elapsed() / 20
    );
}

/// 繰り返し回数の頭打ち (`huge_csi_counts_stay_fast` の対) が
/// **見た目を変えていない**ことの担保。
///
/// 「上限を超えた繰り返しは結果を変えない」が前提なので、
/// 巨大な引数と、上限そのものの引数とで画面が一致することを直接見る。
#[test]
fn clamping_repeat_counts_does_not_change_the_screen() {
    // 通常回数はこれまでどおり動く
    let mut p = vt100::Parser::new(3, 10, 0);
    p.process(b"abcdefghij\x1b[1;1H\x1b[3@");
    assert_eq!(
        p.screen().contents().lines().next().unwrap_or(""),
        "   abcdefg",
        "普通の ICH がずれてはいけない"
    );

    // 巨大な引数 == 「効き切る回数」の引数、を全ての繰り返し系 CSI で確認する。
    // (letter, 効き切る回数) — 5 行 8 桁の画面で飽和する値
    for (letter, saturating) in [
        (b'@', 8u16),  // ICH  桁数ぶんで行が空になる
        (b'L', 5),     // IL   行数ぶんで画面が空になる
        (b'M', 5),     // DL   同上
        (b'P', 8),     // DCH
        (b'X', 8),     // ECH
        (b'S', 5),     // SU
        (b'T', 5),     // SD
    ] {
        let seed: &[u8] = b"abcdefgh\r\nijklmnop\r\nqrstuvwx\r\nyz012345\x1b[2;3H";
        let mut big = vt100::Parser::new(5, 8, 0);
        big.process(seed);
        big.process(&[0x1b, b'[', b'6', b'5', b'5', b'3', b'5', letter]);

        let mut exact = vt100::Parser::new(5, 8, 0);
        exact.process(seed);
        exact.process(
            format!("\x1b[{saturating}{}", letter as char).as_bytes(),
        );

        assert_eq!(
            big.screen().contents(),
            exact.screen().contents(),
            "CSI 65535 {} が飽和回数と違う結果になっている",
            letter as char
        );
        assert_eq!(
            big.screen().cursor_position(),
            exact.screen().cursor_position(),
            "CSI 65535 {} でカーソル位置が違う",
            letter as char
        );
    }
}
