//! 音声入力 — Zaivern 内で完結するプッシュトゥトーク方式。
//!
//! 設定したキーを **押している間だけ** 録音し、離すと認識結果がエージェントの
//! 入力欄へ「そのまま挿入」される。Enter は送られないので、内容を目で見て
//! 確認してから自分で Enter を押すまで送信されない (誤送信防止)。
//!
//! ## エンジン
//! - `mac`: 内蔵の Swift ヘルパー (SFSpeechRecognizer)。ソースはこのファイルに
//!   埋め込んであり、初回だけ `swiftc` で `~/.zaivern/voice/` にビルドされる。
//!   Info.plist をバイナリに埋め込むことで、マイク/音声認識の許可ダイアログが
//!   Zaivern 名義で出るようにしている。
//! - `command`: 任意の外部コマンド (Windows/Linux や自前エンジン用)。
//!
//! ## 子プロセスとの取り決め (両エンジン共通)
//! 標準出力へ 1 行 1 イベント:
//!   - `R`          … 準備完了
//!   - `P <text>`   … 認識途中 (partial)
//!   - `F <text>`   … 確定 (final)
//!   - `E <msg>`    … エラー
//! プレフィックスのない行は `F` (確定テキスト) とみなす。外部コマンドは
//! ただテキストを 1 行ずつ吐くだけでよい。停止は stdin へ `q` + 改行。

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;

use eframe::egui;

/// 認識テキストの届け先。
///
/// `Active` は「そのとき前面にいるエージェント」を毎回引き直すので、
/// 録音したままタブを切り替えれば宛先もついてくる。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Target {
    /// いまアクティブなエージェント
    #[default]
    Active,
    /// 実行中の全エージェント
    Broadcast,
    /// セッション id 指定 (Cockpit の各行の 🎤)
    Session(u64),
}

impl Target {
    /// 設定ファイルに残す文字列表現。Session は保存しない (再起動で消えるため)。
    pub fn name(self) -> &'static str {
        match self {
            Target::Broadcast => "broadcast",
            _ => "active",
        }
    }

    pub fn from_name(s: &str) -> Self {
        match s {
            "broadcast" => Target::Broadcast,
            _ => Target::Active,
        }
    }
}

/// 認識プロセスから UI スレッドへ届くイベント。
#[derive(Debug, Clone)]
pub enum Event {
    /// マイクが開いて認識が始まった
    Ready,
    /// 認識途中の見出し (確定ではない)
    Partial(String),
    /// 確定テキスト
    Final(String),
    /// エラー (ユーザーに見せる文言)
    Error(String),
    /// プロセスが終了した
    Ended,
}

/// 起動中の認識セッション 1 つ。drop すると子プロセスを止める。
pub struct Session {
    child: Child,
    rx: mpsc::Receiver<Event>,
    /// 停止要求済みか (二重 stop 防止)
    stopping: bool,
}

impl Session {
    /// 溜まっているイベントを取り出す。UI スレッドから毎フレーム呼ぶ。
    pub fn poll(&self) -> Vec<Event> {
        self.rx.try_iter().collect()
    }

    /// 録音を止める。子プロセスは最後の確定結果を吐いてから終了するので、
    /// すぐには kill せず、poll し続けて `Event::Ended` を待つこと。
    pub fn stop(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        if let Some(stdin) = self.child.stdin.as_mut() {
            let _ = stdin.write_all(b"q\n");
            let _ = stdin.flush();
        }
    }

    /// 応答がないときの強制終了。
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 設定から実際に使うエンジン名を決める ("mac" | "command" | "off")。
pub fn resolve_engine(engine: &str) -> &'static str {
    match engine {
        "mac" => "mac",
        "command" => "command",
        "off" => "off",
        // auto: macOS は内蔵、その他は外部コマンド
        _ if cfg!(target_os = "macos") => "mac",
        _ => "command",
    }
}

/// 認識を開始する。`ctx` は結果が届いたとき UI を起こすために使う。
pub fn start(
    engine: &str,
    lang: &str,
    command: &str,
    ctx: &egui::Context,
) -> Result<Session, String> {
    let mut cmd = match resolve_engine(engine) {
        "off" => return Err("音声入力は無効に設定されています (🎤 メニューで変更できます)".into()),
        "mac" => {
            let bin = ensure_mac_helper()?;
            let mut c = Command::new(bin);
            c.arg(lang);
            c
        }
        _ => {
            if command.trim().is_empty() {
                return Err(format!(
                    "この OS では音声認識コマンドの設定が必要です。\
                     config.toml の voice_command に、認識テキストを 1 行ずつ\
                     標準出力へ出すコマンドを指定してください ({}) ",
                    std::env::consts::OS
                ));
            }
            let line = command.replace("{lang}", lang);
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            let mut c = if cfg!(target_os = "windows") {
                let mut c = Command::new("cmd");
                c.arg("/C").arg(&line);
                c
            } else {
                let mut c = Command::new(shell);
                c.arg("-lc").arg(&line);
                c
            };
            c.env("ZAIVERN_VOICE_LANG", lang);
            c
        }
    };

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("音声認識プロセスを起動できません: {e}"))?;

    let (tx, rx) = mpsc::channel::<Event>();
    let stdout = child.stdout.take().ok_or("stdout を取得できません")?;
    let stderr = child.stderr.take();

    let t = tx.clone();
    let c = ctx.clone();
    std::thread::Builder::new()
        .name("zv-voice-out".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(ev) = parse_line(&line) {
                    if t.send(ev).is_err() {
                        return;
                    }
                    c.request_repaint();
                }
            }
            let _ = t.send(Event::Ended);
            c.request_repaint();
        })
        .map_err(|e| format!("音声スレッドを起動できません: {e}"))?;

    // stderr は最初の 1 行だけエラーとして拾う (ビルド警告等で溢れさせない)
    if let Some(stderr) = stderr {
        let t = tx;
        let c = ctx.clone();
        let _ = std::thread::Builder::new()
            .name("zv-voice-err".into())
            .spawn(move || {
                let mut first = true;
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if first && !line.trim().is_empty() {
                        first = false;
                        let _ = t.send(Event::Error(line));
                        c.request_repaint();
                    }
                }
            });
    }

    Ok(Session {
        child,
        rx,
        stopping: false,
    })
}

/// 子プロセスの 1 行をイベントへ。プレフィックスなしは確定テキスト扱い。
fn parse_line(line: &str) -> Option<Event> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return None;
    }
    let (tag, rest) = match line.split_once(' ') {
        Some((t, r)) => (t, r.trim()),
        None => (line, ""),
    };
    Some(match tag {
        "R" => Event::Ready,
        "P" => {
            if rest.is_empty() {
                return None;
            }
            Event::Partial(rest.to_string())
        }
        "F" => {
            if rest.is_empty() {
                return None;
            }
            Event::Final(rest.to_string())
        }
        "E" => Event::Error(if rest.is_empty() {
            "音声認識エラー".into()
        } else {
            rest.to_string()
        }),
        // 素のテキストを吐く外部コマンド向け
        _ => Event::Final(line.to_string()),
    })
}

// ─── macOS 内蔵ヘルパー ─────────────────────────────────────────────

fn voice_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".zaivern")
        .join("voice")
}

/// 内蔵ヘルパーのパス。無ければ (ソースが変わっていれば) ビルドする。
/// 初回だけ数秒かかるので、UI スレッドを止めないよう注意して呼ぶこと。
pub fn ensure_mac_helper() -> Result<PathBuf, String> {
    let dir = voice_dir();
    let bin = dir.join("zv-listen");
    let stamp = dir.join("zv-listen.stamp");
    let want = format!("{:x}", fnv1a(HELPER_SWIFT.as_bytes()));

    if bin.exists() && std::fs::read_to_string(&stamp).ok().as_deref() == Some(want.as_str()) {
        return Ok(bin);
    }

    std::fs::create_dir_all(&dir).map_err(|e| format!("{} を作成できません: {e}", dir.display()))?;
    let src = dir.join("zv-listen.swift");
    let plist = dir.join("zv-listen.plist");
    std::fs::write(&src, HELPER_SWIFT).map_err(|e| format!("ソースを書けません: {e}"))?;
    std::fs::write(&plist, HELPER_PLIST).map_err(|e| format!("plist を書けません: {e}"))?;

    let out = Command::new("swiftc")
        .arg("-O")
        .arg(&src)
        .arg("-o")
        .arg(&bin)
        // Info.plist を埋め込むと、マイク許可がこのバイナリ名義で要求できる
        .args(["-Xlinker", "-sectcreate", "-Xlinker", "__TEXT"])
        .args(["-Xlinker", "__info_plist", "-Xlinker"])
        .arg(&plist)
        .output()
        .map_err(|e| {
            format!(
                "swiftc を実行できません ({e})。\
                 Xcode Command Line Tools が必要です: xcode-select --install"
            )
        })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(4).collect();
        return Err(format!(
            "音声ヘルパーのビルドに失敗しました: {}",
            tail.into_iter().rev().collect::<Vec<_>>().join(" / ")
        ));
    }
    let _ = std::fs::write(&stamp, want);
    Ok(bin)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

const HELPER_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>dev.zaivern.listen</string>
  <key>CFBundleName</key><string>Zaivern Code</string>
  <key>NSMicrophoneUsageDescription</key><string>Zaivern Code の音声入力でマイクを使用します。</string>
  <key>NSSpeechRecognitionUsageDescription</key><string>話した内容を文字にしてエージェントの入力欄へ入れます。</string>
</dict>
</plist>
"#;

/// SFSpeechRecognizer を使う常駐ヘルパー。ビルド時に `~/.zaivern/voice/` へ展開される。
const HELPER_SWIFT: &str = r#"// zv-listen — Zaivern Code 内蔵の音声認識ヘルパー (自動生成)
import Foundation
import Speech
import AVFoundation

let lock = NSLock()
func emit(_ tag: String, _ body: String = "") {
    lock.lock()
    print(body.isEmpty ? tag : "\(tag) \(body.replacingOccurrences(of: "\n", with: " "))")
    fflush(stdout)
    lock.unlock()
}

let locale = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "ja-JP"

/// macOS が返す英語のエラーを、操作方法まで書いた日本語に置き換える。
func friendly(_ message: String) -> String {
    if message.contains("Siri and Dictation are disabled") {
        return "macOS の音声入力がオフです。システム設定 > キーボード > 音声入力 をオンにしてください"
    }
    if message.contains("not authorized") || message.contains("denied") {
        return "音声認識が許可されていません。システム設定 > プライバシーとセキュリティ > 音声認識 で許可してください"
    }
    return message
}

func now() -> Double { Date().timeIntervalSince1970 }

/// ひと息つくまでの秒数。これだけ言葉が伸びなかったら、そこまでを確定させる。
let pauseSeconds = 1.2

/// 停止を指示されるまで動き続ける認識ループ。
///
/// SFSpeechRecognizer の 1 タスクは 1 分程度で自動的に終わり、文が確定するたびに
/// タスクも終了する。喋り続けている間ずっと文字を出したいので、**マイク(音声エンジン)は
/// 開いたままタスクだけを張り直す**。こうすると Enter で入力欄を空にした直後でも、
/// 録音し直すことなくそのまま次の発話を拾える。
///
/// さらに、オンデバイス認識は「録音を止めるまで isFinal を返さない」ため、放っておくと
/// 喋っている間ずっと partial のままで入力欄に何も溜まらない。そこで **息継ぎ
/// (pauseSeconds) を検知したらそこで区切って確定させる**。区切るときは新しいタスクを
/// 先に張ってから古い方を endAudio するので、切れ目の音声を取りこぼさない。
final class Listener {
    let engine = AVAudioEngine()
    let reqLock = NSLock()
    var request: SFSpeechAudioBufferRecognitionRequest?
    var task: SFSpeechRecognitionTask?
    var recognizer: SFSpeechRecognizer?
    var lastText = ""
    var finished = false
    /// 区切りごとに増える世代番号。古いタスクからの partial を無視するために使う
    var gen = 0
    /// lastText が最後に伸びた時刻 (息継ぎ判定用)
    var lastChange = now()
    /// 暴走検知用: 直近 5 秒間にタスクを張り直した回数
    var restartsInWindow = 0
    var windowStart = now()

    /// タップから触られるので、リクエストの差し替えは必ずロックして行う。
    func swapRequest(_ next: SFSpeechAudioBufferRecognitionRequest?) -> SFSpeechAudioBufferRecognitionRequest? {
        reqLock.lock()
        let old = request
        request = next
        reqLock.unlock()
        return old
    }

    func start() {
        guard let rec = SFSpeechRecognizer(locale: Locale(identifier: locale)) else {
            emit("E", "この言語の音声認識に対応していません: \(locale)")
            exit(1)
        }
        recognizer = rec
        guard rec.isAvailable else {
            emit("E", "音声認識が利用できません。システム設定 > キーボード > 音声入力 をオンにしてください")
            exit(1)
        }

        // マイクは一度だけ開き、停止を指示されるまで閉じない
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        input.installTap(onBus: 0, bufferSize: 1024, format: format) { [weak self] buf, _ in
            guard let self = self else { return }
            self.reqLock.lock()
            self.request?.append(buf)
            self.reqLock.unlock()
        }
        engine.prepare()
        do { try engine.start() } catch {
            emit("E", "マイクを開けません: \(error.localizedDescription)")
            exit(1)
        }
        listen()
        // 息継ぎを見張って、区切りがついたところから順に確定させていく
        Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            self?.closeSegmentIfPaused()
        }
        emit("R")
    }

    /// 言葉が pauseSeconds 以上伸びていなければ、そこまでをひとまとまりとして確定する。
    ///
    /// **確定 `F` はここで自分から出す。** 古いタスクのコールバックに任せてはいけない —
    /// 確定が空文字だったりタスクが確定を出さずに死んだりすると `F` が永久に出ず、
    /// Zaivern 側は「まだ書き換えてよい文字列」を抱えたままになる。すると次のひとことの
    /// partial がその差分として計算され、**前の文を消して上書きしてしまう**
    /// (= 喋り直すたびに入力欄が置き換わり、続けて溜まっていかない)。
    ///
    /// 出す文字列は、すでに `P` として流したものと同じ `lastText`。Zaivern 側の差分は
    /// 空になるので端末へは 1 バイトも出ず、追跡だけが締められる。以後この区切りは
    /// もう書き換わらないので、次のひとことはその後ろへ書き足されていく。
    ///
    /// 先に確定を出してから次のタスクを張るので、新しい `P` が古い `F` を追い越す
    /// 余地はない。閉じた世代のコールバックは以後すべて捨てる。
    func closeSegmentIfPaused() {
        guard !finished, !lastText.isEmpty else { return }
        guard now() - lastChange >= pauseSeconds else { return }
        emit("F", lastText)
        let old = listen()
        old?.endAudio()
    }

    /// 認識タスクを 1 つ張り、置き換えられた前のリクエストを返す。
    ///
    /// 古いタスクのコールバックは世代番号で見分けて丸ごと捨てる。確定は
    /// `closeSegmentIfPaused` がすでに出しているので、拾い直す必要はない。
    @discardableResult
    func listen() -> SFSpeechAudioBufferRecognitionRequest? {
        guard !finished, let rec = recognizer else { return nil }
        gen += 1
        let myGen = gen
        let req = SFSpeechAudioBufferRecognitionRequest()
        req.shouldReportPartialResults = true
        if rec.supportsOnDeviceRecognition { req.requiresOnDeviceRecognition = true }
        let old = swapRequest(req)
        lastText = ""
        lastChange = now()

        task = rec.recognitionTask(with: req) { [weak self] result, error in
            guard let self = self else { return }
            // 区切りで閉じた世代は用済み。確定は closeSegmentIfPaused が出しているので、
            // ここで何か出すと確定済みの文を後から書き換えてしまう
            guard myGen == self.gen else { return }
            if let result = result {
                let text = result.bestTranscription.formattedString
                if result.isFinal {
                    if !text.isEmpty { emit("F", text) }
                    self.lastText = ""
                    self.restart()
                    return
                } else if text != self.lastText {
                    self.lastText = text
                    self.lastChange = now()
                    emit("P", text)
                }
            }
            if let error = error as NSError? {
                // 無音やタスクの寿命切れはエラーではない — 拾えていた分を確定して張り直す
                if error.domain == "kAFAssistantErrorDomain" {
                    if !self.lastText.isEmpty { emit("F", self.lastText) }
                    self.lastText = ""
                    self.restart()
                } else {
                    emit("E", friendly(error.localizedDescription))
                    exit(1)
                }
            }
        }
        return old
    }

    /// 停止指示が出ていなければ、間を置かずに次のタスクを張る。
    ///
    /// 認識器が壊れた状態だと「張った直後に失敗」を繰り返して CPU を焼くので、
    /// 短時間に張り直しすぎたら諦めてエラーを返す。
    func restart() {
        task = nil
        _ = swapRequest(nil)
        guard !finished else { return }
        let t = now()
        if t - windowStart > 5 {
            windowStart = t
            restartsInWindow = 0
        }
        restartsInWindow += 1
        if restartsInWindow > 12 {
            emit("E", "音声認識が繰り返し失敗しました。いったん停止します")
            exit(1)
        }
        DispatchQueue.main.async { [weak self] in self?.listen() }
    }

    /// マイクを止め、最後の確定結果を少しだけ待ってから終了する。
    func finish() {
        if finished { return }
        finished = true
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        swapRequest(nil)?.endAudio()
        let pending = lastText
        DispatchQueue.main.asyncAfter(deadline: .now() + 2.0) {
            // 確定が来なかったときは途中経過を確定として返す
            if !pending.isEmpty && self.lastText == pending { emit("F", pending) }
            exit(0)
        }
    }
}

let listener = Listener()

SFSpeechRecognizer.requestAuthorization { status in
    DispatchQueue.main.async {
        switch status {
        case .authorized: listener.start()
        case .denied:
            emit("E", "音声認識が許可されていません (システム設定 > プライバシーとセキュリティ > 音声認識)")
            exit(1)
        default:
            emit("E", "音声認識を利用できません")
            exit(1)
        }
    }
}

DispatchQueue.global().async {
    while let line = readLine(strippingNewline: true) {
        if line == "q" {
            DispatchQueue.main.async { listener.finish() }
            return
        }
    }
    DispatchQueue.main.async { listener.finish() }
}

signal(SIGTERM) { _ in exit(0) }
RunLoop.main.run()
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_protocol_lines() {
        assert!(matches!(parse_line("R"), Some(Event::Ready)));
        assert!(matches!(parse_line("P こんにちは"), Some(Event::Partial(t)) if t == "こんにちは"));
        assert!(matches!(parse_line("F 確定です"), Some(Event::Final(t)) if t == "確定です"));
        assert!(matches!(parse_line("E だめ"), Some(Event::Error(t)) if t == "だめ"));
        assert!(parse_line("").is_none());
        assert!(parse_line("P ").is_none());
    }

    #[test]
    fn bare_text_is_treated_as_final() {
        // 外部コマンドがテキストだけを吐くケース
        assert!(matches!(parse_line("今日はいい天気"), Some(Event::Final(t)) if t == "今日はいい天気"));
    }

    #[test]
    fn target_roundtrips_through_config() {
        assert_eq!(Target::from_name(Target::Active.name()), Target::Active);
        assert_eq!(
            Target::from_name(Target::Broadcast.name()),
            Target::Broadcast
        );
        // セッション指定は保存されず、次回は「アクティブ」に戻る
        assert_eq!(Target::from_name(Target::Session(7).name()), Target::Active);
        assert_eq!(Target::from_name("なにこれ"), Target::Active);
    }

    #[test]
    fn helper_keeps_listening_until_stopped() {
        // 確定のたびにタスクを張り直す実装であること (1文で終わる実装への先祖返り検知)
        assert!(HELPER_SWIFT.contains("func restart()"));
        assert!(HELPER_SWIFT.contains("self.restart()"));
        assert!(HELPER_SWIFT.contains("guard !finished"));
    }

    #[test]
    fn helper_finalizes_on_pauses() {
        // オンデバイス認識は停止するまで isFinal を返さないので、息継ぎで区切って
        // 確定させる仕組みが要る (これが無いと入力欄に何も溜まらない)
        assert!(HELPER_SWIFT.contains("func closeSegmentIfPaused()"));
        assert!(HELPER_SWIFT.contains("pauseSeconds"));
        assert!(HELPER_SWIFT.contains("Timer.scheduledTimer"));
        // 区切りの前後で音を落とさないよう、新タスクを張ってから旧リクエストを閉じる
        assert!(HELPER_SWIFT.contains("let old = listen()"));
        assert!(HELPER_SWIFT.contains("old?.endAudio()"));
        // 古いタスクの partial を拾わないための世代管理
        assert!(HELPER_SWIFT.contains("myGen == self.gen"));
    }

    #[test]
    fn helper_emits_final_before_the_next_partial() {
        // Zaivern 側は partial が届くたびに前回ぶんを消して書き直すので、確定 `F` が
        // 出ないままだと次のひとことが前の文を消して上書きしてしまう (= 続けて
        // 溜まっていかない)。区切りでは古いタスクのコールバックに任せず、
        // 新しいタスクを張る前に自分で確定を出すこと。
        let close = HELPER_SWIFT
            .split("func closeSegmentIfPaused()")
            .nth(1)
            .expect("closeSegmentIfPaused があること");
        let emit = close.find("emit(\"F\", lastText)").expect("区切りで自ら確定を出すこと");
        let relisten = close.find("let old = listen()").expect("次のタスクを張ること");
        assert!(emit < relisten, "確定は新しいタスクを張る前に出すこと");
        // 閉じた世代のコールバックは確定も含めて丸ごと捨てる (確定済みの文を
        // 後から書き換えないため)
        assert!(HELPER_SWIFT.contains("guard myGen == self.gen else { return }"));
    }

    #[test]
    fn auto_engine_depends_on_os() {
        let want = if cfg!(target_os = "macos") { "mac" } else { "command" };
        assert_eq!(resolve_engine("auto"), want);
        assert_eq!(resolve_engine("command"), "command");
        assert_eq!(resolve_engine("off"), "off");
    }

    /// 埋め込み Swift が実際に swiftc を通ることを確認する。
    /// ビルドに数秒かかるので通常のテスト実行からは外してある:
    ///   cargo test -- --ignored builds_mac_helper
    #[test]
    #[ignore]
    #[cfg(target_os = "macos")]
    fn builds_mac_helper() {
        let bin = ensure_mac_helper().expect("音声ヘルパーがビルドできること");
        assert!(bin.exists());
    }

    #[test]
    fn helper_source_is_intact() {
        // 生文字列の破損検知
        assert!(HELPER_SWIFT.contains("SFSpeechRecognizer"));
        assert!(HELPER_SWIFT.contains("requestAuthorization"));
        assert!(HELPER_PLIST.contains("NSMicrophoneUsageDescription"));
    }
}
