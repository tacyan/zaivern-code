use std::collections::HashMap;
use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use eframe::egui;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::i18n::{tr, trf};
use crate::keybinds::{key_hint, BindAction};
use crate::lockx::lock_ok;
use crate::theme::Theme;

pub struct SpawnSpec {
    pub title: String,
    pub preset_name: String,
    pub icon: String,
    pub command: String,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    /// PTY 生出力のログ書き出し先。None ならログを残さない。
    /// 再起動をまたいで「前回何をしていたか」を読み返すための素材になる。
    pub log_path: Option<PathBuf>,
}

/// 端末内検索 (Cmd+F) の UI 状態。クエリと直近ヒットを覚えるだけで、
/// 検索そのものはナビゲーション操作のたびにスクロールバック全文へかけ直す
/// (端末は常に流れるので、キャッシュより都度検索の方が正確)。
#[derive(Default)]
pub struct SearchUi {
    /// 検索バーを表示中か。
    pub open: bool,
    /// 検索クエリ (大文字小文字は区別しない)。
    pub query: String,
    /// 直近ヒットの絶対行 (0 = スクロールバック最古行)。次/前の起点。
    pub hit_line: Option<usize>,
    /// 直近検索のヒット行数 (UI 表示用)。
    pub total: usize,
    /// 現在何件目か (1-based、0 は未確定。UI 表示用)。
    pub index: usize,
    /// 次のフレームで検索バーの入力欄へフォーカスを移す (開いた直後用)。
    pub focus_pending: bool,
    /// 現在ヒットの表示位置: (ジャンプ時の scroll 量, 可視行)。
    /// scroll が変わったら無効として扱う (強調表示だけの情報)。
    pub current_vis: Option<(usize, u16)>,
}

/// PTY への書き込み口。**積むだけで、実際に書くのは専用スレッド**。
///
/// 以前はここで直接 `write_all` していた。PTY の入力パイプは子が読まなくなると
/// 詰まり、そのまま書き手を止める。書き手は UI スレッド (キー入力・一斉送信・
/// 音声・スマホ・調停レイヤの配達) なので、固まりかけたエージェントが 1 本あると
/// アプリ全体が止まった。しかも共有ロックを掴んだまま止まるため、読取スレッドの
/// 返事 (CSI 6n など) まで巻き添えになっていた。
///
/// 書き込みを待ち行列へ逃がせば、詰まるのは捨てて構わない writer スレッドだけで済む。
#[derive(Clone)]
struct PtyWriter {
    tx: std::sync::mpsc::Sender<Vec<u8>>,
    /// まだ書けていないバイト数。青天井に溜め込まないための目安。
    queued: Arc<std::sync::atomic::AtomicUsize>,
}

impl PtyWriter {
    /// 待ち行列の上限。これを超えたら子はもう入力を読んでいないので、
    /// 積んでもメモリを食うだけで届かない。人が打つ量からは遠く離してある。
    const MAX_QUEUED: usize = 1 << 20; // 1 MiB

    fn send(&self, bytes: &[u8]) {
        use std::sync::atomic::Ordering as O;
        if self.queued.load(O::Relaxed) > Self::MAX_QUEUED {
            return;
        }
        self.queued.fetch_add(bytes.len(), O::Relaxed);
        if self.tx.send(bytes.to_vec()).is_err() {
            // writer スレッドが終わっている (PTY が閉じた)。数え戻しておく。
            self.queued.fetch_sub(bytes.len(), O::Relaxed);
        }
    }
}

/// PTY へサイズを送る前に要求が安定しているべき連続フレーム数。
///
/// Cockpit のタイル増減・ファイルオープンでタイルサイズが毎フレーム揺れる間は
/// 送らず、同じサイズが K フレーム続いてから 1 回だけ送る (全画面レースの
/// 「前フレーム比の矩形安定」と同じ考え方)。K は小さく保つ — 一発で決まる
/// 普通のリサイズも 1 フレーム遅れで届くだけで、体感は即時のまま。
const RESIZE_STABLE_FRAMES: u8 = 2;

/// リサイズ要求のフレーム安定判定 (純ロジック)。
///
/// vt100 (描画側) は毎フレーム即時に合わせるが、PTY (ConPTY) への通知は
/// ここが「安定した」と判定したサイズだけを [`ResizeCoalescer`] へ渡す。
/// Windows の `ResizePseudoConsole` は conhost への**ブロッキング RPC** で、
/// 毎フレーム × タイル数だけ撃つと 1 個詰まっただけで UI が固まるため。
#[derive(Default)]
struct ResizeDebounce {
    /// 直近フレームで要求されたサイズ。
    last: Option<(u16, u16)>,
    /// `last` が連続で要求されたフレーム数。
    stable: u8,
    /// `last` を PTY へ送り出し済みか。
    shipped: bool,
}

impl ResizeDebounce {
    /// `size` を適用済みとして開始する (spawn 直後の PTY 初期サイズ)。
    /// 最初のフレームが同じサイズを要求しても無駄撃ちしない。
    fn settled(size: (u16, u16)) -> Self {
        Self {
            last: Some(size),
            stable: RESIZE_STABLE_FRAMES,
            shipped: true,
        }
    }

    /// 毎フレームの要求サイズを受け、PTY へ送るべきなら Some を返す。
    /// 同じサイズは高々 1 回しか Some にならない。
    fn on_request(&mut self, size: (u16, u16)) -> Option<(u16, u16)> {
        if self.last == Some(size) {
            if self.shipped {
                return None;
            }
            self.stable = self.stable.saturating_add(1);
            if self.stable >= RESIZE_STABLE_FRAMES {
                self.shipped = true;
                return Some(size);
            }
            None
        } else {
            self.last = Some(size);
            self.stable = 1;
            self.shipped = RESIZE_STABLE_FRAMES <= 1;
            if self.shipped {
                Some(size)
            } else {
                None
            }
        }
    }

    /// まだ送っていない要求が残っているか。draw 側はこれが立っている間
    /// 再描画を要求し続け、安定カウントを必ず完走させる (取りこぼし防止)。
    fn pending(&self) -> bool {
        !self.shipped && self.last.is_some()
    }

    /// 送信に失敗した (PTY のロックが取れなかった) ことにして、同じサイズを
    /// 次フレームでもう一度送らせる。`pending()` が再び立つので draw も回る。
    fn retry(&mut self) {
        self.shipped = false;
    }
}

/// 「最新の 1 サイズ」だけを持つ受け渡し箱。
struct CoalesceState {
    /// まだ適用していない最新の要求。送り手は常に上書きするだけ。
    pending: Option<(u16, u16)>,
    /// セッションが畳まれた印。ワーカーはこれを見て抜ける。
    shutdown: bool,
}

/// PTY リサイズを UI スレッドから引き剥がすためのワーカー。
///
/// [`PtyWriter`] と同じ発想: 詰まり得る呼び出し (Windows では
/// `ResizePseudoConsole` = conhost への同期 RPC) は専用スレッドに任せ、
/// UI スレッドは「最新サイズを上書きして通知する」だけで**絶対に待たない**。
/// 箱には最新の 1 個しか入らないので、リサイズの嵐が来ても
/// ワーカーが実際に撃つ回数は「取り出した時点の最新」ぶんだけに潰れる。
///
/// ワーカーは apply クロージャ経由で master の [`Weak`] しか持たない。
/// セッションが drop されて強参照が消えれば upgrade が失敗して自然に抜ける
/// ため、master (ConPTY ハンドル) より長生きして触ることはできない。
struct ResizeCoalescer {
    shared: Arc<(Mutex<CoalesceState>, Condvar)>,
}

impl ResizeCoalescer {
    /// ワーカーを起こす。`apply` が false を返したら送り先が消えたとみなして畳む。
    /// スレッドを起こせなかったら None (呼び出し側が同期適用へ切り替える)。
    fn start(
        name: String,
        mut apply: impl FnMut(u16, u16) -> bool + Send + 'static,
    ) -> Option<Self> {
        let shared = Arc::new((
            Mutex::new(CoalesceState {
                pending: None,
                shutdown: false,
            }),
            Condvar::new(),
        ));
        let worker_shared = shared.clone();
        std::thread::Builder::new()
            .name(name)
            .spawn(move || {
                let (lock, cv) = &*worker_shared;
                let mut st = lock_ok(lock);
                loop {
                    if st.shutdown {
                        break;
                    }
                    if let Some((rows, cols)) = st.pending.take() {
                        // 適用中はロックを持たない — request 側を一瞬たりとも
                        // ブロッキング RPC に巻き込まないため。
                        drop(st);
                        if !apply(rows, cols) {
                            return;
                        }
                        st = lock_ok(lock);
                    } else {
                        st = cv.wait(st).unwrap_or_else(|e| e.into_inner());
                    }
                }
            })
            .ok()
            .map(|_| Self { shared })
    }

    /// 最新サイズを上書きして通知する。待たない。
    fn request(&self, rows: u16, cols: u16) {
        let (lock, cv) = &*self.shared;
        lock_ok(lock).pending = Some((rows, cols));
        cv.notify_one();
    }

    /// ワーカーへ終了を伝える。join はしない — 通知だけして即戻る。
    fn shutdown(&self) {
        let (lock, cv) = &*self.shared;
        let mut st = lock_ok(lock);
        st.shutdown = true;
        st.pending = None;
        cv.notify_one();
    }
}

pub struct Session {
    /// セッション毎に一意な安定ID(呼び出し側が採番)。sessions の index は
    /// 削除で前へ詰まるため、バブル却下記録などの識別にはこちらを使う。
    pub id: u64,
    pub title: String,
    pub preset_name: String,
    pub icon: String,
    pub command: String,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub parser: Arc<Mutex<vt100::Parser>>,
    /// PTY への書き込み口。問い合わせへの返事を読取スレッドからも書くため共有する。
    writer: PtyWriter,
    /// PTY の master ハンドル。リサイズワーカーが Weak で覗くため Arc に包む。
    /// 強参照はここだけ — drop すれば従来どおり ConPTY が閉じる。
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// PTY リサイズの安定判定 (毎フレームの要求 → 送るべき 1 回に潰す)。
    resize_debounce: ResizeDebounce,
    /// リサイズを実際に撃つワーカー。初回のリサイズ確定時に遅延起動する。
    resizer: Option<ResizeCoalescer>,
    /// ワーカーの起動に失敗した (スレッドを作れなかった)。以後は再試行しない。
    resizer_spawn_failed: bool,
    /// パーサを作り直したときに一度だけ出すお知らせ (スクロールバック消失の告知)。
    /// UI が読み取ったら None に戻す。
    pub parser_rebuilt_notice: Option<String>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// PTY に直接ぶら下がっている子 (cmd.exe / ログインシェル) の PID。
    /// エージェント本体はその**孫**なので、畳むときはここを起点に
    /// プロセスツリーごと落とす ([`kill_tree_command`] の説明を参照)。
    child_pid: Option<u32>,
    pub exited: Arc<AtomicBool>,
    pub exit_code: Arc<Mutex<Option<u32>>>,
    pub started: Instant,
    size: (u16, u16),
    scroll: usize,
    /// IME 変換中テキスト(未確定文字列)。UI 側だけの状態で PTY へは送らない。
    pub preedit: String,
    /// CLI エージェントが承認待ち(プロンプト表示中)と推定される状態。
    pub attention: bool,
    /// 承認待ち (attention == true) になった開始時刻。
    pub attention_since: Option<Instant>,
    /// 終了通知を出したかどうか(多重通知の防止)。
    pub notified_exit: bool,
    /// このセッションが bypass 権限フラグ付きで起動されたか(表示用)。
    pub launched_bypass: bool,
    last_scan: Instant,
    /// 応答済みプロンプトの指紋 (prompt_signature)。自動YES送信・バブルの承認/拒否の
    /// あとに立て、同じプロンプトが画面に残っていても二度目の応答・再検出をしない。
    /// プロンプトが画面から消える、または別のプロンプトに変わったら下ろす。
    answered_sig: Option<u64>,
    /// 自動YESの停滞監視: 自動応答したのにプロンプトが固まったままのとき、
    /// 「画面が意味的に変化していない時間」の起点。自動YESが送った応答にだけ立て、
    /// ユーザーの手動応答 (resolve_attention 経由) では None に戻す — 手動運転中に
    /// 勝手な再送をしないため。画面が変化するたびに現在時刻へ引き直す。
    auto_stall_since: Option<Instant>,
    /// 停滞監視の基準となる意味的画面ハッシュ (auto_stall_since とペア)。
    auto_stall_hash: u64,
    /// 自動YESが効かなかったとき、ペットの承認操作へ切り替えるまでの停滞時間。
    /// 既定 30 秒 (テストで短縮する)。
    auto_yes_resend_after: Duration,
    /// マウスドラッグによる文字選択: (開始セル, 終了セル)。(row, col) の画面表示座標。
    ///
    /// **`sel_abs` を今の画面へ切り取った派生値**であって、真実源ではない。
    /// 直接書き換えず `set_selection` / `set_selection_abs` / `clear_selection`
    /// を通すこと (描画側 `cell_selected` / `normalize_sel` はこの派生値を読む)。
    pub selection: Option<((u16, u16), (u16, u16))>,
    /// 文字選択の真実源: (開始, 終了) を**絶対行** (生きている画面の下端から
    /// 数えた行) で持つ。スクロールしても同じ文字を指し続けるので、
    /// 一画面を超える選択とスクロール中の選択保持ができる。
    sel_abs: Option<((usize, u16), (usize, u16))>,
    /// ドラッグ選択のアンカー(ドラッグ開始セル)。絶対座標。
    sel_anchor_abs: Option<(usize, u16)>,
    /// `sel_abs` / `sel_anchor_abs` を記録した時点の [`LineIndex::scrolled`]。
    ///
    /// 絶対行は**生きている画面の下端**が起点なので、1 行押し出されるたびに
    /// 同じ文字を指す値が +1 される。差分がそのまま「進めるべき量」になる。
    sel_pushed: u64,
    /// 端末内検索 (Cmd+F) の状態。スクロールバック全体を対象にする。
    pub search: SearchUi,
    /// コピー完了フィードバックの表示開始時刻。
    copied_at: Option<Instant>,
    /// ユーザーがキーボードから直接この端末へ文字を送ったか。
    ///
    /// 音声入力は「さっき書いた分を Backspace で消して書き直す」方式なので、
    /// 途中で人が手で打ったり Enter で送信したりすると、覚えている内容と
    /// 入力欄の中身がずれる。ずれたことに気づけるよう印を立て、
    /// 音声側が読んだら下ろす (`take_user_typed`)。
    user_typed: bool,
    /// DECSCUSR で指定された現在のカーソル形状(読取スレッドが書き、描画が読む)。
    cursor_shape: Arc<AtomicU8>,
    /// シェル統合 (OSC 633 / 133) の追跡。読取スレッドが書き、UI が読む。
    ///
    /// **画面 (vt100) を 1 文字も見ない**判定の出どころ。マーカーが来なければ
    /// 空のままで、その場合の挙動は導入前と完全に同じ (`Tier::None`)。
    shell: Arc<Mutex<crate::shellint::Tracker>>,
    /// 端末の通し番号 ([`LineIndex`])。読取スレッドが書き、描画が読む。
    ///
    /// **シェル統合の行番号はここが唯一の出どころ。** vt100 は押し出した
    /// 行数を残さないので、`process` を通した回数ぶんだけ測って積み上げる。
    lines: Arc<LineIndex>,
    /// アプリが CSI ?1004h でフォーカス通知を要求しているか。
    /// (set_focus 経由でのみ読む。app.rs から呼ばれるまでは未使用)
    #[allow(dead_code)]
    focus_reports: Arc<AtomicBool>,
    /// 直近に PTY へ送ったフォーカス状態(同じ状態の連投を防ぐ)。
    #[allow(dead_code)]
    focus_sent: Option<bool>,
    /// OSC 52 で受け取ったクリップボード書き込み要求。app.rs が取り出して egui に渡す。
    #[allow(dead_code)]
    clipboard_pending: Arc<Mutex<Option<String>>>,
    /// OSC 10/11 の色問い合わせに返す前景/背景色 (0xRRGGBB)。
    /// 読取スレッド側が使う。set_report_colors で上書きできる。
    #[allow(dead_code)]
    report_fg: Arc<AtomicU32>,
    #[allow(dead_code)]
    report_bg: Arc<AtomicU32>,
    /// 最後に「見た」(mark_read した) 時点の意味的画面ハッシュ。未読判定の基準。
    seen_hash: u64,
    /// 現在の意味的画面ハッシュ。スピナー・経過秒・カウンタの揺れは
    /// 正規化済みなので、変化 = 本当に新しい出力 (scan_attention の周期で更新)。
    cur_hash: u64,
    /// 1 つ前のスキャン時点の `cur_hash`。「出力が進んでいるか」の裏取り用。
    prev_hash: u64,
    /// 手動の「あとで見る」ピン。フォーカスを当て直す (acknowledge) まで未読扱い。
    pub pinned_unread: bool,
    /// レート制限/使用上限の警告が画面に出ているとき、その行。
    /// 警告が画面から消える (2 スキャン連続で不検出) と自動で外れる。
    pub rate_limited: Option<String>,
    /// レート制限警告を連続で見失った回数 (2 回で解除。1 回では画面遷移の瞬きと区別できない)。
    rl_miss: u8,
    /// レート制限警告が**同じまま**続けて見えた回数。画面由来の判定は
    /// 単発では信じないので、裏取り (`failover::confirm_screen`) の材料にする。
    rl_hits: u8,
    /// 直近にこのセッションへ投げた「プロンプト」の本文。
    /// フェイルオーバーで別プロファイルへ引き継ぐ材料 ([`Session::note_prompt`])。
    /// キーストロークや承認キーは含めない。
    pub last_prompt: Option<String>,
    /// **端末へ直接打ち込んでいる途中の 1 行。**
    ///
    /// 自動命名とフェイルオーバーの引き継ぎは `last_prompt` を材料にするが、
    /// これを埋めていたのは**アプリ経由の送信だけ**だった。ターミナルに
    /// 直接タイプした指示は 1 文字ずつ PTY へ流れるだけで、どこにも
    /// 残らない (= 直接打った人のセッションは永久に無題のままになる)。
    /// ここで打鍵を組み立て直し、確定 (Enter) の瞬間に `note_prompt` へ渡す。
    typed_line: TypedLine,
    /// このセッションの生ログの書き出し先 (再起動時の引き継ぎ・UI 表示用)。
    pub log_path: Option<PathBuf>,
    /// リンク検出の実行時状態 (実在確認のメモ + 直近に解析した 1 行)。
    links: LinkState,
}

/// scan_attention の結果。
pub enum Attention {
    /// 新たに承認待ちになった(ユーザーの操作が必要)。
    NeedsApproval,
    /// 全自動YESモードが承認プロンプトへ自動応答した(説明文)。
    AutoReplied(&'static str),
    /// レート制限/使用上限の警告を新たに検知した(警告行)。
    RateLimited(String),
}

/// 画面テキストの「意味的な」ハッシュ。
///
/// スピナー・経過秒・トークン数などの揺れを supervisor::normalize_line で
/// 潰してから畳むので、値の変化 = 本当に新しい出力。これを未読判定に使う
/// (生バイトを数えると、アイドル中の点滅や時計の再描画で永遠に未読になる)。
fn semantic_hash(text: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for line in text.lines() {
        let n = crate::supervisor::normalize_line(line, false);
        if !n.trim().is_empty() {
            n.hash(&mut h);
        }
    }
    h.finish()
}

/// 画面テキストからレート制限/使用上限の警告行を探す。見つかればその行。
///
/// パターンは Claude Code (`usage limit reached` / `5-hour limit reached ∙ resets …`)、
/// Codex (`You've hit your usage limit`)、一般的な API エラーに合わせてある。
/// 誤検知(会話の中で制限の話をしているだけ)を避けるため、単語 "limit" 単体には
/// 反応しない。
pub fn detect_rate_limit(text: &str) -> Option<String> {
    const PATTERNS: [&str; 9] = [
        "usage limit reached",
        "5-hour limit reached",
        "weekly limit reached",
        "session limit reached",
        "hit your usage limit",
        "approaching usage limit",
        "rate limit reached",
        "too many requests",
        "quota exceeded",
    ];
    for line in text.lines() {
        let low = line.to_lowercase();
        if PATTERNS.iter().any(|p| low.contains(p)) {
            return Some(line.trim().to_string());
        }
    }
    None
}

/// PTY 生出力のログ書き出し先。上限を超えたら `.old` へローテートし、
/// 常に「直近の分」がファイルに残るようにする(無限に太らせない)。
struct LogSink {
    file: std::fs::File,
    path: PathBuf,
    written: u64,
}

/// 1 ファイルあたりのログ上限。超えると .old へ退避して書き直す
/// (合計で最大 2 倍まで。直近分は必ず .log 側にある)。
const LOG_CAP: u64 = 4 * 1024 * 1024;

/// [`Session::note_prompt`] が「プロンプト」と見なす最短の文字数。
/// これ未満は `y` / Enter / 番号キーなどの応答なので覚えない。
const PROMPT_MIN_CHARS: usize = 4;

/// 覚えておくプロンプトの最大文字数 (引き継ぎ材料。無限に太らせない)。
const PROMPT_KEEP_CHARS: usize = 4000;

impl LogSink {
    fn open(path: &Path, header: &str) -> Option<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok()?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mut s = Self {
            file,
            path: path.to_path_buf(),
            written,
        };
        s.write(header.as_bytes());
        Some(s)
    }

    fn write(&mut self, chunk: &[u8]) {
        if self.written.saturating_add(chunk.len() as u64) > LOG_CAP {
            // ローテート失敗 (権限など) でも書き込み自体は諦めない:
            // truncate で開き直して先頭から書く。
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.old"));
            if let Ok(f) = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)
            {
                self.file = f;
                self.written = 0;
            }
        }
        if self.file.write_all(chunk).is_ok() {
            self.written += chunk.len() as u64;
        }
    }
}

/// 全自動YESモード用: 画面の承認プロンプトを分類し、送るキー列と説明を返す。
///
/// bypass 起動でも CLI エージェントは起動時/プラン承認などで対話プロンプトを出すため、
/// これに答えないと「全自動なのに進まない」状態になる。
///
/// エージェントが判らない文脈 (分類ヘルパ) 用。判っているなら
/// [`auto_yes_reply_for`] へ `bin` 名を渡すと、他 CLI 用のルールを弾ける。
#[allow(dead_code)]
pub fn auto_yes_reply(text: &str) -> Option<(&'static [u8], &'static str)> {
    auto_yes_reply_for(text, None)
}

/// [`auto_yes_reply`] のエージェント指定版。
///
/// 判定は 2 段構え:
/// 1. **カタログの応答表** (`agents::PROMPT_RULES` + ユーザー定義ルール)。
///    CLI ごとの実際の文言と送るキーは全部そこにデータとして載っている。
/// 2. 表に無い場合の **汎用ヒューリスティック** (以下)。「1. Yes」「(y/n)」
///    「Press Enter」など、CLI をまたいで通用する形だけを見る。
///
/// **番号入力メニュー (「1. …/2. … 番号を入力してください」型) はここに
/// 含まれない。** 数字は画面が 30 秒動かない停滞時にだけ打つ
/// ([`stalled_reply_for`])。理由は下の [`numbered_menu_reply`] 呼び出し箇所の
/// コメントを参照。
pub fn auto_yes_reply_for(
    text: &str,
    agent: Option<&str>,
) -> Option<(&'static [u8], &'static str)> {
    auto_yes_reply_inner(text, agent, false)
}

/// **停滞時 / 手動の「✔ 承認」用**の分類。[`auto_yes_reply_for`] に
/// 番号入力メニューへの数字応答を足した版。
///
/// 数字を打つのは「自動YESが効かず、番号入力の画面で止まったまま
/// 30 秒 (`auto_yes_resend_after`) 画面が 1 文字も動かない」ときだけ。
/// 通常スキャンから外したのは、Claude Code が出す番号付きの本文
/// (箇条書き + 「1-3 の番号を…」のような文言) に反応して**数字が連続で
/// 打ち込まれる**事故が実際に起きたため。停滞を条件にすると、
/// 数字 1 回ごとに「画面が完全に固まった 30 秒」が必ず必要になるので、
/// 連打は構造的に起こらない。
pub fn stalled_reply_for(text: &str, agent: Option<&str>) -> Option<(&'static [u8], &'static str)> {
    auto_yes_reply_inner(text, agent, true)
}

/// 分類の本体。`allow_numbered` が真のときだけ番号入力メニューにも答える。
fn auto_yes_reply_inner(
    text: &str,
    agent: Option<&str>,
    allow_numbered: bool,
) -> Option<(&'static [u8], &'static str)> {
    // 管理者権限昇格など「自動で押してはいけない」画面ではここで打ち切る。
    if crate::agents::prompt_never_answer(text) {
        return None;
    }
    // カタログの応答表が最優先 (ユーザー定義ルール → 組み込みルールの順)。
    if let Some(hit) = crate::agents::prompt_rule_reply(text, agent) {
        return Some(hit);
    }
    // 「1. …/2. … 番号を入力してください」型。**数字 + Enter** が要る画面。
    // 汎用ヒューリスティックより先に見る — 後段の「1. Yes」判定は Enter を
    // 付けない `b"1"` を返すため、番号入力の画面では確定しないまま残る。
    // 語彙は agents.rs の MENU_* 表、判定は numbered_menu_reply に閉じている。
    if allow_numbered {
        if let Some(hit) = numbered_menu_reply(text) {
            return Some(hit);
        }
    }
    // 以降は CLI をまたいで通用する汎用の形だけを見る。
    // (Antigravity 固有の文言は agents.rs の応答表に移した — 以前はここに
    //  画面が "agy"/"Antigravity" を含むかだけで発火する巨大な直書き判定が
    //  あったが、実物の agy は矢印キー選択 UI で "1. Yes" も "(y/n)" も
    //  出さないため、どの分岐にも当たらず自動YESが素通りしていた。)
    // 初回の bypass 警告: デフォルト選択が「1. No, exit」なので
    // Enter ではなく番号キー「2」で「Yes, I accept」を直接選ぶ。
    if text.contains("Bypass Permissions mode") && text.contains("Yes, I accept") {
        return Some((b"2", "Bypass警告に「Yes, I accept」"));
    }
    // フォルダ信頼確認: デフォルトが「1. Yes, proceed」なので Enter で確定。
    if text.contains("trust the files in this folder") {
        return Some((b"\r", "フォルダ信頼確認に「Yes」"));
    }
    // Press Enter 系の確認プロンプト
    if text.contains("Press Enter to continue")
        || text.contains("Press Enter to proceed")
        || text.contains("Press [Enter]")
    {
        return Some((b"\r", "Enterで続行"));
    }

    let has_question_context = text.contains("Do you")
        || text.contains("Would you")
        || text.contains("Are you")
        || text.contains("approval")
        || text.contains("permission")
        || text.contains("confirm")
        || text.contains("proceed")
        || text.contains("Allow")
        || text.contains("Antigravity")
        || text.contains("実行しますか")
        || text.contains("許可しますか")
        || text.contains("続行しますか")
        || text.contains("承認しますか");

    // Codex / Antigravity CLI TUI の承認画面。質問文と選択肢の組み合わせ。
    let agent_approval = text.contains("Would you like to run")
        || text.contains("needs your approval")
        || text.contains("Do you want to approve network access")
        || text.contains("Do you want to execute")
        || text.contains("Allow command")
        || text.contains("Allow tool")
        || text.contains("Allow action")
        || text.contains("Allow file")
        || text.contains("Antigravity:");
    if agent_approval
        && (text.contains("1. Yes")
            || text.contains("1. Allow")
            || text.contains("Yes, proceed")
            || text.contains("Yes, allow"))
    {
        return Some((b"1", "Codex/Antigravityの承認に「1」"));
    }

    // 選択カーソルが Yes / Allow / はい / 許可 の上にある一般的な確認 → Enter で確定。
    if text.contains("❯ 1. Yes")
        || text.contains("❯ 1. Allow")
        || text.contains("❯ 1. はい")
        || text.contains("❯ 1. 許可")
        || text.contains("❯ 1. 実行")
        || text.contains("❯ 1. 承認")
        || text.contains("❯ 1. Accept")
        || text.contains("❯ 1. Continue")
        || text.contains("❯ Yes")
        || text.contains("❯ Allow")
        || text.contains("❯ はい")
        || text.contains("❯ 許可")
        || text.contains("❯ Continue")
        || text.contains("❯ Proceed")
    {
        return Some((b"\r", "カーソル選択確認に「Enter」"));
    }

    // 質問コンテクストが存在し、かつ番号キー「1. Yes」または「1. Allow」「1. はい」がある場合は直接選ぶ
    if has_question_context
        && (text.contains("1. Yes")
            || text.contains("1. Allow")
            || text.contains("1. はい")
            || text.contains("1. 許可")
            || text.contains("1. 実行")
            || text.contains("1. 承認")
            || text.contains("1. Accept")
            || text.contains("1. Continue")
            || text.contains("1) Yes")
            || text.contains("(1) Yes")
            || text.contains("[1] Yes"))
    {
        return Some((b"1", "「1. Yes/Allow/はい」"));
    }

    // (y/n), [y/N], (はい/いいえ) 等のテキスト問い合わせ
    if text.contains("(y/n)")
        || text.contains("[y/N]")
        || text.contains("[y/n]")
        || text.contains("(Y/n)")
        || text.contains("[Y/n]")
        || text.contains("(y/N)")
        || text.contains("(Y/N)")
        || text.contains("[y/n/a]")
        || text.contains("[Y/n/a]")
        || text.contains("(yes/no)")
        || text.contains("[yes/no]")
        || text.contains("(y/N)?")
        || text.contains("[Y/n]?")
    {
        return Some((b"y\r", "「y」"));
    }

    // YESモードでは質問の種類を限定しない。
    // 画面最下部の直近2行（プロンプト行または直前行）が質問・確認文であれば自動でYesを送信。
    if recent_lines_has_question(text) {
        return Some((b"y\r", "質問・確認ダイアログに自動「Yes」"));
    }
    None
}

/// 画面末尾が質問文か、あるいはプロンプト入力待ち(>, $, :)で直前行が質問文であるか判定
fn recent_lines_has_question(text: &str) -> bool {
    let mut non_empty_lines = text
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let Some(last) = non_empty_lines.next() else {
        return false;
    };

    if is_question_line(last) {
        return true;
    }

    // 最下行が入力プロンプト記号（">", "$", ":", "%" など）または選択指示行の場合
    let is_prompt_symbol = last.ends_with('>')
        || last.ends_with('$')
        || last.ends_with(':')
        || last.ends_with('%')
        || last.contains("(1)")
        || last.contains("[1]");

    if is_prompt_symbol {
        if let Some(prev) = non_empty_lines.next() {
            return is_question_line(prev);
        }
    }

    false
}

/// YESモードで肯定する一般的な承認質問行か。
fn is_question_line(line: &str) -> bool {
    let line = line.trim_end();
    if line.ends_with('?')
        || line.ends_with('？')
        || line.contains("(y/n)")
        || line.contains("[y/N]")
        || line.contains("[y/n]")
        || line.contains("(Y/n)")
        || line.contains("(yes/no)")
        || line.contains("[yes/no]")
        || line.contains("(y/N)")
        || line.contains("[Y/n]")
    {
        return true;
    }

    let endings = [
        "しますか",
        "できますか",
        "よろしいですか",
        "いいですか",
        "どうしますか",
        "続けますか",
        "進めますか",
        "実行しますか",
        "許可しますか",
        "承認しますか",
        "変更しますか",
        "適用しますか",
        "削除しますか",
        "上書きしますか",
        "保存しますか",
        "送信しますか",
        "を選びますか",
        "Continue?",
        "Proceed?",
        "Confirm?",
        "Approve?",
        "Allow?",
        "Overwrite?",
    ];

    endings
        .iter()
        .any(|ending| line.ends_with(ending) || line.contains(ending))
}

// ══════════════════════════════════════════════════════════════════════
//  番号入力メニュー(アンケート/選択式プロンプト)への自動応答
// ══════════════════════════════════════════════════════════════════════
//
// CLI が「1. …/2. …」と選択肢を並べ、**数字を打って Enter** しないと先へ
// 進まない画面。矢印キー UI ((y/n) でもない) なのでこれまでの分岐に一つも
// 当たらず、自動YESをオンにしていてもセッションが止まっていた。
//
// ここにあるのは「行の形」を読む純粋な構文解析だけで、どの語が肯定/見送り/
// アンケートかという知識はすべて agents.rs の表 (MENU_* ) にある。

/// 番号メニューの選択肢 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuOption {
    /// 行頭の番号 (1 始まり)。
    pub num: u8,
    /// 番号と区切り記号を除いた本文。
    pub label: String,
}

/// 行頭に付く選択カーソル / 箇条書き記号。番号を読む前に落とす。
const MENU_LEAD_MARKS: &[char] = &[
    '❯', '>', '▶', '➤', '›', '»', '*', '•', '·', '-', '(', '[', '│', '|', '　',
];
/// 番号と本文の区切り記号。
const MENU_SEPS: &[char] = &['.', ')', ']', ':', '-', '、', '．', '：', '。'];

/// 番号キー + Enter。応答は `&'static [u8]` で返す約束なので表にしておく。
const MENU_KEYS: [&[u8]; 9] = [
    b"1\r", b"2\r", b"3\r", b"4\r", b"5\r", b"6\r", b"7\r", b"8\r", b"9\r",
];
/// 肯定肢を選んだときの説明 (UI 通知用)。
const MENU_DESC_ALLOW: [&str; 9] = [
    "番号メニューの承認肢「1」",
    "番号メニューの承認肢「2」",
    "番号メニューの承認肢「3」",
    "番号メニューの承認肢「4」",
    "番号メニューの承認肢「5」",
    "番号メニューの承認肢「6」",
    "番号メニューの承認肢「7」",
    "番号メニューの承認肢「8」",
    "番号メニューの承認肢「9」",
];
/// 見送り肢を選んだときの説明 (UI 通知用)。
const MENU_DESC_SKIP: [&str; 9] = [
    "アンケート/選択をスキップ「1」",
    "アンケート/選択をスキップ「2」",
    "アンケート/選択をスキップ「3」",
    "アンケート/選択をスキップ「4」",
    "アンケート/選択をスキップ「5」",
    "アンケート/選択をスキップ「6」",
    "アンケート/選択をスキップ「7」",
    "アンケート/選択をスキップ「8」",
    "アンケート/選択をスキップ「9」",
];
/// 評点しか無いアンケートに自動で答えたときの説明 (UI 通知用)。
/// **勝手に答えた事実を隠さない**ため、選んだ番号を必ず文面に出す。
const MENU_DESC_RATING: [&str; 9] = [
    "アンケートに自動で回答しました: 1",
    "アンケートに自動で回答しました: 2",
    "アンケートに自動で回答しました: 3",
    "アンケートに自動で回答しました: 4",
    "アンケートに自動で回答しました: 5",
    "アンケートに自動で回答しました: 6",
    "アンケートに自動で回答しました: 7",
    "アンケートに自動で回答しました: 8",
    "アンケートに自動で回答しました: 9",
];

/// 番号メニューで何を選んだかの区分 (UI 説明文の出し分け用)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuPick {
    /// 承認・肯定の選択肢。
    Affirm,
    /// 見送り・スキップの選択肢。
    Skip,
    /// 評点しか無い尺度で選んだ「最も肯定的な端」。
    Rating,
}

/// 1 行を `(番号, 本文)` に分解する。**純関数**(番号メニュー判定の中核)。
///
/// 対応する形: `1. Yes` / `1) Yes` / `[1] Yes` / `1 - Yes`
/// (先頭のカーソル記号 `❯` や字下げ、ANSI エスケープが付いていてもよい)。
pub fn parse_option_line(line: &str) -> Option<MenuOption> {
    let plain = crate::supervisor::strip_ansi(line);
    let head = plain
        .trim()
        .trim_start_matches(|c: char| MENU_LEAD_MARKS.contains(&c) || c.is_whitespace());
    let digits: String = head.chars().take_while(char::is_ascii_digit).collect();
    // 3 桁以上は年号や時刻の可能性が高い。選択肢の番号としては見ない。
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let num: u8 = digits.parse().ok()?;
    if num == 0 {
        return None;
    }
    // 番号の直後は区切り記号 (空白を挟んでもよい)。無ければ選択肢ではない。
    let mut rest = head[digits.len()..].trim_start().chars();
    if !MENU_SEPS.contains(&rest.next()?) {
        return None;
    }
    let label = rest.as_str().trim();
    if label.is_empty() {
        return None;
    }
    Some(MenuOption {
        num,
        label: label.to_string(),
    })
}

/// 画面から番号メニューの選択肢列を取り出す。**純関数**。
///
/// 1 から始まり 1 ずつ増える連番だけを採用する。選択肢の間に空行や説明行が
/// 挟まっていても続きとして読む。同じ画面に連番が複数あるときは
/// **最後(画面の下)**のものを採る — プロンプトは常に画面末尾にあるため。
pub fn parse_numbered_options(text: &str) -> Vec<MenuOption> {
    let mut best: Vec<MenuOption> = Vec::new();
    let mut cur: Vec<MenuOption> = Vec::new();
    let flush = |cur: &mut Vec<MenuOption>, best: &mut Vec<MenuOption>| {
        if cur.len() >= 2 {
            *best = std::mem::take(cur);
        }
        cur.clear();
    };
    for line in text.lines() {
        let Some(opt) = parse_option_line(line) else {
            continue; // 空行・説明行は読み飛ばす
        };
        if opt.num as usize == cur.len() + 1 {
            cur.push(opt);
        } else if opt.num == 1 {
            // 別の連番が始まった
            flush(&mut cur, &mut best);
            cur.push(opt);
        } else {
            // 番号が飛んだ = 選択肢ではない
            flush(&mut cur, &mut best);
        }
    }
    flush(&mut cur, &mut best);
    best
}

/// 「番号を入力しろ」と言っている行があるか。
/// 選択肢の行そのものは除いて見る (`2. Rate 1-5 stars` の "1-5" で誤爆しない)。
fn menu_number_hint(text: &str) -> bool {
    text.lines()
        .filter(|l| parse_option_line(l).is_none())
        .any(|l| {
            let lc = crate::supervisor::strip_ansi(l).to_lowercase();
            crate::agents::MENU_NUMBER_HINTS
                .iter()
                .any(|h| lc.contains(h))
                || crate::agents::MENU_RANGE_OPENERS.iter().any(|o| {
                    lc.split(o)
                        .skip(1)
                        .any(|t| t.starts_with(|c: char| c.is_ascii_digit()))
                })
        })
}

/// 画面が「番号入力を待っている選択式プロンプト」なら選択肢を返す。**純関数**。
///
/// 応答できるかどうかとは無関係の**検出**専用。答えが決まらない画面でも
/// Some を返すので、scan_attention はこれを見て「承認待ち」を灯せる
/// (= 誰も答えられずセッションが黙って止まる事故を防ぐ)。
pub fn numbered_menu_prompt(text: &str) -> Option<Vec<MenuOption>> {
    let opts = parse_numbered_options(text);
    if opts.len() < 2 || !menu_number_hint(text) {
        return None;
    }
    Some(opts)
}

/// 小文字化済みラベルが表のどれかを含むか。
fn label_hit(label_lc: &str, table: &[&str]) -> bool {
    table.iter().any(|n| label_lc.contains(n))
}

/// 承認・肯定の選択肢か (打ち消し語があれば肯定とみなさない)。
fn menu_is_affirm(label_lc: &str) -> bool {
    !label_hit(label_lc, crate::agents::MENU_NEGATIONS)
        && label_hit(label_lc, crate::agents::MENU_AFFIRM)
}

/// 見送り(スキップ/あとで)の選択肢か。
fn menu_is_skip(label_lc: &str) -> bool {
    if label_hit(label_lc, crate::agents::MENU_SKIP) {
        return true;
    }
    let bare = label_lc.trim_matches(|c: char| c.is_whitespace() || ".!。、,".contains(c));
    crate::agents::MENU_SKIP_EXACT.iter().any(|n| bare == *n)
}

/// 評点しか無い尺度から「最も肯定的な端」を選ぶ。**純関数**。
///
/// 1. 肯定端の語 (`MENU_RATING_BEST`) が当たればそれ。
/// 2. 否定端の語 (`MENU_RATING_WORST`) が当たったら、その反対側の端。
/// 3. ラベルが数字だけの尺度なら一番大きい数字。
/// 4. どれも判らなければ最後の選択肢 (既定)。
fn menu_rating_pick(labels: &[(u8, String)]) -> Option<u8> {
    if labels.len() < 2 {
        return None;
    }
    if let Some((n, _)) = labels
        .iter()
        .find(|(_, l)| label_hit(l, crate::agents::MENU_RATING_BEST))
    {
        return Some(*n);
    }
    if let Some(pos) = labels
        .iter()
        .position(|(_, l)| label_hit(l, crate::agents::MENU_RATING_WORST))
    {
        // 否定端が前半にあるなら肯定端は末尾、後半にあるなら先頭。
        let last = labels.len() - 1;
        let idx = if pos * 2 < last { last } else { 0 };
        return Some(labels[idx].0);
    }
    // 「1. 1 / 2. 2 …」のように本文まで数字だけの尺度。
    let nums: Option<Vec<u32>> = labels
        .iter()
        .map(|(_, l)| l.trim().parse::<u32>().ok())
        .collect();
    if let Some(nums) = nums {
        let best = nums.iter().enumerate().max_by_key(|(_, v)| **v)?.0;
        return Some(labels[best].0);
    }
    labels.last().map(|(n, _)| *n)
}

/// 番号メニューへの応答。答えを決められないときは **None**(人間に委ねる)。
///
/// 決め方は agents.rs の表だけで決まる:
/// 1. 見送り肢 (スキップ/あとで/回答しない) があればそれ。
/// 2. アンケート以外なら肯定肢 (yes/allow/続行/許可)。
/// 3. アンケートで見送り肢も無い(評点しか無い)場合も**必ず答える** —
///    肯定側の端を選び、選んだ番号を説明文に残す。止まる方が害が大きい、
///    というユーザーの判断。自由入力の質問はここに来ないので従来どおり人へ。
pub fn numbered_menu_reply(text: &str) -> Option<(&'static [u8], &'static str)> {
    // 管理者権限昇格など「自動で押してはいけない」画面は番号メニューでも撃たない。
    if crate::agents::prompt_never_answer(text) {
        return None;
    }
    let lc_all = crate::supervisor::strip_ansi(text).to_lowercase();
    // 矢印キーで選ぶ UI に数字を送らない (Enter 確定の CLI が別途処理する)。
    if crate::agents::MENU_ARROW_HINTS
        .iter()
        .any(|h| lc_all.contains(h))
    {
        return None;
    }
    let opts = numbered_menu_prompt(text)?;
    let labels: Vec<(u8, String)> = opts
        .iter()
        .map(|o| (o.num, o.label.to_lowercase()))
        .collect();
    let survey = crate::agents::MENU_SURVEY_MARKS
        .iter()
        .any(|m| lc_all.contains(m));
    let find = |f: &dyn Fn(&str) -> bool, kind: MenuPick| {
        labels.iter().find(|(_, l)| f(l)).map(|(n, _)| (*n, kind))
    };
    let picked = if survey {
        // アンケート/評価: ① 見送り肢 → ② それも無ければ肯定側の端。
        // 肯定肢 (「はい、回答します」) は選ばない — 意見の代筆になるため。
        find(&menu_is_skip, MenuPick::Skip)
            .or_else(|| menu_rating_pick(&labels).map(|n| (n, MenuPick::Rating)))
    } else {
        // 承認など: ① 肯定肢 → ② 見送り肢。
        // ここで見送りを先に見ると「No, and don't ask again」を選んでしまう。
        find(&menu_is_affirm, MenuPick::Affirm).or_else(|| find(&menu_is_skip, MenuPick::Skip))
    };
    let (num, kind) = picked?;
    let idx = usize::from(num).checked_sub(1)?;
    let key = *MENU_KEYS.get(idx)?;
    let desc = match kind {
        MenuPick::Affirm => MENU_DESC_ALLOW[idx],
        MenuPick::Skip => MENU_DESC_SKIP[idx],
        MenuPick::Rating => MENU_DESC_RATING[idx],
    };
    Some((key, desc))
}

/// プロンプト指紋の対象となるマーカー。scan_attention の検出パターンに加え、
/// auto_yes_reply だけが分類する特殊プロンプトも含める。
const SIG_MARKS: [&str; 47] = [
    "Antigravity",
    "Antigravity:",
    "AGY:",
    "Allow execute",
    "Allow tool",
    "Allow action",
    "Do you want",
    "Would you like to proceed",
    "Would you like to run",
    "needs your approval",
    "Do you want to approve network access",
    "Do you approve",
    "Do you allow",
    "Are you sure",
    "Confirm",
    "Proceed",
    "❯ 1. Yes",
    "1. Yes",
    "❯ 1. Allow",
    "1. Allow",
    "❯ 1. はい",
    "1. はい",
    "❯ 1. 許可",
    "1. 許可",
    "❯ 1. 実行",
    "1. 実行",
    "❯ 1. 承認",
    "1. 承認",
    "(y/n)",
    "[y/N]",
    "[y/n]",
    "(Y/n)",
    "[Y/n]",
    "(y/N)",
    "(Y/N)",
    "[y/n/a]",
    "[Y/n/a]",
    "(yes/no)",
    "[yes/no]",
    "Yes, I accept",
    "Yes, proceed",
    "Yes, allow",
    "trust the files in this folder",
    "Bypass Permissions mode",
    "Press Enter to continue",
    "Press Enter to proceed",
    "Press [Enter]",
];

/// 画面に出ている承認プロンプトの「指紋」。
///
/// マーカーを含む行と、その直前の行のテキストだけをハッシュする。
/// 直前行を含めるのは、Claude Code の連続承認キューのように
/// 「Do you want to proceed? / ❯ 1. Yes」自体は同一でも、直上のコマンド
/// プレビューが変わる = 別のプロンプト、を区別するため。行テキストのみで
/// 位置は使わないので、スクロールや下部の出力追加では変わらない。
/// **端末へ直接打ち込んだ 1 行の組み立て状態。**
///
/// 「途中まで打った本文」と「この行を捨てるべきか」の 2 つを持つ。
/// 捨てる印が要るのは、打鍵は **1 回の write に収まるとは限らない**ため。
/// 矢印キーだけが単独で届いたフレームで本文を消しても、次のフレームから
/// 何事も無かったように積み直してしまい、**本文の途中だけを覚える**
/// (「abc←def」を「def」として覚える) 事故になる。
/// 確定 (Enter) まで印を持ち越して、行ごと捨てる。
#[derive(Default)]
pub struct TypedLine {
    buf: String,
    /// この行に再現できない打鍵 (エスケープ列) が混ざったか。
    tainted: bool,
    /// **チャンク境界で文字を割らないための持ち越し。**
    /// 打鍵は 1 回の write に収まるとは限らず、日本語 1 文字 (3 バイト) や
    /// 絵文字 (4 バイト) は途中で切れて届く。ここを lossy で受けると
    /// `U+FFFD` が本文へ焼き付き、次のバイトが来ても直らない。
    dec: crate::textenc::StreamDecoder,
}

/// **打鍵バイト列から「確定した 1 行」を組み立てる (純関数)。**
///
/// `st` は呼び出しをまたいで持ち回る途中状態。確定 (CR / LF) が来たら
/// その行を返し、状態を空へ戻す。1 回の呼び出しに複数行が入っていれば
/// **最後の行**を返す (直近の指示が欲しいので、古い行は捨てて良い)。
///
/// 扱う制御文字:
/// - `\r` / `\n` … 確定
/// - `\x7f` / `\x08` (Backspace) … 1 文字消す
/// - `\x03` (Ctrl+C) / `\x15` (Ctrl+U) … 行を捨てる
/// - `ESC` から始まる列 … カーソル移動などなので**その行ごと捨てる**
///   (途中に矢印キーが入った行を正しく再現するのは無理筋で、
///   間違った本文を覚えるより覚えない方が良い)
/// - bracketed paste の囲み … 剥がして中身だけ残す
/// - その他の制御文字 … 無視
///
/// UTF-8 の途中で切れたバイト列が来ても壊れない。**切れた末尾は置換文字にせず
/// 次の呼び出しへ持ち越す**ので、「あ」が `\u{FFFD}` として本文に残らない。
/// 本物の不正な並びは従来どおり置換する (「まだ来ていない」と「壊れている」は別物)。
pub fn feed_typed_line(st: &mut TypedLine, bytes: &[u8]) -> Option<String> {
    // bracketed paste の囲みを剥がす (中身は普通の文字として扱う)。
    let text = st.dec.feed(bytes);
    let text = text.replace("\u{1b}[200~", "").replace("\u{1b}[201~", "");
    let mut out: Option<String> = None;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' | '\n' => {
                // CRLF は 1 回の確定として扱う。
                if ch == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                let line = std::mem::take(&mut st.buf);
                let tainted = std::mem::take(&mut st.tainted);
                if !tainted && !line.trim().is_empty() {
                    out = Some(line);
                }
            }
            '\u{7f}' | '\u{8}' => {
                st.buf.pop();
            }
            '\u{3}' | '\u{15}' => {
                st.buf.clear();
                st.tainted = false;
            }
            '\u{1b}' => {
                // エスケープ列が混ざった行は再現できないので、
                // **確定まで印を持ち越して**行ごと捨てる。
                st.buf.clear();
                st.tainted = true;
                // 続く列そのものも読み飛ばす (次の確定は下の分岐で拾う)。
                while let Some(&c) = chars.peek() {
                    if c == '\r' || c == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            '\t' => st.buf.push(' '),
            c if c.is_control() => {}
            c => {
                // 暴走した貼り付けで無制限に伸びないよう頭打ちにする。
                if st.buf.chars().count() < PROMPT_KEEP_CHARS {
                    st.buf.push(c);
                }
            }
        }
    }
    out
}

pub fn prompt_signature(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        // 目印は固定表 + 応答表 (agents.rs) の needles。表に足したパターンは
        // 指紋にも自動で効くので、片方だけ更新して取りこぼす事故が起きない。
        // 番号メニューの選択肢行も指紋に含める。選択肢だけが差し替わる
        // 連続プロンプト (アンケートの次の設問など) を別物として区別できる。
        let marked = SIG_MARKS.iter().any(|m| line.contains(m))
            || crate::agents::prompt_sig_marks().any(|m| line.contains(m))
            || parse_option_line(line).is_some();
        if marked || is_question_line(line) {
            if i > 0 {
                lines[i - 1].trim_end().hash(&mut h);
            }
            line.trim_end().hash(&mut h);
        }
    }
    h.finish()
}

// ── 端末問い合わせへの応答 (query / response) ────────────────────────────
//
// vt100 は「読むだけ」の実装で、端末側から返事を書き戻さない。ところが TUI
// アプリ(Neovim / Helix / lazygit / yazi / k9s …)は起動時にカーソル位置や
// 端末種別を問い合わせ、**返事が来るまで待つ**。無視すると固まるか、返事の
// 代わりに問い合わせ文字列そのものがアプリの入力バッファへ紛れ込み、ユーザー
// には「勝手に変な文字が打たれた」ように見える。
//
// そこで PTY 出力を vt100 とは別に軽く走査し、該当シーケンスへ PTY へ返事を
// 書き戻す。読み込みチャンクの途中でシーケンスが切れる(CSI 6n が "\x1b[6" と
// "n" に分かれて届く)のが定番の落とし穴なので、未完成分は pending に持ち越す。

/// カーソル形状 (DECSCUSR: CSI Ps SP q)。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CursorShape {
    /// Ps = 0,1,2 — ブロック(既定)
    #[default]
    Block,
    /// Ps = 3,4 — アンダーライン
    Underline,
    /// Ps = 5,6 — 縦バー(Neovim / Helix の挿入モード)
    Bar,
}

impl CursorShape {
    fn from_ps(ps: u16) -> Self {
        match ps {
            3 | 4 => CursorShape::Underline,
            5 | 6 => CursorShape::Bar,
            // 0,1,2 と未知の値はブロック扱い(xterm と同じ挙動)
            _ => CursorShape::Block,
        }
    }
    fn to_u8(self) -> u8 {
        match self {
            CursorShape::Block => 0,
            CursorShape::Underline => 1,
            CursorShape::Bar => 2,
        }
    }
    fn from_u8(v: u8) -> Self {
        match v {
            1 => CursorShape::Underline,
            2 => CursorShape::Bar,
            _ => CursorShape::Block,
        }
    }
}

/// 走査で見つかった「端末が反応すべき事柄」。
#[derive(Debug, PartialEq, Eq)]
pub enum TermEvent {
    /// そのまま PTY へ書き戻す固定の返事。
    Reply(Vec<u8>),
    /// CSI 6n — 返事にカーソル位置が要るので呼び出し側で組み立てる。
    CursorReport,
    /// CSI ?6n (DECXCPR) — 同上だが返事に "?" が付く。
    ExtCursorReport,
    /// DECSCUSR によるカーソル形状変更。
    CursorShape(CursorShape),
    /// CSI ?1004h/l — フォーカス通知の要求/解除。
    FocusReports(bool),
    /// OSC 52 — システムクリップボードへの書き込み要求。
    Clipboard(String),
    /// OSC 10/11 の色問い合わせ (10=前景 / 11=背景)。
    ColorQuery(u8),
    /// OSC 633 / OSC 133 — シェル統合のマーカー (`crate::shellint`)。
    /// **これが来る間は、コマンドの境界と終了コードを画面から推測しない**。
    Shell(crate::shellint::Marker),
}

/// Primary DA (CSI c) の返事。
///
/// `CSI ?62;1;6;9;15;22c` = VT220 相当 + 132桁(1) + 選択消去(6) + NRCS(9) +
/// テクニカル文字(15) + **ANSIカラー(22)**。xterm-256color を名乗る端末が返す
/// 典型値に合わせてある。22 を含めるのでアプリは色を有効にし、逆に **4(sixel)
/// を含めない**ので yazi / ranger は画像プレビューを諦めてテキストへ落ちる
/// (こちらは sixel を描けないため、これが正しい断り方)。
const DA1_REPLY: &[u8] = b"\x1b[?62;1;6;9;15;22c";

/// Secondary DA (CSI >c)。>0 = VT100系, 95 = ファームウェア版, 0 = ROM版。
/// 「素性の知れた無害な端末」として扱われる値。
const DA2_REPLY: &[u8] = b"\x1b[>0;95;0c";

/// Tertiary DA (CSI =c / DECRPTUI)。ユニットIDは全ゼロ。
const DA3_REPLY: &[u8] = b"\x1bP!|00000000\x1b\\";

/// 持ち越しバッファの上限。これを超えて閉じないシーケンスは壊れているとみなす。
const MAX_PENDING: usize = 64 * 1024;
/// OSC 52 の base64 入力長の上限(復号前)。
const MAX_CLIPBOARD_B64: usize = 512 * 1024;

/// 1つのシーケンスを読んだ結果。
enum SeqParse {
    /// n バイト消費した(返事の有無に関わらず前進する)。
    Consumed(usize),
    /// チャンク境界で切れている。次の read と繋げて読み直す。
    Incomplete,
}

/// PTY 出力ストリームの先読み走査器。read のたびに `scan` を呼ぶ。
#[derive(Default)]
pub struct QueryScanner {
    /// チャンク境界で切れたシーケンスの断片。
    pending: Vec<u8>,
}

impl QueryScanner {
    /// チャンクを走査してイベント列を返す。
    ///
    /// vt100 へは呼び出し側が別途「全バイトをちょうど1回」流す。こちらの
    /// pending は完全に独立したバッファなので、二重投入にはならない。
    pub fn scan(&mut self, chunk: &[u8]) -> Vec<TermEvent> {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut i = 0usize;
        let mut incomplete: Option<usize> = None;
        while i < buf.len() {
            if buf[i] != 0x1b {
                i += 1;
                continue;
            }
            match parse_seq(&buf[i..], &mut out) {
                SeqParse::Consumed(n) => i += n.max(1),
                SeqParse::Incomplete => {
                    incomplete = Some(i);
                    break;
                }
            }
        }
        self.pending = match incomplete {
            Some(s) if buf.len() - s <= MAX_PENDING => buf[s..].to_vec(),
            // 上限超え(閉じない OSC など)は諦めて捨てる。無限に太らせない。
            _ => Vec::new(),
        };
        out
    }
}

/// buf[0] == ESC 前提で1シーケンスを読む。
fn parse_seq(buf: &[u8], out: &mut Vec<TermEvent>) -> SeqParse {
    if buf.len() < 2 {
        return SeqParse::Incomplete;
    }
    match buf[1] {
        b'[' => parse_csi(buf, out),
        b']' => parse_string(buf, 2, out, on_osc),
        b'P' => parse_string(buf, 2, out, on_dcs),
        b'_' => parse_string(buf, 2, out, on_apc),
        // ESC ESC は前の1つが捨てられた合図。2つ目から読み直す。
        0x1b => SeqParse::Consumed(1),
        // ESC = / ESC M / ESC ( B など。関心が無いので2バイト進めるだけ。
        _ => SeqParse::Consumed(2),
    }
}

/// CSI: ESC [ <params 0x30-0x3F> <intermediates 0x20-0x2F> <final 0x40-0x7E>
fn parse_csi(buf: &[u8], out: &mut Vec<TermEvent>) -> SeqParse {
    let mut i = 2;
    while i < buf.len() && (0x30..=0x3f).contains(&buf[i]) {
        i += 1;
    }
    let pend = i;
    while i < buf.len() && (0x20..=0x2f).contains(&buf[i]) {
        i += 1;
    }
    if i - 2 > 256 {
        // 常識外に長い = 壊れている。ESC 1バイト分だけ進めて同期し直す。
        return SeqParse::Consumed(1);
    }
    if i >= buf.len() {
        return SeqParse::Incomplete;
    }
    let final_b = buf[i];
    if !(0x40..=0x7e).contains(&final_b) {
        return SeqParse::Consumed(1);
    }
    let params = &buf[2..pend];
    let inter = &buf[pend..i];
    on_csi(params, inter, final_b, out);
    SeqParse::Consumed(i + 1)
}

fn on_csi(params: &[u8], inter: &[u8], final_b: u8, out: &mut Vec<TermEvent>) {
    match (final_b, inter) {
        // ── DSR: 端末状態の問い合わせ ──
        (b'n', b"") => match params {
            b"6" => out.push(TermEvent::CursorReport),
            b"5" => out.push(TermEvent::Reply(b"\x1b[0n".to_vec())),
            b"?6" => out.push(TermEvent::ExtCursorReport),
            _ => {}
        },
        // ── Device Attributes ──
        (b'c', b"") => match params {
            b"" | b"0" => out.push(TermEvent::Reply(DA1_REPLY.to_vec())),
            b">" | b">0" => out.push(TermEvent::Reply(DA2_REPLY.to_vec())),
            b"=" | b"=0" => out.push(TermEvent::Reply(DA3_REPLY.to_vec())),
            _ => {}
        },
        // ── DECSCUSR: CSI Ps SP q (中間バイトが空白なのが目印) ──
        (b'q', b" ") => {
            let ps = parse_num(params).unwrap_or(0);
            out.push(TermEvent::CursorShape(CursorShape::from_ps(ps)));
        }
        // ── XTVERSION: CSI > Ps q (中間バイト無し) ──
        // kitty / WezTerm を名乗ると解釈できないプロトコルを送られるので、
        // 素直に自分の名前を返して「特別扱いしないでくれ」と伝える。
        (b'q', b"") if params.first() == Some(&b'>') => {
            let name = format!(
                "\x1bP>|Zaivern Code({})\x1b\\",
                option_env!("CARGO_PKG_VERSION").unwrap_or("0")
            );
            out.push(TermEvent::Reply(name.into_bytes()));
        }
        // ── DEC プライベートモードの set/reset ──
        (b'h', b"") | (b'l', b"") if params.first() == Some(&b'?') => {
            let set = final_b == b'h';
            for p in params[1..].split(|c| *c == b';') {
                if p == b"1004" {
                    out.push(TermEvent::FocusReports(set));
                }
            }
        }
        // ── kitty キーボードプロトコル問い合わせ (CSI ?u) ──
        // ここで `CSI ?0u` などを返すと「対応している」と誤解され、以後 kitty
        // 形式のキー入力を期待されてしまう(こちらは生成できない)。仕様どおり
        // 黙って捨てるのが正しい断り方で、アプリは直後に必ず送ってくる DA1 の
        // 返事(上で応答済み)で「非対応」と判定して従来のキー入力へ落ちる。
        (b'u', b"?") => {}
        _ => {}
    }
}

/// 先頭の10進数を読む(空なら None)。
fn parse_num(s: &[u8]) -> Option<u16> {
    let mut n: u32 = 0;
    let mut any = false;
    for &c in s {
        if !c.is_ascii_digit() {
            break;
        }
        any = true;
        n = n.saturating_mul(10).saturating_add((c - b'0') as u32);
    }
    if any {
        Some(n.min(u16::MAX as u32) as u16)
    } else {
        None
    }
}

/// OSC / DCS / APC の共通形: <導入> ... (BEL | ESC \)
fn parse_string(
    buf: &[u8],
    body_start: usize,
    out: &mut Vec<TermEvent>,
    f: fn(&[u8], &mut Vec<TermEvent>),
) -> SeqParse {
    let mut j = body_start;
    while j < buf.len() {
        match buf[j] {
            0x07 => {
                f(&buf[body_start..j], out);
                return SeqParse::Consumed(j + 1);
            }
            0x1b => {
                if j + 1 >= buf.len() {
                    return SeqParse::Incomplete;
                }
                if buf[j + 1] == b'\\' {
                    f(&buf[body_start..j], out);
                    return SeqParse::Consumed(j + 2);
                }
                // ST 以外の ESC = 文字列シーケンスの中断。その ESC から読み直す。
                return SeqParse::Consumed(j);
            }
            _ => j += 1,
        }
    }
    SeqParse::Incomplete
}

fn on_osc(body: &[u8], out: &mut Vec<TermEvent>) {
    let (ps, rest) = match body.iter().position(|c| *c == b';') {
        Some(k) => (&body[..k], &body[k + 1..]),
        None => (body, &body[body.len()..]),
    };
    match ps {
        // OSC 52: クリップボード。"52;<選択先>;<base64>"
        b"52" => {
            let data = match rest.iter().position(|c| *c == b';') {
                Some(k) => &rest[k + 1..],
                None => return,
            };
            // "?" は読み出し要求。端末の中身を勝手に渡すのは危険なので断る。
            if data == b"?" || data.is_empty() {
                return;
            }
            if data.len() > MAX_CLIPBOARD_B64 {
                return;
            }
            if let Some(bytes) = base64_decode(data) {
                if let Ok(s) = String::from_utf8(bytes) {
                    out.push(TermEvent::Clipboard(s));
                }
            }
        }
        // OSC 10/11: 前景色/背景色の問い合わせ。Neovim が 'background' の
        // 自動判定に使う。無視すると返事待ちの分だけ起動が遅れる。
        b"10" | b"11" if rest.first() == Some(&b'?') => {
            let n = if ps == b"10" { 10 } else { 11 };
            out.push(TermEvent::ColorQuery(n));
        }
        // OSC 633 / 133: シェル統合。プロンプトとコマンドの境界・終了コード・
        // コマンド行そのものが構造化されて届く (crate::shellint の説明を参照)。
        // 読むだけで返事はしないので、対応していない発行元にも副作用が無い。
        b"633" | b"133" => {
            if let Some(m) = crate::shellint::parse_osc(ps, rest) {
                out.push(TermEvent::Shell(m));
            }
        }
        _ => {}
    }
}

fn on_dcs(body: &[u8], out: &mut Vec<TermEvent>) {
    // XTGETTCAP: DCS + q <cap を16進にしたもの> ST
    // 対応していないので「失敗」形式 DCS 0 + r <要求内容> ST を返す。黙って
    // いると問い合わせ側が固まる。
    if body.starts_with(b"+q") {
        let mut r = Vec::with_capacity(body.len() + 8);
        r.extend_from_slice(b"\x1bP0+r");
        r.extend_from_slice(&body[2..]);
        r.extend_from_slice(b"\x1b\\");
        out.push(TermEvent::Reply(r));
    }
}

fn on_apc(body: &[u8], out: &mut Vec<TermEvent>) {
    // kitty グラフィックスプロトコルの打診: ESC _ G <key=value,...>;<payload> ESC \
    // 画像は描けないのでエラー応答で明確に断る。黙っていると yazi などが
    // タイムアウトまで固まる(調査で最も危険とされたケース)。
    if body.first() != Some(&b'G') {
        return;
    }
    let ctrl = match body.iter().position(|c| *c == b';') {
        Some(k) => &body[1..k],
        None => &body[1..],
    };
    let mut id: &[u8] = b"0";
    for kv in ctrl.split(|c| *c == b',') {
        if let Some(v) = kv.strip_prefix(b"i=") {
            id = v;
        }
        // q=2 は「応答不要」。仕様どおり黙る。
        if kv == b"q=2" {
            return;
        }
    }
    let mut r = Vec::with_capacity(id.len() + 24);
    r.extend_from_slice(b"\x1b_Gi=");
    r.extend_from_slice(id);
    r.extend_from_slice(b";ENOTSUPPORTED\x1b\\");
    out.push(TermEvent::Reply(r));
}

/// CSI 6n / CSI ?6n の返事を組み立てる。row/col は 0 始まり、返事は 1 始まり。
pub fn cursor_report(row: u16, col: u16, ext: bool) -> Vec<u8> {
    let q = if ext { "?" } else { "" };
    format!("\x1b[{}{};{}R", q, row as u32 + 1, col as u32 + 1).into_bytes()
}

/// OSC 10/11 の返事。xterm と同じ 16bit/成分の rgb: 形式で返す。
pub fn color_report(ps: u8, rgb: u32) -> Vec<u8> {
    let (r, g, b) = ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
    format!("\x1b]{ps};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\").into_bytes()
}

/// 標準 base64 の復号(依存追加を避けるため自前)。不正なら None。
fn base64_decode(src: &[u8]) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let mut out = Vec::with_capacity(src.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut nbits: u32 = 0;
    let mut pad = 0usize;
    for &c in src {
        // 長い payload は改行で折り返されて届くことがある
        if c == b'\r' || c == b'\n' {
            continue;
        }
        if c == b'=' {
            pad += 1;
            continue;
        }
        // パディングの後ろにデータが来るのは不正
        if pad > 0 {
            return None;
        }
        acc = (acc << 6) | val(c)?;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
            acc &= (1u32 << nbits) - 1;
        }
    }
    // 余りが 6bit 以上 = 4文字境界に 1 文字だけ余った不正な入力
    if pad > 2 || nbits >= 6 || acc != 0 {
        return None;
    }
    Some(out)
}

/// **端末へ流れ込むバイト列の符号化。**
///
/// 既定は自動 — UTF-8 を 1 バイトも変えずに素通しし、ISO-2022-JP の切替列を
/// 実際に見たときだけ変換に入る。CP932 / EUC-JP のように切替列を持たない
/// 符号化は見ただけではバイナリと区別が付かないので自動では入らない。
/// `ZAIVERN_TERM_ENCODING` (`cp932` / `euc-jp` / `cp<番号>` / `auto`) で固定できる。
///
/// 読めない名前は既定へ落とす — 綴りを間違えた瞬間に端末が使えなくなる方が悪い。
fn term_encoding() -> crate::textenc::TermEncoding {
    std::env::var("ZAIVERN_TERM_ENCODING")
        .ok()
        .and_then(|s| crate::textenc::term_encoding_by_name(&s))
        .unwrap_or_default()
}

/// セッション復元時、再生した前回スクロールバックの末尾へ入れる区切りバナー。
/// 先頭で代替画面 (?1049) とスクロール領域・文字属性を平常へ戻す — 前回ログが
/// TUI の途中で切れていても、バナーと今回の出力が壊れずに描かれるようにする。
/// `ESC[r` はカーソルをホームへ戻してしまうので、`ESC[999;1H` で最下行へ
/// 移してから書く (再生した最終行の**後ろ**にバナーが並ぶ)。
pub const RESTORE_BANNER: &str = "\x1b[?1049l\x1b[r\x1b[0m\x1b[999;1H\r\n\x1b[2m── 前回のセッションここまで / 再開します ──\x1b[0m\r\n";

/// パーサを作り直したときに、新しい画面の先頭へ流す 1 行のお知らせ。
/// 「黒いまま何も出ない」を絶対に作らないための最低限の手掛かり。
pub const PARSER_REBUILT_BANNER: &str =
    "\x1b[0m\x1b[2m── 端末の描画状態を作り直しました / これより前の履歴は失われています ──\x1b[0m\r\n";

impl Session {
    /// 前回セッションの生ログ (PTY 生バイト列) を vt100 パーサへ流し込み、
    /// 旧スクロールバックを見える状態にする。末尾に [`RESTORE_BANNER`] を足して
    /// 「どこからが今回か」を分かるようにする。spawn 直後 (エージェントの最初の
    /// 出力が届く前) に呼ぶ想定 — 読取スレッドとはパーサのロックで排他される。
    pub fn preload_scrollback(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // 生ログは PTY のバイト列そのままなので、読取スレッドと**同じ**
        // 正規化を通す (通さないと復元した画面だけが化ける)。
        let mut norm = crate::textenc::TermDecoder::new(term_encoding());
        let mut p = lock_ok(&self.parser);
        p.process(norm.feed(bytes));
        let rest = norm.finish();
        if !rest.is_empty() {
            p.process(rest);
        }
        p.process(RESTORE_BANNER.as_bytes());
    }

    pub fn spawn(id: u64, spec: SpawnSpec, ctx: egui::Context) -> Result<Self, String> {
        let (rows, cols) = (30u16, 110u16);
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| trf("PTYを開けませんでした: {e}", &[("e", e.to_string())]))?;

        // 作業ディレクトリは PTY へ渡す前に「素の実在ディレクトリ」へ直す。
        // Windows の `\\?\` 付きパスを渡すと cmd.exe が受け付けず、
        // 黙って C:\Windows で起動してしまう (pathx の説明を参照)。
        // 直した cwd はセッションにもそのまま持たせる (@パスの相対表示が
        // 実際の起動先と食い違わないようにするため)。
        let cwd = crate::pathx::launch_dir(&spec.cwd);
        let cmd = build_command(&spec.command, &cwd, &spec.env);
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| trf("起動に失敗しました: {e}", &[("e", e.to_string())]))?;
        let killer = child.clone_killer();
        // child はこの後 wait 用スレッドへ渡してしまうので、PID は今のうちに取る。
        let child_pid = child.process_id();
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_ROWS)));
        let exited = Arc::new(AtomicBool::new(false));
        let exit_code: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;

        // PTY への書き込みは専用スレッドに任せる (PtyWriter の説明を参照)。
        // 送り手 (UI スレッド / 読取スレッド) が全員居なくなると recv が切れて畳まれる。
        let writer = {
            let mut w = pair.master.take_writer().map_err(|e| e.to_string())?;
            let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
            let queued = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = queued.clone();
            std::thread::spawn(move || {
                while let Ok(chunk) = rx.recv() {
                    let n = chunk.len();
                    let ok = w.write_all(&chunk).is_ok() && w.flush().is_ok();
                    counter.fetch_sub(n, Ordering::Relaxed);
                    if !ok {
                        break; // PTY が閉じた。以降の入力は届かない。
                    }
                }
            });
            PtyWriter { tx, queued }
        };
        let cursor_shape = Arc::new(AtomicU8::new(CursorShape::Block.to_u8()));
        let shell = Arc::new(Mutex::new(crate::shellint::Tracker::new()));
        let lines: Arc<LineIndex> = Arc::new(LineIndex::default());
        let focus_reports = Arc::new(AtomicBool::new(false));
        let clipboard_pending: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        // 既定はダークテーマ寄りの色。app.rs から set_report_colors で上書きできる。
        let report_fg = Arc::new(AtomicU32::new(0xe6e6e6));
        let report_bg = Arc::new(AtomicU32::new(0x12141a));

        // 生ログの書き出し (F5: スクロールバック永続化)。ヘッダで起動を区切る。
        let log_sink: Option<LogSink> = spec.log_path.as_ref().and_then(|p| {
            let epoch = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let header = format!(
                "\n===== [Zaivern] {} — `{}` (epoch {}) =====\n",
                spec.title, spec.command, epoch
            );
            LogSink::open(p, &header)
        });

        {
            let parser = parser.clone();
            let exited = exited.clone();
            let ctx = ctx.clone();
            let writer = writer.clone();
            let cursor_shape = cursor_shape.clone();
            let shell = shell.clone();
            let lines = lines.clone();
            let focus_reports = focus_reports.clone();
            let clipboard_pending = clipboard_pending.clone();
            let report_fg = report_fg.clone();
            let report_bg = report_bg.clone();
            let mut log_sink = log_sink;
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let mut scanner = QueryScanner::default();
                // PTY のバイト列は UTF-8 とは限らない。ISO-2022-JP の切替列を
                // vte は「知らないエスケープ」として黙って食べるので、後続の
                // JIS バイトだけが ASCII として画面に残り `$3$s$K…` になる
                // (textenc の記録を参照)。vt100 へ渡す前に UTF-8 へ揃える。
                // 素の UTF-8 は 1 バイトも変えずに素通しする。
                let mut norm = crate::textenc::TermDecoder::new(term_encoding());
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            // 境界で持ち越したバイトを取り残さない。
                            let rest = norm.finish();
                            if !rest.is_empty() {
                                lock_ok(&parser).process(rest);
                            }
                            break;
                        }
                        Ok(n) => {
                            if let Some(l) = log_sink.as_mut() {
                                // 生ログは**生のまま**残す (再生時に同じ経路を通す)。
                                l.write(&buf[..n]);
                            }
                            let mut reply: Vec<u8> = Vec::new();
                            let mut moved_total = 0u64;
                            // シェル統合のマーカーで区切って流す。マーカーが
                            // 無ければ区間は 1 本 = 従来と同じ経路
                            // (**入れていない利用者は 1 バイトも違わない**)。
                            for seg in shell_segments(&buf[..n]) {
                                // 先に vt100 へ流してから走査する。CSI 6n はアプリが
                                // 「ここまで描いた」直後に送って返事を待つものなので、
                                // チャンクを反映し終えたカーソル位置が正解になる。
                                // ついでに押し出された行数を数える (通し番号の素)。
                                let line = {
                                    let mut p = lock_ok(&parser);
                                    // 1 回の process が押し出せる行数は履歴の容量で
                                    // 頭打ちになる (それ以上は痕跡が消えて数えられ
                                    // ない)。容量より小さく刻んで必ず数え切る。
                                    let mut scrolled = lines.scrolled();
                                    for piece in seg.chunks(FEED_CHUNK) {
                                        let bytes = norm.feed(piece);
                                        let (_, moved, sb_len) =
                                            count_around(&mut p, |p| p.process(bytes));
                                        moved_total += moved;
                                        scrolled = lines.advance(moved, sb_len);
                                    }
                                    // 代替画面 (vim / less / TUI) には履歴が無く、
                                    // 通し番号を数えられない。**無いものを 0 行目と
                                    // 偽らない** — 位置抜きで記録する。
                                    (!p.screen().alternate_screen()).then(|| {
                                        // 区間はマーカーの終端で切ってあるので、
                                        // いまのカーソル行がそのままマーカーの行。
                                        scrolled + u64::from(p.screen().cursor_position().0)
                                    })
                                };
                                for ev in scanner.scan(seg) {
                                    match ev {
                                        TermEvent::Reply(b) => reply.extend_from_slice(&b),
                                        TermEvent::CursorReport | TermEvent::ExtCursorReport => {
                                            let ext = matches!(ev, TermEvent::ExtCursorReport);
                                            let (r, c) = {
                                                let p = lock_ok(&parser);
                                                p.screen().cursor_position()
                                            };
                                            reply.extend_from_slice(&cursor_report(r, c, ext));
                                        }
                                        TermEvent::CursorShape(s) => {
                                            cursor_shape.store(s.to_u8(), Ordering::Relaxed);
                                        }
                                        TermEvent::FocusReports(on) => {
                                            focus_reports.store(on, Ordering::Relaxed);
                                        }
                                        TermEvent::Clipboard(s) => {
                                            *lock_ok(&clipboard_pending) = Some(s);
                                        }
                                        TermEvent::ColorQuery(ps) => {
                                            let rgb =
                                                if ps == 10 { &report_fg } else { &report_bg }
                                                    .load(Ordering::Relaxed);
                                            reply.extend_from_slice(&color_report(ps, rgb));
                                        }
                                        // シェル統合。ここでの仕事は小さな構造体を
                                        // 1 つ積むだけ — 読取スレッドを止めない
                                        // (設計原則 2)。UI が居なくても記録は進む。
                                        TermEvent::Shell(m) => {
                                            let mut t = lock_ok(&shell);
                                            match line {
                                                Some(l) => {
                                                    let now = t.now_ms();
                                                    t.feed_at_line(m, now, Some(l));
                                                }
                                                None => t.feed(m),
                                            }
                                        }
                                    }
                                }
                            }
                            // 履歴から落ちた行を指すブロックを捨てる。
                            // これをやらないと「もう画面に無い行」を指す
                            // ブロックが溜まり続ける (行数の上限と件数の
                            // 上限が食い違う)。窓が動いたときだけでよい。
                            if moved_total > 0 {
                                let oldest = lines.oldest_live();
                                lock_ok(&shell).forget_before(oldest);
                            }
                            if !reply.is_empty() {
                                writer.send(&reply);
                            }
                            crate::perf::repaint(&ctx, "pty_read");
                        }
                    }
                }
                exited.store(true, Ordering::SeqCst);
                crate::perf::repaint(&ctx, "pty_read");
            });
        }
        {
            let exit_code = exit_code.clone();
            let exited = exited.clone();
            std::thread::spawn(move || {
                if let Ok(status) = child.wait() {
                    *lock_ok(&exit_code) = Some(status.exit_code());
                }
                exited.store(true, Ordering::SeqCst);
                crate::perf::repaint(&ctx, "pty_exit");
            });
        }

        // 全自動起動の判定は 2 ルートある。フラグ型 (claude の
        // --dangerously-skip-permissions など) と、フラグを持たない CLI の
        // 環境変数型 (goose の GOOSE_MODE=auto / aider の AIDER_YES_ALWAYS=1)。
        // 後者を見ないと goose / aider は Auto でも全自動YESが働かない。
        let launched_bypass = crate::agents::command_is_bypass(&spec.command)
            || crate::agents::env_enables_auto(&spec.command, &spec.env);

        Ok(Self {
            id,
            title: spec.title,
            preset_name: spec.preset_name,
            icon: spec.icon,
            command: spec.command,
            cwd,
            env: spec.env,
            parser,
            writer,
            master: Arc::new(Mutex::new(pair.master)),
            resize_debounce: ResizeDebounce::settled((rows, cols)),
            resizer: None,
            resizer_spawn_failed: false,
            parser_rebuilt_notice: None,
            killer,
            child_pid,
            exited,
            exit_code,
            started: Instant::now(),
            size: (rows, cols),
            scroll: 0,
            preedit: String::new(),
            attention: false,
            attention_since: None,
            notified_exit: false,
            launched_bypass,
            last_scan: Instant::now(),
            answered_sig: None,
            auto_stall_since: None,
            auto_stall_hash: 0,
            auto_yes_resend_after: Duration::from_secs(30),
            selection: None,
            sel_abs: None,
            sel_pushed: 0,
            sel_anchor_abs: None,
            search: SearchUi::default(),
            copied_at: None,
            user_typed: false,
            cursor_shape,
            shell,
            lines,
            focus_reports,
            focus_sent: None,
            clipboard_pending,
            report_fg,
            report_bg,
            seen_hash: 0,
            cur_hash: 0,
            prev_hash: 0,
            pinned_unread: false,
            rate_limited: None,
            rl_miss: 0,
            rl_hits: 0,
            last_prompt: None,
            log_path: spec.log_path,
            links: LinkState::default(),
            typed_line: TypedLine::default(),
        })
    }

    /// bypass バッジ文字(⚡=bypass起動 / 🛡=通常)。
    pub fn approval_badge(&self) -> &'static str {
        if self.launched_bypass {
            "⚡"
        } else {
            "🛡"
        }
    }

    /// このセッションのコマンドに対応するカタログ定義。
    ///
    /// 先頭トークンの**末尾パス要素**で引くので、`/usr/local/bin/claude` や
    /// `~/.local/bin/agy` のような絶対/相対パス起動でも正しく一致する
    /// (以前は生の先頭トークンを文字列比較していたため、パス付きだと
    /// 既存の claude / codex / agy でも権限機能が丸ごと効かなかった)。
    fn spec(&self) -> Option<&'static crate::agents::AgentSpec> {
        crate::agents::spec_for_command(&self.command)
    }

    /// Zaivern 側で承認モードを統一制御している CLI エージェントか。
    /// 判定はカタログ由来なので、カタログに足した CLI は自動的に対象になる。
    pub fn is_permission_agent(&self) -> bool {
        self.spec().is_some()
    }

    /// このセッションのエージェント名 (`agy` / `claude` …)。カタログ外なら None。
    /// 別名 (`antigravity` / `antigravity-cli`) はカタログ側で正規化されるので、
    /// ここでは常に正規の `bin` 名が返る。
    pub fn agent_bin(&self) -> Option<&'static str> {
        self.spec().map(|s| s.bin)
    }

    /// 実行中セッションへ送れる権限モード切替のキー列。
    /// 実機で確認できていない CLI では None(誤ったキーを送らない)。
    pub fn permission_switch_keys(&self) -> Option<&'static [u8]> {
        self.spec()?.switch_keys_bytes()
    }

    /// 権限モード切替ボタンの説明。未確認の CLI では None。
    pub fn permission_switch_hint(&self) -> Option<&'static str> {
        self.spec()?.switch_hint_text()
    }

    /// メニューの自動YES (`pet_auto_yes` = allow) の対象セッションか。
    /// 対象はカタログ既知の CLI のみ(素のシェルの y/n プロンプトへは撃ち込まない)。
    /// 起動時の承認モード (Ask/bypass) には依存しない — 以前は bypass 起動のみを
    /// 対象にしていたため、Ask 起動だと自動YESをオンにしても何も送られなかった。
    pub fn auto_yes_target(&self, allow: bool) -> bool {
        allow && self.is_permission_agent()
    }

    /// 画面内容から「ユーザーの承認待ち」を推定する(約1秒間隔)。
    /// auto_yes=true なら承認プロンプトへ自動でYESを送信し AutoReplied を返す。
    /// それ以外は、新たに承認待ちへ遷移したときだけ NeedsApproval を返す。
    ///
    /// 応答(自動YES・バブルの承認/拒否)済みのプロンプトは、画面に残っていても
    /// 再送・再検出しない — 1プロンプトにつき応答は一回で完結する。
    /// プロンプトが消えるか、指紋の異なる別プロンプトに変わったら再び対象になる。
    pub fn scan_attention(&mut self, auto_yes: bool) -> Option<Attention> {
        if self.last_scan.elapsed().as_millis() < 900 {
            return None;
        }
        self.last_scan = Instant::now();
        let text = lock_ok(&self.parser).screen().contents();
        // 未読判定用: 意味的な画面ハッシュを更新する (スピナー等の揺れは無視)。
        // 直前のスキャン時の値を控えてから更新する (`output_advanced` の材料)。
        self.prev_hash = self.cur_hash;
        self.cur_hash = semantic_hash(&text);
        // レート制限の「継続 / 解除」の追跡。新規検知の確定は末尾で行う
        // (承認イベントと同時のときは承認を優先し、通知を次回スキャンへ持ち越すため)。
        let rl_detect = detect_rate_limit(&text);
        if self.rate_limited.is_some() {
            match &rl_detect {
                Some(line) => {
                    self.rate_limited = Some(line.clone());
                    self.rl_miss = 0;
                    self.rl_hits = self.rl_hits.saturating_add(1);
                }
                None => {
                    self.rl_miss += 1;
                    if self.rl_miss >= 2 {
                        self.rate_limited = None;
                        self.rl_miss = 0;
                        self.rl_hits = 0;
                    }
                }
            }
        }
        const PATTERNS: [&str; 6] = [
            "Do you want",
            "Would you like to proceed",
            "❯ 1. Yes",
            "1. Yes",
            "(y/n)",
            "[y/N]",
        ];
        // 応答表の絞り込みに使うため、このセッションのエージェント名を渡す。
        // (Antigravity 用のルールが claude のセッションへ流れ込まない)
        let reply = auto_yes_reply_for(&text, self.agent_bin());
        // 番号入力メニューは PATTERNS のどれにも当たらない形なので別に見る。
        // 答えが決まらない画面 (評点しか無いアンケート等) でもここで検知され、
        // 「承認待ち」として看板/承認 UI に出る = 黙って止まったままにならない。
        let present = reply.is_some()
            || PATTERNS.iter().any(|p| text.contains(p))
            || numbered_menu_prompt(&text).is_some();
        // 応答済みエピソードの追跡: プロンプトが画面から消えた、または指紋が
        // 変わった(連続承認キューの次のダイアログ等)ら「応答済み」を下ろす。
        let sig = if present {
            Some(prompt_signature(&text))
        } else {
            None
        };
        if self.answered_sig.is_some() && self.answered_sig != sig {
            self.answered_sig = None;
            self.auto_stall_since = None;
        }
        if !present {
            // 応答前から回している停滞監視 (答えを出せない承認待ち) も、
            // プロンプトが画面から消えたら畳む。
            self.auto_stall_since = None;
        }
        let waiting = present && self.answered_sig.is_none();
        let newly = waiting && !self.attention;
        self.attention = waiting;
        if newly {
            self.attention_since = Some(Instant::now());
        } else if !waiting {
            self.attention_since = None;
        }
        if auto_yes && waiting {
            if let Some((bytes, desc)) = reply {
                // 同じプロンプトへは一度だけ送る。画面に残っていても再送しない
                // (再送は Claude 側の入力欄への Enter/y 連打事故になる)。
                // 指紋が変わって別のプロンプトが来たときだけ、また一度応答する。
                self.answered_sig = sig;
                self.auto_stall_since = Some(Instant::now());
                self.auto_stall_hash = self.cur_hash;
                self.write_bytes(bytes);
                self.attention = false;
                return Some(Attention::AutoReplied(desc));
            }
            // 自動YESが答えを持たない承認待ち (番号入力メニューなど)。
            // ここでは**何も送らず**、停滞監視だけを始める。数字を打つのは
            // 下のウォッチドッグが「画面が 30 秒動かない」と確認した後だけ。
            if self.auto_stall_since.is_none() {
                self.auto_stall_since = Some(Instant::now());
                self.auto_stall_hash = self.cur_hash;
            }
        }
        // 自動YESの停滞ウォッチドッグ。対象は 2 通り:
        //   ① 自動応答したのに同じプロンプトのまま画面が 30 秒まったく
        //      変化しない (= 応答が取りこぼされた)。
        //   ② そもそも自動YESが答えを出せず、承認待ちのまま 30 秒動かない
        //      (= 番号入力の画面で止まっている)。
        // どちらもペットの「✔ 承認」ボタンと同じ操作へ切り替える。②では
        // それが `numbered_menu_reply` の数字 + Enter になる (通常スキャンでは
        // 数字を打たないので、ここが唯一の入口 = 連打が構造的に起こらない)。
        // 出力が流れている間 (cur_hash が動く間) は「進んでいる」ので送らない —
        // 応答済みプロンプトが画面に残っているだけの状態への連打事故を防ぐ。
        if auto_yes && present && (self.answered_sig == sig || self.answered_sig.is_none()) {
            if let Some(since) = self.auto_stall_since {
                if self.cur_hash != self.auto_stall_hash {
                    self.auto_stall_hash = self.cur_hash;
                    self.auto_stall_since = Some(Instant::now());
                } else if since.elapsed() >= self.auto_yes_resend_after {
                    // 押せても押せなくても次の判定は 30 秒後から。
                    // (分類できない画面へ毎スキャン試し続けないため)
                    self.auto_stall_since = Some(Instant::now());
                    // 送るのが番号メニューの数字なら「何番を選んだか」を
                    // そのまま説明に出す。勝手に答えた事実を隠さないための
                    // 約束 (MENU_DESC_* の文面)。それ以外は従来の停滞文言。
                    let menu_keys = numbered_menu_reply(&text).map(|(b, _)| b);
                    let menu_desc = stalled_reply_for(&text, self.agent_bin())
                        .filter(|(b, _)| menu_keys == Some(b))
                        .map(|(_, d)| d);
                    if self.press_pet_approve_button(None) {
                        return Some(Attention::AutoReplied(
                            menu_desc.unwrap_or("自動YES停滞のためペットの承認ボタンを自動押下"),
                        ));
                    }
                }
            }
        }
        if newly {
            return Some(Attention::NeedsApproval);
        }
        // レート制限の新規検知。他に返すイベントが無いときだけ確定させる。
        if self.rate_limited.is_none() {
            if let Some(line) = rl_detect {
                self.rate_limited = Some(line.clone());
                self.rl_miss = 0;
                self.rl_hits = 1;
                return Some(Attention::RateLimited(line));
            }
        }
        None
    }

    /// 上限警告が**同じまま**続けて見えている回数。
    /// 画面由来の判定を信じてよいかの裏取り (`failover::confirm_screen`) に使う。
    pub fn rate_limit_hits(&self) -> u8 {
        self.rl_hits
    }

    /// 直近のスキャンからこのスキャンまでに、意味的な画面内容が動いたか。
    ///
    /// スピナー・経過秒・カウンタは `semantic_hash` が潰しているので、
    /// true = **本当に出力が進んでいる**。「止まっているから異常」の裏取りに使う
    /// (画面テキストの部分一致だけで異常と決めないための材料)。
    pub fn output_advanced(&self) -> bool {
        self.cur_hash != self.prev_hash
    }

    /// このセッションへ投げた「プロンプト」の本文を覚える。
    ///
    /// フェイルオーバーで別プロファイルのセッションへ引き継ぐための材料。
    /// キーストローク・承認キー (`\r` や `\u{1b}` 単体) は覚えない。
    pub fn note_prompt(&mut self, text: &str) {
        let t = text
            .trim_matches(|c: char| c.is_control() || c.is_whitespace())
            .trim();
        if t.chars().count() < PROMPT_MIN_CHARS {
            return;
        }
        self.last_prompt = Some(t.chars().take(PROMPT_KEEP_CHARS).collect());
    }

    /// **端末へ直接打ち込んだ打鍵を 1 行へ組み立て直す。**
    ///
    /// `write_bytes` へ流す直前のバイト列を毎回ここへ通す。Enter (CR/LF) が
    /// 来た時点で、それまでに打った本文を `note_prompt` へ渡す。
    /// これが無いと「ターミナルに直接タイプした指示」がどこにも残らず、
    /// 自動命名も失敗切替の引き継ぎも材料ゼロで走ることになる。
    pub fn note_typed_bytes(&mut self, bytes: &[u8]) {
        if let Some(line) = feed_typed_line(&mut self.typed_line, bytes) {
            self.note_prompt(&line);
        }
    }

    /// 未読か。「最後に見た時点から意味的な画面内容が変わった」または手動ピン。
    pub fn has_unread(&self) -> bool {
        self.pinned_unread || self.cur_hash != self.seen_hash
    }

    /// 表示中のセッションを既読へ。毎フレーム呼んで良い。
    /// 手動の「あとで見る」ピンはここでは外さない (見続けている間に消えると
    /// ピンの意味が無くなるため。外すのは acknowledge)。
    pub fn mark_read(&mut self) {
        self.seen_hash = self.cur_hash;
    }

    /// ユーザーが明示的にこのセッションへフォーカスした / 既読にした。ピンも外す。
    pub fn acknowledge(&mut self) {
        self.seen_hash = self.cur_hash;
        self.pinned_unread = false;
    }

    /// 「あとで見る」ピンを立てる (次に acknowledge するまで未読のまま)。
    pub fn mark_unread(&mut self) {
        self.pinned_unread = true;
    }

    pub fn running(&self) -> bool {
        !self.exited.load(Ordering::SeqCst)
    }

    /// PTY へ入力を送る。待ち行列へ積むだけなので**ここでは待たされない**。
    ///
    /// 直接書くと、子が標準入力を読まなくなった (固まった / 落ちかけている)
    /// ときにパイプが詰まって呼び出し側ごと止まる。呼び出し側は UI スレッドなので
    /// アプリ全体が固まる ([`PtyWriter`] の説明を参照)。
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.writer.send(bytes);
    }

    /// 端末の隅に出すバッジ用: (段, 直近の終了コード)。
    ///
    /// 段が `None` (シェル統合が来ていない) なら `None` — 呼び出し側は
    /// 1 ピクセルも描かない。毎フレーム通る道なので、ロック 1 回・
    /// アロケーション 0 で済ませる (設計原則 3)。
    pub fn shell_badge(&self) -> Option<(crate::shellint::Tier, Option<i32>)> {
        let t = lock_ok(&self.shell);
        (t.tier() != crate::shellint::Tier::None).then(|| (t.tier(), t.last_exit()))
    }

    /// メニュー見出し用のまとめ: (段, 記録件数, 段の変化ログ)。
    ///
    /// 段の変化を出せるようにしておくのは「黙って劣化しない」ための約束
    /// (CLAUDE.md 設計原則 4)。ロックは 1 回で済ませる。
    pub fn shell_status(&self) -> (crate::shellint::Tier, usize, Option<String>) {
        let t = lock_ok(&self.shell);
        (t.tier(), t.recorded(), t.tier_log_text())
    }

    /// **状態ラダーへの供給** — シェルが直接教えてきた判定。
    ///
    /// 画面を 1 文字も読んでいないので、`error` の 3 文字が並んでいるだけで
    /// 「異常」に落ちることが構造的に起こらない。何も来ていなければ `None`
    /// (呼び出し側は従来どおり下の段へ降りる = 導入前と同じ挙動)。
    pub fn shell_read(&self) -> Option<crate::supervisor::protocol::ProtoRead> {
        lock_ok(&self.shell).read_now()
    }

    /// UI へ渡す直近コマンドの写し (新しい順、最大 `n` 件)。
    ///
    /// 参照ではなく値で返すのは、描画中にロックを握り続けないため
    /// (読取スレッドを待たせない)。
    pub fn shell_recent(&self, n: usize) -> Vec<crate::shellint::Command> {
        lock_ok(&self.shell)
            .recent(n)
            .into_iter()
            .cloned()
            .collect()
    }

    /// 上限超えで捨てた件数のギャップ標識 (捨てていなければ `None`)。
    pub fn shell_gap_note(&self) -> Option<String> {
        lock_ok(&self.shell).gap_note()
    }

    /// いま実行中のコマンド行 (シェル統合が無い / 実行中でないなら `None`)。
    pub fn shell_running_command(&self) -> Option<String> {
        lock_ok(&self.shell).running_command().map(str::to_string)
    }

    // ── シェル統合の描画のための問い合わせ (行番号 ↔ 画面行) ──────────

    /// **いま実際に効いている戻り量。**
    ///
    /// `self.scroll` ではなく vt100 に聞く。履歴を戻して見ている最中に出力が
    /// 来ると、vt100 は同じ文字が同じ位置に見えるよう**自分で戻り量を増やす**
    /// が、`self.scroll` は `set_scroll` を通ったときしか更新されない。
    /// 描画 (`draw_screen`) はパーサの値で描くので、こちらに合わせないと
    /// 印と帯だけが本文からずれる。
    fn eff_scroll(&self) -> usize {
        lock_ok(&self.parser).screen().scrollback()
    }

    /// 画面行 `r` (0 = 最上段) の**通し番号**。
    ///
    /// 履歴を戻して見ている間はその戻り量ぶん古い行が見えているので引く。
    /// **入れるときと問い合わせるときで同じ番号体系**、という `shellint` の
    /// 唯一の要求はここで守られる。
    pub fn line_of_row(&self, r: u16) -> u64 {
        self.line_of_row_at(self.eff_scroll(), r)
    }

    /// 戻り量を渡す版 (1 回のロックで複数行を引くため)。
    fn line_of_row_at(&self, scroll: usize, r: u16) -> u64 {
        self.lines
            .scrolled()
            .saturating_sub(scroll as u64)
            .saturating_add(u64::from(r))
    }

    /// 通し番号を今の画面行へ戻す。画面の外なら `None`。
    fn row_of_line_at(&self, scroll: usize, line: u64) -> Option<u16> {
        let top = self.lines.scrolled().saturating_sub(scroll as u64);
        let d = line.checked_sub(top)?;
        (d < u64::from(self.size.0)).then_some(d as u16)
    }

    /// 通し番号を [`abs_row`] と同じ**下端起点の絶対行**へ写す。
    /// 選択のコピー経路 (`selection_text_abs`) がこの座標系を使う。
    fn abs_of_line(&self, line: u64) -> usize {
        let bottom = self
            .lines
            .scrolled()
            .saturating_add(u64::from(self.size.0).saturating_sub(1));
        bottom.saturating_sub(line) as usize
    }

    /// 履歴に残っている**最も古い**絶対行。これより大きい絶対行はもう存在しない
    /// (`selection_text_abs` が切り詰める上限と同じ値)。
    fn oldest_abs(&self) -> usize {
        self.abs_of_line(self.lines.oldest_live())
    }

    /// `line` を画面の最上段へ持ってくるための戻り量。
    fn scroll_for_line(&self, line: u64) -> usize {
        self.lines.scrolled().saturating_sub(line) as usize
    }

    /// **前/次のプロンプトへ跳ぶ。** 跳んだら `true`。
    ///
    /// 基準は画面の最上段なので、続けて押すと 1 つずつ移動する。
    /// シェル統合が来ていない端末では候補が 1 つも無いので**常に `false`**
    /// — 押しても何も起きない (嘘の移動をしない)。
    pub fn shell_jump_prompt(&mut self, forward: bool) -> bool {
        let cur = self.line_of_row(0);
        let target = {
            let t = lock_ok(&self.shell);
            if forward {
                t.next_prompt(cur)
            } else {
                t.prev_prompt(cur)
            }
        };
        let Some(line) = target else {
            return false;
        };
        self.set_scroll(self.scroll_for_line(line));
        true
    }

    /// 上端に固定表示する「いま見えている出力はどのコマンドのものか」。
    ///
    /// コマンド行そのものが画面に見えているときは `None` — 同じものを
    /// 二重に出さない (VS Code の sticky scroll と同じ条件)。該当が
    /// 無ければ `None` で、呼び出し側は**帯ごと描かない** (空白を作らない)。
    pub fn shell_sticky(&self) -> Option<ShellSticky> {
        let top = self.line_of_row(0);
        let t = lock_ok(&self.shell);
        let b = t.block_at(top)?;
        let l = b.lines?;
        // コマンド行が見えている = 帯は要らない。
        if l.command_row() >= top {
            return None;
        }
        let text = crate::shellint::one_line(&b.cmd.command_line, crate::shellint::SUMMARY_CHARS);
        if text.is_empty() {
            return None;
        }
        Some(ShellSticky {
            text,
            ok: b.ok(),
            prompt: l.prompt,
            running: l.end.is_none(),
        })
    }

    /// いま画面に見えているコマンドの印 (VS Code の command decoration)。
    ///
    /// シェル統合が来ていなければ**空** = 1 ピクセルも描かない。
    /// 行を知らないブロックは黙って飛ばす (「たぶんここ」で印を置かない)。
    pub fn shell_marks(&self) -> Vec<ShellMark> {
        let rows = self.size.0;
        if rows == 0 {
            return Vec::new();
        }
        let scroll = self.eff_scroll();
        let top = self.line_of_row_at(scroll, 0);
        let bottom = self.line_of_row_at(scroll, rows - 1);
        let t = lock_ok(&self.shell);
        let mut out: Vec<ShellMark> = Vec::new();
        let mut push = |b: &crate::shellint::CommandBlock| {
            let Some(l) = b.lines else {
                return;
            };
            let row = l.command_row();
            if row < top || row > bottom {
                return;
            }
            let Some(r) = self.row_of_line_at(scroll, row) else {
                return;
            };
            out.push(ShellMark {
                row: r,
                ok: b.ok(),
                command: b.cmd.command_line.clone(),
                summary: b.cmd.summary(),
                output: l.output_start.map(|s| (s, l.end.unwrap_or(bottom))),
                running: l.end.is_none(),
            });
        };
        if let Some(b) = t.running_block() {
            push(b);
        }
        // ブロックは prompt の昇順。新しいほうから見て、窓より古くなったら降りる。
        for b in t.blocks().iter().rev() {
            match b.lines.map(|l| l.command_row()) {
                Some(row) if row < top => break,
                _ => push(b),
            }
        }
        out
    }

    /// 通し番号の行を画面の最上段へ持ってくる。
    pub fn shell_scroll_to_line(&mut self, line: u64) {
        self.set_scroll(self.scroll_for_line(line));
    }

    /// ブロックの出力を丸ごと文字列にする (履歴をまたいで拾う)。
    ///
    /// 範囲は `output_start ..= end` の**通し番号**。まだ画面に残っている
    /// ぶんだけが取れる (落ちた行は `selection_text_abs` が切り詰める)。
    pub fn shell_block_output(&self, range: (u64, u64)) -> String {
        let (a, b) = (self.abs_of_line(range.0), self.abs_of_line(range.1));
        let mut p = lock_ok(&self.parser);
        selection_text_abs(&mut p, ((a, 0), (b, u16::MAX)))
    }

    /// アプリが DECSCUSR で指定した現在のカーソル形状。
    ///
    /// Neovim / Helix は挿入モードで縦バーへ切り替える。追従しないと
    /// 「ずっとブロックのままで壊れて見える」ため描画側でこれを見る。
    pub fn cursor_shape(&self) -> CursorShape {
        CursorShape::from_u8(self.cursor_shape.load(Ordering::Relaxed))
    }

    /// ウィンドウのフォーカス状態を伝える。
    ///
    /// アプリが CSI ?1004h を出しているときだけ ESC[I / ESC[O を送る。
    /// Neovim の FocusGained/FocusLost や lazygit の自動更新がこれを見ている。
    /// 呼び出し側 (app.rs) が `ctx.input(|i| i.viewport().focused)` を毎フレーム
    /// 渡す想定。状態が変わらない限り送らないので毎フレーム呼んでよい。
    #[allow(dead_code)] // TODO(app.rs 連携): 毎フレーム呼び出しを繋ぐまで未使用
    pub fn set_focus(&mut self, focused: bool) {
        if !self.focus_reports.load(Ordering::Relaxed) || !self.running() {
            return;
        }
        if self.focus_sent == Some(focused) {
            return;
        }
        self.focus_sent = Some(focused);
        self.write_bytes(if focused { b"\x1b[I" } else { b"\x1b[O" });
    }

    /// OSC 52 でアプリが要求したクリップボード内容を取り出す(取り出したら消える)。
    ///
    /// Neovim / Helix の「システムクリップボードへヤンク」がこれで届く。
    /// 呼び出し側が `ui.output_mut(|o| o.copied_text = s)` 等へ流す想定。
    #[allow(dead_code)] // TODO(app.rs 連携): egui のクリップボードへ流すまで未使用
    pub fn take_clipboard(&mut self) -> Option<String> {
        lock_ok(&self.clipboard_pending).take()
    }

    /// OSC 10/11 の色問い合わせに返す前景/背景色を設定する。
    /// Neovim はこれで背景の明暗を判定し 'background' を決める。
    #[allow(dead_code)] // TODO(app.rs 連携): テーマ色を渡すまで未使用
    pub fn set_report_colors(&self, fg: egui::Color32, bg: egui::Color32) {
        let pack = |c: egui::Color32| ((c.r() as u32) << 16) | ((c.g() as u32) << 8) | c.b() as u32;
        self.report_fg.store(pack(fg), Ordering::Relaxed);
        self.report_bg.store(pack(bg), Ordering::Relaxed);
    }

    /// 前回聞いてから人が手で打ったか。読んだ時点で印は下ろす。
    /// 音声入力が「書き込み済みの文字列」の追跡を捨てるかどうかの判断に使う。
    pub fn take_user_typed(&mut self) -> bool {
        std::mem::take(&mut self.user_typed)
    }

    /// ユーザー自身の入力(キーボード・IME・ペースト・リモート端末キー・
    /// ブロードキャスト等)がこのセッションへ入る直前に呼ぶ。
    /// `user_typed` の印に加えて、いま画面に出ている承認プロンプトの
    /// エピソードを「ユーザーが自分で応答した」として解決する。
    ///
    /// これが無いと、自動YESオフの手動運転では `answered_sig` が立つ経路が
    /// バブルのボタンしか無い。プロンプト風テキスト(引用の "(y/n)" や
    /// 「Do you want …?」の残り)が画面に見えている限り attention が
    /// 立ちっぱなしになり、バブル/トーストの再出現に加えて coordinator が
    /// WaitingApproval(注入禁止)のまま配達を保留し続け、エージェント間の
    /// やり取りが止まって見える。
    pub fn note_user_input(&mut self) {
        self.user_typed = true;
        self.resolve_attention();
    }

    /// アプリ (CLI エージェント) が bracketed paste を有効にしているか。
    ///
    /// 有効なら複数行の指示を `ESC[200~ … ESC[201~` で包める
    /// ([`crate::submit::body_bytes`])。包まないと本文中の改行がその場で
    /// 確定として扱われ、**指示が途中で分割送信される**。
    pub fn bracketed_paste(&self) -> bool {
        lock_ok(&self.parser).screen().bracketed_paste()
    }

    /// いま CLI の入力欄に見えている本文 (拾えなければ None)。
    ///
    /// 「送ったのに実行されず入力欄で待機している」を検出して確定キーを
    /// 撃ち直すための材料 ([`crate::submit::still_pending`])。
    /// 画面から**エージェントの状態を推測するためではない** —
    /// 「自分が書いた文字列がまだそこにあるか」だけを見る。
    pub fn input_text(&self) -> Option<String> {
        input_area_selection(lock_ok(&self.parser).screen()).map(|(_, t)| t)
    }

    /// 文字列をそのままPTYへ書き込む(プログラム的な入力送信)。成功で true。
    ///
    /// キーボード入力と同じ write_bytes 経路を使うため、ターミナルウィジェットに
    /// フォーカスが無くても子プロセスへ届く(ペットバブル等からの Allow/Deny 応答用)。
    pub fn send_text(&mut self, s: &str) -> bool {
        if !self.running() {
            return false;
        }
        self.write_bytes(s.as_bytes());
        true
    }

    /// 承認待ちフラグを解除する(バブルの承認/拒否や見張りの自動応答の後に呼ぶ)。
    ///
    /// いま画面に出ているプロンプトの指紋を「応答済み」として記録するので、
    /// 同じプロンプトが画面に残っていても再検出せず、バブルが何度も出ない。
    /// プロンプトが消える・別のプロンプトに変わると、また検出対象へ戻る。
    pub fn resolve_attention(&mut self) {
        self.attention = false;
        self.attention_since = None;
        let text = lock_ok(&self.parser).screen().contents();
        self.answered_sig = Some(prompt_signature(&text));
        // 手動 (バブル/手入力) で解決したエピソードは停滞ウォッチドッグの対象外。
        // ユーザーが自分の意思で操作している最中に勝手な再送をしない。
        self.auto_stall_since = None;
    }

    /// バブルの「✔ 承認」で送るキー列を、いま画面に出ているプロンプトから決める。
    ///
    /// auto_yes_reply と同じ分類を再利用する。Bypass 警告のようにデフォルト選択が
    /// 「1. No, exit」のプロンプトへ Enter を送るとセッションが終了してしまうため、
    /// 番号キー「2」などプロンプトに合った承認キーを返す。分類不能なら None。
    ///
    /// ここは [`stalled_reply_for`] 側 (= 番号入力メニューにも答える版) を使う。
    /// 呼び出し元は「人が✔を押した」か「停滞ウォッチドッグが 30 秒待った」かの
    /// どちらかで、どちらも 1 回の明示的な操作に対応するため。
    pub fn approve_reply(&self) -> Option<&'static str> {
        let text = lock_ok(&self.parser).screen().contents();
        let (bytes, _) = stalled_reply_for(&text, self.agent_bin())?;
        std::str::from_utf8(bytes).ok()
    }

    /// ペットの「✔ 承認」ボタンと同じ承認操作を実行する。
    ///
    /// 画面に合う承認キーを優先し、分類不能時だけ `fallback` を使う。
    /// 送信成功時は同じプロンプトを解決済みにし、以後の再送を止める。
    pub fn press_pet_approve_button(&mut self, fallback: Option<&str>) -> bool {
        let keys = self
            .approve_reply()
            .map(str::to_owned)
            .or_else(|| fallback.map(str::to_owned));
        let Some(keys) = keys else {
            return false;
        };
        if !self.send_text(&keys) {
            return false;
        }
        self.resolve_attention();
        true
    }

    /// 毎フレーム呼んでよいリサイズ。vt100 (描画グリッド) は**即時**に合わせ、
    /// PTY (ConPTY) への通知だけをフレーム安定 + ワーカー経由に逃がす。
    ///
    /// Windows の `ResizePseudoConsole` は conhost への同期 RPC で、Cockpit の
    /// タイル増減中に毎フレーム × セッション数だけ UI スレッドから撃つと、
    /// conhost が 1 個詰まっただけで画面ごと固まる。ここは絶対に待たない。
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows < 3 || cols < 20 {
            return;
        }
        // 描画側は即時。ここが 1 フレームでも遅れると、描いた矩形と
        // グリッドがずれて「画面が崩れる」ため、遅延側には含めない。
        if self.size != (rows, cols) {
            self.size = (rows, cols);
            {
                // 縮小は**行を履歴へ押し出す** (vendor/vt100 の set_size パッチ)。
                // 数えないと通し番号と画面行の対応が縮めた行数ぶんずれる。
                let mut p = lock_ok(&self.parser);
                let (_, moved, sb_len) = count_around(&mut p, |p| p.set_size(rows, cols));
                self.lines.advance(moved, sb_len);
            }
            // 行数が変われば絶対行 → 画面行の写像も変わる。派生値を取り直す。
            self.sync_selection();
        }
        if let Some((r, c)) = self.resize_debounce.on_request((rows, cols)) {
            self.ship_resize(r, c);
        }
    }

    /// 安定したサイズをワーカーへ渡す (無ければ遅延起動)。待たない。
    fn ship_resize(&mut self, rows: u16, cols: u16) {
        // ワーカーを一度も起こせなかった環境では毎回 spawn を試さない
        // (失敗するたびにスレッド生成のコストを払い、UI が引っかかる)。
        if self.resizer.is_none() && !self.resizer_spawn_failed {
            let weak: Weak<Mutex<Box<dyn MasterPty + Send>>> = Arc::downgrade(&self.master);
            self.resizer =
                ResizeCoalescer::start(format!("zv-pty-resize-{}", self.id), move |rows, cols| {
                    // セッションが drop 済みなら master は消えている → 撃たずに畳む。
                    let Some(master) = weak.upgrade() else {
                        return false;
                    };
                    let _ = lock_ok(&master).resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                    true
                });
            self.resizer_spawn_failed = self.resizer.is_none();
        }
        match &self.resizer {
            Some(r) => r.request(rows, cols),
            // ワーカーを起こせない環境 (スレッド枯渇など) の代替経路。
            // ここでも **待たない**: master のロックが取れなければ次フレームへ回す。
            // 待つと UI スレッドが ConPTY の同期 RPC (ResizePseudoConsole) に
            // 巻き込まれ、Windows で「ファイルを開いた/ウィンドウを閉じた瞬間に
            // 黒いまま固まる」事故になる。
            None => match crate::lockx::try_lock_ok(&self.master) {
                Some(m) => {
                    let _ = m.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                // 取れなかった = 誰かが RPC 中。送信済みフラグを下ろして
                // 次フレームに再送させる (サイズを取りこぼさない)。
                None => self.resize_debounce.retry(),
            },
        }
    }

    /// まだ PTY へ送っていないリサイズ要求が残っているか。
    /// draw はこれが立っている間 request_repaint し、安定カウントを完走させる。
    pub fn resize_pending(&self) -> bool {
        self.resize_debounce.pending()
    }

    /// UI スレッドが毎フレーム最初に呼ぶ「パーサの健康診断」。
    ///
    /// 読み取りスレッドが panic すると parser の Mutex は poison する。
    /// そのまま `lock_ok` で読み続けると、壊れかけのグリッドを描き続けるか、
    /// 描画側が二次パニックして**画面が真っ黒のまま**になる。
    /// ここで一度だけ作り直し、失われた履歴はバナー 1 行で必ず告知する
    /// (黙って捨てるとユーザーには「勝手に履歴が消えた」としか見えない)。
    pub fn ensure_parser_healthy(&mut self) {
        let (rows, cols) = self.size;
        let (mut p, rebuilt) = crate::lockx::lock_rebuilding(&self.parser, |p| {
            *p = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
        });
        if !rebuilt {
            return;
        }
        // 作り直した画面の先頭にお知らせを流す。端末の中身として残るので
        // 1 フレームで消えず、スクロールしても追える。
        p.process(PARSER_REBUILT_BANNER.as_bytes());
        drop(p);
        self.parser_rebuilt_notice = Some(tr(
            "端末の描画状態を作り直しました(これより前の履歴は失われています)",
        ));
    }

    /// PTY が現在認識しているサイズ (テストの突き合わせ用)。
    #[cfg(test)]
    fn pty_size(&self) -> Option<(u16, u16)> {
        lock_ok(&self.master)
            .get_size()
            .ok()
            .map(|s| (s.rows, s.cols))
    }

    /// エージェントを終了させる。孫まで落とすが**待たない**ので、
    /// UI スレッドから呼んでよい。セッション自体は一覧に残る。
    ///
    /// 順序が肝: 先に `killer.kill()` で根 (シェル) を落とすと、
    /// `taskkill /T` が根を見つけられず木を辿れなくなり、**孫だけが取り残される**。
    /// 木を落としてから、取りこぼしの保険として根を撃つ。
    pub fn kill(&mut self) {
        // 終了済みなら何もしない。wait 済みの child_pid は OS に返却されており、
        // 無関係なプロセス (グループ) に再利用され得る — そこへ kill -KILL /
        // taskkill /T /F を撃つとユーザーの別ジョブを巻き添えにする。
        if self.exited.load(Ordering::SeqCst) {
            return;
        }
        let pid = self.child_pid;
        let mut killer = self.killer.clone_killer();
        std::thread::spawn(move || {
            kill_tree_blocking(pid);
            let _ = killer.kill();
        });
    }

    /// **出力で画面が流れたぶんを取り込む。**
    ///
    /// 履歴を遡って見ている最中に出力が来ると、vt100 は同じ文字が同じ位置に
    /// 見えるよう**自分の戻り量を増やす** ([`count_around`] が復元する)。
    /// `self.scroll` は `set_scroll` を通ったときしか動かないので、放置すると
    ///
    /// 1. 次のスクロールが古い基準からやり直して**画面が飛ぶ**
    /// 2. [`abs_row`] の絶対行は下端起点なので、`sel_abs` が**別の文字**を指す。
    ///    しかも表示は `self.scroll` も同じだけ古いおかげで偶然一致するため、
    ///    **コピーだけが静かにずれる**(スクロールした瞬間に表示も露見する)
    ///
    /// 進める量は戻り量の伸びではなく**押し出した総数**([`LineIndex`])の伸び。
    /// 履歴が容量に達すると戻り量は頭打ちになるのに、行は流れ続けるため。
    /// 押し出されて履歴から消えた行はもう追えないので、最古の行で止め、
    /// 選択が丸ごと落ちたら解除する — **黙って別の場所を指さない。**
    fn adopt_scrolled_output(&mut self) {
        let eff = self.eff_scroll();
        // 減る方向 (代替画面へ入ると履歴が無くなる) は写さない。戻ってきた
        // ときに元の位置を復元できなくなるため。
        let mut changed = eff > self.scroll;
        if changed {
            self.scroll = eff;
        }
        let pushed = self.lines.scrolled();
        let d = usize::try_from(pushed.saturating_sub(self.sel_pushed)).unwrap_or(usize::MAX);
        self.sel_pushed = pushed;
        if d > 0 && (self.sel_abs.is_some() || self.sel_anchor_abs.is_some()) {
            changed = true;
            let oldest = self.oldest_abs();
            if let Some(a) = self.sel_anchor_abs.as_mut() {
                a.0 = a.0.saturating_add(d).min(oldest);
            }
            if let Some((mut a, mut b)) = self.sel_abs {
                a.0 = a.0.saturating_add(d);
                b.0 = b.0.saturating_add(d);
                if a.0 > oldest && b.0 > oldest {
                    // 選択していた行は丸ごと履歴の外へ出た。
                    self.sel_abs = None;
                } else {
                    // はみ出した端 (絶対行が大きい = 古い) は最古の行の先頭で止める
                    // (`selection_text_abs` の切り詰めと同じ扱い)。
                    if a.0 > oldest {
                        a = (oldest, 0);
                    }
                    if b.0 > oldest {
                        b = (oldest, 0);
                    }
                    self.sel_abs = Some((a, b));
                }
            }
        }
        if changed {
            self.sync_selection();
        }
    }

    pub fn set_scroll(&mut self, n: usize) {
        // 先に「出力で流れたぶん」を取り込む。取り込まないと選択が置き去りになる。
        self.adopt_scrolled_output();
        // vt100 は履歴より深い戻りを黙って切り詰めるので、実際に効いた量を持つ。
        // 持たないと「見た目は止まっているのに scroll だけ増える」ので戻すとき空回りする。
        let eff = {
            let mut p = lock_ok(&self.parser);
            p.set_scrollback(n);
            p.screen().scrollback()
        };
        self.scroll = eff;
        // 選択は絶対座標なので消さない。今の画面へ切り取り直すだけ。
        self.sync_selection();
    }

    /// 相対スクロール。**実際に動いたら true** (自動スクロールの再描画要求用)。
    pub fn adjust_scroll(&mut self, delta: i64) -> bool {
        // 相対量の基準は**いま実際に効いている戻り量**。取り込まずに足すと、
        // 出力が来ていた場合その行数ぶん画面が飛ぶ。
        self.adopt_scrolled_output();
        let before = self.scroll;
        let n = (self.scroll as i64 + delta).max(0) as usize;
        self.set_scroll(n);
        self.scroll != before
    }

    /// 絶対座標の選択を、いま見えている画面の座標へ切り取り直す。
    fn sync_selection(&mut self) {
        let rows = self.size.0;
        let scroll = self.scroll;
        self.selection = self
            .sel_abs
            .and_then(|sel| clip_selection(sel, rows, scroll));
    }

    /// 画面座標で選択を設定する (今の scroll を基準に絶対座標へ写す)。
    pub fn set_selection(&mut self, sel: ((u16, u16), (u16, u16))) {
        // 画面座標 → 絶対座標の基準を最新にしてから写す。
        self.adopt_scrolled_output();
        let (rows, scroll) = (self.size.0, self.scroll);
        let a = (abs_row(sel.0 .0, rows, scroll), sel.0 .1);
        let b = (abs_row(sel.1 .0, rows, scroll), sel.1 .1);
        self.set_selection_abs(a, b);
    }

    /// 絶対座標で選択を設定する。
    pub fn set_selection_abs(&mut self, a: (usize, u16), b: (usize, u16)) {
        self.sel_abs = Some((a, b));
        // 以後のずれはここからの差分で数える。
        self.sel_pushed = self.lines.scrolled();
        self.sync_selection();
    }

    /// **選択範囲の文字列** (コピーの中身そのもの)。選択が無ければ空。
    ///
    /// 真実源の絶対座標で取るので、見えている 1 画面には切り詰めない。
    pub(crate) fn selection_string(&mut self) -> String {
        // コピーは絶対行でパーサを直に読む。取り込まないとここだけずれる。
        self.adopt_scrolled_output();
        let Some(sel) = self.sel_abs else {
            return String::new();
        };
        let mut p = lock_ok(&self.parser);
        selection_text_abs(&mut p, sel)
    }

    /// 読取スレッドと**同じ手順**でバイト列を取り込む (テスト用)。
    /// 押し出した行数の数え方まで本番と同じ経路を通すので、
    /// 「出力が来たときのずれ」を PTY 無しで再現できる。
    #[cfg(test)]
    fn feed(&self, bytes: &[u8]) {
        let mut p = lock_ok(&self.parser);
        for piece in bytes.chunks(FEED_CHUNK) {
            let (_, moved, sb_len) = count_around(&mut p, |p| p.process(piece));
            self.lines.advance(moved, sb_len);
        }
    }

    /// 選択とドラッグアンカーを両方捨てる。
    pub fn clear_selection(&mut self) {
        self.sel_abs = None;
        self.sel_anchor_abs = None;
        self.selection = None;
    }

    /// 端末の中身**全部** (スクロールバック履歴 + 現在画面) を選択する
    /// (Ctrl+A / Cmd+A)。可視画面だけではない。
    pub fn select_all(&mut self) {
        let (max_off, rows, cols) = {
            let mut p = lock_ok(&self.parser);
            let saved = p.screen().scrollback();
            p.set_scrollback(usize::MAX);
            let max_off = p.screen().scrollback();
            let (rows, cols) = p.screen().size();
            p.set_scrollback(saved);
            (max_off, rows, cols)
        };
        if rows == 0 || cols == 0 {
            return;
        }
        // 最古の行 = 最大戻り量の窓の最上段。最新の行 = 絶対行 0。
        let top_abs = max_off + rows as usize - 1;
        self.sel_anchor_abs = None;
        self.set_selection_abs((top_abs, 0), (0, cols.saturating_sub(1)));
    }

    /// 端末内検索: 次 (forward=true, 新しい方=下) / 前 (古い方=上) のヒットへ。
    /// スクロールバック全体を検索し、ヒット行が見えるようスクロールする。
    /// ヒットがあれば true。`search` の hit_line / total / index を更新する。
    pub fn search_step(&mut self, forward: bool) -> bool {
        let (lines, rows) = {
            let mut p = lock_ok(&self.parser);
            let rows = p.screen().size().0 as usize;
            (all_terminal_lines(&mut p), rows)
        };
        let hits = line_hits(&lines, &self.search.query);
        self.search.total = hits.len();
        if hits.is_empty() {
            self.search.hit_line = None;
            self.search.index = 0;
            self.search.current_vis = None;
            return false;
        }
        let pos = match (self.search.hit_line, forward) {
            // 初回は一番新しいヒットから (端末は下から上へ探すのが自然)
            (None, _) => hits.len() - 1,
            (Some(cur), true) => hits.iter().position(|&h| h > cur).unwrap_or(0),
            (Some(cur), false) => hits
                .iter()
                .rposition(|&h| h < cur)
                .unwrap_or(hits.len() - 1),
        };
        let hit = hits[pos];
        self.search.hit_line = Some(hit);
        self.search.index = pos + 1;
        let target = search_scroll_target(hit, lines.len(), rows);
        self.set_scroll(target);
        // 現在ヒットの可視行を覚える (強調表示用)。scroll が変わるまで有効。
        let window_start = lines.len().saturating_sub(rows).saturating_sub(target);
        let vis = hit.saturating_sub(window_start);
        self.search.current_vis =
            (vis < rows).then(|| (target, u16::try_from(vis).unwrap_or(u16::MAX)));
        true
    }

    /// (代替画面か, アプリがマウス報告を有効にしているか, SGRエンコードか)。
    /// 代替画面(vim / less / Claude Code 等)にはスクロールバック履歴が無いため、
    /// ローカルスクロールではなくホイールをアプリへ転送する必要がある。
    pub fn wheel_modes(&self) -> (bool, bool, bool) {
        let p = lock_ok(&self.parser);
        let s = p.screen();
        let mouse_on = !matches!(s.mouse_protocol_mode(), vt100::MouseProtocolMode::None);
        let sgr = matches!(
            s.mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Sgr
        );
        (s.alternate_screen(), mouse_on, sgr)
    }

    /// マウスホイール1ノッチをアプリへ転送する。col/row は 0-based セル座標。
    pub fn send_wheel(&mut self, up: bool, col: u16, row: u16, sgr: bool) {
        let cb: u16 = if up { 64 } else { 65 };
        let cx = col.saturating_add(1);
        let cy = row.saturating_add(1);
        if sgr {
            // SGR (1006): ESC [ < Cb ; Cx ; Cy M
            let seq = format!("\x1b[<{cb};{cx};{cy}M");
            self.write_bytes(seq.as_bytes());
        } else {
            // X10/1000: ESC [ M (32+Cb) (32+Cx) (32+Cy) — 各バイトは 255 で頭打ち
            let bb = 32u16.saturating_add(cb).min(255) as u8;
            let bx = 32u16.saturating_add(cx).min(255) as u8;
            let by = 32u16.saturating_add(cy).min(255) as u8;
            self.write_bytes(&[0x1b, b'[', b'M', bb, bx, by]);
        }
    }

    pub fn uptime(&self) -> String {
        let s = self.started.elapsed().as_secs();
        if s >= 3600 {
            format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
        } else {
            format!("{}m{:02}s", s / 60, s % 60)
        }
    }

    /// 看板カードのライブプレビュー用: 画面末尾の「内容のある行」を最大 `rows` 行、
    /// 各行 `max` 文字までで返す (上から下へ時系列順)。英数字か仮名漢字を 1 文字も
    /// 含まない行 (罫線・入力枠だけの行) や空行は飛ばす。
    pub fn screen_tail_lines(&self, rows: usize, max: usize) -> Vec<String> {
        let text = lock_ok(&self.parser).screen().contents();
        pick_tail_lines(&text, rows, max)
    }
}

/// [`Session::screen_tail_lines`] の本体 (テスト用に分離)。
/// インデントは表示情報なので行頭の空白は残し、行末だけ落とす。
fn pick_tail_lines(text: &str, rows: usize, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .rev()
        .map(str::trim_end)
        .filter(|l| {
            let t = l.trim_start();
            !t.is_empty() && t.chars().any(char::is_alphanumeric)
        })
        .take(rows)
        .map(|l| truncate_cols(l, max))
        .collect();
    lines.reverse();
    lines
}

/// `max` **桁**を超える行を「…」付きで詰める。
///
/// 文字数ではなく表示桁数で数える: 「日本語の行だけタイルから 2 倍はみ出す」
/// のを防ぐため。全角文字の途中では切らない
/// ([`crate::textenc::truncate_to_width`] が桁数の唯一の出どころ)。
fn truncate_cols(s: &str, max: usize) -> String {
    crate::textenc::truncate_to_width(s, max)
}

/// kill を撃ってよい相手を判定する純関数 (Drop の木殺しが使う)。
///
/// 終了済み (`exited`) なら **None** — wait 済みの child_pid は OS に返却されて
/// おり、無関係なプロセス (グループ) に再利用され得る。そこへ killpg /
/// taskkill /T /F を撃つとユーザーの別ジョブを巻き添えにする
/// ([`Session::kill`] / [`reap`] / [`abandon`] と同じガード)。
fn kill_target(exited: bool, child_pid: Option<u32>) -> Option<u32> {
    if exited {
        None
    } else {
        child_pid
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // リサイズワーカーを先に畳む (通知だけ — join せず待たない)。
        // ワーカーは master の Weak しか持たないので、万一通知を取り逃しても
        // upgrade 失敗で自然に抜け、master より長生きして触ることはない。
        if let Some(r) = self.resizer.take() {
            r.shutdown();
        }
        // [`reap`] / [`abandon`] を通らずに drop される経路 (テストや異常系) の
        // 最後の砦。`killer.kill()` は PTY 直下の子 (シェル) にしか届かず、
        // ログインシェルの子 (`bash -lc '…; sleep N'`) や孫が生き残って
        // プロセスツリーが漏れる (CI ランナーを飢えさせた原因)。
        // グループごと落としてから、保険として根も撃つ。
        let exited = self.exited.load(Ordering::SeqCst);
        if let Some(pid) = kill_target(exited, self.child_pid) {
            crate::procx::kill_tree(pid);
        }
        if !exited {
            // PID が取れなかったセッションでも根だけは落とす。
            let _ = self.killer.kill();
        }
    }
}

/// プロセス**ツリー**を落とすコマンドを組み立てる。
///
/// PTY に直接ぶら下がっているのは `cmd.exe` (Windows) / ログインシェル (unix) で、
/// エージェント本体 (`claude` → node.exe など) はその**孫**。portable-pty の
/// killer は直接の子だけを TerminateProcess するので、孫は生き残り、
/// PTY に繋がったまま走り続ける。
///
/// Windows ではこれが致命的になる。生き残ったクライアントがいる限り
/// `ClosePseudoConsole` (= master の Drop) が返ってこないためで、
/// UI スレッドでセッションを drop するとウィンドウごと固まって戻らない。
/// ([`reap`] の説明も参照)
fn kill_tree_command(pid: u32) -> std::process::Command {
    #[cfg(windows)]
    // /T = 子孫ごと、/F = 強制。PATH は要らない (System32 にある)。
    let mut c = {
        let mut c = crate::procx::hidden_command_raw("taskkill");
        c.args(["/T", "/F", "/PID", &pid.to_string()]);
        c
    };
    #[cfg(not(windows))]
    // portable-pty の unix 実装は子を setsid するので、子はプロセスグループの
    // リーダーになっている。`-PID` でグループごと落とせる。
    //
    // **`--` は必須**。ここを省くと Linux (procps-ng の /usr/bin/kill) では
    // `-1234` が「まとめ書きした短オプション」として getopt に食われ、
    // 残った先頭 1 桁だけが PID として渡る:
    //   `kill -KILL -213` → `kill(-2, SIGKILL)`   (無関係なグループ 2)
    //   `kill -KILL -193` → `kill(-1, SIGKILL)`   (**シグナルを送れる全プロセス**)
    // しかも終了コードは常に 0 なので、呼び出し側は成功したと誤認する。
    // 「Linux ランナーが突然死ぬ」の正体がこれ (pid の先頭が 1 なら全滅)。
    // `--` を挟めば `kill(-1234, SIGKILL)` と正しく解釈され、相手が居なければ
    // 終了コード 1 が返る。macOS / BSD の kill も `--` を受け付けるので共通。
    let mut c = {
        let mut c = crate::procx::hidden_command_raw("kill");
        c.args(["-KILL", "--", &format!("-{pid}")]);
        c
    };
    // 「〜を終了しました」の報告はどこへも出さない。ターミナルから `zai` を
    // 起動していると、閉じるたびにこの出力が混ざる。
    c.stdout(std::process::Stdio::null());
    c.stderr(std::process::Stdio::null());
    c
}

/// ツリーを落として、落ち切るまで待つ。**UI スレッドから呼んではいけない**。
/// 木を辿れた (= 根がまだ生きていた) なら true。
fn kill_tree_blocking(pid: Option<u32>) -> bool {
    let Some(pid) = pid else { return false };
    match kill_tree_command(pid).spawn() {
        Ok(mut child) => child.wait().map(|s| s.success()).unwrap_or(false),
        Err(_) => false,
    }
}

/// セッションを畳む。**閉じる操作はすべてここを通すこと**。
///
/// `Session` を drop すると master (ConPTY) が閉じる。Windows の
/// `ClosePseudoConsole` は「PTY に繋がっているクライアントが全部消えて、
/// 残りの出力を吐き切る」まで戻ってこない。エージェントを走らせたまま
/// UI スレッドで drop すると、`App::update` の途中で止まったきり
/// 画面が更新されなくなる — 「エージェントを2つ以上動かしている最中に
/// 片方を閉じるとアプリが固まって戻らない」の正体がこれ。
///
/// そこでセッションを別スレッドへ持ち出し、**プロセスツリーを落とし切ってから**
/// drop する。ツリーが消えていれば `ClosePseudoConsole` はすぐ返るし、
/// 万一返らなくても止まるのは捨てるスレッドだけで、UI は動き続ける。
pub fn reap(session: Session) {
    std::thread::spawn(move || {
        let mut session = session;
        // 既に終了したセッション (「終了しました」のタブを ✕ で閉じる等) には
        // kill を撃たない。child_pid は wait 済みで OS に返却されており、
        // 長時間稼働中なら**無関係なプロセス (グループ) に再利用され得る** —
        // そこへ kill -KILL / taskkill /T /F を撃つとユーザーの別ジョブを
        // 巻き添えにする。
        if !session.exited.load(Ordering::SeqCst) {
            // 木が先。根を先に落とすと taskkill が木を辿れず孫が残る
            // ([`Session::kill`] の説明を参照)。
            kill_tree_blocking(session.child_pid);
            let _ = session.killer.kill();
        }
        // ここでようやく ConPTY を閉じる (この時点なら待たされない)。
        drop(session);
    });
}

/// アプリ終了時にセッションを手放す。
///
/// [`reap`] と違って別スレッドに預けない — プロセスが消えるとスレッドも道連れに
/// なるため、終了処理の途中で消える可能性のある場所に後始末を残せない。
/// エージェントを落とすコマンドだけ先に**独立したプロセスとして**起こし、
/// PTY のハンドルは OS に回収させる (drop すると `ClosePseudoConsole` で
/// 終了処理そのものが止まり、ウィンドウが閉じないまま残る)。
pub fn abandon(session: Session) {
    let mut session = session;
    // mem::forget は Drop を通らないため、リサイズワーカーへの終了通知だけ
    // ここで出しておく (放置しても Weak 頼みで無害だが、待機のまま残さない)。
    if let Some(r) = session.resizer.take() {
        r.shutdown();
    }
    // 終了済みセッションには撃たない (reap と同じ理由: wait 済みの PID は
    // 無関係なプロセスに再利用され得る)。
    if !session.exited.load(Ordering::SeqCst) {
        // `/T /F` は根ごと落とすので、これ 1 本でよい。待たない — この子プロセスは
        // 自分より長生きしてよいし、根を先に撃つと木を辿れなくなる
        // ([`Session::kill`] の説明を参照)。
        let started = session
            .child_pid
            .is_some_and(|pid| kill_tree_command(pid).spawn().is_ok());
        if !started {
            // taskkill / kill を起こせなかったときの保険。
            let _ = session.killer.kill();
        }
    }
    std::mem::forget(session);
}

#[cfg(test)]
mod tail_tests {
    use super::truncate_cols;

    #[test]
    fn tail_skips_border_and_blank_lines() {
        let screen = "✻ テストを実行中…\n╭──────╮\n│ >    │\n╰──────╯\n\n";
        assert_eq!(
            super::pick_tail_lines(screen, 8, 120),
            vec!["✻ テストを実行中…"]
        );
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate_cols("短い", 10), "短い");
        // 5 **桁**上限 → 全角 2 文字 (4 桁) + 「…」(1 桁)。
        // 文字数で切ると全角 4 文字 = 8 桁になり、枠を倍はみ出していた。
        assert_eq!(truncate_cols("あいうえおかき", 5), "あい…");
        assert_eq!(truncate_cols("ｱｲｳｴｵｶｷ", 5), "ｱｲｳｴ…", "半角カナは 1 桁");
    }

    #[test]
    fn tail_lines_keep_order_and_skip_borders() {
        let screen = "1st line\n╭──────╮\n  fn main() {\n│ >    │\n✻ テスト実行中…\n\n";
        // 罫線・空行は飛ばし、時系列順 (上→下) のまま返す
        assert_eq!(
            super::pick_tail_lines(screen, 8, 120),
            vec!["1st line", "  fn main() {", "✻ テスト実行中…"]
        );
        // rows で末尾側から絞る (古い行が落ちる)
        assert_eq!(
            super::pick_tail_lines(screen, 2, 120),
            vec!["  fn main() {", "✻ テスト実行中…"]
        );
        // 行頭インデントは残し、行末空白と長すぎる行だけ詰める
        assert_eq!(super::pick_tail_lines("  abcdef   \n", 4, 4), vec!["  a…"]);
        assert!(super::pick_tail_lines("╭──╮\n╰──╯", 4, 120).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::{auto_yes_reply, auto_yes_reply_for, stalled_reply_for};

    /// 停滞時の分類 (番号入力メニューにも答える版) のエージェント無し呼び出し。
    fn stalled_reply(text: &str) -> Option<(&'static [u8], &'static str)> {
        stalled_reply_for(text, None)
    }

    #[test]
    fn bypass_warning_selects_accept() {
        // デフォルトが「No, exit」なので Enter ではなく「2」を送る
        let screen = "WARNING: Claude Code running in Bypass Permissions mode\n\
                      ❯ 1. No, exit\n  2. Yes, I accept";
        let (bytes, _) = auto_yes_reply(screen).unwrap();
        assert_eq!(bytes, b"2");
    }

    #[test]
    fn trust_folder_confirms_default_yes() {
        let screen = "Do you trust the files in this folder?\n\
                      ❯ 1. Yes, proceed\n  2. No, exit";
        let (bytes, _) = auto_yes_reply(screen).unwrap();
        assert_eq!(bytes, b"\r");
    }

    #[test]
    fn default_yes_prompt_sends_enter() {
        let screen = "Do you want to proceed?\n❯ 1. Yes\n  2. No";
        let (bytes, _) = auto_yes_reply(screen).unwrap();
        assert_eq!(bytes, b"\r");
    }

    #[test]
    fn non_default_yes_prompt_sends_number() {
        // カーソルが Yes 以外にある場合は番号キーで直接選択
        let screen = "Do you want to make this edit?\n  1. Yes\n❯ 2. No";
        let (bytes, _) = auto_yes_reply(screen).unwrap();
        assert_eq!(bytes, b"1");
    }

    #[test]
    fn yn_prompt_sends_y() {
        let (bytes, _) = auto_yes_reply("Overwrite? (y/n)").unwrap();
        assert_eq!(bytes, b"y\r");
    }

    #[test]
    fn any_final_question_sends_yes() {
        for screen in [
            "Which deployment strategy should be used?",
            "このまま本番環境へデプロイしますか？",
            "処理を続けますか",
        ] {
            let (bytes, desc) = auto_yes_reply(screen).unwrap();
            assert_eq!(bytes, b"y\r", "screen={screen}");
            assert!(desc.contains("Yes"), "screen={screen}");
        }
    }

    #[test]
    fn question_in_history_does_not_trigger_when_latest_line_is_not_question() {
        let screen = "User: Shall I deploy this?\nAssistant: Build completed successfully.";
        assert!(auto_yes_reply(screen).is_none());
    }

    #[test]
    fn multi_line_question_in_recent_lines_sends_yes() {
        let screen = "Agent: 変更を適用しますか？ [y/N]\n  (1) Yes\n  (2) No\n> ";
        let (bytes, desc) = auto_yes_reply(screen).unwrap();
        assert!(!bytes.is_empty());
        assert!(
            desc.contains("y")
                || desc.contains("1")
                || desc.contains("Yes")
                || desc.contains("自動")
        );
    }

    #[test]
    fn codex_command_approval_sends_yes_shortcut() {
        let screen = "Would you like to run the following command?\n\
                      $ cargo test\n\
                      › 1. Yes, proceed (y)\n\
                        2. Yes, and don't ask again for commands that start with `cargo test`";
        let (bytes, _) = auto_yes_reply(screen).unwrap();
        // 1c96ad8 で「y」から番号キー「1」(= Yes) へ変更
        assert_eq!(bytes, b"1");
    }

    #[test]
    fn codex_network_approval_sends_yes_shortcut() {
        let screen = "Do you want to approve network access to \"crates.io\"?\n\
                      › 1. Yes\n  2. No";
        let (bytes, _) = auto_yes_reply(screen).unwrap();
        assert_eq!(bytes, b"1");
    }

    // ── Antigravity (agy) の承認プロンプト ────────────────────────────
    //
    // 旧テスト `antigravity_all_prompts_send_auto_yes` を差し替えたもの。
    // 旧テストが並べていた "Antigravity: Allow tool call?" 等の文面は、
    // 実物の agy が一度も出さない**推測**だった。実際の agy は
    //   Allow access to this file?
    //   Requesting permission for:
    //     …
    //     Yes, allow access
    //     No, deny access
    //   [Use arrow keys to navigate, Enter to select]
    // という矢印キー選択 UI で、「1. Yes」も「(y/n)」も出さない。
    // そのため旧実装のどの分岐にも当たらず自動YESが素通りしていた
    // (= ユーザー報告「.gemini の設定を入れないと動かない」の原因)。
    // 以下の文面はインストール済み `agy` バイナリに埋め込まれた実物である。

    /// 実物の agy の承認メニューを組み立てる (選択 UI のフッタ付き)。
    fn agy_menu(head: &str, yes: &str, no: &str) -> String {
        format!(
            "{head}\nRequesting permission for:\n  /tmp/work/src/main.rs\n\n  {yes}\n  {no}\n\n\
             [Use arrow keys to navigate, Enter to select]"
        )
    }

    #[test]
    fn antigravity_real_prompts_are_confirmed_with_enter() {
        // (見出し, 肯定選択肢, 否定選択肢) — すべて agy 本体から採取した実文言。
        let cases = [
            (
                "Allow access to this file?",
                "Yes, allow access",
                "No, deny access",
            ),
            (
                "Allow creation of this file?",
                "Yes, allow creation",
                "No, deny creation",
            ),
            (
                "Accept this file edit?",
                "Yes, accept this change",
                "No, reject this change",
            ),
        ];
        for (head, yes, no) in cases {
            let screen = agy_menu(head, yes, no);
            let (bytes, desc) = auto_yes_reply_for(&screen, Some("agy")).unwrap_or_else(|| {
                panic!("agy の承認プロンプトが分類できない: {head}");
            });
            // agy 自身が "Enter to select" と案内しており、肯定側が先頭 (既定選択)。
            assert_eq!(bytes, b"\r", "head={head}");
            assert!(desc.contains("Antigravity"), "head={head} desc={desc}");
        }
    }

    #[test]
    fn antigravity_permission_and_trust_prompts_are_answered() {
        // コマンド実行などの汎用権限要求 (「常に許可」ではなく一回だけ許可を選ぶ)。
        let screen = agy_menu(
            "Requesting permission to run a command",
            "Yes, grant permission for 'git status'",
            "No, deny and always deny for 'git status' in this conversation",
        );
        let (bytes, _) = auto_yes_reply_for(&screen, Some("agy")).unwrap();
        assert_eq!(bytes, b"\r");

        // 起動直後のフォルダ信頼確認。
        let trust = agy_menu(
            "Do you trust this folder?",
            "Yes, I trust this folder",
            "No, exit",
        );
        let (bytes, _) = auto_yes_reply_for(&trust, Some("agy")).unwrap();
        assert_eq!(bytes, b"\r");
    }

    #[test]
    fn antigravity_future_menus_are_covered_by_the_select_hint_rule() {
        // CLI 側の更新で見出しも選択肢の文言も変わった想定。選択 UI のフッタは
        // ウィジェット共通なので、総取りルールが効き続ける (再ビルド不要の保険)。
        let screen = agy_menu(
            "Allow the agent to open a browser tab?",
            "Yes, allow browsing just this once",
            "No, keep me offline",
        );
        let (bytes, _) = auto_yes_reply_for(&screen, Some("agy")).unwrap();
        assert_eq!(bytes, b"\r");
    }

    #[test]
    fn antigravity_rules_do_not_fire_for_other_agents() {
        // agy 専用ルールは、エージェントが判っている他 CLI のタブへは適用しない。
        let screen = agy_menu(
            "Allow access to this file?",
            "Yes, allow access",
            "No, deny access",
        );
        // claude のセッションでは agy ルールが外れる。汎用側にも一致しないこと
        // (見出しが "?" で終わるが、画面末尾はフッタ行なので質問扱いにならない)。
        assert!(
            auto_yes_reply_for(&screen, Some("claude")).is_none(),
            "agy 専用ルールが claude のタブへ漏れている"
        );
        // エージェント不明のときは全ルールを見るので従来通り答えられる。
        assert!(auto_yes_reply_for(&screen, None).is_some());
    }

    #[test]
    fn antigravity_prose_mentioning_yes_is_not_a_prompt() {
        // 承認プロンプトではない普通の出力。"Yes" や "allow" を含んでいても、
        // 見出しと肯定選択肢が揃っていないので発火しない。
        for screen in [
            "I will now check whether the config says yes to telemetry.",
            "Docs: answer \"Yes, allow access\" when the CLI asks you.",
            "Allow access to this file? — この質問には後で答えます\n作業を続行中…",
        ] {
            assert!(
                auto_yes_reply_for(screen, Some("agy")).is_none(),
                "誤爆した: {screen}"
            );
        }
    }

    #[test]
    fn admin_escalation_is_never_auto_answered() {
        // 自動YESは「エージェントのツール承認」を代行するものであって、
        // OS の管理者権限昇格まで肩代わりしない。ここだけは必ずユーザーに残す。
        let screen = agy_menu(
            "Requesting permission for a one-time admin escalation. \
             Administrator privileges are required to set up sandboxing.",
            "Yes, grant permission for sudo",
            "No, deny",
        );
        assert!(
            auto_yes_reply_for(&screen, Some("agy")).is_none(),
            "管理者権限昇格に自動でYESを送ってはいけない"
        );
        assert!(auto_yes_reply_for(&screen, None).is_none());
    }

    #[test]
    fn user_defined_rules_extend_and_override_the_builtin_table() {
        use crate::agents::{prompt_rule_reply_with, PromptRule};

        // config.toml の [[auto_yes_rules]] 相当。再ビルドなしで表を足せる。
        // プロセス共有のレジストリではなく引数でルールを渡す — 並列に走る
        // 他のテストの画面へ、このルールが漏れないようにするため。
        let extend = [PromptRule {
            agent: "",
            needles: &["Continue with this plan?"],
            avoid: &[],
            reply: b"y\r",
            desc: "ユーザー定義ルール",
        }];
        let screen = "Continue with this plan?\n  [1] go  [2] stop";
        let (bytes, desc) = prompt_rule_reply_with(&extend, screen, Some("agy")).unwrap();
        assert_eq!(bytes, b"y\r");
        assert!(desc.contains("ユーザー定義"), "desc={desc}");

        // ユーザールールは組み込み表より先に評価される = 上書きできる。
        let over = [PromptRule {
            agent: "agy",
            needles: &["Allow access to this file?"],
            avoid: &[],
            reply: b"2",
            desc: "ユーザー定義ルール",
        }];
        let menu = agy_menu(
            "Allow access to this file?",
            "Yes, allow access",
            "No, deny access",
        );
        assert_eq!(
            prompt_rule_reply_with(&over, &menu, Some("agy")).map(|(b, _)| b),
            Some(&b"2"[..]),
            "ユーザールールが組み込みを上書きしていない"
        );
        // 同じ画面でも、ルール無しなら組み込み表の Enter に戻る。
        assert_eq!(
            prompt_rule_reply_with(&[], &menu, Some("agy")).map(|(b, _)| b),
            Some(&b"\r"[..])
        );
        // agent 指定は効く: 別 CLI のタブへは流れない。
        assert!(prompt_rule_reply_with(&over, &menu, Some("claude")).is_none());
        // 管理者権限昇格ガードはユーザールールより強い (押し切れない)。
        let sudo = agy_menu(
            "Requesting permission for a one-time admin escalation",
            "Yes, allow access",
            "No, deny",
        );
        assert!(prompt_rule_reply_with(&over, &sudo, Some("agy")).is_none());
    }

    #[test]
    fn config_rules_reach_the_reply_engine_through_the_registry() {
        // config.toml → agents::set_user_prompt_rules → auto_yes_reply の配線確認。
        // 他テストの画面に一致しないよう、目印にテスト専用の合言葉を使う。
        const SENTINEL: &str = "ZAIVERN-TEST-SENTINEL-PROMPT";
        let mut cfg = crate::config::Config::default();
        assert!(cfg.auto_yes_rules.is_empty(), "既定はユーザールール無し");
        cfg.auto_yes_rules.push(crate::config::AutoYesRule {
            pattern: SENTINEL.into(),
            reply: "y\r".into(),
            agent: String::new(),
        });
        crate::config::publish_auto_yes_rules(&cfg);
        let (bytes, _) = auto_yes_reply_for(SENTINEL, Some("agy")).unwrap();
        assert_eq!(bytes, b"y\r", "config のルールが応答エンジンへ届いていない");

        // 空にすれば消える (設定を消して読み直したときに残らない)。
        cfg.auto_yes_rules.clear();
        crate::config::publish_auto_yes_rules(&cfg);
        assert!(auto_yes_reply_for(SENTINEL, Some("agy")).is_none());
    }

    #[test]
    fn plain_output_is_not_a_prompt() {
        // 質問文なしの番号リスト(通常の出力)には反応しない
        assert!(auto_yes_reply("手順:\n1. Yes と入力\n2. 実行").is_none());
        assert!(auto_yes_reply("Codex needs your approval before deployment.").is_none());
        assert!(auto_yes_reply("ビルドが完了しました").is_none());
    }

    #[test]
    fn antigravity_allow_and_japanese_prompts_send_yes() {
        // Antigravity の Allow プロンプト
        let screen1 = "Allow reading file src/main.rs?\n❯ 1. Allow\n  2. Deny";
        let (bytes1, _) = auto_yes_reply(screen1).unwrap();
        assert_eq!(bytes1, b"\r");

        // 日本語プロンプト
        let screen2 = "変更を実行しますか？\n  1. はい\n❯ 2. いいえ";
        let (bytes2, _) = auto_yes_reply(screen2).unwrap();
        assert_eq!(bytes2, b"1");

        // Press Enter プロンプト
        let screen3 = "Press Enter to continue";
        let (bytes3, _) = auto_yes_reply(screen3).unwrap();
        assert_eq!(bytes3, b"\r");
    }

    // ── 番号入力メニュー (アンケート / 選択式プロンプト) ──────────────
    //
    // ユーザー報告「アンケートを数字で入力しないと進まなくなっていた」の回帰テスト。
    // 画面の形は実物の CLI が出す並び (質問 → 選択肢 → 入力を促す行) に合わせている。

    use super::{numbered_menu_prompt, parse_numbered_options, parse_option_line, MenuOption};

    fn opt(num: u8, label: &str) -> MenuOption {
        MenuOption {
            num,
            label: label.into(),
        }
    }

    #[test]
    fn option_line_parser_reads_every_numbering_style() {
        // (入力行, 期待する (番号, 本文))
        let table: &[(&str, Option<(u8, &str)>)] = &[
            ("1. Yes", Some((1, "Yes"))),
            ("2) No", Some((2, "No"))),
            ("[3] Maybe", Some((3, "Maybe"))),
            ("4 - Skip", Some((4, "Skip"))),
            ("  ❯ 1. Yes, allow access", Some((1, "Yes, allow access"))),
            ("\x1b[1m1.\x1b[0m とても満足", Some((1, "とても満足"))),
            ("5、回答しない", Some((5, "回答しない"))),
            // 本文に数字が入っていても番号は行頭のものだけ
            ("2. Rate 1-5 stars", Some((2, "Rate 1-5 stars"))),
            // 選択肢ではないもの
            ("手順:", None),
            ("2024-01-01 build finished", None),
            ("1.", None),            // 本文が無い
            ("0. zero", None),       // 0 始まりは選択肢ではない
            ("100. too many", None), // 3 桁は年号や連番ログの可能性
            ("Enter a number (1-5): ", None),
        ];
        for (line, want) in table {
            let got = parse_option_line(line).map(|o| (o.num, o.label));
            let got = got.as_ref().map(|(n, l)| (*n, l.as_str()));
            assert_eq!(got, *want, "line={line:?}");
        }
    }

    #[test]
    fn option_list_survives_blank_lines_and_takes_the_last_run() {
        // 選択肢の間に空行や飾り行が入る CLI がある
        let screen = "古い手順:\n1. これは本文\n2. これも本文\n\
                      \n質問です\n\n  1. はい\n\n  2. いいえ\n\n  3. あとで\n";
        assert_eq!(
            parse_numbered_options(screen),
            vec![opt(1, "はい"), opt(2, "いいえ"), opt(3, "あとで")],
            "画面下の連番 (= 実際のプロンプト) を採らなかった"
        );
    }

    #[test]
    fn numbered_menu_needs_an_explicit_number_prompt() {
        // 「番号を打て」と言っていない番号リストはメニューではない (誤爆防止)
        let table: &[(&str, bool)] = &[
            ("手順:\n1. Yes と入力\n2. 実行", false),
            (
                "ログ: 処理に 1-3 秒かかりました\n1. 前処理\n2. 本処理",
                false,
            ),
            // 「(1-5)」が選択肢の本文の中にあるだけ → メニュー扱いしない
            ("結果一覧\n1. Rate on a scale (1-5)\n2. Skip it", false),
            (
                "好きな番号は?\n1. one\n2. two\nEnter a number (1-2): ",
                true,
            ),
            ("Choose an option:\n1) one\n2) two", true),
            (
                "どれにしますか\n1. これ\n2. あれ\n番号を入力してください: ",
                true,
            ),
            ("Pick:\n[1] one\n[2] two\nSelect an option [1-2]: ", true),
            ("Pick:\n1 - one\n2 - two\nChoose 1-2", true),
        ];
        for (screen, want) in table {
            assert_eq!(
                numbered_menu_prompt(screen).is_some(),
                *want,
                "screen={screen:?}"
            );
        }
        // 誤爆しない = 自動応答もしない
        assert!(auto_yes_reply("手順:\n1. Yes と入力\n2. 実行").is_none());
        assert!(stalled_reply("手順:\n1. Yes と入力\n2. 実行").is_none());
    }

    #[test]
    fn numbered_menu_prefers_allow_then_skip() {
        // (画面, 送るキー列, 説明に含まれる語)
        let table: &[(&str, &[u8], &str)] = &[
            // 承認系: 肯定肢を選ぶ
            (
                "Allow this command to run?\n\
                 1. No, cancel\n2. Yes, allow this once\n3. Yes, and don't ask again\n\
                 Enter a number (1-3): ",
                b"2\r",
                "承認肢",
            ),
            // アンケート: スキップ肢がある → 評点ではなくスキップ
            (
                "How would you rate your experience with this CLI?\n\
                 1. Very satisfied\n2. Neutral\n3. Very dissatisfied\n4. Skip this survey\n\
                 Enter a number (1-4): ",
                b"4\r",
                "スキップ",
            ),
            // 日本語アンケート + スキップ
            (
                "アンケートにご協力ください。満足度はいかがですか?\n\
                 1) とても満足\n2) 普通\n3) 不満\n4) 回答しない\n\
                 番号を入力してください: ",
                b"4\r",
                "スキップ",
            ),
            // 英日混在: 肯定肢が日本語
            (
                "Allow file write?\n1. Cancel\n2. はい、許可します\nEnter a number (1-2): ",
                b"2\r",
                "承認肢",
            ),
            // 承認系でも肯定肢が無ければ見送り肢 (更新の催促などを黙って畳む)
            (
                "Update available. Install now?\n1. Not now\n2. Install\n\
                 Enter your choice (1-2): ",
                b"1\r",
                "スキップ",
            ),
        ];
        for (screen, want, desc_part) in table {
            let (bytes, desc) = stalled_reply(screen).unwrap_or_else(|| {
                panic!("番号メニューに応答しなかった:\n{screen}");
            });
            assert_eq!(bytes, *want, "screen={screen}");
            assert!(desc.contains(desc_part), "desc={desc} screen={screen}");
        }
    }

    #[test]
    fn rating_only_survey_answers_the_positive_end_and_says_so() {
        // 評点しか無い尺度でも止めない (止まる方が害が大きい、というユーザーの判断)。
        // ただし「勝手に答えた」ことが判る説明を必ず返す。
        let table: &[(&str, &[u8])] = &[
            // 降順 (1 が最も肯定的)
            (
                "How satisfied are you with Zaivern?\n\
                 1. Very satisfied\n2. Satisfied\n3. Neutral\n4. Dissatisfied\n5. Very dissatisfied\n\
                 Enter a number (1-5): ",
                b"1\r",
            ),
            // 昇順 (5 が最も肯定的)
            (
                "How likely are you to recommend us?\n\
                 1. Very unlikely\n2. Unlikely\n3. Neutral\n4. Likely\n5. Very likely\n\
                 Enter a number (1-5): ",
                b"5\r",
            ),
            // 否定端しか語で判らない → 反対側の端
            (
                "満足度の評価にご協力ください\n\
                 1. 全く思わない\n2. あまり\n3. どちらとも\n4. まあまあ\n5. そう思う\n\
                 番号を入力してください: ",
                b"5\r",
            ),
            // ラベルが数字だけの尺度 → 一番大きい数字
            (
                "Rate this session (survey)\n1. 1\n2. 2\n3. 3\nEnter a number (1-3): ",
                b"3\r",
            ),
            // 語で向きが判らない → 既定は最後の選択肢
            (
                "アンケート: 今回の評価を選んでください\n1. A\n2. B\n3. C\n番号を入力: ",
                b"3\r",
            ),
        ];
        for (screen, want) in table {
            let (bytes, desc) = stalled_reply(screen)
                .unwrap_or_else(|| panic!("評点アンケートに答えなかった:\n{screen}"));
            assert_eq!(bytes, *want, "screen={screen}");
            assert!(
                desc.contains("自動で回答しました"),
                "自動回答の事実が説明に出ていない: desc={desc}"
            );
            // 説明には選んだ番号がそのまま出る (UI で「5」と判る)
            let picked = String::from_utf8_lossy(want).trim_end().to_string();
            assert!(desc.ends_with(&picked), "desc={desc} picked={picked}");
        }
    }

    #[test]
    fn free_form_choice_menus_are_left_to_the_human() {
        // 肯定でも見送りでもアンケートでもない「どれを使う?」は人が決める。
        // ただし承認待ちとしては検知される (scan_attention 側のテストを参照)。
        let screen = "Which model do you want to use?\n\
                      1. opus\n2. sonnet\n3. haiku\nEnter a number (1-3): ";
        assert!(auto_yes_reply(screen).is_none(), "自由選択に勝手に答えた");
        assert!(
            stalled_reply(screen).is_none(),
            "停滞後でも自由選択には答えない"
        );
        assert!(
            numbered_menu_prompt(screen).is_some(),
            "承認待ちとして検知できる形になっていない"
        );
    }

    #[test]
    fn numbered_menu_digits_only_fire_when_stalled() {
        // ユーザー報告: Claude Code の番号メニューへ**数字が連続で打ち込まれた**。
        // 通常スキャンの分類 (auto_yes_reply*) は数字 + Enter を一切返さない。
        // 数字を打つのは停滞ウォッチドッグ経由 (stalled_reply*) だけにする。
        let table: &[(&str, &[u8])] = &[
            (
                "Allow this command to run?\n\
                 1. No, cancel\n2. Yes, allow this once\n\
                 Enter a number (1-2): ",
                b"2\r",
            ),
            (
                "アンケート: 満足度はいかがですか?\n\
                 1) とても満足\n2) 普通\n3) 回答しない\n\
                 番号を入力してください: ",
                b"3\r",
            ),
            (
                "Help us improve! Take a short survey?\n\
                 1. Take the survey\n2. Maybe later\n\
                 Enter a number (1-2): ",
                b"2\r",
            ),
        ];
        for (screen, want) in table {
            assert!(
                auto_yes_reply(screen).is_none(),
                "通常スキャンで番号メニューへ数字を打った: {screen}"
            );
            assert!(
                auto_yes_reply_for(screen, Some("claude")).is_none(),
                "通常スキャン (claude) で番号メニューへ数字を打った: {screen}"
            );
            assert_eq!(
                stalled_reply(screen).map(|(b, _)| b),
                Some(*want),
                "停滞後にも数字を打たない (= 永久に止まる): {screen}"
            );
        }
    }

    #[test]
    fn never_guard_beats_a_numbered_menu() {
        // 管理者権限昇格は番号メニューの形をしていても絶対に自動応答しない。
        let screen = "Administrator privileges are required to continue.\n\
                      1. Yes, allow\n2. No\nEnter a number (1-2): ";
        assert!(auto_yes_reply(screen).is_none(), "権限昇格に自動応答した");
        assert!(auto_yes_reply_for(screen, Some("agy")).is_none());
        // 停滞後の分類でも同じ (30 秒待ったからといって権限昇格は押さない)
        assert!(
            stalled_reply(screen).is_none(),
            "停滞時に権限昇格へ応答した"
        );
        assert!(stalled_reply_for(screen, Some("agy")).is_none());
        // 検知自体はされる = 人が判断できるよう承認待ちには出る
        assert!(numbered_menu_prompt(screen).is_some());
    }

    #[test]
    fn arrow_key_selector_gets_enter_not_a_digit() {
        // Antigravity のような矢印キー UI へ数字を撃つと入力欄が汚れる。
        let screen = "Allow access to this file?\n\
                      1. Yes, allow access\n2. No\n\
                      [Use arrow keys to navigate, Enter to select]";
        let (bytes, _) = stalled_reply_for(screen, Some("agy")).unwrap();
        assert_eq!(bytes, b"\r", "矢印キー UI に数字を送った");
        // 「番号を入力」の文言が混ざっていても数字は送らない
        let mixed = format!("{screen}\nEnter a number (1-2): ");
        assert_eq!(
            stalled_reply_for(&mixed, Some("agy")).map(|(b, _)| b),
            Some(&b"\r"[..])
        );
        // カタログに載っていない CLI でも、矢印ヒントがあれば数字は送らない
        let unknown = "Pick one\n1. Yes, continue\n2. No\n\
                       Use arrow keys to move, then Enter\nEnter a number (1-2): ";
        assert!(
            stalled_reply_for(unknown, Some("claude")).map(|(b, _)| b) != Some(&b"1\r"[..]),
            "矢印キー UI に番号を送った"
        );
    }

    #[test]
    fn numbered_menu_end_to_end_per_agent_bin() {
        // (エージェント bin, 画面, 送るバイト列)
        let table: &[(&str, &str, &[u8])] = &[
            (
                "claude",
                "Do you want to make this edit to config.toml?\n\
                 1. Yes\n2. Yes, and don't ask again\n3. No\n\
                 Enter a number (1-3): ",
                b"1\r",
            ),
            (
                "codex",
                "Codex needs your approval to run `cargo test`.\n\
                 1. No, cancel\n2. Yes, proceed\n\
                 Select an option [1-2]: ",
                b"2\r",
            ),
            (
                "gemini",
                "Help us improve Gemini CLI! Take a short survey?\n\
                 1. Take the survey\n2. Maybe later\n\
                 Enter a number (1-2): ",
                b"2\r",
            ),
            (
                "cursor-agent",
                "How would you rate this session?\n\
                 1. Excellent\n2. Good\n3. Bad\n\
                 Enter a number (1-3): ",
                b"1\r",
            ),
            (
                "agy",
                "Allow creation of this file?\n1. Yes, allow creation\n2. No\n\
                 Enter a number (1-2): ",
                b"\r", // 応答表 (矢印キー UI) が最優先
            ),
        ];
        for (bin, screen, want) in table {
            let got = stalled_reply_for(screen, Some(bin)).map(|(b, _)| b);
            assert_eq!(got, Some(*want), "bin={bin} screen={screen}");
        }
    }

    #[test]
    fn user_rule_overrides_the_built_in_menu_choice() {
        use crate::agents::{prompt_rule_reply_with, PromptRule};
        let screen = "How would you rate this CLI?\n\
                      1. Great\n2. Fine\n3. Bad\n4. Skip\n\
                      Enter a number (1-4): ";
        // 組み込みの選び方ではスキップ肢
        assert_eq!(stalled_reply(screen).map(|(b, _)| b), Some(&b"4\r"[..]));
        // config.toml の [[auto_yes_rules]] で「3 を送る」に上書きできる。
        // (auto_yes_reply_for はユーザールール → 番号メニューの順に見るので、
        //  ここで一致すれば番号メニューの判断には進まない)
        let user = [PromptRule {
            agent: "",
            needles: &["How would you rate this CLI?"],
            avoid: &[],
            reply: b"3\r",
            desc: "テスト用ユーザールール",
        }];
        assert_eq!(
            prompt_rule_reply_with(&user, screen, Some("claude")).map(|(b, _)| b),
            Some(&b"3\r"[..]),
            "ユーザー定義ルールが番号メニューより優先されない"
        );
    }

    #[test]
    fn resize_debounce_retry_ships_the_same_size_again() {
        // T1: PTY のロックが取れずに送れなかったとき、次フレームで送り直す。
        let mut d = super::ResizeDebounce::default();
        let mut shipped = None;
        for _ in 0..super::RESIZE_STABLE_FRAMES {
            shipped = d.on_request((30, 100));
        }
        assert_eq!(shipped, Some((30, 100)), "安定後に送信サイズが出ない");
        assert!(!d.pending(), "送信済みなのに保留が残っている");
        d.retry();
        assert!(
            d.pending(),
            "retry 後に保留が立たない (draw が回らず止まる)"
        );
        assert_eq!(
            d.on_request((30, 100)),
            Some((30, 100)),
            "retry 後に同じサイズを送り直さなかった"
        );
    }

    // ── レート制限の検知 ──────────────────────────────────────────────

    use super::detect_rate_limit;

    #[test]
    fn rate_limit_detects_known_cli_messages() {
        // Claude Code のフッタ表記
        let l = detect_rate_limit("some output\n5-hour limit reached ∙ resets 3am\n").unwrap();
        assert!(l.contains("resets 3am"));
        // 一般的な使用上限
        assert!(detect_rate_limit("Usage limit reached. Try again later.").is_some());
        // Codex 系
        assert!(detect_rate_limit("You've hit your usage limit.").is_some());
        // API エラー
        assert!(detect_rate_limit("HTTP 429: Too Many Requests").is_some());
        // 事前警告
        assert!(detect_rate_limit("Approaching usage limit · 80%").is_some());
    }

    #[test]
    fn rate_limit_ignores_normal_conversation() {
        // 「limit」という単語や制限の話題だけでは反応しない
        assert!(detect_rate_limit("we should limit the retries to 3").is_none());
        assert!(detect_rate_limit("set a rate limiter on the API").is_none());
        assert!(detect_rate_limit("普通のビルド出力です").is_none());
    }

    // ── 未読管理 (意味的ハッシュ) ────────────────────────────────────

    use super::semantic_hash;

    #[test]
    fn semantic_hash_ignores_spinners_and_counters() {
        // スピナー記号と数値カウンタの揺れだけでは変わらない
        let a = semantic_hash("⠋ Working… 12s · 3.2k tokens\nreading files");
        let b = semantic_hash("⠙ Working… 13s · 3.4k tokens\nreading files");
        assert_eq!(a, b, "スピナー/カウンタの揺れで未読になってはいけない");
        // 本当に新しい出力では変わる
        let c = semantic_hash("⠋ Working… 12s\nreading files\ndone: wrote main.rs");
        assert_ne!(a, c);
    }

    #[test]
    fn unread_lifecycle_via_real_pty() {
        use super::{Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        let spec = SpawnSpec {
            title: "unread-e2e".into(),
            preset_name: "test".into(),
            icon: "◆".into(),
            command: "echo UNREAD_MARKER_1; sleep 5".into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut s = Session::spawn(997, spec, eframe::egui::Context::default()).expect("PTY起動");
        assert!(!s.has_unread(), "起動直後はまだ何も出ていない");

        // 出力が出る → スキャンで cur_hash が動き、未読になる
        let mut unread = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            let _ = s.scan_attention(false);
            if s.has_unread() {
                unread = true;
                break;
            }
        }
        assert!(unread, "新しい出力で未読が立たなかった");

        // 見た (mark_read) → 既読へ
        s.mark_read();
        assert!(!s.has_unread());

        // 「あとで見る」ピン → mark_read では消えず、acknowledge で消える
        s.mark_unread();
        assert!(s.has_unread());
        s.mark_read();
        assert!(s.has_unread(), "ピンは表示中の既読処理では外れない");
        s.acknowledge();
        assert!(!s.has_unread());
        s.kill();
    }

    #[test]
    fn pty_log_records_output_and_survives_restart_semantics() {
        use super::{Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        let dir = std::env::temp_dir().join(format!("zaivern-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = dir.join("probe-1.log");
        let spec = SpawnSpec {
            title: "log-e2e".into(),
            preset_name: "test".into(),
            icon: "📜".into(),
            command: "echo LOG_MARKER_OK".into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: Some(log.clone()),
        };
        let mut s = Session::spawn(996, spec, eframe::egui::Context::default()).expect("PTY起動");
        let mut ok = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            if text.contains("LOG_MARKER_OK") {
                ok = true;
                break;
            }
        }
        assert!(ok, "PTY 出力がログに書かれなかった");
        // ヘッダで起動の区切りが分かる
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("===== [Zaivern] log-e2e"));
        s.kill();
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::{
        abs_row, all_terminal_lines, autoscroll_step, clip_selection, input_area_selection,
        is_image_paste_chord_on, key_bytes, line_hits, mac_agent_input_bytes, normalize_sel,
        prune_clip_pngs, save_clipboard_png, screen_row, search_scroll_target, selection_text,
        selection_text_abs, word_selection, Session, CLIP_PNG_KEEP,
    };

    /// 履歴を積んだパーサと「実在する最古の絶対行」を作る。
    fn history(rows: u16, cols: u16, n: usize) -> (vt100::Parser, usize) {
        let mut p = vt100::Parser::new(rows, cols, 200);
        for i in 0..n {
            p.process(format!("line{:03}\r\n", i).as_bytes());
        }
        p.set_scrollback(usize::MAX);
        let max_abs = p.screen().scrollback() + rows as usize - 1;
        p.set_scrollback(0);
        (p, max_abs)
    }

    #[test]
    fn all_terminal_lines_covers_scrollback_and_screen() {
        let mut p = vt100::Parser::new(5, 20, 100);
        for i in 0..30 {
            p.process(format!("line{:03}\r\n", i).as_bytes());
        }
        let lines = all_terminal_lines(&mut p);
        assert!(lines.len() >= 30, "30行全部が取れる: {}", lines.len());
        for (i, l) in lines.iter().take(30).enumerate() {
            assert_eq!(l, &format!("line{:03}", i));
        }
        // 呼び出し後は scrollback 位置が元 (0) に戻っている
        assert_eq!(p.screen().scrollback(), 0);
    }

    #[test]
    fn replayed_log_bytes_rebuild_scrollback_with_banner() {
        // セッション復元の心臓部: 生ログのバイト列を process へ流すだけで
        // 前回のスクロールバックが再構成される (PTY 不要・in-process)。
        let mut p = vt100::Parser::new(5, 60, 100);
        for i in 0..12 {
            p.process(format!("\x1b[1m前回の出力 {:02}\x1b[0m\r\n", i).as_bytes());
        }
        p.process(super::RESTORE_BANNER.as_bytes());
        let lines = all_terminal_lines(&mut p);
        assert!(
            lines.iter().any(|l| l.contains("前回の出力 00")),
            "画面外へ流れた行もスクロールバックに残る: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("前回のセッションここまで")),
            "区切りバナーが最後に見える: {lines:?}"
        );
        // バナーは前回分の後 (= ここから下が今回のライブ出力になる)
        let old = lines
            .iter()
            .position(|l| l.contains("前回の出力 11"))
            .unwrap();
        let banner = lines
            .iter()
            .position(|l| l.contains("前回のセッションここまで"))
            .unwrap();
        assert!(
            banner > old,
            "バナーは再生分の末尾: old={old} banner={banner}"
        );
    }

    #[test]
    fn replay_banner_recovers_from_alternate_screen_log() {
        // 前回ログが代替画面 (?1049h) の途中で切れていても、バナー先頭の
        // ?1049l で平常へ戻り、以降の出力が普通のグリッドへ描かれる。
        let mut p = vt100::Parser::new(5, 60, 100);
        p.process(b"scrollback-line\r\n\x1b[?1049h\x1b[2JTUI!");
        p.process(super::RESTORE_BANNER.as_bytes());
        assert!(!p.screen().alternate_screen(), "代替画面から抜けている");
        p.process(b"live-output\r\n");
        let lines = all_terminal_lines(&mut p);
        assert!(lines.iter().any(|l| l.contains("live-output")), "{lines:?}");
    }

    #[test]
    fn deep_scrollback_read_does_not_panic() {
        // vt100 0.15.2 素のままだと offset > 画面行数 で減算オーバーフロー
        // panic していた (vendor/vt100 パッチの回帰テスト)。
        let mut p = vt100::Parser::new(5, 20, 100);
        for i in 0..40 {
            p.process(format!("deep{:03}\r\n", i).as_bytes());
        }
        p.set_scrollback(30); // 画面 5 行を大きく超えて戻る
        let contents = p.screen().contents();
        // 一番上の可視行は 30 行戻った位置の行になる
        assert!(
            contents.lines().next().unwrap_or("").starts_with("deep"),
            "深いスクロールバックでも読める: {contents:?}"
        );
        assert_eq!(
            p.screen().rows(0, 20).count(),
            5,
            "可視行数は画面行数のまま"
        );
        p.set_scrollback(0);
    }

    #[test]
    fn decrc_after_shrink_does_not_panic() {
        // 代替画面 (?1049h) がカーソルを保存 → ペイン縮小 → ?1049l の DECRC が
        // 縮小前の行番号を復元 → 次の描画で範囲外 unwrap により PTY 読取
        // スレッドが panic し、端末が黒いまま戻らなくなっていた
        // (vendor/vt100 の saved_pos クランプの回帰テスト)。
        let mut p = vt100::Parser::new(30, 80, 100);
        p.process(b"\x1b[30;1H"); // カーソルを最下行 (30行目) へ
        p.process(b"\x1b[?1049h"); // 代替画面へ (通常グリッドの位置を保存)
        p.set_size(12, 80); // Cockpit でファイルを開く等でペインが縮む
        p.process(b"\x1b[?1049l"); // 代替画面終了 + DECRC (保存位置を復元)
        p.process(b"\r\x1b[2K$ x"); // プロンプト再描画 — ここで落ちないこと
        let (row, _col) = p.screen().cursor_position();
        assert!(row < 12, "復元されたカーソルは画面内に収まる: row={row}");
    }

    #[test]
    fn line_hits_is_case_insensitive_and_skips_empty_query() {
        let lines = vec![
            "Error: build failed".to_string(),
            "ok".to_string(),
            "  ERROR again".to_string(),
        ];
        assert_eq!(line_hits(&lines, "error"), vec![0, 2]);
        assert_eq!(line_hits(&lines, "ERROR"), vec![0, 2]);
        assert_eq!(line_hits(&lines, ""), Vec::<usize>::new());
    }

    #[test]
    fn search_scroll_target_centers_hit() {
        // total 100 行 / 画面 10 行 → 最大戻り量 90
        assert_eq!(search_scroll_target(0, 100, 10), 90); // 最古行 → 一番上まで戻る
        assert_eq!(search_scroll_target(99, 100, 10), 0); // 最新行 → 戻らない
        assert_eq!(search_scroll_target(50, 100, 10), 45); // 中央寄せ
        assert_eq!(search_scroll_target(5, 8, 10), 0); // 画面に収まる量なら戻らない
    }

    #[test]
    fn session_search_step_finds_and_navigates_hits() {
        let dir = std::env::current_dir().unwrap();
        let spec = super::SpawnSpec {
            title: "test".into(),
            command: "echo search-test".into(),
            cwd: dir,
            env: std::collections::HashMap::new(),
            preset_name: String::new(),
            icon: "💬".into(),
            log_path: None,
        };
        let mut session = Session::spawn(9992, spec, eframe::egui::Context::default()).unwrap();
        // 画面 30 行を超える出力を直接パーサへ流し込み、スクロールバックを作る
        {
            let mut p = session.parser.lock().unwrap();
            for i in 0..80 {
                let tag = if i % 10 == 5 { "NEEDLE" } else { "hay" };
                p.process(format!("row{:03} {}\r\n", i, tag).as_bytes());
            }
        }
        session.search.query = "needle".to_string();
        // 初回 = 一番新しいヒット (row075)
        assert!(session.search_step(false));
        assert_eq!(session.search.total, 8);
        assert_eq!(session.search.index, 8);
        let first_hit = session.search.hit_line.unwrap();
        // 現在ヒットの可視行が画面内に入っている (強調表示用)
        let (_, vis) = session.search.current_vis.unwrap();
        assert!(vis < 30, "可視行は画面行数未満: {vis}");
        // 前 (古い方) へ → row065 のはず (絶対行も戻り量も増える)
        assert!(session.search_step(false));
        assert_eq!(session.search.index, 7);
        assert!(session.search.hit_line.unwrap() < first_hit);
        // 次 (新しい方) へ戻る
        assert!(session.search_step(true));
        assert_eq!(session.search.index, 8);
        assert_eq!(session.search.hit_line.unwrap(), first_hit);
        // ヒットしないクエリは false で状態リセット
        session.search.query = "no-such-text".to_string();
        assert!(!session.search_step(true));
        assert_eq!(session.search.total, 0);
        assert_eq!(session.search.index, 0);
    }

    /// DoD: セッションは**指定したフォルダで実際に**起動する。
    ///
    /// アプリが持つルートは canonicalize 済みで、Windows ではそれが `\\?\C:\…`
    /// (verbatim 形式) になる。これをそのまま PTY へ渡すと cmd.exe が
    /// 「UNC paths are not supported」と言ってカレントディレクトリを捨て、
    /// エージェントが `C:\Windows` で動き出す。素のパスへ直す処理の回帰テスト。
    #[test]
    fn session_starts_in_the_requested_directory() {
        use std::time::Duration;
        let dir = crate::test_util::unique_temp_dir("zaivern-term-test", "cwd");
        // アプリ内部で持つ形 (canonicalize 済み) をそのまま渡す
        let asked = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        let command = if cfg!(windows) { "cd" } else { "pwd" };
        let spec = super::SpawnSpec {
            title: "cwd".into(),
            command: command.into(),
            cwd: asked.clone(),
            env: std::collections::HashMap::new(),
            preset_name: String::new(),
            icon: "💬".into(),
            log_path: None,
        };
        let mut session =
            Session::spawn(9993, spec, eframe::egui::Context::default()).expect("PTY起動");

        // 覚えている cwd も素のパス (@パス補完の相対表示が起動先と揃う)
        let want = crate::pathx::plain(asked);
        assert_eq!(session.cwd, want, "セッションの cwd は素のパス");

        // 実際にその場所で走ったか、子プロセスの出力で確かめる。
        // 端末幅で折り返されても比較できるよう、空白を落として突き合わせる。
        let squeeze = |s: &str| -> String { s.split_whitespace().collect() };
        let expected = squeeze(&want.to_string_lossy());
        let mut screen = String::new();
        for _ in 0..100 {
            screen = session.parser.lock().unwrap().screen().contents();
            if squeeze(&screen).contains(&expected) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            squeeze(&screen).contains(&expected),
            "起動先が {} でない (画面: {screen:?})",
            want.display()
        );
        session.kill();
        std::fs::remove_dir_all(&dir).ok();
    }

    fn mac_command() -> egui::Modifiers {
        egui::Modifiers {
            alt: false,
            ctrl: false,
            shift: false,
            mac_cmd: true,
            command: true,
        }
    }

    #[test]
    fn save_clipboard_png_roundtrip_and_ascii_name() {
        let dir = crate::test_util::unique_temp_dir("zaivern-term-test", "clip-roundtrip");
        // 3x2 の RGBA バッファ (左上だけ不透明の赤)
        let (w, h) = (3usize, 2usize);
        let mut rgba = vec![0u8; w * h * 4];
        rgba[0] = 255;
        rgba[3] = 255;
        let path = save_clipboard_png(w, h, &rgba, &dir).unwrap();
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(
            name.is_ascii() && !name.contains(' '),
            "空白なし ASCII 名: {name}"
        );
        assert!(
            name.starts_with("clip-") && name.ends_with(".png"),
            "{name}"
        );
        // image クレートで読み戻して寸法と画素が一致する
        let img = image::open(&path).unwrap().to_rgba8();
        assert_eq!((img.width(), img.height()), (3, 2));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_clipboard_png_rejects_degenerate_input() {
        let dir = crate::test_util::unique_temp_dir("zaivern-term-test", "clip-degenerate");
        // ゼロサイズは拒否
        assert!(save_clipboard_png(0, 0, &[], &dir).is_err());
        assert!(save_clipboard_png(0, 5, &[], &dir).is_err());
        assert!(save_clipboard_png(5, 0, &[], &dir).is_err());
        // バッファ長の不一致は panic せず Err
        assert!(save_clipboard_png(2, 2, &[0u8; 15], &dir).is_err());
        assert!(save_clipboard_png(2, 2, &[0u8; 17], &dir).is_err());
        // 乗算あふれも Err
        assert!(save_clipboard_png(usize::MAX, 2, &[0u8; 4], &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_clipboard_png_prunes_old_files() {
        let dir = crate::test_util::unique_temp_dir("zaivern-term-test", "clip-prune");
        // 自前の命名に合わないファイルは間引き対象外
        std::fs::write(dir.join("note.txt"), b"keep").unwrap();
        std::fs::write(dir.join("unrelated.png"), b"keep").unwrap();
        let rgba = [0u8; 4];
        let mut last = None;
        for _ in 0..CLIP_PNG_KEEP + 3 {
            last = Some(save_clipboard_png(1, 1, &rgba, &dir).unwrap());
        }
        let count_clips = |dir: &std::path::Path| {
            std::fs::read_dir(dir)
                .unwrap()
                .flatten()
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.starts_with("clip-") && n.ends_with(".png")
                })
                .count()
        };
        assert_eq!(count_clips(&dir), CLIP_PNG_KEEP, "保存上限で古い分が消える");
        assert!(last.unwrap().exists(), "直近の保存分は残る");
        assert!(dir.join("note.txt").exists());
        assert!(dir.join("unrelated.png").exists());
        // keep を明示しても縮む
        prune_clip_pngs(&dir, 2);
        assert_eq!(count_clips(&dir), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_paste_chord_per_os_table() {
        // (mac?, ctrl, shift, alt, mac_cmd, key, expect)
        let cases = [
            // macOS: ⌘V だけが対象
            (true, false, false, false, true, egui::Key::V, true),
            (true, true, false, false, false, egui::Key::V, false), // Ctrl+V は対象外
            (true, false, true, false, true, egui::Key::V, false),  // ⌘⇧V
            (true, false, false, true, true, egui::Key::V, false),  // ⌘⌥V
            (true, false, false, false, true, egui::Key::C, false), // ⌘C
            // Windows/Linux: Ctrl+V だけが対象
            (false, true, false, false, false, egui::Key::V, true),
            (false, true, true, false, false, egui::Key::V, false), // Ctrl+Shift+V は素通し
            (false, true, false, true, false, egui::Key::V, false), // Ctrl+Alt+V
            (false, false, false, false, true, egui::Key::V, false), // ⌘ 相当のみ
            (false, true, false, false, false, egui::Key::B, false),
        ];
        for (mac, ctrl, shift, alt, mac_cmd, key, expect) in cases {
            let m = egui::Modifiers {
                alt,
                ctrl,
                shift,
                mac_cmd,
                command: ctrl || mac_cmd,
            };
            assert_eq!(
                is_image_paste_chord_on(key, m, mac),
                expect,
                "mac={mac} ctrl={ctrl} shift={shift} alt={alt} cmd={mac_cmd} key={key:?}"
            );
        }
    }

    #[test]
    fn mac_command_a_is_not_forwarded_to_pty() {
        // ⌘A は PTY へ送らずローカル全選択で扱う (Ctrl+A を送っても
        // CLI 側では行頭移動になるだけで全選択にならないため)
        assert_eq!(mac_agent_input_bytes(egui::Key::A, mac_command()), None);
        assert_eq!(mac_agent_input_bytes(egui::Key::C, mac_command()), None);
        assert_eq!(
            mac_agent_input_bytes(egui::Key::A, egui::Modifiers::CTRL),
            None
        );
    }

    #[test]
    fn mac_command_line_editing_maps_to_readline_bytes() {
        // ⌘← / ⌘→ / ⌘⌫ = 行頭 / 行末 / 行頭まで削除
        assert_eq!(
            mac_agent_input_bytes(egui::Key::ArrowLeft, mac_command()),
            Some(b"\x01".as_slice())
        );
        assert_eq!(
            mac_agent_input_bytes(egui::Key::ArrowRight, mac_command()),
            Some(b"\x05".as_slice())
        );
        assert_eq!(
            mac_agent_input_bytes(egui::Key::Backspace, mac_command()),
            Some(b"\x15".as_slice())
        );
        // ⌘K = 画面クリア (Ctrl+L)
        assert_eq!(
            mac_agent_input_bytes(egui::Key::K, mac_command()),
            Some(b"\x0c".as_slice())
        );
        // Command なしでは何も返さない
        assert_eq!(
            mac_agent_input_bytes(egui::Key::ArrowLeft, egui::Modifiers::ALT),
            None
        );
    }

    #[test]
    fn option_word_keys_map_to_readline_escapes() {
        // ⌥← / ⌥→ = 単語移動、⌥⌫ = 単語削除 (readline ESC シーケンス)
        let alt = egui::Modifiers::ALT;
        assert_eq!(
            key_bytes(egui::Key::ArrowLeft, alt, false),
            Some(b"\x1bb".to_vec())
        );
        assert_eq!(
            key_bytes(egui::Key::ArrowRight, alt, false),
            Some(b"\x1bf".to_vec())
        );
        assert_eq!(
            key_bytes(egui::Key::Backspace, alt, false),
            Some(vec![0x1b, 0x7f])
        );
        // 修飾なしは従来どおり
        assert_eq!(
            key_bytes(egui::Key::ArrowLeft, egui::Modifiers::NONE, false),
            Some(b"\x1b[D".to_vec())
        );
        assert_eq!(
            key_bytes(egui::Key::Backspace, egui::Modifiers::NONE, false),
            Some(vec![0x7f])
        );
    }

    #[test]
    fn normalize_sel_orders_row_major() {
        // 上方向・左方向へのドラッグでも (開始 <= 終了) に揃う
        assert_eq!(normalize_sel(((2, 3), (0, 5))), ((0, 5), (2, 3)));
        assert_eq!(normalize_sel(((1, 8), (1, 2))), ((1, 2), (1, 8)));
        assert_eq!(normalize_sel(((0, 0), (0, 0))), ((0, 0), (0, 0)));
    }

    #[test]
    fn selection_text_extracts_single_line() {
        let mut p = vt100::Parser::new(5, 20, 0);
        p.process(b"hello world");
        assert_eq!(selection_text(p.screen(), ((0, 0), (0, 4))), "hello");
        assert_eq!(selection_text(p.screen(), ((0, 6), (0, 10))), "world");
    }

    #[test]
    fn abs_row_and_screen_row_are_inverse() {
        // (画面行, 行数, scroll) → 絶対行。画面の下端が絶対行 scroll。
        let cases = [
            ((0u16, 5u16, 0usize), 4usize),
            ((4, 5, 0), 0),
            ((0, 5, 10), 14),
            ((4, 5, 10), 10),
            ((0, 1, 7), 7),
        ];
        for ((r, rows, scroll), want) in cases {
            assert_eq!(
                abs_row(r, rows, scroll),
                want,
                "abs_row({r},{rows},{scroll})"
            );
            assert_eq!(
                screen_row(want, rows, scroll),
                Some(r),
                "screen_row({want})"
            );
        }
        // 画面の外は None
        assert_eq!(screen_row(5, 5, 0), None); // 1 行ぶん上 (古い)
        assert_eq!(screen_row(9, 5, 10), None); // 1 行ぶん下 (新しい)
        assert_eq!(screen_row(0, 0, 0), None); // 行数 0
                                               // 行数 0 でも panic しない
        assert_eq!(abs_row(0, 0, 3), 3);
    }

    #[test]
    fn clip_selection_clamps_ends_to_the_visible_window() {
        let rows = 5u16;
        // 全部見えている (絶対行 4 = 最上段 … 0 = 最下段)
        assert_eq!(
            clip_selection(((4, 2), (0, 7)), rows, 0),
            Some(((0, 2), (4, 7)))
        );
        // 逆順で渡しても同じ — 絶対行が「大きい」ほうが画面の上
        assert_eq!(
            clip_selection(((0, 7), (4, 2)), rows, 0),
            Some(((0, 2), (4, 7)))
        );
        // 上へはみ出した端は画面の一番上・列 0 で止める
        assert_eq!(
            clip_selection(((99, 2), (0, 7)), rows, 0),
            Some(((0, 0), (4, 7)))
        );
        // 下へはみ出した端は画面の一番下・列 u16::MAX で止める
        assert_eq!(
            clip_selection(((6, 2), (0, 7)), rows, 3),
            Some(((1, 2), (4, u16::MAX)))
        );
        // 交差しない (全部が画面より上 / 全部が画面より下)
        assert_eq!(clip_selection(((99, 0), (50, 0)), rows, 0), None);
        assert_eq!(clip_selection(((1, 0), (0, 0)), rows, 10), None);
        // 行数 0
        assert_eq!(clip_selection(((0, 0), (0, 0)), 0, 0), None);
    }

    #[test]
    fn autoscroll_step_grows_with_overshoot_and_is_capped() {
        let cell = 10.0_f32;
        // (はみ出し px, 期待行数)
        for (over, want) in [
            (1.0_f32, 1_i64),
            (10.0, 1),
            (11.0, 2),
            (30.0, 3),
            (1000.0, 8), // 上限 8 行 — 一瞬で履歴の端まで飛ばさない
        ] {
            assert_eq!(autoscroll_step(over, cell), want, "over={over}");
        }
        // 退化した入力でも 0 や負を返さない (返すと自動スクロールが止まる)
        assert_eq!(autoscroll_step(5.0, 0.0), 1);
        assert_eq!(autoscroll_step(-5.0, 10.0), 1);
        assert_eq!(autoscroll_step(f32::NAN, 10.0), 1);
    }

    #[test]
    fn 画面外ドラッグは自動スクロールで一画面を超えて伸びる() {
        // handle_mouse_selection と同じ手順を egui 抜きで踏む:
        // 「スクロールを先に適用 → その scroll で画面セルを絶対座標へ写す」。
        let rows = 5u16;
        let mut scroll = 0usize;
        let anchor = (abs_row(rows - 1, rows, scroll), 0u16); // 最下段で押した
        assert_eq!(anchor, (0, 0));
        let mut head = anchor;
        for _ in 0..12 {
            scroll += autoscroll_step(1.0, 10.0) as usize; // 上端の外へ 1 行ずつ
            head = (abs_row(0, rows, scroll), 3);
        }
        assert_eq!(head.0, 16);
        assert!(
            head.0 >= rows as usize,
            "一画面 ({rows} 行) を超えて伸びている: {}",
            head.0
        );
        // いま見えている範囲へ切り取ると、下端(アンカー)は画面外なので端で止まる
        assert_eq!(
            clip_selection((anchor, head), rows, scroll),
            Some(((0, 3), (rows - 1, u16::MAX)))
        );
    }

    #[test]
    fn 一画面を超える選択がコピーで全部取れる() {
        // 実 vt100::Parser に 30 行の履歴を作る (画面は 5 行しかない)。
        let (mut p, max_abs) = history(5, 20, 30);
        // 最古の行 (絶対行 max_abs) から最新の行 (絶対行 1) まで =
        // 30 行 = 6 画面ぶん。従来の selection_text は 5 行しか取れない。
        let text = selection_text_abs(&mut p, ((max_abs, 0), (1, 19)));
        let want = (0..30)
            .map(|i| format!("line{:03}", i))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text, want);
        assert_eq!(
            text.lines().count(),
            30,
            "一画面 (5 行) では収まらない行数が取れている"
        );
        // 呼び出し後は scrollback 位置が元 (0) に戻っている
        assert_eq!(p.screen().scrollback(), 0);
    }

    #[test]
    fn selection_text_abs_matches_selection_text_within_one_screen() {
        let (mut p, _) = history(5, 20, 30);
        // 画面内だけの選択 (絶対行 3..1 = 画面行 1..3) は従来と一致する
        let want = selection_text(p.screen(), ((1, 2), (3, 6)));
        assert_eq!(selection_text_abs(&mut p, ((3, 2), (1, 6))), want);
        // 端を逆順で渡しても同じ
        assert_eq!(selection_text_abs(&mut p, ((1, 6), (3, 2))), want);
        assert_eq!(p.screen().scrollback(), 0);
    }

    #[test]
    fn selection_text_abs_clamps_beyond_history() {
        let (mut p, max_abs) = history(5, 20, 30);
        // 履歴より古い行を指しても panic せず、最古の行から取れる
        let text = selection_text_abs(&mut p, ((max_abs + 500, 0), (1, 19)));
        assert!(text.starts_with("line000"), "先頭が最古の行: {text:?}");
        assert!(text.ends_with("line029"), "末尾が最新の行: {text:?}");
        assert_eq!(p.screen().scrollback(), 0);
    }

    #[test]
    fn session_select_all_covers_entire_screen() {
        let dir = std::env::current_dir().unwrap();
        let spec = super::SpawnSpec {
            title: "test".into(),
            command: "echo hello".into(),
            cwd: dir,
            env: std::collections::HashMap::new(),
            preset_name: String::new(),
            icon: "💬".into(),
            log_path: None,
        };
        let mut session = Session::spawn(9991, spec, eframe::egui::Context::default()).unwrap();
        session.select_all();
        assert!(session.selection.is_some());
    }

    #[test]
    fn selection_text_multiline_and_reversed() {
        let mut p = vt100::Parser::new(5, 20, 0);
        p.process(b"hello world\r\nsecond line");
        // 逆方向の選択(下→上)でも正しく取れる。行末の余白は落とす
        assert_eq!(
            selection_text(p.screen(), ((1, 5), (0, 6))),
            "world\nsecond"
        );
    }

    #[test]
    fn input_area_selection_claude_style_box() {
        // Claude Code 風: 上下罫線に挟まれた「› 本文」行
        let mut p = vt100::Parser::new(8, 30, 0);
        p.process(b"agent output\r\n");
        p.process("\x1b[3;1H------------------------------".as_bytes());
        p.process("\x1b[4;1H\u{203a} hello world".as_bytes());
        p.process("\x1b[5;1H------------------------------".as_bytes());
        p.process(b"\x1b[4;14H"); // カーソルは入力行の末尾
        let (sel, text) = input_area_selection(p.screen()).expect("入力欄が検出できる");
        assert_eq!(text, "hello world");
        assert_eq!(sel.0, (3, 2), "選択はマーカー直後から");
        assert_eq!(sel.1, (3, 12), "選択は本文の右端まで");
    }

    #[test]
    fn input_area_selection_multiline_input() {
        // 複数行入力: 2行目はマーカー幅ぶんインデントされる (Claude Code 方式)
        let mut p = vt100::Parser::new(8, 30, 0);
        p.process("\x1b[2;1H──────────".as_bytes());
        p.process("\x1b[3;1H\u{203a} 一行目の本文".as_bytes());
        p.process("\x1b[4;1H  二行目の本文".as_bytes());
        p.process("\x1b[5;1H──────────".as_bytes());
        p.process(b"\x1b[4;16H"); // カーソルは2行目側
        let (sel, text) = input_area_selection(p.screen()).expect("複数行でも検出できる");
        assert_eq!(text, "一行目の本文\n二行目の本文");
        assert_eq!((sel.0).0, 2);
        assert_eq!((sel.1).0, 3);
    }

    #[test]
    fn input_area_selection_gemini_style_side_borders() {
        // Gemini CLI 風: │ で囲まれた箱の中の「> 本文」
        let mut p = vt100::Parser::new(6, 20, 0);
        p.process("\x1b[2;1H\u{256d}──────────\u{256e}".as_bytes());
        p.process("\x1b[3;1H\u{2502} > draft  \u{2502}".as_bytes());
        p.process("\x1b[4;1H\u{2570}──────────\u{256f}".as_bytes());
        p.process(b"\x1b[3;10H");
        let (_, text) = input_area_selection(p.screen()).expect("枠付きでも検出できる");
        assert_eq!(text, "draft");
    }

    #[test]
    fn input_area_selection_shell_dollar_prompt() {
        let mut p = vt100::Parser::new(6, 30, 0);
        p.process(b"$ cargo build");
        let (_, text) = input_area_selection(p.screen()).expect("$ プロンプトも対象");
        assert_eq!(text, "cargo build");
    }

    #[test]
    fn input_area_selection_none_on_plain_output() {
        // マーカーの無い普通の出力画面では None → Ctrl+A は従来通り PTY へ
        let mut p = vt100::Parser::new(6, 20, 0);
        p.process(b"compiling foo\r\nfinished");
        assert!(input_area_selection(p.screen()).is_none());
        // zsh 既定風のプロンプト (マーカーが行頭に無い) も対象外
        let mut p2 = vt100::Parser::new(6, 40, 0);
        p2.process(b"tacyan@Mac dev % ls -la");
        assert!(input_area_selection(p2.screen()).is_none());
    }

    #[test]
    fn input_area_selection_empty_input_returns_none() {
        // 入力欄はあるが本文が空 → コピーするものが無いので None
        let mut p = vt100::Parser::new(6, 20, 0);
        p.process("\x1b[3;1H\u{203a} ".as_bytes());
        p.process(b"\x1b[3;3H");
        assert!(input_area_selection(p.screen()).is_none());
    }

    #[test]
    fn word_selection_expands_token() {
        let mut p = vt100::Parser::new(5, 20, 0);
        p.process(b"foo bar-baz qux");
        // "bar-baz" の途中をダブルクリック → 語全体
        assert_eq!(word_selection(p.screen(), 0, 5), Some(((0, 4), (0, 10))));
        // 空白の上は選択なし
        assert_eq!(word_selection(p.screen(), 0, 3), None);
    }

    // 子が POSIX シェル (stty/printf/read) 前提の実PTY e2e。Windows では
    // cmd 経由で別物になるため unix 限定 (spawn_prompt_session 系と同じ制約)。
    #[cfg(unix)]
    #[test]
    fn pet_bubble_approve_flow_e2e() {
        use super::{Attention, Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        // 実PTYで承認プロンプトを出して入力を待つ子プロセス
        let cmd = r#"printf 'Do you want to proceed? (y/n) '; read ans; if [ "$ans" = y ]; then echo PET_APPROVED_OK; fi"#;
        let spec = SpawnSpec {
            title: "pet-e2e".into(),
            preset_name: "test".into(),
            icon: "🐾".into(),
            command: cmd.into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut s = Session::spawn(999, spec, eframe::egui::Context::default()).expect("PTY起動");

        // 1) プロンプト検知で attention が立つ(= ペットバブルの表示条件)
        //    scan_attention は起動から900msスロットルされるためポーリングで待つ
        let mut detected = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(false), Some(Attention::NeedsApproval)) {
                detected = true;
                break;
            }
        }
        assert!(detected, "承認プロンプトが検知されなかった");
        assert!(s.attention);

        // 2) バブルの「✔ 承認」と同じ経路 (app.rs の BubbleAction::Approve 分岐)
        let keys = s
            .approve_reply()
            .map(str::to_string)
            .unwrap_or_else(|| "\r".into());
        assert_eq!(keys, "y\r", "(y/n) プロンプトには y+Enter を送るはず");
        assert!(s.send_text(&keys), "承認キーの送信に失敗");
        s.resolve_attention();
        assert!(!s.attention);

        // 3) 子プロセスが承認を受け取り、処理を進めて完了する
        let mut approved = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            let text = s.parser.lock().unwrap().screen().contents();
            if text.contains("PET_APPROVED_OK") {
                approved = true;
                break;
            }
        }
        assert!(approved, "承認後に子プロセスが進まなかった");
        s.kill();
    }

    // 子が POSIX シェル (stty/printf/read) 前提の実PTY e2e。Windows では
    // cmd 経由で別物になるため unix 限定 (spawn_prompt_session 系と同じ制約)。
    #[cfg(unix)]
    #[test]
    fn pet_bubble_approve_flow_antigravity() {
        use super::{Attention, Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        // Antigravity (agy) を想定した承認プロンプトを模したダミーのコマンド
        // "Antigravity: Allow execute this command? (y/n)" を出力し、入力を待つ
        let cmd = r#"printf 'Antigravity: Allow execute this command? (y/n) '; read ans; if [ "$ans" = y ]; then echo AGY_APPROVED_OK; fi"#;
        let spec = SpawnSpec {
            title: "pet-e2e-agy".into(),
            preset_name: "test".into(),
            icon: "🚀".into(),
            command: cmd.into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut s = Session::spawn(998, spec, eframe::egui::Context::default()).expect("PTY起動");

        // 1) プロンプト検知で attention が立つ
        let mut detected = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(false), Some(Attention::NeedsApproval)) {
                detected = true;
                break;
            }
        }
        assert!(detected, "Antigravityの承認プロンプトが検知されなかった");
        assert!(s.attention);

        // 2) 承認キー（y\r）の取得と送信
        let keys = s
            .approve_reply()
            .map(str::to_string)
            .unwrap_or_else(|| "\r".into());
        assert_eq!(
            keys, "y\r",
            "Antigravityの (y/n) プロンプトには y+Enter を送るはず"
        );
        assert!(s.send_text(&keys), "承認キーの送信に失敗");
        s.resolve_attention();
        assert!(!s.attention);

        // 3) 子プロセスが承認を受け取る
        let mut approved = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            let text = s.parser.lock().unwrap().screen().contents();
            if text.contains("AGY_APPROVED_OK") {
                approved = true;
                break;
            }
        }
        assert!(approved, "承認後に子プロセスが進まなかった");
        s.kill();
    }

    // ── 応答の一回完結(エピソード方式) ────────────────────────────────

    #[test]
    fn prompt_signature_keyed_by_content_not_position() {
        use super::prompt_signature;
        let a = "cmd: echo hi\nDo you want to proceed?\n❯ 1. Yes\n  2. No";
        // 上に古い出力が増えて行位置がずれても指紋は同じ(スクロール耐性)
        let scrolled = format!("older output\nmore output\n{a}");
        assert_eq!(prompt_signature(a), prompt_signature(&scrolled));
        // プロンプトの下に無関係の出力が増えても同じ
        let below = format!("{a}\nstreaming output…");
        assert_eq!(prompt_signature(a), prompt_signature(&below));
        // 直上のコマンドプレビューが違えば別のプロンプト(連続承認キューの区別)
        let other = a.replace("echo hi", "cargo test");
        assert_ne!(prompt_signature(a), prompt_signature(&other));
    }

    #[test]
    fn generic_question_signature_is_keyed_by_question_content() {
        use super::prompt_signature;

        let first = "output\nChoose the production target?";
        let same_with_history = "old output\noutput\nChoose the production target?";
        let next = "output\nRun the database migration?";
        assert_eq!(prompt_signature(first), prompt_signature(same_with_history));
        assert_ne!(prompt_signature(first), prompt_signature(next));
    }

    // 子が POSIX シェル (stty/printf/read) 前提の実PTY e2e。Windows では
    // cmd 経由で別物になるため unix 限定 (spawn_prompt_session 系と同じ制約)。
    #[cfg(unix)]
    #[test]
    fn auto_yes_replies_only_once_while_same_prompt_remains() {
        use super::{Attention, Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        // 入力を読まずにプロンプトを出しっぱなしにする子。TUI ダイアログ同様
        // エコー無し (以前は画面に残っている限り2秒おきに再送 → Enter連打事故)。
        // sleep は生かしておくためだけ (検知 + 4秒の監視窓を超えれば十分)。
        let cmd = r#"stty -echo; printf 'Do you want to proceed? (y/n) '; sleep 10"#;
        let spec = SpawnSpec {
            title: "one-shot-auto".into(),
            preset_name: "test".into(),
            icon: "⚡".into(),
            command: cmd.into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut s = Session::spawn(995, spec, eframe::egui::Context::default()).expect("PTY起動");

        let mut replies = 0u32;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(true), Some(Attention::AutoReplied(_))) {
                replies += 1;
                break;
            }
        }
        assert_eq!(replies, 1, "自動YESが送られなかった");

        // プロンプトは画面に残ったまま。4秒スキャンし続けても再送・再検出しない
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            match s.scan_attention(true) {
                Some(Attention::AutoReplied(_)) => replies += 1,
                Some(Attention::NeedsApproval) => panic!("応答済みプロンプトを再検出した"),
                _ => {}
            }
        }
        assert_eq!(
            replies, 1,
            "同じプロンプトへ自動YESが再送された(Enter連打バグ)"
        );
        assert!(
            !s.attention,
            "応答済みの間はバブル表示条件(attention)が立たない"
        );
        s.kill();
    }

    // 子が POSIX シェル (stty/printf/read) 前提の実PTY e2e。Windows では
    // cmd 経由で別物になるため unix 限定 (spawn_prompt_session 系と同じ制約)。
    #[cfg(unix)]
    #[test]
    fn auto_yes_presses_pet_approve_after_stall_timeout() {
        use super::{Attention, Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        // 自動YESの応答 (y\r) を無視してプロンプトが固まったままの子。
        // 「YESを送ったのに効かず 30 秒止まる」停滞を再現する (テストは 2 秒に短縮)。
        let cmd = r#"stty -echo; printf 'Do you want to proceed? (y/n) '; sleep 10"#;
        let spec = SpawnSpec {
            title: "stall-resend".into(),
            preset_name: "test".into(),
            icon: "⏳".into(),
            command: cmd.into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut s = Session::spawn(992, spec, eframe::egui::Context::default()).expect("PTY起動");
        s.auto_yes_resend_after = Duration::from_secs(2);

        // 1) 最初の自動YES
        let mut first = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(true), Some(Attention::AutoReplied(_))) {
                first = true;
                break;
            }
        }
        assert!(first, "最初の自動YESが送られなかった");

        // 2) 画面が固まったまま 2 秒経過 → ペットの承認操作へ切り替える
        let mut pet_approved = 0u32;
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(Attention::AutoReplied(desc)) = s.scan_attention(true) {
                assert!(desc.contains("ペットの承認ボタン"));
                pet_approved += 1;
                break;
            }
        }
        assert_eq!(pet_approved, 1, "停滞 2 秒後にペット承認が実行されなかった");
        assert!(
            s.auto_stall_since.is_none(),
            "ペット承認後も停滞監視が残った"
        );

        // 3) ペット承認後は、同じ画面が残っても再送しない
        let mut repeated = 0u32;
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(true), Some(Attention::AutoReplied(_))) {
                repeated += 1;
            }
        }
        assert_eq!(repeated, 0, "ペット承認後に同じプロンプトへ再送した");
        s.kill();
    }

    // 子が POSIX シェル (stty/printf/read) 前提の実PTY e2e。Windows では
    // cmd 経由で別物になるため unix 限定 (spawn_prompt_session 系と同じ制約)。
    #[cfg(unix)]
    #[test]
    fn auto_yes_visible_choice_is_received_by_child_process() {
        use super::{Attention, Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        // 実際の PTY に承認選択肢を表示して入力を待ち、自動 YES が届いた場合だけ
        // 成功マーカーを出す。分類だけでなく、画面検知→キー送信→子プロセス受信を通す。
        let cmd = r#"printf 'Do you want to execute this command?\n[y] Yes, approve\n[n] No\nChoice (y/n): '; read ans; if [ "$ans" = y ]; then echo AUTO_YES_E2E_APPROVED; else echo AUTO_YES_E2E_DENIED; fi"#;
        let spec = SpawnSpec {
            title: "auto-yes-visible-e2e".into(),
            preset_name: "test".into(),
            icon: "⚡".into(),
            command: cmd.into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut s = Session::spawn(994, spec, eframe::egui::Context::default()).expect("PTY起動");

        let mut auto_replied = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            let screen = s.parser.lock().unwrap().screen().contents();
            if screen.contains("Choice (y/n):") {
                eprintln!("承認前のPTY画面:\n{screen}");
            }
            if matches!(
                s.scan_attention(true),
                Some(Attention::AutoReplied("「y」"))
            ) {
                auto_replied = true;
                break;
            }
        }
        assert!(
            auto_replied,
            "表示された承認選択肢へ自動YESが送られなかった"
        );

        let mut approved = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            let text = s.parser.lock().unwrap().screen().contents();
            if text.contains("AUTO_YES_E2E_APPROVED") {
                eprintln!("自動YES受信後のPTY画面:\n{text}");
                approved = true;
                break;
            }
            assert!(
                !text.contains("AUTO_YES_E2E_DENIED"),
                "子プロセスがYES以外を受信した"
            );
        }
        assert!(
            approved,
            "子プロセスが自動YESを受信して承認処理を完了しなかった"
        );
        s.kill();
    }

    // 子が POSIX シェル (stty/printf/read) 前提の実PTY e2e。Windows では
    // cmd 経由で別物になるため unix 限定 (spawn_prompt_session 系と同じ制約)。
    #[cfg(unix)]
    #[test]
    fn disabled_auto_yes_leaves_visible_choice_waiting() {
        use super::{Attention, Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        let cmd = r#"printf 'Do you want to execute this command?\n[y] Yes, approve\n[n] No\nChoice (y/n): '; read ans; if [ "$ans" = y ]; then echo DISABLED_AUTO_YES_APPROVED; else echo DISABLED_AUTO_YES_DENIED; fi"#;
        let spec = SpawnSpec {
            title: "disabled-auto-yes-visible-e2e".into(),
            preset_name: "test".into(),
            icon: "🛡".into(),
            command: cmd.into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut s = Session::spawn(993, spec, eframe::egui::Context::default()).expect("PTY起動");

        let mut needs_approval = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(false), Some(Attention::NeedsApproval)) {
                needs_approval = true;
                break;
            }
        }
        assert!(
            needs_approval,
            "自動YESオフ時に承認待ちとして検知されなかった"
        );
        assert!(s.attention, "自動YESオフ時に承認通知条件が立たなかった");

        // 自動入力が誤送信されないことを、子プロセスを待たせたまま確認する。
        std::thread::sleep(Duration::from_millis(1_200));
        let screen = s.parser.lock().unwrap().screen().contents();
        eprintln!("自動YESオフで待機中のPTY画面:\n{screen}");
        assert!(screen.contains("Choice (y/n):"));
        assert!(!screen.contains("DISABLED_AUTO_YES_APPROVED"));
        assert!(!screen.contains("DISABLED_AUTO_YES_DENIED"));
        assert!(s.running(), "承認入力前に子プロセスが終了した");
        s.kill();
    }

    // 子が POSIX シェル (stty/printf/read) 前提の実PTY e2e。Windows では
    // cmd 経由で別物になるため unix 限定 (spawn_prompt_session 系と同じ制約)。
    #[cfg(unix)]
    #[test]
    fn resolve_attention_suppresses_same_prompt_redetection() {
        use super::{Attention, Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        let cmd = r#"stty -echo; printf 'Do you want to proceed? (y/n) '; sleep 10"#;
        let spec = SpawnSpec {
            title: "one-shot-deny".into(),
            preset_name: "test".into(),
            icon: "✖".into(),
            command: cmd.into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut s = Session::spawn(994, spec, eframe::egui::Context::default()).expect("PTY起動");

        let mut detected = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(false), Some(Attention::NeedsApproval)) {
                detected = true;
                break;
            }
        }
        assert!(detected, "承認プロンプトが検知されなかった");

        // バブルの「✖ 拒否」相当: Esc 送信 + resolve_attention
        assert!(s.send_text("\u{1b}"));
        s.resolve_attention();
        assert!(!s.attention);

        // プロンプトが画面に残っていても、バブルが再表示される条件へ戻らない
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                s.scan_attention(false).is_none(),
                "拒否済みプロンプトを再検出した(バブル再出現バグ)"
            );
            assert!(!s.attention);
        }
        s.kill();
    }

    // 子が POSIX シェル (stty/printf/read) 前提の実PTY e2e。Windows では
    // cmd 経由で別物になるため unix 限定 (spawn_prompt_session 系と同じ制約)。
    #[cfg(unix)]
    #[test]
    fn manual_typing_resolves_attention_episode() {
        use super::{Attention, Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        // 自動YESオフの手動運転: ユーザーが端末へ直接応答したら、プロンプト風
        // テキストが画面に残っていても承認待ちを引きずらない。引きずると
        // coordinator が WaitingApproval のまま配達を保留し続け、エージェント間の
        // やり取りが進まなくなる (2026-07-24 の手動運転バグ)。
        let cmd = r#"stty -echo; sleep 2; printf 'Do you want to proceed? (y/n) '; sleep 10"#;
        let spec = SpawnSpec {
            title: "manual-answer".into(),
            preset_name: "test".into(),
            icon: "⌨".into(),
            command: cmd.into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut s = Session::spawn(992, spec, eframe::egui::Context::default()).expect("PTY起動");

        // 1) プロンプトが出る前の手入力は user_typed を立てるだけで、
        //    後から出る本物のプロンプトの検知を抑止しない
        s.note_user_input();
        assert!(s.take_user_typed(), "手入力の印が立たなかった");

        let mut detected = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(false), Some(Attention::NeedsApproval)) {
                detected = true;
                break;
            }
        }
        assert!(detected, "手入力後に出た承認プロンプトが検知されなかった");
        assert!(s.attention);

        // 2) ユーザーが端末で直接応答した (terminal::draw のキーボード経路相当)。
        //    子は入力を読まないのでプロンプトは画面に残ったままになる。
        s.note_user_input();
        s.write_bytes(b"y\r");
        assert!(!s.attention, "手入力の応答後も承認待ちが残った");
        assert!(s.take_user_typed());

        // 3) 同じプロンプトが画面に残っていても、再検出して引きずらない
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                s.scan_attention(false).is_none(),
                "手入力で応答済みのプロンプトを再検出した(承認待ち引きずりバグ)"
            );
            assert!(!s.attention);
        }
        s.kill();
    }

    // 子が POSIX シェル (stty/printf/read) 前提の実PTY e2e。Windows では
    // cmd 経由で別物になるため unix 限定 (spawn_prompt_session 系と同じ制約)。
    #[cfg(unix)]
    #[test]
    fn next_prompt_with_different_signature_is_detected_again() {
        use super::{Attention, Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        // 連続承認キューを模す: 1つ目に応答済みでも、内容の異なる2つ目が
        // 現れたら(1つ目が画面から消えていなくても)新規プロンプトとして検出する
        let cmd = r#"stty -echo; printf 'cmd A\nDo you want to proceed? (y/n) '; sleep 4; printf '\ncmd B\nDo you want to proceed? (y/n) '; sleep 5"#;
        let spec = SpawnSpec {
            title: "queued-prompts".into(),
            preset_name: "test".into(),
            icon: "🔁".into(),
            command: cmd.into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut s = Session::spawn(993, spec, eframe::egui::Context::default()).expect("PTY起動");

        let mut detected = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(false), Some(Attention::NeedsApproval)) {
                detected = true;
                break;
            }
        }
        assert!(detected, "1つ目のプロンプトが検知されなかった");
        s.resolve_attention();

        let mut redetected = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(false), Some(Attention::NeedsApproval)) {
                redetected = true;
                break;
            }
        }
        assert!(
            redetected,
            "指紋の異なる2つ目のプロンプトが検知されなかった"
        );
        s.kill();
    }

    // ── 番号メニューの取りこぼし防止 / パーサ復旧 (セッション経由) ──────

    /// 答えの決まらない番号メニューでも「承認待ち」として看板に出る。
    /// ここが抜けると、自動YESが答えられない画面で**黙って止まる**。
    #[cfg(unix)]
    #[test]
    fn unanswerable_numbered_menu_surfaces_as_needs_approval() {
        use super::Attention;
        use std::time::Duration;

        let mut s = spawn_prompt_session(9401, "sleep 5");
        // 子の出力待ちに依存せず、画面を直接組み立てて判定だけを見る。
        let menu = "Which model do you want to use?\r\n\
                    1. opus\r\n2. sonnet\r\n3. haiku\r\nEnter a number (1-3): ";
        s.parser.lock().unwrap().process(menu.as_bytes());
        // scan_attention は 900ms のスロットルがある
        std::thread::sleep(Duration::from_millis(1_000));
        // 自動YESオンでも答えは決まらない → 承認待ちとして上がること
        assert!(
            matches!(s.scan_attention(true), Some(Attention::NeedsApproval)),
            "答えられない番号メニューが承認待ちにならなかった"
        );
        assert!(s.attention, "承認待ちフラグが立っていない");
        let screen = s.parser.lock().unwrap().screen().contents();
        assert!(
            screen.contains("Enter a number (1-3)"),
            "勝手に応答して画面が進んでしまった: {screen}"
        );
        s.kill();
    }

    /// 番号入力メニューへ数字を打つのは「30 秒 (テストでは短縮) 画面が
    /// まったく動かない」停滞時だけ。通常スキャンでは承認待ちに出すだけで、
    /// 数字は打たない (ユーザー報告の「数字が連続で入力される」の再発防止)。
    #[cfg(unix)]
    #[test]
    fn numbered_menu_digit_waits_for_the_stall_timeout() {
        use super::Attention;
        use std::time::{Duration, Instant};

        let mut s = spawn_prompt_session(9403, "sleep 10");
        s.auto_yes_resend_after = Duration::from_millis(600);
        let menu = "Allow this command to run?\r\n\
                    1. No, cancel\r\n2. Yes, allow this once\r\n\
                    Enter a number (1-2): ";
        s.parser.lock().unwrap().process(menu.as_bytes());
        std::thread::sleep(Duration::from_millis(1_000));

        // 1) 最初のスキャンは「承認待ち」を上げるだけ (数字は打たない)
        assert!(
            matches!(s.scan_attention(true), Some(Attention::NeedsApproval)),
            "番号メニューが承認待ちにならなかった"
        );
        // 2) 停滞閾値の前は何度スキャンしても撃たない
        for _ in 0..3 {
            s.last_scan = Instant::now() - Duration::from_millis(900);
            assert!(
                s.scan_attention(true).is_none(),
                "停滞閾値の前に番号メニューへ数字を打った"
            );
        }
        // 3) 画面が固まったまま閾値を超えたら、そこで初めて数字を打つ
        std::thread::sleep(Duration::from_millis(700));
        s.last_scan = Instant::now() - Duration::from_millis(900);
        match s.scan_attention(true) {
            Some(Attention::AutoReplied(desc)) => assert!(
                desc.contains('2'),
                "勝手に選んだ番号が説明に出ていない: {desc}"
            ),
            _ => panic!("停滞後も番号メニューに答えなかった"),
        }
        // 4) 打ったあとは同じ画面が残っても打ち直さない (数字の連打を作らない)
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(200));
            s.last_scan = Instant::now() - Duration::from_millis(900);
            assert!(
                !matches!(s.scan_attention(true), Some(Attention::AutoReplied(_))),
                "同じ番号メニューへ数字を再送した"
            );
        }
        s.kill();
    }

    /// T2: パーサの Mutex が poison しても、作り直して**必ず何か描ける**状態に戻す。
    /// 黙って真っ黒のままにせず、履歴が消えたことを 1 行のバナーで知らせる。
    #[cfg(unix)]
    #[test]
    fn poisoned_parser_is_rebuilt_once_with_a_notice() {
        let mut s = spawn_prompt_session(9402, "sleep 5");
        let p = s.parser.clone();
        // 別スレッドがパーサを握ったまま panic した状況を作る。
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // テスト出力を汚さない
        let _ = std::thread::spawn(move || {
            let _g = crate::lockx::lock_ok(&p);
            panic!("poison the parser");
        })
        .join();
        std::panic::set_hook(prev);
        assert!(s.parser.is_poisoned(), "前提: パーサが poison していない");

        s.ensure_parser_healthy();
        assert!(
            s.parser_rebuilt_notice.is_some(),
            "作り直しを黙って行った (ユーザーに知らせていない)"
        );
        let screen = s.parser.lock().unwrap().screen().contents();
        assert!(
            screen.contains("履歴は失われています"),
            "作り直しのバナーが画面に出ていない: {screen}"
        );
        assert!(
            !s.parser.is_poisoned(),
            "毒が落ちていない (毎フレーム作り直す)"
        );

        // 2 回目は何も起きない (バナーの連発・履歴の再消失をしない)。
        s.parser_rebuilt_notice = None;
        s.ensure_parser_healthy();
        assert!(s.parser_rebuilt_notice.is_none(), "健康なのに作り直した");
        s.kill();
    }

    // ── 900ms スキャンスロットルの境界 ────────────────────────────────

    /// 承認プロンプトを出す子を実PTYで起こす (unix 前提のテスト用ヘルパ)。
    #[cfg(unix)]
    fn spawn_prompt_session(id: u64, cmd: &str) -> super::Session {
        use super::{Session, SpawnSpec};
        use std::collections::HashMap;
        Session::spawn(
            id,
            SpawnSpec {
                title: "attention-e2e".into(),
                preset_name: "test".into(),
                icon: "⏱".into(),
                command: cmd.into(),
                cwd: std::env::temp_dir(),
                env: HashMap::new(),
                log_path: None,
            },
            eframe::egui::Context::default(),
        )
        .expect("PTY起動")
    }

    /// scan_attention を通さず、画面に needle が出るまで直接待つ。
    /// (スロットルの検証では「プロンプトは確実に見えている」状態から始めたい)
    #[cfg(unix)]
    fn wait_prompt_on_screen(s: &super::Session, needle: &str) {
        use std::time::Duration;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if s.parser
                .lock()
                .unwrap()
                .screen()
                .contents()
                .contains(needle)
            {
                return;
            }
        }
        panic!("プロンプト {needle:?} が画面に出なかった");
    }

    /// **配線の証明**: 実 PTY から届いた OSC 633 が [`Session`] のトラッカーへ入る。
    ///
    /// 見るのは「シェルが言った終了コード」だけで、画面は 1 文字も読まない。
    /// 逆に**画面には終了コードがどこにも書かれていない**ことも確かめる —
    /// これが「画面からは原理的に取れない情報を取っている」という主張の中身。
    #[cfg(unix)]
    #[test]
    fn シェル統合のマーカーが実ptyからトラッカーへ届く() {
        use std::time::Duration;
        // 長い sleep は書かない (プロセス残留の温床)。1 度出して終わるだけ。
        let cmd = r#"printf '\033]633;A\007\033]633;B\007\033]633;E;cargo test;n1\007\033]633;C\007out\r\n\033]633;D;3\007'"#;
        let s = spawn_prompt_session(983, cmd);
        let mut got = None;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(50));
            if let Some(c) = s.shell_recent(1).into_iter().next() {
                got = Some(c);
                break;
            }
        }
        let c = got.expect("D まで届かなかった (読取スレッドの配線が切れている)");
        assert_eq!(
            c.command_line, "cargo test",
            "コマンド行はシェルが直接教える"
        );
        assert_eq!(c.exit_code, Some(3));
        let (tier, n, log) = s.shell_status();
        assert_eq!(tier, crate::shellint::Tier::Rich);
        assert_eq!(n, 1);
        assert!(log.is_some(), "段の変化が記録されていない");
        let screen = super::lock_ok(&s.parser).screen().contents();
        assert!(
            !screen.contains('3') || !screen.contains("code"),
            "画面に終了コードが書かれているなら前提が崩れる: {screen:?}"
        );
        super::reap(s);
    }

    #[cfg(unix)]
    #[test]
    fn scan_throttle_blocks_under_900ms_and_adopts_at_boundary() {
        use super::Attention;
        use std::time::{Duration, Instant};

        // 入力を読まずにプロンプトを出しっぱなしにする子 (エコー無し)。
        let cmd = r#"stty -echo; printf 'Do you want to proceed? (y/n) '; sleep 5"#;
        let mut s = spawn_prompt_session(981, cmd);
        wait_prompt_on_screen(&s, "(y/n)");

        // 900ms 未満: プロンプトが画面に見えていても None のまま。
        // cur_hash (未読判定用の意味的ハッシュ) もスロットル中は更新されない。
        s.last_scan = Instant::now();
        assert!(
            s.scan_attention(false).is_none(),
            "スロットル中に検出された"
        );
        assert_eq!(s.cur_hash, 0, "スロットル中に cur_hash が更新された");
        assert!(!s.attention);

        // 850ms 相当でもまだ弾かれる (閾値 900ms の下側ブラケット)
        s.last_scan = Instant::now() - Duration::from_millis(850);
        assert!(s.scan_attention(false).is_none());
        assert_eq!(s.cur_hash, 0);

        // 900ms ちょうどで境界を通過 → 検出が採用され、ハッシュも動く
        s.last_scan = Instant::now() - Duration::from_millis(900);
        assert!(
            matches!(s.scan_attention(false), Some(Attention::NeedsApproval)),
            "境界通過のスキャンで検出されなかった"
        );
        assert!(s.attention);
        assert_ne!(
            s.cur_hash, 0,
            "採用されたスキャンで cur_hash が更新されるはず"
        );
        s.kill();
    }

    #[cfg(unix)]
    #[test]
    fn throttled_scan_does_not_extend_the_900ms_window() {
        use super::Attention;
        use std::time::{Duration, Instant};

        let cmd = r#"stty -echo; printf 'Do you want to proceed? (y/n) '; sleep 5"#;
        let mut s = spawn_prompt_session(982, cmd);
        wait_prompt_on_screen(&s, "(y/n)");

        // 窓の起点は「最後に採用されたスキャン」。弾かれた試行では動かない。
        s.last_scan = Instant::now() - Duration::from_millis(500);
        assert!(
            s.scan_attention(false).is_none(),
            "500ms 経過では弾かれるはず"
        );
        std::thread::sleep(Duration::from_millis(500));
        // 起点から計 ~1000ms。もし弾かれた試行が last_scan を更新していたら
        // まだ ~500ms しか経っていないことになり None のままになる。
        assert!(
            matches!(s.scan_attention(false), Some(Attention::NeedsApproval)),
            "弾かれた試行がスロットル窓を延長した"
        );
        s.kill();
    }

    // ── 自動YES 無効↔有効の切替 ──────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn enabling_auto_yes_replies_to_already_detected_prompt() {
        use super::Attention;
        use std::time::{Duration, Instant};

        let cmd = r#"stty -echo; printf 'Do you want to proceed? (y/n) '; sleep 10"#;
        let mut s = spawn_prompt_session(983, cmd);

        // 1) 自動YESオフで検出済み (バブル待ちの状態)
        let mut detected = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(false), Some(Attention::NeedsApproval)) {
                detected = true;
                break;
            }
        }
        assert!(detected, "オフのうちに承認プロンプトが検知されなかった");
        assert!(s.attention);

        // 2) オンへ切替 → 次の採用スキャンで同じプロンプトへ自動応答される
        s.last_scan = Instant::now() - Duration::from_millis(900);
        assert!(
            matches!(s.scan_attention(true), Some(Attention::AutoReplied(_))),
            "オン切替後の次スキャンで自動応答されなかった"
        );
        assert!(!s.attention, "自動応答後は承認待ちが解除される");

        // 3) 同じプロンプトが画面に残っていても応答は一度きり
        s.last_scan = Instant::now() - Duration::from_millis(900);
        assert!(s.scan_attention(true).is_none());
        s.kill();
    }

    #[cfg(unix)]
    #[test]
    fn disabling_auto_yes_after_reply_stops_redetection_and_resend() {
        use super::Attention;
        use std::time::{Duration, Instant};

        let cmd = r#"stty -echo; printf 'Do you want to proceed? (y/n) '; sleep 10"#;
        let mut s = spawn_prompt_session(984, cmd);
        // 停滞閾値を大きく超えさせても「オフなら再送しない」ことを見るため短縮。
        s.auto_yes_resend_after = Duration::from_millis(300);

        // 1) オンで自動応答済みにする
        let mut replied = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(true), Some(Attention::AutoReplied(_))) {
                replied = true;
                break;
            }
        }
        assert!(replied, "自動YESが送られなかった");

        // 2) オフへ切替。プロンプトは画面に残ったまま停滞閾値 (300ms) を超えるが、
        //    再検出 (NeedsApproval) も停滞再送 (AutoReplied) も起きない
        std::thread::sleep(Duration::from_millis(400));
        for _ in 0..8 {
            s.last_scan = Instant::now() - Duration::from_millis(900);
            match s.scan_attention(false) {
                Some(Attention::AutoReplied(_)) => panic!("オフ切替後に停滞再送された"),
                Some(Attention::NeedsApproval) => {
                    panic!("応答済みプロンプトをオフ切替後に再検出した")
                }
                _ => {}
            }
            assert!(!s.attention);
            std::thread::sleep(Duration::from_millis(100));
        }
        s.kill();
    }

    // ── 停滞再送の周期性 ─────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn stall_fallback_presses_pet_approve_only_once() {
        use super::Attention;
        use std::time::{Duration, Instant};

        // 応答 (y\r) を無視してプロンプトが固まったままの子。
        let cmd = r#"stty -echo; printf 'Do you want to proceed? (y/n) '; sleep 10"#;
        let mut s = spawn_prompt_session(985, cmd);
        s.auto_yes_resend_after = Duration::from_millis(600);

        // 1) 最初の自動YES
        let mut first = None;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(true), Some(Attention::AutoReplied(_))) {
                first = Some(Instant::now());
                break;
            }
        }
        let first = first.expect("最初の自動YESが送られなかった");

        // 2) 直後の採用スキャンでは再送しない (間隔未満の二重発火防止)
        s.last_scan = Instant::now() - Duration::from_millis(900);
        assert!(s.scan_attention(true).is_none(), "間隔未満で二重発火した");

        // 3) 間隔経過後にペット承認が発火する
        let mut second = None;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            s.last_scan = Instant::now() - Duration::from_millis(900);
            match s.scan_attention(true) {
                Some(Attention::AutoReplied(_)) => {
                    second = Some(Instant::now());
                    break;
                }
                Some(Attention::NeedsApproval) => panic!("応答済みプロンプトを再検出した"),
                _ => {}
            }
        }
        let second = second.expect("停滞後のペット承認が発火しなかった");
        assert!(
            second.duration_since(first) >= Duration::from_millis(550),
            "ペット承認の待機時間 (600ms) より早く発火した"
        );

        // 4) ペット承認直後は再発火しない
        s.last_scan = Instant::now() - Duration::from_millis(900);
        assert!(
            s.scan_attention(true).is_none(),
            "ペット承認直後に二重発火した"
        );

        // 5) さらに間隔が経っても同じプロンプトには再発火しない
        let mut third = false;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            s.last_scan = Instant::now() - Duration::from_millis(900);
            if matches!(s.scan_attention(true), Some(Attention::AutoReplied(_))) {
                third = true;
                break;
            }
        }
        assert!(!third, "ペット承認後に同じプロンプトへ再発火した");
        s.kill();
    }

    // ── 選択肢が画面外へスクロールした場合 ────────────────────────────

    #[cfg(unix)]
    #[test]
    fn prompt_scrolled_off_screen_clears_attention_without_events() {
        use super::Attention;
        use std::time::{Duration, Instant};

        // プロンプトを出して 3 秒待った後、行を氾濫させて選択肢を
        // 可視域 (30 行) の外へ追い出し、入力自体は待ち続ける子。
        let cmd = r#"stty -echo; printf 'Do you want to proceed? (y/n) '; sleep 3; i=0; while [ $i -lt 40 ]; do echo "filler-$i"; i=$((i+1)); done; sleep 5"#;
        let mut s = spawn_prompt_session(986, cmd);

        // 1) まず承認待ちとして検出される
        let mut detected = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(false), Some(Attention::NeedsApproval)) {
                detected = true;
                break;
            }
        }
        assert!(detected, "承認プロンプトが検知されなかった");
        assert!(s.attention);

        // 2) 氾濫でプロンプトが可視画面から消えるまで待つ
        let mut gone = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if !s
                .parser
                .lock()
                .unwrap()
                .screen()
                .contents()
                .contains("(y/n)")
            {
                gone = true;
                break;
            }
        }
        assert!(gone, "プロンプトが画面外へ流れなかった");

        // 3) 現挙動: 子はまだ入力を待っているが、選択肢が画面外に出ると
        //    次の採用スキャンで承認待ちは黙って解除され、イベントは出ない
        //    (解除を知らせる Attention は存在しない)。
        s.last_scan = Instant::now() - Duration::from_millis(900);
        assert!(
            s.scan_attention(false).is_none(),
            "画面外へ流れたプロンプトでイベントが出た"
        );
        assert!(
            !s.attention,
            "画面外に流れたプロンプトの承認待ちが解除されなかった"
        );

        // 4) 以後も再検出しない (画面に見えないものは検出対象外)
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(100));
            s.last_scan = Instant::now() - Duration::from_millis(900);
            assert!(s.scan_attention(false).is_none());
            assert!(!s.attention);
        }
        s.kill();
    }

    // ── 権限モード判定のカタログ経由ルーティング ──────────────────────

    /// 指定コマンド文字列で Session を1つ起こす。
    /// ここで見たいのは `self.command` から引く判定だけなので、
    /// 実際にそのバイナリが存在する必要は無い(shell が not found で終わるだけ)。
    fn probe_session(id: u64, command: &str) -> super::Session {
        probe_session_env(id, command, std::collections::HashMap::new())
    }

    fn probe_session_env(
        id: u64,
        command: &str,
        env: std::collections::HashMap<String, String>,
    ) -> super::Session {
        use super::{Session, SpawnSpec};
        Session::spawn(
            id,
            SpawnSpec {
                title: "probe".into(),
                preset_name: "probe".into(),
                icon: "🔍".into(),
                command: command.into(),
                cwd: std::env::temp_dir(),
                env,
                log_path: None,
            },
            eframe::egui::Context::default(),
        )
        .expect("PTY起動")
    }

    /// 絶対パス起動でもカタログに一致すること。
    /// 以前は先頭トークンを生で文字列比較していたため、`/usr/local/bin/claude`
    /// だと既存の claude / codex / agy ですら権限機能が全部死んでいた。
    #[test]
    fn absolute_path_command_head_resolves() {
        let mut s = probe_session(9101, "/usr/local/bin/claude --model opus");
        assert!(s.is_permission_agent(), "絶対パスの claude が認識されない");
        assert_eq!(s.permission_switch_keys(), Some(&b"\x1b[Z"[..]));
        assert_eq!(
            s.permission_switch_hint(),
            Some("権限モード切替 (Shift+Tab)")
        );
        s.kill();

        // 相対パス・~ 展開前の形・サブコマンド形式も同様
        let mut s = probe_session(9102, "~/.local/bin/agy -p");
        assert!(s.is_permission_agent());
        s.kill();
        let mut s = probe_session(9103, "./node_modules/.bin/codex exec");
        assert!(s.is_permission_agent());
        assert_eq!(s.permission_switch_keys(), Some(&b"/permissions\r"[..]));
        s.kill();
    }

    /// セッションから応答表への配線。別名で起動しても `agy` のルールが効くこと。
    ///
    /// `scan_attention` / `approve_reply` はここで得た `bin` 名で応答表を
    /// 絞り込むので、別名が正規化されないと Antigravity 用ルールが
    /// 丸ごと外れて自動YESが素通りする (今回の不具合そのもの)。
    #[test]
    fn antigravity_aliases_route_to_the_agy_prompt_rules() {
        for (i, cmd) in [
            "agy --dangerously-skip-permissions",
            "antigravity",
            "antigravity-cli --model gemini-3-pro",
            "/usr/local/bin/antigravity",
        ]
        .iter()
        .enumerate()
        {
            let mut s = probe_session(9300 + i as u64, cmd);
            assert_eq!(s.agent_bin(), Some("agy"), "{cmd} が agy に正規化されない");
            // 実物の agy の承認メニューに対し、このセッションで Enter が選ばれる。
            let screen = agy_menu(
                "Allow access to this file?",
                "Yes, allow access",
                "No, deny access",
            );
            assert_eq!(
                auto_yes_reply_for(&screen, s.agent_bin()).map(|(b, _)| b),
                Some(&b"\r"[..]),
                "{cmd}: Antigravity の承認プロンプトに応答できない"
            );
            s.kill();
        }
    }

    /// カタログに載った新しい CLI も権限エージェントとして認識される。
    #[test]
    fn new_catalog_agents_are_permission_agents() {
        for (i, cmd) in ["opencode", "copilot", "amp", "goose run", "aider"]
            .iter()
            .enumerate()
        {
            let mut s = probe_session(9200 + i as u64, cmd);
            assert!(s.is_permission_agent(), "{} が認識されない", cmd);
            s.kill();
        }
        // カタログ外は従来どおり対象外
        let mut s = probe_session(9250, "bash -lc ls");
        assert!(!s.is_permission_agent());
        s.kill();
    }

    /// 実機確認できていない CLI は切替キーを一切返さない。
    /// (生きたセッションへ当て推量のキーを撃ち込まないための安全性テスト)
    #[test]
    fn unverified_agents_expose_no_switch_keys() {
        for (i, cmd) in ["opencode", "goose run", "aider", "amp"].iter().enumerate() {
            let mut s = probe_session(9300 + i as u64, cmd);
            assert!(s.is_permission_agent(), "{}", cmd);
            assert_eq!(s.permission_switch_keys(), None, "{}", cmd);
            assert_eq!(s.permission_switch_hint(), None, "{}", cmd);
            s.kill();
        }
    }

    /// Ask モード起動は bypass 起動と判定しない(⚡バッジを誤表示しない)。
    #[test]
    fn bypass_launch_is_false_under_ask_for_new_agents() {
        use crate::agents::{apply_approval, Approval};
        for (i, bin) in ["opencode", "copilot", "amp", "claude", "codex", "goose"]
            .iter()
            .enumerate()
        {
            let cmd = apply_approval(bin, Approval::Ask);
            let mut s = probe_session(9400 + i as u64, &cmd);
            assert!(
                !s.launched_bypass,
                "Ask モードなのに bypass 起動と判定: {} -> {}",
                bin, cmd
            );
            s.kill();
        }
    }

    /// Auto モードなら新しい CLI でも bypass 起動と判定される(gap #3 の本体)。
    #[test]
    fn bypass_launch_is_true_under_auto_for_new_agents() {
        use crate::agents::{apply_approval, Approval};
        for (i, bin) in ["opencode", "copilot", "amp", "mimo"].iter().enumerate() {
            let cmd = apply_approval(bin, Approval::Auto);
            let mut s = probe_session(9500 + i as u64, &cmd);
            assert!(
                s.launched_bypass,
                "Auto モードが bypass 判定されない: {}",
                cmd
            );
            s.kill();
        }
    }

    /// 環境変数型 (goose / aider) の Auto も bypass 起動と判定される。
    /// フラグを持たないので `command_is_bypass` だけでは拾えない経路。
    #[test]
    fn bypass_launch_follows_auto_env_for_flagless_agents() {
        use crate::agents::{merged_env, Approval};
        use std::collections::HashMap;
        let empty = HashMap::new();
        for (i, bin) in ["goose", "aider"].iter().enumerate() {
            let auto = merged_env(bin, Approval::Auto, &empty);
            let mut s = probe_session_env(9600 + i as u64, bin, auto);
            assert!(s.launched_bypass, "{} の Auto が bypass 判定されない", bin);
            s.kill();

            let ask = merged_env(bin, Approval::Ask, &empty);
            let mut s = probe_session_env(9610 + i as u64, bin, ask);
            assert!(
                !s.launched_bypass,
                "{} の Ask が bypass 判定されている",
                bin
            );
            s.kill();
        }
    }

    /// メニューの自動YES (pet_auto_yes) は起動時の承認モードに依存しない。
    /// 以前は bypass 起動のみを対象にしていたため、Ask 起動のセッションでは
    /// 自動YESをオンにしても承認プロンプトが放置された(再発防止)。
    #[test]
    fn pet_auto_yes_covers_ask_launched_sessions() {
        use crate::agents::{apply_approval, Approval};
        let cmd = apply_approval("claude", Approval::Ask);
        let mut s = probe_session(9700, &cmd);
        assert!(
            s.auto_yes_target(true),
            "Ask 起動でも pet_auto_yes オンなら自動YESの対象"
        );
        assert!(
            !s.auto_yes_target(false),
            "pet_auto_yes オフでは自動応答しない"
        );
        s.kill();

        // カタログ外の素のコマンドは対象外(y/n プロンプトへ誤爆しない)
        let mut sh = probe_session(9701, "sleep 1");
        assert!(
            !sh.auto_yes_target(true),
            "カタログ外セッションは自動YESの対象外"
        );
        sh.kill();
    }
}

/// PTY で走らせるコマンドを組む。`cwd` は [`crate::pathx::launch_dir`] を通した
/// 素の実在ディレクトリであること (Windows では `\\?\` 付きだと cmd.exe が
/// カレントディレクトリを捨てて `C:\Windows` で起動してしまう)。
/// Windows: 起動するコマンド文字列を載せる環境変数の名前。
///
/// コマンドを**コマンドラインに書かない**ためにある (理由は
/// [`windows_shell_args`] を参照)。
#[cfg_attr(not(windows), allow(dead_code))]
const WINDOWS_CMD_ENV: &str = "ZAIVERN_CMD";

/// Windows で `cmd.exe` に渡す引数列。**コードページを UTF-8 (65001) に上げてから**
/// [`WINDOWS_CMD_ENV`] に入れたコマンドを起動する。
///
/// # なぜコードページを上げるのか
///
/// ConPTY の擬似コンソールは OS の OEM コードページ (日本語 Windows なら 932) で
/// 始まる。コンソールへ **UTF-8 のバイト列を直接書く**プログラム — git for Windows・
/// Go 製の CLI・MSYS 系ツールなど — の出力はその OEM として解釈されてから画面に
/// 落ちるので、日本語が軒並み化ける。エージェントの返答も進捗も読めなくなるが、
/// 原因は画面のどこにも出ない。`chcp 65001` を先に通せばコンソール自体が UTF-8 に
/// なり、以後そのセッションで起動する子プロセスにも効く。
///
/// # なぜコマンドを環境変数で渡すのか
///
/// `cmd.exe /C <コマンド>` と直接並べると、コマンドは 2 つの流儀で二重に解釈される:
/// `CommandBuilder` は C ランタイムの規則で `"` を `\"` へ逃がすが、**cmd は
/// `\"` を知らない**。そのため引用符を含むコマンド (空白入りのパス、
/// `--flag "日本語の指示"` など) がそのまま壊れる — ユーザー名やインストール先に
/// 空白があるだけで起動に失敗する。
///
/// 環境変数なら値はコマンドラインを経由しない (Windows の環境ブロックは UTF-16 で
/// 渡る) ので、引用符・空白・日本語を含む任意のコマンドがそのまま届く。
/// cmd は `%VAR%` の展開を `&` による分割よりも**先**に行うため、
/// `npm run build && claude` のような複合コマンドも構造を保ったまま実行される。
///
/// # 実装メモ
///
/// - 繋ぎは `&&` ではなく **`&`**。`chcp` が使えない環境でもエージェントの起動
///   そのものは続けたい (化けても起動しないより良い)。
/// - `>nul 2>nul` で「現在のコード ページ: 65001」の 1 行を隠す。
/// - コマンド無し (素のシェル) は `/K`。`/C` だと chcp 直後に cmd が終了して
///   端末が即座に閉じる。
// Windows 以外では使わないが、規則そのものはテストで固定しておく。
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_shell_args(has_command: bool) -> Vec<String> {
    const CHCP: &str = "chcp 65001 >nul 2>nul";
    if has_command {
        vec!["/C".to_string(), format!("{CHCP} & %{WINDOWS_CMD_ENV}%")]
    } else {
        vec!["/K".to_string(), CHCP.to_string()]
    }
}

/// Windows のコマンド文字列に含まれる `%NAME%` を先に解決する。
///
/// コマンドを環境変数経由で渡すと、cmd の展開は 1 回しか走らない
/// (変数の**値の中**にある `%NAME%` はもう展開されない)。
/// プリセットに `%USERPROFILE%\bin\tool.exe` と書いていた人がいるので、
/// 直接並べていた頃と同じ結果になるよう、ここで自分で解決しておく。
///
/// 見つからない名前は cmd と同じく**そのまま残す** (`50%` のような素の `%` も壊さない)。
/// 探す順はプリセットの env → プロセスの env (プリセットで上書きできるようにする)。
#[cfg_attr(not(windows), allow(dead_code))]
fn expand_windows_env_refs(command: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(command.len());
    let mut rest = command;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        // 変数名は次の `%` まで。改行や `%` を挟まないものだけを名前として扱う。
        match after.find('%') {
            Some(end) if end > 0 && !after[..end].contains('\n') => {
                let name = &after[..end];
                match lookup(name) {
                    Some(v) => out.push_str(&v),
                    // 未定義ならそのまま (cmd も残す)
                    None => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            // 閉じが無い / `%%` → 素の `%` として残す
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn build_command(command: &str, cwd: &Path, env: &HashMap<String, String>) -> CommandBuilder {
    let has_command = !command.trim().is_empty();
    // シェル統合 (OSC 633) の注入計画。**オプトインで、コマンド指定が無いとき
    // だけ**返る。無効なら None なので、以下の分岐は導入前と 1 バイトも変わらない。
    #[cfg(not(windows))]
    let shell_program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    // Windows の既定 (cmd.exe) にシェル統合の手段は無い — VS Code も同じ判断。
    // COMSPEC を pwsh へ向けている人だけがここで拾われる。
    #[cfg(windows)]
    let shell_program = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
    let plan = crate::shellint::launch_plan(&shell_program, has_command);

    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = CommandBuilder::new(&shell_program);
        match &plan {
            Some(p) => {
                for a in &p.args {
                    c.arg(a);
                }
            }
            None if has_command => {
                c.arg("-lc");
                c.arg(command);
            }
            None => {
                c.arg("-l");
            }
        }
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = CommandBuilder::new(&shell_program);
        match &plan {
            Some(p) => {
                for a in &p.args {
                    c.arg(a);
                }
            }
            None => {
                for a in windows_shell_args(has_command) {
                    c.arg(a);
                }
            }
        }
        c
    };
    cmd.cwd(cwd);
    // ユーザーのシェルが持つ PATH を渡す。macOS の `.app` 起動では PATH が
    // launchd の最小構成になっており、しかも `-lc` のログインシェルは
    // `.zshrc` / `.bashrc` を読まないので、これが無いと `claude` などが
    // command not found になる (shellenv の説明を参照)。
    // プリセット側の env は後で当てるので、ユーザーが PATH を書いていればそちらが勝つ。
    cmd.env("PATH", crate::shellenv::user_path());
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("ZAIVERN", "1");
    // エージェント PTY の印。プロンプトフレームワーク (bash-preexec /
    // 自作 PROMPT_COMMAND / bash-git-prompt) がエージェント端末を固める問題へ、
    // 「エージェントのシェルだけ切る」逃げ道を渡す (VS Code の VSCODE_AGENT 相当)。
    // エージェントが起動する孫シェルまで環境として伝わるのが要点。
    if has_command {
        for (k, v) in crate::shellint::agent_env() {
            cmd.env(k, v);
        }
    }
    if let Some(p) = &plan {
        for (k, v) in &p.env {
            cmd.env(k, v);
        }
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    // 起動するコマンドは最後に載せる (プリセットの env に同名があっても、
    // 実際に走るのは選ばれたエージェントのコマンドであること)。
    // 値はコマンドラインを通らないので、引用符・空白・日本語のまま届く。
    #[cfg(windows)]
    {
        let command = command.trim();
        if !command.is_empty() {
            let expanded = expand_windows_env_refs(command, &|name: &str| {
                env.get(name).cloned().or_else(|| std::env::var(name).ok())
            });
            cmd.env(WINDOWS_CMD_ENV, expanded);
        }
    }
    cmd
}

fn key_bytes(key: egui::Key, m: egui::Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    use egui::Key as K;
    let arrow = |c: u8| -> Vec<u8> {
        if app_cursor {
            vec![0x1b, b'O', c]
        } else {
            vec![0x1b, b'[', c]
        }
    };
    let b = match key {
        K::Enter => {
            if m.shift || m.alt {
                b"\x1b\r".to_vec()
            } else {
                b"\r".to_vec()
            }
        }
        K::Tab => {
            if m.shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        // ⌥⌫ = 直前の単語を削除 (readline: ESC DEL = backward-kill-word)
        K::Backspace => {
            if m.alt {
                vec![0x1b, 0x7f]
            } else {
                vec![0x7f]
            }
        }
        K::Escape => vec![0x1b],
        K::ArrowUp => arrow(b'A'),
        K::ArrowDown => arrow(b'B'),
        // ⌥←/⌥→ = 単語単位の移動 (readline: ESC b / ESC f)
        K::ArrowRight => {
            if m.alt {
                b"\x1bf".to_vec()
            } else {
                arrow(b'C')
            }
        }
        K::ArrowLeft => {
            if m.alt {
                b"\x1bb".to_vec()
            } else {
                arrow(b'D')
            }
        }
        K::Home => b"\x1b[H".to_vec(),
        K::End => b"\x1b[F".to_vec(),
        K::PageUp => b"\x1b[5~".to_vec(),
        K::PageDown => b"\x1b[6~".to_vec(),
        K::Delete => b"\x1b[3~".to_vec(),
        _ => {
            if m.ctrl && !m.alt {
                let name = key.name();
                if name.len() == 1 {
                    let ch = name.as_bytes()[0].to_ascii_lowercase();
                    if ch.is_ascii_lowercase() {
                        return Some(vec![ch - b'a' + 1]);
                    }
                }
            }
            return None;
        }
    };
    Some(b)
}

/// macOS の標準編集ショートカットを、エージェント側の入力欄 (readline 系
/// CLI) が理解する Control / ESC シーケンスへ変換する。Command 系を一括
/// 転送するとアプリ全体のショートカットまで PTY に漏れるため、対応対象を
/// 明示的に限定する。⌘A はここでは扱わない (PTY へ送らずローカル全選択)。
fn mac_agent_input_bytes(key: egui::Key, m: egui::Modifiers) -> Option<&'static [u8]> {
    use egui::Key as K;
    if !m.mac_cmd {
        return None;
    }
    match key {
        K::ArrowLeft => Some(b"\x01"),  // ⌘← = 行頭 (Ctrl+A)
        K::ArrowRight => Some(b"\x05"), // ⌘→ = 行末 (Ctrl+E)
        K::Backspace => Some(b"\x15"),  // ⌘⌫ = 行頭まで削除 (Ctrl+U)
        K::K => Some(b"\x0c"),          // ⌘K = 画面クリア (Ctrl+L 再描画)
        _ => None,
    }
}

/// 端末内検索バー (Cmd+F)。端末の右上に浮かせて表示する。
fn terminal_search_bar_ui(
    ui: &mut egui::Ui,
    session: &mut Session,
    theme: &Theme,
    rect: egui::Rect,
) {
    let area_id = egui::Id::new(("zv-term-search", session.id));
    let pos = egui::pos2(
        (rect.right() - 330.0).max(rect.left() + 4.0),
        rect.top() + 6.0,
    );
    egui::Area::new(area_id)
        .fixed_pos(pos)
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    let te = ui.add(
                        egui::TextEdit::singleline(&mut session.search.query)
                            .desired_width(150.0)
                            .hint_text(tr("端末内を検索…")),
                    );
                    if session.search.focus_pending {
                        te.request_focus();
                        session.search.focus_pending = false;
                    }
                    let (enter, shift) =
                        ui.input(|i| (i.key_pressed(egui::Key::Enter), i.modifiers.shift));
                    if te.lost_focus() && enter {
                        // Enter = 前 (古い方) へ / Shift+Enter = 次 (新しい方) へ
                        session.search_step(shift);
                        te.request_focus();
                    } else if te.changed() {
                        // 打つたびに検索し直す (起点は最新ヒットへリセット)
                        session.search.hit_line = None;
                        session.search_step(false);
                    }
                    let count = if session.search.total > 0 {
                        format!("{}/{}", session.search.index, session.search.total)
                    } else if session.search.query.is_empty() {
                        String::new()
                    } else {
                        tr("0件")
                    };
                    ui.label(egui::RichText::new(count).size(11.0).color(theme.text_dim));
                    if ui
                        .small_button("▲")
                        .on_hover_text(tr("前 (古い方) へ (Enter)"))
                        .clicked()
                    {
                        session.search_step(false);
                    }
                    if ui
                        .small_button("▼")
                        .on_hover_text(tr("次 (新しい方) へ (Shift+Enter)"))
                        .clicked()
                    {
                        session.search_step(true);
                    }
                    let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
                    if ui.small_button("✕").clicked() || esc {
                        session.search.open = false;
                        session.search.hit_line = None;
                        session.search.index = 0;
                        session.search.total = 0;
                        session.search.current_vis = None;
                        session.set_scroll(0);
                    }
                });
            });
        });
}

/// 表示中画面の検索ヒットに半透明ハイライトを重ねる。
/// ワイド文字 (CJK 等) は 2 セル幅として扱う。
fn paint_search_highlights(
    painter: &egui::Painter,
    session: &Session,
    theme: &Theme,
    rect: egui::Rect,
    padding: f32,
    cell_w: f32,
    cell_h: f32,
) {
    let q: Vec<char> = session
        .search
        .query
        .chars()
        .flat_map(|c| c.to_lowercase())
        .collect();
    if q.is_empty() {
        return;
    }
    let p = lock_ok(&session.parser);
    let screen = p.screen();
    let (rows, cols) = screen.size();
    let fill = theme.warn.gamma_multiply(0.30);
    let stroke = egui::Stroke::new(1.0_f32, theme.warn.gamma_multiply(0.75));
    // 現在ヒット行はより濃く強調する (ジャンプ後に scroll が動いたら通常表示)
    let cur_row = session
        .search
        .current_vis
        .and_then(|(s, r)| (s == session.scroll).then_some(r));
    let cur_fill = theme.warn.gamma_multiply(0.55);
    let cur_stroke = egui::Stroke::new(1.5_f32, theme.warn);
    for row in 0..rows {
        // 行の小文字化文字列と「文字 → (セル列, セル幅)」対応表を作る
        let mut chars: Vec<char> = Vec::new();
        let mut colmap: Vec<(u16, u16)> = Vec::new();
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let w = cell_draw_cols(screen, row, col);
            let s = cell.contents();
            if s.is_empty() {
                chars.push(' ');
                colmap.push((col, 1));
            } else {
                for ch in s.chars().flat_map(|c| c.to_lowercase()) {
                    chars.push(ch);
                    colmap.push((col, w));
                }
            }
        }
        if chars.len() < q.len() {
            continue;
        }
        let mut i = 0;
        while i + q.len() <= chars.len() {
            if chars[i..i + q.len()] == q[..] {
                let (c0, _) = colmap[i];
                let (c1, w1) = colmap[i + q.len() - 1];
                let x0 = rect.min.x + padding + f32::from(c0) * cell_w;
                let x1 = rect.min.x + padding + f32::from(c1 + w1) * cell_w;
                let y0 = rect.min.y + padding + f32::from(row) * cell_h;
                let r = egui::Rect::from_min_max(
                    egui::pos2(x0, y0),
                    egui::pos2(x1.min(rect.max.x), (y0 + cell_h).min(rect.max.y)),
                );
                if cur_row == Some(row) {
                    painter.rect(r, 2.0, cur_fill, cur_stroke);
                } else {
                    painter.rect(r, 2.0, fill, stroke);
                }
                i += q.len();
            } else {
                i += 1;
            }
        }
    }
}

/// スクロールバック全体 + 現在画面の全行を絶対行順 (最古が先頭) で取り出す。
/// 呼び出し中だけ scrollback 位置を動かし、終わったら必ず元の位置へ戻す。
fn all_terminal_lines(p: &mut vt100::Parser) -> Vec<String> {
    let saved = p.screen().scrollback();
    p.set_scrollback(usize::MAX);
    let top = p.screen().scrollback();
    let (rows, cols) = p.screen().size();
    if rows == 0 {
        p.set_scrollback(saved);
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::with_capacity(top + rows as usize);
    loop {
        // 窓の先頭が「次に読みたい絶対行」に来るよう戻り量を選ぶ
        let o = top.saturating_sub(lines.len());
        p.set_scrollback(o);
        let start = top - p.screen().scrollback();
        for (r, row) in p.screen().rows(0, cols).enumerate() {
            if start + r == lines.len() {
                lines.push(row);
            }
        }
        if o == 0 {
            break;
        }
    }
    p.set_scrollback(saved);
    lines
}

/// query を含む行番号を列挙する (大文字小文字は区別しない)。
fn line_hits(lines: &[String], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

/// ヒット行が画面中央あたりに来る scrollback 量を計算する。
fn search_scroll_target(hit: usize, total_lines: usize, rows: usize) -> usize {
    let top = total_lines.saturating_sub(rows); // 最大の戻り量
    let want_start = hit.saturating_sub(rows / 2); // 窓の先頭に置きたい絶対行
    top.saturating_sub(want_start.min(top))
}

fn ansi_color(theme: &Theme, i: u8) -> egui::Color32 {
    if i < 16 {
        theme.ansi[i as usize]
    } else if i < 232 {
        let i = i - 16;
        let f = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        egui::Color32::from_rgb(f(i / 36), f((i % 36) / 6), f(i % 6))
    } else {
        let v = 8 + (i - 232) * 10;
        egui::Color32::from_rgb(v, v, v)
    }
}

fn cell_color(theme: &Theme, c: vt100::Color, is_fg: bool) -> egui::Color32 {
    match c {
        vt100::Color::Default => {
            if is_fg {
                theme.term_fg
            } else {
                theme.term_bg
            }
        }
        vt100::Color::Idx(i) => ansi_color(theme, i),
        vt100::Color::Rgb(r, g, b) => egui::Color32::from_rgb(r, g, b),
    }
}

fn brighten(c: egui::Color32) -> egui::Color32 {
    egui::Color32::from_rgb(
        c.r().saturating_add(45),
        c.g().saturating_add(45),
        c.b().saturating_add(45),
    )
}

// ─── 文字選択(マウスドラッグでコピー) ────────────────────────────

/// 画面行 (0 = 最上段) を「生きている画面の下端から数えた絶対行」へ写す。
///
/// scroll (スクロールバックの戻り量) が動いても、同じ文字は同じ絶対行を指す。
/// これが無いと選択は画面座標のままなので、一画面を超えて伸ばせない。
pub fn abs_row(r: u16, rows: u16, scroll: usize) -> usize {
    scroll + (rows.saturating_sub(1).saturating_sub(r)) as usize
}

/// 絶対行を今の画面行へ戻す。画面の外なら None。
pub fn screen_row(abs: usize, rows: u16, scroll: usize) -> Option<u16> {
    if rows == 0 {
        return None;
    }
    let d = abs.checked_sub(scroll)?;
    if d >= rows as usize {
        return None;
    }
    Some(rows - 1 - d as u16)
}

/// 絶対座標の選択を、いま見えている画面の座標へ切り取る。
///
/// 画面外へはみ出した端は画面の端で止める。まったく見えていなければ None。
/// 画面の並び順は「絶対行が**大きい**ほうが上」なので、行の比較は
/// `Reverse` を噛ませる (素の `<=` で並べると上下が入れ替わる)。
pub fn clip_selection(
    sel: ((usize, u16), (usize, u16)),
    rows: u16,
    scroll: usize,
) -> Option<((u16, u16), (u16, u16))> {
    if rows == 0 {
        return None;
    }
    let key = |p: (usize, u16)| (std::cmp::Reverse(p.0), p.1);
    let (start, end) = if key(sel.0) <= key(sel.1) {
        (sel.0, sel.1)
    } else {
        (sel.1, sel.0)
    };
    // start = 上端 (絶対行が大きい) / end = 下端 (絶対行が小さい)
    let win_top = scroll + rows as usize - 1;
    if end.0 > win_top || start.0 < scroll {
        return None; // 可視窓 [scroll, win_top] と交差しない
    }
    // はみ出した上端は列 0、はみ出した下端は列 u16::MAX でクランプする
    // (selection_text が e.1.min(last_col) するので MAX でも安全)。
    let top = match screen_row(start.0, rows, scroll) {
        Some(r) => (r, start.1),
        None => (0, 0),
    };
    let bottom = match screen_row(end.0, rows, scroll) {
        Some(r) => (r, end.1),
        None => (rows - 1, u16::MAX),
    };
    Some((top, bottom))
}

// ─── シェル統合の通し番号 (OSC 133 の行を記録できる形にする) ──────────────

/// 押し出された行数を数えるための観測点。
///
/// # なぜ数える必要があるのか
///
/// [`abs_row`] の座標は**生きている画面の下端起点**なので、出力が 1 行増える
/// たびに同じ文字を指す値が変わる。`shellint` が記録するのは逆向きの
/// **単調非減少な通し番号**で、両者は `通し番号 = 押し出した総数 + 画面行`
/// で結ばれる。つまり必要なのは「押し出した総数」ただ 1 つ。
///
/// vt100 はそれをどこにも残さない。数えられるのは 2 つだけ —
/// 1. **履歴の長さの伸び**。容量に達するまでは押し出した行数そのもの。
/// 2. **戻り量の伸び**。vt100 は履歴を戻して見ている間、同じ文字が同じ位置に
///    見えるよう 1 スクロールごとに戻り量を +1 する。ただし
///    **0 のときは加算しない**実装なので、測る側が 1 以上へ留める必要がある。
///
/// 容量に達したあとは 1 が 0 になるので 2 だけが効く。逆に履歴が空のうちは
/// 2 が効かないので 1 だけが効く。**両方を取って大きいほうを採る。**
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ScrollProbe {
    /// 履歴に入っている行数 = `min(押し出した総数, 容量)`。
    pub sb_len: usize,
    /// いま何行戻して見ているか。
    pub offset: usize,
    /// 代替画面か。代替画面のグリッドは履歴を持たない (容量 0) ので、
    /// ここで測った 0 を通常画面の値と引き算すると桁違いの嘘が出る。
    pub alt: bool,
}

/// 2 つの観測点から**押し出された行数**を出す (純関数)。
///
/// `before` は**戻り量を 1 以上へ留めたあと**に取ること (0 のままだと
/// vt100 が数えてくれない)。留めた量が `pinned` なら、飽和するまでの
/// 余裕は `容量 - pinned` — だから留める値は小さいほどよい。
pub fn scrolled_delta(before: ScrollProbe, after: ScrollProbe) -> u64 {
    // 代替画面は履歴を持たない。出入りの瞬間に引き算すると、抜けた瞬間に
    // 「通常画面の履歴長ぶん一気にスクロールした」という嘘が出る。
    // 代替画面の中ではシェルのマーカーは出ないので、数えないで困らない。
    if before.alt || after.alt {
        return 0;
    }
    let by_len = after.sb_len.saturating_sub(before.sb_len);
    let by_off = after.offset.saturating_sub(before.offset);
    by_len.max(by_off) as u64
}

/// パーサを 1 回動かし、**押し出された行数**と動かしたあとの履歴長を返す。
///
/// 呼び出し前の戻り量は必ず元へ戻す — ただし「vt100 が自分で動かしたときと
/// 同じ値」へ戻す (0 = 最新に貼り付いたまま / 1 以上 = 同じ文字を見続ける)。
/// ここを素の元値へ戻すと、履歴を見ている最中に出力が来たとき画面が流れる。
fn count_around<R>(
    p: &mut vt100::Parser,
    f: impl FnOnce(&mut vt100::Parser) -> R,
) -> (R, u64, usize) {
    let saved = p.screen().scrollback();
    // 履歴の行数は「戻れる上限」として読める。
    p.set_scrollback(usize::MAX);
    let sb_len = p.screen().scrollback();
    // 1 へ留める (履歴が空なら 0 のまま = 長さの伸びだけで数える)。
    p.set_scrollback(1);
    let before = ScrollProbe {
        sb_len,
        offset: p.screen().scrollback(),
        alt: p.screen().alternate_screen(),
    };
    let out = f(p);
    // 長さを測る前に戻り量を読む (set_scrollback が上書きしてしまう)。
    let offset = p.screen().scrollback();
    let alt = p.screen().alternate_screen();
    p.set_scrollback(usize::MAX);
    let after = ScrollProbe {
        sb_len: p.screen().scrollback(),
        offset,
        alt,
    };
    let moved = scrolled_delta(before, after);
    p.set_scrollback(if saved == 0 {
        0
    } else {
        saved.saturating_add(moved as usize)
    });
    (out, moved, after.sb_len)
}

/// シェル統合マーカー (OSC 133 / 633) の**終端の次**の位置 (純関数)。
///
/// ここでバイト列を切ってから vt100 へ流すと、マーカーを見た瞬間の
/// カーソル位置がそのまま「マーカーの行」になる。切らないと
/// 「`C` + 出力 8000 行」が 1 回の read で届いたとき、出力の**末尾**を
/// 出力の**先頭**として記録してしまう。
///
/// マーカーが 1 つも無ければ空 = 分割なし。**シェル統合を入れていない
/// 利用者は 1 バイトも違う経路を通らない。**
pub fn shell_marker_cuts(bytes: &[u8]) -> Vec<usize> {
    let mut cuts = Vec::new();
    let mut i = 0usize;
    while i + 6 <= bytes.len() {
        if bytes[i] != 0x1b || bytes[i + 1] != b']' {
            i += 1;
            continue;
        }
        let ps = &bytes[i + 2..i + 6];
        if ps != b"133;" && ps != b"633;" {
            i += 1;
            continue;
        }
        // 終端は BEL か ST (ESC \)。この塊の中で閉じていなければ切らない
        // (途中で切ると行が 1 つ手前へずれる。次の read で拾い直せばよい)。
        let mut j = i + 6;
        let end = loop {
            match bytes.get(j) {
                None => break None,
                Some(0x07) => break Some(j + 1),
                Some(0x1b) => {
                    break if bytes.get(j + 1) == Some(&b'\\') {
                        Some(j + 2)
                    } else {
                        None
                    }
                }
                Some(_) => j += 1,
            }
        };
        match end {
            Some(e) => {
                cuts.push(e);
                i = e;
            }
            None => break,
        }
    }
    cuts
}

/// [`shell_marker_cuts`] の切れ目でバイト列を区間へ分ける (純関数)。
///
/// 切れ目が無ければ**元の 1 本をそのまま**返す。
pub fn shell_segments(bytes: &[u8]) -> Vec<&[u8]> {
    let cuts = shell_marker_cuts(bytes);
    if cuts.is_empty() {
        return vec![bytes];
    }
    let mut out = Vec::with_capacity(cuts.len() + 1);
    let mut from = 0usize;
    for c in cuts {
        out.push(&bytes[from..c]);
        from = c;
    }
    if from < bytes.len() {
        out.push(&bytes[from..]);
    }
    out
}

/// 上端に固定表示する 1 行 (ターミナルの sticky scroll)。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShellSticky {
    /// 1 行へ丸めたコマンド行。
    pub text: String,
    /// 成功したか。終了コード不明なら `None` (印を付けない)。
    pub ok: Option<bool>,
    /// 帯を押したときの着地点 (プロンプト行の通し番号)。
    pub prompt: u64,
    /// まだ実行中か (下へ開いたまま)。
    pub running: bool,
}

/// 画面に見えている 1 コマンドの印 (VS Code の command decoration)。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShellMark {
    /// 印を置く画面行 (0 = 最上段)。
    pub row: u16,
    /// 成功したか。`None` は終了コード不明 = **印を付けない**。
    pub ok: Option<bool>,
    /// 打たれたコマンド行 (空なら「コマンド行を知らない段」)。
    pub command: String,
    /// ホバー/メニューに出す要約。
    pub summary: String,
    /// 出力の通し番号の範囲 `(先頭, 末尾)`。まだ出力が始まっていなければ `None`。
    pub output: Option<(u64, u64)>,
    /// まだ実行中か (終了コードが来ていない = 下へ開いたまま)。
    pub running: bool,
}

/// vt100 に持たせるスクロールバックの行数 (履歴の容量)。
pub const SCROLLBACK_ROWS: usize = 5000;

/// 1 回の [`vt100::Parser::process`] へ渡すバイト数の上限。
///
/// [`count_around`] が数えられるのは**高々「容量 - 留めた量」行**まで
/// (それ以上は vt100 が古い行を捨てて痕跡が消える)。1 バイトが最大 1 行を
/// 押し出すので、**容量より小さく刻めば数え損ねない。**
/// `FEED_CHUNK < SCROLLBACK_ROWS - 1` が成り立つことを
/// `feed_chunk_fits_in_scrollback` が固定する。
const FEED_CHUNK: usize = 4096;

/// 端末の**通し番号**。読取スレッドが書き、描画が読む。
///
/// `scrolled` は「画面の上へ押し出した総行数」。画面行 `r` (0 = 最上段) の
/// 通し番号は `scrolled - 戻り量 + r`、履歴の最古は `scrolled - sb_len`。
#[derive(Debug, Default)]
pub struct LineIndex {
    scrolled: AtomicU64,
    sb_len: AtomicU64,
}

impl LineIndex {
    /// 押し出した総行数。
    pub fn scrolled(&self) -> u64 {
        self.scrolled.load(Ordering::Relaxed)
    }

    /// まだ読める最も古い通し番号。これより古い行はもう存在しない。
    pub fn oldest_live(&self) -> u64 {
        self.scrolled()
            .saturating_sub(self.sb_len.load(Ordering::Relaxed))
    }

    /// 1 回ぶんの観測を取り込み、取り込んだあとの総行数を返す。
    fn advance(&self, moved: u64, sb_len: usize) -> u64 {
        self.sb_len.store(sb_len as u64, Ordering::Relaxed);
        self.scrolled.fetch_add(moved, Ordering::Relaxed) + moved
    }
}
/// 選択範囲を行優先(row-major)で正規化し、(開始 <= 終了) にして返す。
fn normalize_sel(sel: ((u16, u16), (u16, u16))) -> ((u16, u16), (u16, u16)) {
    let (a, b) = sel;
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// セル (r, c) が正規化済み選択範囲に含まれるか(行優先のストリーム選択)。
fn cell_selected(r: u16, c: u16, (s, e): ((u16, u16), (u16, u16))) -> bool {
    (r, c) >= s && (r, c) <= e
}

/// 選択範囲の文字列を組み立てる。行末の余白は落とし、行は改行で繋ぐ。
fn selection_text(screen: &vt100::Screen, sel: ((u16, u16), (u16, u16))) -> String {
    let (s, e) = normalize_sel(sel);
    let (_, cols) = screen.size();
    let last_col = cols.saturating_sub(1);
    let mut out = String::new();
    for r in s.0..=e.0 {
        if r > s.0 {
            out.push('\n');
        }
        let c0 = if r == s.0 { s.1 } else { 0 };
        let c1 = if r == e.0 {
            e.1.min(last_col)
        } else {
            last_col
        };
        let mut line = String::new();
        for c in c0..=c1 {
            let Some(cell) = screen.cell(r, c) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let t = cell.contents();
            if t.is_empty() {
                line.push(' ');
            } else {
                line.push_str(&t);
            }
        }
        out.push_str(line.trim_end());
    }
    out
}

/// 絶対座標の選択範囲を、**スクロールバックをまたいで**文字列に組み立てる。
///
/// 見えている 1 画面しか読めない `selection_text` を、1 画面ぶんずつ
/// `set_scrollback` して呼び直すことで履歴全体へ広げる。
/// 呼び出し前の scrollback 位置は必ず元へ戻す (`all_terminal_lines` と同じ作法)。
fn selection_text_abs(p: &mut vt100::Parser, sel: ((usize, u16), (usize, u16))) -> String {
    let saved = p.screen().scrollback();
    p.set_scrollback(usize::MAX);
    let max_off = p.screen().scrollback();
    let rows = p.screen().size().0;
    if rows == 0 {
        p.set_scrollback(saved);
        return String::new();
    }
    let rows_u = rows as usize;
    // 実在する最古の行 = 最大戻り量の窓の最上段。ここより古い指定は切り詰める。
    let max_abs = max_off + rows_u - 1;
    let key = |q: (usize, u16)| (std::cmp::Reverse(q.0), q.1);
    let (s0, e0) = if key(sel.0) <= key(sel.1) {
        (sel.0, sel.1)
    } else {
        (sel.1, sel.0)
    };
    let top_abs = s0.0.min(max_abs); // 上端 (絶対行が大きい = 古い)
    let bot_abs = e0.0.min(max_abs); // 下端 (絶対行が小さい = 新しい)
    let s_col = if s0.0 > max_abs { 0 } else { s0.1 };
    let e_col = if e0.0 > max_abs { u16::MAX } else { e0.1 };
    let mut parts: Vec<String> = Vec::new();
    let mut cur_top = top_abs; // まだ読んでいない中で最も古い絶対行
    loop {
        let want = cur_top.saturating_sub(rows_u - 1).max(bot_abs);
        p.set_scrollback(want);
        // 履歴より深い戻りは vt100 が切り詰めるので、効いた量で画面行へ写す。
        let off = p.screen().scrollback();
        let win_top = off + rows_u - 1;
        let c_top = win_top.min(cur_top);
        let c_bot = off.max(bot_abs);
        let r_top = (win_top - c_top) as u16;
        let r_bot = (win_top - c_bot) as u16;
        // 端の列が効くのは選択そのものの端の行だけ。途中の行は行全体。
        let col0 = if c_top == top_abs { s_col } else { 0 };
        let col1 = if c_bot == bot_abs { e_col } else { u16::MAX };
        parts.push(selection_text(p.screen(), ((r_top, col0), (r_bot, col1))));
        if c_bot <= bot_abs {
            break;
        }
        cur_top = c_bot - 1;
    }
    p.set_scrollback(saved);
    parts.join("\n")
}

/// (r, c) を含む「空白区切りの語」の範囲を返す(ダブルクリック選択用)。
/// 空白セルの上なら None。
fn word_selection(screen: &vt100::Screen, r: u16, c: u16) -> Option<((u16, u16), (u16, u16))> {
    let (_, cols) = screen.size();
    let filled = |cix: u16| -> bool {
        screen
            .cell(r, cix)
            .map(|cell| {
                // 全角文字の継続セルも語の一部として扱う
                cell.is_wide_continuation() || !cell.contents().trim().is_empty()
            })
            .unwrap_or(false)
    };
    if !filled(c) {
        return None;
    }
    let mut c0 = c;
    while c0 > 0 && filled(c0 - 1) {
        c0 -= 1;
    }
    let mut c1 = c;
    while c1 + 1 < cols && filled(c1 + 1) {
        c1 += 1;
    }
    Some(((r, c0), (r, c1)))
}

/// CLI エージェント (Claude Code / Codex / Gemini 等) が画面下部に描く
/// プロンプト入力欄を推定する (Ctrl+A の「入力中テキストだけ選択」用)。
/// カーソル行から上へ遡ってマーカー行 (任意の左枠 │ の後に › ❯ ▸ ▶ ▌ > $ %
/// が来る行) を探し、そこから下へ空行・罫線行・次のマーカー行にぶつかる
/// までを入力欄とみなす。特定ツールの画面構造には依存せず見た目だけで
/// 判定するので、新しい CLI でもマーカーが一般的なら動く。
/// 戻り値は (選択範囲, 枠・マーカーを除いた入力テキスト)。入力が空なら None。
type InputAreaSel = (((u16, u16), (u16, u16)), String);

fn input_area_selection(screen: &vt100::Screen) -> Option<InputAreaSel> {
    let (rows, cols) = screen.size();
    if rows == 0 || cols == 0 {
        return None;
    }
    let (cur_r, _) = screen.cursor_position();
    let cur_r = cur_r.min(rows - 1);
    let row_chars = |r: u16| -> Vec<char> {
        let mut line: Vec<char> = Vec::new();
        for c in 0..cols {
            let Some(cell) = screen.cell(r, c) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let t = cell.contents();
            if t.is_empty() {
                line.push(' ');
            } else {
                line.extend(t.chars());
            }
        }
        while line.last() == Some(&' ') {
            line.pop();
        }
        line
    };
    let is_side = |ch: char| matches!(ch, '│' | '┃' | '║');
    let is_rule = |ch: char| {
        matches!(
            ch,
            '─' | '━'
                | '═'
                | '╌'
                | '┄'
                | '┈'
                | '╍'
                | '┅'
                | '┉'
                | '-'
                | '–'
                | '—'
                | '╭'
                | '╮'
                | '╰'
                | '╯'
                | '┌'
                | '┐'
                | '└'
                | '┘'
                | '├'
                | '┤'
                | '┬'
                | '┴'
                | '┼'
        )
    };
    // 罫線・枠だけの行 = 入力欄の上下境界
    let is_border_row = |cs: &[char]| {
        let mut n = 0usize;
        for &ch in cs {
            if ch == ' ' {
                continue;
            }
            if is_rule(ch) || is_side(ch) {
                n += 1;
            } else {
                return false;
            }
        }
        n >= 2
    };
    // 行頭 (任意の左枠+空白の後) のプロンプトマーカー。本文開始の文字位置を返す。
    // ">>" や "$HOME" のような本文を誤検出しないよう、マーカーの直後は
    // 空白か行末に限る。
    let marker_body_col = |cs: &[char]| -> Option<usize> {
        let mut i = 0;
        while i < cs.len() && cs[i] == ' ' {
            i += 1;
        }
        if i < cs.len() && is_side(cs[i]) {
            i += 1;
            while i < cs.len() && cs[i] == ' ' {
                i += 1;
            }
        }
        if i >= cs.len() || !matches!(cs[i], '›' | '❯' | '▸' | '▶' | '▌' | '>' | '$' | '%')
        {
            return None;
        }
        if i + 1 < cs.len() && cs[i + 1] != ' ' {
            return None;
        }
        Some(i + 2)
    };
    // カーソル行から上へマーカー行を探す (途中に空行・罫線があれば入力欄ではない)
    let mut marker: Option<(u16, usize)> = None;
    let low = cur_r.saturating_sub(40);
    for r in (low..=cur_r).rev() {
        let cs = row_chars(r);
        if let Some(col) = marker_body_col(&cs) {
            marker = Some((r, col));
            break;
        }
        if cs.is_empty() || is_border_row(&cs) {
            break;
        }
    }
    let (m_row, body_col) = marker?;
    // マーカー行から下へ続く本文行 (折返し・複数行入力)
    let mut bottom = m_row;
    for r in m_row + 1..rows {
        let cs = row_chars(r);
        if cs.is_empty() || is_border_row(&cs) || marker_body_col(&cs).is_some() {
            break;
        }
        bottom = r;
    }
    let mut text = String::new();
    for r in m_row..=bottom {
        let mut cs = row_chars(r);
        // 右枠を除去
        if cs.last().copied().is_some_and(is_side) {
            cs.pop();
            while cs.last() == Some(&' ') {
                cs.pop();
            }
        }
        // マーカー行は本文開始位置から。折返し行は左枠と、マーカー幅ぶんの
        // インデント (Claude Code は折返しを本文開始列に揃える) を飛ばす。
        let start = if r == m_row {
            body_col.min(cs.len())
        } else {
            let mut i = 0;
            if cs.first().copied().is_some_and(is_side) {
                i = 1;
            }
            while i < cs.len() && cs[i] == ' ' && i < body_col {
                i += 1;
            }
            i
        };
        if r > m_row && !screen.row_wrapped(r - 1) {
            // 端末が折返した行は改行を挟まず連結、それ以外は見た目通り改行
            text.push('\n');
        }
        text.extend(cs[start..].iter());
    }
    let text = text.trim_matches('\n').trim_end().to_string();
    if text.trim().is_empty() {
        return None;
    }
    // 選択範囲の見た目: マーカー直後 〜 最終行の右端 (右枠・空白は除く)
    let mut end_col = 0u16;
    for c in 0..cols {
        if let Some(cell) = screen.cell(bottom, c) {
            if cell.is_wide_continuation() || !cell.contents().trim().is_empty() {
                end_col = c;
            }
        }
    }
    while end_col > 0 {
        let t = screen
            .cell(bottom, end_col)
            .map(|c| c.contents())
            .unwrap_or_default();
        if t.trim().is_empty() || t.chars().next().is_some_and(is_side) {
            end_col -= 1;
        } else {
            break;
        }
    }
    let start_col = (body_col as u16).min(cols - 1);
    Some((((m_row, start_col), (bottom, end_col)), text))
}

/// 選択範囲をクリップボードへコピーし、フィードバック表示を開始する。
fn copy_selection(ui: &egui::Ui, session: &mut Session) {
    let text = session.selection_string();
    if !text.is_empty() {
        ui.ctx().copy_text(text);
        session.copied_at = Some(Instant::now());
    }
}

/// ドロップ/送信用のパス表記。セッションの cwd 配下なら相対、それ以外は絶対。
/// canonicalize は両側に best-effort で当て、シンボリックリンク差を吸収する。
fn prompt_path(path: &Path, cwd: &Path) -> String {
    let c_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let c_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    c_path
        .strip_prefix(&c_cwd)
        .unwrap_or(&c_path)
        .to_string_lossy()
        .into_owned()
}

/// ドラッグが画面の外へ出たときに 1 フレームで送る行数。
/// はみ出すほど速くするが、上限を置かないと一瞬で履歴の端まで飛ぶ。
fn autoscroll_step(overshoot: f32, cell_h: f32) -> i64 {
    if !(cell_h > 0.0) || !overshoot.is_finite() || overshoot <= 0.0 {
        return 1;
    }
    ((overshoot / cell_h).ceil() as i64).clamp(1, 8)
}

/// マウスによる文字選択(ドラッグ=範囲 / ダブルクリック=語 / トリプルクリック=行)。
fn handle_mouse_selection(
    session: &mut Session,
    response: &egui::Response,
    rect: egui::Rect,
    padding: f32,
    cell_w: f32,
    cell_h: f32,
) {
    let (rows_n, cols_n) = session.size;
    let to_cell = |pos: egui::Pos2| -> (u16, u16) {
        let c = ((pos.x - rect.min.x - padding) / cell_w).floor().max(0.0) as u16;
        let r = ((pos.y - rect.min.y - padding) / cell_h).floor().max(0.0) as u16;
        (
            r.min(rows_n.saturating_sub(1)),
            c.min(cols_n.saturating_sub(1)),
        )
    };
    if response.clicked() {
        // クリック(ドラッグなし)で選択解除
        session.clear_selection();
    }
    if response.drag_started_by(egui::PointerButton::Primary) {
        if let Some(pos) = response.interact_pointer_pos() {
            let (r, c) = to_cell(pos);
            session.clear_selection();
            session.sel_anchor_abs = Some((abs_row(r, rows_n, session.scroll), c));
        }
    }
    if response.dragged_by(egui::PointerButton::Primary) {
        if let (Some(anchor), Some(pos)) = (session.sel_anchor_abs, response.interact_pointer_pos())
        {
            // 画面の外まで引っ張られたら自動でスクロールし、選択を
            // 一画面を超えて伸ばせるようにする。
            let top = rect.min.y + padding;
            let bottom = rect.max.y - padding;
            let moved = if pos.y < top {
                session.adjust_scroll(autoscroll_step(top - pos.y, cell_h))
            } else if pos.y > bottom && session.scroll > 0 {
                session.adjust_scroll(-autoscroll_step(pos.y - bottom, cell_h))
            } else {
                false
            };
            if moved {
                // 押しっぱなしで止まっていても次のフレームを起こす
                // (起こさないと 1 行ずつしか進まない)。
                crate::perf::repaint(&response.ctx, "term_sel_autoscroll");
            }
            // **スクロールを適用してから**画面セル → 絶対座標へ写す
            // (順序が逆だと 1 フレームぶん、送った行数だけ選択がずれる)。
            let (r, c) = to_cell(pos);
            let head = (abs_row(r, rows_n, session.scroll), c);
            session.set_selection_abs(anchor, head);
        }
    }
    if response.double_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let (r, c) = to_cell(pos);
            let word = {
                let p = lock_ok(&session.parser);
                word_selection(p.screen(), r, c)
            };
            match word {
                Some(sel) => session.set_selection(sel),
                None => session.clear_selection(),
            }
        }
    }
    if response.triple_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let (r, _) = to_cell(pos);
            session.set_selection(((r, 0), (r, cols_n.saturating_sub(1))));
        }
    }
}

/// クリップボード画像の保存先 (OS の一時ディレクトリ配下)。
fn clip_image_dir() -> PathBuf {
    std::env::temp_dir().join("zaivern-clip")
}

/// クリップボード画像の保存上限。超えた分は古い順に間引く。
const CLIP_PNG_KEEP: usize = 24;

/// RGBA8 バッファを PNG として dir へ保存し、そのパスを返す。
/// ファイル名は空白なしの ASCII に限定する (prompt_path はシェルクオート
/// をしないため、空白入りだと CLI 側でパスが分断される)。
/// 保存のついでに古い clip-*.png を CLIP_PNG_KEEP 件まで間引く。
fn save_clipboard_png(w: usize, h: usize, rgba: &[u8], dir: &Path) -> std::io::Result<PathBuf> {
    use std::io::{Error, ErrorKind};
    // ゼロサイズ・長さ不一致 (乗算あふれ含む) は panic せずエラーで返す
    let expect = w.checked_mul(h).and_then(|n| n.checked_mul(4));
    if w == 0 || h == 0 || expect != Some(rgba.len()) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("クリップボード画像が不正: {}x{} bytes={}", w, h, rgba.len()),
        ));
    }
    let img = image::RgbaImage::from_raw(w as u32, h as u32, rgba.to_vec())
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "RGBA バッファを画像化できない"))?;
    std::fs::create_dir_all(dir)?;
    // タイムスタンプ+PID+連番で衝突しない名前を作る (すべて ASCII)
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!(
        "clip-{}-{}-{}.png",
        ms,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    img.save_with_format(&path, image::ImageFormat::Png)
        .map_err(|e| Error::new(ErrorKind::Other, e))?;
    prune_clip_pngs(dir, CLIP_PNG_KEEP);
    Ok(path)
}

/// dir 内の clip-*.png を新しい方から keep 件だけ残して削除する (best-effort)。
/// 自前の命名に合うファイルだけを対象にし、他のファイルには触らない。
fn prune_clip_pngs(dir: &Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, usize, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_str()?;
            if !(name.starts_with("clip-") && name.ends_with(".png")) {
                return None;
            }
            let t = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            // 更新時刻→名前長→名前の順 (連番は桁数込みで数値順になる)
            Some((t, name.len(), p))
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort();
    for (_, _, p) in files.iter().take(files.len() - keep) {
        let _ = std::fs::remove_file(p);
    }
}

/// V + そのOSの「ペースト修飾」(macOS: ⌘ / それ以外: Ctrl) だけの組み合わせか。
/// Ctrl+Shift+V (端末流の生ペースト) や Alt 併用は対象外にして既存挙動を守る。
///
/// OS 判定は引数で受ける (両分岐をどの環境でもテストできるようにする)。
/// 実行時の値は [`InputCaps::mac`] 経由で `cfg!(target_os = "macos")` が入る。
pub(crate) fn is_image_paste_chord_on(key: egui::Key, m: egui::Modifiers, mac: bool) -> bool {
    if key != egui::Key::V || m.shift || m.alt {
        return false;
    }
    if mac {
        m.mac_cmd && !m.ctrl
    } else {
        m.ctrl && !m.mac_cmd
    }
}

/// クリップボードの画像を PNG 保存してパスを返す薄いアダプタ。
/// - テキストも載っている場合は egui-winit の Paste イベントに任せて None
///   (二重貼り防止。egui-winit はテキストが取れたときだけ Paste を出す)。
/// - クリップボード初期化失敗 (ヘッドレス CI 等) や画像なし・保存失敗も
///   None で握りつぶし、従来の挙動へフォールバックする。
/// 呼び出しはキー入力時だけに限定する (起動時にクリップボードへ触らない)。
pub(crate) fn clipboard_image_to_png() -> Option<PathBuf> {
    let mut cb = arboard::Clipboard::new().ok()?;
    if cb.get_text().map(|t| !t.is_empty()).unwrap_or(false) {
        return None;
    }
    let img = cb.get_image().ok()?;
    save_clipboard_png(img.width, img.height, &img.bytes, &clip_image_dir()).ok()
}

/// 1 フレーム分の入力を翻訳した結果。
///
/// PTY へ書くバイト列は **1 本にまとめる**。イベントごとに書き分けると
/// 「あ」の 3 バイトが別々の write に割れて子プロセスへ届き、
/// UTF-8 の途中で切れた列を読んだ CLI が化けることがある。
#[derive(Default, Debug, PartialEq, Eq)]
struct InputPlan {
    /// PTY へ 1 回で書き出すバイト列。
    out: Vec<u8>,
    /// 選択範囲をクリップボードへ。
    copy: bool,
    /// 画面全体を選択。
    select_all: bool,
    /// CLI の入力欄だけを選択してコピー。
    input_select: Option<InputAreaSel>,
}

/// 翻訳に必要な端末の状態 (画面ロックを持ち込まずに済むよう値で渡す)。
#[derive(Clone, Copy, Debug)]
struct InputCaps {
    /// DECCKM (アプリケーションカーソルキー) が有効か。
    app_cursor: bool,
    /// ブラケットペーストが有効か。
    bracketed: bool,
    /// 画面末尾を見ているか (履歴スクロール中は Ctrl+A の入力欄選択を使わない)。
    at_bottom: bool,
    /// macOS か (⌘ 系のキー割り当てを両分岐ともテストできるよう引数で受ける)。
    mac: bool,
}

/// このフレームで IME の変換が「終わった」か。
///
/// 変換の確定・取り消しに使った Enter / Escape は **IME への操作**であって
/// 端末への入力ではない。素通しすると
///   * 日本語を確定した瞬間に CLI へ送信されてしまう (確定 Enter が改行になる)
///   * 変換を取り消した Escape が TUI のモードを抜けてしまう
/// という、日本語入力では毎回起きる事故になる。
///
/// egui-winit は環境によって並びが違う (Windows は Commit の前後に
/// Enabled/Disabled を出し、macOS は Disabled を出さない) ので、
/// **順序に依存せずフレーム単位で**判定する。
fn ime_ended_in_frame(events: &[egui::Event], composing_at_start: bool) -> bool {
    let mut ended = false;
    for ev in events {
        if let egui::Event::Ime(ime) = ev {
            match ime {
                egui::ImeEvent::Commit(_) => return true,
                // 未確定文字列が空になった = 確定 or 取り消しで変換が閉じた
                egui::ImeEvent::Preedit(t) if t.is_empty() && composing_at_start => ended = true,
                egui::ImeEvent::Disabled if composing_at_start => ended = true,
                _ => {}
            }
        }
    }
    ended
}

/// キーボード/IME/ペースト入力を PTY 向けのバイト列へ翻訳する純関数。
///
/// 端末にもクリップボードにも触らないので、IME の並び (未確定 → 更新 → 確定)
/// をテストからそのまま流し込める。副作用が要る 2 つだけ関数で受け取る:
/// `input_area` (画面から CLI の入力欄を探す) と `paste_image`
/// (クリップボードの画像を保存して `@パス` を作る)。
///
/// IME の規則:
/// 1. **未確定文字列 (preedit) は PTY へ送らない。** 画面にオーバーレイ表示するだけ。
///    途中経過を送ると、ハングルは初声だけが送られて音節が分裂し、日本語は
///    未変換のかなが混ざる。
/// 2. **確定文字列 (Commit) だけを送る。** 1 フレーム 1 回の書き込みに束ねる。
/// 3. 変換中に届いた `Text` は無視する (IME が処理中の生キーが漏れたもの)。
/// 4. 確定と**同じ文字列**が同フレームで `Text` としても届く環境がある。
///    そのまま流すと CJK が二重に入る (Windows の一部 IME で起きる) ので落とす。
/// 5. 変換が終わったフレームの Enter / Escape は IME への操作なので送らない。
///    次のフレームの Enter は通常どおり送る (確定 → 送信 の 2 打鍵は成立する)。
fn translate_input<A, P>(
    events: &[egui::Event],
    preedit: &mut String,
    caps: InputCaps,
    mut input_area: A,
    mut paste_image: P,
) -> InputPlan
where
    A: FnMut() -> Option<InputAreaSel>,
    P: FnMut() -> Option<String>,
{
    let mut plan = InputPlan::default();
    let ime_ended = ime_ended_in_frame(events, !preedit.is_empty());
    // 確定した文字列 (同フレームの Text 重複を落とすため覚えておく)
    let mut committed: Vec<String> = Vec::new();

    for ev in events {
        match ev {
            // ⌘C: 選択範囲をクリップボードへ(選択が無ければ何もしない)。
            // Ctrl+C は Key イベントとしてそのまま PTY へ届く(SIGINT)。
            egui::Event::Copy => {
                plan.copy = true;
            }
            egui::Event::Text(t) => {
                // 規則 3: 変換中の生テキストは IME に任せる
                if !preedit.is_empty() {
                    continue;
                }
                // 規則 4: 確定文字列の二重入力を落とす
                if let Some(i) = committed.iter().position(|c| c == t) {
                    committed.remove(i);
                    continue;
                }
                plan.out.extend_from_slice(t.as_bytes());
            }
            // IME(日本語入力など): 変換確定文字列を PTY へ送り、
            // 変換中の未確定文字列はオーバーレイ表示用に保持する。
            egui::Event::Ime(ime) => match ime {
                egui::ImeEvent::Commit(t) => {
                    preedit.clear();
                    if !t.is_empty() {
                        plan.out.extend_from_slice(t.as_bytes());
                        committed.push(t.clone());
                    }
                }
                egui::ImeEvent::Preedit(t) => {
                    preedit.clear();
                    preedit.push_str(t);
                }
                egui::ImeEvent::Enabled | egui::ImeEvent::Disabled => {
                    preedit.clear();
                }
            },
            egui::Event::Paste(t) => {
                if caps.bracketed {
                    plan.out.extend_from_slice(b"\x1b[200~");
                }
                plan.out.extend_from_slice(t.as_bytes());
                if caps.bracketed {
                    plan.out.extend_from_slice(b"\x1b[201~");
                }
            }
            // ⌘V / Ctrl+V で画像を貼る: egui-winit はクリップボードに
            // テキストが無いと Paste イベントを出さず、押下キーイベントも
            // 飲み込む (リリースだけ届く) ため、V のキーリリースで画像を
            // 拾う。画像は PNG 保存して @パス を挿入するだけで Enter は
            // 送らない (ドラッグ&ドロップ挿入と同じ振る舞い)。画像なし・
            // 保存失敗時は何もせず従来の挙動のまま。
            egui::Event::Key {
                key,
                pressed: false,
                modifiers,
                ..
            } if is_image_paste_chord_on(*key, *modifiers, caps.mac) => {
                if let Some(text) = paste_image() {
                    plan.out.extend_from_slice(text.as_bytes());
                }
            }
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                if modifiers.mac_cmd {
                    // ⌘A: Terminal.app と同じく、PTY へは送らずローカルで
                    // 表示中の画面全体を選択する (⌘C でそのままコピーできる)。
                    // CLI 側へ Ctrl+A を送っても「行頭移動」になるだけで
                    // 全選択にはならない。
                    if *key == egui::Key::A {
                        plan.select_all = true;
                    } else if let Some(b) = mac_agent_input_bytes(*key, *modifiers) {
                        plan.out.extend_from_slice(b);
                    }
                    continue;
                }
                // IME 変換中はキーを IME に任せる(Enter/矢印で確定・候補選択するため)
                if !preedit.is_empty() {
                    continue;
                }
                // 規則 5: 変換を閉じた Enter / Escape は端末へ送らない
                if ime_ended && matches!(*key, egui::Key::Enter | egui::Key::Escape) {
                    continue;
                }
                // Ctrl+A: 画面から CLI の入力欄 (› ❯ > 等のプロンプト行) を
                // 検出できたら PTY へ \x01 を送らず、いま打ち込んでいる本文
                // だけを選択してクリップボードへコピーする (音声入力した文を
                // そのまま使い回すため)。Claude Code / Codex / Gemini など
                // ツールを問わず見た目で判定し、検出できない画面 (素の
                // シェル等) では従来通り行頭移動として送る。
                if *key == egui::Key::A
                    && modifiers.ctrl
                    && !modifiers.alt
                    && !modifiers.shift
                    && caps.at_bottom
                {
                    if let Some(f) = input_area() {
                        plan.input_select = Some(f);
                        continue;
                    }
                }
                if let Some(b) = key_bytes(*key, *modifiers, caps.app_cursor) {
                    plan.out.extend_from_slice(&b);
                }
            }
            _ => {}
        }
    }
    plan
}

/// フォーカス中のキーボード/IME/ペースト入力を PTY へ転送する。
fn forward_keyboard_input(ui: &mut egui::Ui, session: &mut Session, focus_id: egui::Id) {
    ui.memory_mut(|m| {
        m.set_focus_lock_filter(
            focus_id,
            egui::EventFilter {
                tab: true,
                horizontal_arrows: true,
                vertical_arrows: true,
                escape: true,
            },
        )
    });
    let events = ui.input(|i| i.events.clone());
    let (app_cursor, bracketed) = {
        let p = lock_ok(&session.parser);
        let s = p.screen();
        (s.application_cursor(), s.bracketed_paste())
    };
    let caps = InputCaps {
        app_cursor,
        bracketed,
        at_bottom: session.scroll == 0,
        mac: cfg!(target_os = "macos"),
    };
    // 純関数側へ渡す副作用 2 つ。呼ばれたときだけ画面ロック/クリップボードに触る。
    let parser = session.parser.clone();
    let cwd = session.cwd.clone();
    let InputPlan {
        out,
        copy: want_copy,
        select_all: want_select_all,
        input_select,
    } = translate_input(
        &events,
        &mut session.preedit,
        caps,
        || input_area_selection(lock_ok(&parser).screen()),
        || clipboard_image_to_png().map(|png| format!("@{} ", prompt_path(&png, &cwd))),
    );
    if !out.is_empty() {
        // 人が打った分は音声入力の書き込み追跡とずれるので印を立てる。
        // 承認プロンプトへの手入力応答もここで「応答済み」として解決する
        // (自動YESオフの手動運転で attention を引きずらないため)。
        session.note_user_input();
        // 直接打ち込んだ指示も覚える (自動命名 / 失敗切替の引き継ぎの材料)。
        // 送る前に通すこと — `write_bytes` の後だと、送信で状態が変わった
        // セッションに対して古い行を記録してしまう。
        session.note_typed_bytes(&out);
        session.write_bytes(&out);
        session.set_scroll(0);
    }
    if want_select_all {
        session.select_all();
    }
    if let Some((sel, text)) = input_select {
        // Ctrl+A の入力欄選択: 選択表示 + 即コピー (⌘C を待たない)
        session.set_selection(sel);
        session.sel_anchor_abs = None;
        ui.ctx().copy_text(text);
        session.copied_at = Some(Instant::now());
    }
    if want_copy {
        copy_selection(ui, session);
    }
}

/// ホイール入力の処理: マウス報告転送 / 矢印キー代用 / ローカル履歴スクロール。
fn handle_wheel_scroll(
    ui: &mut egui::Ui,
    session: &mut Session,
    rect: egui::Rect,
    padding: f32,
    cell_w: f32,
    cell_h: f32,
) {
    let dy = ui.input(|i| i.raw_scroll_delta.y);
    if dy.abs() > 0.5 {
        let (alt, mouse_on, sgr) = session.wheel_modes();
        let up = dy > 0.0;
        // ホイールの移動量をノッチ数へ(1〜8)
        let notches = ((dy.abs() / cell_h).ceil() as i32).clamp(1, 8);
        if mouse_on {
            // アプリがマウス報告中: ホイールをそのまま転送する。
            // これで Claude Code / less / vim などがアプリ側でスクロールする。
            let hover = ui
                .input(|i| i.pointer.hover_pos())
                .unwrap_or_else(|| rect.center());
            let col = (((hover.x - rect.min.x - padding) / cell_w).floor().max(0.0)) as u16;
            let row = (((hover.y - rect.min.y - padding) / cell_h).floor().max(0.0)) as u16;
            for _ in 0..notches {
                session.send_wheel(up, col, row, sgr);
            }
            // 代替画面に切り替わった後もローカル履歴表示が残らないようにする
            if session.scroll != 0 {
                session.set_scroll(0);
            }
        } else if alt {
            // マウス無効の全画面アプリ: 矢印キーで代用スクロール
            let arrow: &[u8] = if up { b"\x1b[A" } else { b"\x1b[B" };
            for _ in 0..notches {
                session.write_bytes(arrow);
            }
            if session.scroll != 0 {
                session.set_scroll(0);
            }
        } else {
            // 通常画面(シェル等): ローカルのスクロールバック履歴。
            // 整数切り捨てで 0 行になると、ゆっくりスクロールしたとき
            // 一番下(scroll=0)まで戻り切れず履歴表示が残るため、
            // 1 イベントにつき最低 1 行は必ず動かす。
            let mut lines = (dy / cell_h * 2.0) as i64;
            if lines == 0 {
                lines = if up { 1 } else { -1 };
            }
            session.adjust_scroll(lines);
        }
        // 外側 ScrollArea との二重スクロールを防ぐためホイールを消費する
        ui.input_mut(|i| {
            i.raw_scroll_delta.y = 0.0;
            i.smooth_scroll_delta.y = 0.0;
        });
    }
}

/// セル `(row, col)` を描くときに実際に使ってよい桁数を決める純関数。
///
/// 普段は「全角 = 2 桁 / それ以外 = 1 桁」。ただし**全角の右半分が失われた
/// セルが残ることがある**: 端末を狭めると、右端に入り切らなくなった全角が
/// 継続セルを失ったまま最終列に居残り、そのあと広げると行の途中に居座る
/// (vt100 のリサイズは行を組み直さない)。そのまま 2 桁で描くと、
///   * 最終列なら 背景・下線・カーソルが端末の枠から半桁はみ出し、
///   * 行の途中なら 右隣のセルの文字とグリフが重なる (二重に見える)。
///
/// どちらも「置き場がある桁数まで詰める」ことで防ぐ。`right_free` は
/// 右隣が継続セル (= 自分の右半分) か空セルであること。
fn draw_cols(wide: bool, col: u16, cols: u16, right_free: bool) -> u16 {
    if wide && col + 1 < cols && right_free {
        2
    } else {
        1
    }
}

/// [`draw_cols`] を画面の実状態から呼ぶ。
fn cell_draw_cols(screen: &vt100::Screen, row: u16, col: u16) -> u16 {
    let (_, cols) = screen.size();
    let wide = screen.cell(row, col).is_some_and(|c| c.is_wide());
    let right_free = col
        .checked_add(1)
        .and_then(|n| screen.cell(row, n))
        .is_some_and(|n| n.is_wide_continuation() || !n.has_contents());
    draw_cols(wide, col, cols, right_free)
}

/// カーソルが占めるセル範囲 `(開始列, 桁数)`。
///
/// 全角文字の上にカーソルがあるとき 1 桁で描くと「文字の左半分だけ反転する」
/// ので、日本語を打っている間ずっと壊れて見える。**グリッドが 2 セル取って
/// いるならカーソルも 2 セル**にして、描画とセルの持ち方を一致させる。
///
/// 継続セル (全角の右半分) を指している場合は左半分まで戻す。アプリが
/// `CUP` / `DECRC` で右半分を直接指定することは正常に起こり得るため、
/// そこを「1 桁の別文字」として描かない。
fn cursor_span(screen: &vt100::Screen, row: u16, col: u16) -> (u16, u16) {
    match screen.cell(row, col) {
        Some(c) if c.is_wide() => (col, cell_draw_cols(screen, row, col)),
        Some(c) if c.is_wide_continuation() => {
            let left = col.saturating_sub(1);
            (left, cell_draw_cols(screen, row, left))
        }
        _ => (col, 1),
    }
}

// ---------------------------------------------------------------------------
// リンク検出 (URL / ファイルパス:行:桁)
// ---------------------------------------------------------------------------
//
// エージェントとビルドツールの出力は **ほぼ全部** `src/foo.rs:12:5` の形で
// ファイルを指す。ここをクリックで開けるかどうかが AI コックピットの生死を分ける。
//
// 設計の要:
// * 判定は **純関数** (`detect_links`) に閉じ込め、ファイルシステムは
//   `exists` という差し込み口だけで触る。テストは実ファイル無しで書け、
//   実装側は TTL キャッシュを挿せる (毎フレーム `stat` を撃たない)。
// * **実在しないパスはリンクにしない**。押せそうに見えて押せないのが最悪。
// * 走査するのは **ポインタの下の 1 行だけ**。画面全体は見ない
//   (設計原則 3 = アイドル時のコストはゼロ)。

/// 行テキストから見つけたリンク候補。位置は **行テキストのバイト範囲**で、
/// `&line[start..end]` は必ず有効な部分文字列になる (CJK でも境界を割らない)。
#[derive(Debug, Clone, PartialEq)]
pub struct LinkMatch {
    pub start: usize,
    pub end: usize,
    pub kind: LinkKind,
}

/// リンクの中身。行・桁は **1 起点** (画面に書かれていたまま)。
#[derive(Debug, Clone, PartialEq)]
pub enum LinkKind {
    Url(String),
    File {
        raw: String,
        line: Option<u32>,
        col: Option<u32>,
    },
}

/// 画面上で解決済みのリンク (桁範囲 + 飛び先)。
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLink {
    pub col_start: u16,
    /// 終端は含まない。
    pub col_end: u16,
    pub target: LinkTarget,
}

/// リンクの飛び先。ファイルの行・桁は **0 起点** (エディタ側の数え方)。
#[derive(Debug, Clone, PartialEq)]
pub enum LinkTarget {
    Url(String),
    File {
        path: PathBuf,
        line: usize,
        col: usize,
    },
}

/// 端末リンクの実行時状態。実在確認のメモと、直近に解析した 1 行だけを持つ。
#[derive(Default)]
pub struct LinkState {
    exists: HashMap<PathBuf, (bool, Instant)>,
    /// (行, 行テキストのハッシュ, 解決済みリンク)。
    row: Option<(u16, u64, Vec<ResolvedLink>)>,
    /// 直近にポインタを押し下げた座標。クリックとドラッグ選択を見分けるために持つ。
    /// egui の `press_origin` は **離した瞬間のフレームでもう None** に戻るので、
    /// クリックが確定するそのフレームでは読めない。押されている間に控えておく。
    press_at: Option<egui::Pos2>,
}

/// 実在確認の有効期間。ビルドで生まれたファイルが数秒で拾えるだけの短さ。
const LINK_EXISTS_TTL: Duration = Duration::from_secs(5);
/// 実在確認メモの上限 (超えたら丸ごと捨てる。LRU を持つほどの物ではない)。
const LINK_EXISTS_CAP: usize = 512;
/// 空白を含むパスのために後続トークンを何個まで連結して試すか。
const LINK_MERGE_MAX: usize = 4;

const URL_SCHEMES: [&str; 2] = ["https://", "http://"];

/// 開き括弧・引用符。トークンの頭に付いていたら落とす。
const LINK_OPENERS: [char; 8] = ['(', '[', '{', '<', '"', '\'', '`', '@'];

fn count_char(s: &str, c: char) -> usize {
    s.chars().filter(|x| *x == c).count()
}

/// URL の本体に入り得る文字か。空白・制御文字・`<>"` ` は必ず外。
fn is_url_body(c: char) -> bool {
    !c.is_whitespace() && !c.is_control() && !matches!(c, '<' | '>' | '"' | '`' | '\u{3000}')
}

/// URL の末尾から「文章の句読点」を落とす。
///
/// 括弧は **釣り合っていない閉じだけ**を落とすので、`(https://example.com)` の
/// `)` は消え、`https://example.com/a_(b)` は丸ごと残る。
fn trim_url_tail(url: &str) -> &str {
    let mut end = url.len();
    while let Some(c) = url[..end].chars().next_back() {
        let head = &url[..end];
        let drop = match c {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '、' | '。' | '，' | '．' => true,
            ')' => count_char(head, '(') < count_char(head, ')'),
            ']' => count_char(head, '[') < count_char(head, ']'),
            '}' => count_char(head, '{') < count_char(head, '}'),
            '>' => true,
            _ => false,
        };
        if !drop {
            break;
        }
        end -= c.len_utf8();
    }
    &url[..end]
}

/// 行から `http(s)://…` を拾う。ファイルシステムには触らない純関数。
pub fn detect_urls(line: &str) -> Vec<LinkMatch> {
    let mut out: Vec<LinkMatch> = Vec::new();
    let mut i = 0usize;
    while i < line.len() {
        let Some(rel) = line[i..].find("http") else {
            break;
        };
        let s = i + rel;
        i = s + 4;
        // 直前が ASCII 英数字なら別の語の一部 (`xhttp://…`) — 単語境界を見る。
        // CJK は日本語の本文へ URL を直に埋め込むのが普通なので境界扱いにする
        // (`説明はhttps://…` を落とさない)。
        if line[..s]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            continue;
        }
        let Some(scheme) = URL_SCHEMES.iter().find(|p| line[s..].starts_with(**p)) else {
            continue;
        };
        let mut e = s + scheme.len();
        for c in line[e..].chars() {
            if !is_url_body(c) {
                break;
            }
            e += c.len_utf8();
        }
        let trimmed = trim_url_tail(&line[s..e]);
        // ホスト部が空 (`https://`) はリンクにしない
        if trimmed[scheme.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric())
        {
            out.push(LinkMatch {
                start: s,
                end: s + trimmed.len(),
                kind: LinkKind::Url(trimmed.to_string()),
            });
            i = i.max(s + trimmed.len());
        }
    }
    out
}

/// 行を空白で切ったトークンのバイト範囲。全角空白 (U+3000) も区切りになる。
fn whitespace_tokens(line: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in line.char_indices() {
        if c.is_whitespace() {
            if let Some(s) = start.take() {
                out.push((s, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push((s, line.len()));
    }
    out
}

/// 末尾の要素が `.<拡張子>` を持つか (`foo.rs` / `日本語ファイル.txt`)。
fn has_file_ext(s: &str) -> bool {
    let name = s.rsplit(['/', '\\']).next().unwrap_or(s);
    let Some(dot) = name.rfind('.') else {
        return false;
    };
    if dot == 0 || dot + 1 >= name.len() {
        return false;
    }
    let ext = &name[dot + 1..];
    ext.len() <= 16
        && ext
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'))
}

/// **パスらしさ**。区切りか拡張子のどちらかが要る。
///
/// 「画面テキストの部分一致で状態を判定しない」と同じ教訓で、`error` や `12` を
/// 「たまたま存在しないファイル」として毎回 `stat` しないための足切りでもある。
fn looks_like_path(s: &str) -> bool {
    if s.is_empty() || s.len() > 1024 {
        return false;
    }
    if s.chars().any(|c| c.is_control()) || s.contains("://") {
        return false;
    }
    if !s.chars().any(|c| c.is_alphanumeric()) {
        return false;
    }
    s.contains('/') || s.contains('\\') || has_file_ext(s)
}

/// パス候補の末尾から句読点・閉じ括弧・引用符を落とす。
fn trim_path_tail(s: &str) -> &str {
    let mut end = s.len();
    while let Some(c) = s[..end].chars().next_back() {
        let head = &s[..end];
        let drop = match c {
            '.' | ',' | ';' | ':' | '!' | '?' | '、' | '。' | '，' | '．' => true,
            '"' | '\'' | '`' => true,
            ')' => count_char(head, '(') < count_char(head, ')'),
            ']' => count_char(head, '[') < count_char(head, ']'),
            '}' => count_char(head, '{') < count_char(head, '}'),
            '>' => count_char(head, '<') < count_char(head, '>'),
            _ => false,
        };
        if !drop {
            break;
        }
        end -= c.len_utf8();
    }
    &s[..end]
}

fn trim_path_quotes(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, '"' | '\'' | '`'))
}

/// MSVC 形式 `foo.rs(12,5)` / `foo.rs(12)` の末尾を割る。
fn split_paren_pos(s: &str) -> Option<(&str, u32, Option<u32>)> {
    let body = s.strip_suffix(')')?;
    let open = body.rfind('(')?;
    let inside = &body[open + 1..];
    if inside.is_empty() {
        return None;
    }
    let mut it = inside.splitn(2, ',');
    let line = it.next()?.trim().parse::<u32>().ok()?;
    let col = match it.next() {
        Some(c) => Some(c.trim().parse::<u32>().ok()?),
        None => None,
    };
    Some((&body[..open], line, col))
}

/// 末尾の `:<数字>` を 1 つ剥がす。
///
/// Windows の `C:\path\foo.rs` を壊さないよう、残りが 1 文字のドライブ文字
/// だけになる場合は剥がさない。
fn split_colon_num(s: &str) -> Option<(&str, u32)> {
    let colon = s.rfind(':')?;
    let num = &s[colon + 1..];
    if num.is_empty() || num.len() > 9 || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let head = &s[..colon];
    if head.is_empty() {
        return None;
    }
    if head.len() == 1 && head.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some((head, num.parse().ok()?))
}

/// [`parse_path_token`] の結果。
/// `(トークン内の先頭バイト, 終端バイト, パス, 1 起点の行, 1 起点の桁)`。
type ParsedPath = (usize, usize, String, Option<u32>, Option<u32>);

/// トークン (またはトークンを連結した範囲) 1 個を「パス + 行 + 桁」に割る。
/// 返す `(先頭, 終端)` は渡した文字列内の相対バイト位置。
fn parse_path_token(tok: &str) -> Option<ParsedPath> {
    let mut s = 0usize;
    while let Some(c) = tok[s..].chars().next() {
        if LINK_OPENERS.contains(&c) {
            s += c.len_utf8();
        } else {
            break;
        }
    }
    let body = &tok[s..];
    if body.is_empty() {
        return None;
    }
    // MSVC 形式を先に見る (末尾の `)` を句読点として落とさないため)
    if let Some((path, line, col)) = split_paren_pos(body) {
        let path = trim_path_quotes(path);
        if looks_like_path(path) {
            return Some((s, s + body.len(), path.to_string(), Some(line), col));
        }
    }
    let t = trim_path_tail(body);
    if t.is_empty() {
        return None;
    }
    let end = s + t.len();
    let (mut path, mut line, mut col) = (t, None, None);
    if let Some((head, n)) = split_colon_num(path) {
        if let Some((head2, n2)) = split_colon_num(head) {
            path = head2;
            line = Some(n2);
            col = Some(n);
        } else {
            path = head;
            line = Some(n);
        }
    }
    let path = trim_path_quotes(path);
    if !looks_like_path(path) {
        return None;
    }
    Some((s, end, path.to_string(), line, col))
}

/// 行 1 本からリンクを検出する **純関数**。
///
/// `exists` は「パス文字列 → 実在するか」を答える差し込み口。テストは実ファイルを
/// 作らずに済み、実装側は TTL キャッシュを挿せる (毎フレーム `stat` を撃たない
/// ための唯一の口)。**`exists` が false を返したものはリンクにならない。**
pub fn detect_links(line: &str, exists: &mut dyn FnMut(&str) -> bool) -> Vec<LinkMatch> {
    let mut out = detect_urls(line);
    let toks = whitespace_tokens(line);
    let mut ti = 0usize;
    while ti < toks.len() {
        let (ts, te) = toks[ti];
        // URL と重なるトークンは飛ばす (`https://x/a.rs:12` を二重に拾わない)
        if out.iter().any(|l| l.start < te && ts < l.end) {
            ti += 1;
            continue;
        }
        // 空白を含むパスのため後続トークンを連結して試す。連結候補も
        // 「`looks_like_path` を通り、かつ実在するもの」しか採らないので、
        // 誤検出も無駄な `stat` も増えない。
        let last = (ti + LINK_MERGE_MAX).min(toks.len() - 1);
        let mut consumed = ti;
        let mut hit: Option<LinkMatch> = None;
        for (tj, tok) in toks.iter().enumerate().take(last + 1).skip(ti) {
            let Some((rs, re, raw, ln, col)) = parse_path_token(&line[ts..tok.1]) else {
                continue;
            };
            if !exists(&raw) {
                continue;
            }
            consumed = tj;
            hit = Some(LinkMatch {
                start: ts + rs,
                end: ts + re,
                kind: LinkKind::File { raw, line: ln, col },
            });
            break;
        }
        match hit {
            Some(m) => {
                out.push(m);
                ti = consumed + 1;
            }
            None => ti += 1,
        }
    }
    out.sort_by_key(|m| m.start);
    out
}

/// 相対パスを **その端末の作業ディレクトリ**基準で解決する。
///
/// ワークスペースルート直書きは禁止 — 端末が `cd` していたら別のファイルを
/// 開いてしまう。`~` はホームへ展開する (`dirs` 経由なのでユーザー名に依存しない)。
pub fn resolve_link_path(raw: &str, cwd: &Path) -> PathBuf {
    let expanded: PathBuf = if raw == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw))
    } else if let Some(rest) = raw.strip_prefix("~/") {
        match dirs::home_dir() {
            Some(h) => h.join(rest),
            None => PathBuf::from(raw),
        }
    } else {
        PathBuf::from(raw)
    };
    // Windows の `C:\…` は unix 上では is_absolute() が false になるが、
    // その場合 join した先は実在しないので結局リンクにならない (両 OS で正しい)。
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// 実在確認の TTL 付きメモ。行の中身が変わったときだけ通る道なので、
/// 静止した画面では `stat` が 1 回も飛ばない。
fn exists_cached(cache: &mut HashMap<PathBuf, (bool, Instant)>, p: &Path) -> bool {
    let now = Instant::now();
    if let Some((v, at)) = cache.get(p) {
        if now.duration_since(*at) < LINK_EXISTS_TTL {
            return *v;
        }
    }
    if cache.len() >= LINK_EXISTS_CAP {
        cache.clear();
    }
    let v = p.is_file();
    cache.insert(p.to_path_buf(), (v, now));
    v
}

fn link_text_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// 画面 1 行のテキストと「文字の開始バイト位置 → 桁」の対応表。
///
/// 全角文字は 2 桁進むので、バイト位置をそのまま桁にしてはいけない。
fn row_text_cols(screen: &vt100::Screen, row: u16) -> (String, Vec<(usize, u16)>) {
    let (_, cols) = screen.size();
    let mut text = String::new();
    let mut map: Vec<(usize, u16)> = Vec::with_capacity(usize::from(cols));
    for c in 0..cols {
        let Some(cell) = screen.cell(row, c) else {
            continue;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        map.push((text.len(), c));
        let s = cell.contents();
        if s.is_empty() {
            text.push(' ');
        } else {
            text.push_str(&s);
        }
    }
    (text, map)
}

/// バイト範囲 → 桁範囲。`map` は [`row_text_cols`] が返す対応表。
fn cols_for_range(map: &[(usize, u16)], start: usize, end: usize, cols: u16) -> (u16, u16) {
    let col_start = map
        .iter()
        .rev()
        .find(|(b, _)| *b <= start)
        .map_or(0, |(_, c)| *c);
    let col_end = map
        .iter()
        .find(|(b, _)| *b >= end)
        .map_or(cols, |(_, c)| *c);
    (col_start, col_end.max(col_start.saturating_add(1)))
}

/// このクリックでリンクを開いてよいか。**egui に触らない純関数**にして、
/// 「素のクリックで開ける」と「文字選択を壊さない」の両立をテーブルテストで固定する。
///
/// 端末の素のクリックは既に (1) ドラッグ選択の起点 (2) 選択の解除
/// (3) ダブル/トリプルクリックの語・行選択、として使われている。
/// リンクを開くのは **そのどれでもないと確定したクリックだけ**。
///
/// * `hovering` — ポインタの下にリンクがあるか。
/// * `dragged_px` — 押し始めから離すまでにポインタが動いた距離 (px)。
/// * `slop_px` — クリックと見なす移動量の上限。egui が `clicked()` の判定に使う
///   `max_click_dist` をそのまま渡す (自前のしきい値を作ると egui と食い違う)。
/// * `click_count` — 0=クリックなし / 1=シングル / 2=ダブル / 3=トリプル。
/// * `modified` — 修飾キー (mac は Command / 他は Ctrl) が押されているか。
/// * `had_selection` — このクリックの **前**に選択範囲があったか。
fn should_open_link(
    hovering: bool,
    dragged_px: f32,
    slop_px: f32,
    click_count: u8,
    modified: bool,
    had_selection: bool,
) -> bool {
    // ダブル(語選択)・トリプル(行選択)は選択操作。0 は「そもそも押していない」。
    if !hovering || click_count != 1 {
        return false;
    }
    // 測れなかった値 (NaN / 無限大 / 負) は安全側に倒して開かない。
    // NaN は比較が常に false になるので、大小を見る前に弾いておく。
    if !dragged_px.is_finite() || !slop_px.is_finite() || dragged_px < 0.0 {
        return false;
    }
    // しきい値を超えて動いていたらドラッグ選択。開かない。
    if dragged_px > slop_px {
        return false;
    }
    // 修飾キー付きは従来からの経路。指が覚えているので、選択の有無に関わらず開く。
    if modified {
        return true;
    }
    // 素のクリックで選択範囲がある間は「選択を解除したい」の意味。
    // ここで開くと、読み返そうとして選択しただけの人がブラウザへ飛ばされる。
    !had_selection
}

/// 端末のリンククリックが積む「このファイルのこの位置を開いて」要求のキー。
/// `draw` の呼び出し側は多数あるので戻り値ではなく egui の一時データを通す
/// (`zv-drop-consumed` と同じ約束)。
const OPEN_REQUEST_KEY: &str = "zv-terminal-open-file";

/// 端末リンクのクリック要求を回収する。行・桁は **0 起点**。
pub fn take_open_request(ctx: &egui::Context) -> Option<(PathBuf, usize, usize)> {
    ctx.data_mut(|d| d.remove_temp::<(PathBuf, usize, usize)>(egui::Id::new(OPEN_REQUEST_KEY)))
}

impl Session {
    /// ポインタ下 `(row, col)` にあるリンク。無ければ None。
    ///
    /// 解析するのは **その 1 行だけ**で、行テキストが変わらない限り再計算しない。
    fn link_at(&mut self, row: u16, col: u16) -> Option<ResolvedLink> {
        let (text, map, cols) = {
            let p = lock_ok(&self.parser);
            let screen = p.screen();
            let (_, cols) = screen.size();
            let (t, m) = row_text_cols(screen, row);
            (t, m, cols)
        };
        let h = link_text_hash(&text);
        let fresh = self
            .links
            .row
            .as_ref()
            .is_some_and(|(r, hh, _)| *r == row && *hh == h);
        if !fresh {
            let cwd = self.cwd.clone();
            let cache = &mut self.links.exists;
            let mut resolved: HashMap<String, PathBuf> = HashMap::new();
            let matches = {
                let mut exists = |raw: &str| -> bool {
                    let p = resolve_link_path(raw, &cwd);
                    let ok = exists_cached(cache, &p);
                    if ok {
                        resolved.insert(raw.to_string(), p);
                    }
                    ok
                };
                detect_links(&text, &mut exists)
            };
            let mut out = Vec::new();
            for m in matches {
                let (cs, ce) = cols_for_range(&map, m.start, m.end, cols);
                let target = match m.kind {
                    LinkKind::Url(u) => LinkTarget::Url(u),
                    LinkKind::File { raw, line, col } => {
                        let Some(path) = resolved.get(&raw).cloned() else {
                            continue;
                        };
                        LinkTarget::File {
                            path,
                            line: line.unwrap_or(1).saturating_sub(1) as usize,
                            col: col.unwrap_or(1).saturating_sub(1) as usize,
                        }
                    }
                };
                out.push(ResolvedLink {
                    col_start: cs,
                    col_end: ce,
                    target,
                });
            }
            self.links.row = Some((row, h, out));
        }
        self.links
            .row
            .as_ref()?
            .2
            .iter()
            .find(|l| col >= l.col_start && col < l.col_end)
            .cloned()
    }
}

/// リンクのホバー表示とクリック処理。
///
/// **修飾キー無しのクリックでも開く**。VS Code は ⌘ を要求するが、
/// 「リンクに見えるのに押しても何も起きない」の方がずっと多く報告される。
/// 代わりに、選択操作と衝突しないクリックだけを [`should_open_link`] で選り分ける
/// (ドラッグした / ダブル・トリプルクリック / 選択が残っている、は全部見送る)。
/// 修飾キー付きは従来どおり開くので、覚えている指も壊れない。
#[allow(clippy::too_many_arguments)]
fn handle_links(
    ui: &egui::Ui,
    painter: &egui::Painter,
    session: &mut Session,
    theme: &Theme,
    response: &egui::Response,
    rect: egui::Rect,
    padding: f32,
    cell_w: f32,
    cell_h: f32,
    had_selection: bool,
) {
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
    }
    let Some(pos) = ui.ctx().pointer_hover_pos() else {
        return;
    };
    if !rect.contains(pos) {
        return;
    }
    let (rows_n, cols_n) = session.size;
    if rows_n == 0 || cols_n == 0 {
        return;
    }
    let colf = (pos.x - rect.min.x - padding) / cell_w;
    let rowf = (pos.y - rect.min.y - padding) / cell_h;
    if colf < 0.0 || rowf < 0.0 {
        return;
    }
    let (col, row) = (colf.floor() as u16, rowf.floor() as u16);
    if row >= rows_n || col >= cols_n {
        return;
    }
    let Some(link) = session.link_at(row, col) else {
        return;
    };

    let modified = ui.input(|i| i.modifiers.command);
    // 端末セルと同じく整数ピクセルへ揃える (小数のままだと 100% 表示で揺れる)
    let ppp = ui.ctx().pixels_per_point();
    let origin = rect.min + egui::vec2(padding, padding);
    let x0 = crate::theme::snap_len(origin.x + f32::from(link.col_start) * cell_w, ppp);
    let x1 =
        crate::theme::snap_len(origin.x + f32::from(link.col_end) * cell_w, ppp).min(rect.max.x);
    let y = crate::theme::snap_len(origin.y + f32::from(row) * cell_h + cell_h - 1.0, ppp);
    // 修飾キーの有無で見た目を変えない。押せば開けるのだから、押せると分かる
    // 描き方を常に出す。色は `Theme::link_color()` — アクセント色を流用すると
    // 選択の塗り・カーソル・フォーカスリングと同じ色になり、「押せるもの」と
    // 「いま選ばれているもの」が見分けられなくなる。
    painter.line_segment(
        [egui::pos2(x0, y), egui::pos2(x1, y)],
        egui::Stroke::new(1.5_f32, theme.link_color()),
    );

    let label = match &link.target {
        LinkTarget::Url(u) => u.clone(),
        LinkTarget::File { path, line, .. } => {
            format!("{}:{}", path.display(), line.saturating_add(1))
        }
    };
    // 手のカーソルも修飾キーに関係なく出す (押せるものは押せる形をしている)。
    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    response
        .clone()
        .on_hover_text(trf("クリックで開く: {t}", &[("t", label)]));

    // クリックとドラッグ選択の見分け。しきい値は egui が `clicked()` を判定する
    // のに使う値をそのまま借りる (自前の数字を置くと egui の判定と食い違う)。
    let slop_px = ui.ctx().options(|o| o.input_options.max_click_dist);
    // 押し始めの記録が無いフレームは「測れなかった」= 開かない側へ倒す。
    let dragged_px = session
        .links
        .press_at
        .map_or(f32::INFINITY, |p| p.distance(pos));
    // 右クリック(コンテキストメニュー)まで拾わないよう、主ボタンだけを見る。
    // egui の `Response::clicked` はボタンを区別しない点に注意。
    let click_count = if response.triple_clicked_by(egui::PointerButton::Primary) {
        3
    } else if response.double_clicked_by(egui::PointerButton::Primary) {
        2
    } else if response.clicked_by(egui::PointerButton::Primary) {
        1
    } else {
        0
    };
    if should_open_link(
        response.hovered(),
        dragged_px,
        slop_px,
        click_count,
        modified,
        had_selection,
    ) {
        match link.target {
            LinkTarget::Url(u) => ui.ctx().open_url(egui::OpenUrl::new_tab(u)),
            LinkTarget::File { path, line, col } => {
                ui.ctx().data_mut(|d| {
                    d.insert_temp(egui::Id::new(OPEN_REQUEST_KEY), (path, line, col));
                });
            }
        }
    }
}

/// 画面グリッド(文字セル・選択ハイライト)、カーソル、IME オーバーレイの描画。
#[allow(clippy::too_many_arguments)]
fn draw_screen(
    ui: &egui::Ui,
    painter: &egui::Painter,
    session: &Session,
    theme: &Theme,
    font_id: &egui::FontId,
    rect: egui::Rect,
    padding: f32,
    cell_w: f32,
    cell_h: f32,
    focused: bool,
) {
    let sel_norm = session.selection.map(normalize_sel);
    let parser = lock_ok(&session.parser);
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let origin = rect.min + egui::vec2(padding, padding);

    for r in 0..rows {
        for cix in 0..cols {
            let Some(cell) = screen.cell(r, cix) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let x = origin.x + cix as f32 * cell_w;
            let y = origin.y + r as f32 * cell_h;
            if y + cell_h > rect.max.y {
                break;
            }
            if x >= rect.max.x {
                continue;
            }
            let w = cell_w * f32::from(cell_draw_cols(screen, r, cix));
            let cell_rect = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, cell_h));

            let mut fg = cell_color(theme, cell.fgcolor(), true);
            let mut bg = match cell.bgcolor() {
                vt100::Color::Default => None,
                other => Some(cell_color(theme, other, false)),
            };
            if cell.inverse() {
                let old = fg;
                fg = bg.unwrap_or(theme.term_bg);
                bg = Some(old);
            }
            if let Some(bgc) = bg {
                painter.rect_filled(cell_rect, 0.0, bgc);
            }
            // 選択範囲のハイライト(文字色はそのまま、背景に半透明アクセント)
            if let Some(sel) = sel_norm {
                if cell_selected(r, cix, sel) {
                    painter.rect_filled(cell_rect, 0.0, theme.accent.gamma_multiply(0.3));
                }
            }
            let contents = cell.contents();
            if !contents.is_empty() && contents != " " {
                let color = if cell.bold() { brighten(fg) } else { fg };
                painter.text(
                    egui::pos2(x, y),
                    egui::Align2::LEFT_TOP,
                    contents,
                    font_id.clone(),
                    color,
                );
            }
            if cell.underline() {
                painter.line_segment(
                    [
                        egui::pos2(x, y + cell_h - 1.0),
                        egui::pos2(x + w, y + cell_h - 1.0),
                    ],
                    egui::Stroke::new(1.0_f32, fg),
                );
            }
        }
    }

    let (cr, cc) = screen.cursor_position();
    let (cur_col, cur_cells) = cursor_span(screen, cr, cc);
    let cursor_rect = egui::Rect::from_min_size(
        egui::pos2(
            origin.x + f32::from(cur_col) * cell_w,
            origin.y + f32::from(cr) * cell_h,
        ),
        egui::vec2(cell_w * f32::from(cur_cells), cell_h),
    );

    if session.scroll == 0 && !screen.hide_cursor() {
        // DECSCUSR の指定に合わせて形を変える(バー=挿入モード等)。
        // 点滅は目が疲れるうえ再描画が増えるので形だけ再現する。
        let shape = session.cursor_shape();
        let thin_w = (cell_w * 0.18).max(1.5);
        let thin_h = (cell_h * 0.14).max(1.5);
        let shape_rect = match shape {
            CursorShape::Block => cursor_rect,
            CursorShape::Underline => egui::Rect::from_min_max(
                egui::pos2(cursor_rect.min.x, cursor_rect.max.y - thin_h),
                cursor_rect.max,
            ),
            CursorShape::Bar => egui::Rect::from_min_max(
                cursor_rect.min,
                egui::pos2(cursor_rect.min.x + thin_w, cursor_rect.max.y),
            ),
        };
        if shape == CursorShape::Block {
            if focused {
                painter.rect_filled(cursor_rect, 1.0, theme.accent.gamma_multiply(0.55));
            } else {
                painter.rect_stroke(
                    cursor_rect,
                    1.0,
                    egui::Stroke::new(1.0_f32, theme.accent.gamma_multiply(0.7)),
                );
            }
        } else {
            // 細い形は薄いと見えないので、非フォーカス時も塗りで描く
            let a = if focused { 1.0 } else { 0.5 };
            painter.rect_filled(shape_rect, 1.0, theme.accent.gamma_multiply(a));
        }
    }

    if focused {
        // IME を有効化し、変換候補ウィンドウをカーソル位置に出す
        // (これが無いと日本語入力イベントが届かない)。
        //
        // egui-winit は `PlatformOutput::ime` が `Some` のときだけ
        // `Window::set_ime_allowed(true)` を呼び、その矩形を
        // `set_ime_cursor_area` に流す。つまりここを出すこと自体が
        // 「この端末は IME を受け付ける」の宣言になっている。
        //
        // 矩形は [`cursor_span`] で求めた**全角なら 2 桁ぶん**のカーソル矩形。
        // 1 桁で渡すと、全角を打っている最中の候補ウィンドウが半桁ずれる。
        //
        // 注意 (Linux): egui-winit 0.29 は Linux で IME イベントを丸ごと
        // 無視する (upstream egui#5008 の回避)。したがって Linux では
        // 未確定文字列のオーバーレイは出ず、確定文字列は `Event::Text` として
        // 届く経路になる — [`translate_input`] は Text も通常入力として
        // 扱うので、そのままでも文字は端末へ入る。
        ui.ctx().output_mut(|o| {
            o.mutable_text_under_cursor = true;
            o.ime = Some(egui::output::IMEOutput { rect, cursor_rect });
        });

        // IME 変換中の未確定文字列をカーソル位置にオーバーレイ表示。
        //
        // 幅は「確定したらグリッドで何桁になるか」で測る (文字数で測ると
        // 日本語が枠の 2 倍はみ出す)。等幅フォントでも合成文字や絵文字は
        // グリフ送りが桁数と一致しないので、桁数とグリフ幅の**広いほう**を
        // 場所取りに使い、端末の右端を越えるぶんだけ左へ寄せる
        // (寄せないと隣のパネルの上に未確定文字列が流れ出す)。
        if !session.preedit.is_empty() {
            let galley =
                painter.layout_no_wrap(session.preedit.clone(), font_id.clone(), theme.term_fg);
            let want =
                (crate::textenc::str_width(&session.preedit) as f32 * cell_w).max(galley.size().x);
            let left = rect.min.x + padding;
            let right = rect.max.x - padding;
            let x = cursor_rect.min.x.min((right - want).max(left));
            let pos = egui::pos2(x, cursor_rect.min.y);
            let bg = egui::Rect::from_min_size(pos, galley.size()).expand(1.0);
            painter.rect_filled(bg, 2.0, theme.accent.gamma_multiply(0.35));
            painter.galley(pos, galley, theme.term_fg);
            painter.line_segment(
                [
                    egui::pos2(bg.min.x, bg.max.y),
                    egui::pos2(bg.max.x, bg.max.y),
                ],
                egui::Stroke::new(1.5_f32, theme.accent),
            );
        }
    }
}

/// 端末 1 セルの寸法と、実際に描くのに使うフォント。
///
/// ── セルを物理ピクセルの整数へ揃える ──────────────────────────────
/// 桁位置は `origin.x + col * cell_w` で決まる。`cell_w` が小数のままだと
/// epaint は galley の位置だけを丸めるため、桁の間隔が 8/8/7/8 px と揺れて
/// 文字がガタガタに見える (100% 表示 = ppp 1.0 の Windows で最悪化する)。
/// フォントサイズも幅も高さも `theme::snap_*` で丸めてから使う。
/// 丸め結果が 0 になると桁数計算がゼロ除算になるので 1 物理ピクセルで底打ちする。
///
/// [`draw`] と [`apply_sizes`] の呼び出し側が**同じ値**を使えるように、
/// 計算はここ 1 箇所だけに置く (ずれると描いた矩形と PTY のグリッドが食い違う)。
pub fn cell_metrics(ui: &egui::Ui, font_size: f32) -> (egui::FontId, f32, f32) {
    let ppp = ui.ctx().pixels_per_point();
    let font_id = egui::FontId::monospace(crate::theme::snap_font_size(font_size, ppp));
    let (w, h) = ui.fonts(|f| {
        (
            crate::theme::snap_len(f.glyph_width(&font_id, 'M'), ppp).max(1.0 / ppp),
            crate::theme::snap_len(f.row_height(&font_id), ppp).max(1.0 / ppp),
        )
    });
    (font_id, w, h)
}

/// 右クリックメニューに出す直近コマンドの件数。
///
/// メニューは画面に浮くので、長すぎると下が見切れる (「どの幅でも見切れない」)。
/// 「さっき打ったやつ」を拾うのに 8 行あれば足りる。
const SHELL_MENU_ROWS: usize = 8;

/// 右クリックメニューのシェル統合セクション。
///
/// **段を必ず出す** — CLAUDE.md 設計原則 4 の「今どの段にいるか を UI に出す」。
/// ただし段が `None` (マーカーが 1 つも来ていない) のときは**見出しごと出さない**。
/// 常に「無効」と書いた行を置くのは「常に0を表示するバッジ」と同じで、
/// 情報ではなく雑音になる。
fn shell_integration_menu(ui: &mut egui::Ui, session: &mut Session, theme: &Theme) {
    use crate::shellint::Tier;
    let (tier, recorded, tier_log) = session.shell_status();
    if tier == Tier::None {
        return;
    }
    ui.separator();
    let head = ui.label(
        egui::RichText::new(trf(
            "{mark} シェル統合: {tier} ({n} 件)",
            &[
                ("mark", tier.mark().to_string()),
                ("tier", tr(tier.label())),
                ("n", recorded.to_string()),
            ],
        ))
        .small()
        .color(theme.text_dim),
    );
    // 「どうやってこの段になったか」はホバーで出す (常時出すと行が増えるだけ)。
    if let Some(log) = tier_log {
        head.on_hover_text(log);
    }
    if let Some(cmd) = session.shell_running_command() {
        let cmd = if cmd.is_empty() {
            tr("(コマンド行は不明)")
        } else {
            cmd
        };
        ui.label(
            egui::RichText::new(trf("▶ 実行中: {cmd}", &[("cmd", ellipsize(&cmd, 48))]))
                .small()
                .color(theme.accent),
        );
    }
    // 捨てたぶんは黙って消さない (設計原則 2 のギャップ標識)。
    if let Some(gap) = session.shell_gap_note() {
        ui.label(egui::RichText::new(gap).small().color(theme.text_dim));
    }
    let recent = session.shell_recent(SHELL_MENU_ROWS);
    if recent.is_empty() {
        return;
    }
    let mut insert: Option<String> = None;
    for c in &recent {
        let color = match c.ok() {
            Some(true) => theme.ok,
            Some(false) => theme.err,
            None => theme.text_dim,
        };
        let label = ellipsize(&c.summary(), 56);
        let hover = trf(
            "クリックで入力欄へ挿入 (Enter は送りません)\n{cmd}\n終了コード: {code} / 所要 {ms}ms",
            &[
                ("cmd", c.command_line.clone()),
                (
                    "code",
                    c.exit_code
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| tr("不明")),
                ),
                ("ms", c.duration_ms().to_string()),
            ],
        );
        let btn = egui::Button::new(egui::RichText::new(label).color(color)).frame(false);
        // コマンド行を知らない段では押しても入れるものが無いので押させない。
        let r = ui
            .add_enabled(!c.command_line.is_empty(), btn)
            .on_hover_text(hover);
        if r.clicked() {
            insert = Some(c.command_line.clone());
        }
    }
    if let Some(line) = insert {
        // **Enter は送らない。** 誤クリックで `rm -rf` が走る作りにしない。
        // 打ち直しの手間を消すのが目的で、勝手に実行するのは目的ではない。
        session.write_bytes(line.as_bytes());
        session.note_user_input();
        ui.close_menu();
    }
}

// ─── シェル統合の描画 (装飾 / sticky / ジャンプ) ──────────────────────

/// sticky 帯の矩形 (純関数)。**本文の最上行に重ねる。**
///
/// 行を 1 本挿し込む形にすると、コマンドが変わるたびに本文が上下する
/// (「画面が突然変わらない」に反する)。重ねれば本文の座標は 1 ピクセルも
/// 動かない。本文が 2 行ぶんも取れない小さな端末では `None` —
/// 帯で画面を埋めてしまわない。
pub fn shell_sticky_rect(rect: egui::Rect, padding: f32, cell_h: f32) -> Option<egui::Rect> {
    let w = rect.width() - padding * 2.0;
    if w <= 0.0 || cell_h <= 0.0 || rect.height() < padding * 2.0 + cell_h * 2.0 {
        return None;
    }
    Some(egui::Rect::from_min_size(
        egui::pos2(rect.min.x + padding, rect.min.y + padding),
        egui::vec2(w, cell_h),
    ))
}

/// コマンドの印の矩形 (純関数)。**左の余白の中だけ**に収める。
///
/// 本文は `rect.min.x + padding` から始まるので、そこへ食い込むと
/// 1 桁目の選択が奪われる。余白より内側へは絶対に出さない。
pub fn shell_mark_rect(
    rect: egui::Rect,
    padding: f32,
    cell_h: f32,
    row: u16,
) -> Option<egui::Rect> {
    let w = (padding - 2.0).min(4.0);
    if w <= 0.0 || cell_h <= 0.0 {
        return None;
    }
    let top = rect.min.y + padding + f32::from(row) * cell_h;
    if top + cell_h > rect.max.y - padding + 0.5 {
        return None;
    }
    Some(egui::Rect::from_min_size(
        egui::pos2(rect.min.x + 1.0, top),
        egui::vec2(w, cell_h),
    ))
}

/// 印と帯の色。終了コード不明は**印を付けない** (`None`)。
fn shell_mark_color(theme: &Theme, ok: Option<bool>, running: bool) -> Option<egui::Color32> {
    if running {
        return Some(theme.accent);
    }
    match ok {
        Some(true) => Some(theme.ok),
        Some(false) => Some(theme.err),
        None => None,
    }
}

/// コマンド装飾 (左の印) と sticky 帯を描く。
///
/// **シェル統合が来ていない端末では 1 ピクセルも描かない** — `shell_marks`
/// が空を返し、`shell_sticky` が `None` を返すので、この関数は
/// 何もせずに戻る (印も帯も出ない)。
fn draw_shell_decorations(
    ui: &mut egui::Ui,
    session: &mut Session,
    theme: &Theme,
    rect: egui::Rect,
    padding: f32,
    cell_w: f32,
    cell_h: f32,
) {
    let marks = session.shell_marks();
    let sticky = session.shell_sticky();
    if marks.is_empty() && sticky.is_none() {
        return;
    }
    let painter = ui.painter_at(rect);
    let mut jump_to: Option<u64> = None;
    let mut copy: Option<(u64, u64)> = None;
    let mut insert: Option<String> = None;

    for m in &marks {
        let Some(color) = shell_mark_color(theme, m.ok, m.running) else {
            continue;
        };
        let Some(r) = shell_mark_rect(rect, padding, cell_h, m.row) else {
            continue;
        };
        let res = ui.allocate_new_ui(egui::UiBuilder::new().max_rect(r), |ui| {
            ui.spacing_mut().button_padding = egui::vec2(0.0, 0.0);
            let btn = egui::Button::new("")
                .fill(color)
                .rounding(1.5)
                .min_size(r.size());
            egui::menu::menu_custom_button(ui, btn, |ui| {
                ui.label(
                    egui::RichText::new(ellipsize(&m.summary, 56))
                        .small()
                        .color(theme.text_dim),
                );
                if m.output.is_some() && ui.button(tr("出力を丸ごとコピー")).clicked() {
                    copy = m.output;
                    ui.close_menu();
                }
                if !m.command.is_empty()
                    && ui
                        .button(tr("もう一度実行する (入力欄へ入れる)"))
                        .on_hover_text(tr(
                            "Enter は送りません。誤クリックで消えないものが消える作りにしない",
                        ))
                        .clicked()
                {
                    insert = Some(m.command.clone());
                    ui.close_menu();
                }
            })
            .response
        });
        res.inner.on_hover_text(ellipsize(&m.summary, 80));
    }

    if let Some(st) = &sticky {
        if let Some(band) = shell_sticky_rect(rect, padding, cell_h) {
            painter.rect_filled(band, 3.0, theme.term_bg.gamma_multiply(0.92));
            painter.rect_stroke(
                band,
                3.0,
                egui::Stroke::new(1.0_f32, theme.text_dim.gamma_multiply(0.35)),
            );
            if let Some(c) = shell_mark_color(theme, st.ok, st.running) {
                painter.rect_filled(
                    egui::Rect::from_min_size(band.min, egui::vec2(3.0, band.height())),
                    1.5,
                    c,
                );
            }
            let budget = ((band.width() - 10.0) / cell_w).floor().max(1.0) as usize;
            painter.text(
                egui::pos2(band.min.x + 8.0, band.center().y),
                egui::Align2::LEFT_CENTER,
                ellipsize(&st.text, budget),
                egui::FontId::proportional((cell_h * 0.72).clamp(9.0, 14.0)),
                theme.text_dim,
            );
            let res = ui.interact(
                band,
                ui.id().with(("zv-term-sticky", session.id)),
                egui::Sense::click(),
            );
            if res
                .on_hover_text(tr(
                    "いま見えている出力を出したコマンド。クリックでその行へ戻る",
                ))
                .clicked()
            {
                jump_to = Some(st.prompt);
            }
        }
    }

    if let Some(line) = jump_to {
        session.shell_scroll_to_line(line);
    }
    if let Some(range) = copy {
        let text = session.shell_block_output(range);
        if !text.is_empty() {
            ui.ctx().copy_text(text);
        }
    }
    if let Some(line) = insert {
        session.write_bytes(line.as_bytes());
        session.note_user_input();
    }
}

/// 表示用に `max` 文字で省略する (全文はホバーで出す)。
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Render a terminal session. `interactive` forwards keyboard input on focus,
/// `allow_resize` lets this view drive the PTY size.
/// `hover_scroll`: ホバーだけでホイールを履歴スクロールに使うか。
/// false ならフォーカス中のみ消費し、それ以外は外側の ScrollArea に抜ける
/// (Cockpit グリッドがミニターミナルで埋まってもページをスクロールできる)。
pub fn draw(
    ui: &mut egui::Ui,
    session: &mut Session,
    theme: &Theme,
    font_size: f32,
    interactive: bool,
    allow_resize: bool,
    hover_scroll: bool,
) -> egui::Response {
    let (font_id, cell_w, cell_h) = cell_metrics(ui, font_size);

    let avail = ui.available_size();
    let desired = egui::vec2(avail.x.max(120.0), avail.y.max(50.0));
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::click_and_drag());

    if interactive && (response.clicked() || response.drag_started()) {
        response.request_focus();
    }
    let focused = interactive && response.has_focus();

    // ── ドラッグ&ドロップでパスをプロンプトへ挿入 ──
    // ファイルツリーの行 (内部ドラッグ) と OS からのファイルドロップの両方を受ける。
    // 送信 (Enter) はしない — 入力欄に @パス が入るだけなので、暴発しない。
    if let Some(path) = response.dnd_release_payload::<PathBuf>() {
        let text = format!("@{} ", prompt_path(&path, &session.cwd));
        session.write_bytes(text.as_bytes());
    }
    let os_dropped: Vec<egui::DroppedFile> = ui.input(|i| i.raw.dropped_files.clone());
    if !os_dropped.is_empty() && ui.rect_contains_pointer(rect) {
        let mut text = String::new();
        for f in &os_dropped {
            if let Some(p) = &f.path {
                text.push_str(&format!("@{} ", prompt_path(p, &session.cwd)));
            }
        }
        if !text.is_empty() {
            session.write_bytes(text.as_bytes());
            // エディタ側の既定処理 (タブで開く) と二重にならないよう印を立てる
            ui.ctx()
                .data_mut(|d| d.insert_temp(egui::Id::new("zv-drop-consumed"), true));
        }
    }
    // ドラッグ中はドロップ先が分かるよう枠を光らせる
    let dragging_file = response.dnd_hover_payload::<PathBuf>().is_some()
        || (ui.input(|i| !i.raw.hovered_files.is_empty()) && ui.rect_contains_pointer(rect));
    if dragging_file {
        ui.painter()
            .rect_stroke(rect, 6.0, egui::Stroke::new(2.0_f32, theme.accent));
    }

    // 分割レイアウト側の [`apply_sizes`] と同じ値を使う (ずれると
    // 「描いた矩形と PTY のグリッド」が食い違う)。
    let padding = TERM_PADDING;
    if allow_resize {
        let cols = ((rect.width() - padding * 2.0) / cell_w).floor() as u16;
        let rows = ((rect.height() - padding * 2.0) / cell_h).floor() as u16;
        session.resize(rows, cols);
        if session.resize_pending() {
            // 安定カウント (RESIZE_STABLE_FRAMES) が完走する前に再描画が
            // 止まると、最終サイズが PTY へ届かないまま残る。完走するまで
            // フレームを回し続けて取りこぼしを防ぐ (高々 K フレーム)。
            crate::perf::repaint(ui.ctx(), "term_focus");
        }
    }

    // ── マウスによる文字選択(ドラッグ=範囲 / ダブルクリック=語 / トリプルクリック=行) ──
    // リンクを開いてよいかは「このクリックの前に選択があったか」で変わるが、
    // handle_mouse_selection はクリックで選択を消してしまう。先に控えておく。
    // 出力で画面が流れたぶんをここで 1 回だけ取り込む。以降この関数の中では
    // `session.scroll` と `session.selection` が**パーサと同じ現在**を指す。
    session.adopt_scrolled_output();
    let had_selection = session.selection.is_some();
    if interactive {
        // 押し始めの座標。ドラッグ選択とクリックの見分けに使う。
        // egui の press_origin は離した瞬間のフレームで None に戻っているので、
        // 押されている間 (= Some の間) にこちらへ写しておく。
        if let Some(p) = ui.input(|i| i.pointer.press_origin()) {
            session.links.press_at = Some(p);
        }
        handle_mouse_selection(session, &response, rect, padding, cell_w, cell_h);
    }

    // 代替画面(Claude Code / vim / less 等)にスクロールバック履歴は無いため、
    // 切替後も古い履歴ビューが画面に残らないよう自動で一番下へ戻す。
    if session.scroll > 0 && session.wheel_modes().0 {
        session.set_scroll(0);
    }

    // Cmd+F ルーティング用に「どの端末がフォーカス中か」を egui 一時データへ
    // 残す。app 側のグローバルショートカット処理は (パネル描画より先に走るので)
    // 前フレームのこの値を読んで、エディタ検索か端末内検索かを振り分ける。
    let focus_flag = egui::Id::new("zv-focused-terminal");
    if focused {
        ui.data_mut(|d| d.insert_temp(focus_flag, session.id));
    } else if ui.data(|d| d.get_temp::<u64>(focus_flag)) == Some(session.id) {
        ui.data_mut(|d| d.remove::<u64>(focus_flag));
    }

    if focused {
        forward_keyboard_input(ui, session, response.id);
    } else if !session.preedit.is_empty() {
        session.preedit.clear();
    }

    if interactive && (focused || hover_scroll) && response.hovered() {
        handle_wheel_scroll(ui, session, rect, padding, cell_w, cell_h);
    }

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, theme.term_bg);

    // 描く前にパーサの健康診断。poison していたらここで作り直す
    // (壊れたグリッドを読んで描画側が落ち、以後ずっと真っ黒…を防ぐ)。
    session.ensure_parser_healthy();

    draw_screen(
        ui, &painter, session, theme, &font_id, rect, padding, cell_w, cell_h, focused,
    );

    // シェル統合の印と sticky 帯。マーカーが来ていなければ何も描かない。
    draw_shell_decorations(ui, session, theme, rect, padding, cell_w, cell_h);

    // リンク (URL / ファイル:行:桁) のホバー表示とクリック。
    // 走査するのはポインタ下の 1 行だけなので、静止中のコストは 0。
    if interactive && response.hovered() {
        handle_links(
            ui,
            &painter,
            session,
            theme,
            &response,
            rect,
            padding,
            cell_w,
            cell_h,
            had_selection,
        );
    }

    // 端末内検索 (Cmd+F): 表示中画面のヒットをハイライトし、バーを浮かせる
    if session.search.open && !session.search.query.is_empty() {
        paint_search_highlights(&painter, session, theme, rect, padding, cell_w, cell_h);
    }
    if interactive && session.search.open {
        terminal_search_bar_ui(ui, session, theme, rect);
    }

    // 履歴表示中だけ「⤓ 一番下へ」ボタンを出す。一番下(scroll == 0)なら何も表示しない。
    if session.scroll > 0 {
        let label = trf("⤒ {n} ⤓ 一番下へ", &[("n", session.scroll.to_string())]);
        let galley = painter.layout_no_wrap(
            label.clone(),
            egui::FontId::proportional(11.0),
            theme.term_bg,
        );
        let btn_size = galley.size() + egui::vec2(14.0, 6.0);
        let btn_rect = egui::Rect::from_min_size(
            egui::pos2(rect.max.x - btn_size.x - 8.0, rect.min.y + 6.0),
            btn_size,
        );
        let r = ui.put(
            btn_rect,
            egui::Button::new(egui::RichText::new(label).size(11.0).color(theme.term_bg))
                .fill(theme.warn)
                .rounding(4.0),
        );
        if r.on_hover_text(tr("クリックで履歴表示を終了して一番下(最新)へ戻る"))
            .clicked()
        {
            session.set_scroll(0);
        }
    }

    if focused {
        painter.rect_stroke(
            rect,
            6.0,
            egui::Stroke::new(1.0_f32, theme.accent.gamma_multiply(0.55)),
        );
    }

    if !session.running() {
        let code = lock_ok(&session.exit_code).unwrap_or(0);
        painter.text(
            egui::pos2(rect.max.x - 8.0, rect.max.y - 6.0),
            egui::Align2::RIGHT_BOTTOM,
            trf("✕ 終了 (code {code})", &[("code", code.to_string())]),
            egui::FontId::proportional(11.0),
            theme.err,
        );
    }

    // シェル統合の段と直近の終了コードを左下に小さく出す。
    //
    // **段が None のときは何も描かない** = 使っていない人の画面は 1 px も
    // 変わらない (「画面が突然変わらない」)。出すのは 3 語ぶんの幅だけで、
    // 色が直近コマンドの成否 (終了コードという事実) を表す。
    if let Some((tier, exit)) = session.shell_badge() {
        let color = match exit {
            Some(0) => theme.ok,
            Some(_) => theme.err,
            None => theme.text_dim,
        };
        let text = match exit {
            Some(code) if code != 0 => trf(
                "{mark} shell {tier} · exit {code}",
                &[
                    ("mark", tier.mark().to_string()),
                    ("tier", tr(tier.label())),
                    ("code", code.to_string()),
                ],
            ),
            _ => trf(
                "{mark} shell {tier}",
                &[
                    ("mark", tier.mark().to_string()),
                    ("tier", tr(tier.label())),
                ],
            ),
        };
        painter.text(
            egui::pos2(rect.min.x + 8.0, rect.max.y - 6.0),
            egui::Align2::LEFT_BOTTOM,
            text,
            egui::FontId::proportional(10.0),
            color.gamma_multiply(0.85),
        );
    }

    // 右クリックメニュー: コピー操作
    if interactive {
        response.context_menu(|ui| {
            // 判定は真実源 (絶対座標) で。スクロールで画面の外へ出ただけの
            // 選択まで「無い」にすると、コピーが押せなくなる。
            let has_sel = session.sel_abs.is_some();
            // 打鍵の表記はベタ書きしない。コピーは egui-winit 固定の打鍵、
            // 検索は再割り当てできるので app 側が配った表記を使う。
            let copy_key = crate::keybinds::format_shortcut(egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND,
                egui::Key::C,
            ));
            let find_key = key_hint(ui.ctx(), BindAction::Find);
            if ui
                .add_enabled(
                    has_sel,
                    egui::Button::new(trf("📋 選択をコピー ({key})", &[("key", copy_key)])),
                )
                .clicked()
            {
                copy_selection(ui, session);
                ui.close_menu();
            }
            if ui.button(tr("📄 画面全体をコピー")).clicked() {
                let text = lock_ok(&session.parser).screen().contents();
                ui.ctx().copy_text(text);
                session.copied_at = Some(Instant::now());
                ui.close_menu();
            }
            if ui
                .button(trf("🔍 端末内を検索 ({key})", &[("key", find_key)]))
                .clicked()
            {
                session.search.open = true;
                session.search.focus_pending = true;
                ui.close_menu();
            }
            if has_sel && ui.button(tr("✕ 選択を解除")).clicked() {
                session.clear_selection();
                ui.close_menu();
            }
            shell_integration_menu(ui, session, theme);
        });
    }

    // コピー完了フィードバック(短時間表示して自動で消える)
    if let Some(t) = session.copied_at {
        if t.elapsed().as_millis() < 1200 {
            let galley = painter.layout_no_wrap(
                tr("📋 コピーしました"),
                egui::FontId::proportional(12.0),
                theme.term_bg,
            );
            let bg = egui::Rect::from_center_size(
                egui::pos2(rect.center().x, rect.min.y + 8.0 + galley.size().y * 0.5),
                galley.size() + egui::vec2(14.0, 6.0),
            );
            painter.rect_filled(bg, 8.0, theme.accent);
            painter.galley(bg.min + egui::vec2(7.0, 3.0), galley, theme.term_bg);
            crate::perf::repaint_after(
                ui.ctx(),
                std::time::Duration::from_millis(150),
                "term_osc52",
            );
        } else {
            session.copied_at = None;
        }
    }

    response
}

/// セッションを畳む経路 ([`reap`] / [`abandon`] / [`Session::kill`]) の取り決め。
///
/// 実体は「別スレッドへ持ち出してから ConPTY を閉じる」ことなので、
/// 効いているかどうかはコンパイル時の `Send` 境界と、組み立てるコマンドで見る。
#[cfg(test)]
mod reap_tests {
    use super::*;

    /// `reap` / `abandon` はセッションを別スレッドへ渡す。渡せなくなったら
    /// ConPTY の後始末が UI スレッドへ戻り、閉じた瞬間にアプリが固まる。
    #[test]
    fn session_can_leave_the_ui_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Session>();
    }

    /// 直接の子だけでなく**子孫まで**落とすこと。PTY にぶら下がるのは
    /// cmd.exe / ログインシェルで、エージェント本体はその孫。孫が残ると
    /// Windows では `ClosePseudoConsole` が返らなくなる。
    #[test]
    fn kill_reaches_the_whole_tree() {
        let cmd = kill_tree_command(4321);
        let prog = cmd.get_program().to_string_lossy().into_owned();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        #[cfg(windows)]
        {
            assert_eq!(prog, "taskkill");
            assert!(args.contains(&"/T".to_string()), "子孫を含めない: {args:?}");
            assert!(args.contains(&"/F".to_string()), "強制終了でない: {args:?}");
            assert_eq!(args.last().map(String::as_str), Some("4321"));
        }
        #[cfg(not(windows))]
        {
            assert_eq!(prog, "kill");
            // 先頭の `-` がプロセスグループ指定。これが無いと孫が残る。
            // `--` を落とすと Linux の procps-ng kill が `-4321` を短オプションの
            // まとめ書きと解釈し、`kill(-4, SIGKILL)` = 無関係なグループへ撃つ
            // (先頭が 1 の PID なら `kill(-1, …)` で全プロセス道連れ)。
            // しかも終了コードは 0 なので誰も気付けない。回帰防止にここで固定する。
            assert_eq!(
                args,
                vec!["-KILL".to_string(), "--".to_string(), "-4321".to_string()],
                "`--` が無いと Linux で kill が別のプロセスグループを撃つ"
            );
        }
    }

    /// PID が取れなかったセッションでも、畳む処理が止まらないこと。
    #[test]
    fn missing_pid_is_not_fatal() {
        assert!(!kill_tree_blocking(None));
    }

    /// Drop の木殺しは「終了済みセッションへ kill を撃たない」ガード
    /// (b27faf5) を必ず通ること。wait 済みの PID は再利用され得るため、
    /// exited なら相手が何であれ撃ってはいけない。
    #[test]
    fn kill_target_honors_the_already_exited_guard() {
        // (exited, child_pid, 期待, 説明)
        let cases = [
            (false, Some(42), Some(42), "生きている子は撃ってよい"),
            (
                true,
                Some(42),
                None,
                "終了済み → PID 再利用の巻き添え防止で撃たない",
            ),
            (
                false,
                None,
                None,
                "PID 不明なら木は辿れない (killer の保険のみ)",
            ),
            (true, None, None, "終了済み + PID 不明も当然撃たない"),
        ];
        for (exited, pid, want, why) in cases {
            assert_eq!(kill_target(exited, pid), want, "{why}");
        }
    }
}

/// PTY への書き込みが呼び出し側 (UI スレッド) を止めないことの取り決め。
#[cfg(test)]
mod pty_writer_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 受け手が一切読まない writer を模して、送り側だけを見る。
    fn stalled_writer() -> (PtyWriter, std::sync::mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let queued = Arc::new(AtomicUsize::new(0));
        (PtyWriter { tx, queued }, rx)
    }

    /// 子が入力を一切読まなくても、書き込みは即座に返ること。
    /// ここで待たされると `App::update` の途中で画面が止まる。
    #[test]
    fn writing_to_a_stalled_child_does_not_block() {
        let (w, _rx) = stalled_writer();
        let t = Instant::now();
        for _ in 0..1000 {
            w.send(b"hello\r");
        }
        let took = t.elapsed();
        assert!(
            took < Duration::from_millis(200),
            "読まれない PTY への書き込みが呼び出し側を待たせた ({took:?})"
        );
    }

    /// 届かない入力を無制限に溜め込まないこと。上限を超えたら捨てる
    /// (子が読んでいない以上、積んでもメモリを食うだけで届かない)。
    #[test]
    fn the_backlog_is_capped() {
        let (w, _rx) = stalled_writer();
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..64 {
            w.send(&chunk);
        }
        let queued = w.queued.load(Ordering::Relaxed);
        assert!(
            queued <= PtyWriter::MAX_QUEUED + chunk.len(),
            "待ち行列が上限を超えて育った: {queued}"
        );
    }

    /// 実 PTY で、書いた入力が本当に子へ届くこと。
    /// 書き込みを非同期にした分、ここが壊れると無音で入力が消える。
    #[test]
    fn input_reaches_the_child_through_the_queue() {
        let spec = SpawnSpec {
            title: "w".into(),
            preset_name: "w".into(),
            icon: "w".into(),
            command: String::new(), // 素のシェル
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut s = Session::spawn(1, spec, egui::Context::default()).unwrap();
        // シェルが入力を受け付けるようになるまで待ってから打つ
        std::thread::sleep(Duration::from_millis(1500));
        s.write_bytes(b"echo ZAIVERNPING\r");

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = false;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            if lock_ok(&s.parser)
                .screen()
                .contents()
                .contains("ZAIVERNPING")
            {
                seen = true;
                break;
            }
        }
        reap(s);
        assert!(seen, "待ち行列越しの入力が子へ届かなかった");
    }

    /// writer スレッドが畳まれた後に書いても、数えた分が残らないこと
    /// (残ると上限に張り付いて、再開しても以後の入力が捨てられ続ける)。
    #[test]
    fn counter_is_restored_when_the_writer_is_gone() {
        let (w, rx) = stalled_writer();
        drop(rx);
        w.send(b"gone");
        assert_eq!(w.queued.load(Ordering::Relaxed), 0);
    }
}

/// バグ本体の回帰テスト: 「動いているエージェントを閉じるとアプリが固まって戻らない」。
///
/// 実 PTY で**孫プロセス**を持つセッションを起こして閉じる。孫を作るのが肝で、
/// PTY に直接ぶら下がるのはシェルであり、エージェント本体はその下にいる。
/// 以前は孫が生き残って PTY に繋がったままになり、`ClosePseudoConsole`
/// (= master の Drop) が UI スレッドで返らなくなっていた。
#[cfg(test)]
mod reap_pty_tests {
    use super::*;

    /// 孫プロセスが「生きている間ずっとファイルへ書き足す」セッションを起こす。
    /// 戻り値は (セッション, 監視するファイル)。
    fn spawn_with_a_noisy_grandchild(tag: &str) -> (Session, PathBuf) {
        let dir = std::env::temp_dir().join(format!("zaivern-reap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let probe = dir.join(format!("{tag}.txt"));
        let _ = std::fs::remove_file(&probe);
        let p = probe.display().to_string();

        // シェルがそのまま exec してしまわないよう、必ず「シェルの子」を起こす。
        #[cfg(windows)]
        let command = format!(
            "powershell -NoProfile -Command \"while($true){{ Add-Content -LiteralPath '{p}' -Value 'x'; Start-Sleep -Milliseconds 100 }}\""
        );
        #[cfg(not(windows))]
        let command = format!("/bin/sh -c 'while true; do echo x >> {p}; sleep 0.1; done'");

        let spec = SpawnSpec {
            title: "reap".into(),
            preset_name: "reap".into(),
            icon: "r".into(),
            command,
            cwd: dir,
            env: HashMap::new(),
            log_path: None,
        };
        let session = Session::spawn(1, spec, egui::Context::default()).unwrap();
        (session, probe)
    }

    /// `probe` が育ち始めるまで待つ。育たなければ None (環境が実行できていない)。
    fn wait_until_growing(probe: &Path) -> Option<u64> {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            if let Ok(m) = std::fs::metadata(probe) {
                if m.len() > 0 {
                    return Some(m.len());
                }
            }
        }
        None
    }

    fn len_of(probe: &Path) -> u64 {
        std::fs::metadata(probe).map(|m| m.len()).unwrap_or(0)
    }

    /// 閉じる操作そのものが呼び出し側 (UI スレッド) を待たせないこと。
    /// ここが待たされると `App::update` の途中で画面が止まり、二度と戻らない。
    #[test]
    fn closing_a_running_agent_returns_immediately() {
        let (session, probe) = spawn_with_a_noisy_grandchild("nonblock");
        if wait_until_growing(&probe).is_none() {
            // 孫を起こせない環境。ここで落としても得るものが無いので見送る。
            reap(session);
            return;
        }
        let t = Instant::now();
        reap(session);
        let took = t.elapsed();
        assert!(
            took < Duration::from_millis(300),
            "閉じる操作が呼び出し側を待たせた ({took:?})。UI スレッドならここで固まる"
        );
    }

    /// 木を辿るのは根 (シェル) が**生きているうち**でなければならない。
    /// 先に根を落としてしまうと `taskkill /T` は根を見つけられず、
    /// 孫がそのまま取り残される (= PTY を掴んだままになる)。
    #[test]
    fn the_tree_is_walked_while_the_root_is_still_alive() {
        let (mut session, probe) = spawn_with_a_noisy_grandchild("order");
        if wait_until_growing(&probe).is_none() {
            reap(session);
            return;
        }
        // 根が生きている状態なら木を辿れる。
        assert!(
            kill_tree_blocking(session.child_pid),
            "生きているセッションの木を辿れなかった"
        );
        // 木が消えた後は、同じ PID を渡しても辿れない — だから順序が要る。
        //
        // 消えるまでは待つ (固定 sleep にしない)。上の一撃で木は全員 SIGKILL
        // されているが、実際に「居なくなる」のは wait/reap が済んでからで、
        // 混んだ CI では孤児の引き取り (init への再ペアレント) が数百 ms 遅れる。
        // ゾンビもプロセスグループの一員なので、その間は辿れて当然。
        let _ = session.killer.kill();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut unreachable = false;
        while Instant::now() < deadline {
            if !kill_tree_blocking(session.child_pid) {
                unreachable = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            unreachable,
            "木が消えた後も辿れてしまう (pid={:?})。\
             kill が到達性を正直に報告していない — Linux なら `kill -KILL -- -PID` の \
             `--` が落ちて別グループを撃っている可能性が高い (kill_tree_command 参照)",
            session.child_pid
        );
        reap(session);
    }

    /// 閉じたら**孫まで**止まること。孫が残ると PTY を掴んだままになり、
    /// 次に閉じるときの `ClosePseudoConsole` が返らなくなる
    /// (エージェントが裏で走り続けてトークンを食う問題でもある)。
    #[test]
    fn closing_a_running_agent_stops_the_whole_tree() {
        let (session, probe) = spawn_with_a_noisy_grandchild("tree");
        if wait_until_growing(&probe).is_none() {
            reap(session);
            return;
        }
        reap(session);

        // 落ち切るまでの猶予。taskkill / kill は別プロセスなので少し待つ。
        let deadline = Instant::now() + Duration::from_secs(10);
        let quiet = loop {
            let before = len_of(&probe);
            std::thread::sleep(Duration::from_millis(500));
            if len_of(&probe) == before {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
        };
        assert!(
            quiet,
            "閉じたのに孫プロセスが書き続けている: {}",
            probe.display()
        );
    }

    /// [`reap`] / [`abandon`] を通さず **drop しただけ**でも孫まで止まること
    /// (Drop の木殺し)。テストや異常系はこの経路で畳まれるため、ここが
    /// 単発 kill のままだとログインシェルの子・孫が生き残り、実 PTY テストの
    /// たびにプロセスツリーが漏れて CI ランナーを飢えさせる。
    #[test]
    fn dropping_a_session_stops_the_whole_tree() {
        let (session, probe) = spawn_with_a_noisy_grandchild("droptree");
        if wait_until_growing(&probe).is_none() {
            drop(session);
            return;
        }
        drop(session);

        // 落ち切るまでの猶予 (Windows の taskkill は別プロセスなので少し待つ)。
        let deadline = Instant::now() + Duration::from_secs(10);
        let quiet = loop {
            let before = len_of(&probe);
            std::thread::sleep(Duration::from_millis(500));
            if len_of(&probe) == before {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
        };
        assert!(
            quiet,
            "drop したのに孫プロセスが書き続けている: {}",
            probe.display()
        );
    }
}

#[cfg(test)]
mod shell_args_tests {
    use super::*;

    /// エージェントは「コードページを上げてから」起動する。
    /// この前置きが外れると、日本語 Windows で出力が化けて読めなくなる。
    /// コマンド自体は環境変数から読むので、コマンドラインには載らない。
    #[test]
    fn command_runs_after_switching_to_utf8() {
        let args = windows_shell_args(true);
        assert_eq!(args[0], "/C");
        assert_eq!(args[1], "chcp 65001 >nul 2>nul & %ZAIVERN_CMD%");
    }

    /// 繋ぎは `&`。`&&` にすると chcp が使えない環境でエージェントが起動しなくなる。
    #[test]
    fn chcp_failure_must_not_block_the_agent() {
        let args = windows_shell_args(true);
        assert!(args[1].contains(" & %"), "実際: {}", args[1]);
        assert!(!args[1].contains("&& %"), "&& だと起動できない環境が出る");
    }

    /// 素のシェルは `/K` — `/C` だと chcp 直後に cmd が終わって端末が即閉じる。
    #[test]
    fn plain_shell_keeps_running() {
        assert_eq!(
            windows_shell_args(false),
            vec!["/K", "chcp 65001 >nul 2>nul"]
        );
    }

    /// `%NAME%` は自分で解決する (環境変数経由では cmd が二度目の展開をしないため)。
    /// 定義済みの名前だけを置き換え、未定義はそのまま残す。
    #[test]
    fn env_refs_are_expanded_from_the_lookup() {
        let look = |n: &str| match n {
            "HOMEDIR" => Some(r"C:\Users\山田 太郎".to_string()),
            _ => None,
        };
        assert_eq!(
            expand_windows_env_refs(r"%HOMEDIR%\bin\tool.exe --x", &look),
            r"C:\Users\山田 太郎\bin\tool.exe --x"
        );
        assert_eq!(
            expand_windows_env_refs("%NOPE% --x", &look),
            "%NOPE% --x",
            "未定義の名前は cmd と同じくそのまま残す"
        );
    }

    /// `%` を含むだけの文字列を壊さないこと (`echo 50%` など)。
    #[test]
    fn stray_percent_signs_survive() {
        let none = |_: &str| None;
        assert_eq!(expand_windows_env_refs("echo 50%", &none), "echo 50%");
        assert_eq!(expand_windows_env_refs("echo %%", &none), "echo %%");
        assert_eq!(expand_windows_env_refs("", &none), "");
        assert_eq!(
            expand_windows_env_refs("claude --p \"率 100%\"", &none),
            "claude --p \"率 100%\""
        );
    }

    /// 引用符・空白を含むコマンドは一切加工しない (cmd が読む形をそのまま保つ)。
    #[test]
    fn quotes_and_spaces_are_left_untouched() {
        let none = |_: &str| None;
        let cmd = r#""C:\Program Files\zai\gemini.cmd" --flag "日本語の指示""#;
        assert_eq!(expand_windows_env_refs(cmd, &none), cmd);
    }
}

/// ConPTY を実際に開いて、UTF-8 のバイト列が化けずに画面へ出ることを確かめる。
///
/// `type` はファイルの中身を**そのままコンソールへ書く**ので、コードページが
/// OEM (日本語 Windows なら 932) のままだと UTF-8 の日本語がそこで壊れる —
/// ユーザーが報告した「メッセージが文字化けする」と同じ経路を踏む回帰テスト。
///
/// 置き場所は OS の一時ディレクトリで、**名前に空白と日本語を含める**。
/// 引用符付きのパスをコマンドラインに直接並べていた頃はここで起動に失敗していた
/// (`\"` を cmd が解釈できない) ので、その再発もこのテストで捕まる。
#[cfg(test)]
#[cfg(windows)]
mod pty_utf8_tests {
    use super::*;

    #[test]
    fn utf8_output_reaches_the_screen_unmangled() {
        let dir = std::env::temp_dir().join(format!("zaivern cp テスト {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("出力 sample.txt");
        let body = "進捗: 日本語のメッセージが化けないこと";
        std::fs::write(&path, body.as_bytes()).unwrap();

        let spec = SpawnSpec {
            title: "cp".into(),
            preset_name: "cp".into(),
            icon: "cp".into(),
            // エージェントと同じ条件にするため、chcp の**後に起動する別プロセス**へ
            // 書かせる。cmd の内蔵コマンド (type / echo) は自分の起動時に読んだ
            // コードページを使い続けるので、内蔵のままでは検証にならない。
            // 空白と日本語を含むパスを引用符付きで渡すので、
            // コマンドラインの二重解釈が戻れば起動に失敗して落ちる。
            command: format!("cmd /C type \"{}\"", path.display()),
            cwd: dir.clone(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut sess = Session::spawn(9001, spec, egui::Context::default()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        let mut screen = String::new();
        while Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            screen = lock_ok(&sess.parser).screen().contents();
            if screen.contains(body) || screen.contains('\u{fffd}') {
                break;
            }
        }
        sess.kill();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            screen.contains(body),
            "UTF-8 の出力が化けている / 起動できていない:\n{screen}"
        );
    }
}

#[cfg(test)]
mod query_tests {
    use super::*;

    /// 走査してイベント列を得る(1チャンク版)。
    fn scan1(input: &[u8]) -> Vec<TermEvent> {
        QueryScanner::default().scan(input)
    }

    /// Reply イベントのバイト列を全部つなげる。
    fn replies(evs: &[TermEvent]) -> Vec<u8> {
        let mut v = Vec::new();
        for e in evs {
            if let TermEvent::Reply(b) = e {
                v.extend_from_slice(b);
            }
        }
        v
    }

    #[test]
    fn dsr_cursor_position_is_one_based() {
        assert_eq!(scan1(b"\x1b[6n"), vec![TermEvent::CursorReport]);
        // 0始まりの (0,0) は 1始まりの 1;1
        assert_eq!(cursor_report(0, 0, false), b"\x1b[1;1R".to_vec());
        assert_eq!(cursor_report(11, 4, false), b"\x1b[12;5R".to_vec());
        // DECXCPR は "?" 付き
        assert_eq!(scan1(b"\x1b[?6n"), vec![TermEvent::ExtCursorReport]);
        assert_eq!(cursor_report(11, 4, true), b"\x1b[?12;5R".to_vec());
    }

    #[test]
    fn dsr_device_status_replies_ok() {
        assert_eq!(replies(&scan1(b"\x1b[5n")), b"\x1b[0n".to_vec());
    }

    #[test]
    fn primary_da_reports_color_but_not_sixel() {
        let r = replies(&scan1(b"\x1b[c"));
        assert_eq!(r, b"\x1b[?62;1;6;9;15;22c".to_vec());
        // CSI 0c も同じ
        assert_eq!(replies(&scan1(b"\x1b[0c")), r);
        let s = String::from_utf8(r).unwrap();
        assert!(s.contains(";22c"), "ANSIカラー(22)を申告する");
        assert!(!s.contains(";4;"), "sixel(4)は申告しない");
    }

    #[test]
    fn secondary_and_tertiary_da() {
        assert_eq!(replies(&scan1(b"\x1b[>c")), b"\x1b[>0;95;0c".to_vec());
        assert_eq!(replies(&scan1(b"\x1b[>0c")), b"\x1b[>0;95;0c".to_vec());
        assert_eq!(
            replies(&scan1(b"\x1b[=c")),
            b"\x1bP!|00000000\x1b\\".to_vec()
        );
    }

    #[test]
    fn xtversion_answers_with_our_own_name() {
        let r = String::from_utf8(replies(&scan1(b"\x1b[>0q"))).unwrap();
        assert!(r.starts_with("\x1bP>|Zaivern Code("), "got {r:?}");
        assert!(r.ends_with("\x1b\\"));
        // kitty / WezTerm を名乗らない = 特殊プロトコルを送られない
        assert!(!r.contains("kitty") && !r.contains("WezTerm"));
    }

    #[test]
    fn xtgettcap_answers_unsupported_form() {
        // DCS + q 544e ST ("TN" を16進で)
        let r = replies(&scan1(b"\x1bP+q544e\x1b\\"));
        assert_eq!(r, b"\x1bP0+r544e\x1b\\".to_vec());
    }

    #[test]
    fn kitty_keyboard_query_is_declined_silently() {
        // 返事をすると「対応している」と誤解されるので何も返さない
        assert_eq!(scan1(b"\x1b[?u"), vec![]);
        // ただし直後の DA1 にはちゃんと答える(アプリはこれで非対応と判定する)
        assert_eq!(
            replies(&scan1(b"\x1b[?u\x1b[c")),
            b"\x1b[?62;1;6;9;15;22c".to_vec()
        );
    }

    #[test]
    fn kitty_graphics_probe_gets_an_error_reply() {
        let r = replies(&scan1(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"));
        assert_eq!(r, b"\x1b_Gi=31;ENOTSUPPORTED\x1b\\".to_vec());
        // q=2 (応答不要) のときは黙る
        assert_eq!(
            replies(&scan1(b"\x1b_Gi=7,q=2,a=q;AA\x1b\\")),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn decscusr_all_ps_values() {
        use CursorShape::*;
        let cases: &[(&[u8], CursorShape)] = &[
            (b"\x1b[ q", Block), // 引数省略 = 既定
            (b"\x1b[0 q", Block),
            (b"\x1b[1 q", Block),     // 点滅ブロック
            (b"\x1b[2 q", Block),     // 固定ブロック
            (b"\x1b[3 q", Underline), // 点滅アンダーライン
            (b"\x1b[4 q", Underline),
            (b"\x1b[5 q", Bar), // 点滅バー (nvim/helix の挿入モード)
            (b"\x1b[6 q", Bar),
            (b"\x1b[9 q", Block), // 未知の値はブロックへ倒す
        ];
        for (seq, want) in cases {
            assert_eq!(
                scan1(seq),
                vec![TermEvent::CursorShape(*want)],
                "seq={:?}",
                String::from_utf8_lossy(seq)
            );
        }
        // 中間バイトの空白が無い CSI 6 q は DECSCUSR ではない(誤検出しない)
        assert_eq!(scan1(b"\x1b[6q"), vec![]);
    }

    #[test]
    fn focus_mode_set_and_reset() {
        assert_eq!(scan1(b"\x1b[?1004h"), vec![TermEvent::FocusReports(true)]);
        assert_eq!(scan1(b"\x1b[?1004l"), vec![TermEvent::FocusReports(false)]);
        // 他のモードとまとめて指定されても拾う
        assert_eq!(
            scan1(b"\x1b[?1049;1004;2004h"),
            vec![TermEvent::FocusReports(true)]
        );
        // 別モードだけなら何も起きない
        assert_eq!(scan1(b"\x1b[?1049h"), vec![]);
    }

    // ── チャンク境界で切れたシーケンス(この実装の一番の勘所) ──

    #[test]
    fn query_split_across_two_reads() {
        let mut s = QueryScanner::default();
        // "\x1b[6" までで read が返り、続きは次の read で来る
        assert_eq!(s.scan(b"hello\x1b[6"), vec![]);
        assert_eq!(s.scan(b"n"), vec![TermEvent::CursorReport]);
    }

    #[test]
    fn query_split_at_every_possible_offset() {
        // どこで切れても必ず1回だけ検出されること
        let seq = b"abc\x1b[6n\x1b[c\x1b[6 qdef";
        for cut in 0..=seq.len() {
            let mut s = QueryScanner::default();
            let mut evs = s.scan(&seq[..cut]);
            evs.extend(s.scan(&seq[cut..]));
            assert_eq!(
                evs,
                vec![
                    TermEvent::CursorReport,
                    TermEvent::Reply(DA1_REPLY.to_vec()),
                    TermEvent::CursorShape(CursorShape::Bar),
                ],
                "cut={cut}"
            );
        }
    }

    #[test]
    fn osc_and_dcs_split_across_reads() {
        let seq = b"\x1b]52;c;aGk=\x07\x1bP+q544e\x1b\\";
        for cut in 0..=seq.len() {
            let mut s = QueryScanner::default();
            let mut evs = s.scan(&seq[..cut]);
            evs.extend(s.scan(&seq[cut..]));
            assert_eq!(
                evs,
                vec![
                    TermEvent::Clipboard("hi".into()),
                    TermEvent::Reply(b"\x1bP0+r544e\x1b\\".to_vec()),
                ],
                "cut={cut}"
            );
        }
    }

    #[test]
    fn split_one_byte_at_a_time() {
        // 極端な例: 1バイトずつ届いても取りこぼさない
        let seq = b"\x1b[6n\x1b[?1004h\x1b[5 q";
        let mut s = QueryScanner::default();
        let mut evs = Vec::new();
        for b in seq {
            evs.extend(s.scan(&[*b]));
        }
        assert_eq!(
            evs,
            vec![
                TermEvent::CursorReport,
                TermEvent::FocusReports(true),
                TermEvent::CursorShape(CursorShape::Bar),
            ]
        );
    }

    // ── OSC 52 / base64 ──

    #[test]
    fn osc52_decodes_all_padding_variants() {
        // 余り0 / 余り2文字(==) / 余り3文字(=) の3パターン
        let cases: &[(&[u8], &str)] = &[
            (b"\x1b]52;c;YWJjZGVm\x07", "abcdef"), // 6byte, パディング無し
            (b"\x1b]52;c;YQ==\x07", "a"),          // "=="
            (b"\x1b]52;c;YWI=\x07", "ab"),         // "="
            (b"\x1b]52;c;\x07", ""),               // 空(何も起きない, 下で確認)
        ];
        for (seq, want) in &cases[..3] {
            assert_eq!(scan1(seq), vec![TermEvent::Clipboard((*want).into())]);
        }
        assert_eq!(scan1(cases[3].0), vec![]);
        // ST 終端でも同じ
        assert_eq!(
            scan1(b"\x1b]52;c;YWJj\x1b\\"),
            vec![TermEvent::Clipboard("abc".into())]
        );
        // 日本語 (UTF-8) も通る
        assert_eq!(
            scan1(b"\x1b]52;c;44GC44GE\x07"),
            vec![TermEvent::Clipboard("あい".into())]
        );
        // 折り返された base64
        assert_eq!(
            scan1(b"\x1b]52;c;YWJj\r\nZGVm\x07"),
            vec![TermEvent::Clipboard("abcdef".into())]
        );
    }

    #[test]
    fn osc52_read_request_is_refused() {
        // "?" は端末の中身を読み出す要求。勝手に渡さない。
        assert_eq!(scan1(b"\x1b]52;c;?\x07"), vec![]);
    }

    #[test]
    fn osc52_malformed_payloads_are_dropped() {
        for bad in [
            &b"\x1b]52;c;YWJ!\x07"[..],  // 不正な文字
            &b"\x1b]52;c;YWJjZ\x07"[..], // 4文字境界に1文字余る
            &b"\x1b]52;c;=YWJj\x07"[..], // パディングの後にデータ
            &b"\x1b]52;c;YQ===\x07"[..], // パディング過剰
            &b"\x1b]52;c;/w==\x07"[..],  // 0xFF = 不正な UTF-8
        ] {
            assert_eq!(scan1(bad), vec![], "bad={:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn osc52_oversized_payload_is_dropped() {
        let mut seq = b"\x1b]52;c;".to_vec();
        seq.extend(std::iter::repeat_n(b'A', MAX_CLIPBOARD_B64 + 4));
        seq.push(0x07);
        assert_eq!(scan1(&seq), vec![]);
    }

    #[test]
    fn unterminated_string_does_not_grow_forever() {
        let mut s = QueryScanner::default();
        // 終端の来ない OSC を延々流し込んでも pending は上限で捨てられる
        s.scan(b"\x1b]52;c;");
        for _ in 0..40 {
            s.scan(&vec![b'A'; 4096]);
        }
        assert!(s.pending.len() <= MAX_PENDING);
    }

    #[test]
    fn osc_color_queries() {
        assert_eq!(scan1(b"\x1b]10;?\x1b\\"), vec![TermEvent::ColorQuery(10)]);
        assert_eq!(scan1(b"\x1b]11;?\x07"), vec![TermEvent::ColorQuery(11)]);
        assert_eq!(
            color_report(11, 0x12141a),
            b"\x1b]11;rgb:1212/1414/1a1a\x1b\\".to_vec()
        );
        // 色の「設定」(? が無い) には返事をしない
        assert_eq!(scan1(b"\x1b]11;#000000\x07"), vec![]);
    }

    // ── 誤検出しないこと ──

    #[test]
    fn ordinary_output_produces_no_replies() {
        let evs =
            scan1(b"\x1b[1;32mgreen\x1b[0m\r\n\x1b[2J\x1b[H\x1b[?1049h\x1b[38;2;255;0;0mred\x1b[m");
        assert_eq!(evs, vec![]);
    }

    #[test]
    fn garbage_escapes_do_not_desync_the_scanner() {
        // 壊れた ESC の直後の正しい問い合わせを取りこぼさない
        assert_eq!(scan1(b"\x1b[\x01\x1b[6n"), vec![TermEvent::CursorReport]);
        assert_eq!(scan1(b"\x1b\x1b[6n"), vec![TermEvent::CursorReport]);
        // OSC が ST 以外の ESC で中断されても続きを読む
        assert_eq!(scan1(b"\x1b]52;c;YQ\x1b[6n"), vec![TermEvent::CursorReport]);
    }
}

/// 本物の PTY を相手にした結合テスト。
///
/// 単体テストは「走査器が正しいバイト列を作る」ことしか見ない。ここでは実際に
/// 子プロセスを起こし、返事が**子プロセスの標準入力まで届く**ことを確かめる。
#[cfg(test)]
#[cfg(unix)]
mod pty_tests {
    use super::*;

    /// スクリプトを PTY で走らせ、画面に needle が出るまで待って画面全体を返す。
    fn run_in_pty(script: &str, secs: u64) -> String {
        let dir = std::env::temp_dir().join(format!("zaivern-pty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("t{}.sh", script.len()));
        std::fs::write(&path, script).unwrap();

        let spec = SpawnSpec {
            title: "t".into(),
            preset_name: "t".into(),
            icon: "t".into(),
            command: format!("/bin/bash --noprofile --norc {}", path.display()),
            cwd: dir.clone(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut sess = Session::spawn(1, spec, egui::Context::default()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(secs);
        let mut screen = String::new();
        while Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            screen = sess.parser.lock().unwrap().screen().contents();
            if screen.contains("DONE") {
                break;
            }
        }
        sess.kill();
        let _ = std::fs::remove_file(&path);
        screen
    }

    /// CSI 6n の返事が本当に子プロセスの入力へ届くか。
    /// ESC は見やすいよう '^' へ置換して表示している。
    #[test]
    fn child_receives_cursor_position_reply() {
        let out = run_in_pty(
            r#"
stty raw -echo min 0 time 30
printf '\033[6n'
sleep 1
R=$(dd bs=1 count=32 2>/dev/null | tr '\033' '^')
stty sane
printf '\r\nCPR<%s>\r\nDONE\r\n' "$R"
"#,
            15,
        );
        assert!(out.contains("CPR<"), "画面: {out}");
        // ESC [ <row> ; <col> R が返っていること
        let body = out.split("CPR<").nth(1).unwrap().split('>').next().unwrap();
        assert!(body.starts_with("^["), "CPR の返事が来ていない: {body:?}");
        assert!(body.ends_with('R'), "CPR の終端が R でない: {body:?}");
        let nums = &body[2..body.len() - 1];
        let (row, col) = nums.split_once(';').expect("row;col 形式であること");
        assert!(row.parse::<u16>().unwrap() >= 1, "行は1始まり: {body:?}");
        assert!(col.parse::<u16>().unwrap() >= 1, "列は1始まり: {body:?}");
    }

    /// Primary DA の返事が子プロセスへ届くか。
    #[test]
    fn child_receives_primary_da_reply() {
        let out = run_in_pty(
            r#"
stty raw -echo min 0 time 30
printf '\033[c'
sleep 1
R=$(dd bs=1 count=32 2>/dev/null | tr '\033' '^')
stty sane
printf '\r\nDA<%s>\r\nDONE\r\n' "$R"
"#,
            15,
        );
        let body = out.split("DA<").nth(1).unwrap().split('>').next().unwrap();
        assert_eq!(body, "^[?62;1;6;9;15;22c", "画面: {out}");
    }

    /// DECSCUSR がセッションのカーソル形状へ反映されるか(描画側が見る値)。
    #[test]
    fn decscusr_updates_session_cursor_shape() {
        let dir = std::env::temp_dir();
        let spec = SpawnSpec {
            title: "t".into(),
            preset_name: "t".into(),
            icon: "t".into(),
            // バー → アンダーライン → ブロック と切り替える
            command: "printf '\\033[6 q'; sleep 5".into(),
            cwd: dir,
            env: HashMap::new(),
            log_path: None,
        };
        let mut sess = Session::spawn(2, spec, egui::Context::default()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while Instant::now() < deadline && sess.cursor_shape() != CursorShape::Bar {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(sess.cursor_shape(), CursorShape::Bar);
        sess.kill();
    }

    /// OSC 52 が take_clipboard で取り出せるか。
    #[test]
    fn osc52_reaches_take_clipboard() {
        let spec = SpawnSpec {
            title: "t".into(),
            preset_name: "t".into(),
            icon: "t".into(),
            // "yanked" を base64 で
            command: "printf '\\033]52;c;eWFua2Vk\\007'; sleep 5".into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut sess = Session::spawn(3, spec, egui::Context::default()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let mut got = None;
        while Instant::now() < deadline && got.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(50));
            got = sess.take_clipboard();
        }
        assert_eq!(got.as_deref(), Some("yanked"));
        sess.kill();
    }

    /// 承認待ち発生時に attention_since が正常にセット・解除されるか。
    #[test]
    fn test_attention_since_tracking() {
        let spec = SpawnSpec {
            title: "t".into(),
            preset_name: "t".into(),
            icon: "t".into(),
            command: "echo 'Do you want to proceed? (y/n)'; sleep 5".into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: None,
        };
        let mut sess = Session::spawn(4, spec, egui::Context::default()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while Instant::now() < deadline && !sess.attention {
            std::thread::sleep(std::time::Duration::from_millis(50));
            sess.scan_attention(false);
        }
        assert!(sess.attention);
        assert!(sess.attention_since.is_some());

        sess.resolve_attention();
        assert!(!sess.attention);
        assert!(sess.attention_since.is_none());
        sess.kill();
    }

    /// Drop の木殺しが**プロセスグループ全体**へ届くこと。孫の実 PID を
    /// 画面経由で取り、drop 後に ESRCH (もういない) になるのを確認する。
    /// ログインシェル配下の `sleep` 系が生き残って積もり、CI ランナーを
    /// 飢えさせた回帰のテスト。
    #[test]
    fn dropping_a_session_kills_the_grandchild_process_group() {
        let spec = SpawnSpec {
            title: "t".into(),
            preset_name: "t".into(),
            icon: "t".into(),
            // ログインシェル (-lc) の下に /bin/sh、さらにその下に sleep。
            // 非対話シェルはジョブ制御が無いので、全員が同じプロセスグループ
            // (= PTY 直下の子の pgid) にいる。`_END` は PID が画面へ
            // **書き切られた**ことの目印 (途中読みで PID を切り詰めない)。
            command: "/bin/sh -c 'sleep 30 & echo GPID_IS_${!}_END; wait'".into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            log_path: None,
        };
        let sess = Session::spawn(5, spec, egui::Context::default()).unwrap();

        // 画面から孫 PID を読む
        let deadline = Instant::now() + std::time::Duration::from_secs(15);
        let mut gpid: Option<i32> = None;
        while Instant::now() < deadline && gpid.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let screen = sess.parser.lock().unwrap().screen().contents();
            if let Some(rest) = screen.split("GPID_IS_").nth(1) {
                if rest.contains("_END") {
                    if let Some(digits) = rest.split("_END").next() {
                        gpid = digits.parse::<i32>().ok();
                    }
                }
            }
        }
        let gpid = gpid.expect("孫 PID が画面に出なかった");
        assert_eq!(
            unsafe { libc::kill(gpid, 0) },
            0,
            "孫 (pid={gpid}) が起きていない"
        );

        drop(sess);

        // 2 秒以内に ESRCH (もういない) になること
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        let mut gone = false;
        while Instant::now() < deadline {
            if unsafe { libc::kill(gpid, 0) } != 0 {
                let errno = std::io::Error::last_os_error().raw_os_error();
                assert_eq!(
                    errno,
                    Some(libc::ESRCH),
                    "kill(pid, 0) の失敗理由が ESRCH でない"
                );
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            gone,
            "drop 後も孫 (pid={gpid}) が生きている — Drop の木殺しがグループへ届いていない"
        );
    }
}

/// 画面を縮めたときに「いま描かれている内容」が消えないこと。
///
/// Cockpit でファイルを開くと編集ペインが割り込んでミニターミナルの行数が減る。
/// vt100 の元実装はそこで**末尾から**行を捨てるため、TUI (Claude Code 等) が
/// 描いていた本文が丸ごと消え、ペインを戻しても空行が入るだけで復元しない
/// — その端末だけが黒いまま戻らなくなる。実端末と同じく上から履歴へ送ること。
#[cfg(test)]
mod resize_tests {
    /// `rows` 行を書いた画面を作る (最終行にカーソルが乗った状態)。
    fn filled(rows: u16, cols: u16, scrollback: usize) -> vt100::Parser {
        let mut p = vt100::Parser::new(rows, cols, scrollback);
        for i in 0..rows {
            p.process(format!("line{i}").as_bytes());
            if i + 1 < rows {
                p.process(b"\r\n");
            }
        }
        p
    }

    fn lines(p: &vt100::Parser) -> Vec<String> {
        p.screen().contents().lines().map(str::to_string).collect()
    }

    /// 縮めても直近の行 (カーソル側) が残ること。元実装はここで line9 を捨てていた。
    #[test]
    fn shrinking_keeps_the_newest_lines() {
        let mut p = filled(10, 40, 100);
        p.set_size(6, 40);
        let got = lines(&p);
        assert!(
            got.contains(&"line9".to_string()),
            "最新行が消えている: {got:?}"
        );
        assert!(
            !got.contains(&"line0".to_string()),
            "あふれた分は上から捨てる: {got:?}"
        );
    }

    /// 代替画面 (TUI) でも同じ。履歴が無いぶん、消えると本当に戻らない。
    #[test]
    fn alternate_screen_keeps_the_newest_lines() {
        let mut p = vt100::Parser::new(10, 40, 100);
        p.process(b"\x1b[?1049h");
        for i in 0..10 {
            p.process(format!("alt{i}").as_bytes());
            if i + 1 < 10 {
                p.process(b"\r\n");
            }
        }
        p.set_size(5, 40);
        let got = lines(&p);
        assert!(
            got.contains(&"alt9".to_string()),
            "TUI が描いた手前の内容が消えている: {got:?}"
        );
    }

    /// 上から送った行は履歴に入るので、スクロールで遡れば読める
    /// (通常画面では「消える」のではなく「上へ流れる」が正しい)。
    #[test]
    fn overflow_goes_into_the_scrollback() {
        let mut p = filled(10, 40, 100);
        p.set_size(6, 40);
        p.set_scrollback(4);
        let got = lines(&p);
        assert!(
            got.contains(&"line0".to_string()),
            "あふれた行が履歴に残っていない: {got:?}"
        );
    }

    /// カーソルは画面内に残ること (外に出ると次の出力が描かれない)。
    #[test]
    fn cursor_stays_on_screen() {
        let mut p = filled(10, 40, 100);
        p.set_size(4, 40);
        let (row, _col) = p.screen().cursor_position();
        assert!(row < 4, "カーソルが画面外: row={row}");
    }

    /// 縮めてから戻す往復で、残っていた内容が壊れないこと。
    #[test]
    fn shrink_then_grow_does_not_lose_more() {
        let mut p = filled(10, 40, 100);
        p.set_size(6, 40);
        p.set_size(10, 40);
        let got = lines(&p);
        assert!(
            got.contains(&"line9".to_string()),
            "往復で内容が失われた: {got:?}"
        );
    }

    /// 幅だけ変える・広げるだけ、は今までどおり内容を保つこと。
    #[test]
    fn widening_and_growing_keep_everything() {
        let mut p = filled(5, 40, 100);
        p.set_size(5, 80);
        p.set_size(8, 80);
        let got = lines(&p);
        for i in 0..5 {
            assert!(
                got.contains(&format!("line{i}")),
                "line{i} が消えた: {got:?}"
            );
        }
    }
}

/// PTY リサイズが UI スレッドを止めないことの取り決め。
///
/// Windows の `ResizePseudoConsole` は conhost への同期 RPC。Cockpit の
/// タイル増減で毎フレーム × セッション数だけ UI スレッドから撃つと、
/// conhost が 1 個詰まっただけでアプリ全体が固まる (「画面が崩れて
/// 動かなくなる」の正体)。ここでは (1) 送り側が絶対に待たないこと、
/// (2) 嵐が潰し込まれること、(3) 最終サイズは必ず届くこと、を固定する。
#[cfg(test)]
mod pty_resize_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// 潰し込み: 送り側は最新サイズを上書きするだけで待たず、
    /// 詰まった受け側が動き出したら「取り出した時点の最新」だけが届く。
    #[test]
    fn coalescer_delivers_only_the_latest_size_and_never_blocks_the_sender() {
        let sink: Arc<Mutex<Vec<(u16, u16)>>> = Arc::new(Mutex::new(Vec::new()));
        // conhost が詰まった状況の再現: テストがゲートを握っている間、
        // 受け側 (apply) は 1 回目の適用で止まったままになる。
        let gate = Arc::new(Mutex::new(()));
        let hold = gate.lock().unwrap();

        let c = {
            let sink = sink.clone();
            let gate = gate.clone();
            ResizeCoalescer::start("zv-test-coalesce".into(), move |r, co| {
                drop(gate.lock());
                sink.lock().unwrap().push((r, co));
                true
            })
            .expect("ワーカー起動")
        };

        let t0 = Instant::now();
        for i in 0..1000u16 {
            c.request(10 + i % 50, 40 + i % 80);
        }
        c.request(24, 100); // 最終サイズ
        let sent_in = t0.elapsed();
        // 受け側が完全に詰まっていても、送り側 1001 発は待たされない。
        assert!(
            sent_in < Duration::from_secs(1),
            "送り側がブロックした: {sent_in:?}"
        );

        drop(hold); // conhost が復帰
        let deadline = Instant::now() + Duration::from_secs(5);
        while sink.lock().unwrap().last() != Some(&(24, 100)) {
            assert!(
                Instant::now() < deadline,
                "最終サイズが届かない: {:?}",
                sink.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let delivered = sink.lock().unwrap().clone();
        // ゲートで止まっていた高々 1 発 + 最新の 1 発。1001 発が素通りしたら失敗。
        assert!(
            delivered.len() <= 2,
            "潰し込みが効いていない: {delivered:?}"
        );
        c.shutdown();
    }

    /// 取りこぼし禁止: 嵐のあと静止しても、最後のサイズは必ず 1 回だけ届く。
    /// (フレームが止まっても worker は notify で動くので、追加の呼び出しは不要)
    #[test]
    fn last_size_is_delivered_exactly_once_after_quiescence() {
        let sink: Arc<Mutex<Vec<(u16, u16)>>> = Arc::new(Mutex::new(Vec::new()));
        let c = {
            let sink = sink.clone();
            ResizeCoalescer::start("zv-test-lost-update".into(), move |r, co| {
                sink.lock().unwrap().push((r, co));
                true
            })
            .expect("ワーカー起動")
        };
        for i in 0..200u16 {
            c.request(10 + i % 13, 40 + i % 23);
        }
        let last = (99u16, 199u16); // 嵐には現れない値
        c.request(last.0, last.1);
        // ここから先は一切呼ばない (= フレームが止まった状況)。
        let deadline = Instant::now() + Duration::from_secs(5);
        while sink.lock().unwrap().last() != Some(&last) {
            assert!(
                Instant::now() < deadline,
                "静止後に最終サイズが流れてこない: {:?}",
                sink.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(50)); // 余分な再送が無いか見張る
        let delivered = sink.lock().unwrap().clone();
        let n = delivered.iter().filter(|&&s| s == last).count();
        assert_eq!(n, 1, "最終サイズが {n} 回届いた: {delivered:?}");
        assert_eq!(delivered.last(), Some(&last));
        c.shutdown();
    }

    /// フレーム安定判定の方針表。全画面レースと同じ「前フレーム比の矩形安定」:
    /// 同じサイズが RESIZE_STABLE_FRAMES 回続いてはじめて 1 回だけ出荷する。
    #[test]
    fn debounce_policy_table() {
        assert_eq!(
            RESIZE_STABLE_FRAMES, 2,
            "この表は K=2 前提。K を変えたら表も更新すること"
        );
        let a = (10u16, 40u16);
        let b = (12u16, 50u16);
        // (フレームの要求, 期待する出荷, 説明)
        let table: &[((u16, u16), Option<(u16, u16)>, &str)] = &[
            (a, None, "変化直後は出さない (安定 1 フレーム目)"),
            (a, Some(a), "すぐ安定したケース: 2 フレーム目で出荷"),
            (a, None, "出荷済みサイズは再送しない"),
            (a, None, "何フレーム続いても再送しない"),
            (b, None, "揺れ始め: 出さない"),
            (a, None, "振動: 出さない"),
            (b, None, "振動: 出さない"),
            (b, Some(b), "振動が収まって 2 フレーム安定 → 最終だけ出荷"),
            (b, None, "以降は静かなまま"),
        ];
        let mut d = ResizeDebounce::default();
        assert!(!d.pending(), "初期状態で未出荷扱いはおかしい");
        for (i, (req, want, why)) in table.iter().enumerate() {
            assert_eq!(d.on_request(*req), *want, "frame {i}: {why}");
        }
        assert!(!d.pending(), "表の最後は出荷済みで終わるはず");

        // settled 開始: spawn 直後と同じサイズを要求しても無駄撃ちしない。
        let mut d = ResizeDebounce::settled(a);
        for i in 0..4 {
            assert_eq!(d.on_request(a), None, "初期サイズを再送した (frame {i})");
        }
        assert!(!d.pending());
        // そこから変化すれば通常どおり K フレームで出荷。
        assert_eq!(d.on_request(b), None);
        assert!(d.pending(), "変化直後は未出荷 (draw が再描画を要求する印)");
        assert_eq!(d.on_request(b), Some(b));
        assert!(!d.pending());
    }

    fn resize_probe_session(id: u64) -> Session {
        // リサイズの間 PTY を生かしておくだけの静かな子。
        #[cfg(windows)]
        let command = "powershell -NoProfile -Command \"Start-Sleep -Seconds 30\"".to_string();
        #[cfg(not(windows))]
        let command = "/bin/sh -c 'sleep 30'".to_string();
        Session::spawn(
            id,
            SpawnSpec {
                title: "resize".into(),
                preset_name: "resize".into(),
                icon: "r".into(),
                command,
                cwd: std::env::temp_dir(),
                env: HashMap::new(),
                log_path: None,
            },
            egui::Context::default(),
        )
        .expect("PTY起動")
    }

    /// 実 PTY: リサイズの嵐が呼び出し側 (UI スレッド相当) を待たせないこと、
    /// それでも最終サイズは PTY まで必ず届くこと。修正前はこのループが
    /// 1 発ごとに ConPTY への同期 RPC (`ResizePseudoConsole`) を撃っていた。
    #[test]
    fn resize_storm_neither_blocks_the_caller_nor_loses_the_final_size() {
        let mut s = resize_probe_session(9401);
        let t0 = Instant::now();
        for i in 0..100u16 {
            let (rows, cols) = (10 + i % 17, 40 + i % 29);
            // 同じサイズを K フレームぶん要求し、毎回ワーカーへの出荷まで起こす
            // (変化だけを 100 回並べると安定せず 1 発も出荷されないため)。
            for _ in 0..RESIZE_STABLE_FRAMES {
                s.resize(rows, cols);
            }
        }
        let (frows, fcols) = (24u16, 100u16);
        for _ in 0..RESIZE_STABLE_FRAMES {
            s.resize(frows, fcols);
        }
        let elapsed = t0.elapsed();
        // 修正前は 101 回の同期 RPC がここに乗っていた。呼び出し側は
        // サイズの上書きと通知しかしないので、詰まりようがない。
        assert!(
            elapsed < Duration::from_secs(3),
            "リサイズの呼び出し側がブロックした: {elapsed:?}"
        );
        // 描画グリッド (vt100) は即時に最終サイズ。
        assert_eq!(
            lock_ok(&s.parser).screen().size(),
            (frows, fcols),
            "vt100 が要求サイズに即時追従していない"
        );
        // PTY 本体にも水面下で最終サイズが届く (取りこぼし禁止)。
        let deadline = Instant::now() + Duration::from_secs(10);
        while s.pty_size() != Some((frows, fcols)) {
            assert!(
                Instant::now() < deadline,
                "最終サイズが PTY へ届かない: {:?}",
                s.pty_size()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        s.kill();
    }
}

/// CJK / IME まわりの正しさ。
///
/// 日本語・ハングル・中国語の入力と表示は、CI で人が打って確かめられない。
/// そこで「vt100 の実物へ書き込んで読み返す」「イベント列を純関数へ流す」
/// の 2 通りで、手で試すのと同じことを機械にやらせる。
#[cfg(test)]
mod cjk_tests {
    use super::{
        cursor_span, ime_ended_in_frame, selection_text, translate_input, word_selection,
        InputCaps, InputPlan,
    };
    use eframe::egui;

    // ───────────────────────── グリッド (vt100 実物) ─────────────────────────

    fn parser(rows: u16, cols: u16) -> vt100::Parser {
        vt100::Parser::new(rows, cols, 100)
    }

    /// 1 行ぶんを「継続セルを飛ばした見た目の文字列」として読む。
    fn row_text(p: &vt100::Parser, row: u16) -> String {
        let screen = p.screen();
        let (_, cols) = screen.size();
        let mut out = String::new();
        for c in 0..cols {
            let Some(cell) = screen.cell(row, c) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let t = cell.contents();
            out.push_str(if t.is_empty() { " " } else { &t });
        }
        out.trim_end().to_string()
    }

    /// 全角文字は「左半分 = 本体 / 右半分 = 継続セル」の対で並ぶこと。
    ///
    /// **必ず守られる不変条件**は「継続セルの左は必ず全角」のほう。ここが崩れると
    /// 描画側 (`is_wide_continuation()` を読み飛ばす) が 1 桁ずれる。
    ///
    /// 逆向き (全角の右に継続セルがある) は**常には成り立たない**: 端末を狭めると、
    /// 右端に入り切らなくなった全角が継続セルを失って最終列に残る。これは
    /// [`super::draw_cols`] が描画時に 1 桁へ詰めて吸収する契約なので、
    /// 「行の途中で対が崩れていないこと」だけを検査する。
    fn assert_no_split_glyph(p: &vt100::Parser, what: &str) {
        let screen = p.screen();
        let (rows, cols) = screen.size();
        for r in 0..rows {
            for c in 0..cols {
                let Some(cell) = screen.cell(r, c) else {
                    continue;
                };
                if cell.is_wide() && c + 1 < cols {
                    let next = screen.cell(r, c + 1).expect("右隣のセル");
                    assert!(
                        next.is_wide_continuation() || !next.has_contents(),
                        "{what}: 全角の右隣に別の文字が入り込んでいる ({r},{c})"
                    );
                }
                if cell.is_wide_continuation() {
                    assert!(c > 0, "{what}: 継続セルが行頭にある ({r},{c})");
                    let prev = screen.cell(r, c - 1).expect("左隣のセル");
                    assert!(prev.is_wide(), "{what}: 継続セルの左が全角でない ({r},{c})");
                }
                // どこにあっても、描画に使う桁数は画面の右端を越えない
                let w = super::cell_draw_cols(screen, r, c);
                assert!(
                    c + w <= cols,
                    "{what}: 描画桁が画面をはみ出す ({r},{c}) w={w} cols={cols}"
                );
            }
        }
    }

    /// 右端に入り切らない全角は**1 文字まるごと**次行へ送られること。
    /// 半分だけ置いて折り返すと「文字が縦に割れる」= CJK 端末の典型的な壊れ方。
    #[test]
    fn wide_char_wraps_as_one_unit_at_the_right_edge() {
        // 奇数桁の端末: 全角は 1 桁だけ余った状態にぶつかる
        for cols in [5u16, 7, 9] {
            let mut p = parser(3, cols);
            p.process("あいうえお".as_bytes());
            assert_no_split_glyph(&p, &format!("{cols} 桁で折返し"));
            // 全部の文字がどこかに残っている (欠落しない)
            let all: String = (0..3).map(|r| row_text(&p, r)).collect();
            for ch in "あいうえお".chars() {
                assert!(all.contains(ch), "{cols} 桁: {ch} が消えた ({all:?})");
            }
        }
    }

    /// 半角と全角が混ざって右端をまたぐ場合も割れないこと。
    #[test]
    fn mixed_width_wrap_keeps_wide_chars_intact() {
        let mut p = parser(4, 6);
        p.process("ab日本語cd".as_bytes());
        assert_no_split_glyph(&p, "半角全角の混在");
        let joined: String = (0..4).map(|r| row_text(&p, r)).collect();
        for ch in "日本語".chars() {
            assert!(joined.contains(ch), "{ch} が消えた ({joined:?})");
        }
    }

    /// 幅 1 の端末へ全角を書いても panic せず、無限ループにもならないこと
    /// (`vendor/vt100` の桁溢れ修正が効いていることの回帰テスト)。
    #[test]
    fn wide_char_on_a_one_column_terminal_does_not_panic() {
        for cols in [1u16, 2] {
            let mut p = parser(3, cols);
            p.process("あい漢字".as_bytes());
            assert_no_split_glyph(&p, &format!("{cols} 桁"));
        }
    }

    /// 全角の**左半分**を半角で上書きしたら、右半分 (継続セル) も消えること。
    /// 消し残すと前の字の右半分が画面に居座る = Windows で報告される
    /// 「CJK が二重に見える」の典型。
    #[test]
    fn overwriting_the_left_half_of_a_wide_cell_clears_both_halves() {
        let mut p = parser(1, 10);
        p.process("日本".as_bytes());
        assert_eq!(row_text(&p, 0), "日本");
        // 行頭へ戻って半角 1 文字を書く
        p.process(b"\r");
        p.process(b"x");
        assert_no_split_glyph(&p, "左半分の上書き");
        let screen = p.screen();
        assert_eq!(screen.cell(0, 0).unwrap().contents(), "x");
        assert!(
            !screen.cell(0, 1).unwrap().is_wide_continuation(),
            "右半分が継続セルのまま残っている (二重描画の原因)"
        );
        assert!(
            screen.cell(0, 1).unwrap().contents().trim().is_empty(),
            "右半分は空白でなければならない: {:?}",
            screen.cell(0, 1).unwrap().contents()
        );
        assert_eq!(row_text(&p, 0), "x 本", "残るのは 2 文字目だけ");
    }

    /// 全角の**右半分**へ直接書き込んだら、左半分も消えること。
    #[test]
    fn overwriting_the_right_half_of_a_wide_cell_clears_the_left_half() {
        let mut p = parser(1, 10);
        p.process("日本".as_bytes());
        // CUP で 2 桁目 (= 「日」の右半分) へ移動して半角を書く
        p.process(b"\x1b[1;2H");
        p.process(b"z");
        assert_no_split_glyph(&p, "右半分の上書き");
        let screen = p.screen();
        assert_eq!(screen.cell(0, 1).unwrap().contents(), "z");
        assert!(
            !screen.cell(0, 0).unwrap().is_wide(),
            "左半分が全角のまま残っている (半分だけの字が描かれる)"
        );
        assert_eq!(row_text(&p, 0), " z本");
    }

    /// 全角を全角で上書きしても対が崩れないこと。
    #[test]
    fn overwriting_a_wide_cell_with_another_wide_char_is_clean() {
        let mut p = parser(1, 10);
        p.process("日本語".as_bytes());
        p.process(b"\r");
        p.process("한글".as_bytes());
        assert_no_split_glyph(&p, "全角を全角で上書き");
        assert_eq!(row_text(&p, 0), "한글語");
    }

    /// 全角が乗った画面を広げたり狭めたりしても、割れない・落ちないこと。
    /// (debug ビルドで走るので、`vendor/vt100` の減算オーバーフローも捕まる)
    #[test]
    fn resize_with_wide_chars_never_splits_or_panics() {
        let mut p = parser(6, 20);
        p.process(
            "日本語のテキスト\r\n한국어 텍스트\r\n中文文本\r\nascii mixed 日本\r\n".as_bytes(),
        );
        // 狭める → 広げる → 極端に狭める → 戻す
        for (rows, cols) in [
            (6u16, 9u16),
            (6, 3),
            (6, 1),
            (6, 40),
            (2, 5),
            (10, 21),
            (6, 20),
        ] {
            p.set_size(rows, cols);
            assert_no_split_glyph(&p, &format!("{rows}x{cols} へリサイズ"));
            // 読み出しでも落ちないこと (描画が舐める経路と同じ)
            let screen = p.screen();
            let (r, c) = screen.size();
            assert_eq!((r, c), (rows, cols));
            for row in 0..r {
                let _ = row_text(&p, row);
                // 最終列に取り残された全角は 1 桁へ詰めて描く (枠からはみ出させない)
                assert_eq!(
                    super::cell_draw_cols(screen, row, c.saturating_sub(1)),
                    1,
                    "{rows}x{cols}: 最終列を 2 桁で描こうとしている"
                );
            }
            let _ = screen.contents();
        }
    }

    /// スクロールバックへ全角が流れても、読み出しで割れない・落ちないこと。
    #[test]
    fn wide_chars_in_scrollback_survive_reading() {
        let mut p = parser(3, 8);
        for i in 0..40 {
            p.process(format!("行{i:02} 日本語\r\n").as_bytes());
        }
        for back in [0usize, 1, 5, 20, 100] {
            p.set_scrollback(back);
            assert_no_split_glyph(&p, &format!("scrollback={back}"));
            let _ = p.screen().contents();
        }
        p.set_scrollback(0);
    }

    // ───────────────────────── カーソル / 選択 ─────────────────────────

    /// 全角の上のカーソルは 2 桁ぶんになること (左半分だけ反転させない)。
    #[test]
    fn cursor_covers_both_halves_of_a_wide_cell() {
        let mut p = parser(1, 10);
        p.process("あA".as_bytes());
        let screen = p.screen();
        // 全角の左半分 → 2 桁
        assert_eq!(cursor_span(screen, 0, 0), (0, 2), "全角の上は 2 桁");
        // 全角の右半分 (継続セル) → 左半分へ戻して 2 桁
        assert_eq!(cursor_span(screen, 0, 1), (0, 2), "継続セルは左半分へ戻す");
        // 半角の上 → 1 桁
        assert_eq!(cursor_span(screen, 0, 2), (2, 1), "半角の上は 1 桁");
        // 空セル・画面外 → 1 桁 (落ちない)
        assert_eq!(cursor_span(screen, 0, 9), (9, 1));
        assert_eq!(cursor_span(screen, 0, 99), (99, 1));
        assert_eq!(cursor_span(screen, 9, 0), (0, 1));
    }

    /// 描画桁を決める規則の表 (`wide × 位置 × 右隣の空き`)。
    #[test]
    fn draw_cols_policy_table() {
        // (全角か, 列, 桁数, 右隣が空いているか, 期待)
        let table: &[(bool, u16, u16, bool, u16, &str)] = &[
            (false, 0, 10, true, 1, "半角は常に 1 桁"),
            (false, 9, 10, true, 1, "半角 (最終列)"),
            (true, 0, 10, true, 2, "全角 + 右に空きあり"),
            (true, 8, 10, true, 2, "全角 (最終列のひとつ手前)"),
            (true, 9, 10, true, 1, "全角が最終列 = 置き場が無いので 1 桁"),
            (true, 0, 1, true, 1, "1 桁の端末"),
            (true, 0, 10, false, 1, "右隣に別の文字 = 重なるので 1 桁"),
        ];
        for &(wide, col, cols, right_free, want, what) in table {
            assert_eq!(
                super::draw_cols(wide, col, cols, right_free),
                want,
                "{what}"
            );
        }
    }

    /// 実画面から桁数を引く経路 (継続セル・空セル・画面外) の確認。
    #[test]
    fn cell_draw_cols_reads_the_real_screen() {
        let mut p = parser(1, 6);
        p.process("日本x".as_bytes());
        let screen = p.screen();
        assert_eq!(super::cell_draw_cols(screen, 0, 0), 2, "「日」");
        assert_eq!(
            super::cell_draw_cols(screen, 0, 1),
            1,
            "継続セル自体は 1 桁"
        );
        assert_eq!(super::cell_draw_cols(screen, 0, 2), 2, "「本」");
        assert_eq!(super::cell_draw_cols(screen, 0, 4), 1, "半角 x");
        assert_eq!(super::cell_draw_cols(screen, 0, 5), 1, "空セル");
        assert_eq!(
            super::cell_draw_cols(screen, 0, 99),
            1,
            "画面外でも落ちない"
        );
        assert_eq!(
            super::cell_draw_cols(screen, 0, u16::MAX),
            1,
            "桁が振り切れても落ちない"
        );
    }

    /// 実際にカーソルが全角の上にあるとき 2 桁になること (`CUP` で置いた場合も)。
    #[test]
    fn cursor_span_follows_the_real_cursor_position() {
        let mut p = parser(2, 10);
        p.process("日本語".as_bytes());
        p.process(b"\x1b[1;3H"); // 「本」の左半分
        let (r, c) = p.screen().cursor_position();
        assert_eq!(cursor_span(p.screen(), r, c), (2, 2));
        p.process(b"\x1b[1;4H"); // 「本」の右半分
        let (r, c) = p.screen().cursor_position();
        assert_eq!(
            cursor_span(p.screen(), r, c),
            (2, 2),
            "右半分でも左から 2 桁"
        );
    }

    /// 選択範囲のコピーが全角を欠かさない・二重にしないこと。
    /// 継続セルの上で範囲が切れても、文字は 1 回だけ入る。
    #[test]
    fn selection_of_wide_chars_is_not_duplicated_or_split() {
        let mut p = parser(2, 12);
        p.process("日本語abc".as_bytes());
        let screen = p.screen();
        // 全画面
        assert_eq!(selection_text(screen, ((0, 0), (0, 11))), "日本語abc");
        // 「日本」だけ (継続セル 3 で切る)
        assert_eq!(selection_text(screen, ((0, 0), (0, 3))), "日本");
        // 継続セルから始める (「本」の右半分から) → 半分の字は入らない
        assert_eq!(selection_text(screen, ((0, 3), (0, 5))), "語");
        // 1 セルだけ: 全角の左半分
        assert_eq!(selection_text(screen, ((0, 0), (0, 0))), "日");
        // 1 セルだけ: 全角の右半分 (継続セル) → 空
        assert_eq!(selection_text(screen, ((0, 1), (0, 1))), "");
    }

    /// ダブルクリックの語選択が全角の途中で切れないこと。
    #[test]
    fn word_selection_spans_whole_wide_chars() {
        let mut p = parser(1, 20);
        p.process("ab 日本語です cd".as_bytes());
        let screen = p.screen();
        // 「日本語です」は 3..=12 桁 (全角 5 文字 = 10 セル)
        let want = Some(((0, 3), (0, 12)));
        for c in 3..=12 {
            assert_eq!(word_selection(screen, 0, c), want, "col={c} から広げた語");
        }
        // 空白の上は None
        assert_eq!(word_selection(screen, 0, 2), None);
    }

    // ───────────────────────── IME 入力 ─────────────────────────

    fn caps() -> InputCaps {
        InputCaps {
            app_cursor: false,
            bracketed: false,
            at_bottom: true,
            mac: false,
        }
    }

    fn preedit(s: &str) -> egui::Event {
        egui::Event::Ime(egui::ImeEvent::Preedit(s.to_string()))
    }

    fn commit(s: &str) -> egui::Event {
        egui::Event::Ime(egui::ImeEvent::Commit(s.to_string()))
    }

    fn key(k: egui::Key) -> egui::Event {
        egui::Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// テスト用の実行: 副作用 2 つは「何も起きない」にして純粋に回す。
    fn run(events: &[egui::Event], state: &mut String) -> InputPlan {
        translate_input(events, state, caps(), || None, || None)
    }

    fn out_str(plan: &InputPlan) -> String {
        String::from_utf8(plan.out.clone()).expect("PTY へ送るのは常に妥当な UTF-8")
    }

    /// 未確定 → 更新 → 確定 で、**確定文字列が 1 回だけ**送られること。
    /// 途中経過は 1 バイトも送らない (送るとハングルが分裂し、日本語は
    /// 未変換のかなが混ざる)。
    #[test]
    fn preedit_is_never_sent_and_commit_is_sent_once() {
        let mut state = String::new();
        // フレーム 1〜3: 変換中 (「にほんご」→「日本語」)
        for step in ["に", "にほ", "にほん", "にほんご", "日本語"] {
            let plan = run(&[preedit(step)], &mut state);
            assert!(plan.out.is_empty(), "未確定 {step:?} が PTY へ漏れた");
            assert_eq!(state, step, "オーバーレイ表示用に保持する");
        }
        // フレーム 4: 確定
        let plan = run(&[preedit(""), commit("日本語")], &mut state);
        assert_eq!(out_str(&plan), "日本語", "確定文字列だけが 1 回送られる");
        assert!(state.is_empty(), "確定したら未確定文字列は消える");
    }

    /// ハングルの多打鍵合成 (ㅎ → 하 → 한 → 한글) が途中で送られないこと。
    /// 競合製品で「音節が分裂して入る」と報告されている経路そのもの。
    #[test]
    fn hangul_multi_keystroke_composition_is_not_split() {
        let mut state = String::new();
        for step in ["ㅎ", "하", "한", "한ㄱ", "한그", "한글"] {
            let plan = run(&[preedit(step)], &mut state);
            assert!(plan.out.is_empty(), "未確定 {step:?} が漏れた");
        }
        let plan = run(&[preedit(""), commit("한글")], &mut state);
        assert_eq!(out_str(&plan), "한글");
        // 1 回の書き込みにまとまっている = 音節が write 境界で割れない
        assert_eq!(plan.out.len(), "한글".len());
    }

    /// 変換を取り消した (未確定が空になっただけ) フレームは 1 バイトも送らない。
    /// 取り消しに使った Escape も端末へ送らない (TUI のモードが抜けてしまう)。
    #[test]
    fn cancelled_preedit_writes_nothing_and_swallows_escape() {
        let mut state = String::new();
        run(&[preedit("にほん")], &mut state);
        assert_eq!(state, "にほん");
        let plan = run(
            &[
                preedit(""),
                egui::Event::Ime(egui::ImeEvent::Disabled),
                key(egui::Key::Escape),
            ],
            &mut state,
        );
        assert!(
            plan.out.is_empty(),
            "取り消しで何かが送られた: {:?}",
            plan.out
        );
        assert!(state.is_empty());
        // 変換していないときの Escape は従来どおり通る
        let plan = run(&[key(egui::Key::Escape)], &mut state);
        assert_eq!(plan.out, b"\x1b", "素の Escape は端末へ送る");
    }

    /// 確定に使った Enter は「送信」ではないこと。
    /// 確定と同じフレームの Enter は飲み、**次のフレーム**の Enter は送る
    /// (日本語入力の「変換確定 → もう一度 Enter で送信」が成立する)。
    #[test]
    fn commit_enter_confirms_without_submitting_but_the_next_enter_submits() {
        let mut state = String::new();
        run(&[preedit("にほんご")], &mut state);
        // Windows 系の並び: Preedit("") → Commit → Disabled → Enter キー
        let plan = run(
            &[
                preedit(""),
                commit("日本語"),
                egui::Event::Ime(egui::ImeEvent::Disabled),
                key(egui::Key::Enter),
            ],
            &mut state,
        );
        assert_eq!(
            out_str(&plan),
            "日本語",
            "確定 Enter で改行を送ってはいけない"
        );
        // 次のフレームの Enter は素通し = 送信できる
        let plan = run(&[key(egui::Key::Enter)], &mut state);
        assert_eq!(plan.out, b"\r", "2 打鍵目の Enter は送信として届く");
    }

    /// キーが確定より**先**に並ぶ環境でも同じ結果になること
    /// (egui-winit のイベント順は OS ごとに違う)。
    #[test]
    fn enter_before_commit_in_the_same_frame_is_also_swallowed() {
        let mut state = String::new();
        run(&[preedit("かんじ")], &mut state);
        let plan = run(
            &[key(egui::Key::Enter), preedit(""), commit("漢字")],
            &mut state,
        );
        assert_eq!(out_str(&plan), "漢字", "並び順に関係なく確定 Enter は飲む");
    }

    /// 確定と**同じ文字列**が `Text` としても届く環境で二重入力しないこと
    /// (Windows の一部 IME で報告される「CJK が 2 回入る」)。
    #[test]
    fn commit_echoed_as_text_does_not_duplicate() {
        let mut state = String::new();
        let plan = run(
            &[commit("日本語"), egui::Event::Text("日本語".into())],
            &mut state,
        );
        assert_eq!(out_str(&plan), "日本語", "1 回だけ送る");
        // 逆順 (Text が先) でも同じ
        let mut state = String::new();
        let plan = run(
            &[egui::Event::Text("日本語".into()), commit("日本語")],
            &mut state,
        );
        assert_eq!(
            out_str(&plan),
            "日本語日本語",
            "先行 Text は素の入力として扱う"
        );
    }

    /// 別々の文字を確定した直後に、たまたま同じ文字を打鍵した場合は落とさない。
    /// (重複排除は「確定 1 件につき最大 1 件」まで)
    #[test]
    fn dedupe_only_cancels_one_echo_per_commit() {
        let mut state = String::new();
        let plan = run(
            &[
                commit("あ"),
                egui::Event::Text("あ".into()), // エコー → 落とす
                egui::Event::Text("あ".into()), // 実際の打鍵 → 通す
            ],
            &mut state,
        );
        assert_eq!(out_str(&plan), "ああ");
    }

    /// 変換中に届いた生テキストは無視されること (未確定のかなが漏れない)。
    #[test]
    fn text_events_during_composition_are_ignored() {
        let mut state = String::new();
        let plan = run(
            &[
                preedit("に"),
                egui::Event::Text("n".into()),
                preedit("にほ"),
            ],
            &mut state,
        );
        assert!(
            plan.out.is_empty(),
            "変換中の生テキストが漏れた: {:?}",
            plan.out
        );
        assert_eq!(state, "にほ");
    }

    /// 変換中のキー (Enter / 矢印 / Escape) は IME に渡し、端末へは送らないこと。
    #[test]
    fn keys_during_composition_go_to_the_ime() {
        let mut state = String::new();
        run(&[preedit("へんかん")], &mut state);
        for k in [
            egui::Key::Enter,
            egui::Key::ArrowDown,
            egui::Key::ArrowUp,
            egui::Key::Space,
            egui::Key::Escape,
            egui::Key::Tab,
        ] {
            let plan = run(&[key(k)], &mut state);
            assert!(plan.out.is_empty(), "変換中の {k:?} が端末へ漏れた");
        }
    }

    /// 空の確定 (変換を確定せずに閉じた) は 1 バイトも送らないこと。
    #[test]
    fn empty_commit_writes_nothing() {
        let mut state = String::from("にほん");
        let plan = run(&[commit("")], &mut state);
        assert!(plan.out.is_empty());
        assert!(state.is_empty());
    }

    /// 確定文字列は**1 回の書き込み**にまとまること
    /// (イベントごとに書き分けるとマルチバイトが write 境界で割れる)。
    #[test]
    fn a_frame_produces_exactly_one_write_payload() {
        let mut state = String::new();
        let plan = run(
            &[
                commit("あ"),
                egui::Event::Text("b".into()),
                commit("う"),
                egui::Event::Text("え".into()),
            ],
            &mut state,
        );
        assert_eq!(out_str(&plan), "あbうえ", "順序どおり 1 本に連結される");
        assert_eq!(
            std::str::from_utf8(&plan.out).map(str::to_string),
            Ok("あbうえ".to_string()),
            "書き込む列は常に文字境界で閉じている"
        );
    }

    /// ブラケットペーストでマルチバイトが分断されないこと。
    /// 前後の印と本文が 1 本のバイト列に収まり、文字が印をまたがない。
    #[test]
    fn bracketed_paste_keeps_multibyte_text_in_one_write() {
        let text = "日本語の貼り付け 한글 中文 🚀";
        for bracketed in [false, true] {
            let mut state = String::new();
            let plan = translate_input(
                &[egui::Event::Paste(text.into())],
                &mut state,
                InputCaps {
                    bracketed,
                    ..caps()
                },
                || None,
                || None,
            );
            let s = out_str(&plan);
            if bracketed {
                assert_eq!(s, format!("\x1b[200~{text}\x1b[201~"));
                // 本文の前後だけに印があり、本文の内側では切れていない
                assert_eq!(s.matches("\x1b[200~").count(), 1);
                assert_eq!(s.matches("\x1b[201~").count(), 1);
            } else {
                assert_eq!(s, text);
            }
            assert!(s.contains(text), "本文が欠けた: {s:?}");
        }
    }

    /// クリップボード画像の `@パス` 挿入がマルチバイトのパスでも壊れないこと。
    #[test]
    fn image_paste_inserts_a_multibyte_path_in_one_write() {
        let inserted = "@画像/スクリーンショット 001.png ";
        let mut state = String::new();
        let plan = translate_input(
            &[egui::Event::Key {
                key: egui::Key::V,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::CTRL,
            }],
            &mut state,
            caps(),
            || None,
            || Some(inserted.to_string()),
        );
        assert_eq!(out_str(&plan), inserted);
        // Enter は送らない (勝手に送信しない)
        assert!(!plan.out.contains(&b'\r'));
    }

    /// 変換中に画面外へフォーカスが移った等で IME が無効化されたら、
    /// 未確定文字列は消えて何も送られないこと。
    #[test]
    fn ime_disabled_clears_the_preedit_without_writing() {
        let mut state = String::from("にほん");
        let plan = run(&[egui::Event::Ime(egui::ImeEvent::Disabled)], &mut state);
        assert!(plan.out.is_empty());
        assert!(state.is_empty());
        let mut state = String::from("にほん");
        let plan = run(&[egui::Event::Ime(egui::ImeEvent::Enabled)], &mut state);
        assert!(plan.out.is_empty());
        assert!(state.is_empty());
    }

    /// 「このフレームで変換が終わったか」の判定表。
    #[test]
    fn ime_frame_end_detection_table() {
        let cases: &[(&str, Vec<egui::Event>, bool, bool)] = &[
            ("確定あり", vec![commit("あ")], false, true),
            (
                "確定あり(変換中から)",
                vec![preedit(""), commit("あ")],
                true,
                true,
            ),
            ("未確定が空になった", vec![preedit("")], true, true),
            (
                "変換していないのに空の未確定",
                vec![preedit("")],
                false,
                false,
            ),
            ("変換継続", vec![preedit("にほ")], true, false),
            (
                "無効化(変換中)",
                vec![egui::Event::Ime(egui::ImeEvent::Disabled)],
                true,
                true,
            ),
            (
                "無効化(非変換)",
                vec![egui::Event::Ime(egui::ImeEvent::Disabled)],
                false,
                false,
            ),
            ("IME 無関係", vec![key(egui::Key::Enter)], false, false),
        ];
        for (what, events, composing, want) in cases {
            assert_eq!(
                ime_ended_in_frame(events, *composing),
                *want,
                "{what} (変換中={composing})"
            );
        }
    }

    /// IME を通さない通常入力の経路が壊れていないこと (退行よけ)。
    #[test]
    fn plain_typing_still_reaches_the_pty() {
        let mut state = String::new();
        let plan = run(
            &[
                egui::Event::Text("echo ".into()),
                egui::Event::Text("日本語".into()),
                key(egui::Key::Enter),
            ],
            &mut state,
        );
        assert_eq!(out_str(&plan), "echo 日本語\r");
        assert!(!plan.copy && !plan.select_all && plan.input_select.is_none());
    }

    /// Ctrl+A の入力欄選択が使えるとき・使えないときの分岐。
    #[test]
    fn ctrl_a_falls_back_to_the_pty_when_no_input_area_is_found() {
        let ctrl_a = egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL,
        };
        // 入力欄が見つからない → 従来どおり \x01 (行頭移動)
        let mut state = String::new();
        let plan = translate_input(&[ctrl_a.clone()], &mut state, caps(), || None, || None);
        assert_eq!(plan.out, b"\x01");
        // 見つかった → PTY へは送らずローカル選択
        let found = (((0u16, 0u16), (0u16, 4u16)), "日本語です".to_string());
        let mut state = String::new();
        let plan = translate_input(
            &[ctrl_a],
            &mut state,
            caps(),
            || Some(found.clone()),
            || None,
        );
        assert!(plan.out.is_empty());
        assert_eq!(plan.input_select, Some(found));
    }
}

// ════════════════════════════════════════════════════════════════════════
// 端末の分割 (Terminal Splits) — orca / tmux / Ghostty 相当
//
// 1 枚のタイル (Cockpit のセル・下部パネル・エディタ横) の中で、端末ペインを
// 何段でも入れ子に分割できるようにするためのモデル。
//
// 設計方針:
//   * ここは **純粋なデータ構造** — Session も PTY も egui の状態も持たない。
//     セッションは [`SessionId`] (= `Session::id`) でしか触らない。新しい
//     ペインを作るのは呼び出し側で、モデルは「どこに置くか」だけを決める。
//   * 幾何は [`SplitLayout::rects`] が唯一の真実源。描画・リサイズ・方向
//     フォーカスはすべてこの矩形を見る (ズレようがない)。
//   * 保存は実行時 ID ではなく **安定キー** (文字列) で行う → [`SplitLayoutRec`]。
//     復元時に解決できないリーフは黙って落とし、親を畳む (panic しない)。
// ════════════════════════════════════════════════════════════════════════

/// リーフが指すセッション。`Session::id` と同じ値を入れる。
pub type SessionId = u64;

/// 分割の向き。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SplitDir {
    /// 子を**左右**に並べる (境界線は縦)。「右に分割」。
    Horizontal,
    /// 子を**上下**に並べる (境界線は横)。「下に分割」。
    Vertical,
}

/// 方向キーによるフォーカス移動の向き。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FocusDir {
    Up,
    Down,
    Left,
    Right,
}

/// 分割比の下限 (と 1-下限 が上限)。これ以上は潰せない。
pub const MIN_RATIO: f32 = 0.05;
/// ガタードラッグで確保するペインの最小ピクセル幅/高さ。
pub const MIN_PANE_PX: f32 = 48.0;
/// 既定のガター (仕切り) 幅。
pub const GUTTER: f32 = 6.0;
/// 端末の内側余白。`draw` と [`apply_sizes`] で同じ値を使う (ずれると
/// 「描いた矩形と PTY のグリッドが食い違う」事故になる)。
pub const TERM_PADDING: f32 = 6.0;

/// ペインヘッダ (アイコン・題名・活動ランプ・✕/◎) の高さ。
///
/// **ペインが 2 枚以上あるときだけ**確保する。1 枚のタイルは今日と 1 px も
/// 変わらない見た目のままにする ([`pane_body`] がその判定を持つ)。
pub const PANE_HEADER_H: f32 = 18.0;
/// フォーカス中ペインを囲む枠の太さ。細い輪 — 太枠にすると端末が狭く見える。
pub const FOCUS_RING: f32 = 1.5;
/// 非フォーカスのペインに掛ける「かすませ」の濃さ (背景色の α)。
pub const DIM_ALPHA: u8 = 26;

/// ペインヘッダの中身。呼び出し側 (Session を持っている側) が供給する。
///
/// モデルは `Session` を知らないので、アイコンも題名も活動ランプの色も
/// ここで受け取る。色は必ず `Theme` から渡すこと (直書き禁止)。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaneChrome {
    /// エージェントのアイコン (絵文字 1 文字を想定。空でもよい)。
    pub icon: String,
    /// ペインの題名。長ければヘッダ内でクリップされる。
    pub title: String,
    /// 活動ランプの色。`None` なら描かない (静止中)。
    pub dot: Option<egui::Color32>,
}

/// [`draw_split`] の結果。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SplitDraw {
    /// 幾何かフォーカスが変わった → 呼び出し側は [`apply_sizes`] を撃ち直す。
    pub changed: bool,
    /// ヘッダの ✕ が押されたペイン。**モデルは何もしない** — セッションの
    /// 後始末 (reap) と [`SplitLayout::close_leaf`] は呼び出し側の仕事。
    pub close: Option<SessionId>,
}

/// 新しいペインで何を起こすかの指示。モデルは何も起動しない。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PanePreset {
    /// **新規起動とまったく同じ**エージェント — 既定プリセット (先頭) を
    /// ワークスペースの作業フォルダで。`👾 Agent ＋` / `NewAgent` キーで
    /// 起こすのと 1 か所も違わない (分割かどうかで挙動を変えない)。
    NewAgent,
    /// 素のシェルをワークスペースの作業フォルダで (同上)。
    Shell,
}

/// キー入力から決まる分割操作。純関数 [`split_key_action`] が返す。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SplitAction {
    /// 新しいセッションを起こして `dir` 方向へ分割する。
    /// 起動するのは呼び出し側 — モデルは置き場所しか決めない。
    SplitWith { dir: SplitDir, preset: PanePreset },
    /// フォーカス中のペインを閉じる。
    ClosePane,
    /// 幾何的な隣へフォーカスを移す。
    Focus(FocusDir),
    /// フォーカス中ペインのズームをトグルする。
    Zoom,
    /// 全ペインを等面積にする。
    Equalize,
    /// フォーカス中ペインを広げる (`grow`) / 狭める。マウスでガターを
    /// 掴めない状況 (キーボードのみ) のための同等操作。
    Resize { grow: bool },
}

/// キーボードでのリサイズ 1 回ぶんの比率。
/// 10 回で端から端まで動く程度 — 細かすぎると操作感が悪く、粗いと行き過ぎる。
pub const RESIZE_STEP: f32 = 0.05;

/// 分割操作の修飾キー (macOS = Cmd+Option / Windows・Linux = Ctrl+Alt) か。
///
/// * macOS では **Ctrl は絶対に奪わない** — `Ctrl+C` / `Ctrl+D` は端末のもの。
/// * Windows / Linux では **Cmd 相当は Ctrl** なので `Ctrl+Alt` を見る。
///   単独 `Ctrl+文字` も奪わない (同上)。
fn is_split_chord(m: &egui::Modifiers, mac: bool) -> bool {
    if !m.alt {
        return false;
    }
    if mac {
        m.mac_cmd && !m.ctrl
    } else {
        m.ctrl && !m.mac_cmd
    }
}

/// **キー → 分割操作** の決定表 (純関数・副作用なし)。
///
/// 修飾は macOS が `Cmd+Option`、Windows / Linux が `Ctrl+Alt`。
/// この一族は `keybinds.rs` の既定表と 1 つも衝突しない (`W` `Z` `E` `N`
/// `H` `J` `K` `L` と矢印は `cmd+alt` / `ctrl+alt` で未使用)。
///
/// | キー | 操作 |
/// |---|---|
/// | `Shift+→` | 右に分割 (新しいエージェント = 新規起動と同じ) |
/// | `Shift+↓` | 下に分割 (同上) |
/// | `Shift+H` / `Shift+L` | フォーカス中ペインを狭める / 広げる |
/// | `N` | 右に分割してシェルを開く |
/// | `W` | ペインを閉じる |
/// | `←` `→` `↑` `↓` / `H` `J` `K` `L` | フォーカス移動 |
/// | `Z` | ズーム |
/// | `E` | 等分 |
///
/// これ以外 (素の文字・`Ctrl+文字`・`Alt+矢印` など) は `None` を返し、
/// 入力はそのまま端末へ流れる。
pub fn split_key_action(key: egui::Key, mods: &egui::Modifiers, mac: bool) -> Option<SplitAction> {
    if !is_split_chord(mods, mac) {
        return None;
    }
    use egui::Key as K;
    if mods.shift {
        // Shift + 矢印 = 「新しいペインを置く向き」を指す。
        return match key {
            K::ArrowRight => Some(SplitAction::SplitWith {
                dir: SplitDir::Horizontal,
                preset: PanePreset::NewAgent,
            }),
            K::ArrowDown => Some(SplitAction::SplitWith {
                dir: SplitDir::Vertical,
                preset: PanePreset::NewAgent,
            }),
            // 移動 (H / L) と同じ指で、押しっぱなしにできる幅調整。
            K::H => Some(SplitAction::Resize { grow: false }),
            K::L => Some(SplitAction::Resize { grow: true }),
            _ => None,
        };
    }
    match key {
        K::N => Some(SplitAction::SplitWith {
            dir: SplitDir::Horizontal,
            preset: PanePreset::Shell,
        }),
        K::W => Some(SplitAction::ClosePane),
        K::Z => Some(SplitAction::Zoom),
        K::E => Some(SplitAction::Equalize),
        K::ArrowLeft | K::H => Some(SplitAction::Focus(FocusDir::Left)),
        K::ArrowDown | K::J => Some(SplitAction::Focus(FocusDir::Down)),
        K::ArrowUp | K::K => Some(SplitAction::Focus(FocusDir::Up)),
        K::ArrowRight | K::L => Some(SplitAction::Focus(FocusDir::Right)),
        _ => None,
    }
}

/// ペイン矩形から**端末本体**の矩形を切り出す。
///
/// `multi` (= ペインが 2 枚以上) のときだけヘッダぶんを上から削る。
/// 1 枚のときは受け取った矩形をそのまま返す — 既存の見た目を 1 px も変えない。
/// 極端に低いペインでヘッダが本体を食い潰さないよう半分で頭打ちにする。
pub fn pane_body(pane: egui::Rect, multi: bool) -> egui::Rect {
    if !multi {
        return pane;
    }
    let h = PANE_HEADER_H.min((pane.height() * 0.5).max(0.0));
    egui::Rect::from_min_max(egui::pos2(pane.min.x, pane.min.y + h), pane.max)
}

/// 分割木のノード。
#[derive(Clone, Debug, PartialEq)]
pub enum SplitNode {
    /// 端末 1 枚。
    Leaf(SessionId),
    /// 2 分割。`ratio` は **ガターを除いた領域のうち `a` が取る割合**。
    Split {
        dir: SplitDir,
        ratio: f32,
        a: Box<SplitNode>,
        b: Box<SplitNode>,
    },
}

/// ルートからノードへの経路。`false` = `a` 側、`true` = `b` 側。
pub type SplitPath = Vec<bool>;

/// 描画可能なガター 1 本ぶんの情報。
#[derive(Clone, Debug, PartialEq)]
pub struct Gutter {
    /// この分割ノードへの経路 ([`SplitLayout::drag_gutter`] に渡す)。
    pub path: SplitPath,
    pub dir: SplitDir,
    /// 仕切りの帯そのもの (当たり判定・描画用)。
    pub rect: egui::Rect,
    /// この分割が占める領域全体 (ドラッグ量 → 比率の換算に使う)。
    pub span: egui::Rect,
}

fn clamp_ratio(r: f32) -> f32 {
    if !r.is_finite() {
        return 0.5;
    }
    r.clamp(MIN_RATIO, 1.0 - MIN_RATIO)
}

/// ピクセル最小幅も考慮した比率クランプ。領域が狭すぎるときは中央固定。
fn clamp_ratio_px(r: f32, avail_px: f32) -> f32 {
    let lo = MIN_RATIO.max(if avail_px > 0.0 {
        MIN_PANE_PX / avail_px
    } else {
        MIN_RATIO
    });
    let hi = 1.0 - lo;
    if lo >= hi {
        return 0.5;
    }
    if !r.is_finite() {
        return 0.5;
    }
    r.clamp(lo, hi)
}

/// 1 つの分割を (a の矩形, ガター帯, b の矩形) に割る。
///
/// 3 つは**必ず `area` の内側**で、互いに重ならず、隙間なく `area` を埋める。
/// 端数は `a` 側を floor して `b` に寄せる — 同じ入力なら常に同じ出力になる。
fn split_area(
    area: egui::Rect,
    dir: SplitDir,
    ratio: f32,
    gutter: f32,
) -> (egui::Rect, egui::Rect, egui::Rect) {
    let g = gutter.max(0.0);
    match dir {
        SplitDir::Horizontal => {
            let total = area.width().max(0.0);
            let avail = (total - g).max(0.0);
            let gw = total - avail; // = min(g, total)
            let a_w = (avail * ratio).floor().clamp(0.0, avail);
            let x0 = area.min.x;
            let a = egui::Rect::from_min_max(
                egui::pos2(x0, area.min.y),
                egui::pos2(x0 + a_w, area.max.y),
            );
            let gr = egui::Rect::from_min_max(
                egui::pos2(a.max.x, area.min.y),
                egui::pos2((a.max.x + gw).min(area.max.x), area.max.y),
            );
            let b = egui::Rect::from_min_max(
                egui::pos2(gr.max.x, area.min.y),
                egui::pos2(area.max.x, area.max.y),
            );
            (a, gr, b)
        }
        SplitDir::Vertical => {
            let total = area.height().max(0.0);
            let avail = (total - g).max(0.0);
            let gh = total - avail;
            let a_h = (avail * ratio).floor().clamp(0.0, avail);
            let y0 = area.min.y;
            let a = egui::Rect::from_min_max(
                egui::pos2(area.min.x, y0),
                egui::pos2(area.max.x, y0 + a_h),
            );
            let gr = egui::Rect::from_min_max(
                egui::pos2(area.min.x, a.max.y),
                egui::pos2(area.max.x, (a.max.y + gh).min(area.max.y)),
            );
            let b = egui::Rect::from_min_max(
                egui::pos2(area.min.x, gr.max.y),
                egui::pos2(area.max.x, area.max.y),
            );
            (a, gr, b)
        }
    }
}

impl SplitNode {
    fn leaves_into(&self, out: &mut Vec<SessionId>) {
        match self {
            SplitNode::Leaf(id) => out.push(*id),
            SplitNode::Split { a, b, .. } => {
                a.leaves_into(out);
                b.leaves_into(out);
            }
        }
    }

    /// 木に含まれるリーフを**左上から右下の順**で並べる (in-order)。
    pub fn leaves(&self) -> Vec<SessionId> {
        let mut v = Vec::new();
        self.leaves_into(&mut v);
        v
    }

    /// リーフ数。
    pub fn leaf_count(&self) -> usize {
        match self {
            SplitNode::Leaf(_) => 1,
            SplitNode::Split { a, b, .. } => a.leaf_count() + b.leaf_count(),
        }
    }

    /// このセッションを含むか。
    pub fn contains(&self, id: SessionId) -> bool {
        match self {
            SplitNode::Leaf(x) => *x == id,
            SplitNode::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    fn first_leaf(&self) -> SessionId {
        match self {
            SplitNode::Leaf(x) => *x,
            SplitNode::Split { a, .. } => a.first_leaf(),
        }
    }

    fn find_path(&self, id: SessionId, path: &mut SplitPath) -> bool {
        match self {
            SplitNode::Leaf(x) => *x == id,
            SplitNode::Split { a, b, .. } => {
                path.push(false);
                if a.find_path(id, path) {
                    return true;
                }
                path.pop();
                path.push(true);
                if b.find_path(id, path) {
                    return true;
                }
                path.pop();
                false
            }
        }
    }

    fn at_path_mut(&mut self, path: &[bool]) -> Option<&mut SplitNode> {
        match path.split_first() {
            None => Some(self),
            Some((step, rest)) => match self {
                SplitNode::Leaf(_) => None,
                SplitNode::Split { a, b, .. } => {
                    if *step {
                        b.at_path_mut(rest)
                    } else {
                        a.at_path_mut(rest)
                    }
                }
            },
        }
    }

    fn replace_leaf(&mut self, id: SessionId, node: SplitNode) -> bool {
        match self {
            SplitNode::Leaf(x) if *x == id => {
                *self = node;
                true
            }
            SplitNode::Leaf(_) => false,
            SplitNode::Split { a, b, .. } => {
                if a.contains(id) {
                    a.replace_leaf(id, node)
                } else if b.contains(id) {
                    b.replace_leaf(id, node)
                } else {
                    false
                }
            }
        }
    }

    fn rects_into(&self, area: egui::Rect, gutter: f32, out: &mut Vec<(SessionId, egui::Rect)>) {
        match self {
            SplitNode::Leaf(id) => out.push((*id, area)),
            SplitNode::Split { dir, ratio, a, b } => {
                let (ra, _, rb) = split_area(area, *dir, *ratio, gutter);
                a.rects_into(ra, gutter, out);
                b.rects_into(rb, gutter, out);
            }
        }
    }

    fn gutters_into(
        &self,
        area: egui::Rect,
        gutter: f32,
        path: &mut SplitPath,
        out: &mut Vec<Gutter>,
    ) {
        if let SplitNode::Split { dir, ratio, a, b } = self {
            let (ra, gr, rb) = split_area(area, *dir, *ratio, gutter);
            out.push(Gutter {
                path: path.clone(),
                dir: *dir,
                rect: gr,
                span: area,
            });
            path.push(false);
            a.gutters_into(ra, gutter, path, out);
            path.pop();
            path.push(true);
            b.gutters_into(rb, gutter, path, out);
            path.pop();
        }
    }

    /// 全ての分割比を「両側のリーフ数に比例」させる = 面積が均等になる。
    fn equalize(&mut self) -> usize {
        match self {
            SplitNode::Leaf(_) => 1,
            SplitNode::Split { ratio, a, b, .. } => {
                let na = a.equalize();
                let nb = b.equalize();
                *ratio = clamp_ratio(na as f32 / (na + nb) as f32);
                na + nb
            }
        }
    }
}

/// 指定リーフを消し、親を畳む。畳んだ地点の兄弟の先頭リーフを `fallback` に残す
/// (閉じたペインがフォーカス中だったとき、そこへフォーカスを移すため)。
fn remove_leaf(
    node: SplitNode,
    id: SessionId,
    fallback: &mut Option<SessionId>,
) -> Option<SplitNode> {
    match node {
        SplitNode::Leaf(x) if x == id => None,
        SplitNode::Leaf(x) => Some(SplitNode::Leaf(x)),
        SplitNode::Split { dir, ratio, a, b } => {
            if a.contains(id) {
                match remove_leaf(*a, id, fallback) {
                    Some(a2) => Some(SplitNode::Split {
                        dir,
                        ratio,
                        a: Box::new(a2),
                        b,
                    }),
                    None => {
                        *fallback = Some(b.first_leaf());
                        Some(*b)
                    }
                }
            } else if b.contains(id) {
                match remove_leaf(*b, id, fallback) {
                    Some(b2) => Some(SplitNode::Split {
                        dir,
                        ratio,
                        a,
                        b: Box::new(b2),
                    }),
                    None => {
                        *fallback = Some(a.first_leaf());
                        Some(*a)
                    }
                }
            } else {
                Some(SplitNode::Split { dir, ratio, a, b })
            }
        }
    }
}

/// 条件を満たさないリーフを落として木を畳む (復元時の自己修復)。
fn retain_node(node: SplitNode, keep: &mut dyn FnMut(SessionId) -> bool) -> Option<SplitNode> {
    match node {
        SplitNode::Leaf(x) => keep(x).then_some(SplitNode::Leaf(x)),
        SplitNode::Split { dir, ratio, a, b } => {
            let a2 = retain_node(*a, keep);
            let b2 = retain_node(*b, keep);
            match (a2, b2) {
                (Some(x), Some(y)) => Some(SplitNode::Split {
                    dir,
                    ratio: clamp_ratio(ratio),
                    a: Box::new(x),
                    b: Box::new(y),
                }),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            }
        }
    }
}

/// 1 タイル分の分割レイアウト。
///
/// フィールドは非公開 — 「空の分割」「木に居ないセッションへのフォーカス」
/// といった壊れた状態を作らせないため、操作は全てメソッド経由にする。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SplitLayout {
    root: Option<SplitNode>,
    focus: Option<SessionId>,
    zoomed: bool,
}

impl SplitLayout {
    /// 空のレイアウト (ペイン 0 枚)。
    pub fn new() -> Self {
        Self::default()
    }

    /// 端末 1 枚だけのレイアウト。
    pub fn single(id: SessionId) -> Self {
        Self {
            root: Some(SplitNode::Leaf(id)),
            focus: Some(id),
            zoomed: false,
        }
    }

    /// 木そのもの。**テスト専用** — 本番の描画・保存は `rects` / `to_rec` を
    /// 通す (木を直接読ませると幾何の真実源が 2 つになる)。
    #[cfg(test)]
    pub fn root(&self) -> Option<&SplitNode> {
        self.root.as_ref()
    }
    pub fn focus(&self) -> Option<SessionId> {
        self.focus
    }
    pub fn zoomed(&self) -> bool {
        self.zoomed
    }
    /// ペイン数。
    pub fn len(&self) -> usize {
        self.root.as_ref().map_or(0, |r| r.leaf_count())
    }
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }
    pub fn contains(&self, id: SessionId) -> bool {
        self.root.as_ref().is_some_and(|r| r.contains(id))
    }
    /// 左上から右下の順のリーフ一覧。
    pub fn leaves(&self) -> Vec<SessionId> {
        self.root.as_ref().map(|r| r.leaves()).unwrap_or_default()
    }

    /// 木に居るセッションだけフォーカスできる。
    pub fn set_focus(&mut self, id: SessionId) -> bool {
        if self.contains(id) {
            self.focus = Some(id);
            true
        } else {
            false
        }
    }

    /// フォーカス中のペインを分割し、新しいリーフ `new_id` を隣に置く。
    ///
    /// セッションを起こすのは呼び出し側 — ここは木を組み替えるだけ。
    /// 木が空なら `new_id` が最初のペインになる。既に居る ID は拒否する。
    pub fn split_focused(&mut self, dir: SplitDir, new_id: SessionId) -> bool {
        if self.contains(new_id) {
            return false;
        }
        let Some(root) = self.root.as_mut() else {
            self.root = Some(SplitNode::Leaf(new_id));
            self.focus = Some(new_id);
            return true;
        };
        // フォーカスが行方不明なら先頭ペインを分割する (無操作にはしない)。
        let target = match self.focus {
            Some(f) if root.contains(f) => f,
            _ => root.first_leaf(),
        };
        let node = SplitNode::Split {
            dir,
            ratio: 0.5,
            a: Box::new(SplitNode::Leaf(target)),
            b: Box::new(SplitNode::Leaf(new_id)),
        };
        if !root.replace_leaf(target, node) {
            return false;
        }
        self.focus = Some(new_id);
        // 新しいペインを開いたらズームは解除 (見えないペインを増やさない)。
        self.zoomed = false;
        true
    }

    /// ペインを閉じる。親は畳まれ、兄弟が親の矩形をそのまま受け取る。
    pub fn close_leaf(&mut self, id: SessionId) -> bool {
        let Some(root) = self.root.take() else {
            return false;
        };
        if !root.contains(id) {
            self.root = Some(root);
            return false;
        }
        let mut fallback = None;
        self.root = remove_leaf(root, id, &mut fallback);
        match &self.root {
            None => {
                self.focus = None;
                self.zoomed = false;
            }
            Some(r) => {
                if self.focus == Some(id) || !self.focus.is_some_and(|f| r.contains(f)) {
                    self.focus = Some(fallback.unwrap_or_else(|| r.first_leaf()));
                }
            }
        }
        true
    }

    /// 幾何的な隣のペインへフォーカスを移す (VS Code / tmux と同じ判定)。
    ///
    /// 判定は「その向きに居る候補のうち **最も近い** もの、その中で
    /// **フォーカス辺との重なりが最大** のもの」。端では何もせず false。
    /// ズーム中でも**分割前の幾何**で判定するので、移った先がズーム表示になる。
    pub fn focus_dir(&mut self, dir: FocusDir, area: egui::Rect, gutter: f32) -> bool {
        let rects = self.rects_unzoomed(area, gutter);
        if rects.is_empty() {
            return false;
        }
        let Some(cur) = self.focus.filter(|f| self.contains(*f)) else {
            self.focus = Some(rects[0].0);
            return true;
        };
        let Some(cur_rect) = rects.iter().find(|(id, _)| *id == cur).map(|(_, r)| *r) else {
            return false;
        };
        match dir_pick(cur_rect, &rects, cur, dir) {
            Some(id) => {
                self.focus = Some(id);
                true
            }
            None => false,
        }
    }

    /// フォーカス中ペインを囲む分割の比率を動かす。`delta` は比率 (0.05 = 5%)。
    /// 正なら**フォーカス中のペインが広がる**。最小サイズでクランプする。
    pub fn resize_focused(&mut self, delta: f32) -> bool {
        let Some(f) = self.focus else { return false };
        let Some(root) = self.root.as_mut() else {
            return false;
        };
        let mut path = SplitPath::new();
        if !root.find_path(f, &mut path) || path.is_empty() {
            return false;
        }
        let on_b = *path.last().unwrap();
        path.pop(); // 親 (分割ノード) へ
        let Some(SplitNode::Split { ratio, .. }) = root.at_path_mut(&path) else {
            return false;
        };
        let before = *ratio;
        let next = clamp_ratio(if on_b { before - delta } else { before + delta });
        *ratio = next;
        (next - before).abs() > f32::EPSILON
    }

    /// ガターのドラッグを比率に反映する。`span_px` はその分割が占める長さ。
    pub fn drag_gutter(&mut self, path: &[bool], delta_px: f32, span_px: f32, gutter: f32) -> bool {
        let Some(root) = self.root.as_mut() else {
            return false;
        };
        let Some(SplitNode::Split { ratio, .. }) = root.at_path_mut(path) else {
            return false;
        };
        let avail = (span_px - gutter.max(0.0)).max(1.0);
        let before = *ratio;
        let next = clamp_ratio_px(before + delta_px / avail, avail);
        *ratio = next;
        (next - before).abs() > f32::EPSILON
    }

    /// 経路の分割比。**テスト専用** — 保存は [`Self::to_rec`] が木ごと書き出す。
    #[cfg(test)]
    pub fn ratio_at(&self, path: &[bool]) -> Option<f32> {
        let mut node = self.root.as_ref()?;
        for step in path {
            match node {
                SplitNode::Split { a, b, .. } => node = if *step { b } else { a },
                SplitNode::Leaf(_) => return None,
            }
        }
        match node {
            SplitNode::Split { ratio, .. } => Some(*ratio),
            SplitNode::Leaf(_) => None,
        }
    }

    /// 全ペインを等面積にする。
    pub fn equalize(&mut self) {
        if let Some(r) = self.root.as_mut() {
            r.equalize();
        }
    }

    /// **その仕切りだけ**を 50:50 に戻す (orca のガター ダブルクリック相当 —
    /// 掴んだ 1 本の左右/上下だけが揃い、他の分割は動かない)。
    pub fn equalize_at(&mut self, path: &[bool]) -> bool {
        let Some(root) = self.root.as_mut() else {
            return false;
        };
        let Some(SplitNode::Split { ratio, .. }) = root.at_path_mut(path) else {
            return false;
        };
        let changed = (*ratio - 0.5).abs() > f32::EPSILON;
        *ratio = 0.5;
        changed
    }

    /// ズームのトグル。戻り値は**トグル後**の状態。
    /// ズーム中は [`Self::rects`] がフォーカス中ペイン 1 枚だけを返す。
    pub fn zoom_focused(&mut self) -> bool {
        if self.focus.is_none_or(|f| !self.contains(f)) {
            self.zoomed = false;
            return false;
        }
        self.zoomed = !self.zoomed;
        self.zoomed
    }

    /// 生きていないセッションのリーフを落として木を畳む。戻り値は変化したか。
    ///
    /// フォーカス中のペインが死んだときは、**元の並び順で最も近い**生存ペイン
    /// (直後 → 直前の順) へフォーカスを移す。先頭に飛ばすと、4 枚並べていて
    /// 3 枚目が落ちただけで視線が左上へ吹っ飛ぶため。
    pub fn heal(&mut self, alive: &mut dyn FnMut(SessionId) -> bool) -> bool {
        let Some(root) = self.root.take() else {
            return false;
        };
        let before = root.clone();
        // 畳む前の並びとフォーカス位置を覚えておく (近い順の探索に使う)。
        let order = before.leaves();
        let at = self
            .focus
            .and_then(|f| order.iter().position(|x| *x == f))
            .unwrap_or(0);
        self.root = retain_node(root, alive);
        let changed = self.root.as_ref() != Some(&before);
        match &self.root {
            None => {
                self.focus = None;
                self.zoomed = false;
            }
            Some(r) => {
                if self.focus.is_none_or(|f| !r.contains(f)) {
                    self.focus = Some(nearest_survivor(&order, at, r));
                }
            }
        }
        changed
    }

    /// **描画する**ペインの矩形。ズーム中はフォーカス中の 1 枚だけ。
    ///
    /// 不変条件: 返る矩形はすべて `area` の内側で、互いに重ならない。
    pub fn rects(&self, area: egui::Rect, gutter: f32) -> Vec<(SessionId, egui::Rect)> {
        if self.zoomed {
            if let Some(f) = self.focus.filter(|f| self.contains(*f)) {
                return vec![(f, area)];
            }
        }
        self.rects_unzoomed(area, gutter)
    }

    /// ズームを無視した本来の幾何 (方向フォーカスの判定に使う)。
    pub fn rects_unzoomed(&self, area: egui::Rect, gutter: f32) -> Vec<(SessionId, egui::Rect)> {
        let mut out = Vec::new();
        if let Some(r) = &self.root {
            r.rects_into(area, gutter, &mut out);
        }
        out
    }

    /// ドラッグ可能な仕切り一覧。ズーム中は空 (仕切りは見えない)。
    pub fn gutters(&self, area: egui::Rect, gutter: f32) -> Vec<Gutter> {
        let mut out = Vec::new();
        if self.zoomed && self.focus.is_some_and(|f| self.contains(f)) {
            return out;
        }
        if let Some(r) = &self.root {
            let mut path = SplitPath::new();
            r.gutters_into(area, gutter, &mut path, &mut out);
        }
        out
    }

    /// 保存用の形へ落とす。キーを引けないリーフは落として木を畳む。
    pub fn to_rec(&self, key_of: &mut dyn FnMut(SessionId) -> Option<String>) -> SplitLayoutRec {
        let mut rec = SplitLayoutRec {
            nodes: Vec::new(),
            focus: String::new(),
            zoomed: self.zoomed,
        };
        let Some(root) = self.root.clone() else {
            return rec;
        };
        // キーを引けないリーフを先に落としてから書き出す (穴あきの木を保存しない)。
        let mut keys: Vec<(SessionId, String)> = Vec::new();
        let healed = retain_node(root, &mut |id| match key_of(id) {
            Some(k) => {
                keys.push((id, k));
                true
            }
            None => false,
        });
        let Some(healed) = healed else { return rec };
        let key = |id: SessionId| -> String {
            keys.iter()
                .find(|(i, _)| *i == id)
                .map(|(_, k)| k.clone())
                .unwrap_or_default()
        };
        encode_node(&healed, &key, &mut rec.nodes);
        if let Some(f) = self.focus.filter(|f| healed.contains(*f)) {
            rec.focus = key(f);
        }
        rec
    }
}

/// トークン列 (先行順) へ書き出す。
fn encode_node(node: &SplitNode, key: &dyn Fn(SessionId) -> String, out: &mut Vec<String>) {
    match node {
        SplitNode::Leaf(id) => out.push(format!("L:{}", key(*id))),
        SplitNode::Split { dir, ratio, a, b } => {
            let tag = match dir {
                SplitDir::Horizontal => 'H',
                SplitDir::Vertical => 'V',
            };
            out.push(format!("{tag}:{:.6}", clamp_ratio(*ratio)));
            encode_node(a, key, out);
            encode_node(b, key, out);
        }
    }
}

/// 復元途中の木 (まだ実行時 ID に解決していない)。
enum KeyNode {
    Leaf(String),
    Split {
        dir: SplitDir,
        ratio: f32,
        a: Box<KeyNode>,
        b: Box<KeyNode>,
    },
}

fn decode_node(toks: &[String], i: &mut usize) -> Option<KeyNode> {
    let t = toks.get(*i)?;
    *i += 1;
    if let Some(k) = t.strip_prefix("L:") {
        return Some(KeyNode::Leaf(k.to_string()));
    }
    let dir = if t.starts_with("H:") {
        SplitDir::Horizontal
    } else if t.starts_with("V:") {
        SplitDir::Vertical
    } else {
        return None;
    };
    let ratio = clamp_ratio(t[2..].parse::<f32>().ok()?);
    let a = decode_node(toks, i)?;
    let b = decode_node(toks, i)?;
    Some(KeyNode::Split {
        dir,
        ratio,
        a: Box::new(a),
        b: Box::new(b),
    })
}

/// 安定キー → 実行時 ID。引けないリーフ・重複したリーフは落として畳む。
fn key_to_node(
    k: &KeyNode,
    id_of: &mut dyn FnMut(&str) -> Option<SessionId>,
    seen: &mut Vec<SessionId>,
) -> Option<SplitNode> {
    match k {
        KeyNode::Leaf(s) => {
            let id = id_of(s)?;
            if seen.contains(&id) {
                return None;
            }
            seen.push(id);
            Some(SplitNode::Leaf(id))
        }
        KeyNode::Split { dir, ratio, a, b } => {
            let a2 = key_to_node(a, id_of, seen);
            let b2 = key_to_node(b, id_of, seen);
            match (a2, b2) {
                (Some(x), Some(y)) => Some(SplitNode::Split {
                    dir: *dir,
                    ratio: clamp_ratio(*ratio),
                    a: Box::new(x),
                    b: Box::new(y),
                }),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            }
        }
    }
}

/// 保存用の分割レイアウト。
///
/// 実行時 ID (`Session::id`) は再起動で変わるので、リーフは**安定キー**の
/// 文字列で書く。木は先行順のトークン列に潰してあるので TOML でも JSON でも
/// そのまま往復できる (再帰 enum を TOML に書くときの罠を踏まない)。
///
/// トークンの形:
///   * `"L:<キー>"`        — リーフ。`L:` 以降は全部キー (`:` を含んでよい)。
///   * `"H:<比率>"`        — 左右分割。続く 2 ノードが a, b。
///   * `"V:<比率>"`        — 上下分割。
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SplitLayoutRec {
    pub nodes: Vec<String>,
    /// フォーカス中リーフの安定キー (空 = 無し)。
    pub focus: String,
    pub zoomed: bool,
}

/// [`SplitLayoutRec::to_line`] のフィールド区切り (ASCII Unit Separator)。
/// パス・プリセット名・題名のどれにも現れ得ない制御文字を使う。
const REC_FS: char = '\u{1f}';
/// 同・ノード列の区切り (ASCII Record Separator)。
const REC_RS: char = '\u{1e}';

impl SplitLayoutRec {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 1 行の文字列へ潰す。保存側 (`session.rs`) に**単純な `Vec<String>` を
    /// 1 本足すだけ**で済ませるための形 — TOML のテーブル配列の順序制約
    /// (配列は単純値より後) を踏まない。
    pub fn to_line(&self) -> String {
        if self.nodes.is_empty() {
            return String::new();
        }
        format!(
            "{}{REC_FS}{}{REC_FS}{}",
            u8::from(self.zoomed),
            self.focus,
            self.nodes.join(&REC_RS.to_string())
        )
    }

    /// [`Self::to_line`] の逆。壊れていれば空 (= 分割なし) を返す。
    pub fn from_line(line: &str) -> Self {
        let mut it = line.splitn(3, REC_FS);
        let (Some(z), Some(focus), Some(nodes)) = (it.next(), it.next(), it.next()) else {
            return Self::default();
        };
        Self {
            nodes: nodes.split(REC_RS).map(str::to_string).collect(),
            focus: focus.to_string(),
            zoomed: z == "1",
        }
    }

    /// 実行時の形へ戻す。キーを引けないセッションのリーフは落として親を畳む
    /// (壊れた保存ファイルでも panic せず、残ったペインだけで開き直す)。
    pub fn to_layout(&self, id_of: &mut dyn FnMut(&str) -> Option<SessionId>) -> SplitLayout {
        let mut i = 0usize;
        let Some(kn) = decode_node(&self.nodes, &mut i) else {
            return SplitLayout::new();
        };
        // 余分なトークンが付いていたら壊れた記録として捨てる。
        if i != self.nodes.len() {
            return SplitLayout::new();
        }
        let mut seen = Vec::new();
        let Some(root) = key_to_node(&kn, id_of, &mut seen) else {
            return SplitLayout::new();
        };
        let focus = if self.focus.is_empty() {
            None
        } else {
            id_of(&self.focus).filter(|f| root.contains(*f))
        };
        let focus = Some(focus.unwrap_or_else(|| root.first_leaf()));
        SplitLayout {
            root: Some(root),
            focus,
            zoomed: self.zoomed && focus.is_some(),
        }
    }
}

/// 元の並び `order` の `at` 番目から、生き残ったリーフを外側へ探す
/// (直後 → 直前 → …)。1 つも見つからなければ木の先頭を返す。
fn nearest_survivor(order: &[SessionId], at: usize, alive: &SplitNode) -> SessionId {
    for step in 1..=order.len() {
        if let Some(id) = order.get(at + step) {
            if alive.contains(*id) {
                return *id;
            }
        }
        if let Some(id) = at.checked_sub(step).and_then(|i| order.get(i)) {
            if alive.contains(*id) {
                return *id;
            }
        }
    }
    alive.first_leaf()
}

fn overlap(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    (a1.min(b1) - a0.max(b0)).max(0.0)
}

/// 幾何的な隣を選ぶ。近い順 → 重なりが大きい順 → 上/左が先 → ID 順 (決定的)。
fn dir_pick(
    cur: egui::Rect,
    rects: &[(SessionId, egui::Rect)],
    cur_id: SessionId,
    dir: FocusDir,
) -> Option<SessionId> {
    const EPS: f32 = 0.5;
    // (id, 隙間, 重なり, 垂直方向の開始座標)
    let mut scored: Vec<(SessionId, f32, f32, f32)> = Vec::new();
    for (id, r) in rects {
        if *id == cur_id {
            continue;
        }
        let (gap, ov, perp) = match dir {
            FocusDir::Left => (
                cur.min.x - r.max.x,
                overlap(cur.min.y, cur.max.y, r.min.y, r.max.y),
                r.min.y,
            ),
            FocusDir::Right => (
                r.min.x - cur.max.x,
                overlap(cur.min.y, cur.max.y, r.min.y, r.max.y),
                r.min.y,
            ),
            FocusDir::Up => (
                cur.min.y - r.max.y,
                overlap(cur.min.x, cur.max.x, r.min.x, r.max.x),
                r.min.x,
            ),
            FocusDir::Down => (
                r.min.y - cur.max.y,
                overlap(cur.min.x, cur.max.x, r.min.x, r.max.x),
                r.min.x,
            ),
        };
        if gap < -EPS || ov <= 0.0 {
            continue;
        }
        scored.push((*id, gap.max(0.0), ov, perp));
    }
    if scored.is_empty() {
        return None;
    }
    let min_gap = scored.iter().fold(f32::INFINITY, |m, s| m.min(s.1));
    scored.retain(|s| s.1 <= min_gap + EPS);
    scored.sort_by(|x, y| {
        y.2.partial_cmp(&x.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(x.3.partial_cmp(&y.3).unwrap_or(std::cmp::Ordering::Equal))
            .then(x.0.cmp(&y.0))
    });
    scored.first().map(|s| s.0)
}

/// 見えているペインそれぞれに「新しい矩形での行数・桁数」を通知する。
///
/// モデルは `Session` を知らないので、実際の [`Session::resize`] は
/// `emit` の中で呼んでもらう。`resize` は既存のコアレッサ経由なので、
/// UI スレッドが ConPTY の同期 RPC を待つことは無い。
///
/// `area` は `draw_split` に渡すのと同じ矩形、`cell_w`/`cell_h` は端末フォントの
/// 1 セルの大きさ。ズーム中は表示中の 1 枚にしか通知しない (隠れたペインは
/// 描かれないので、サイズを変える必要が無い)。
pub fn apply_sizes(
    layout: &SplitLayout,
    area: egui::Rect,
    gutter: f32,
    cell_w: f32,
    cell_h: f32,
    emit: &mut dyn FnMut(SessionId, u16, u16),
) {
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
    }
    // ヘッダを出す条件は `draw_split` と同じ (ズーム中も出す)。ここがズレると
    // 「描いた矩形と PTY のグリッド」が食い違い、最終行が隠れる。
    let multi = layout.len() > 1;
    for (id, r) in layout.rects(area, gutter) {
        let body = pane_body(r, multi);
        let cols = ((body.width() - TERM_PADDING * 2.0) / cell_w)
            .floor()
            .max(1.0) as u16;
        let rows = ((body.height() - TERM_PADDING * 2.0) / cell_h)
            .floor()
            .max(1.0) as u16;
        emit(id, rows, cols);
    }
}

/// 分割レイアウトを描く。
///
/// ペインの中身は呼び出し側が `leaf` で描く (Cockpit なら
/// `terminal::draw` をそのまま呼べばよい)。この関数がやるのは
/// 「矩形の割り当て」「ペインヘッダ」「仕切りの描画とドラッグ」
/// 「クリックでのフォーカス移動」だけ。
///
/// ヘッダ (アイコン・題名・活動ランプ・◎・✕) は **ペインが 2 枚以上のときだけ**
/// 出る。1 枚のタイルは今日とまったく同じ見た目のまま (`chrome` も呼ばれない)。
///
/// 戻り値の [`SplitDraw::changed`] が true = レイアウトが変わった →
/// 呼び出し側は [`apply_sizes`] を撃ち直すこと。
///
/// 再描画は要求しない (ドラッグ中は egui が自前でフレームを回す)。
/// アイドル時のゼロ再描画を壊さないため、ここから `request_repaint` は呼ばない。
/// 仕切りのドラッグも比率を書き換えるだけ — PTY への resize は呼び出し側が
/// [`apply_sizes`] → 既存のコアレッサ経由で出すので、ConPTY を叩き続けない。
pub fn draw_split(
    ui: &mut egui::Ui,
    layout: &mut SplitLayout,
    area: egui::Rect,
    gutter: f32,
    theme: &Theme,
    chrome: &mut dyn FnMut(SessionId) -> PaneChrome,
    leaf: &mut dyn FnMut(&mut egui::Ui, egui::Rect, SessionId, bool),
) -> SplitDraw {
    let mut out = SplitDraw::default();
    let changed = &mut out.changed;
    let rects = layout.rects(area, gutter);
    if rects.is_empty() {
        return out;
    }

    // クリックしたペインへフォーカスを移す。イベントは**消費しない**ので、
    // 同じクリックは端末側 (選択・カーソル移動) にもそのまま届く。
    let press = ui.input(|i| {
        if i.pointer.button_pressed(egui::PointerButton::Primary) {
            i.pointer.interact_pos()
        } else {
            None
        }
    });
    if let Some(p) = press {
        if let Some((id, _)) = rects.iter().find(|(_, r)| r.contains(p)) {
            if layout.focus() != Some(*id) && layout.set_focus(*id) {
                *changed = true;
            }
        }
    }

    let focus = layout.focus();
    // ヘッダを出すかは**タイルのペイン数**で決める。ズーム中も出す
    // (出さないとズームを戻す ◎ が消えてしまう)。`apply_sizes` と同じ条件。
    let multi = layout.len() > 1;
    let mut zoom_req = false;
    for (id, r) in &rects {
        let body = pane_body(*r, multi);
        let is_focus = focus == Some(*id);
        if multi {
            let head = egui::Rect::from_min_max(r.min, egui::pos2(r.max.x, body.min.y));
            let (hit_close, hit_zoom) = pane_header_ui(ui, head, *id, is_focus, theme, chrome);
            if hit_close {
                out.close = Some(*id);
            }
            if hit_zoom {
                // フォーカスを移してからズームする (別ペインの ◎ でも直感どおり)。
                layout.set_focus(*id);
                zoom_req = true;
            }
        }
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(body)
                .id_salt(("zv-split-pane", *id))
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        leaf(&mut child, body, *id, is_focus);
        if multi {
            // フォーカスの合図は「細い輪 + 非フォーカスを少しかすませる」。
            // 太枠にすると端末が狭く見えるので、あくまで静かに。
            if is_focus {
                ui.painter().rect_stroke(
                    r.shrink(FOCUS_RING * 0.5),
                    4.0,
                    egui::Stroke::new(FOCUS_RING, theme.accent),
                );
            } else {
                let veil = egui::Color32::from_rgba_unmultiplied(
                    theme.bg.r(),
                    theme.bg.g(),
                    theme.bg.b(),
                    DIM_ALPHA,
                );
                ui.painter().rect_filled(*r, 4.0, veil);
            }
        }
    }
    if zoom_req {
        layout.zoom_focused();
        *changed = true;
    }

    // ── 仕切り (ドラッグでリサイズ / ダブルクリックで均等化) ──
    for g in layout.gutters(area, gutter) {
        // 帯そのものは細いので、掴める範囲だけ少し広げる。
        let hit = match g.dir {
            SplitDir::Horizontal => g.rect.expand2(egui::vec2(2.0, 0.0)),
            SplitDir::Vertical => g.rect.expand2(egui::vec2(0.0, 2.0)),
        };
        let id = ui.id().with(("zv-split-gutter", g.path.as_slice()));
        let resp = ui.interact(hit, id, egui::Sense::click_and_drag());
        let hot = resp.hovered() || resp.dragged();
        if hot {
            ui.ctx().set_cursor_icon(match g.dir {
                SplitDir::Horizontal => egui::CursorIcon::ResizeHorizontal,
                SplitDir::Vertical => egui::CursorIcon::ResizeVertical,
            });
        }
        if resp.double_clicked() {
            // 掴んだ 1 本だけを 50:50 に戻す (他の分割は動かさない)。
            if layout.equalize_at(&g.path) {
                *changed = true;
            }
        } else if resp.dragged() {
            let d = resp.drag_delta();
            let (delta, span) = match g.dir {
                SplitDir::Horizontal => (d.x, g.span.width()),
                SplitDir::Vertical => (d.y, g.span.height()),
            };
            if delta != 0.0 && layout.drag_gutter(&g.path, delta, span, gutter) {
                *changed = true;
            }
        }
        let col = if hot { theme.accent } else { theme.border };
        let bar = match g.dir {
            SplitDir::Horizontal => g.rect.shrink2(egui::vec2(g.rect.width() * 0.3, 2.0)),
            SplitDir::Vertical => g.rect.shrink2(egui::vec2(2.0, g.rect.height() * 0.3)),
        };
        ui.painter().rect_filled(bar, 1.0, col);
    }

    out
}

/// ペイン 1 枚のヘッダを描く。戻り値は `(✕ が押されたか, ◎ が押されたか)`。
///
/// 幅は `head` そのまま。題名はクリップ矩形で切るだけなので、
/// どんな長さ・どんな言語 (CJK 含む) でもヘッダからはみ出さない。
fn pane_header_ui(
    ui: &mut egui::Ui,
    head: egui::Rect,
    id: SessionId,
    is_focus: bool,
    theme: &Theme,
    chrome: &mut dyn FnMut(SessionId) -> PaneChrome,
) -> (bool, bool) {
    if head.height() <= 1.0 || head.width() <= 1.0 {
        return (false, false);
    }
    let c = chrome(id);
    let p = ui.painter();
    p.rect_filled(head, 0.0, theme.panel_alt);
    // 下端に 1 本だけ罫を引いて端末本体と切り離す。
    p.rect_filled(
        egui::Rect::from_min_max(egui::pos2(head.min.x, head.max.y - 1.0), head.max),
        0.0,
        theme.border,
    );

    let bh = head.height();
    let btn = egui::vec2(bh, bh);
    let close_r = egui::Rect::from_min_size(egui::pos2(head.max.x - bh, head.min.y), btn);
    let zoom_r = egui::Rect::from_min_size(egui::pos2(head.max.x - bh * 2.0, head.min.y), btn);
    let font = egui::FontId::proportional((bh * 0.62).max(7.0));

    // 活動ランプ (◎ の左)。色は呼び出し側 = Theme 由来。
    let mut text_right = zoom_r.min.x - 2.0;
    if let Some(dot) = c.dot {
        let cx = text_right - bh * 0.35;
        ui.painter()
            .circle_filled(egui::pos2(cx, head.center().y), (bh * 0.16).max(1.5), dot);
        text_right = cx - bh * 0.35;
    }

    // アイコン + 題名。はみ出しはクリップで落とす。
    let label = if c.icon.is_empty() {
        c.title.clone()
    } else {
        format!("{} {}", c.icon, c.title)
    };
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(head.min.x + 4.0, head.min.y),
        egui::pos2(text_right.max(head.min.x + 4.0), head.max.y),
    );
    if text_rect.width() > 1.0 && !label.is_empty() {
        ui.painter().with_clip_rect(text_rect).text(
            egui::pos2(text_rect.min.x, head.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            font.clone(),
            if is_focus { theme.text } else { theme.text_dim },
        );
    }

    // ◎ / ✕。`ui.interact` は本体 (body) と重ならない位置なので、
    // 端末側のクリックを奪わない。
    let mut hit = (false, false);
    if head.width() > bh * 2.5 {
        let zoom = ui
            .interact(
                zoom_r,
                ui.id().with(("zv-pane-zoom", id)),
                egui::Sense::click(),
            )
            .on_hover_text(tr("このペインだけを大きく表示 (もう一度で戻す)"));
        let close = ui
            .interact(
                close_r,
                ui.id().with(("zv-pane-close", id)),
                egui::Sense::click(),
            )
            .on_hover_text(tr("このペインを閉じる"));
        for (r, resp, glyph) in [(zoom_r, &zoom, "◎"), (close_r, &close, "✕")] {
            let col = if resp.hovered() {
                theme.accent
            } else {
                theme.text_dim
            };
            ui.painter().text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                glyph,
                font.clone(),
                col,
            );
        }
        hit = (close.clicked(), zoom.clicked());
    }
    hit
}

#[cfg(test)]
mod split_tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))
    }

    /// 1 → 右に 2 → 下に 3 (3 は 2 の下) の 3 ペイン。
    fn three() -> SplitLayout {
        let mut l = SplitLayout::single(1);
        assert!(l.split_focused(SplitDir::Horizontal, 2));
        assert!(l.split_focused(SplitDir::Vertical, 3));
        l
    }

    /// 田の字 4 ペイン: 左列 = 1(上)/3(下)、右列 = 2(上)/4(下)。
    fn quad() -> SplitLayout {
        let mut l = SplitLayout::single(1);
        l.split_focused(SplitDir::Horizontal, 2); // 1 | 2
        l.set_focus(1);
        l.split_focused(SplitDir::Vertical, 3); // 左列 1/3
        l.set_focus(2);
        l.split_focused(SplitDir::Vertical, 4); // 右列 2/4
        l.set_focus(1);
        l
    }

    /// 壊れた木を作っていないかの共通チェック。
    fn assert_invariants(l: &SplitLayout, what: &str) {
        let ls = l.leaves();
        let mut sorted = ls.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ls.len(), "{what}: リーフが重複している");
        assert_eq!(ls.len(), l.len(), "{what}: leaf_count がズレている");
        match l.focus() {
            Some(f) => assert!(ls.contains(&f), "{what}: 木に居ないペインへフォーカス"),
            None => assert!(ls.is_empty(), "{what}: ペインがあるのにフォーカス無し"),
        }
        // 分割比は必ず [MIN, 1-MIN]。
        fn walk(n: &SplitNode, what: &str) {
            if let SplitNode::Split { ratio, a, b, .. } = n {
                assert!(
                    (MIN_RATIO..=1.0 - MIN_RATIO).contains(ratio),
                    "{what}: 比率が範囲外 {ratio}"
                );
                walk(a, what);
                walk(b, what);
            }
        }
        if let Some(r) = l.root() {
            walk(r, what);
        }
    }

    #[test]
    fn split_close_invariants_table() {
        // (操作列, 期待するペイン, 期待するフォーカス)
        type Op = (&'static str, u64);
        let cases: &[(&str, &[Op], &[u64], Option<u64>)] = &[
            ("空から 1 枚", &[("h", 1)], &[1], Some(1)),
            ("右に 2 枚", &[("h", 1), ("h", 2)], &[1, 2], Some(2)),
            (
                "縦横まぜて 3 枚",
                &[("h", 1), ("h", 2), ("v", 3)],
                &[1, 2, 3],
                Some(3),
            ),
            (
                "重複 ID は拒否",
                &[("h", 1), ("h", 2), ("h", 2)],
                &[1, 2],
                Some(2),
            ),
            (
                "閉じて親が畳まれる",
                &[("h", 1), ("h", 2), ("v", 3), ("x", 3)],
                &[1, 2],
                Some(2),
            ),
            (
                "全部閉じたら空",
                &[("h", 1), ("h", 2), ("x", 1), ("x", 2)],
                &[],
                None,
            ),
            (
                "居ないペインを閉じても無害",
                &[("h", 1), ("x", 99)],
                &[1],
                Some(1),
            ),
        ];
        for (name, ops, want_leaves, want_focus) in cases {
            let mut l = SplitLayout::new();
            for (op, id) in *ops {
                match *op {
                    "h" => {
                        l.split_focused(SplitDir::Horizontal, *id);
                    }
                    "v" => {
                        l.split_focused(SplitDir::Vertical, *id);
                    }
                    "x" => {
                        l.close_leaf(*id);
                    }
                    _ => unreachable!(),
                }
                assert_invariants(&l, name);
            }
            assert_eq!(&l.leaves(), want_leaves, "{name}: ペイン一覧");
            assert_eq!(l.focus(), *want_focus, "{name}: フォーカス");
        }
    }

    #[test]
    fn close_focused_moves_focus_to_sibling() {
        let mut l = quad();
        l.set_focus(3);
        assert!(l.close_leaf(3));
        // 3 の兄弟は 1 → そこへ移る
        assert_eq!(l.focus(), Some(1));
        assert_eq!(l.leaves(), vec![1, 2, 4]);
        assert_invariants(&l, "兄弟へフォーカス");
    }

    #[test]
    fn close_unfocused_keeps_focus() {
        let mut l = quad(); // focus = 1
        assert!(l.close_leaf(4));
        assert_eq!(l.focus(), Some(1));
        assert_eq!(l.leaves(), vec![1, 3, 2]);
    }

    #[test]
    fn rects_stay_inside_and_never_overlap() {
        let areas = [
            rect(0.0, 0.0, 800.0, 600.0),
            rect(12.5, 7.25, 333.0, 199.0),
            rect(-40.0, -10.0, 1000.0, 61.0),
            rect(0.0, 0.0, 20.0, 20.0), // ガターより狭い極小
        ];
        let gutters = [0.0, 6.0, 13.0];
        let layouts: [(&str, SplitLayout); 3] = [
            ("1枚", SplitLayout::single(7)),
            ("3枚", three()),
            ("田の字", quad()),
        ];
        for (name, l) in &layouts {
            for a in areas {
                for g in gutters {
                    let rs = l.rects(a, g);
                    assert_eq!(rs.len(), l.len(), "{name}: 枚数");
                    for (id, r) in &rs {
                        assert!(
                            r.min.x >= a.min.x - 0.01
                                && r.min.y >= a.min.y - 0.01
                                && r.max.x <= a.max.x + 0.01
                                && r.max.y <= a.max.y + 0.01,
                            "{name}: #{id} が領域外 {r:?} ⊄ {a:?}"
                        );
                        assert!(r.width() >= 0.0 && r.height() >= 0.0, "{name}: 負のサイズ");
                    }
                    for i in 0..rs.len() {
                        for j in (i + 1)..rs.len() {
                            let (x, y) = (rs[i].1, rs[j].1);
                            let ov = overlap(x.min.x, x.max.x, y.min.x, y.max.x)
                                * overlap(x.min.y, x.max.y, y.min.y, y.max.y);
                            assert!(ov <= 0.01, "{name}: #{} と #{} が重なる", rs[i].0, rs[j].0);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn rects_are_deterministic() {
        let l = quad();
        let a = rect(3.5, 11.25, 777.0, 401.0);
        assert_eq!(l.rects(a, 6.0), l.rects(a, 6.0));
        assert_eq!(quad().rects(a, 6.0), l.rects(a, 6.0));
    }

    #[test]
    fn gutter_is_accounted_in_geometry() {
        let l = {
            let mut l = SplitLayout::single(1);
            l.split_focused(SplitDir::Horizontal, 2);
            l
        };
        let a = rect(0.0, 0.0, 106.0, 50.0);
        let rs = l.rects(a, 6.0);
        // 使える幅は 100 → 50/50、間に 6px の仕切り。
        assert_eq!(rs[0].1.width(), 50.0);
        assert_eq!(rs[1].1.width(), 50.0);
        assert_eq!(rs[1].1.min.x - rs[0].1.max.x, 6.0);
        let gs = l.gutters(a, 6.0);
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].rect.width(), 6.0);
        assert!(gs[0].path.is_empty(), "ルートの分割は空経路");
    }

    #[test]
    fn focus_dir_on_quad_table() {
        // 田の字: 左上=1 右上=2 左下=3 右下=4
        let a = rect(0.0, 0.0, 400.0, 400.0);
        let cases: &[(u64, FocusDir, Option<u64>)] = &[
            (1, FocusDir::Right, Some(2)),
            (1, FocusDir::Down, Some(3)),
            (1, FocusDir::Left, None),
            (1, FocusDir::Up, None),
            (2, FocusDir::Left, Some(1)),
            (2, FocusDir::Down, Some(4)),
            (2, FocusDir::Right, None),
            (2, FocusDir::Up, None),
            (3, FocusDir::Up, Some(1)),
            (3, FocusDir::Right, Some(4)),
            (3, FocusDir::Left, None),
            (3, FocusDir::Down, None),
            (4, FocusDir::Up, Some(2)),
            (4, FocusDir::Left, Some(3)),
            (4, FocusDir::Right, None),
            (4, FocusDir::Down, None),
        ];
        for (from, dir, want) in cases {
            let mut l = quad();
            assert!(l.set_focus(*from));
            let moved = l.focus_dir(*dir, a, 6.0);
            match want {
                Some(w) => {
                    assert!(moved, "{from} から {dir:?} は動くはず");
                    assert_eq!(l.focus(), Some(*w), "{from} から {dir:?}");
                }
                None => {
                    assert!(!moved, "{from} から {dir:?} は端なので動かない");
                    assert_eq!(l.focus(), Some(*from));
                }
            }
        }
    }

    #[test]
    fn focus_dir_prefers_nearest_column_over_widest_overlap() {
        // 3 列: 1 | (2 上 / 3 下) | 4。1 から Right は「隣の列」の 2 が正解で、
        // 重なりが大きい 4 (全高) に飛んではいけない。
        let mut l = SplitLayout::single(1);
        l.split_focused(SplitDir::Horizontal, 9); // 1 | 9
        l.split_focused(SplitDir::Horizontal, 4); // 1 | (9 | 4)
        l.set_focus(9);
        l.split_focused(SplitDir::Vertical, 3); // 中列を 9/3 に
        l.set_focus(1);
        assert!(l.focus_dir(FocusDir::Right, rect(0.0, 0.0, 900.0, 300.0), 6.0));
        assert_eq!(l.focus(), Some(9), "隣の列の上側へ");
    }

    #[test]
    fn resize_focused_clamps_table() {
        // (フォーカス, デルタ列, 期待比率)
        let cases: &[(u64, &[f32], f32)] = &[
            (2, &[0.1], 0.4),              // b 側 → 広がると ratio は減る
            (1, &[0.1], 0.6),              // a 側 → 増える
            (1, &[10.0], 1.0 - MIN_RATIO), // 上限クランプ
            (1, &[-10.0], MIN_RATIO),      // 下限クランプ
            (2, &[10.0], MIN_RATIO),       // b 側の上限 = a の下限
            (1, &[0.1, 0.1, -0.2], 0.5),   // 往復
        ];
        for (focus, deltas, want) in cases {
            let mut l = SplitLayout::single(1);
            l.split_focused(SplitDir::Horizontal, 2);
            assert!(l.set_focus(*focus));
            for d in *deltas {
                l.resize_focused(*d);
            }
            let got = l.ratio_at(&[]).expect("ルートは分割ノード");
            assert!(
                (got - want).abs() < 1e-5,
                "focus={focus} deltas={deltas:?}: {got} != {want}"
            );
            assert_invariants(&l, "リサイズ後");
        }
    }

    #[test]
    fn drag_gutter_respects_min_pane_px() {
        let mut l = SplitLayout::single(1);
        l.split_focused(SplitDir::Horizontal, 2);
        // 200px の領域 (使える幅 194) を思い切り左へ引く → 48px 未満にはしない
        l.drag_gutter(&[], -1000.0, 200.0, 6.0);
        let r = l.ratio_at(&[]).unwrap();
        assert!(r * 194.0 >= MIN_PANE_PX - 0.5, "左ペインが潰れた: {r}");
        l.drag_gutter(&[], 1000.0, 200.0, 6.0);
        let r = l.ratio_at(&[]).unwrap();
        assert!(
            (1.0 - r) * 194.0 >= MIN_PANE_PX - 0.5,
            "右ペインが潰れた: {r}"
        );
    }

    #[test]
    fn zoom_shows_only_focus_and_restores_geometry() {
        let a = rect(0.0, 0.0, 400.0, 400.0);
        let mut l = quad();
        l.set_focus(4);
        let before = l.rects(a, 6.0);
        assert_eq!(before.len(), 4);

        assert!(l.zoom_focused(), "ズーム ON");
        let zoomed = l.rects(a, 6.0);
        assert_eq!(zoomed, vec![(4, a)], "ズーム中は 1 枚が全面");
        assert!(l.gutters(a, 6.0).is_empty(), "ズーム中は仕切りを出さない");

        assert!(!l.zoom_focused(), "ズーム OFF");
        assert_eq!(l.rects(a, 6.0), before, "元の幾何に戻る");

        // ズーム中に分割したら自動で解除される (見えないペインを作らない)
        l.zoom_focused();
        l.split_focused(SplitDir::Vertical, 5);
        assert!(!l.zoomed());
        assert_eq!(l.rects(a, 6.0).len(), 5);
    }

    #[test]
    fn equalize_makes_equal_areas() {
        let a = rect(0.0, 0.0, 600.0, 600.0);
        let mut l = three(); // 1 | (2 / 3)
        l.drag_gutter(&[], 200.0, 600.0, 0.0);
        l.equalize();
        let rs = l.rects(a, 0.0);
        // 左 1 枚 : 右 2 枚 → 1/3 : 2/3
        assert!((rs[0].1.width() - 200.0).abs() <= 1.0, "{:?}", rs[0].1);
        assert!((rs[1].1.width() - 400.0).abs() <= 1.0, "{:?}", rs[1].1);
        let areas: Vec<f32> = rs.iter().map(|(_, r)| r.width() * r.height()).collect();
        for w in &areas {
            assert!((w - areas[0]).abs() <= 600.0, "面積が揃わない {areas:?}");
        }
        assert_invariants(&l, "均等化後");
    }

    #[test]
    fn serde_round_trip_and_heal() {
        let mut l = quad();
        l.set_focus(3);
        l.drag_gutter(&[], 40.0, 400.0, 6.0);
        let key = |id: SessionId| Some(format!("/logs/agent-{id}.log"));
        let rec = l.to_rec(&mut |id| key(id));

        // TOML で往復しても壊れない (実際の保存経路と同じ形式)。
        let toml_s = toml::to_string(&rec).expect("TOML へ書ける");
        let back: SplitLayoutRec = toml::from_str(&toml_s).expect("TOML から読める");
        assert_eq!(back, rec);

        let ids = |k: &str| -> Option<SessionId> {
            k.rsplit_once("agent-")
                .and_then(|(_, r)| r.strip_suffix(".log"))
                .and_then(|n| n.parse().ok())
        };
        let restored = back.to_layout(&mut |k| ids(k));
        assert_eq!(restored.leaves(), l.leaves());
        assert_eq!(restored.focus(), Some(3));
        assert!((restored.ratio_at(&[]).unwrap() - l.ratio_at(&[]).unwrap()).abs() < 1e-4);
        assert_invariants(&restored, "復元後");

        // セッションが 1 本消えていたら、そのリーフを落として親を畳む。
        let healed = back.to_layout(&mut |k| ids(k).filter(|id| *id != 3));
        assert_eq!(healed.leaves(), vec![1, 2, 4], "3 だけ消えて畳まれる");
        assert_eq!(healed.focus(), Some(1), "消えたフォーカスは先頭へ");
        assert_invariants(&healed, "自己修復後");

        // 全滅・壊れた記録でも panic しない。
        assert!(back.to_layout(&mut |_| None).is_empty());
        let broken = SplitLayoutRec {
            nodes: vec!["H:0.5".into(), "L:/logs/agent-1.log".into()], // b が足りない
            focus: String::new(),
            zoomed: false,
        };
        assert!(broken.to_layout(&mut |k| ids(k)).is_empty());
        assert!(SplitLayoutRec::default()
            .to_layout(&mut |k| ids(k))
            .is_empty());
    }

    #[test]
    fn to_rec_drops_leaves_without_a_stable_key() {
        let l = quad();
        let rec = l.to_rec(&mut |id| (id != 2).then(|| format!("k{id}")));
        let back = rec.to_layout(&mut |k| k.strip_prefix('k').and_then(|n| n.parse().ok()));
        assert_eq!(back.leaves(), vec![1, 3, 4]);
        assert_invariants(&back, "キー欠けの保存");
    }

    #[test]
    fn heal_drops_dead_sessions() {
        let mut l = quad();
        l.set_focus(2);
        let alive = [1u64, 4];
        assert!(l.heal(&mut |id| alive.contains(&id)));
        assert_eq!(l.leaves(), vec![1, 4]);
        // 元の並びは [1,3,2,4]。死んだ 2 の**直後**の生存ペインは 4。
        // (先頭 1 へ飛ばすと、視線が関係ない左上へ吹っ飛ぶ)
        assert_eq!(l.focus(), Some(4));
        assert!(!l.heal(&mut |id| alive.contains(&id)), "2 度目は変化なし");
        assert!(l.heal(&mut |_| false));
        assert!(l.is_empty());
        assert_eq!(l.focus(), None);
    }

    #[test]
    fn apply_sizes_emits_one_call_per_visible_leaf() {
        let a = rect(0.0, 0.0, 812.0, 612.0);
        let (cw, ch) = (8.0, 16.0);
        let mut l = SplitLayout::single(1);
        l.split_focused(SplitDir::Horizontal, 2); // 使える幅 806 → 403 / 403

        let mut got: Vec<(SessionId, u16, u16)> = Vec::new();
        apply_sizes(&l, a, 6.0, cw, ch, &mut |id, r, c| got.push((id, r, c)));
        // 2 枚あるのでヘッダ (18) が乗る。
        // cols = floor((403 - 12) / 8) = 48、rows = floor((612 - 18 - 12) / 16) = 36
        assert_eq!(got, vec![(1, 36, 48), (2, 36, 48)]);

        // 上下分割: 使える高さ 606 → 303 / 303、
        // rows = floor((303 - 18 - 12) / 16) = 17
        let mut l2 = SplitLayout::single(1);
        l2.split_focused(SplitDir::Vertical, 2);
        let mut got2: Vec<(SessionId, u16, u16)> = Vec::new();
        apply_sizes(&l2, a, 6.0, cw, ch, &mut |id, r, c| got2.push((id, r, c)));
        assert_eq!(got2, vec![(1, 17, 100), (2, 17, 100)]);

        // ズーム中は見えている 1 枚だけ (全面サイズ)。ヘッダは残る
        // (◎ が消えるとズームを戻せなくなるため) → rows = floor((612-18-12)/16) = 36
        l2.set_focus(2);
        l2.zoom_focused();
        let mut got3: Vec<(SessionId, u16, u16)> = Vec::new();
        apply_sizes(&l2, a, 6.0, cw, ch, &mut |id, r, c| got3.push((id, r, c)));
        assert_eq!(got3, vec![(2, 36, 100)]);

        // 1 枚だけのタイルはヘッダを取らない = 今日とまったく同じ寸法。
        // rows = floor((612 - 12) / 16) = 37、cols = floor((812 - 12) / 8) = 100
        let mut solo: Vec<(SessionId, u16, u16)> = Vec::new();
        apply_sizes(&SplitLayout::single(9), a, 6.0, cw, ch, &mut |id, r, c| {
            solo.push((id, r, c))
        });
        assert_eq!(solo, vec![(9, 37, 100)]);

        // セル幅 0 の異常系では 1 件も出さない (0 除算で NaN を配らない)。
        let mut none: Vec<(SessionId, u16, u16)> = Vec::new();
        apply_sizes(&l2, a, 6.0, 0.0, ch, &mut |id, r, c| none.push((id, r, c)));
        assert!(none.is_empty());
    }

    #[test]
    fn empty_layout_is_harmless() {
        let mut l = SplitLayout::new();
        let a = rect(0.0, 0.0, 100.0, 100.0);
        assert!(l.rects(a, 6.0).is_empty());
        assert!(l.gutters(a, 6.0).is_empty());
        assert!(!l.focus_dir(FocusDir::Left, a, 6.0));
        assert!(!l.resize_focused(0.1));
        assert!(!l.zoom_focused());
        assert!(!l.close_leaf(1));
        assert!(!l.set_focus(1));
        l.equalize();
        assert!(l.is_empty());
    }

    // ────────────────────────────────────────────────────────────────
    // キーの決定表 (split_key_action)
    // ────────────────────────────────────────────────────────────────

    /// macOS の Cmd+Option。
    fn mac_mod(shift: bool) -> egui::Modifiers {
        egui::Modifiers {
            alt: true,
            ctrl: false,
            shift,
            mac_cmd: true,
            command: true,
        }
    }
    /// Windows / Linux の Ctrl+Alt。
    fn win_mod(shift: bool) -> egui::Modifiers {
        egui::Modifiers {
            alt: true,
            ctrl: true,
            shift,
            mac_cmd: false,
            command: true,
        }
    }

    const SPLIT_R: SplitAction = SplitAction::SplitWith {
        dir: SplitDir::Horizontal,
        preset: PanePreset::NewAgent,
    };
    const SPLIT_D: SplitAction = SplitAction::SplitWith {
        dir: SplitDir::Vertical,
        preset: PanePreset::NewAgent,
    };
    const SPLIT_SHELL: SplitAction = SplitAction::SplitWith {
        dir: SplitDir::Horizontal,
        preset: PanePreset::Shell,
    };

    /// 同じ表が macOS (Cmd+Option) でも Windows/Linux (Ctrl+Alt) でも成立する。
    #[test]
    fn split_key_action_table_both_platforms() {
        use egui::Key as K;
        // (キー, Shift, 期待するアクション)
        let table: &[(egui::Key, bool, SplitAction)] = &[
            (K::ArrowRight, true, SPLIT_R),
            (K::ArrowDown, true, SPLIT_D),
            (K::N, false, SPLIT_SHELL),
            (K::W, false, SplitAction::ClosePane),
            (K::Z, false, SplitAction::Zoom),
            (K::E, false, SplitAction::Equalize),
            (K::ArrowLeft, false, SplitAction::Focus(FocusDir::Left)),
            (K::ArrowRight, false, SplitAction::Focus(FocusDir::Right)),
            (K::ArrowUp, false, SplitAction::Focus(FocusDir::Up)),
            (K::ArrowDown, false, SplitAction::Focus(FocusDir::Down)),
            (K::H, false, SplitAction::Focus(FocusDir::Left)),
            (K::J, false, SplitAction::Focus(FocusDir::Down)),
            (K::K, false, SplitAction::Focus(FocusDir::Up)),
            (K::L, false, SplitAction::Focus(FocusDir::Right)),
            // Shift+H / Shift+L = キーボードでの幅調整 (ガタードラッグ相当)
            (K::H, true, SplitAction::Resize { grow: false }),
            (K::L, true, SplitAction::Resize { grow: true }),
        ];
        for (key, shift, want) in table {
            assert_eq!(
                split_key_action(*key, &mac_mod(*shift), true),
                Some(*want),
                "macOS: {key:?} shift={shift}"
            );
            assert_eq!(
                split_key_action(*key, &win_mod(*shift), false),
                Some(*want),
                "Windows/Linux: {key:?} shift={shift}"
            );
        }
    }

    /// **奪ってはいけない**打鍵。ここが false になると端末に文字が打てなくなる。
    #[test]
    fn split_key_action_never_steals_terminal_input() {
        use egui::Key as K;
        let none = egui::Modifiers::NONE;
        let ctrl_only = egui::Modifiers {
            alt: false,
            ctrl: true,
            shift: false,
            mac_cmd: false,
            command: true,
        };
        let alt_only = egui::Modifiers {
            alt: true,
            ctrl: false,
            shift: false,
            mac_cmd: false,
            command: false,
        };
        // (キー, 修飾, mac か)
        let never: &[(egui::Key, egui::Modifiers, bool)] = &[
            // 素の文字・矢印は当然すべて端末へ
            (K::W, none, true),
            (K::Z, none, false),
            (K::ArrowRight, none, true),
            // Ctrl+C / Ctrl+D — macOS で奪ったらシェルが操作不能になる
            (K::C, ctrl_only, true),
            (K::D, ctrl_only, true),
            (K::W, ctrl_only, true),
            // Windows/Linux でも単独 Ctrl+文字 は端末のもの
            (K::W, ctrl_only, false),
            (K::E, ctrl_only, false),
            // Alt 単独 = readline の Meta (Alt+B / Alt+F など)
            (K::H, alt_only, true),
            (K::L, alt_only, false),
            // Alt+矢印 は keybinds.rs の MoveLineUp/Down
            (K::ArrowUp, alt_only, true),
            (K::ArrowDown, alt_only, false),
            // macOS で Ctrl+Alt は端末のもの (Cmd が要る)
            (K::Z, win_mod(false), true),
            // Windows で Cmd(=mac_cmd)+Alt は来ない
            (K::Z, mac_mod(false), false),
            // 表に無いキーは Shift 有無に関わらず素通し
            (K::Q, mac_mod(false), true),
            (K::Q, win_mod(false), false),
            (K::Z, mac_mod(true), true),
            (K::W, win_mod(true), false),
            (K::ArrowLeft, mac_mod(true), true),
            (K::ArrowUp, win_mod(true), false),
        ];
        for (key, mods, mac) in never {
            assert_eq!(
                split_key_action(*key, mods, *mac),
                None,
                "奪ってはいけない: {key:?} {mods:?} mac={mac}"
            );
        }
    }

    /// `keybinds.rs` の既定表と 1 つも衝突しない (端末フォーカス中に
    /// グローバルショートカットを覆い隠さない)。
    #[test]
    fn split_chords_do_not_shadow_global_shortcuts() {
        use egui::Key as K;
        // 分割が使うキー
        let ours = [
            K::N,
            K::W,
            K::Z,
            K::E,
            K::H,
            K::J,
            K::K,
            K::L,
            K::ArrowLeft,
            K::ArrowRight,
            K::ArrowUp,
            K::ArrowDown,
        ];
        // `cmd+alt` / `ctrl+alt` を既に使っている既定ショートカットのキー
        let taken = [
            K::D,            // toggle_deck
            K::S,            // save_all
            K::F,            // open_replace
            K::B,            // toggle_bookmark
            K::OpenBracket,  // move_tab_left
            K::CloseBracket, // move_tab_right
        ];
        for k in ours {
            assert!(!taken.contains(&k), "{k:?} は cmd+alt で既に使われている");
        }
    }

    // ────────────────────────────────────────────────────────────────
    // ペインヘッダぶんの幾何
    // ────────────────────────────────────────────────────────────────

    /// ヘッダは 2 枚以上のときだけ乗る。本体は必ず領域の内側で重ならない。
    #[test]
    fn pane_body_accounts_chrome_only_when_multi() {
        let a = rect(10.0, 20.0, 800.0, 600.0);

        // 1 枚 = 今日とまったく同じ (1 px も削らない)
        let solo = SplitLayout::single(1);
        let rs = solo.rects(a, GUTTER);
        assert_eq!(rs.len(), 1);
        assert_eq!(pane_body(rs[0].1, solo.len() > 1), rs[0].1);
        assert_eq!(rs[0].1, a);

        // 4 枚 = すべてにヘッダ
        let q = quad();
        let rs = q.rects(a, GUTTER);
        assert_eq!(rs.len(), 4);
        let bodies: Vec<egui::Rect> = rs.iter().map(|(_, r)| pane_body(*r, true)).collect();
        for (i, (b, (_, full))) in bodies.iter().zip(rs.iter()).enumerate() {
            assert!(a.contains_rect(*b), "本体 {i} が領域からはみ出した");
            assert!(
                (b.min.y - full.min.y - PANE_HEADER_H).abs() < 0.01,
                "本体 {i} のヘッダ高が違う"
            );
            assert_eq!(b.min.x, full.min.x);
            assert_eq!(b.max, full.max);
            assert!(b.height() > 0.0 && b.width() > 0.0);
        }
        for i in 0..bodies.len() {
            for j in (i + 1)..bodies.len() {
                let x = bodies[i].intersect(bodies[j]);
                assert!(
                    x.width() <= 0.01 || x.height() <= 0.01,
                    "本体 {i} と {j} が重なった"
                );
            }
        }

        // 極端に低いペインでもヘッダが本体を食い潰さない
        let tiny = rect(0.0, 0.0, 100.0, 10.0);
        let b = pane_body(tiny, true);
        assert!(b.height() > 0.0 && b.height() <= tiny.height());
        assert!(tiny.contains_rect(b));
    }

    // ────────────────────────────────────────────────────────────────
    // フォーカス移動 (3 枚 / 4 枚 × 全方向)
    // ────────────────────────────────────────────────────────────────

    /// 3 ペイン (左 = 1 / 右上 = 2 / 右下 = 3) を全方向でなぞる。
    #[test]
    fn focus_dir_on_three_pane_table() {
        let a = rect(0.0, 0.0, 800.0, 600.0);
        // (開始, 向き, 期待する行き先。None = 端で動かない)
        let table: &[(SessionId, FocusDir, Option<SessionId>)] = &[
            (1, FocusDir::Right, Some(2)),
            (1, FocusDir::Left, None),
            (1, FocusDir::Up, None),
            (1, FocusDir::Down, None),
            (2, FocusDir::Left, Some(1)),
            (2, FocusDir::Down, Some(3)),
            (2, FocusDir::Up, None),
            (2, FocusDir::Right, None),
            (3, FocusDir::Left, Some(1)),
            (3, FocusDir::Up, Some(2)),
            (3, FocusDir::Down, None),
            (3, FocusDir::Right, None),
        ];
        for (from, dir, want) in table {
            let mut l = three();
            assert!(l.set_focus(*from));
            let moved = l.focus_dir(*dir, a, GUTTER);
            match want {
                Some(w) => {
                    assert!(moved, "{from} から {dir:?} へ動かなかった");
                    assert_eq!(l.focus(), Some(*w), "{from} から {dir:?}");
                }
                None => {
                    assert!(!moved, "{from} から {dir:?} は端のはずが動いた");
                    assert_eq!(l.focus(), Some(*from));
                }
            }
        }
    }

    /// 田の字 4 枚を全方向でなぞる (往復して元へ戻ることも見る)。
    #[test]
    fn focus_dir_on_quad_round_trips() {
        let a = rect(0.0, 0.0, 800.0, 600.0);
        // 左列 1(上)/3(下)、右列 2(上)/4(下)
        let pairs: &[(SessionId, FocusDir, SessionId, FocusDir)] = &[
            (1, FocusDir::Right, 2, FocusDir::Left),
            (1, FocusDir::Down, 3, FocusDir::Up),
            (2, FocusDir::Down, 4, FocusDir::Up),
            (3, FocusDir::Right, 4, FocusDir::Left),
        ];
        for (from, go, to, back) in pairs {
            let mut l = quad();
            assert!(l.set_focus(*from));
            assert!(l.focus_dir(*go, a, GUTTER), "{from} → {go:?}");
            assert_eq!(l.focus(), Some(*to));
            assert!(l.focus_dir(*back, a, GUTTER), "{to} → {back:?}");
            assert_eq!(l.focus(), Some(*from), "{from} へ戻らなかった");
        }
        // 四隅の外向きは端で止まる
        for (id, dir) in [
            (1u64, FocusDir::Left),
            (1, FocusDir::Up),
            (2, FocusDir::Right),
            (2, FocusDir::Up),
            (3, FocusDir::Left),
            (3, FocusDir::Down),
            (4, FocusDir::Right),
            (4, FocusDir::Down),
        ] {
            let mut l = quad();
            assert!(l.set_focus(id));
            assert!(!l.focus_dir(dir, a, GUTTER), "{id} の {dir:?} は端のはず");
            assert_eq!(l.focus(), Some(id));
        }
    }

    // ────────────────────────────────────────────────────────────────
    // 寿命 (プロセス終了で木が畳まれる)
    // ────────────────────────────────────────────────────────────────

    /// フォーカス中のペインが終了したら、**並び順で最も近い**生存ペインへ移る。
    #[test]
    fn exit_heals_tree_and_moves_focus_to_nearest() {
        let a = rect(0.0, 0.0, 800.0, 600.0);
        // 3 枚目 (in-order の真ん中の次) が死んだら 4 枚目へ
        let mut l = quad();
        assert_eq!(l.leaves(), vec![1, 3, 2, 4]);
        assert!(l.set_focus(2));
        assert!(l.heal(&mut |id| id != 2));
        assert_eq!(l.focus(), Some(4), "2 の直後の生存ペインへ移るはず");
        assert_eq!(l.len(), 3);
        assert!(!l.contains(2));
        assert_invariants(&l, "1 枚落ちた後");
        // 幾何は生き残った 3 枚で領域を埋め直す (兄弟が場所を継ぐ)
        let rs = l.rects(a, GUTTER);
        assert_eq!(rs.len(), 3);
        for (_, r) in &rs {
            assert!(a.contains_rect(*r));
        }

        // 末尾が死んだら直前へ
        let mut l = quad();
        assert!(l.set_focus(4));
        assert!(l.heal(&mut |id| id != 4));
        assert_eq!(l.focus(), Some(2), "末尾が死んだら直前の生存ペインへ");

        // フォーカスしていないペインが死んでもフォーカスは動かない
        let mut l = quad();
        assert!(l.set_focus(1));
        assert!(l.heal(&mut |id| id != 3));
        assert_eq!(l.focus(), Some(1));

        // 全滅したら空になり、ズームも解ける (タイルは「閉じたエージェント」扱い)
        let mut l = quad();
        l.zoom_focused();
        assert!(l.heal(&mut |_| false));
        assert!(l.is_empty());
        assert_eq!(l.focus(), None);
        assert!(!l.zoomed());
        assert!(l.rects(a, GUTTER).is_empty());
    }

    /// 最後の 1 枚を閉じるとタイルは空になる (= 閉じたエージェントと同じ)。
    #[test]
    fn closing_the_last_pane_empties_the_tile() {
        let mut l = SplitLayout::single(7);
        assert!(l.close_leaf(7));
        assert!(l.is_empty());
        assert_eq!(l.focus(), None);
        assert_eq!(l.len(), 0);
    }

    // ────────────────────────────────────────────────────────────────
    // 保存と復元 (1 行表現)
    // ────────────────────────────────────────────────────────────────

    /// 1 行に潰して往復できる。復元時に**居ないセッションのリーフは黙って落ちる**。
    #[test]
    fn to_line_round_trip_drops_missing_sessions() {
        let mut l = quad();
        assert!(l.set_focus(3));
        // 安定キー = ログのパス相当 (`:` や空白を含んでも壊れないこと)
        let key_of = |id: SessionId| Some(format!("/var/log s/zai:{id}.log"));
        let line = l.to_rec(&mut |id| key_of(id)).to_line();
        assert!(!line.is_empty());

        // 素直な往復
        let back = SplitLayoutRec::from_line(&line);
        assert_eq!(back, l.to_rec(&mut |id| key_of(id)));
        let mut ids: Vec<(String, SessionId)> = (1..=4).map(|i| (key_of(i).unwrap(), i)).collect();
        let restored = back.to_layout(&mut |k| ids.iter().find(|(s, _)| s == k).map(|(_, i)| *i));
        assert_eq!(restored.leaves(), l.leaves());
        assert_eq!(restored.focus(), Some(3));
        assert_invariants(&restored, "往復後");

        // セッション 3 が消えていた場合 — 落として親を畳み、panic しない
        ids.retain(|(_, i)| *i != 3);
        let healed = SplitLayoutRec::from_line(&line)
            .to_layout(&mut |k| ids.iter().find(|(s, _)| s == k).map(|(_, i)| *i));
        assert_eq!(healed.len(), 3);
        assert!(!healed.contains(3));
        assert!(healed.focus().is_some_and(|f| healed.contains(f)));
        assert_invariants(&healed, "欠けたセッションを落とした後");

        // 全部消えていたら空 (分割なし) に戻る
        let gone = SplitLayoutRec::from_line(&line).to_layout(&mut |_| None);
        assert!(gone.is_empty());

        // 壊れた行・空行でも panic せず空を返す
        for bad in ["", "ごみ", "1\u{1f}x", "\u{1f}\u{1f}"] {
            let r = SplitLayoutRec::from_line(bad);
            assert!(r.to_layout(&mut |_| Some(1)).len() <= 1);
        }
        // 分割していないレイアウトは空行になる (保存領域を汚さない)
        assert!(SplitLayout::new()
            .to_rec(&mut |_| None)
            .to_line()
            .is_empty());
    }

    /// ズームの状態も 1 行に乗る。
    #[test]
    fn to_line_carries_zoom_flag() {
        let mut l = three();
        l.set_focus(2);
        assert!(l.zoom_focused());
        let line = l.to_rec(&mut |id| Some(id.to_string())).to_line();
        let back = SplitLayoutRec::from_line(&line);
        assert!(back.zoomed);
        let restored = back.to_layout(&mut |k| k.parse::<SessionId>().ok());
        assert!(restored.zoomed());
        assert_eq!(restored.focus(), Some(2));
    }

    // ────────────────────────────────────────────────────────────────
    // ガター
    // ────────────────────────────────────────────────────────────────

    /// ドラッグは最小ペイン幅でクランプされ、行き過ぎても比率が壊れない。
    #[test]
    fn gutter_drag_clamps_at_both_ends() {
        let span = 400.0_f32;
        let avail = span - GUTTER;
        for push in [-10_000.0_f32, -span, -1.0, 1.0, span, 10_000.0] {
            let mut l = SplitLayout::single(1);
            l.split_focused(SplitDir::Horizontal, 2);
            l.drag_gutter(&[], push, span, GUTTER);
            let r = l.ratio_at(&[]).unwrap();
            let lo = MIN_RATIO.max(MIN_PANE_PX / avail);
            assert!(
                r >= lo - 1e-4 && r <= 1.0 - lo + 1e-4,
                "比率 {r} が最小ペイン幅を割った (push={push})"
            );
            // どちらのペインも MIN_PANE_PX を下回らない
            let rs = l.rects(rect(0.0, 0.0, span, 100.0), GUTTER);
            for (id, rr) in rs {
                assert!(rr.width() >= MIN_PANE_PX - 1.0, "ペイン {id} が潰れた");
            }
        }
    }

    /// ダブルクリック相当 (`equalize_at`) は**掴んだ 1 本だけ**を 50:50 に戻す。
    #[test]
    fn equalize_at_only_touches_that_divider() {
        let mut l = three(); // ルート = 左右分割、その b 側が上下分割
        l.drag_gutter(&[], 60.0, 400.0, GUTTER);
        l.drag_gutter(&[true], 40.0, 300.0, GUTTER);
        let outer = l.ratio_at(&[]).unwrap();
        let inner = l.ratio_at(&[true]).unwrap();
        assert!((outer - 0.5).abs() > 0.01 && (inner - 0.5).abs() > 0.01);

        assert!(l.equalize_at(&[true]));
        assert!((l.ratio_at(&[true]).unwrap() - 0.5).abs() < 1e-6);
        assert_eq!(l.ratio_at(&[]).unwrap(), outer, "他の仕切りは動かさない");

        // 既に 50:50 なら変化なし / 存在しない経路は false
        assert!(!l.equalize_at(&[true]));
        assert!(!l.equalize_at(&[false]), "リーフを指す経路は無視する");
        assert!(!SplitLayout::new().equalize_at(&[]));
    }

    /// **回帰の要**: ペイン 1 枚のタイルは、分割機能を入れる前と
    /// 行数・桁数が 1 つも変わらない。
    ///
    /// ヘッダぶんを削ったり、ガターを差し引いたりした瞬間に
    /// 「Cockpit のミニターミナルが 1 行狭くなった」という形で表に出る。
    #[test]
    fn single_pane_tile_geometry_is_identical_to_undivided() {
        // 端数の出る不揃いなサイズで試す (割り切れる数だと事故を見逃す)。
        let cases: &[(f32, f32, f32, f32)] = &[
            (803.0, 457.0, 8.0, 17.0),
            (320.5, 199.5, 7.0, 15.0),
            (1201.0, 33.0, 9.0, 19.0),
        ];
        for (w, h, cw, ch) in cases {
            let a = rect(11.0, 23.0, *w, *h);
            // ヘッダは 1 枚のときに 1 px も取らない。
            assert_eq!(pane_body(a, false), a, "1 枚でヘッダを取った");

            let l = SplitLayout::single(7);
            let mut got: Vec<(SessionId, u16, u16)> = Vec::new();
            apply_sizes(&l, a, GUTTER, *cw, *ch, &mut |id, r, c| {
                got.push((id, r, c))
            });

            // `draw` が allow_resize でやっている計算そのもの。
            let cols = ((a.width() - TERM_PADDING * 2.0) / cw).floor().max(1.0) as u16;
            let rows = ((a.height() - TERM_PADDING * 2.0) / ch).floor().max(1.0) as u16;
            assert_eq!(got, vec![(7, rows, cols)], "{w}x{h} で桁数/行数が変わった");
        }
    }

    /// 端末セルが**物理ピクセルの整数**に着地する (文字のガタつき対策)。
    ///
    /// 100% 表示 (ppp 1.0、Windows に多い) と 125% / 150% / 200% で確かめる。
    /// ここが小数に戻ると `origin.x + col * cell_w` の丸めが桁ごとに揺れ、
    /// 桁間隔が 8/8/7/8 px になって「文字がガタガタ」に見える。
    #[test]
    fn cell_metrics_land_on_whole_device_pixels() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        for ppp in [1.0_f32, 1.25, 1.5, 2.0] {
            ctx.set_pixels_per_point(ppp);
            let _ = ctx.run(Default::default(), |_| {});
            for size in [8.0_f32, 10.5, 12.0, 13.3, 14.0] {
                let mut got = (egui::FontId::monospace(size), 0.0_f32, 0.0_f32);
                let _ = ctx.run(Default::default(), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        got = cell_metrics(ui, size);
                    });
                });
                let (font, cw, ch) = got;
                for (what, v) in [("フォント", font.size), ("幅", cw), ("高さ", ch)] {
                    let px = v * ppp;
                    assert!(
                        (px - px.round()).abs() < 1e-3,
                        "{what}が物理ピクセル整数でない: {v} @ppp {ppp} (= {px}px, size {size})"
                    );
                    assert!(px >= 1.0, "{what}が 0 px になった @ppp {ppp}");
                }
            }
        }
    }
}

/// リンク検出のテーブルテスト。
///
/// PTY は一切起動しない — 判定は純関数なので、実在確認は差し込み口
/// (`exists`) を偽装するだけで済む。実ファイルを使うテストだけ
/// `crate::test_util::unique_temp_dir` を通す (実 `~/.zaivern` に触れない)。
#[cfg(test)]
mod link_tests {
    use super::{
        cols_for_range, detect_links, detect_urls, exists_cached, resolve_link_path, row_text_cols,
        should_open_link, LinkKind, LinkMatch,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// `known` に載っているパスだけ「実在する」とみなして検出する。
    fn links(line: &str, known: &[&str]) -> Vec<LinkMatch> {
        let mut oracle = |p: &str| known.contains(&p);
        detect_links(line, &mut oracle)
    }

    /// ファイル系リンクだけを `(パス, 行, 桁, 画面上の文字列)` に落とす。
    fn files(line: &str, known: &[&str]) -> Vec<(String, Option<u32>, Option<u32>, String)> {
        links(line, known)
            .into_iter()
            .filter_map(|m| match m.kind {
                LinkKind::File { raw, line: l, col } => {
                    Some((raw, l, col, line[m.start..m.end].to_string()))
                }
                LinkKind::Url(_) => None,
            })
            .collect()
    }

    fn urls(line: &str) -> Vec<String> {
        detect_urls(line)
            .into_iter()
            .map(|m| match m.kind {
                LinkKind::Url(u) => u,
                LinkKind::File { .. } => unreachable!("detect_urls は URL しか返さない"),
            })
            .collect()
    }

    #[test]
    fn ファイルパスの表記ゆれを表で押さえる() {
        // (行, 実在するとみなすパス, 期待するパス, 行, 桁)
        let table: &[(&str, &str, &str, Option<u32>, Option<u32>)] = &[
            // 行 + 桁 (rustc / eslint / tsc)
            (
                "error at src/foo.rs:12:5",
                "src/foo.rs",
                "src/foo.rs",
                Some(12),
                Some(5),
            ),
            // 行だけ
            ("src/foo.rs:12", "src/foo.rs", "src/foo.rs", Some(12), None),
            // 明示的な相対
            ("./foo.rs:12", "./foo.rs", "./foo.rs", Some(12), None),
            // 絶対 (先頭の / も含めて 1 トークン)
            (
                "/abs/foo.rs:12",
                "/abs/foo.rs",
                "/abs/foo.rs",
                Some(12),
                None,
            ),
            // Windows のドライブ付き。`C:` を行番号と読み違えない
            (
                "C:\\path\\foo.rs:12",
                "C:\\path\\foo.rs",
                "C:\\path\\foo.rs",
                Some(12),
                None,
            ),
            // MSVC 形式
            ("foo.rs(12,5)", "foo.rs", "foo.rs", Some(12), Some(5)),
            ("foo.rs(12)", "foo.rs", "foo.rs", Some(12), None),
            // 行番号なし
            ("see src/foo.rs", "src/foo.rs", "src/foo.rs", None, None),
            // 括弧に包まれている
            (
                "(src/foo.rs:12)",
                "src/foo.rs",
                "src/foo.rs",
                Some(12),
                None,
            ),
            // 文末のピリオドを飲み込まない
            (
                "壊れたのは src/foo.rs:12。",
                "src/foo.rs",
                "src/foo.rs",
                Some(12),
                None,
            ),
            // 日本語ファイル名
            (
                "テスト/日本語ファイル.rs:3:1",
                "テスト/日本語ファイル.rs",
                "テスト/日本語ファイル.rs",
                Some(3),
                Some(1),
            ),
            // 引用符で囲われた空白入りのパス
            (
                "\"a b/c d.rs\":12",
                "a b/c d.rs",
                "a b/c d.rs",
                Some(12),
                None,
            ),
            // 引用符なしの空白入りパス (連結して実在するものだけ採る)
            ("in a b/c.rs:7", "a b/c.rs", "a b/c.rs", Some(7), None),
        ];
        for (line, known, path, ln, col) in table {
            let got = files(line, &[known]);
            assert_eq!(got.len(), 1, "{line:?} でリンクが 1 件にならない: {got:?}");
            assert_eq!(
                (got[0].0.as_str(), got[0].1, got[0].2),
                (*path, *ln, *col),
                "{line:?}"
            );
        }
    }

    #[test]
    fn 実在しないパスはリンクにしない() {
        for line in [
            "src/does_not_exist.rs:12:5",
            "foo.rs(12,5)",
            "/abs/nope.rs",
            "./nope.rs:1",
        ] {
            assert!(
                files(line, &[]).is_empty(),
                "{line:?} を実在確認なしでリンクにした"
            );
        }
    }

    #[test]
    fn パスらしくない語では実在確認すら撃たない() {
        // stat の回数がそのままアイドルコストになるので、足切りを固定する。
        let mut asked: Vec<String> = Vec::new();
        let mut oracle = |p: &str| {
            asked.push(p.to_string());
            false
        };
        detect_links("error: test 12 FAILED おわり", &mut oracle);
        assert!(asked.is_empty(), "パスらしくない語を stat した: {asked:?}");
    }

    #[test]
    fn 単体のurlをそのまま拾う() {
        assert_eq!(urls("https://example.com"), ["https://example.com"]);
        assert_eq!(urls("開く http://example.com/a"), ["http://example.com/a"]);
    }

    #[test]
    fn urlの末尾の句読点と閉じ括弧を含めない() {
        let table: &[(&str, &str)] = &[
            ("(https://example.com)", "https://example.com"),
            ("詳しくは https://example.com。", "https://example.com"),
            ("see https://example.com.", "https://example.com"),
            ("[https://example.com]", "https://example.com"),
            ("https://example.com,", "https://example.com"),
            ("<https://example.com>", "https://example.com"),
            // 釣り合っている括弧は URL の一部として残す
            ("https://example.com/a_(b)", "https://example.com/a_(b)"),
            ("(https://example.com/a_(b))", "https://example.com/a_(b)"),
        ];
        for (line, want) in table {
            assert_eq!(urls(line), [*want], "{line:?}");
        }
    }

    #[test]
    fn スキームだけの文字列と語の途中はurlにしない() {
        assert!(urls("https://").is_empty());
        assert!(urls("xhttps://example.com").is_empty());
        // 日本語の本文に直に埋め込まれた URL は拾う (境界扱い)
        assert_eq!(
            urls("説明はhttps://example.com です"),
            ["https://example.com"]
        );
    }

    #[test]
    fn 一行に複数のリンクがあっても全部拾う() {
        let line = "src/a.rs:1:2 と src/b.rs:3 と https://example.com";
        let got = links(line, &["src/a.rs", "src/b.rs"]);
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(&line[got[0].start..got[0].end], "src/a.rs:1:2");
        assert_eq!(&line[got[1].start..got[1].end], "src/b.rs:3");
        assert_eq!(&line[got[2].start..got[2].end], "https://example.com");
    }

    #[test]
    fn cjkを含む行でバイト境界を割らない() {
        // 範囲は必ず文字境界。割れていればこのスライスがパニックする。
        let line = "日本語のログ src/foo.rs:12:5 を確認 https://例え.jp/パス です";
        let got = links(line, &["src/foo.rs"]);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(&line[got[0].start..got[0].end], "src/foo.rs:12:5");
        assert_eq!(&line[got[1].start..got[1].end], "https://例え.jp/パス");
        for m in &got {
            assert!(line.is_char_boundary(m.start) && line.is_char_boundary(m.end));
        }
    }

    fn parser(rows: u16, cols: u16) -> vt100::Parser {
        vt100::Parser::new(rows, cols, 0)
    }

    #[test]
    fn ansi色付きの行でも検出できる() {
        let mut p = parser(1, 40);
        p.process(b"\x1b[31msrc/foo.rs:12:5\x1b[0m ok");
        let (text, _) = row_text_cols(p.screen(), 0);
        let got = files(&text, &["src/foo.rs"]);
        assert_eq!(got.len(), 1, "text={text:?}");
        assert_eq!((got[0].1, got[0].2), (Some(12), Some(5)));
    }

    #[test]
    fn 全角文字の手前にあるリンクの桁がずれない() {
        // 「日本語」= 6 桁ぶん。バイト位置をそのまま桁にすると 9 になってしまう。
        let mut p = parser(1, 40);
        p.process("日本語 src/foo.rs:12".as_bytes());
        let (text, map) = row_text_cols(p.screen(), 0);
        let got = links(&text, &["src/foo.rs"]);
        assert_eq!(got.len(), 1, "text={text:?}");
        let (c0, c1) = cols_for_range(&map, got[0].start, got[0].end, 40);
        assert_eq!(c0, 7, "全角 3 文字 + 空白 のあと = 7 桁目");
        assert_eq!(c1, 7 + "src/foo.rs:12".len() as u16);
    }

    #[test]
    fn 相対パスは端末の作業ディレクトリ基準で解決する() {
        let dir = crate::test_util::unique_temp_dir("zv-link", "cwd");
        let sub = dir.join("src");
        std::fs::create_dir_all(&sub).expect("mkdir");
        let file = sub.join("foo.rs");
        std::fs::write(&file, "fn main() {}").expect("write");

        // ワークスペースルートではなく cwd 基準であることの確認
        let mut oracle = |raw: &str| resolve_link_path(raw, &dir).is_file();
        let got = detect_links("error at src/foo.rs:12:5", &mut oracle);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(resolve_link_path("src/foo.rs", &dir), file);
        // 別の cwd から見れば同じ行でもリンクにならない
        let other = crate::test_util::unique_temp_dir("zv-link", "other");
        let mut oracle2 = |raw: &str| resolve_link_path(raw, &other).is_file();
        assert!(detect_links("error at src/foo.rs:12:5", &mut oracle2).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn windowsの絶対パスは両osで壊れない() {
        let cwd = std::env::temp_dir();
        let p = resolve_link_path("C:\\path\\foo.rs", &cwd);
        if cfg!(windows) {
            assert!(p.is_absolute(), "Windows では絶対パスのまま: {p:?}");
            assert_eq!(p, PathBuf::from("C:\\path\\foo.rs"));
        } else {
            // unix ではドライブ表記は絶対にならないので cwd 配下へ落ちる
            // (実在しない = リンクにならない)。パニックしないことが要件。
            assert!(p.starts_with(&cwd), "unix では cwd 配下: {p:?}");
            assert!(!p.is_file());
        }
    }

    #[test]
    fn 実在確認はttlの間キャッシュされる() {
        // 毎フレーム stat を撃たないための要。消したファイルでも TTL 内は
        // 覚えていた答えを返す = ファイルシステムを叩いていない証拠。
        let dir = crate::test_util::unique_temp_dir("zv-link", "cache");
        let file = dir.join("a.rs");
        std::fs::write(&file, "x").expect("write");
        let mut cache: HashMap<PathBuf, (bool, std::time::Instant)> = HashMap::new();
        assert!(exists_cached(&mut cache, &file));
        std::fs::remove_file(&file).expect("rm");
        assert!(
            exists_cached(&mut cache, &file),
            "TTL 内なのに stat し直している"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「素のクリックで開く」が選択操作を壊していないことの表。
    ///
    /// ここが本体。描画側 (`handle_links`) は egui から値を集めて
    /// この関数へ渡すだけなので、選択との衝突はこの表で固定できる。
    /// `slop` は egui の `max_click_dist` 既定値と同じ 6px を代表値に使う
    /// (実コードは egui の設定から読むので、値が変わっても追随する)。
    #[test]
    fn 素のクリックで開くが選択操作は壊さない() {
        const SLOP: f32 = 6.0;
        // (説明, hovering, dragged_px, click_count, modified, had_selection, 期待)
        let table: &[(&str, bool, f32, u8, bool, bool, bool)] = &[
            (
                "素のシングルクリックは開く",
                true,
                0.0,
                1,
                false,
                false,
                true,
            ),
            (
                "修飾キー付きも従来どおり開く",
                true,
                0.0,
                1,
                true,
                false,
                true,
            ),
            (
                "リンクの上でなければ開かない",
                false,
                0.0,
                1,
                true,
                false,
                false,
            ),
            (
                "クリックしていないフレームは開かない",
                true,
                0.0,
                0,
                false,
                false,
                false,
            ),
            (
                "ダブルクリック(語選択)では開かない",
                true,
                0.0,
                2,
                false,
                false,
                false,
            ),
            (
                "修飾キー付きダブルクリックでも開かない",
                true,
                0.0,
                2,
                true,
                false,
                false,
            ),
            (
                "トリプルクリック(行選択)では開かない",
                true,
                0.0,
                3,
                false,
                false,
                false,
            ),
            (
                "修飾キー付きトリプルクリックでも開かない",
                true,
                0.0,
                3,
                true,
                false,
                false,
            ),
            (
                "しきい値ちょうどの微動はクリック",
                true,
                SLOP,
                1,
                false,
                false,
                true,
            ),
            (
                "しきい値を超えたらドラッグ選択",
                true,
                SLOP + 0.001,
                1,
                false,
                false,
                false,
            ),
            (
                "大きく引きずったら開かない",
                true,
                400.0,
                1,
                false,
                false,
                false,
            ),
            (
                "引きずれば修飾キー付きでも開かない",
                true,
                400.0,
                1,
                true,
                false,
                false,
            ),
            (
                "選択が残っている素のクリックは解除の意図",
                true,
                0.0,
                1,
                false,
                true,
                false,
            ),
            (
                "選択があっても修飾キー付きなら開く",
                true,
                0.0,
                1,
                true,
                true,
                true,
            ),
            (
                "測れない距離(NaN)は開かない",
                true,
                f32::NAN,
                1,
                false,
                false,
                false,
            ),
            (
                "測れない距離(∞)は開かない",
                true,
                f32::INFINITY,
                1,
                false,
                false,
                false,
            ),
            (
                "負の距離はあり得ないので開かない",
                true,
                -1.0,
                1,
                false,
                false,
                false,
            ),
        ];
        for (why, hovering, dragged, count, modified, had_sel, want) in table {
            let got = should_open_link(*hovering, *dragged, SLOP, *count, *modified, *had_sel);
            assert_eq!(
                got, *want,
                "{why}: hovering={hovering} dragged={dragged} count={count} \
                 modified={modified} had_selection={had_sel}"
            );
        }
        // しきい値そのものが壊れた値でも「開かない」側へ倒れる
        for slop in [f32::NAN, f32::INFINITY] {
            assert!(
                !should_open_link(true, 0.0, slop, 1, false, false),
                "しきい値が {slop} のとき開いてしまった"
            );
        }
    }
}

#[cfg(test)]
mod typed_line_tests {
    use super::{feed_typed_line, TypedLine};

    fn feed(seq: &[&str]) -> Vec<String> {
        let mut st = TypedLine::default();
        let mut out = Vec::new();
        for s in seq {
            if let Some(l) = feed_typed_line(&mut st, s.as_bytes()) {
                out.push(l);
            }
        }
        out
    }

    #[test]
    fn 打鍵を組み立てて_enter_で確定する() {
        assert_eq!(feed(&["テ", "ス", "ト", "して", "\r"]), vec!["テストして"]);
    }

    #[test]
    fn 一括で来ても確定する() {
        assert_eq!(feed(&["テストしてください\r"]), vec!["テストしてください"]);
    }

    #[test]
    fn crlf_は一回の確定() {
        assert_eq!(feed(&["やって\r\n"]), vec!["やって"]);
    }

    #[test]
    fn バックスペースで消える() {
        assert_eq!(feed(&["abcd", "\u{7f}", "\u{7f}", "\r"]), vec!["ab"]);
    }

    #[test]
    fn ctrl_c_と_ctrl_u_で行を捨てる() {
        assert_eq!(feed(&["書きかけ", "\u{3}", "\r"]), Vec::<String>::new());
        assert_eq!(feed(&["書きかけ", "\u{15}", "\r"]), Vec::<String>::new());
    }

    /// 空 Enter (承認プロンプトへの Enter 等) は「指示」ではない。
    /// ここを覚えると、承認するたびにセッション名が壊れる。
    #[test]
    fn 空の_enter_は確定として扱わない() {
        assert_eq!(feed(&["\r", "\r", "  ", "\r"]), Vec::<String>::new());
    }

    /// 矢印キーなどが混ざった行は本文を再現できないので覚えない。
    /// 間違った本文で命名するより、命名しない方が良い。
    #[test]
    fn エスケープ列が混ざった行は捨てる() {
        assert_eq!(feed(&["abc", "\u{1b}[D", "x\r"]), Vec::<String>::new());
    }

    /// **エスケープ列が別の write で来ても行を捨てる。**
    /// 打鍵は 1 回の write に収まるとは限らないので、印を確定まで持ち越す。
    /// ここが持続しないと「abc←def」を「def」として覚えてしまう。
    #[test]
    fn エスケープ列の直後に打ち直しても行は捨てる() {
        assert_eq!(
            feed(&["abc", "\u{1b}[D", "でふぉると\r"]),
            Vec::<String>::new()
        );
    }

    /// 捨てた印は次の行へ持ち越さない (1 行捨てたら以後ずっと無効、を防ぐ)。
    #[test]
    fn 捨てた印は次の行へ持ち越さない() {
        assert_eq!(
            feed(&["abc", "\u{1b}[D", "x\r", "つぎの指示\r"]),
            vec!["つぎの指示"]
        );
    }

    #[test]
    fn bracketed_paste_の囲みは剥がす() {
        assert_eq!(
            feed(&["\u{1b}[200~一行目 二行目\u{1b}[201~", "\r"]),
            vec!["一行目 二行目"]
        );
    }

    /// 1 回の呼び出しに複数行が入っていたら、直近の行だけを返す。
    #[test]
    fn 複数行が来たら最後の行を返す() {
        assert_eq!(feed(&["ひとつめ\r ふたつめ\r"]), vec![" ふたつめ"]);
    }

    /// 暴走した貼り付けで無制限に伸びない。
    #[test]
    fn 長すぎる本文は頭打ちにする() {
        let mut st = TypedLine::default();
        let big = "あ".repeat(10_000);
        let got = feed_typed_line(&mut st, format!("{big}\r").as_bytes()).expect("確定する");
        assert_eq!(got.chars().count(), super::PROMPT_KEEP_CHARS);
    }

    /// 壊れた UTF-8 が来ても panic しない (PTY 由来のバイト列は信用できない)。
    #[test]
    fn 壊れた_utf8_でも落ちない() {
        let mut st = TypedLine::default();
        let _ = feed_typed_line(&mut st, &[0xff, 0xfe, b'a', b'b', b'\r']);
    }

    // ───────── チャンク境界の番人 ─────────
    //
    // 打鍵・貼り付けは 1 回の write に収まるとは限らない。日本語 1 文字は
    // 3 バイト、絵文字は 4 バイトなので、境界がその途中に落ちるのは普通に起きる。
    // ここが lossy だと `U+FFFD` が本文へ焼き付き、**次のバイトが来ても直らない**。

    /// **修正前はここで `Some("\u{fffd}\u{fffd}\u{fffd}本語のしじ")` が返っていた。**
    /// 総当たり: どこで 2 分割しても、一括で流したのと同じ 1 行になる。
    #[test]
    fn 打鍵がどこで割れても同じ本文になる() {
        let src = "日本語 🎉 café ＡＢ の指示";
        let bytes = src.as_bytes();
        for cut in 0..=bytes.len() {
            let mut st = TypedLine::default();
            let _ = feed_typed_line(&mut st, &bytes[..cut]);
            let got = feed_typed_line(&mut st, &[&bytes[cut..], b"\r"].concat());
            assert_eq!(got.as_deref(), Some(src), "cut={cut}");
        }
    }

    /// 1 バイトずつ届いても同じ (最悪の割れ方)。
    #[test]
    fn 打鍵が一バイトずつ届いても同じ本文になる() {
        let src = "日本語 🎉 の指示";
        let mut st = TypedLine::default();
        for b in src.as_bytes() {
            assert_eq!(feed_typed_line(&mut st, &[*b]), None);
        }
        assert_eq!(feed_typed_line(&mut st, b"\r").as_deref(), Some(src));
    }

    /// 覚えた本文に置換文字が混ざらない (混ざると自動命名も引き継ぎも化ける)。
    #[test]
    fn 覚えた本文に置換文字が残らない() {
        let src = "絵文字🎉と日本語";
        let bytes = src.as_bytes();
        for cut in 1..bytes.len() {
            let mut st = TypedLine::default();
            let _ = feed_typed_line(&mut st, &bytes[..cut]);
            let got =
                feed_typed_line(&mut st, &[&bytes[cut..], b"\r"].concat()).unwrap_or_default();
            assert!(!got.contains('\u{fffd}'), "cut={cut} got={got:?}");
        }
    }
}

/// **画面 (vt100) 側のチャンク境界の番人。**
///
/// PTY 読み取りスレッドは生バイト列をそのまま `Parser::process` へ流す。
/// vte の UTF-8 状態機械は呼び出しをまたいで途中の列を覚えるので、
/// ここは元から割れない — が、「割れない」は**検証されていて初めて事実**であり、
/// 途中に `from_utf8_lossy` を挟む改変が入った瞬間に壊れる。
/// その改変を止めるのがこのテスト。
#[cfg(test)]
mod screen_boundary_tests {
    /// 3 バイト・4 バイト・結合文字・全角をわざと混ぜる。
    /// ASCII だけでは境界問題は再現しない。
    const SAMPLE: &str = "日本語 🎉 café ＡＢ か\u{3099} 👨\u{200d}👩\u{200d}👧\u{200d}👦";

    fn screen_of(chunks: &[&[u8]]) -> String {
        let mut p = vt100::Parser::new(10, 200, 100);
        for c in chunks {
            p.process(c);
        }
        p.screen().contents()
    }

    /// 総当たり: どこで 2 分割しても画面が一致し、置換文字が出ない。
    #[test]
    fn 画面はどこで割っても同じになる() {
        let bytes = SAMPLE.as_bytes();
        let whole = screen_of(&[bytes]);
        assert!(!whole.contains('\u{fffd}'), "素材からして化けている");
        for cut in 0..=bytes.len() {
            let split = screen_of(&[&bytes[..cut], &bytes[cut..]]);
            assert_eq!(split, whole, "cut={cut} で画面が変わった");
        }
    }

    /// 1 バイトずつ流しても同じ画面になる (PTY が最悪の切り方をした場合)。
    #[test]
    fn 画面は一バイトずつ流しても同じになる() {
        let bytes = SAMPLE.as_bytes();
        let whole = screen_of(&[bytes]);
        let one: Vec<&[u8]> = bytes.iter().map(std::slice::from_ref).collect();
        assert_eq!(screen_of(&one), whole);
    }
}

/// **端末の文字化けの番人 (画面まで見る)。**
///
/// `textenc::TermDecoder` の単体テストは「バイト列が UTF-8 になる」までしか
/// 見ない。ユーザーが見るのは vt100 の画面なので、**実 `vt100::Parser` に
/// 流して画面の文字列**で確かめる。修正前はここが `$3$s$K$A$OF|K\8l` だった。
#[cfg(test)]
mod term_encoding_screen_tests {
    use crate::textenc::{TermDecoder, TermEncoding};

    /// ISO-2022-JP の「こんにちは日本語」。`ESC $ B` で JIS X 0208、
    /// `ESC ( B` で ASCII へ。**実測で `$3$s$K$A$OF|K\8l` になっていた入力そのもの**
    /// (ひらがなの区の先行バイトが 0x24 = `$` なので `$` が並ぶ)。
    const ISO2022JP_JA: &[u8] = b"\x1b$B$3$s$K$A$OF|K\\8l\x1b(B";
    /// CP932 (Shift_JIS) の「日本語」。
    const CP932_JA: &[u8] = &[0x93, 0xfa, 0x96, 0x7b, 0x8c, 0xea];
    /// EUC-JP の「日本語」。
    const EUCJP_JA: &[u8] = &[0xc6, 0xfc, 0xcb, 0xdc, 0xb8, 0xec];

    /// 読取スレッドと**同じ経路**で画面を作る (正規化 → `process`)。
    fn screen_of(enc: TermEncoding, chunks: &[&[u8]]) -> String {
        let mut p = vt100::Parser::new(10, 40, 100);
        let mut d = TermDecoder::new(enc);
        for c in chunks {
            p.process(d.feed(c));
        }
        p.process(d.finish());
        p.screen().contents()
    }

    /// 正規化を通さない素の画面 (「直った」ことを比較で示すため)。
    fn raw_screen_of(chunks: &[&[u8]]) -> String {
        let mut p = vt100::Parser::new(10, 40, 100);
        for c in chunks {
            p.process(c);
        }
        p.screen().contents()
    }

    /// **これが報告された不具合そのもの。**
    #[test]
    fn iso2022jpの日本語が画面で日本語になる() {
        // 直す前の姿を先に固定しておく (直った証拠になる)。
        assert_eq!(raw_screen_of(&[ISO2022JP_JA]), "$3$s$K$A$OF|K\\8l");
        assert_eq!(
            screen_of(TermEncoding::Auto, &[ISO2022JP_JA]),
            "こんにちは日本語"
        );
    }

    /// どこで割っても同じ画面 (2 分割の総当たり)。
    #[test]
    fn iso2022jpは画面もどこで割っても同じになる() {
        let whole = screen_of(TermEncoding::Auto, &[ISO2022JP_JA]);
        assert_eq!(whole, "こんにちは日本語");
        for cut in 0..=ISO2022JP_JA.len() {
            let split = screen_of(
                TermEncoding::Auto,
                &[&ISO2022JP_JA[..cut], &ISO2022JP_JA[cut..]],
            );
            assert_eq!(split, whole, "cut={cut} で画面が変わった");
        }
        let one: Vec<&[u8]> = ISO2022JP_JA.iter().map(std::slice::from_ref).collect();
        assert_eq!(screen_of(TermEncoding::Auto, &one), whole, "1 バイトずつ");
    }

    /// 明示したコードページでも画面が正しい文字になる。
    /// `encoding_rs` の表を使うので **OS を問わず**同じ結果になる。
    #[test]
    fn 明示したコードページでも画面が日本語になる() {
        assert_eq!(
            screen_of(TermEncoding::CodePage(932), &[CP932_JA]),
            "日本語"
        );
        assert_eq!(
            screen_of(TermEncoding::CodePage(51932), &[EUCJP_JA]),
            "日本語"
        );
        for (cp, src) in [(932u32, CP932_JA), (51932, EUCJP_JA)] {
            for cut in 0..=src.len() {
                assert_eq!(
                    screen_of(TermEncoding::CodePage(cp), &[&src[..cut], &src[cut..]]),
                    "日本語",
                    "cp={cp} cut={cut}"
                );
            }
        }
    }

    /// **UTF-8 の画面は 1 ドットも変わらない。** 既存の境界テストと同じ素材で、
    /// 正規化を挟んだ画面と挟まない画面が一致することを見る。
    #[test]
    fn utf8の画面は正規化を挟んでも変わらない() {
        let sample = "日本語 🎉 café ＡＢ か\u{3099} \x1b[1;32m緑\x1b[0m ok";
        let bytes = sample.as_bytes();
        let raw = raw_screen_of(&[bytes]);
        assert!(!raw.contains('\u{fffd}'), "素材からして化けている");
        assert_eq!(screen_of(TermEncoding::Auto, &[bytes]), raw);
        for cut in 0..=bytes.len() {
            assert_eq!(
                screen_of(TermEncoding::Auto, &[&bytes[..cut], &bytes[cut..]]),
                raw,
                "cut={cut}"
            );
        }
    }

    /// バイナリを `cat` した画面も変わらない (素通しなので当然、が番人)。
    #[test]
    fn バイナリを流した画面は正規化を挟んでも変わらない() {
        let mut bin: Vec<u8> = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut x: u32 = 20260812;
        for _ in 0..2000 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            bin.push((x >> 16) as u8);
        }
        assert_eq!(
            screen_of(TermEncoding::Auto, &[&bin]),
            raw_screen_of(&[&bin])
        );
    }

    /// TUI が使う列 (DEC 罫線 / 代替画面 / OSC タイトル) を壊さない。
    #[test]
    fn tuiの制御列を壊さない() {
        let src: &[u8] = b"\x1b]0;title\x07\x1b[?1049h\x1b(0qqqj\x1b(B\x1b[31mred\x1b[0m";
        assert_eq!(
            screen_of(TermEncoding::Auto, &[src]),
            raw_screen_of(&[src]),
            "正規化で画面が変わった"
        );
    }

    /// 環境変数の入口が生きている (読めない名前は既定へ落ちる)。
    #[test]
    fn 端末の符号化の名前を解釈できる() {
        use crate::textenc::term_encoding_by_name as by;
        assert_eq!(by("auto"), Some(TermEncoding::Auto));
        assert_eq!(by("cp932"), Some(TermEncoding::CodePage(932)));
        assert_eq!(by("でたらめ"), None);
    }
}

// ─── シェル統合: 通し番号と描画のテスト ────────────────────────────

#[cfg(test)]
mod shell_line_tests {
    use super::{
        count_around, scrolled_delta, shell_mark_color, shell_mark_rect, shell_marker_cuts,
        shell_segments, shell_sticky_rect, LineIndex, ScrollProbe, FEED_CHUNK, SCROLLBACK_ROWS,
    };
    use crate::shellint::{Marker, Tracker};

    fn probe(sb_len: usize, offset: usize) -> ScrollProbe {
        ScrollProbe {
            sb_len,
            offset,
            alt: false,
        }
    }

    #[test]
    fn feed_chunk_fits_in_scrollback() {
        // 1 バイトが最大 1 行を押し出すので、刻み幅が容量未満なら
        // 「留めた 1 行 + 刻み幅」が容量を超えない = 必ず数え切れる。
        assert!(
            FEED_CHUNK + 1 <= SCROLLBACK_ROWS,
            "刻み幅が容量を超えている"
        );
    }

    #[test]
    fn scrolled_delta_reads_length_growth_and_offset_growth() {
        // 履歴がまだ空 → 戻り量は動かない。長さの伸びだけが答え。
        assert_eq!(scrolled_delta(probe(0, 0), probe(7, 0)), 7);
        // 容量に達していない間は両方が同じ値を出す。
        assert_eq!(scrolled_delta(probe(10, 1), probe(14, 5)), 4);
        // 満杯 (長さが動かない) → 戻り量だけが効く。**ここが本番。**
        assert_eq!(scrolled_delta(probe(5000, 1), probe(5000, 41)), 40);
        // 何も起きていない。
        assert_eq!(scrolled_delta(probe(5000, 1), probe(5000, 1)), 0);
        // 逆行 (履歴が縮む) は 0 に留める。負の通し番号は作らない。
        assert_eq!(scrolled_delta(probe(30, 4), probe(10, 1)), 0);
    }

    #[test]
    fn scrolled_delta_never_counts_across_the_alternate_screen() {
        // 代替画面のグリッドは履歴を持たない (容量 0)。抜けた瞬間に
        // 通常画面の履歴長と引き算すると「一気に 5000 行流れた」と出る。
        let alt = ScrollProbe {
            sb_len: 0,
            offset: 0,
            alt: true,
        };
        assert_eq!(scrolled_delta(alt, probe(5000, 0)), 0);
        assert_eq!(scrolled_delta(probe(5000, 1), alt), 0);
    }

    /// 実際の vt100 を回して「押し出した行数」が合うことを見る。
    /// **容量より多く流す**ので、飽和したあとも数えられていることの証明になる。
    fn count_lines(cap: usize, rows: u16, total: usize, per_call: usize) -> u64 {
        let mut p = vt100::Parser::new(rows, 20, cap);
        let idx = LineIndex::default();
        let mut bytes = Vec::new();
        for i in 0..total {
            bytes.extend_from_slice(format!("line{i}\r\n").as_bytes());
        }
        for piece in bytes.chunks(per_call) {
            let (_, moved, sb_len) = count_around(&mut p, |p| p.process(piece));
            idx.advance(moved, sb_len);
        }
        idx.scrolled()
    }

    #[test]
    fn scrolled_count_survives_a_full_scrollback() {
        // 画面 5 行なので、k 行送ると押し出されるのは k - (5 - 1)。
        // 容量 8 = 100 行流せば必ず飽和する。飽和しても数え続けること。
        assert_eq!(count_lines(8, 5, 100, 16), 96);
        // 刻みを変えても同じ答え (刻み幅 < 容量 である限り)。
        assert_eq!(count_lines(8, 5, 100, 7), 96);
        // 容量に一度も届かない場合。
        assert_eq!(count_lines(5000, 5, 100, 64), 96);
    }

    #[test]
    fn scrolled_count_restores_the_users_scroll_position() {
        let mut p = vt100::Parser::new(5, 20, 100);
        for i in 0..40 {
            let _ = count_around(&mut p, |p| p.process(format!("l{i}\r\n").as_bytes()));
        }
        // 最新に貼り付いたまま = 0 のまま。
        assert_eq!(p.screen().scrollback(), 0);
        // 履歴を戻して見ている間は、同じ文字が同じ位置に見えるよう
        // vt100 自身が戻り量を増やす。測定がその挙動を壊さないこと。
        p.set_scrollback(10);
        let before = p.screen().contents();
        let (_, moved, _) = count_around(&mut p, |p| p.process(b"x\r\ny\r\n"));
        assert_eq!(moved, 2);
        assert_eq!(p.screen().scrollback(), 12);
        assert_eq!(p.screen().contents(), before, "見ている画面が流れた");
    }

    #[test]
    fn marker_cuts_split_right_after_the_terminator() {
        // マーカーが 1 つも無ければ**切らない** (= 従来と同じ 1 本)。
        let plain = b"hello\r\nworld\r\n";
        assert!(shell_marker_cuts(plain).is_empty());
        assert_eq!(shell_segments(plain), vec![&plain[..]]);
        // BEL 終端。
        let bel = b"a\x1b]133;C\x07out";
        assert_eq!(shell_marker_cuts(bel), vec![9]);
        assert_eq!(shell_segments(bel), vec![&bel[..9], &bel[9..]]);
        // ST 終端 + OSC 633。
        let st = b"\x1b]633;E;ls\x1b\\rest";
        assert_eq!(shell_marker_cuts(st), vec![12]);
        // 2 つ並んでいれば 2 回切る。
        let two = b"\x1b]133;D;0\x07\x1b]133;A\x07$ ";
        assert_eq!(shell_marker_cuts(two), vec![10, 18]);
        // 閉じていないものは切らない (次の read で拾い直す)。
        assert!(shell_marker_cuts(b"x\x1b]133;A").is_empty());
        // 関係の無い OSC (52 / 10) では切らない。
        assert!(shell_marker_cuts(b"\x1b]52;c;YQ==\x07").is_empty());
    }

    #[test]
    fn sticky_band_hides_itself_when_the_terminal_is_too_short() {
        let r =
            |w: f32, h: f32| egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(w, h));
        // 本文が 2 行ぶん取れない = 帯で画面を埋めてしまう → 出さない。
        assert!(shell_sticky_rect(r(400.0, 24.0), 6.0, 17.0).is_none());
        assert!(shell_sticky_rect(r(4.0, 400.0), 6.0, 17.0).is_none());
        let band = shell_sticky_rect(r(400.0, 400.0), 6.0, 17.0).expect("出るはず");
        assert_eq!(band.height(), 17.0);
        assert!(
            r(400.0, 400.0).contains_rect(band),
            "帯が領域からはみ出した"
        );
    }

    #[test]
    fn marks_stay_inside_the_left_padding_and_never_overlap() {
        // 極端な大きさでも (1) 領域内に収まる (2) 互いに重ならない
        // (3) 本文の 1 桁目 (rect.min.x + padding) へ食い込まない。
        for (w, h) in [(900.0_f32, 700.0_f32), (1200.0, 300.0), (140.0, 60.0)] {
            let rect = egui::Rect::from_min_size(egui::pos2(3.0, 7.0), egui::vec2(w, h));
            let (padding, cell_h) = (6.0_f32, 17.0_f32);
            let mut prev: Option<egui::Rect> = None;
            for row in 0..40u16 {
                let Some(m) = shell_mark_rect(rect, padding, cell_h, row) else {
                    continue;
                };
                assert!(rect.contains_rect(m), "{w}x{h} row{row}: 領域外");
                assert!(
                    m.max.x <= rect.min.x + padding,
                    "{w}x{h} row{row}: 本文へ食い込んだ"
                );
                if let Some(q) = prev {
                    assert!(q.max.y <= m.min.y, "{w}x{h} row{row}: 印が重なった");
                }
                prev = Some(m);
            }
        }
        // 帯と印は縦に重なるが、横は決して重ならない (帯は本文側にある)。
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 700.0));
        let band = shell_sticky_rect(rect, 6.0, 17.0).expect("出るはず");
        let mark = shell_mark_rect(rect, 6.0, 17.0, 0).expect("出るはず");
        assert!(mark.max.x <= band.min.x, "印と帯が重なった");
    }

    #[test]
    fn an_unknown_exit_code_gets_no_mark_at_all() {
        let th = crate::theme::by_name("dark");
        // 終了コード不明 = 印を付けない (誤った印を出さない)。
        assert!(shell_mark_color(&th, None, false).is_none());
        assert!(shell_mark_color(&th, Some(true), false).is_some());
        assert!(shell_mark_color(&th, Some(false), false).is_some());
        // 実行中は「まだ分からない」を色で出す (成功でも失敗でもない)。
        assert!(shell_mark_color(&th, None, true).is_some());
    }

    /// OSC 133 を出さないシェル = マーカーが 1 つも来ない。
    #[test]
    fn without_shell_integration_there_is_nothing_to_draw() {
        let t = Tracker::new();
        assert!(t.blocks().is_empty());
        assert!(t.running_block().is_none());
        // 印も帯もジャンプ先も、どの行を聞いても出てこない。
        for line in [0u64, 1, 7, 4096, u64::MAX / 2] {
            assert!(t.block_at(line).is_none(), "line {line} に印が出た");
            assert!(t.prev_prompt(line).is_none(), "line {line} で上へ跳べた");
            assert!(t.next_prompt(line).is_none(), "line {line} で下へ跳べた");
        }
    }

    /// 行番号を渡していない経路 (行を数えられない呼び出し側) でも、
    /// 位置を捏造しない = 印も帯も出ない。
    #[test]
    fn markers_without_line_numbers_never_produce_a_position() {
        let mut t = Tracker::new();
        t.feed_at(Marker::PromptStart, 0);
        t.feed_at(Marker::PromptEnd, 1);
        t.feed_at(Marker::PreExec, 2);
        t.feed_at(Marker::Finished(Some(0)), 3);
        assert_eq!(t.blocks().len(), 1, "コマンド自体は記録される");
        assert!(t.blocks()[0].lines.is_none(), "位置を捏造した");
        assert!(t.block_at(0).is_none());
        assert!(t.prev_prompt(u64::MAX).is_none());
    }

    /// ジャンプ先の決定: 画面最上段を基準に、前/次のプロンプトへ 1 つずつ。
    #[test]
    fn prompt_jump_targets_are_the_neighbouring_prompts() {
        let mut t = Tracker::new();
        // 3 つのコマンドを 10 / 30 / 50 行目のプロンプトで置く。
        for (i, at) in [10u64, 30, 50].into_iter().enumerate() {
            t.feed_at_line(Marker::PromptStart, i as u64 * 10, Some(at));
            t.feed_at_line(Marker::PromptEnd, i as u64 * 10, Some(at));
            t.feed_at_line(Marker::PreExec, i as u64 * 10, Some(at + 1));
            t.feed_at_line(Marker::Finished(Some(0)), i as u64 * 10, Some(at + 9));
        }
        assert_eq!(t.prev_prompt(50), Some(30));
        assert_eq!(t.prev_prompt(30), Some(10));
        assert_eq!(t.prev_prompt(10), None, "いちばん上より先は無い");
        assert_eq!(t.next_prompt(10), Some(30));
        assert_eq!(t.next_prompt(30), Some(50));
        assert_eq!(t.next_prompt(50), None, "いちばん下より先は無い");
        // 出力の途中 (35 行目) からでも上下の境界へ跳べる。
        assert_eq!(t.prev_prompt(35), Some(30));
        assert_eq!(t.next_prompt(35), Some(50));
        // sticky の中身: その行を含むブロックが引ける。
        assert_eq!(
            t.block_at(35).and_then(|b| b.lines).map(|l| l.prompt),
            Some(30)
        );
        // どのブロックにも属さない行では帯を出さない。
        assert!(t.block_at(0).is_none());
    }

    /// 履歴から落ちた行のブロックを忘れること (窓と整合させる)。
    #[test]
    fn blocks_older_than_the_live_window_are_forgotten() {
        let mut t = Tracker::new();
        for i in 0..5u64 {
            let at = i * 10;
            t.feed_at_line(Marker::PromptStart, at, Some(at));
            t.feed_at_line(Marker::PromptEnd, at, Some(at));
            t.feed_at_line(Marker::PreExec, at, Some(at + 1));
            t.feed_at_line(Marker::Finished(Some(0)), at, Some(at + 5));
        }
        assert_eq!(t.blocks().len(), 5);
        // 25 行目より古い行はもう存在しない → 0/10 のブロックを忘れる。
        assert_eq!(t.forget_before(25), 2);
        assert_eq!(t.blocks().len(), 3);
        assert_eq!(t.oldest_indexed_line(), Some(20));
        assert!(t.gap_note().is_some(), "捨てたことを黙っている");
    }
    /// **読取スレッドと同じ手順**でバイト列を通し、記録された行が本当の
    /// 画面行と合うことを見る。単体の純関数が全部緑でも、繋ぎ方を間違えると
    /// ここだけが落ちる (CLAUDE.md「実バイナリを回さないと分からない回帰」)。
    fn run_pipeline(rows: u16, cap: usize, chunks: &[&[u8]]) -> (Tracker, u64) {
        let mut p = vt100::Parser::new(rows, 40, cap);
        let mut scanner = super::QueryScanner::default();
        let idx = LineIndex::default();
        let mut t = Tracker::new();
        let mut clock = 0u64;
        for chunk in chunks {
            for seg in shell_segments(chunk) {
                let mut scrolled = idx.scrolled();
                for piece in seg.chunks(FEED_CHUNK) {
                    let (_, moved, sb_len) = count_around(&mut p, |p| p.process(piece));
                    scrolled = idx.advance(moved, sb_len);
                }
                let line = (!p.screen().alternate_screen())
                    .then(|| scrolled + u64::from(p.screen().cursor_position().0));
                for ev in scanner.scan(seg) {
                    if let super::TermEvent::Shell(m) = ev {
                        clock += 1;
                        t.feed_at_line(m, clock, line);
                    }
                }
            }
            if idx.scrolled() > 0 {
                t.forget_before(idx.oldest_live());
            }
        }
        (t, idx.scrolled())
    }

    #[test]
    fn osc133_markers_land_on_the_rows_they_were_printed_on() {
        // 画面 10 行。プロンプト → コマンド → 出力 3 行 → 終了。
        let (t, _) = run_pipeline(
            10,
            5000,
            &[b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07a\r\nb\r\nc\r\n\x1b]133;D;0\x07"],
        );
        assert_eq!(t.blocks().len(), 1);
        let l = t.blocks()[0].lines.expect("行が付いていない");
        assert_eq!(l.prompt, 0, "プロンプトは 0 行目");
        assert_eq!(l.input, Some(0), "1 行プロンプトなので入力も 0 行目");
        assert_eq!(l.output_start, Some(1), "出力は 1 行目から");
        assert_eq!(l.end, Some(4), "a/b/c を出して 4 行目で終わる");
        assert_eq!(t.blocks()[0].ok(), Some(true));
        // sticky: 出力の途中はこのブロックのもの。
        assert_eq!(
            t.block_at(2).and_then(|b| b.lines).map(|l| l.prompt),
            Some(0)
        );
        // OSC 133 だけの段ではコマンド行が来ない (来ないものを捏造しない)。
        assert_eq!(t.blocks()[0].cmd.command_line, "");
        // プロンプトより前 (存在しない行) では帯を出さない。
        assert!(t.block_at(9).is_none());
    }

    #[test]
    fn a_huge_output_in_one_read_does_not_drag_the_start_marker_to_its_end() {
        // `C` と 300 行の出力が **1 回の read** で届く形。区切らずに流すと
        // 「出力の先頭」が出力の**末尾**として記録される (最も痛い誤り)。
        let mut chunk = b"\x1b]133;A\x07$ \x1b]133;B\x07seq\r\n\x1b]133;C\x07".to_vec();
        for i in 0..300 {
            chunk.extend_from_slice(format!("{i}\r\n").as_bytes());
        }
        chunk.extend_from_slice(b"\x1b]133;D;0\x07");
        let (t, scrolled) = run_pipeline(10, 5000, &[&chunk]);
        let l = t.blocks()[0].lines.expect("行が付いていない");
        assert_eq!(l.prompt, 0);
        assert_eq!(l.output_start, Some(1), "出力の先頭が末尾へずれた");
        assert_eq!(l.end, Some(301), "出力 300 行 + コマンド行");
        // 画面 10 行なので 301 - 9 行が押し出されている。
        assert_eq!(scrolled, 292);
        // 出力の途中を指しても、ちゃんとこのコマンドが引ける。
        assert_eq!(
            t.block_at(150).and_then(|b| b.lines).map(|l| l.prompt),
            Some(0)
        );
    }

    #[test]
    fn two_line_prompts_keep_the_prompt_row_and_the_input_row_apart() {
        // powerlevel10k のような 2 行プロンプト: A のあと改行してから B。
        let (t, _) = run_pipeline(
            10,
            5000,
            &[b"\x1b]133;A\x07top\r\n\xe2\x9d\xaf \x1b]133;B\x07id\r\n\x1b]133;C\x07u\r\n\x1b]133;D;1\x07"],
        );
        let l = t.blocks()[0].lines.expect("行が付いていない");
        assert_eq!(l.prompt, 0, "A はプロンプトの 1 行目");
        assert_eq!(l.input, Some(1), "B は 2 行目");
        assert_eq!(l.output_start, Some(2));
        assert_eq!(t.blocks()[0].ok(), Some(false), "終了コード 1 は失敗");
        // ジャンプの着地点はプロンプトの先頭 (A) であって B ではない。
        assert_eq!(t.prev_prompt(5), Some(0));
    }

    #[test]
    fn markers_split_across_two_reads_are_still_recorded() {
        // OSC が read の境界で割れる形。区切りは見つけられないが、
        // 走査側の pending が拾うので**記録は落ちない**。
        let (t, _) = run_pipeline(
            10,
            5000,
            &[
                b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]13",
                b"3;C\x07x\r\n\x1b]133;D;0\x07",
            ],
        );
        assert_eq!(t.blocks().len(), 1);
        let l = t.blocks()[0].lines.expect("行が付いていない");
        assert_eq!(l.prompt, 0);
        assert_eq!(l.end, Some(2));
    }

    #[test]
    fn the_line_index_keeps_working_after_the_scrollback_is_full() {
        // 容量 20 行に対し 200 行のコマンドを 40 本。飽和後も
        // 行が進み続け、落ちた行のブロックは忘れられていること。
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        for i in 0..40 {
            let mut c = b"\x1b]133;A\x07$ \x1b]133;B\x07".to_vec();
            c.extend_from_slice(format!("\x1b]633;E;cmd{i}\x07cmd{i}\r\n").as_bytes());
            c.extend_from_slice(b"\x1b]133;C\x07");
            for k in 0..5 {
                c.extend_from_slice(format!("out{i}-{k}\r\n").as_bytes());
            }
            c.extend_from_slice(b"\x1b]133;D;0\x07");
            chunks.push(c);
        }
        let refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();
        let (t, scrolled) = run_pipeline(10, 20, &refs);
        // 40 本 × 6 行 = 240 行。画面 10 行ぶんが残る。
        assert_eq!(scrolled, 240 - 9);
        // 直近のブロックは行を持ち、いちばん新しいプロンプトが引ける。
        let last = t.blocks().last().expect("空になった");
        let l = last.lines.expect("飽和後に行が消えた");
        assert_eq!(last.cmd.command_line, "cmd39");
        assert_eq!(l.prompt, 234, "飽和後も通し番号が進んでいない");
        // 履歴から落ちた行のブロックは忘れている。**残っているものは
        // すべて、まだ生きている窓に末尾が掛かっている**のが不変条件。
        let oldest_live = scrolled - 20; // 履歴の容量ぶんだけ遡れる
        for b in t.blocks() {
            let l = b.lines.expect("索引から落ちた");
            assert!(
                l.end.is_some_and(|e| e >= oldest_live),
                "もう画面に無い行を指すブロックが残っている: {l:?}"
            );
        }
        assert!(
            t.blocks().len() < 10,
            "忘れていない ({} 件)",
            t.blocks().len()
        );
        assert!(t.gap_note().is_some(), "捨てたことを黙っている");
    }
}

// ─── 選択と「出力で流れた画面」のずれ ──────────────────────────────

#[cfg(test)]
mod selection_scroll_tests {
    use super::{normalize_sel, selection_text, Session, SpawnSpec};
    use crate::lockx::lock_ok;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// 子が 1 バイトも書かないセッション。読取スレッドが畳まれるまで待つので、
    /// 以降パーサを触るのはテストだけになる (起動シェルの profile 出力が
    /// 混ざると入力が非決定になる)。
    fn quiet_session(id: u64, rows: u16, cols: u16, scrollback: usize) -> Session {
        let spec = SpawnSpec {
            title: "sel".into(),
            command: "exit 0".into(),
            cwd: std::env::current_dir().unwrap(),
            env: std::collections::HashMap::new(),
            preset_name: String::new(),
            icon: "💬".into(),
            log_path: None,
        };
        let mut s = Session::spawn(id, spec, eframe::egui::Context::default()).unwrap();
        // 読取スレッドはパーサの Arc を 1 本持つ。畳まれたら 1 本に戻る。
        let deadline = Instant::now() + Duration::from_secs(20);
        while Arc::strong_count(&s.parser) > 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            Arc::strong_count(&s.parser),
            1,
            "読取スレッドが畳まれている"
        );
        *lock_ok(&s.parser) = vt100::Parser::new(rows, cols, scrollback);
        s.size = (rows, cols);
        s.scroll = 0;
        s.lines.scrolled.store(0, Ordering::Relaxed);
        s.lines.sb_len.store(0, Ordering::Relaxed);
        s.sel_pushed = 0;
        s.clear_selection();
        s
    }

    /// 行を n 本流す。
    fn feed_lines(s: &Session, range: std::ops::Range<usize>) {
        for i in range {
            s.feed(format!("line{:03}\r\n", i).as_bytes());
        }
    }

    /// 画面に出ているハイライトの下に**実際にある**文字
    /// (`draw_screen` はパーサの今の窓から描くので、これが利用者の見るもの)。
    fn under_highlight(s: &Session) -> String {
        let sel = s.selection.expect("選択が画面に見えている");
        let p = lock_ok(&s.parser);
        selection_text(p.screen(), normalize_sel(sel))
    }

    #[test]
    fn 遡って選択したまま出力が来ても同じ文字を指し続ける() {
        let (rows, cols) = (5u16, 20u16);
        let mut s = quiet_session(9971, rows, cols, 200);
        feed_lines(&s, 0..30);
        // 履歴を 10 行ぶん遡って、真ん中の行を選ぶ。
        s.set_scroll(10);
        s.set_selection(((2, 0), (2, 6)));
        let want = s.selection_string();
        assert!(want.starts_with("line"), "行を選べている: {want:?}");
        assert_eq!(under_highlight(&s), want, "選ぶ直前は一致している");

        // ここでエージェントが 3 行出力する (利用者は遡ったまま)。
        s.feed(b"a\r\nb\r\nc\r\n");
        assert_eq!(s.selection_string(), want, "コピーが同じ文字を指し続ける");
        assert_eq!(under_highlight(&s), want, "ハイライトが同じ文字の上にある");

        // さらに 1 行ぶん遡る。画面は 1 行しか動かない。
        let before = s.eff_scroll();
        assert!(s.adjust_scroll(1), "遡れる");
        assert_eq!(s.eff_scroll(), before + 1, "1 行ぶんだけ動く");
        assert_eq!(under_highlight(&s), want, "スクロールしてもずれない");
        assert_eq!(s.selection_string(), want, "コピーもずれない");
        s.kill();
    }

    #[test]
    fn 履歴から押し出された選択は端で止まるか解除される() {
        let (rows, cols) = (5u16, 20u16);
        // 履歴は 8 行しか持てない = すぐ満杯になる。
        let mut s = quiet_session(9972, rows, cols, 8);
        feed_lines(&s, 0..20);
        // 履歴の最古 2 行を選ぶ (画面行 0..1)。
        s.set_scroll(usize::MAX);
        assert!(s.eff_scroll() > 0, "遡れている");
        s.set_selection(((0, 0), (1, 6)));
        let want = s.selection_string();
        let mut kept = want.lines();
        let dropped = kept.next().expect("上の行").to_string();
        let survives = kept.next().expect("下の行").to_string();

        // 1 行流れると、上の行は履歴から落ちる → 残った側で止まる。
        s.feed(b"a\r\n");
        let now = s.selection_string();
        assert!(
            !now.contains(&dropped),
            "消えた行を指し続けていない: {now:?}"
        );
        assert_eq!(now, survives, "生きている端で止まる: {now:?}");

        // さらに流れて選択が丸ごと落ちたら解除する (黙って別の場所を指さない)。
        s.feed(b"b\r\nc\r\nd\r\ne\r\nf\r\n");
        assert_eq!(s.selection_string(), "", "残骸をコピーさせない");
        assert_eq!(s.sel_abs, None, "選択が解除されている");
        assert!(s.selection.is_none(), "ハイライトも消えている");
        s.kill();
    }

    #[test]
    fn 代替画面を出入りしても選択がずれない() {
        let (rows, cols) = (5u16, 20u16);
        let mut s = quiet_session(9973, rows, cols, 200);
        feed_lines(&s, 0..30);
        s.set_scroll(6);
        s.set_selection(((1, 0), (1, 6)));
        let want = s.selection_string();
        let abs = s.sel_abs;
        assert!(want.starts_with("line"), "行を選べている: {want:?}");

        // vim / less: 代替画面へ入って描いて出る。履歴は 1 行も伸びない。
        s.feed(b"\x1b[?1049h");
        s.feed(b"alt screen\r\nmore lines\r\n");
        s.feed(b"\x1b[?1049l");
        assert_eq!(s.sel_abs, abs, "代替画面は絶対行を動かさない");
        assert_eq!(s.selection_string(), want, "戻ってきたら同じ文字を指す");
        s.kill();
    }
}
