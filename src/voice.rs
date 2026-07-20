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
//! - `powershell`: Windows 標準の System.Speech (オフラインのディクテーション)。
//!   ソースはこのファイルに埋め込んであり、`~/.zaivern/voice/` へ展開して
//!   `powershell.exe` (5.1) で走らせる。PowerShell 7 (pwsh) には System.Speech が
//!   無いので必ず `powershell.exe` を使うこと。
//! - `browser`: スマホリモートの `/voice` ページをブラウザで開き、Web Speech API に
//!   認識させる。子プロセスを持たないので `Session` にはならず、`App` 側で直接
//!   ブラウザを開く (`start` を呼んではいけない)。どの OS でも使える最後の砦。
//! - `command`: 任意の外部コマンド (自前エンジン用)。
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
use std::sync::OnceLock;

use eframe::egui;

/// Windows: GUI アプリから子プロセスを起こすときにコンソール窓を出さないフラグ。
/// (src/sound.rs と同じ値)
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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

/// 設定から実際に使うエンジン名を決める。
/// 返るのは "mac" | "powershell" | "browser" | "command" | "off" のどれか。
///
/// シェルを起動する認識器の探索が入るので、描画のたびに呼ばないこと
/// (探索結果は [`powershell_recognizers`] が一度だけキャッシュする)。
pub fn resolve_engine(engine: &str, lang: &str, command: &str) -> &'static str {
    // auto 以外なら探索は不要 — 無駄に powershell.exe を起こさない
    let ps_ok = if engine == "mac" || engine == "command" || engine == "powershell"
        || engine == "browser" || engine == "off" || !command.trim().is_empty()
    {
        false
    } else {
        recognizer_matches(powershell_recognizers(), lang)
    };
    resolve_engine_core(engine, std::env::consts::OS, ps_ok, command)
}

/// [`resolve_engine`] の純粋な中身。OS と「Windows 側に認識器があるか」を
/// 引数で受け取るので、どの OS 上でも全パターンをテストできる。
///
/// auto の優先順位は上から:
///   1. macOS → 内蔵の Swift ヘルパー
///   2. voice_command が設定済み → その外部コマンド (旧来の逃げ道を残す)
///   3. Windows で対応言語の認識器あり → PowerShell
///   4. それ以外 → ブラウザの /voice ページ
fn resolve_engine_core(engine: &str, os: &str, ps_ok: bool, command: &str) -> &'static str {
    match engine {
        "mac" => "mac",
        "command" => "command",
        "powershell" => "powershell",
        "browser" => "browser",
        "off" => "off",
        // auto
        _ if os == "macos" => "mac",
        _ if !command.trim().is_empty() => "command",
        _ if os == "windows" && ps_ok => "powershell",
        _ => "browser",
    }
}

/// 🎤 を押したとき実際に通る経路を、ツールチップ用の 1 行で返す。
/// 探索結果はキャッシュ済みなので描画スレッドから呼んでよい。
pub fn route_hint(engine: &str, lang: &str, command: &str) -> &'static str {
    match resolve_engine(engine, lang, command) {
        "mac" => "この PC では macOS 内蔵の音声認識を使います",
        "powershell" => "この PC では Windows 標準の音声認識を使います",
        "command" => "この PC では config.toml の voice_command を使います",
        "browser" => "この PC ではブラウザの音声入力ページが開きます (そちらのマイクで話します)",
        _ => "音声入力は無効に設定されています",
    }
}

/// Windows にインストール済みの音声認識エンジンのカルチャ名一覧。
/// `powershell.exe` を起こすので、`OnceLock` で 1 回だけ調べて使い回す。
/// Windows 以外では常に空。
pub fn powershell_recognizers() -> &'static [String] {
    static CACHE: OnceLock<Vec<String>> = OnceLock::new();
    CACHE.get_or_init(probe_powershell_recognizers)
}

fn probe_powershell_recognizers() -> Vec<String> {
    if !cfg!(target_os = "windows") {
        return Vec::new();
    }
    let mut c = Command::new("powershell.exe");
    c.args([
        "-NoProfile",
        "-Command",
        "Add-Type -AssemblyName System.Speech; \
         [System.Speech.Recognition.SpeechRecognitionEngine]::InstalledRecognizers() \
         | % { $_.Culture.Name }",
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let Ok(out) = c.output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// 一覧の中に `lang` を扱える認識器があるか。
/// 完全一致が無ければ言語部分 (ja-JP の "ja") だけで拾う — 埋め込みスクリプト側の
/// 選び方と揃えてある。
fn recognizer_matches(list: &[String], lang: &str) -> bool {
    let lang = lang.trim();
    if lang.is_empty() {
        return false;
    }
    let primary = |s: &str| s.split('-').next().unwrap_or("").to_ascii_lowercase();
    let want = primary(lang);
    list.iter()
        .any(|c| c.eq_ignore_ascii_case(lang) || primary(c) == want)
}

/// macOS 以外で `voice_engine = "mac"` が選ばれたときの案内。
/// swiftc / xcode-select の話をしても意味が無いので、OS 名と代替手段だけを出す。
fn mac_only_error(os: &str) -> String {
    format!(
        "voice_engine = \"mac\" は macOS 専用です (この PC は {os})。\
         config.toml の voice_engine を \"auto\" に戻すと、{os} に合った方法\
         (Windows なら標準の音声認識、それ以外はブラウザの音声入力ページ) が使われます"
    )
}

/// 認識を開始する。`ctx` は結果が届いたとき UI を起こすために使う。
pub fn start(
    engine: &str,
    lang: &str,
    command: &str,
    ctx: &egui::Context,
) -> Result<Session, String> {
    let mut cmd = match resolve_engine(engine, lang, command) {
        "off" => return Err("音声入力は無効に設定されています (🎤 メニューで変更できます)".into()),
        "browser" => {
            // ブラウザ経路は子プロセスを持たない。App 側が /voice を開くので
            // ここまで来るのは呼び出しミス。
            return Err("ブラウザの音声入力ページを使う設定です (start は呼ばれません)".into());
        }
        "mac" => {
            // mac エンジンは macOS 専用。他の OS で swiftc を探しに行っても
            // 意味不明なエラーになるだけなので、ここで打ち切る。
            if !cfg!(target_os = "macos") {
                return Err(mac_only_error(std::env::consts::OS));
            }
            let bin = ensure_mac_helper()?;
            let mut c = Command::new(bin);
            c.arg(lang);
            c
        }
        "powershell" => {
            let script = ensure_powershell_helper()?;
            let mut c = Command::new("powershell.exe");
            c.arg("-NoProfile")
                .arg("-ExecutionPolicy")
                .arg("Bypass")
                .arg("-File")
                .arg(&script)
                .arg(lang);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                c.creation_flags(CREATE_NO_WINDOW);
            }
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

// ─── Windows 内蔵ヘルパー ───────────────────────────────────────────

/// 埋め込みの PowerShell スクリプトを `~/.zaivern/voice/` へ展開してパスを返す。
/// 中身が変わったときだけ書き直す (mac ヘルパーと同じ stamp 方式)。
pub fn ensure_powershell_helper() -> Result<PathBuf, String> {
    let dir = voice_dir();
    let src = dir.join("zv-listen.ps1");
    let stamp = dir.join("zv-listen.ps1.stamp");
    let want = format!("{:x}", fnv1a(HELPER_PS1.as_bytes()));

    if src.exists() && std::fs::read_to_string(&stamp).ok().as_deref() == Some(want.as_str()) {
        return Ok(src);
    }

    std::fs::create_dir_all(&dir).map_err(|e| format!("{} を作成できません: {e}", dir.display()))?;
    // PowerShell 5.1 は BOM が無いと UTF-8 の .ps1 を誤読する (日本語が化ける)
    let mut bytes = String::from("\u{feff}");
    bytes.push_str(HELPER_PS1);
    std::fs::write(&src, bytes).map_err(|e| format!("音声スクリプトを書けません: {e}"))?;
    let _ = std::fs::write(&stamp, want);
    Ok(src)
}

/// System.Speech を使う常駐ヘルパー。`~/.zaivern/voice/zv-listen.ps1` へ展開される。
///
/// 認識イベントは別スレッドから飛んでくるが、PowerShell の
/// `Register-ObjectEvent -Action` はパイプラインが空くまで動かない (stdin 待ちで
/// 止まると発火しない) ので、購読と stdin 待ちはまとめて C# 側に持たせている。
const HELPER_PS1: &str = r#"# zv-listen.ps1 — Zaivern Code 内蔵の音声認識ヘルパー (自動生成)
# powershell.exe (5.1) 専用。pwsh (PowerShell 7) には System.Speech がありません。
param([string]$Lang = "ja-JP")
$ErrorActionPreference = "Stop"
# Zaivern 側は UTF-8 の行として読むので、既定の OEM コードページに落とさない
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
try {
    Add-Type -AssemblyName System.Speech
} catch {
    [Console]::Out.WriteLine("E Windows の音声認識 (System.Speech) を読み込めません: $($_.Exception.Message)")
    exit 1
}
Add-Type -ReferencedAssemblies System.Speech -TypeDefinition @"
using System;
using System.Speech.Recognition;

public class ZvListen {
    static readonly object gate = new object();

    // 1 行 1 イベント: R / P <text> / F <text> / E <msg>
    static void Emit(string tag, string body) {
        lock (gate) {
            body = (body ?? "").Replace("\r", " ").Replace("\n", " ");
            Console.Out.WriteLine(body.Length == 0 ? tag : tag + " " + body);
            Console.Out.Flush();
        }
    }

    // 完全一致を優先し、無ければ言語部分 (ja-JP の ja) で拾う
    static RecognizerInfo Pick(string lang) {
        RecognizerInfo loose = null;
        string primary = lang.Split('-')[0];
        foreach (RecognizerInfo ri in SpeechRecognitionEngine.InstalledRecognizers()) {
            if (string.Equals(ri.Culture.Name, lang, StringComparison.OrdinalIgnoreCase)) return ri;
            if (loose == null && string.Equals(ri.Culture.TwoLetterISOLanguageName, primary, StringComparison.OrdinalIgnoreCase)) loose = ri;
        }
        return loose;
    }

    public static int Run(string lang) {
        RecognizerInfo info = Pick(lang);
        if (info == null) {
            Emit("E", "この言語の音声認識が Windows にありません: " + lang + " (設定 > 時刻と言語 > 音声認識 で言語を追加してください)");
            return 1;
        }
        SpeechRecognitionEngine rec = new SpeechRecognitionEngine(info);
        // ディクテーション文法 = 決まり文句ではなく自由な話し言葉を拾う
        rec.LoadGrammar(new DictationGrammar());
        rec.SpeechHypothesized += delegate(object s, SpeechHypothesizedEventArgs e) { Emit("P", e.Result.Text); };
        rec.SpeechRecognized += delegate(object s, SpeechRecognizedEventArgs e) { Emit("F", e.Result.Text); };
        try {
            rec.SetInputToDefaultAudioDevice();
        } catch (Exception ex) {
            Emit("E", "マイクを開けません: " + ex.Message + " (設定 > プライバシー > マイク を確認してください)");
            return 1;
        }
        // Multiple = 1 文で終わらず、止めるまで認識し続ける
        rec.RecognizeAsync(RecognizeMode.Multiple);
        Emit("R", "");
        // 停止は stdin の "q" + 改行。パイプが閉じたときも止める。
        string line;
        while ((line = Console.In.ReadLine()) != null) {
            if (line.Trim() == "q") break;
        }
        try { rec.RecognizeAsyncStop(); } catch { }
        try { rec.Dispose(); } catch { }
        return 0;
    }
}
"@
exit [ZvListen]::Run($Lang)
"#;

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
    fn explicit_engine_wins_on_every_os() {
        // 明示指定は OS も探索結果も見ない
        for os in ["macos", "windows", "linux"] {
            for ps in [true, false] {
                for cmd in ["", "whisper --stdout"] {
                    for name in ["mac", "command", "powershell", "browser", "off"] {
                        assert_eq!(resolve_engine_core(name, os, ps, cmd), name);
                    }
                }
            }
        }
    }

    #[test]
    fn auto_engine_matrix() {
        // macOS は探索結果によらず内蔵 (voice_command があっても内蔵が勝つ)
        assert_eq!(resolve_engine_core("auto", "macos", false, ""), "mac");
        assert_eq!(resolve_engine_core("auto", "macos", true, ""), "mac");
        assert_eq!(resolve_engine_core("auto", "macos", false, "whisper"), "mac");

        // Windows: 認識器があれば PowerShell、無ければブラウザ
        assert_eq!(resolve_engine_core("auto", "windows", true, ""), "powershell");
        assert_eq!(resolve_engine_core("auto", "windows", false, ""), "browser");

        // Linux/その他は常にブラウザ (認識器の有無は関係ない)
        assert_eq!(resolve_engine_core("auto", "linux", false, ""), "browser");
        assert_eq!(resolve_engine_core("auto", "linux", true, ""), "browser");

        // voice_command が入っていれば mac 以外では必ずそれが勝つ (旧来の逃げ道)
        for os in ["windows", "linux"] {
            for ps in [true, false] {
                assert_eq!(resolve_engine_core("auto", os, ps, "whisper --stdout"), "command");
                // 空白だけは「未設定」とみなす
                assert_ne!(resolve_engine_core("auto", os, ps, "   "), "command");
            }
        }
    }

    #[test]
    fn recognizer_lookup_falls_back_to_language() {
        let list = vec!["en-US".to_string(), "ja-JP".to_string()];
        assert!(recognizer_matches(&list, "ja-JP"));
        assert!(recognizer_matches(&list, "JA-jp")); // 大小無視
        assert!(recognizer_matches(&list, "ja")); // 言語だけでも拾う
        assert!(recognizer_matches(&list, "en-GB")); // 地域違いは言語で拾う
        assert!(!recognizer_matches(&list, "fr-FR"));
        assert!(!recognizer_matches(&list, ""));
        assert!(!recognizer_matches(&[], "ja-JP"));
    }

    #[test]
    fn mac_engine_is_rejected_off_macos() {
        // macOS 以外で "mac" を選んだら swiftc の話ではなく、OS 名と代替手段を出すこと
        for os in ["windows", "linux"] {
            let err = mac_only_error(os);
            assert!(err.contains("macOS 専用"), "{err}");
            assert!(err.contains(os), "{err}");
            assert!(err.contains("auto"), "{err}");
            assert!(!err.contains("swiftc"), "{err}");
            assert!(!err.contains("xcode-select"), "{err}");
        }
    }

    #[test]
    fn powershell_helper_speaks_the_line_protocol() {
        // R / P / F / E を出し、stdin の "q" で止まること
        assert!(HELPER_PS1.contains("Emit(\"R\", \"\")"));
        assert!(HELPER_PS1.contains("Emit(\"P\", e.Result.Text)"));
        assert!(HELPER_PS1.contains("Emit(\"F\", e.Result.Text)"));
        assert!(HELPER_PS1.contains("Emit(\"E\""));
        assert!(HELPER_PS1.contains("Console.In.ReadLine()"));
        assert!(HELPER_PS1.contains("if (line.Trim() == \"q\") break;"));
        assert!(HELPER_PS1.contains("RecognizeAsyncStop()"));
    }

    #[test]
    fn powershell_helper_dictates_continuously() {
        // 1 文で終わる実装への先祖返り検知
        assert!(HELPER_PS1.contains("DictationGrammar"));
        assert!(HELPER_PS1.contains("RecognizeMode.Multiple"));
        assert!(HELPER_PS1.contains("SpeechHypothesized"));
        assert!(HELPER_PS1.contains("SpeechRecognized"));
        // 日本語が化けないよう UTF-8 で吐くこと
        assert!(HELPER_PS1.contains("[Console]::OutputEncoding"));
        // pwsh には System.Speech が無いので powershell.exe 前提であること
        assert!(HELPER_PS1.contains("System.Speech"));
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
