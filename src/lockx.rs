//! Mutex ロックの poison 耐性ヘルパ。
//!
//! reader スレッド等が lock 保持中に panic すると Mutex は poison し、以後の
//! `lock().unwrap()` が UI スレッドで連鎖 panic してアプリ全体が落ちる。
//! ここでは poison を「他スレッドが panic した」という印としては受け取りつつ、
//! データ自体は最後に書かれた状態のまま使い続ける。

use std::sync::{Mutex, MutexGuard};

/// poison していても into_inner でガードを取り出し、poison 後もデータへアクセスして継続する lock。
pub(crate) fn lock_ok<'a, T>(m: &'a Mutex<T>) -> MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// 別スレッドが lock 保持中に panic して poison した Mutex からも値を回収できる。
    #[test]
    fn lock_ok_recovers_value_from_poisoned_mutex() {
        let m = Arc::new(Mutex::new(7_i32));
        let m2 = Arc::clone(&m);
        let joined = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison the mutex on purpose");
        })
        .join();
        assert!(joined.is_err());
        assert!(m.is_poisoned());
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
}
