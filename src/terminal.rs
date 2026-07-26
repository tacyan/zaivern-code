use std::collections::HashMap;
use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use eframe::egui;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::i18n::{tr, trf};
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
    pub selection: Option<((u16, u16), (u16, u16))>,
    /// ドラッグ選択のアンカー(ドラッグ開始セル)。
    sel_anchor: Option<(u16, u16)>,
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
    /// 手動の「あとで見る」ピン。フォーカスを当て直す (acknowledge) まで未読扱い。
    pub pinned_unread: bool,
    /// レート制限/使用上限の警告が画面に出ているとき、その行。
    /// 警告が画面から消える (2 スキャン連続で不検出) と自動で外れる。
    pub rate_limited: Option<String>,
    /// レート制限警告を連続で見失った回数 (2 回で解除。1 回では画面遷移の瞬きと区別できない)。
    rl_miss: u8,
    /// このセッションの生ログの書き出し先 (再起動時の引き継ぎ・UI 表示用)。
    pub log_path: Option<PathBuf>,
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
pub fn auto_yes_reply_for(
    text: &str,
    agent: Option<&str>,
) -> Option<(&'static [u8], &'static str)> {
    // 管理者権限昇格など「自動で押してはいけない」画面ではここで打ち切る。
    if crate::agents::prompt_never_answer(text) {
        return None;
    }
    // カタログの応答表が最優先 (ユーザー定義ルール → 組み込みルールの順)。
    if let Some(hit) = crate::agents::prompt_rule_reply(text, agent) {
        return Some(hit);
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
    if text.contains("Press Enter to continue") || text.contains("Press Enter to proceed") || text.contains("Press [Enter]") {
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
    if agent_approval && (text.contains("1. Yes") || text.contains("1. Allow") || text.contains("Yes, proceed") || text.contains("Yes, allow")) {
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
    if has_question_context && (
        text.contains("1. Yes")
            || text.contains("1. Allow")
            || text.contains("1. はい")
            || text.contains("1. 許可")
            || text.contains("1. 実行")
            || text.contains("1. 承認")
            || text.contains("1. Accept")
            || text.contains("1. Continue")
            || text.contains("1) Yes")
            || text.contains("(1) Yes")
            || text.contains("[1] Yes")
    ) {
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
    let mut non_empty_lines = text.lines().rev().map(str::trim).filter(|line| !line.is_empty());
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

    endings.iter().any(|ending| line.ends_with(ending) || line.contains(ending))
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
pub fn prompt_signature(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        // 目印は固定表 + 応答表 (agents.rs) の needles。表に足したパターンは
        // 指紋にも自動で効くので、片方だけ更新して取りこぼす事故が起きない。
        let marked = SIG_MARKS.iter().any(|m| line.contains(m))
            || crate::agents::prompt_sig_marks().any(|m| line.contains(m));
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

/// セッション復元時、再生した前回スクロールバックの末尾へ入れる区切りバナー。
/// 先頭で代替画面 (?1049) とスクロール領域・文字属性を平常へ戻す — 前回ログが
/// TUI の途中で切れていても、バナーと今回の出力が壊れずに描かれるようにする。
/// `ESC[r` はカーソルをホームへ戻してしまうので、`ESC[999;1H` で最下行へ
/// 移してから書く (再生した最終行の**後ろ**にバナーが並ぶ)。
pub const RESTORE_BANNER: &str = "\x1b[?1049l\x1b[r\x1b[0m\x1b[999;1H\r\n\x1b[2m── 前回のセッションここまで / 再開します ──\x1b[0m\r\n";

impl Session {
    /// 前回セッションの生ログ (PTY 生バイト列) を vt100 パーサへ流し込み、
    /// 旧スクロールバックを見える状態にする。末尾に [`RESTORE_BANNER`] を足して
    /// 「どこからが今回か」を分かるようにする。spawn 直後 (エージェントの最初の
    /// 出力が届く前) に呼ぶ想定 — 読取スレッドとはパーサのロックで排他される。
    pub fn preload_scrollback(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut p = lock_ok(&self.parser);
        p.process(bytes);
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

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 5000)));
        let exited = Arc::new(AtomicBool::new(false));
        let exit_code: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| e.to_string())?;

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
            let focus_reports = focus_reports.clone();
            let clipboard_pending = clipboard_pending.clone();
            let report_fg = report_fg.clone();
            let report_bg = report_bg.clone();
            let mut log_sink = log_sink;
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let mut scanner = QueryScanner::default();
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Some(l) = log_sink.as_mut() {
                                l.write(&buf[..n]);
                            }
                            // 先に vt100 へ流してから走査する。CSI 6n はアプリが
                            // 「ここまで描いた」直後に送って返事を待つものなので、
                            // チャンクを反映し終えたカーソル位置が正解になる。
                            lock_ok(&parser).process(&buf[..n]);
                            let mut reply: Vec<u8> = Vec::new();
                            for ev in scanner.scan(&buf[..n]) {
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
                                        let rgb = if ps == 10 { &report_fg } else { &report_bg }
                                            .load(Ordering::Relaxed);
                                        reply.extend_from_slice(&color_report(ps, rgb));
                                    }
                                }
                            }
                            if !reply.is_empty() {
                                writer.send(&reply);
                            }
                            ctx.request_repaint();
                        }
                    }
                }
                exited.store(true, Ordering::SeqCst);
                ctx.request_repaint();
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
                ctx.request_repaint();
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
            sel_anchor: None,
            search: SearchUi::default(),
            copied_at: None,
            user_typed: false,
            cursor_shape,
            focus_reports,
            focus_sent: None,
            clipboard_pending,
            report_fg,
            report_bg,
            seen_hash: 0,
            cur_hash: 0,
            pinned_unread: false,
            rate_limited: None,
            rl_miss: 0,
            log_path: spec.log_path,
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
        self.cur_hash = semantic_hash(&text);
        // レート制限の「継続 / 解除」の追跡。新規検知の確定は末尾で行う
        // (承認イベントと同時のときは承認を優先し、通知を次回スキャンへ持ち越すため)。
        let rl_detect = detect_rate_limit(&text);
        if self.rate_limited.is_some() {
            match &rl_detect {
                Some(line) => {
                    self.rate_limited = Some(line.clone());
                    self.rl_miss = 0;
                }
                None => {
                    self.rl_miss += 1;
                    if self.rl_miss >= 2 {
                        self.rate_limited = None;
                        self.rl_miss = 0;
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
        let present = reply.is_some() || PATTERNS.iter().any(|p| text.contains(p));
        // 応答済みエピソードの追跡: プロンプトが画面から消えた、または指紋が
        // 変わった(連続承認キューの次のダイアログ等)ら「応答済み」を下ろす。
        let sig = if present { Some(prompt_signature(&text)) } else { None };
        if self.answered_sig.is_some() && self.answered_sig != sig {
            self.answered_sig = None;
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
        }
        // 自動YESの停滞ウォッチドッグ: 自動応答したのに同じプロンプトのまま
        // 画面が 30 秒間まったく変化しない (= 応答が取りこぼされた) 場合は、
        // YESを再送せず、ペットの「✔ 承認」ボタンと同じ操作へ切り替える。
        // 出力が流れている間 (cur_hash が動く間) は「進んでいる」ので送らない —
        // 応答済みプロンプトが画面に残っているだけの状態への連打事故を防ぐ。
        if auto_yes && present && self.answered_sig == sig {
            if let Some(since) = self.auto_stall_since {
                if self.cur_hash != self.auto_stall_hash {
                    self.auto_stall_hash = self.cur_hash;
                    self.auto_stall_since = Some(Instant::now());
                } else if since.elapsed() >= self.auto_yes_resend_after
                    && self.press_pet_approve_button(None)
                {
                    return Some(Attention::AutoReplied(
                        "自動YES停滞のためペットの承認ボタンを自動押下",
                    ));
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
                return Some(Attention::RateLimited(line));
            }
        }
        None
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
        let pack = |c: egui::Color32| {
            ((c.r() as u32) << 16) | ((c.g() as u32) << 8) | c.b() as u32
        };
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
    pub fn approve_reply(&self) -> Option<&'static str> {
        let text = lock_ok(&self.parser).screen().contents();
        let (bytes, _) = auto_yes_reply_for(&text, self.agent_bin())?;
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
            lock_ok(&self.parser).set_size(rows, cols);
        }
        if let Some((r, c)) = self.resize_debounce.on_request((rows, cols)) {
            self.ship_resize(r, c);
        }
    }

    /// 安定したサイズをワーカーへ渡す (無ければ遅延起動)。待たない。
    fn ship_resize(&mut self, rows: u16, cols: u16) {
        if self.resizer.is_none() {
            let weak: Weak<Mutex<Box<dyn MasterPty + Send>>> = Arc::downgrade(&self.master);
            self.resizer = ResizeCoalescer::start(
                format!("zv-pty-resize-{}", self.id),
                move |rows, cols| {
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
                },
            );
        }
        match &self.resizer {
            Some(r) => r.request(rows, cols),
            // ワーカーを起こせない環境 (スレッド枯渇など) では従来どおり同期適用。
            // 詰まり得るが、サイズを取りこぼすよりはよい。
            None => {
                let _ = lock_ok(&self.master).resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
        }
    }

    /// まだ PTY へ送っていないリサイズ要求が残っているか。
    /// draw はこれが立っている間 request_repaint し、安定カウントを完走させる。
    pub fn resize_pending(&self) -> bool {
        self.resize_debounce.pending()
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

    pub fn set_scroll(&mut self, n: usize) {
        if n != self.scroll {
            // 画面がスクロールすると選択セル座標の指す文字が変わるため解除する
            self.selection = None;
            self.sel_anchor = None;
        }
        self.scroll = n;
        lock_ok(&self.parser).set_scrollback(n);
    }

    pub fn adjust_scroll(&mut self, delta: i64) {
        let n = (self.scroll as i64 + delta).max(0) as usize;
        self.set_scroll(n);
    }

    /// ターミナル画面全体の文字列をすべて選択状態にする (Ctrl+A / Cmd+A)
    pub fn select_all(&mut self) {
        let p = lock_ok(&self.parser);
        let (rows, cols) = p.screen().size();
        if rows > 0 && cols > 0 {
            self.selection = Some(((0, 0), (rows.saturating_sub(1), cols.saturating_sub(1))));
        }
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
        let mouse_on = !matches!(
            s.mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
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
        assert_eq!(super::pick_tail_lines(screen, 8, 120), vec!["✻ テストを実行中…"]);
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
    use super::{auto_yes_reply, auto_yes_reply_for};

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
        assert!(desc.contains("y") || desc.contains("1") || desc.contains("Yes") || desc.contains("自動"));
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
            ("Allow access to this file?", "Yes, allow access", "No, deny access"),
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
        all_terminal_lines, input_area_selection, is_image_paste_chord_on, key_bytes, line_hits,
        mac_agent_input_bytes, normalize_sel, prune_clip_pngs, save_clipboard_png,
        search_scroll_target, selection_text, word_selection, Session, CLIP_PNG_KEEP,
    };

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
        let old = lines.iter().position(|l| l.contains("前回の出力 11")).unwrap();
        let banner = lines
            .iter()
            .position(|l| l.contains("前回のセッションここまで"))
            .unwrap();
        assert!(banner > old, "バナーは再生分の末尾: old={old} banner={banner}");
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
        assert_eq!(p.screen().rows(0, 20).count(), 5, "可視行数は画面行数のまま");
        p.set_scrollback(0);
    }

    #[test]
    fn decrc_after_shrink_does_not_panic() {
        // 代替画面 (?1049h) がカーソルを保存 → ペイン縮小 → ?1049l の DECRC が
        // 縮小前の行番号を復元 → 次の描画で範囲外 unwrap により PTY 読取
        // スレッドが panic し、端末が黒いまま戻らなくなっていた
        // (vendor/vt100 の saved_pos クランプの回帰テスト)。
        let mut p = vt100::Parser::new(30, 80, 100);
        p.process(b"\x1b[30;1H");   // カーソルを最下行 (30行目) へ
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
        let mut session =
            Session::spawn(9992, spec, eframe::egui::Context::default()).unwrap();
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
        assert!(name.starts_with("clip-") && name.ends_with(".png"), "{name}");
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
        let mut s =
            Session::spawn(999, spec, eframe::egui::Context::default()).expect("PTY起動");

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
        let mut s =
            Session::spawn(998, spec, eframe::egui::Context::default()).expect("PTY起動");

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
        assert_eq!(keys, "y\r", "Antigravityの (y/n) プロンプトには y+Enter を送るはず");
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
        let mut s =
            Session::spawn(995, spec, eframe::egui::Context::default()).expect("PTY起動");

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
        assert_eq!(replies, 1, "同じプロンプトへ自動YESが再送された(Enter連打バグ)");
        assert!(!s.attention, "応答済みの間はバブル表示条件(attention)が立たない");
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
        let mut s =
            Session::spawn(992, spec, eframe::egui::Context::default()).expect("PTY起動");
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
        assert!(s.auto_stall_since.is_none(), "ペット承認後も停滞監視が残った");

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
        let mut s =
            Session::spawn(994, spec, eframe::egui::Context::default()).expect("PTY起動");

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
        assert!(auto_replied, "表示された承認選択肢へ自動YESが送られなかった");

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
        assert!(approved, "子プロセスが自動YESを受信して承認処理を完了しなかった");
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
        let mut s =
            Session::spawn(993, spec, eframe::egui::Context::default()).expect("PTY起動");

        let mut needs_approval = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(false), Some(Attention::NeedsApproval)) {
                needs_approval = true;
                break;
            }
        }
        assert!(needs_approval, "自動YESオフ時に承認待ちとして検知されなかった");
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
        let mut s =
            Session::spawn(994, spec, eframe::egui::Context::default()).expect("PTY起動");

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
        let mut s =
            Session::spawn(992, spec, eframe::egui::Context::default()).expect("PTY起動");

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
        let mut s =
            Session::spawn(993, spec, eframe::egui::Context::default()).expect("PTY起動");

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
        assert!(redetected, "指紋の異なる2つ目のプロンプトが検知されなかった");
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
            if s.parser.lock().unwrap().screen().contents().contains(needle) {
                return;
            }
        }
        panic!("プロンプト {needle:?} が画面に出なかった");
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
        assert!(s.scan_attention(false).is_none(), "スロットル中に検出された");
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
        assert_ne!(s.cur_hash, 0, "採用されたスキャンで cur_hash が更新されるはず");
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
        assert!(s.scan_attention(false).is_none(), "500ms 経過では弾かれるはず");
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
        assert!(s.scan_attention(true).is_none(), "ペット承認直後に二重発火した");

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
            if !s.parser.lock().unwrap().screen().contents().contains("(y/n)") {
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
        assert!(!s.attention, "画面外に流れたプロンプトの承認待ちが解除されなかった");

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
                bin,
                cmd
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
            assert!(s.launched_bypass, "Auto モードが bypass 判定されない: {}", cmd);
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
            assert!(!s.launched_bypass, "{} の Ask が bypass 判定されている", bin);
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
        assert!(!s.auto_yes_target(false), "pet_auto_yes オフでは自動応答しない");
        s.kill();

        // カタログ外の素のコマンドは対象外(y/n プロンプトへ誤爆しない)
        let mut sh = probe_session(9701, "sleep 1");
        assert!(!sh.auto_yes_target(true), "カタログ外セッションは自動YESの対象外");
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
    #[cfg(not(windows))]
    let mut cmd = {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let mut c = CommandBuilder::new(shell);
        if command.trim().is_empty() {
            c.arg("-l");
        } else {
            c.arg("-lc");
            c.arg(command);
        }
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
        let mut c = CommandBuilder::new(shell);
        for a in windows_shell_args(!command.trim().is_empty()) {
            c.arg(a);
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
                env.get(name)
                    .cloned()
                    .or_else(|| std::env::var(name).ok())
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
    let pos = egui::pos2((rect.right() - 330.0).max(rect.left() + 4.0), rect.top() + 6.0);
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
                    let (enter, shift) = ui.input(|i| {
                        (i.key_pressed(egui::Key::Enter), i.modifiers.shift)
                    });
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
            let Some(cell) = screen.cell(row, col) else { continue };
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
        let c1 = if r == e.0 { e.1.min(last_col) } else { last_col };
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
        if i >= cs.len() || !matches!(cs[i], '›' | '❯' | '▸' | '▶' | '▌' | '>' | '$' | '%') {
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
    let Some(sel) = session.selection else {
        return;
    };
    let text = {
        let p = lock_ok(&session.parser);
        selection_text(p.screen(), sel)
    };
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
        session.selection = None;
        session.sel_anchor = None;
    }
    if response.drag_started_by(egui::PointerButton::Primary) {
        if let Some(pos) = response.interact_pointer_pos() {
            session.sel_anchor = Some(to_cell(pos));
            session.selection = None;
        }
    }
    if response.dragged_by(egui::PointerButton::Primary) {
        if let (Some(anchor), Some(pos)) =
            (session.sel_anchor, response.interact_pointer_pos())
        {
            session.selection = Some((anchor, to_cell(pos)));
        }
    }
    if response.double_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let (r, c) = to_cell(pos);
            session.selection = {
                let p = lock_ok(&session.parser);
                word_selection(p.screen(), r, c)
            };
        }
    }
    if response.triple_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let (r, _) = to_cell(pos);
            session.selection = Some(((r, 0), (r, cols_n.saturating_sub(1))));
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
fn is_image_paste_chord_on(key: egui::Key, m: egui::Modifiers, mac: bool) -> bool {
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
fn clipboard_image_to_png() -> Option<PathBuf> {
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
        || {
            clipboard_image_to_png().map(|png| format!("@{} ", prompt_path(&png, &cwd)))
        },
    );
    if !out.is_empty() {
        // 人が打った分は音声入力の書き込み追跡とずれるので印を立てる。
        // 承認プロンプトへの手入力応答もここで「応答済み」として解決する
        // (自動YESオフの手動運転で attention を引きずらないため)。
        session.note_user_input();
        session.write_bytes(&out);
        session.set_scroll(0);
    }
    if want_select_all {
        session.select_all();
    }
    if let Some((sel, text)) = input_select {
        // Ctrl+A の入力欄選択: 選択表示 + 即コピー (⌘C を待たない)
        session.selection = Some(sel);
        session.sel_anchor = None;
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
            let cell_rect =
                egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, cell_h));

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
            o.ime = Some(egui::output::IMEOutput {
                rect,
                cursor_rect,
            });
        });

        // IME 変換中の未確定文字列をカーソル位置にオーバーレイ表示。
        //
        // 幅は「確定したらグリッドで何桁になるか」で測る (文字数で測ると
        // 日本語が枠の 2 倍はみ出す)。等幅フォントでも合成文字や絵文字は
        // グリフ送りが桁数と一致しないので、桁数とグリフ幅の**広いほう**を
        // 場所取りに使い、端末の右端を越えるぶんだけ左へ寄せる
        // (寄せないと隣のパネルの上に未確定文字列が流れ出す)。
        if !session.preedit.is_empty() {
            let galley = painter.layout_no_wrap(
                session.preedit.clone(),
                font_id.clone(),
                theme.term_fg,
            );
            let want = (crate::textenc::str_width(&session.preedit) as f32 * cell_w)
                .max(galley.size().x);
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
    let font_id = egui::FontId::monospace(font_size);
    let (cell_w, cell_h) = ui.fonts(|f| (f.glyph_width(&font_id, 'M'), f.row_height(&font_id)));

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
        || (ui.input(|i| !i.raw.hovered_files.is_empty())
            && ui.rect_contains_pointer(rect));
    if dragging_file {
        ui.painter()
            .rect_stroke(rect, 6.0, egui::Stroke::new(2.0_f32, theme.accent));
    }

    let padding = 6.0;
    if allow_resize {
        let cols = ((rect.width() - padding * 2.0) / cell_w).floor() as u16;
        let rows = ((rect.height() - padding * 2.0) / cell_h).floor() as u16;
        session.resize(rows, cols);
        if session.resize_pending() {
            // 安定カウント (RESIZE_STABLE_FRAMES) が完走する前に再描画が
            // 止まると、最終サイズが PTY へ届かないまま残る。完走するまで
            // フレームを回し続けて取りこぼしを防ぐ (高々 K フレーム)。
            ui.ctx().request_repaint();
        }
    }

    // ── マウスによる文字選択(ドラッグ=範囲 / ダブルクリック=語 / トリプルクリック=行) ──
    if interactive {
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

    draw_screen(
        ui, &painter, session, theme, &font_id, rect, padding, cell_w, cell_h, focused,
    );

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
            egui::Button::new(
                egui::RichText::new(label).size(11.0).color(theme.term_bg),
            )
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

    // 右クリックメニュー: コピー操作
    if interactive {
        response.context_menu(|ui| {
            let has_sel = session.selection.is_some();
            if ui
                .add_enabled(has_sel, egui::Button::new(tr("📋 選択をコピー (⌘C)")))
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
            if ui.button(tr("🔍 端末内を検索 (⌘F)")).clicked() {
                session.search.open = true;
                session.search.focus_pending = true;
                ui.close_menu();
            }
            if has_sel && ui.button(tr("✕ 選択を解除")).clicked() {
                session.selection = None;
                session.sel_anchor = None;
                ui.close_menu();
            }
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
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(150));
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
            (true, Some(42), None, "終了済み → PID 再利用の巻き添え防止で撃たない"),
            (false, None, None, "PID 不明なら木は辿れない (killer の保険のみ)"),
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
            if lock_ok(&s.parser).screen().contents().contains("ZAIVERNPING") {
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
        assert!(quiet, "閉じたのに孫プロセスが書き続けている: {}", probe.display());
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
        assert_eq!(windows_shell_args(false), vec!["/K", "chcp 65001 >nul 2>nul"]);
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
        assert_eq!(replies(&scan1(b"\x1b[=c")), b"\x1bP!|00000000\x1b\\".to_vec());
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
        assert_eq!(replies(&scan1(b"\x1b_Gi=7,q=2,a=q;AA\x1b\\")), Vec::<u8>::new());
    }

    #[test]
    fn decscusr_all_ps_values() {
        use CursorShape::*;
        let cases: &[(&[u8], CursorShape)] = &[
            (b"\x1b[ q", Block),      // 引数省略 = 既定
            (b"\x1b[0 q", Block),
            (b"\x1b[1 q", Block),     // 点滅ブロック
            (b"\x1b[2 q", Block),     // 固定ブロック
            (b"\x1b[3 q", Underline), // 点滅アンダーライン
            (b"\x1b[4 q", Underline),
            (b"\x1b[5 q", Bar),       // 点滅バー (nvim/helix の挿入モード)
            (b"\x1b[6 q", Bar),
            (b"\x1b[9 q", Block),     // 未知の値はブロックへ倒す
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
            (b"\x1b]52;c;YWJjZGVm\x07", "abcdef"),     // 6byte, パディング無し
            (b"\x1b]52;c;YQ==\x07", "a"),              // "=="
            (b"\x1b]52;c;YWI=\x07", "ab"),             // "="
            (b"\x1b]52;c;\x07", ""),                   // 空(何も起きない, 下で確認)
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
            &b"\x1b]52;c;YWJ!\x07"[..],   // 不正な文字
            &b"\x1b]52;c;YWJjZ\x07"[..],  // 4文字境界に1文字余る
            &b"\x1b]52;c;=YWJj\x07"[..],  // パディングの後にデータ
            &b"\x1b]52;c;YQ===\x07"[..],  // パディング過剰
            &b"\x1b]52;c;/w==\x07"[..],   // 0xFF = 不正な UTF-8
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
        assert_eq!(color_report(11, 0x12141a), b"\x1b]11;rgb:1212/1414/1a1a\x1b\\".to_vec());
        // 色の「設定」(? が無い) には返事をしない
        assert_eq!(scan1(b"\x1b]11;#000000\x07"), vec![]);
    }

    // ── 誤検出しないこと ──

    #[test]
    fn ordinary_output_produces_no_replies() {
        let evs = scan1(
            b"\x1b[1;32mgreen\x1b[0m\r\n\x1b[2J\x1b[H\x1b[?1049h\x1b[38;2;255;0;0mred\x1b[m",
        );
        assert_eq!(evs, vec![]);
    }

    #[test]
    fn garbage_escapes_do_not_desync_the_scanner() {
        // 壊れた ESC の直後の正しい問い合わせを取りこぼさない
        assert_eq!(
            scan1(b"\x1b[\x01\x1b[6n"),
            vec![TermEvent::CursorReport]
        );
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
        assert_eq!(unsafe { libc::kill(gpid, 0) }, 0, "孫 (pid={gpid}) が起きていない");

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
        p.screen()
            .contents()
            .lines()
            .map(str::to_string)
            .collect()
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
            assert!(got.contains(&format!("line{i}")), "line{i} が消えた: {got:?}");
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
            let Some(cell) = screen.cell(row, c) else { continue };
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
                let Some(cell) = screen.cell(r, c) else { continue };
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
                    assert!(
                        prev.is_wide(),
                        "{what}: 継続セルの左が全角でない ({r},{c})"
                    );
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
        p.process("日本語のテキスト\r\n한국어 텍스트\r\n中文文本\r\nascii mixed 日本\r\n".as_bytes());
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
        assert_eq!(super::cell_draw_cols(screen, 0, 1), 1, "継続セル自体は 1 桁");
        assert_eq!(super::cell_draw_cols(screen, 0, 2), 2, "「本」");
        assert_eq!(super::cell_draw_cols(screen, 0, 4), 1, "半角 x");
        assert_eq!(super::cell_draw_cols(screen, 0, 5), 1, "空セル");
        assert_eq!(super::cell_draw_cols(screen, 0, 99), 1, "画面外でも落ちない");
        assert_eq!(super::cell_draw_cols(screen, 0, u16::MAX), 1, "桁が振り切れても落ちない");
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
        assert_eq!(cursor_span(p.screen(), r, c), (2, 2), "右半分でも左から 2 桁");
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
            &[preedit(""), egui::Event::Ime(egui::ImeEvent::Disabled), key(egui::Key::Escape)],
            &mut state,
        );
        assert!(plan.out.is_empty(), "取り消しで何かが送られた: {:?}", plan.out);
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
        assert_eq!(out_str(&plan), "日本語", "確定 Enter で改行を送ってはいけない");
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
        let plan = run(&[key(egui::Key::Enter), preedit(""), commit("漢字")], &mut state);
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
        assert_eq!(out_str(&plan), "日本語日本語", "先行 Text は素の入力として扱う");
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
            &[preedit("に"), egui::Event::Text("n".into()), preedit("にほ")],
            &mut state,
        );
        assert!(plan.out.is_empty(), "変換中の生テキストが漏れた: {:?}", plan.out);
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
                InputCaps { bracketed, ..caps() },
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
            ("確定あり(変換中から)", vec![preedit(""), commit("あ")], true, true),
            ("未確定が空になった", vec![preedit("")], true, true),
            ("変換していないのに空の未確定", vec![preedit("")], false, false),
            ("変換継続", vec![preedit("にほ")], true, false),
            ("無効化(変換中)", vec![egui::Event::Ime(egui::ImeEvent::Disabled)], true, true),
            ("無効化(非変換)", vec![egui::Event::Ime(egui::ImeEvent::Disabled)], false, false),
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
