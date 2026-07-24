//! CLI 制御チャネル (仕様 6章)。
//!
//! `zai` は既定で GUI を起動する。**既知のサブコマンド名が第1引数に来たときだけ**
//! CLI として動作し、それ以外 (パス・存在しない語・引数なし) は従来どおり
//! ワークスペース指定として扱う ＝ `try_run_cli` が `None` を返し、
//! 呼び出し側 (main.rs) は GUI 起動へ落ちる。
//!
//! 実行中インスタンスとは `~/.zaivern/instance.json` を介して発見し、
//! 既存のローカル HTTP サーバ (remote.rs) へ素の TCP で HTTP/1.1 を話す。
//! 認証は remote.rs に合わせて `X-Token` ヘッダを使う (Bearer ではない)。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::zaivern_dir;

// ───────────────────────── インスタンスファイル ─────────────────────────

/// 実行中インスタンスの接続情報 (`~/.zaivern/instance.json`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub port: u16,
    pub token: String,
    pub workspace: String,
    pub pid: u32,
}

impl Instance {
    /// 現在のプロセスを指すインスタンス情報を作る。
    pub fn current(port: u16, token: &str, workspace: &str) -> Self {
        Self {
            port,
            token: token.to_string(),
            workspace: workspace.to_string(),
            pid: std::process::id(),
        }
    }
}

pub fn instance_path() -> PathBuf {
    zaivern_dir().join("instance.json")
}

/// 起動時に呼ぶ。`~/.zaivern/instance.json` を書き出す。
pub fn write_instance_file(port: u16, token: &str, workspace: &str) -> Result<(), String> {
    let inst = Instance::current(port, token, workspace);
    let dir = zaivern_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("~/.zaivern を作成できません: {e}"))?;
    let json = serde_json::to_string(&inst).map_err(|e| format!("JSON 化に失敗: {e}"))?;
    std::fs::write(instance_path(), json)
        .map_err(|e| format!("instance.json を書けません: {e}"))
}

/// 終了時に呼ぶ。存在しなくてもエラーにしない。
pub fn remove_instance_file() {
    let _ = std::fs::remove_file(instance_path());
}

/// `~/.zaivern/instance.json` を読む。ファイルが無い・壊れている・
/// `pid` が既に死んでいる場合は `None`。
pub fn read_instance_file() -> Option<Instance> {
    let raw = std::fs::read_to_string(instance_path()).ok()?;
    let inst: Instance = serde_json::from_str(&raw).ok()?;
    if !pid_alive(inst.pid) {
        return None;
    }
    Some(inst)
}

/// プロセスが生きているか。
///
/// 追加クレートを増やさないため外部コマンドで判定する。
/// unix: `kill -0 <pid>` — シグナルを送らず存在確認だけを行う標準的な手法。
/// windows: `tasklist /FI "PID eq <pid>" /NH` の出力に pid が現れるかを見る。
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

// ───────────────────────── 最小 HTTP クライアント ─────────────────────────

/// `http://127.0.0.1:<port>` へ HTTP/1.1 を素の TCP で話す。
/// 認証は remote.rs の実装に合わせ `X-Token` ヘッダ。
/// 戻り値は (ステータスコード, ボディ)。
fn http(inst: &Instance, method: &str, path: &str, body: Option<String>) -> Result<(u16, String), String> {
    let addr = format!("127.0.0.1:{}", inst.port);
    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| format!("インスタンスへ接続できません ({addr}): {e}"))?;
    // サーバ側は UI スレッドの応答を最大 15 秒待つ。それより短くすると
    // こちらが先に切れてしまうので、余裕を持たせる。
    let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(20)));

    let body = body.unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         X-Token: {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n",
        port = inst.port,
        token = inst.token,
        len = body.len(),
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("送信に失敗: {e}"))?;
    if !body.is_empty() {
        stream
            .write_all(body.as_bytes())
            .map_err(|e| format!("送信に失敗: {e}"))?;
    }
    stream.flush().ok();

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("応答の受信に失敗: {e}"))?;

    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, resp_body) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b.to_string()),
        None => (text.as_str(), String::new()),
    };
    let code = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| "応答を解釈できません".to_string())?;
    Ok((code, resp_body))
}

/// 実行中インスタンスを取得する。無ければ日本語で説明して `Err`。
fn require_instance() -> Result<Instance, String> {
    read_instance_file().ok_or_else(|| {
        "実行中の Zaivern Code が見つかりません。先に `zai` でエディタを起動してください。".to_string()
    })
}

/// API を叩き、成功なら本文を返す。エラーは日本語メッセージにする。
fn call(inst: &Instance, method: &str, path: &str, body: Option<String>) -> Result<String, String> {
    match http(inst, method, path, body)? {
        (200, b) => Ok(b),
        (401, _) => Err("認証に失敗しました。instance.json のトークンが古い可能性があります。".into()),
        (404, _) => Err(format!("この操作は実行中のインスタンスが対応していません ({path})。")),
        (504, _) => Err(
            "エディタが応答しません。ウィンドウが背面にあると OS が動作を止めるため、\n\
現在の状態を読む操作はエディタを一度前面に出してから実行してください。"
                .into(),
        ),
        (c, b) => Err(format!("エラー応答 {c}: {}", b.trim())),
    }
}

// ───────────────────────── 引数ディスパッチ ─────────────────────────

/// 第1引数が CLI サブコマンドとして既知かどうか。
/// ここに載っていない語 (パス・`.`・未知語) は GUI 起動として扱う。
pub fn is_cli_subcommand(word: &str) -> bool {
    matches!(
        word,
        "open"
            | "notify"
            | "prompt"
            | "run"
            | "panel"
            | "status"
            | "state"
            | "plugin"
            | "app"
            | "--help"
            | "-h"
            | "--version"
            | "-V"
    )
}

pub const HELP: &str = "\
Zaivern Code — CLI 制御チャネル

使い方:
  zai                          エディタを起動 (カレントディレクトリ)
  zai <ディレクトリ>            エディタを起動 (ワークスペース指定)

サブコマンド (実行中のエディタを操作します):
  zai open <ファイル> [--line N]        ファイルを開く
  zai notify <メッセージ> [--level info|warn|error]
                                        通知を表示する
  zai prompt <テキスト> [--agent 名前] [--submit]
                                        エージェント入力欄へ差し込む
  zai run <コマンド...>                 ターミナルでコマンドを実行する
  zai panel <パネルID> <テキスト>       パネルの内容を書き換える
  zai status <テキスト>                 ステータスバーの表示を変える
  zai state                             実行中インスタンスの状態を JSON で出力

プラグイン (エディタが起動していなくても使えます):
  zai plugin list                       導入済みプラグインを一覧表示
  zai plugin new <名前>                 雛形を作成してパスを表示
  zai plugin enable <名前>              有効化
  zai plugin disable <名前>             無効化

アプリ登録 (OS のアプリ一覧から起動できるようにします):
  zai app install                       Launchpad / アプリメニュー / スタートメニューへ登録
  zai app uninstall                     登録を解除

その他:
  zai --help | -h                       このヘルプ
  zai --version | -V                    バージョン
";

/// CLI として処理したら `Some(終了コード)`、
/// CLI 呼び出しではない (GUI を起動すべき) なら `None`。
///
/// `args` はプログラム名を除いた引数列 (`std::env::args().skip(1)`)。
pub fn try_run_cli(args: &[String]) -> Option<i32> {
    let first = args.first()?;
    if !is_cli_subcommand(first) {
        return None;
    }
    let rest = &args[1..];
    // "app" はプロジェクトによくあるディレクトリ名。単独指定で ./app が実在するなら
    // ワークスペース指定として GUI 起動に譲る (登録は `zai app install` と明示する)。
    if first == "app" && rest.is_empty() && std::path::Path::new("app").is_dir() {
        return None;
    }
    Some(match first.as_str() {
        "--help" | "-h" => {
            println!("{HELP}");
            0
        }
        "--version" | "-V" => {
            println!("Zaivern Code {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "plugin" => run_plugin(rest),
        "app" => crate::desktop::run(rest),
        other => match run_remote(other, rest) {
            Ok(out) => {
                if !out.is_empty() {
                    println!("{out}");
                }
                0
            }
            Err(msg) => {
                eprintln!("{msg}");
                1
            }
        },
    })
}

// ───────────────────────── 引数ヘルパ ─────────────────────────

/// `--key 値` を取り出し、残りの位置引数を返す。
fn take_opt(args: &[String], key: &str) -> (Option<String>, Vec<String>) {
    let mut value = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == key {
            if i + 1 < args.len() {
                value = Some(args[i + 1].clone());
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        rest.push(args[i].clone());
        i += 1;
    }
    (value, rest)
}

/// `--flag` の有無を取り出し、残りの位置引数を返す。
fn take_flag(args: &[String], key: &str) -> (bool, Vec<String>) {
    let found = args.iter().any(|a| a == key);
    let rest: Vec<String> = args.iter().filter(|a| *a != key).cloned().collect();
    (found, rest)
}

// ───────────────────────── 実行中インスタンス向けサブコマンド ─────────────────────────

fn run_remote(cmd: &str, args: &[String]) -> Result<String, String> {
    let inst = require_instance()?;
    match cmd {
        "open" => {
            let (line, rest) = take_opt(args, "--line");
            let path = rest
                .first()
                .ok_or("開くファイルを指定してください: zai open <ファイル>")?;
            let line: i64 = line.and_then(|l| l.parse().ok()).unwrap_or(0);
            let body = serde_json::json!({ "path": path, "line": line }).to_string();
            call(&inst, "POST", "/api/open", Some(body))?;
            Ok(format!("開きました: {path}"))
        }
        "notify" => {
            let (level, rest) = take_opt(args, "--level");
            let level = level.unwrap_or_else(|| "info".into());
            if !matches!(level.as_str(), "info" | "warn" | "error") {
                return Err(format!("--level は info / warn / error のいずれかです: {level}"));
            }
            let message = rest.join(" ");
            if message.is_empty() {
                return Err("通知するメッセージを指定してください: zai notify <メッセージ>".into());
            }
            let body = serde_json::json!({ "message": message, "level": level }).to_string();
            call(&inst, "POST", "/api/notify", Some(body))?;
            Ok("通知しました。".into())
        }
        "prompt" => {
            let (agent, rest) = take_opt(args, "--agent");
            let (submit, rest) = take_flag(&rest, "--submit");
            let text = rest.join(" ");
            if text.is_empty() {
                return Err("送るテキストを指定してください: zai prompt <テキスト>".into());
            }
            let body = serde_json::json!({
                "text": text,
                "agent": agent.clone().unwrap_or_default(),
                "submit": submit,
            })
            .to_string();
            // 専用 API がまだ無いインスタンスでは音声送信 API へ退避する
            // (テキスト差し込みという意味は同じ)。
            match call(&inst, "POST", "/api/prompt", Some(body)) {
                Ok(_) => {}
                Err(_) if agent.is_none() => {
                    let fallback =
                        serde_json::json!({ "text": text, "id": -1, "submit": submit }).to_string();
                    call(&inst, "POST", "/api/voice", Some(fallback))?;
                }
                Err(e) => return Err(e),
            }
            Ok(if submit {
                "エージェントへ送信しました。".into()
            } else {
                "エージェント入力欄へ差し込みました。".into()
            })
        }
        "run" => {
            // `zai run -- ls -la` の形も許す
            let args: &[String] = if args.first().map(|a| a == "--").unwrap_or(false) {
                &args[1..]
            } else {
                args
            };
            let command = args.join(" ");
            if command.is_empty() {
                return Err("実行するコマンドを指定してください: zai run <コマンド...>".into());
            }
            let body = serde_json::json!({ "text": command, "raw": false }).to_string();
            call(&inst, "POST", "/api/term", Some(body))?;
            Ok(format!("実行しました: {command}"))
        }
        "panel" => {
            let panel = args
                .first()
                .ok_or("パネルIDを指定してください: zai panel <パネルID> <テキスト>")?;
            let text = args[1.min(args.len())..].join(" ");
            let body = serde_json::json!({ "panel": panel, "text": text }).to_string();
            call(&inst, "POST", "/api/panel", Some(body))?;
            Ok(format!("パネルを更新しました: {panel}"))
        }
        "status" => {
            let text = args.join(" ");
            if text.is_empty() {
                return Err("表示するテキストを指定してください: zai status <テキスト>".into());
            }
            let body = serde_json::json!({ "text": text }).to_string();
            call(&inst, "POST", "/api/status", Some(body))?;
            Ok("ステータスを更新しました。".into())
        }
        "state" => {
            let out = call(&inst, "GET", "/api/state", None)?;
            Ok(out.trim().to_string())
        }
        other => Err(format!("不明なサブコマンドです: {other}")),
    }
}

// ───────────────────────── plugin サブコマンド (インスタンス不要) ─────────────────────────

fn run_plugin(args: &[String]) -> i32 {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let name = args.get(1).cloned().unwrap_or_default();
    let result = match sub {
        "list" => plugin_list(),
        "new" => plugin_new(&name),
        "enable" => plugin_set_enabled(&name, true),
        "disable" => plugin_set_enabled(&name, false),
        "" => Err("plugin のサブコマンドを指定してください: list / new / enable / disable".into()),
        other => Err(format!("不明な plugin サブコマンドです: {other}")),
    };
    match result {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            0
        }
        Err(msg) => {
            eprintln!("{msg}");
            1
        }
    }
}

fn plugin_list() -> Result<String, String> {
    let cfg = crate::config::load_plugins_config();
    let plugins = crate::plugins::scan_installed();
    if plugins.is_empty() {
        return Ok("導入済みのプラグインはありません。".into());
    }
    let mut out = String::new();
    for p in &plugins {
        let mark = if cfg.is_enabled(&p.name) { "有効" } else { "無効" };
        out.push_str(&format!("[{mark}] {} {}", p.name, p.version));
        if let Some(e) = &p.error {
            out.push_str(&format!("  ⚠ {e}"));
        }
        out.push('\n');
    }
    Ok(out.trim_end().to_string())
}

fn plugin_new(name: &str) -> Result<String, String> {
    if name.is_empty() {
        return Err("プラグイン名を指定してください: zai plugin new <名前>".into());
    }
    let dir = crate::plugins::create_template(name)?;
    Ok(format!("プラグインの雛形を作成しました: {}", dir.display()))
}

fn plugin_set_enabled(name: &str, enable: bool) -> Result<String, String> {
    if name.is_empty() {
        return Err("プラグイン名を指定してください。".into());
    }
    if !crate::plugins::valid_name(name) {
        return Err(format!("プラグイン名として使えません: {name}"));
    }
    let mut plugins = crate::config::load_plugins_config();
    if plugins.is_enabled(name) != enable {
        plugins.set_enabled(name, enable);
        crate::config::save_plugins_config(&plugins)?;
    }
    Ok(if enable {
        format!("有効にしました: {name}")
    } else {
        format!("無効にしました: {name}")
    })
}


// ───────────────────────── テスト ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── 引数ディスパッチ: GUI 起動を壊さないことが最重要 ──

    #[test]
    fn empty_args_launch_gui() {
        assert_eq!(try_run_cli(&[]), None);
    }

    #[test]
    fn dot_and_paths_launch_gui() {
        for a in [".", "..", "/some/path", "./src", "~/dev/x", "my-project"] {
            assert_eq!(try_run_cli(&v(&[a])), None, "{a} は GUI 起動であるべき");
        }
    }

    #[test]
    fn unknown_words_launch_gui() {
        for a in ["opening", "statues", "plugins", "runner", "--verbose", "-x"] {
            assert_eq!(try_run_cli(&v(&[a])), None, "{a} は GUI 起動であるべき");
        }
    }

    #[test]
    fn every_spec_subcommand_is_recognized() {
        for a in [
            "open", "notify", "prompt", "run", "panel", "status", "state", "plugin", "app",
            "--help", "-h", "--version", "-V",
        ] {
            assert!(is_cli_subcommand(a), "{a} は CLI サブコマンドであるべき");
        }
    }

    #[test]
    fn subcommand_words_are_exact_only() {
        for a in ["Open", "OPEN", "open ", " open", "state2", "plugin-list"] {
            assert!(!is_cli_subcommand(a), "{a:?} は CLI 扱いすべきでない");
        }
    }

    #[test]
    fn help_and_version_exit_zero() {
        assert_eq!(try_run_cli(&v(&["--help"])), Some(0));
        assert_eq!(try_run_cli(&v(&["-h"])), Some(0));
        assert_eq!(try_run_cli(&v(&["--version"])), Some(0));
        assert_eq!(try_run_cli(&v(&["-V"])), Some(0));
    }

    // ── ヘルプ文言 ──

    #[test]
    fn help_lists_every_subcommand() {
        for needle in [
            "zai open",
            "zai notify",
            "zai prompt",
            "zai run",
            "zai panel",
            "zai status",
            "zai state",
            "zai plugin list",
            "zai plugin new",
            "zai plugin enable",
            "zai plugin disable",
            "zai app install",
            "zai app uninstall",
            "--help",
            "--version",
        ] {
            assert!(HELP.contains(needle), "ヘルプに {needle} が無い");
        }
    }

    #[test]
    fn help_is_japanese() {
        assert!(HELP.contains("使い方:"));
        assert!(HELP.contains("サブコマンド"));
    }

    // ── instance.json の往復 ──

    #[test]
    fn instance_roundtrip() {
        let inst = Instance {
            port: 8900,
            token: "dc3143dcc1".into(),
            workspace: "/path/to/ws".into(),
            pid: 12345,
        };
        let json = serde_json::to_string(&inst).unwrap();
        let back: Instance = serde_json::from_str(&json).unwrap();
        assert_eq!(inst, back);
    }

    #[test]
    fn instance_matches_spec_shape() {
        // 仕様 6章の例: {"port":8900,"token":"dc3143dcc1","workspace":"/path","pid":12345}
        let raw = r#"{"port":8900,"token":"dc3143dcc1","workspace":"/path","pid":12345}"#;
        let inst: Instance = serde_json::from_str(raw).unwrap();
        assert_eq!(inst.port, 8900);
        assert_eq!(inst.token, "dc3143dcc1");
        assert_eq!(inst.workspace, "/path");
        assert_eq!(inst.pid, 12345);
    }

    #[test]
    fn instance_current_uses_own_pid() {
        let inst = Instance::current(8899, "abc", "/ws");
        assert_eq!(inst.pid, std::process::id());
        assert!(pid_alive(inst.pid), "自プロセスは生きているはず");
    }

    #[test]
    fn pid_zero_is_not_alive() {
        assert!(!pid_alive(0));
    }

    // ── 引数ヘルパ ──

    #[test]
    fn take_opt_extracts_value_and_rest() {
        let (val, rest) = take_opt(&v(&["src/main.rs", "--line", "42"]), "--line");
        assert_eq!(val.as_deref(), Some("42"));
        assert_eq!(rest, v(&["src/main.rs"]));
    }

    #[test]
    fn take_flag_extracts_presence() {
        let (found, rest) = take_flag(&v(&["hello", "--submit", "world"]), "--submit");
        assert!(found);
        assert_eq!(rest, v(&["hello", "world"]));
    }


    
    
    
    }
