use std::collections::HashMap;
use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::i18n::{tr, trf};
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
    writer: Arc<Mutex<Box<dyn IoWrite + Send>>>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    pub exited: Arc<AtomicBool>,
    pub exit_code: Arc<Mutex<Option<u32>>>,
    pub started: Instant,
    size: (u16, u16),
    scroll: usize,
    /// IME 変換中テキスト(未確定文字列)。UI 側だけの状態で PTY へは送らない。
    pub preedit: String,
    /// CLI エージェントが承認待ち(プロンプト表示中)と推定される状態。
    pub attention: bool,
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
    /// 自動YESの再送までの停滞時間。既定 30 秒 (テストで短縮する)。
    auto_yes_resend_after: Duration,
    /// マウスドラッグによる文字選択: (開始セル, 終了セル)。(row, col) の画面表示座標。
    pub selection: Option<((u16, u16), (u16, u16))>,
    /// ドラッグ選択のアンカー(ドラッグ開始セル)。
    sel_anchor: Option<(u16, u16)>,
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
pub fn auto_yes_reply(text: &str) -> Option<(&'static [u8], &'static str)> {
    // ⚡ 最優先: Antigravity CLI (Google AGY) 専用の自動YESプロンプト超強化判定
    if text.contains("Antigravity") || text.contains("AGY") || text.contains("antigravity") || text.contains("agy") {
        // カーソル選択プロンプト（❯ 1 / ❯ Yes / ❯ Allow / ❯ はい / ❯ 許可 / ❯ Accept / ❯ Proceed）
        if text.contains("❯ 1")
            || text.contains("❯ Yes")
            || text.contains("❯ Allow")
            || text.contains("❯ はい")
            || text.contains("❯ 許可")
            || text.contains("❯ Accept")
            || text.contains("❯ Proceed")
            || text.contains("❯ Continue")
        {
            return Some((b"\r", "Antigravityのカーソル選択プロンプトに自動「Enter」"));
        }

        // 選択番号「1.」/「1)」/「[1]」/「(1)」の肯定選択肢
        let has_num_one = text.contains("1. Yes")
            || text.contains("1. Allow")
            || text.contains("1. はい")
            || text.contains("1. 許可")
            || text.contains("1. Accept")
            || text.contains("1. Proceed")
            || text.contains("1. Continue")
            || text.contains("1. Approve")
            || text.contains("1) Yes")
            || text.contains("1) Allow")
            || text.contains("[1] Yes")
            || text.contains("[1] Allow")
            || text.contains("(1) Yes")
            || text.contains("(1) Allow");
        if has_num_one {
            return Some((b"1", "Antigravityの選択プロンプトに自動「1」"));
        }

        // y/n テキスト形式
        if text.contains("(y/n)")
            || text.contains("[y/N]")
            || text.contains("[y/n]")
            || text.contains("(y/N)")
            || text.contains("(Y/n)")
            || text.contains("[Y/n]")
            || text.contains("(yes/no)")
            || text.contains("[yes/no]")
            || text.contains("(y/n)?")
            || text.contains("[y/N]?")
        {
            return Some((b"y\r", "Antigravityの(y/n)問い合わせに自動「y」"));
        }

        // 実行・許可・操作の全肯定キーワード
        let has_action_kw = text.contains("Allow")
            || text.contains("Approve")
            || text.contains("Confirm")
            || text.contains("Execute")
            || text.contains("Run")
            || text.contains("Proceed")
            || text.contains("Accept")
            || text.contains("許可")
            || text.contains("承認")
            || text.contains("実行")
            || text.contains("適用")
            || text.contains("続行")
            || text.contains("保存");
        if has_action_kw {
            return Some((b"y\r", "Antigravityの承認画面に自動「y」"));
        }

        // Antigravity の表示で質問マーク「?」「？」が含まれる、または末尾に入力待ちがある場合
        if text.contains('?') || text.contains('？') || recent_lines_has_question(text) {
            return Some((b"y\r", "Antigravityの問いかけに自動「y」"));
        }
    }

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
        return Some((b"y", "Codex/Antigravityの承認に「Yes」"));
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

/// YESモードで肯定する一般的な質問行か。
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
        if SIG_MARKS.iter().any(|m| line.contains(m)) || is_question_line(line) {
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

impl Session {
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

        let cmd = build_command(&spec.command, &spec.cwd, &spec.env);
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| trf("起動に失敗しました: {e}", &[("e", e.to_string())]))?;
        let killer = child.clone_killer();
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 5000)));
        let exited = Arc::new(AtomicBool::new(false));
        let exit_code: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| e.to_string())?;

        let writer: Arc<Mutex<Box<dyn IoWrite + Send>>> =
            Arc::new(Mutex::new(pair.master.take_writer().map_err(|e| e.to_string())?));
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
                            parser.lock().unwrap().process(&buf[..n]);
                            let mut reply: Vec<u8> = Vec::new();
                            for ev in scanner.scan(&buf[..n]) {
                                match ev {
                                    TermEvent::Reply(b) => reply.extend_from_slice(&b),
                                    TermEvent::CursorReport | TermEvent::ExtCursorReport => {
                                        let ext = matches!(ev, TermEvent::ExtCursorReport);
                                        let (r, c) = {
                                            let p = parser.lock().unwrap();
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
                                        *clipboard_pending.lock().unwrap() = Some(s);
                                    }
                                    TermEvent::ColorQuery(ps) => {
                                        let rgb = if ps == 10 { &report_fg } else { &report_bg }
                                            .load(Ordering::Relaxed);
                                        reply.extend_from_slice(&color_report(ps, rgb));
                                    }
                                }
                            }
                            if !reply.is_empty() {
                                let mut w = writer.lock().unwrap();
                                let _ = w.write_all(&reply);
                                let _ = w.flush();
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
                    *exit_code.lock().unwrap() = Some(status.exit_code());
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
            cwd: spec.cwd,
            env: spec.env,
            parser,
            writer,
            master: pair.master,
            killer,
            exited,
            exit_code,
            started: Instant::now(),
            size: (rows, cols),
            scroll: 0,
            preedit: String::new(),
            attention: false,
            notified_exit: false,
            launched_bypass,
            last_scan: Instant::now(),
            answered_sig: None,
            auto_stall_since: None,
            auto_stall_hash: 0,
            auto_yes_resend_after: Duration::from_secs(30),
            selection: None,
            sel_anchor: None,
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
        let text = self.parser.lock().unwrap().screen().contents();
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
        let reply = auto_yes_reply(&text);
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
        // 画面が 30 秒間まったく変化しない (= 応答が取りこぼされた) 場合だけ、
        // もう一度だけ応答を送る。以後も停滞が続けば 30 秒おきに繰り返す。
        // 出力が流れている間 (cur_hash が動く間) は「進んでいる」ので送らない —
        // 応答済みプロンプトが画面に残っているだけの状態への連打事故を防ぐ。
        if auto_yes && present && self.answered_sig == sig {
            if let Some(since) = self.auto_stall_since {
                if self.cur_hash != self.auto_stall_hash {
                    self.auto_stall_hash = self.cur_hash;
                    self.auto_stall_since = Some(Instant::now());
                } else if since.elapsed() >= self.auto_yes_resend_after {
                    if let Some((bytes, desc)) = reply {
                        self.auto_stall_since = Some(Instant::now());
                        self.write_bytes(bytes);
                        return Some(Attention::AutoReplied(desc));
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

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(bytes);
        let _ = w.flush();
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
        self.clipboard_pending.lock().unwrap().take()
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
        let text = self.parser.lock().unwrap().screen().contents();
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
        let text = self.parser.lock().unwrap().screen().contents();
        let (bytes, _) = auto_yes_reply(&text)?;
        std::str::from_utf8(bytes).ok()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.size == (rows, cols) || rows < 3 || cols < 20 {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser.lock().unwrap().set_size(rows, cols);
    }

    pub fn kill(&mut self) {
        let _ = self.killer.kill();
    }

    pub fn set_scroll(&mut self, n: usize) {
        if n != self.scroll {
            // 画面がスクロールすると選択セル座標の指す文字が変わるため解除する
            self.selection = None;
            self.sel_anchor = None;
        }
        self.scroll = n;
        self.parser.lock().unwrap().set_scrollback(n);
    }

    pub fn adjust_scroll(&mut self, delta: i64) {
        let n = (self.scroll as i64 + delta).max(0) as usize;
        self.set_scroll(n);
    }

    /// ターミナル画面全体の文字列をすべて選択状態にする (Ctrl+A / Cmd+A)
    pub fn select_all(&mut self) {
        let p = self.parser.lock().unwrap();
        let (rows, cols) = p.screen().size();
        if rows > 0 && cols > 0 {
            self.selection = Some(((0, 0), (rows.saturating_sub(1), cols.saturating_sub(1))));
        }
    }

    /// (代替画面か, アプリがマウス報告を有効にしているか, SGRエンコードか)。
    /// 代替画面(vim / less / Claude Code 等)にはスクロールバック履歴が無いため、
    /// ローカルスクロールではなくホイールをアプリへ転送する必要がある。
    pub fn wheel_modes(&self) -> (bool, bool, bool) {
        let p = self.parser.lock().unwrap();
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
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::auto_yes_reply;

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
        assert_eq!(bytes, b"y");
    }

    #[test]
    fn codex_network_approval_sends_yes_shortcut() {
        let screen = "Do you want to approve network access to \"crates.io\"?\n\
                      › 1. Yes\n  2. No";
        let (bytes, _) = auto_yes_reply(screen).unwrap();
        assert_eq!(bytes, b"y");
    }

    #[test]
    fn antigravity_all_prompts_send_auto_yes() {
        // (y/n) パターン
        let (bytes, desc) = auto_yes_reply("Antigravity: Allow execute this command? (y/n) ").unwrap();
        assert_eq!(bytes, b"y\r");
        assert!(desc.contains("Antigravity"));

        // 1. Allow パターン
        let (bytes, _) = auto_yes_reply("Antigravity: Allow tool call?\n  1. Allow\n  2. Deny").unwrap();
        assert_eq!(bytes, b"1");

        // ❯ 1. Yes パターン
        let (bytes, _) = auto_yes_reply("AGY: Confirm action\n  ❯ 1. Yes\n    2. No").unwrap();
        assert_eq!(bytes, b"\r");

        // 日本語プロンプトパターン
        let (bytes, _) = auto_yes_reply("Antigravity: ツールを許可しますか？ [y/N]").unwrap();
        assert_eq!(bytes, b"y\r");

        // 追加された拡張プロンプトパターン
        let (bytes, _) = auto_yes_reply("AGY: Proceed with file save?").unwrap();
        assert_eq!(bytes, b"y\r");

        let (bytes, _) = auto_yes_reply("antigravity: 変更を適用しますか？").unwrap();
        assert_eq!(bytes, b"y\r");

        let (bytes, _) = auto_yes_reply("Antigravity: Select option\n  1. Allow always\n  2. Deny").unwrap();
        assert_eq!(bytes, b"1");
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
            command: "echo UNREAD_MARKER_1; sleep 30".into(),
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

    use super::{normalize_sel, selection_text, word_selection, Session};

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
    fn word_selection_expands_token() {
        let mut p = vt100::Parser::new(5, 20, 0);
        p.process(b"foo bar-baz qux");
        // "bar-baz" の途中をダブルクリック → 語全体
        assert_eq!(word_selection(p.screen(), 0, 5), Some(((0, 4), (0, 10))));
        // 空白の上は選択なし
        assert_eq!(word_selection(p.screen(), 0, 3), None);
    }

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

    #[test]
    fn auto_yes_replies_only_once_while_same_prompt_remains() {
        use super::{Attention, Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        // 入力を読まずにプロンプトを出しっぱなしにする子。TUI ダイアログ同様
        // エコー無し (以前は画面に残っている限り2秒おきに再送 → Enter連打事故)。
        let cmd = r#"stty -echo; printf 'Do you want to proceed? (y/n) '; sleep 30"#;
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

    #[test]
    fn auto_yes_resends_after_stall_timeout() {
        use super::{Attention, Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        // 自動YESの応答 (y\r) を無視してプロンプトが固まったままの子。
        // 「YESを送ったのに効かず 30 秒止まる」停滞を再現する (テストは 2 秒に短縮)。
        let cmd = r#"stty -echo; printf 'Do you want to proceed? (y/n) '; sleep 30"#;
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

        // 2) 画面が固まったまま 2 秒経過 → ウォッチドッグが再送する
        let mut resent = 0u32;
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(true), Some(Attention::AutoReplied(_))) {
                resent += 1;
                break;
            }
        }
        assert_eq!(resent, 1, "停滞 2 秒後に自動YESが再送されなかった");

        // 3) 手動応答扱いにするとウォッチドッグは止まる
        s.resolve_attention();
        let mut after_manual = 0u32;
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(s.scan_attention(true), Some(Attention::AutoReplied(_))) {
                after_manual += 1;
            }
        }
        assert_eq!(after_manual, 0, "手動応答後に勝手な再送をした");
        s.kill();
    }

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

    #[test]
    fn resolve_attention_suppresses_same_prompt_redetection() {
        use super::{Attention, Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        let cmd = r#"stty -echo; printf 'Do you want to proceed? (y/n) '; sleep 30"#;
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

    #[test]
    fn manual_typing_resolves_attention_episode() {
        use super::{Attention, Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        // 自動YESオフの手動運転: ユーザーが端末へ直接応答したら、プロンプト風
        // テキストが画面に残っていても承認待ちを引きずらない。引きずると
        // coordinator が WaitingApproval のまま配達を保留し続け、エージェント間の
        // やり取りが進まなくなる (2026-07-24 の手動運転バグ)。
        let cmd = r#"stty -echo; sleep 2; printf 'Do you want to proceed? (y/n) '; sleep 30"#;
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

    #[test]
    fn next_prompt_with_different_signature_is_detected_again() {
        use super::{Attention, Session, SpawnSpec};
        use std::collections::HashMap;
        use std::time::Duration;

        // 連続承認キューを模す: 1つ目に応答済みでも、内容の異なる2つ目が
        // 現れたら(1つ目が画面から消えていなくても)新規プロンプトとして検出する
        let cmd = r#"stty -echo; printf 'cmd A\nDo you want to proceed? (y/n) '; sleep 4; printf '\ncmd B\nDo you want to proceed? (y/n) '; sleep 30"#;
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
        if !command.trim().is_empty() {
            c.arg("/C");
            c.arg(command);
        }
        c
    };
    cmd.cwd(cwd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("ZAIVERN", "1");
    for (k, v) in env {
        cmd.env(k, v);
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
        K::Backspace => vec![0x7f],
        K::Escape => vec![0x1b],
        K::ArrowUp => arrow(b'A'),
        K::ArrowDown => arrow(b'B'),
        K::ArrowRight => arrow(b'C'),
        K::ArrowLeft => arrow(b'D'),
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

/// 選択範囲をクリップボードへコピーし、フィードバック表示を開始する。
fn copy_selection(ui: &egui::Ui, session: &mut Session) {
    let Some(sel) = session.selection else {
        return;
    };
    let text = {
        let p = session.parser.lock().unwrap();
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
    }

    // ── マウスによる文字選択(ドラッグ=範囲 / ダブルクリック=語 / トリプルクリック=行) ──
    if interactive {
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
                    let p = session.parser.lock().unwrap();
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

    // 代替画面(Claude Code / vim / less 等)にスクロールバック履歴は無いため、
    // 切替後も古い履歴ビューが画面に残らないよう自動で一番下へ戻す。
    if session.scroll > 0 && session.wheel_modes().0 {
        session.set_scroll(0);
    }

    if focused {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
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
            let p = session.parser.lock().unwrap();
            let s = p.screen();
            (s.application_cursor(), s.bracketed_paste())
        };
        let mut out: Vec<u8> = Vec::new();
        let mut want_copy = false;
        for ev in &events {
            match ev {
                // ⌘C: 選択範囲をクリップボードへ(選択が無ければ何もしない)。
                // Ctrl+C は Key イベントとしてそのまま PTY へ届く(SIGINT)。
                egui::Event::Copy => {
                    want_copy = true;
                }
                egui::Event::Text(t) => {
                    out.extend_from_slice(t.as_bytes());
                }
                // IME(日本語入力など): 変換確定文字列を PTY へ送り、
                // 変換中の未確定文字列はオーバーレイ表示用に保持する。
                egui::Event::Ime(ime) => match ime {
                    egui::ImeEvent::Commit(t) => {
                        out.extend_from_slice(t.as_bytes());
                        session.preedit.clear();
                    }
                    egui::ImeEvent::Preedit(t) => {
                        session.preedit = t.clone();
                    }
                    egui::ImeEvent::Enabled | egui::ImeEvent::Disabled => {
                        session.preedit.clear();
                    }
                },
                egui::Event::Paste(t) => {
                    if bracketed {
                        out.extend_from_slice(b"\x1b[200~");
                    }
                    out.extend_from_slice(t.as_bytes());
                    if bracketed {
                        out.extend_from_slice(b"\x1b[201~");
                    }
                }
                egui::Event::Key {
                    key: egui::Key::A,
                    pressed: true,
                    modifiers,
                    ..
                } if modifiers.mac_cmd || modifiers.command || (modifiers.ctrl && modifiers.shift) => {
                    session.select_all();
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if modifiers.mac_cmd {
                        if *key == egui::Key::A {
                            session.select_all();
                        }
                        continue;
                    }
                    // IME 変換中はキーを IME に任せる(Enter/矢印で確定・候補選択するため)
                    if !session.preedit.is_empty() {
                        continue;
                    }
                    if let Some(b) = key_bytes(*key, *modifiers, app_cursor) {
                        out.extend_from_slice(&b);
                    }
                }
                _ => {}
            }
        }
        if !out.is_empty() {
            // 人が打った分は音声入力の書き込み追跡とずれるので印を立てる。
            // 承認プロンプトへの手入力応答もここで「応答済み」として解決する
            // (自動YESオフの手動運転で attention を引きずらないため)。
            session.note_user_input();
            session.write_bytes(&out);
            session.set_scroll(0);
        }
        if want_copy {
            copy_selection(ui, session);
        }
    } else if !session.preedit.is_empty() {
        session.preedit.clear();
    }

    if interactive && (focused || hover_scroll) && response.hovered() {
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

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, theme.term_bg);

    let sel_norm = session.selection.map(normalize_sel);
    {
        let parser = session.parser.lock().unwrap();
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
                let w = if cell.is_wide() { cell_w * 2.0 } else { cell_w };
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
        let cursor_rect = egui::Rect::from_min_size(
            egui::pos2(
                origin.x + cc as f32 * cell_w,
                origin.y + cr as f32 * cell_h,
            ),
            egui::vec2(cell_w, cell_h),
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
            // (これが無いと日本語入力イベントが届かない)
            ui.ctx().output_mut(|o| {
                o.mutable_text_under_cursor = true;
                o.ime = Some(egui::output::IMEOutput {
                    rect,
                    cursor_rect,
                });
            });

            // IME 変換中の未確定文字列をカーソル位置にオーバーレイ表示
            if !session.preedit.is_empty() {
                let galley = painter.layout_no_wrap(
                    session.preedit.clone(),
                    font_id.clone(),
                    theme.term_fg,
                );
                let pos = cursor_rect.min;
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
        let code = session.exit_code.lock().unwrap().unwrap_or(0);
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
                let text = session.parser.lock().unwrap().screen().contents();
                ui.ctx().copy_text(text);
                session.copied_at = Some(Instant::now());
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
        seq.extend(std::iter::repeat(b'A').take(MAX_CLIPBOARD_B64 + 4));
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
}
