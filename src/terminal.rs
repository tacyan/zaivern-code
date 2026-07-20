use std::collections::HashMap;
use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::theme::Theme;

pub struct SpawnSpec {
    pub title: String,
    pub preset_name: String,
    pub icon: String,
    pub command: String,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
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
    writer: Box<dyn IoWrite + Send>,
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
    /// 全自動YESの直近送信時刻(同じプロンプトへの連打防止)。
    last_auto_reply: Option<Instant>,
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
}

/// scan_attention の結果。
pub enum Attention {
    /// 新たに承認待ちになった(ユーザーの操作が必要)。
    NeedsApproval,
    /// 全自動YESモードが承認プロンプトへ自動応答した(説明文)。
    AutoReplied(&'static str),
}

/// 全自動YESモード用: 画面の承認プロンプトを分類し、送るキー列と説明を返す。
///
/// bypass 起動でも CLI エージェントは起動時/プラン承認などで対話プロンプトを出すため、
/// これに答えないと「全自動なのに進まない」状態になる。
pub fn auto_yes_reply(text: &str) -> Option<(&'static [u8], &'static str)> {
    // 初回の bypass 警告: デフォルト選択が「1. No, exit」なので
    // Enter ではなく番号キー「2」で「Yes, I accept」を直接選ぶ。
    if text.contains("Bypass Permissions mode") && text.contains("Yes, I accept") {
        return Some((b"2", "Bypass警告に「Yes, I accept」"));
    }
    // フォルダ信頼確認: デフォルトが「1. Yes, proceed」なので Enter で確定。
    if text.contains("trust the files in this folder") {
        return Some((b"\r", "フォルダ信頼確認に「Yes」"));
    }
    // 選択カーソルが Yes の上にある一般的な確認 → Enter で確定。
    if text.contains("❯ 1. Yes") {
        return Some((b"\r", "「Yes」"));
    }
    // カーソル位置に依らず番号キーで「1. Yes」を直接選ぶ。
    // 誤爆防止のため質問文(Do you want / Would you like)がある場合のみ。
    if (text.contains("Do you want") || text.contains("Would you like to proceed"))
        && text.contains("1. Yes")
    {
        return Some((b"1", "「1. Yes」"));
    }
    if text.contains("(y/n)") || text.contains("[y/N]") || text.contains("[y/n]") {
        return Some((b"y\r", "「y」"));
    }
    None
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
            .map_err(|e| format!("PTYを開けませんでした: {e}"))?;

        let cmd = build_command(&spec.command, &spec.cwd, &spec.env);
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("起動に失敗しました: {e}"))?;
        let killer = child.clone_killer();
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 5000)));
        let exited = Arc::new(AtomicBool::new(false));
        let exit_code: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| e.to_string())?;
        {
            let parser = parser.clone();
            let exited = exited.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            parser.lock().unwrap().process(&buf[..n]);
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

        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

        let launched_bypass = crate::agents::command_is_bypass(&spec.command);

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
            last_auto_reply: None,
            selection: None,
            sel_anchor: None,
            copied_at: None,
            user_typed: false,
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

    fn command_head(&self) -> Option<&str> {
        self.command.split_whitespace().next()
    }

    /// Zaivern 側で承認モードを統一制御している CLI エージェントか。
    pub fn is_permission_agent(&self) -> bool {
        matches!(self.command_head(), Some("claude" | "codex" | "agy"))
    }

    /// 実行中セッションへ送れる権限モード切替のキー列。
    pub fn permission_switch_keys(&self) -> Option<&'static [u8]> {
        match self.command_head()? {
            "claude" | "agy" => Some(b"\x1b[Z"),
            "codex" => Some(b"/permissions\r"),
            _ => None,
        }
    }

    /// 権限モード切替ボタンの説明。
    pub fn permission_switch_hint(&self) -> Option<&'static str> {
        match self.command_head()? {
            "claude" => Some("権限モード切替 (Shift+Tab)"),
            "agy" => Some("権限モード切替 (Shift+Tab)"),
            "codex" => Some("権限モード切替 (/permissions)"),
            _ => None,
        }
    }

    /// 全自動YESの対象セッションか(bypass 権限で起動した対応 CLI のみ)。
    pub fn auto_yes(&self) -> bool {
        self.launched_bypass && self.is_permission_agent()
    }

    /// 画面内容から「ユーザーの承認待ち」を推定する(約1秒間隔)。
    /// auto_yes=true なら承認プロンプトへ自動でYESを送信し AutoReplied を返す。
    /// それ以外は、新たに承認待ちへ遷移したときだけ NeedsApproval を返す。
    pub fn scan_attention(&mut self, auto_yes: bool) -> Option<Attention> {
        if self.last_scan.elapsed().as_millis() < 900 {
            return None;
        }
        self.last_scan = Instant::now();
        let text = self.parser.lock().unwrap().screen().contents();
        const PATTERNS: [&str; 6] = [
            "Do you want",
            "Would you like to proceed",
            "❯ 1. Yes",
            "1. Yes",
            "(y/n)",
            "[y/N]",
        ];
        let reply = auto_yes_reply(&text);
        let waiting = reply.is_some() || PATTERNS.iter().any(|p| text.contains(p));
        let newly = waiting && !self.attention;
        self.attention = waiting;
        if auto_yes {
            if let Some((bytes, desc)) = reply {
                // 直前の応答が効くまでの猶予を置き、同じプロンプトへ連打しない。
                // プロンプトが画面に残っている限り2秒おきに再送する(取りこぼし対策)。
                let ready = self
                    .last_auto_reply
                    .map(|t| t.elapsed().as_millis() >= 2000)
                    .unwrap_or(true);
                if ready {
                    self.last_auto_reply = Some(Instant::now());
                    self.write_bytes(bytes);
                    self.attention = false;
                    return Some(Attention::AutoReplied(desc));
                }
                return None;
            }
        }
        if newly {
            Some(Attention::NeedsApproval)
        } else {
            None
        }
    }

    pub fn running(&self) -> bool {
        !self.exited.load(Ordering::SeqCst)
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// 前回聞いてから人が手で打ったか。読んだ時点で印は下ろす。
    /// 音声入力が「書き込み済みの文字列」の追跡を捨てるかどうかの判断に使う。
    pub fn take_user_typed(&mut self) -> bool {
        std::mem::take(&mut self.user_typed)
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

    /// 承認待ちフラグを解除する(バブルで応答した後に呼ぶ)。
    ///
    /// プロンプトがまだ画面に残っていれば次回の scan_attention で再検出される。
    pub fn resolve_attention(&mut self) {
        self.attention = false;
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
    fn plain_output_is_not_a_prompt() {
        // 質問文なしの番号リスト(通常の出力)には反応しない
        assert!(auto_yes_reply("手順:\n1. Yes と入力\n2. 実行").is_none());
        assert!(auto_yes_reply("ビルドが完了しました").is_none());
    }

    use super::{normalize_sel, selection_text, word_selection};

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
            icon: "🦀".into(),
            command: cmd.into(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
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
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if modifiers.mac_cmd {
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
            // 人が打った分は音声入力の書き込み追跡とずれるので印を立てる
            session.user_typed = true;
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
            if focused {
                painter.rect_filled(cursor_rect, 1.0, theme.accent.gamma_multiply(0.55));
            } else {
                painter.rect_stroke(
                    cursor_rect,
                    1.0,
                    egui::Stroke::new(1.0_f32, theme.accent.gamma_multiply(0.7)),
                );
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
        let label = format!("⤒ {} ⤓ 一番下へ", session.scroll);
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
        if r.on_hover_text("クリックで履歴表示を終了して一番下(最新)へ戻る")
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
            format!("✕ 終了 (code {code})"),
            egui::FontId::proportional(11.0),
            theme.err,
        );
    }

    // 右クリックメニュー: コピー操作
    if interactive {
        response.context_menu(|ui| {
            let has_sel = session.selection.is_some();
            if ui
                .add_enabled(has_sel, egui::Button::new("📋 選択をコピー (⌘C)"))
                .clicked()
            {
                copy_selection(ui, session);
                ui.close_menu();
            }
            if ui.button("📄 画面全体をコピー").clicked() {
                let text = session.parser.lock().unwrap().screen().contents();
                ui.ctx().copy_text(text);
                session.copied_at = Some(Instant::now());
                ui.close_menu();
            }
            if has_sel && ui.button("✕ 選択を解除").clicked() {
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
                "📋 コピーしました".into(),
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
