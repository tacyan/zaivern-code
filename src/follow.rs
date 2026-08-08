//! # Follow the agent — 追従中のエージェントが触っている行へエディタを寄せる
//!
//! 「いま何をしているか」を**新しいパネルを増やさずに**見せるための仕掛け。
//! Zed の "follow" と同じ発想で、追従先は **1 体だけ**に絞る。
//!
//! ## どこから行番号を取るか (設計原則 4)
//!
//! 画面 (ピクセル) からは**推測しない**。段位は上から順に:
//!
//! 1. `git status --porcelain=v1 -z` ([`crate::worktree::scan_touched`]) —
//!    「どのファイルを触ったか」の構造化された事実。
//! 2. ファイルの mtime — 触ったファイルが複数あるとき「どれが直近か」を決める。
//!    git は時刻を持たないので、ここだけはファイルシステムに聞く。
//! 3. `git diff -U0 -- <file>` の hunk ヘッダ `@@ -a,b +c,d @@` —
//!    「新しい側の何行目か」。[`last_hunk_line`] が純関数で取り出す。
//!
//! 端末の出力を読んで "Edit(src/foo.rs:120)" を拾うようなことはしていない。
//!
//! ## アイドル時のコスト (設計原則 3)
//!
//! * 追従が**オフなら git を 1 回も叩かない** — [`Follow::tick`] が
//!   走査クロージャを一度も呼ばずに戻る (`follow_tests` が回数で固定している)。
//! * オンでも [`SCAN_INTERVAL`] のスロットリングが掛かり、走査は**別スレッド**。
//!   UI スレッドは `try_recv` するだけで待たない。
//! * 自分から `request_repaint` を呼ばない。追従が意味を持つのは
//!   「エージェントが走っている」ときだけで、その間は `idle_repaint_ms` が
//!   既に 250ms 刻みを予約している。
//!
//! ## ユーザーの操作が常に勝つ
//!
//! 「画面が突然変わらない」と追従は正面から衝突する。そこで:
//!
//! * ユーザーが自分でスクロールしたら [`Follow::note_user_scroll`] で**一時停止**。
//!   以後は明示的な [`Follow::resume`] があるまで 1 行も動かさない (git も叩かない)。
//! * 追従対象が消えた / 止まったら [`Follow::prune`] が黙って解除する。
//! * 同じ場所へは二度飛ばない ([`Follow::tick`] は変化したときだけ `Some`)。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// 走査の最短間隔。追従中でもこれより速く git を叩かない。
pub const SCAN_INTERVAL: Duration = Duration::from_millis(1200);

/// 追従先の 1 点 — 「このファイルのこの行」。行は **1 始まり**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spot {
    pub path: PathBuf,
    pub line: usize,
}

/// `git diff` の hunk ヘッダ 1 本から**新しい側の開始行**を取り出す。
///
/// 形は `@@ -a,b +c,d @@ ...` (`,b` / `,d` は 1 のとき省略される)。
/// 削除だけの hunk は `+c,0` になり、`c` は「消えた場所の直前の行」なので
/// 1 行目より手前へ行かないよう 1 で下げ止める。
pub fn hunk_new_start(header: &str) -> Option<usize> {
    let rest = header.strip_prefix("@@ ")?;
    // "-a,b +c,d @@ ..." から "+c,d" を取る
    let plus = rest.split_whitespace().find(|t| t.starts_with('+'))?;
    let num = &plus[1..];
    let head = num.split(',').next()?;
    let n: usize = head.parse().ok()?;
    Some(n.max(1))
}

/// `git diff -U0` 全体から**最後の hunk** の行を取る (= 直近に触った場所)。
pub fn last_hunk_line(diff: &str) -> Option<usize> {
    diff.lines()
        .filter(|l| l.starts_with("@@ "))
        .filter_map(hunk_new_start)
        .next_back()
}

/// 触ったファイル群から「いま追うべき 1 点」を決める (I/O は mtime と diff だけ)。
///
/// * `touched` は**リポジトリ相対**パス ([`crate::worktree::scan_touched`] の返り値)。
/// * 実体が無いもの (削除された / リネーム前の旧パス) は候補から外す。
///   → 追従対象のファイルが消えても飛ばない。
/// * mtime が最も新しい 1 本を選び、その diff から行を取る。
/// * diff が空 (未追跡の新規ファイル等) なら 1 行目。
pub fn pick_spot<F>(dir: &Path, touched: &HashSet<PathBuf>, diff_of: F) -> Option<Spot>
where
    F: Fn(&Path) -> String,
{
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for rel in touched {
        let abs = dir.join(rel);
        let Ok(meta) = std::fs::metadata(&abs) else {
            continue; // 消えたファイルは追わない
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(at) = meta.modified() else { continue };
        // 同着 (mtime の分解能が粗い環境) はパス順で決める — 実行のたびに
        // 飛び先が入れ替わらないようにするため。
        let better = match &best {
            None => true,
            Some((p, t)) => at > *t || (at == *t && abs < *p),
        };
        if better {
            best = Some((abs, at));
        }
    }
    let (path, _) = best?;
    let rel = path.strip_prefix(dir).unwrap_or(&path).to_path_buf();
    let line = last_hunk_line(&diff_of(&rel)).unwrap_or(1);
    Some(Spot { path, line })
}

/// **git を実際に叩く**走査 (ここだけが I/O)。必ずワーカースレッドから呼ぶこと。
pub fn probe(dir: &Path) -> Option<Spot> {
    let touched = crate::worktree::scan_touched(dir, None).ok()?;
    pick_spot(dir, &touched, |rel| {
        let spec = rel.to_string_lossy().into_owned();
        crate::worktree::git_out(dir, &["diff", "-U0", "--", &spec]).unwrap_or_default()
    })
}

/// 追従の状態機械。
///
/// `target == None` がオフ。オンのまま `paused` が立っているのが
/// 「ユーザーが自分でスクロールしたので黙っている」状態。
#[derive(Default)]
pub struct Follow {
    target: Option<u64>,
    paused: bool,
    last: Option<Instant>,
    spot: Option<Spot>,
    rx: Option<Receiver<Option<Spot>>>,
}

impl Follow {
    /// 追従中のセッション ID (オフなら `None`)。
    pub fn target(&self) -> Option<u64> {
        self.target
    }

    /// 追従がオンか (一時停止中も**オン**)。
    pub fn is_on(&self) -> bool {
        self.target.is_some()
    }

    /// ユーザーのスクロールで一時停止しているか。
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// 実際に画面を動かしてよい状態か。
    pub fn is_active(&self) -> bool {
        self.is_on() && !self.paused
    }

    /// 最後に飛んだ場所 (ステータスバーの表示に使う)。
    pub fn spot(&self) -> Option<&Spot> {
        self.spot.as_ref()
    }

    /// 追従を始める / 同じ相手なら解除する。返り値は**適用後にオンか**。
    pub fn toggle(&mut self, id: u64) -> bool {
        if self.target == Some(id) {
            self.stop();
            false
        } else {
            self.target = Some(id);
            self.paused = false;
            self.last = None;
            self.spot = None;
            self.rx = None;
            true
        }
    }

    /// 追従をやめる (走査中の受け口も捨てる)。
    pub fn stop(&mut self) {
        self.target = None;
        self.paused = false;
        self.last = None;
        self.spot = None;
        self.rx = None;
    }

    /// ユーザーが自分でスクロールした。返り値は**このとき初めて止まった**か。
    pub fn note_user_scroll(&mut self) -> bool {
        if !self.is_active() {
            return false;
        }
        self.paused = true;
        // 走査中の結果は捨てる (止めたのに 1 回だけ飛ぶのを防ぐ)
        self.rx = None;
        true
    }

    /// 明示的な再開。返り値は**実際に再開したか** (止まっていなければ false)。
    pub fn resume(&mut self) -> bool {
        if !self.is_on() || !self.paused {
            return false;
        }
        self.paused = false;
        // 再開した瞬間に 1 回走査させる (待たせない)
        self.last = None;
        true
    }

    /// 追従できる相手の一覧を渡して、消えた / 止まった相手なら解除する。
    /// 返り値は**このとき解除したか** (呼び出し側がトーストを出すため)。
    pub fn prune(&mut self, followable: &[u64]) -> bool {
        let Some(id) = self.target else { return false };
        if followable.contains(&id) {
            return false;
        }
        self.stop();
        true
    }

    /// 1 フレーム進める。
    ///
    /// `start_scan` は「別スレッドで走査を始めて受け口を返す」責務。
    /// **オフ / 一時停止 / 期限前 / 走査中のいずれかなら 1 回も呼ばれない。**
    ///
    /// 返り値は「今フレーム飛ぶべき場所」。前回と同じ場所なら `None` を返すので、
    /// ユーザーがそこに居る限り画面は動かない。
    pub fn tick<F>(&mut self, now: Instant, start_scan: F) -> Option<Spot>
    where
        F: FnOnce() -> Receiver<Option<Spot>>,
    {
        if !self.is_active() {
            return None;
        }
        // ① 走査結果の取り込み (待たない)
        let mut got: Option<Option<Spot>> = None;
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(v) => {
                    got = Some(v);
                    self.rx = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.rx = None,
            }
        }
        // ② 期限が来ていて走査中でなければ、次の走査を始める
        if self.rx.is_none()
            && self
                .last
                .is_none_or(|t| now.duration_since(t) >= SCAN_INTERVAL)
        {
            self.last = Some(now);
            self.rx = Some(start_scan());
        }
        // ③ 取り込んだ結果が**前回と違う場所**のときだけ飛ぶ
        let spot = got??;
        if self.spot.as_ref() == Some(&spot) {
            return None;
        }
        self.spot = Some(spot.clone());
        Some(spot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn ready(v: Option<Spot>) -> Receiver<Option<Spot>> {
        let (tx, rx) = channel();
        let _ = tx.send(v);
        rx
    }

    fn spot(p: &str, line: usize) -> Spot {
        Spot {
            path: PathBuf::from(p),
            line,
        }
    }

    #[test]
    fn hunkヘッダから新しい側の開始行を取る() {
        assert_eq!(hunk_new_start("@@ -1,2 +3,4 @@ fn main()"), Some(3));
        // ",1" の省略形
        assert_eq!(hunk_new_start("@@ -10 +12 @@"), Some(12));
        // 削除だけ (+c,0) — 0 行目へは行かない
        assert_eq!(hunk_new_start("@@ -5,3 +4,0 @@"), Some(4));
        assert_eq!(hunk_new_start("@@ -1,1 +0,0 @@"), Some(1));
        assert_eq!(hunk_new_start("not a hunk"), None);
    }

    #[test]
    fn 最後のhunkの行を拾う() {
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n@@ -30,0 +31,2 @@\n+c\n+d\n";
        assert_eq!(last_hunk_line(diff), Some(31));
        assert_eq!(last_hunk_line(""), None);
    }

    #[test]
    fn 触ったファイルが消えていたら追わない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-follow-test", "gone");
        let mut touched = HashSet::new();
        touched.insert(PathBuf::from("消えた.rs"));
        assert_eq!(pick_spot(&dir, &touched, |_| String::new()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 直近に書かれたファイルを選び_diffの行へ寄せる() {
        let dir = crate::test_util::unique_temp_dir("zaivern-follow-test", "pick");
        std::fs::write(dir.join("a.rs"), "1\n").unwrap();
        // mtime の分解能が粗い環境でも順序が決まるように少し空ける
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.join("b.rs"), "1\n").unwrap();
        let touched: HashSet<PathBuf> = ["a.rs", "b.rs"].iter().map(PathBuf::from).collect();
        let got = pick_spot(&dir, &touched, |rel| {
            if rel == Path::new("b.rs") {
                "@@ -1 +7,2 @@\n".into()
            } else {
                String::new()
            }
        })
        .expect("候補があるはず");
        assert_eq!(got.path, dir.join("b.rs"));
        assert_eq!(got.line, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 未追跡ファイルはdiffが空でも1行目へ寄せる() {
        let dir = crate::test_util::unique_temp_dir("zaivern-follow-test", "untracked");
        std::fs::write(dir.join("new.rs"), "x\n").unwrap();
        let touched: HashSet<PathBuf> = ["new.rs"].iter().map(PathBuf::from).collect();
        let got = pick_spot(&dir, &touched, |_| String::new()).unwrap();
        assert_eq!(got.line, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 追従がオフならgitを一度も叩かない() {
        let mut f = Follow::default();
        let mut calls = 0u32;
        let now = Instant::now();
        for _ in 0..100 {
            let got = f.tick(now, || {
                calls += 1;
                ready(Some(spot("x", 1)))
            });
            assert!(got.is_none());
        }
        assert_eq!(calls, 0, "オフなのに走査が走った");
        assert!(!f.is_on());
    }

    #[test]
    fn オンにすると1回走査してその場所を返す() {
        let mut f = Follow::default();
        assert!(f.toggle(7));
        let mut calls = 0u32;
        let t0 = Instant::now();
        // 1 回目: 走査を始めるが、結果はまだ取り込まない
        assert_eq!(
            f.tick(t0, || {
                calls += 1;
                ready(Some(spot("a.rs", 12)))
            }),
            None
        );
        // 2 回目: 受け口から取り込んで飛ぶ (期限前なので走査は増えない)
        assert_eq!(
            f.tick(t0, || {
                calls += 1;
                ready(None)
            }),
            Some(spot("a.rs", 12))
        );
        assert_eq!(calls, 1, "期限前なのに 2 回叩いた");
        assert_eq!(f.spot(), Some(&spot("a.rs", 12)));
    }

    #[test]
    fn 同じ場所へは二度飛ばない() {
        let mut f = Follow::default();
        f.toggle(1);
        let t0 = Instant::now();
        f.tick(t0, || ready(Some(spot("a.rs", 3))));
        assert_eq!(f.tick(t0, || ready(None)), Some(spot("a.rs", 3)));
        let t1 = t0 + SCAN_INTERVAL;
        f.tick(t1, || ready(Some(spot("a.rs", 3))));
        assert_eq!(f.tick(t1, || ready(None)), None, "同じ場所へ飛び直した");
    }

    #[test]
    fn スロットリング_期限が来るまで走査しない() {
        let mut f = Follow::default();
        f.toggle(1);
        let mut calls = 0u32;
        let t0 = Instant::now();
        f.tick(t0, || {
            calls += 1;
            ready(None)
        });
        // 受け口は空になっている。期限内は何度回しても増えない。
        for _ in 0..50 {
            f.tick(t0 + Duration::from_millis(100), || {
                calls += 1;
                ready(None)
            });
        }
        assert_eq!(calls, 1);
        f.tick(t0 + SCAN_INTERVAL, || {
            calls += 1;
            ready(None)
        });
        assert_eq!(calls, 2, "期限を過ぎても走査しなかった");
    }

    #[test]
    fn 状態機械_オン_ユーザースクロールで一時停止_明示再開() {
        let mut f = Follow::default();
        assert!(f.toggle(42));
        assert!(f.is_active());
        // ユーザーが自分でスクロール → 一時停止 (オンのまま)
        assert!(f.note_user_scroll());
        assert!(f.is_on() && f.is_paused() && !f.is_active());
        // 二度目の通知では「今止まった」とは言わない
        assert!(!f.note_user_scroll());
        // 止まっている間は git を 1 回も叩かない
        let mut calls = 0u32;
        let now = Instant::now();
        for _ in 0..20 {
            f.tick(now, || {
                calls += 1;
                ready(Some(spot("a.rs", 1)))
            });
        }
        assert_eq!(calls, 0, "一時停止中に走査が走った");
        // 明示的な再開でだけ戻る
        assert!(f.resume());
        assert!(f.is_active());
        assert!(!f.resume(), "止まっていないのに再開したと言った");
        f.tick(now, || {
            calls += 1;
            ready(None)
        });
        assert_eq!(calls, 1);
    }

    #[test]
    fn トグルは同じ相手で解除_別の相手なら乗り換え() {
        let mut f = Follow::default();
        assert!(f.toggle(1));
        assert_eq!(f.target(), Some(1));
        assert!(f.toggle(2), "別の相手はオンのまま乗り換える");
        assert_eq!(f.target(), Some(2));
        assert!(!f.toggle(2));
        assert_eq!(f.target(), None);
    }

    #[test]
    fn 対象エージェントが終了したら黙って解除する() {
        let mut f = Follow::default();
        f.toggle(9);
        assert!(!f.prune(&[9, 10]), "生きている間は解除しない");
        assert!(f.prune(&[10]), "消えた相手を追い続けた");
        assert!(!f.is_on());
        assert!(!f.prune(&[10]), "オフなのに解除したと言った");
        // 解除後は走査もしない
        let mut calls = 0u32;
        f.tick(Instant::now(), || {
            calls += 1;
            ready(None)
        });
        assert_eq!(calls, 0);
    }

    #[test]
    fn 一時停止中に対象が消えても解除できる() {
        let mut f = Follow::default();
        f.toggle(3);
        f.note_user_scroll();
        assert!(f.prune(&[]));
        assert!(!f.is_on() && !f.is_paused());
    }
}
