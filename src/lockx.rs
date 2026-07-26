//! Mutex ロックの poison 耐性ヘルパと、UI スレッドを絶対に待たせないための道具。
//!
//! # なぜ要るのか
//!
//! reader スレッド等が lock 保持中に panic すると Mutex は poison し、以後の
//! `lock().unwrap()` が UI スレッドで連鎖 panic してアプリ全体が落ちる。
//! ここでは poison を「他スレッドが panic した」という印としては受け取りつつ、
//! データ自体は最後に書かれた状態のまま使い続ける。
//!
//! ただし **poison を握り潰すだけでは足りない**。panic した相手が
//! `vt100::Parser::process()` の途中だった場合、中身は「書きかけ」で整合が
//! 崩れていることがあり、そのまま描画へ渡すと今度は UI スレッドが毎フレーム
//! panic する。フレーム保護がその小画面を隔離するので、ユーザーからは
//! **そのタイルだけが真っ黒のまま二度と戻らない**ように見える (実際の報告と一致)。
//!
//! そこで方針を 3 段に分ける:
//!
//! | 用途 | 関数 | 方針 |
//! |---|---|---|
//! | 中身が壊れていても困らない (数値・フラグ等) | [`lock_ok`] | poison を無視して続行 |
//! | 中身が壊れると描画が死ぬ (vt100 パーサ等) | [`lock_rebuilding`] | **作り直して** poison を解除し、呼び出し側へ「作り直した」と伝える |
//! | 相手がブロッキング OS 呼び出しを抱えている (ConPTY の master 等) | [`try_lock_ok`] | **待たない**。取れなければ諦める |
//!
//! [`lock_rebuilding`] は作り直したあと [`Mutex::clear_poison`] で毒を落とす。
//! 落とさないと毎回「作り直し」判定になり、画面が永久に白紙のままになる。

use std::sync::{Mutex, MutexGuard, TryLockError};

/// poison していても into_inner でガードを取り出し、poison 後もデータへアクセスして継続する lock。
///
/// 中身の整合が崩れていても致命的でないもの (カウンタ、`Option<String>` の受け渡し箱、
/// 終了コードなど) 向け。パーサのように「壊れた状態で読むと描画側が死ぬ」ものには
/// [`lock_rebuilding`] を使うこと。
pub(crate) fn lock_ok<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// **待たない** lock。poison していても中身は取り出す。
///
/// UI スレッドが「別スレッドがブロッキング OS 呼び出しを抱えたまま握っているかもしれない」
/// Mutex へ触るときに使う。Windows の `ResizePseudoConsole` / `ClosePseudoConsole` は
/// conhost への同期 RPC で、子が出力を吐き切るまで返らないことがある。そこを
/// `lock()` で待つと `App::update` の途中でウィンドウごと固まる。
/// 取れなければ `None` — 呼び出し側は「今フレームは諦めて次で試す」を選ぶ。
#[allow(dead_code)] // 配線は terminal.rs 側 (パッチ仕様を参照)。先に道具だけ置く。
pub(crate) fn try_lock_ok<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(g) => Some(g),
        // 毒は他スレッドの panic の跡。中身は最後に書かれた状態のまま使える。
        Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// poison していたら `rebuild` で中身を作り直してからガードを返す。
///
/// 戻り値の `bool` が `true` のとき「作り直した」= 呼び出し側は
/// **1 行のお知らせを画面に出す**こと (端末なら履歴が消えた旨)。黙って捨てると
/// ユーザーには「勝手にスクロールバックが消えた」としか見えない。
///
/// 作り直したあとは [`Mutex::clear_poison`] で毒を落とす。落とさないと以後
/// 毎回この関数が「作り直し」を選んでしまい、画面が永久に空のままになる。
/// (`clear_poison` は Rust 1.77 で安定化。本プロジェクトは 1.88+ を要求する)
#[allow(dead_code)] // 配線は terminal.rs 側 (パッチ仕様を参照)。先に道具だけ置く。
pub(crate) fn lock_rebuilding<T>(
    m: &Mutex<T>,
    rebuild: impl FnOnce(&mut T),
) -> (MutexGuard<'_, T>, bool) {
    match m.lock() {
        Ok(g) => (g, false),
        Err(e) => {
            let mut g = e.into_inner();
            rebuild(&mut g);
            m.clear_poison();
            (g, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// 別スレッドが lock 保持中に panic して poison した Mutex を作る。
    fn poisoned(v: i32) -> Arc<Mutex<i32>> {
        let m = Arc::new(Mutex::new(v));
        let m2 = Arc::clone(&m);
        let joined = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison the mutex on purpose");
        })
        .join();
        assert!(joined.is_err());
        assert!(m.is_poisoned());
        m
    }

    /// 別スレッドが lock 保持中に panic して poison した Mutex からも値を回収できる。
    #[test]
    fn lock_ok_recovers_value_from_poisoned_mutex() {
        let m = poisoned(7);
        assert_eq!(*lock_ok(&m), 7);
    }

    /// poison 前に書かれた値が見え、poison 後の書き込みも通常どおり反映される。
    #[test]
    fn lock_ok_allows_writes_after_poison() {
        let m = Arc::new(Mutex::new(0_i32));
        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let mut g = m2.lock().unwrap();
            *g = 41;
            panic!("poison after write");
        })
        .join();
        assert!(m.is_poisoned());
        *lock_ok(&m) += 1;
        assert_eq!(*lock_ok(&m), 42);
    }

    /// poison していないときは作り直さない (履歴を無駄に捨てない)。
    #[test]
    fn lock_rebuilding_leaves_healthy_mutex_alone() {
        let m = Mutex::new(5_i32);
        let (g, rebuilt) = lock_rebuilding(&m, |v| *v = -1);
        assert_eq!(*g, 5);
        assert!(!rebuilt, "健全な Mutex を作り直してはいけない");
    }

    /// poison していたら作り直し、作り直した旨を返す。
    #[test]
    fn lock_rebuilding_replaces_contents_of_poisoned_mutex() {
        let m = poisoned(999);
        let (g, rebuilt) = lock_rebuilding(&m, |v| *v = 0);
        assert!(rebuilt, "poison は呼び出し側へ伝えること");
        assert_eq!(*g, 0, "中身は作り直された値であること");
    }

    /// **回復は 1 回だけ**。毒を落とすので次からは作り直さない。
    /// ここが抜けると毎フレーム作り直しになり、端末が永久に空になる。
    #[test]
    fn lock_rebuilding_clears_poison_so_recovery_happens_once() {
        let m = poisoned(1);
        let (g, first) = lock_rebuilding(&m, |v| *v = 100);
        assert!(first);
        drop(g);
        assert!(!m.is_poisoned(), "回復後は毒を落としておくこと");

        // 2 回目以降は普通の lock として振る舞い、中身を保つ。
        *lock_ok(&m) += 23;
        let (g, second) = lock_rebuilding(&m, |v| *v = -1);
        assert!(!second, "1 度回復したら以後は作り直さない");
        assert_eq!(*g, 123, "回復後に書いた内容が消えてはいけない");
    }

    /// 回復後にもう一度 poison したら、また作り直す (毒は再度立つ)。
    #[test]
    fn lock_rebuilding_recovers_again_after_second_poison() {
        let m = poisoned(1);
        let (g, _) = lock_rebuilding(&m, |v| *v = 0);
        drop(g);

        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("二度目");
        })
        .join();
        assert!(m.is_poisoned());
        let (g, rebuilt) = lock_rebuilding(&m, |v| *v = 42);
        assert!(rebuilt);
        assert_eq!(*g, 42);
    }

    /// 他スレッドが握っている間、`try_lock_ok` は **待たずに** None を返す。
    /// UI スレッドがブロッキング OS 呼び出しに巻き込まれないことの担保。
    #[test]
    fn try_lock_ok_never_blocks_on_a_held_lock() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let m = Arc::new(Mutex::new(0_i32));
        let m2 = Arc::clone(&m);
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            held_tx.send(()).unwrap();
            // 「ブロッキング RPC の最中」を模す。解放の合図が来るまで握り続ける。
            let _ = release_rx.recv();
        });
        held_rx.recv().unwrap();

        let t0 = Instant::now();
        assert!(
            try_lock_ok(&m).is_none(),
            "他スレッドが握っている間は取れないこと"
        );
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "try_lock_ok が待ってしまっている: {:?}",
            t0.elapsed()
        );

        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(try_lock_ok(&m).is_some(), "解放後は取れること");
    }

    /// poison していても `try_lock_ok` は中身を返す (空いてさえいれば)。
    #[test]
    fn try_lock_ok_recovers_from_poison() {
        let m = poisoned(11);
        assert_eq!(*try_lock_ok(&m).expect("空いているので取れる"), 11);
    }
}
