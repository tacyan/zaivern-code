//! **外部でのファイル書き換えの見張りを、描画スレッドから降ろす。**
//!
//! ## なぜ要るか
//!
//! `app::check_external_changes` は「開いているタブのファイル」と
//! 「ファイルツリーが読んだフォルダ」の mtime を突き合わせるだけの、
//! **数十回の `stat` で終わる仕事**である。ところがこれを UI スレッドで
//! やるために `app::idle_repaint_ms` が 2 秒ごとに 1 フレーム予約しており、
//! 変化が 1 件も無くても **egui のフレームを丸ごと 1 枚**回していた。
//!
//! 実測 (v0.16.0, release, macOS 15, 空フォルダ 1 つ):
//!
//! | | 1 回の費用 | 頻度 | アイドル CPU 相当 |
//! |---|---:|---:|---:|
//! | egui のフレーム 1 枚 | 約 3.3ms | 2 秒に 1 回 | 0.17%/コア |
//! | 見張りの `stat` だけ | 約 12µs (20 パス) | 1 秒に 1 回 | 0.0012%/コア |
//!
//! **同じ検出をしているのに費用が 2 桁違う。** 差は全部「描いた」ぶんで、
//! 画面は 1px も変わっていない。設計原則 3 (アイドル時のコストはゼロ) は
//! 「見張るな」ではなく「**見張った結果、何も変わっていないなら描くな**」
//! と読むべきで、それをやるのがこのモジュール。
//!
//! ## 何をするか
//!
//! 1. UI スレッドが「いま見張るもの」を [`FsWatch::publish`] で置く。
//!    置くのは**パスと、UI が信じている mtime** ([`Target`])
//! 2. 別スレッドが [`POLL_MS`] ごとに `stat` して突き合わせる
//! 3. **UI の信じている値と食い違ったときだけ** `request_repaint` する。
//!    UI は次のフレームで今までどおり `check_external_changes` を回す
//!
//! ## 「UI が信じている値」を渡すのが肝
//!
//! 見張り側が*自分の*前回値を憶える作りにすると、**新しく見張り始めた
//! パスの初回で取りこぼす**。タブを開いた瞬間 (UI が mtime を読む) と
//! 見張りの初回 `stat` のあいだに書き換えが起きると、見張りは新しい
//! mtime を「最初からそうだった」と憶えてしまい、**永久に起こさない**。
//!
//! UI の値をそのまま渡せば、判定式 ([`is_news`]) は
//! `check_external_changes` が UI スレッドでやる比較と**同一**になり、
//! 取りこぼしが構造的に起こり得ない。代わりに UI は、値が変わったら
//! 置き直す義務を負う (置き直さないと見張りが起こし続ける)。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// `stat` の刻み。
///
/// `check_external_changes` 自身が 1 秒のゲートを持つので、**それに合わせる**。
/// 従来の検出は「2 秒ごとのフレーム × 1 秒のゲート」= 最悪 2 秒 (背面 6 秒 /
/// 最小化 10 秒) 遅れていたので、1 秒固定はどの場面でも**遅くならない**。
pub const POLL_MS: u64 = 1000;

/// 見張り 1 件。**UI スレッドが信じている姿**をそのまま持つ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// 見張るパス (ファイルでもフォルダでもよい)
    pub path: PathBuf,
    /// UI が「これがディスクの姿」と信じている mtime。取れないなら `None`
    pub known: Option<SystemTime>,
    /// **もう知らせた値**。`Some(v)` のときだけ 2 つ目の許容値になる。
    ///
    /// 未保存の編集と競合したバッファは読み直せないので、UI は
    /// `conflict_notified` に「この mtime はもう警告した」と記録して
    /// 同じ警告を繰り返さない。ここへ写しておかないと、見張りが
    /// **1 秒ごとに永久に起こし続ける**。
    pub acked: Option<Option<SystemTime>>,
}

impl Target {
    /// ファイルの見張り (競合警告済みの mtime を 2 つ目の許容値にする)。
    pub fn file(path: PathBuf, known: Option<SystemTime>, acked: Option<SystemTime>) -> Self {
        Self {
            path,
            known,
            // `None` (まだ何も警告していない) を `Some(None)` にすると、
            // 「消えた」= `stat` 失敗を許容値に格上げしてしまう。写すのは
            // 実際に警告した mtime があるときだけ。
            acked: acked.map(Some),
        }
    }

    /// フォルダの見張り (許容値は 1 つだけ)。
    pub fn dir(path: PathBuf, known: Option<SystemTime>) -> Self {
        Self {
            path,
            known,
            acked: None,
        }
    }
}

/// **観測した mtime が UI にとって新しいか** = 1 フレーム起こす価値があるか。
///
/// 純関数。`check_external_changes` (UI 側) の比較と同じ式であることが
/// このモジュールの正しさそのものなので、テーブルテストで固定してある。
pub fn is_news(observed: Option<SystemTime>, t: &Target) -> bool {
    observed != t.known && t.acked != Some(observed)
}

/// パスの mtime。取れない (消えた・権限が無い) なら `None`。
///
/// `editor::disk_mtime` / `file_tree::dir_mtime` と**同じ式**。
/// どちらか一方だけを変えると見張りが嘘をつくので、変えるときは 3 つとも。
fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// 見張り対象を 1 回ぶん突き合わせて、**起こす価値があるか**を返す。
///
/// 実 I/O をする唯一の場所。`stat` は最初の 1 件で差が出たら**そこで降りる**
/// (どうせ UI が全件を見直すので、残りを数える意味が無い)。
fn any_news(targets: &[Target]) -> bool {
    targets.iter().any(|t| is_news(mtime_of(&t.path), t))
}

/// 見張りスレッドと UI スレッドのあいだの受け渡し。
struct Shared {
    /// いま見張るもの。UI が置き換え、見張りが読む
    targets: Mutex<Arc<Vec<Target>>>,
    /// 見張りが「変わった」と言っている。UI が [`FsWatch::take_news`] で消費する
    news: AtomicBool,
    /// アプリ終了。スレッドはこれを見て降りる
    stop: AtomicBool,
}

/// 外部変更の見張り。**UI スレッドから持つ側**。
///
/// `Drop` でスレッドへ停止を伝える (最大 [`POLL_MS`] で降りる)。
pub struct FsWatch {
    shared: Arc<Shared>,
    /// 前回 [`publish`](Self::publish) した内容の指紋。
    /// 毎フレーム `Vec<Target>` を作り直さないための門番
    sig: u64,
    /// スレッドが起きているか。起こせなかった環境では従来の定期フレームへ戻す
    live: bool,
}

impl FsWatch {
    /// 見張りスレッドを起こす。起こせなければ [`active`](Self::active) が
    /// `false` のまま返り、呼び出し側は従来どおり定期フレームで見張る。
    pub fn new(ctx: &eframe::egui::Context) -> Self {
        let shared = Arc::new(Shared {
            targets: Mutex::new(Arc::new(Vec::new())),
            news: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let live = spawn(shared.clone(), ctx.clone());
        Self {
            shared,
            sig: 0,
            live,
        }
    }

    /// 見張りスレッドが生きているか。`false` なら呼び出し側が自分で見張る。
    pub fn active(&self) -> bool {
        self.live
    }

    /// 見張り対象を置き直す。**指紋が同じなら 1 バイトも確保しない。**
    ///
    /// `sig` は呼び出し側が [`Sig`] で組んだ指紋。中身を作るのは
    /// 指紋が変わったときだけなので、60fps で呼ばれても費用は
    /// 「パスと mtime を舐めて畳む」ぶんしか増えない。
    pub fn publish(&mut self, sig: u64, build: impl FnOnce() -> Vec<Target>) {
        if !self.live || sig == self.sig {
            return;
        }
        self.sig = sig;
        let targets = Arc::new(build());
        if let Ok(mut slot) = self.shared.targets.lock() {
            *slot = targets;
        }
    }

    /// 見張りが見つけた変化を**消費する**。`true` なら UI は今すぐ確認する。
    pub fn take_news(&self) -> bool {
        self.shared.news.swap(false, Ordering::AcqRel)
    }
}

impl Drop for FsWatch {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}

/// 見張りスレッドの本体。起こせたら `true`。
fn spawn(shared: Arc<Shared>, ctx: eframe::egui::Context) -> bool {
    std::thread::Builder::new()
        .name("zv-fswatch".into())
        .spawn(move || {
            let step = Duration::from_millis(POLL_MS);
            while !shared.stop.load(Ordering::Acquire) {
                std::thread::sleep(step);
                if shared.stop.load(Ordering::Acquire) {
                    break;
                }
                // 既に「変わった」を立てたまま UI がまだ消費していないなら、
                // もう一度 `stat` する意味は無い (UI は全件を見直す)。
                if shared.news.load(Ordering::Acquire) {
                    continue;
                }
                let targets = match shared.targets.lock() {
                    Ok(t) => t.clone(),
                    Err(_) => break,
                };
                if targets.is_empty() {
                    continue;
                }
                if any_news(&targets) {
                    shared.news.store(true, Ordering::Release);
                    // 出所を `perf::dump` の内訳へ出す。**変化があったときだけ**
                    // 立つので、アイドルの内訳には 1 行も出ないのが正しい姿。
                    crate::perf::repaint(&ctx, "fswatch");
                }
            }
        })
        .is_ok()
}

/// 見張り対象の**指紋**。`Vec<Target>` を毎フレーム作らないための門番。
///
/// FNV-1a (64bit)。`std` の `DefaultHasher` は版をまたぐ安定性が保証されて
/// いないが、ここは同一プロセス内の前後比較にしか使わないので値の互換性は
/// 要らない — それでも自前で持つのは、**衝突したら見張りが古いままになる**
/// ので分布の素性が分かっているものを使いたいため。
#[derive(Debug, Clone, Copy)]
pub struct Sig(u64);

impl Default for Sig {
    fn default() -> Self {
        Self::new()
    }
}

impl Sig {
    pub fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.0 ^= x as u64;
            self.0 = self.0.wrapping_mul(0x1000_0000_01b3);
        }
    }

    fn time(&mut self, t: Option<SystemTime>) {
        match t.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()) {
            Some(d) => {
                self.bytes(&d.as_secs().to_le_bytes());
                self.bytes(&d.subsec_nanos().to_le_bytes());
            }
            // **`None` を素通りさせない。** 消えたファイルと EPOCH ちょうどの
            // ファイルが同じ指紋になると、置き直しが飛んで見張りが古びる。
            None => self.bytes(b"\0none"),
        }
    }

    /// ファイル 1 件を畳む。
    pub fn file(&mut self, path: &Path, known: Option<SystemTime>, acked: Option<SystemTime>) {
        self.bytes(path.as_os_str().as_encoded_bytes());
        self.time(known);
        // acked は「有る/無い」で意味が変わるので印を分ける
        match acked {
            Some(t) => {
                self.bytes(b"\x01");
                self.time(Some(t));
            }
            None => self.bytes(b"\x00"),
        }
    }

    /// フォルダ 1 件を畳む。
    pub fn dir(&mut self, path: &Path, known: Option<SystemTime>) {
        self.bytes(path.as_os_str().as_encoded_bytes());
        self.time(known);
        self.bytes(b"\x02");
    }

    pub fn finish(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as Dur;

    fn t(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Dur::from_secs(secs))
    }

    // ── 判定式 (UI 側の比較と同一であること) ─────────────────────────

    #[test]
    fn 同じmtimeなら起こさない() {
        let tgt = Target::dir("/x".into(), t(100));
        assert!(!is_news(t(100), &tgt));
    }

    #[test]
    fn 違うmtimeなら起こす() {
        let tgt = Target::dir("/x".into(), t(100));
        assert!(is_news(t(101), &tgt));
        // 消えた場合も「変わった」
        assert!(is_news(None, &tgt));
    }

    #[test]
    fn 消えていた対象が現れたら起こす() {
        let tgt = Target::dir("/x".into(), None);
        assert!(is_news(t(1), &tgt));
        assert!(!is_news(None, &tgt));
    }

    #[test]
    fn 警告済みのmtimeでは起こさないが別の値なら起こす() {
        // 未保存の編集と競合 → UI は t(200) を警告済みにした
        let tgt = Target::file("/x".into(), t(100), t(200));
        assert!(!is_news(t(200), &tgt), "同じ競合で何度も起こさない");
        assert!(
            !is_news(t(100), &tgt),
            "UI が信じている値へ戻ったなら用は無い"
        );
        assert!(is_news(t(300), &tgt), "さらに書き換わったら起こす");
        assert!(is_news(None, &tgt), "消えたら起こす");
    }

    #[test]
    fn 警告が無いファイルは消失を取りこぼさない() {
        // `acked: None` を `Some(None)` と取り違えると、消えたファイル
        // (stat 失敗 = None) が「警告済み」に当たって永久に起こらなくなる。
        let tgt = Target::file("/x".into(), t(100), None);
        assert_eq!(tgt.acked, None);
        assert!(is_news(None, &tgt));
    }

    // ── 指紋 ─────────────────────────────────────────────────────────

    #[test]
    fn 指紋は中身が同じなら同じで違えば違う() {
        let mk = |m, a| {
            let mut s = Sig::new();
            s.file(Path::new("/a/b.rs"), m, a);
            s.dir(Path::new("/a"), t(9));
            s.finish()
        };
        assert_eq!(mk(t(1), None), mk(t(1), None));
        assert_ne!(mk(t(1), None), mk(t(2), None));
        assert_ne!(mk(t(1), None), mk(t(1), t(1)));
        assert_ne!(mk(t(1), None), mk(None, None));
    }

    #[test]
    fn 指紋は並びが違えば違う() {
        let a = {
            let mut s = Sig::new();
            s.file(Path::new("/a"), t(1), None);
            s.file(Path::new("/b"), t(2), None);
            s.finish()
        };
        let b = {
            let mut s = Sig::new();
            s.file(Path::new("/b"), t(2), None);
            s.file(Path::new("/a"), t(1), None);
            s.finish()
        };
        assert_ne!(a, b);
    }

    #[test]
    fn 指紋はファイルとフォルダを区別する() {
        let f = {
            let mut s = Sig::new();
            s.file(Path::new("/a"), t(1), None);
            s.finish()
        };
        let d = {
            let mut s = Sig::new();
            s.dir(Path::new("/a"), t(1));
            s.finish()
        };
        assert_ne!(f, d);
    }

    // ── 実ファイルでの突き合わせ ─────────────────────────────────────

    #[test]
    fn 実ファイルの書き換えを見つける() {
        let dir = crate::test_util::unique_temp_dir("zv", "fswatch");
        std::fs::create_dir_all(&dir).expect("一時ディレクトリ");
        let f = dir.join("a.txt");
        std::fs::write(&f, b"one").expect("書ける");
        let known = mtime_of(&f);
        let targets = vec![Target::file(f.clone(), known, None)];
        assert!(!any_news(&targets), "触っていないので起こさない");

        // mtime の分解能を跨ぐ (秒単位しか持たないファイルシステムがある)
        std::thread::sleep(Dur::from_millis(1100));
        std::fs::write(&f, b"two").expect("書ける");
        assert!(any_news(&targets), "外から書き換えたら起こす");

        std::fs::remove_file(&f).expect("消せる");
        assert!(any_news(&targets), "消えたら起こす");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 対象が空なら何も起こさない() {
        assert!(!any_news(&[]));
    }

    // ── 呼び出しの順番 (これが崩れると静かに見張らなくなる) ──────────

    fn update_body() -> String {
        // `app` は 51 ファイルへ分割済み。**どれか 1 つを読むと見落とす**ので、
        // 全部を分割前と同じ順に繋いだ `app::SRC` を通す。
        let src = crate::app::SRC.replace("\r\n", "\n");
        src.split("fn update_impl(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {")
            .nth(1)
            .expect("update_impl がある")
            .to_string()
    }

    /// **見張りの取り込みは `check_external_changes` の直前。**
    ///
    /// 逆順だと、見張りが「変わった」と言って起こしたフレームで
    /// 1 秒ゲートが開いておらず、確認が捨てられる。次のフレームは
    /// 誰も予約しないので、そこで取り込みが止まる。
    #[test]
    fn 見張りの取り込みは外部変更チェックより先() {
        let body = update_body();
        let tick = body
            .find("self.watch_tick(ctx);")
            .expect("watch_tick を呼ぶ");
        let check = body
            .find("self.check_external_changes();")
            .expect("check_external_changes を呼ぶ");
        assert!(tick < check, "watch_tick は check_external_changes より先");
    }

    /// **置き直しは全ての描画のあと。**
    ///
    /// `FileTree` は描くときに初めてフォルダを読む。描画より前に置くと
    /// 「このフレームで増えたフォルダ」が見張り対象から 1 枚ぶん漏れる。
    /// 次のフレームが来る保証は無い版なので、1 枚遅れ = 永久に見張らない。
    #[test]
    fn 見張りの置き直しは描画のあと予約の直前() {
        let body = update_body();
        let publish = body
            .find("self.publish_watch_targets();")
            .expect("publish_watch_targets を呼ぶ");
        let sidebar = body
            .find("me.sidebar(ctx)")
            .expect("サイドバー (ファイルツリー) を描く");
        let schedule = body
            .find("self.schedule_idle_repaint(ctx);")
            .expect("schedule_idle_repaint を呼ぶ");
        assert!(sidebar < publish, "ファイルツリーを描いたあとで置き直す");
        assert!(publish < schedule, "予約を決めるより先に置き直す");
    }

    /// **見張りが生きているなら、家事のための定期フレームは 0 枚。**
    ///
    /// これがこの版の主張そのもの。`watching_files` を落としても
    /// 他の理由 (待ち・アニメ・エージェント) は今までどおり効く。
    #[test]
    fn 見張りが生きていれば定期フレームを予約しない() {
        use crate::app::{idle_repaint_ms, IdleSignals};
        let watched = IdleSignals {
            focused: true,
            visible: true,
            // 見張りスレッドが担当している = UI 側は false で組み立てる
            watching_files: false,
            ..Default::default()
        };
        assert_eq!(idle_repaint_ms(watched), None, "1 枚も予約しない");

        // 後退経路 (スレッドを起こせなかった環境) は従来どおり回る
        let fallback = IdleSignals {
            watching_files: true,
            ..watched
        };
        assert!(
            idle_repaint_ms(fallback).is_some(),
            "見張れないなら UI が見張る"
        );

        // 見張りは「他の理由」を消さない
        let awaiting = IdleSignals {
            awaiting: true,
            ..watched
        };
        assert!(idle_repaint_ms(awaiting).is_some());
        let agents = IdleSignals {
            agents_running: true,
            ..watched
        };
        assert!(idle_repaint_ms(agents).is_some());
        let timers = IdleSignals {
            timer_due_in_ms: Some(900_000),
            ..watched
        };
        // **期限まで寝る。** 900 秒後のフックのために 2 秒ごとに
        // 描いていたのをやめたのがこの版の主眼なので、ここも 900 秒。
        assert_eq!(idle_repaint_ms(timers), Some(900_000));
    }
}
